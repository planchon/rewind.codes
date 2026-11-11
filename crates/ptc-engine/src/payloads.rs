//! When payload values move (`archi/06-storage-and-scale.md`, "Payloads"): at commit, values
//! above the threshold go to the `BlobStore` and the entry keeps only the digest, with a
//! `payloads` row (size, preview) committed with the activation; on delivery to the script,
//! values are loaded back.

use std::collections::BTreeSet;

use ptc_core::{
    Command, Completion, Event, HistoryEntry, JsonValue, Lifecycle, Payload, ToolFailure,
    canonical_json, canonicalize,
};

use crate::ports::{BlobError, BlobStore, Delivery, PayloadRecord, TenantId};

/// Characters of a payload kept as its preview.
const PREVIEW_CHARS: usize = 256;

/// The payload an event carries, if any (an event carries at most one).
pub(crate) fn payload_of(event: &Event) -> Option<&Payload> {
    match event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Some(&started.input),
        Event::Command(Command::ScheduleToolCall { args, .. }) => Some(args),
        Event::Command(Command::CompleteRun { output }) => Some(output),
        Event::Completion(Completion::ToolCallCompleted { result, .. }) => Some(result),
        Event::Completion(Completion::SignalReceived { payload, .. }) => Some(payload),
        _ => None,
    }
}

/// Mutable form of [`payload_of`].
pub(crate) fn payload_mut(event: &mut Event) -> Option<&mut Payload> {
    match event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Some(&mut started.input),
        Event::Command(Command::ScheduleToolCall { args, .. }) => Some(args),
        Event::Command(Command::CompleteRun { output }) => Some(output),
        Event::Completion(Completion::ToolCallCompleted { result, .. }) => Some(result),
        Event::Completion(Completion::SignalReceived { payload, .. }) => Some(payload),
        _ => None,
    }
}

/// Size of a payload's canonical JSON, if its value is held.
pub(crate) fn value_size(payload: &Payload) -> Option<usize> {
    payload.value().map(|value| canonical_json(value).len())
}

/// Moves the values above `threshold` bytes to the blob store, keeping their digests, and
/// returns their rows, one per digest.
pub(crate) async fn offload<B: BlobStore>(
    blobs: &B,
    tenant: &TenantId,
    entries: &mut [HistoryEntry],
    threshold: usize,
) -> Result<Vec<PayloadRecord>, BlobError> {
    let mut records = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let Some(payload) = payload_mut(&mut entry.event) else {
            continue;
        };
        let Payload::Value(value) = payload else {
            continue;
        };
        let canonical = canonical_json(value);
        if canonical.len() <= threshold {
            continue;
        }
        let digest = Payload::digest_of(value);
        if seen.insert(digest) {
            records.push(PayloadRecord {
                digest,
                size: canonical.len() as u64,
                preview: payload.preview(PREVIEW_CHARS),
                value: None,
            });
            blobs.put(tenant, digest, canonical.into_bytes()).await?;
        }
        *payload = Payload::Digest(digest);
    }
    Ok(records)
}

/// The value of a payload, loaded from the blob store if only its digest is held, with its
/// keys in canonical order: a script observes the same value live and on replay, wherever it
/// was loaded from. A loaded value is checked against its digest.
pub(crate) async fn load<B: BlobStore>(
    blobs: &B,
    tenant: &TenantId,
    payload: &Payload,
) -> Result<JsonValue, BlobError> {
    match payload {
        Payload::Value(value) => Ok(canonicalize(value)),
        Payload::Digest(digest) => {
            let bytes = blobs
                .get(tenant, *digest)
                .await?
                .ok_or(BlobError::Missing(*digest))?;
            let value: JsonValue =
                serde_json::from_slice(&bytes).map_err(|err| BlobError::Corrupt {
                    digest: *digest,
                    message: err.to_string(),
                })?;
            if Payload::digest_of(&value) != *digest {
                return Err(BlobError::Corrupt {
                    digest: *digest,
                    message: "the value does not hash to its digest".into(),
                });
            }
            Ok(canonicalize(&value))
        }
    }
}

/// Loads every payload of the entries: the history as a self-contained document.
pub(crate) async fn load_all<B: BlobStore>(
    blobs: &B,
    tenant: &TenantId,
    entries: &mut [HistoryEntry],
) -> Result<(), BlobError> {
    for entry in entries {
        if let Some(payload) = payload_mut(&mut entry.event)
            && let Payload::Digest(_) = payload
        {
            *payload = Payload::Value(load(blobs, tenant, payload).await?);
        }
    }
    Ok(())
}

/// The delivery of a completion, its payloads loaded.
pub(crate) async fn delivery<B: BlobStore>(
    blobs: &B,
    tenant: &TenantId,
    completion: &Completion,
) -> Result<Delivery, BlobError> {
    Ok(match completion {
        Completion::ToolCallCompleted { seq, result } => Delivery::ToolCallCompleted {
            seq: *seq,
            result: load(blobs, tenant, result).await?,
        },
        Completion::ToolCallFailed { seq, error } => Delivery::ToolCallFailed {
            seq: *seq,
            error: ToolFailure {
                data: error.data.as_ref().map(canonicalize),
                ..error.clone()
            },
        },
        Completion::TimerFired { seq } => Delivery::TimerFired { seq: *seq },
        Completion::SignalReceived { name, payload } => Delivery::SignalReceived {
            name: name.clone(),
            payload: load(blobs, tenant, payload).await?,
        },
        Completion::CancelRequested { reason } => Delivery::CancelRequested {
            reason: reason.clone(),
        },
    })
}
