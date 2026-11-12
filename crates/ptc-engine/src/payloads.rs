//! When payload values move (`archi/06-storage-and-scale.md`, "Payloads").
//!
//! At commit, a payload goes to one of three tiers by the size of its canonical JSON:
//!
//! | Size | Where the value lives |
//! | --- | --- |
//! | up to [`PayloadTiers::inline`] (1 KiB) | in the event |
//! | up to [`PayloadTiers::table`] (64 KiB) | in the store's `payloads` table, committed with the activation |
//! | above | in the `BlobStore`, written before the commit |
//!
//! A payload that leaves the event keeps only its digest there, and gets a `payloads` row (size,
//! preview, and the value in the table tier) and a reference from the run. On delivery to the
//! script, values are loaded back from where they live: the table first, then the blob store.

use std::collections::BTreeSet;
use std::future::Future;

use ptc_core::{
    Command, Completion, Digest, Event, HistoryEntry, JsonValue, Lifecycle, Payload, ToolFailure,
    canonical_json, canonicalize,
};

use crate::engine::Engine;
use crate::ports::{
    BlobError, BlobStore, Clock, Delivery, Entropy, JournalStore, PayloadRecord, ScriptExecutor,
    TenantId, ToolDispatcher,
};

/// Characters of a payload kept as its preview.
const PREVIEW_CHARS: usize = 256;

/// The size thresholds of the payload tiers, in bytes of canonical JSON. Load tests confirm the
/// defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadTiers {
    /// Payloads up to this size stay in the event. 1 KiB by default.
    pub inline: usize,
    /// Payloads up to this size, above `inline`, live in the store's `payloads` table; above
    /// it, in the `BlobStore`. 64 KiB by default.
    pub table: usize,
}

impl Default for PayloadTiers {
    fn default() -> Self {
        Self {
            inline: 1024,
            table: 64 * 1024,
        }
    }
}

/// Where payload values held only by their digest are loaded from.
pub trait PayloadSource: Send + Sync {
    /// The value whose canonical JSON hashes to `digest`, or `None` if it is nowhere.
    fn fetch(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<JsonValue>, BlobError>> + Send;
}

/// A blob store alone: payloads above the table tier, and journals whose payloads are all in
/// the blob store.
impl<B: BlobStore> PayloadSource for B {
    async fn fetch(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<JsonValue>, BlobError> {
        let Some(bytes) = self.get(tenant, digest).await? else {
            return Ok(None);
        };
        let value = serde_json::from_slice(&bytes).map_err(|err| BlobError::Corrupt {
            digest,
            message: err.to_string(),
        })?;
        Ok(Some(value))
    }
}

/// Where a run's payloads live: the store's `payloads` table, then the blob store.
#[derive(Debug, Clone, Copy)]
pub struct StoredPayloads<'a, J, B> {
    pub journal: &'a J,
    pub blobs: &'a B,
}

impl<J: JournalStore, B: BlobStore> PayloadSource for StoredPayloads<'_, J, B> {
    async fn fetch(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<JsonValue>, BlobError> {
        let record = self
            .journal
            .get_payload(tenant, digest)
            .await
            .map_err(|err| BlobError::Backend(format!("the payloads table: {err}")))?;
        if let Some(value) = record.and_then(|record| record.value) {
            return Ok(Some(value));
        }
        self.blobs.fetch(tenant, digest).await
    }
}

impl<J, B, X, D, C, E> Engine<J, B, X, D, C, E>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: Clock,
    E: Entropy,
{
    /// Where this engine's payloads live.
    pub(crate) fn payloads(&self) -> StoredPayloads<'_, J, B> {
        StoredPayloads {
            journal: &self.journal,
            blobs: &self.blobs,
        }
    }

    /// The value of a payload, loaded from where it lives if only its digest is held.
    pub(crate) async fn load_payload(
        &self,
        tenant: &TenantId,
        payload: &Payload,
    ) -> Result<JsonValue, BlobError> {
        load(&self.payloads(), tenant, payload).await
    }
}

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

/// Moves the values above the inline tier out of the events, keeping their digests: those up to
/// the table tier go into their `payloads` row, the larger ones to the blob store. Returns the
/// rows, one per digest; the commit writes them with the run's references.
pub(crate) async fn offload<B: BlobStore>(
    blobs: &B,
    tenant: &TenantId,
    entries: &mut [HistoryEntry],
    tiers: PayloadTiers,
) -> Result<Vec<PayloadRecord>, BlobError> {
    let mut records = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let Some(payload) = payload_mut(&mut entry.event) else {
            continue;
        };
        let Payload::Value(value) = &*payload else {
            continue;
        };
        let canonical = canonical_json(value);
        if canonical.len() <= tiers.inline {
            continue;
        }
        let digest = Payload::digest_of(value);
        if seen.insert(digest) {
            let in_table = canonical.len() <= tiers.table;
            records.push(PayloadRecord {
                digest,
                size: canonical.len() as u64,
                preview: payload.preview(PREVIEW_CHARS),
                value: in_table.then(|| canonicalize(value)),
            });
            if !in_table {
                blobs.put(tenant, digest, canonical.into_bytes()).await?;
            }
        }
        *payload = Payload::Digest(digest);
    }
    Ok(records)
}

/// The value of a payload, loaded from `source` if only its digest is held, with its keys in
/// canonical order: a script observes the same value live and on replay, wherever it was loaded
/// from. A loaded value is checked against its digest.
pub(crate) async fn load<P: PayloadSource>(
    source: &P,
    tenant: &TenantId,
    payload: &Payload,
) -> Result<JsonValue, BlobError> {
    match payload {
        Payload::Value(value) => Ok(canonicalize(value)),
        Payload::Digest(digest) => {
            let value = source
                .fetch(tenant, *digest)
                .await?
                .ok_or(BlobError::Missing(*digest))?;
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
pub(crate) async fn load_all<P: PayloadSource>(
    source: &P,
    tenant: &TenantId,
    entries: &mut [HistoryEntry],
) -> Result<(), BlobError> {
    for entry in entries {
        if let Some(payload) = payload_mut(&mut entry.event)
            && let Payload::Digest(_) = payload
        {
            *payload = Payload::Value(load(source, tenant, payload).await?);
        }
    }
    Ok(())
}

/// The delivery of a completion, its payloads loaded.
pub(crate) async fn delivery<P: PayloadSource>(
    source: &P,
    tenant: &TenantId,
    completion: &Completion,
) -> Result<Delivery, BlobError> {
    Ok(match completion {
        Completion::ToolCallCompleted { seq, result } => Delivery::ToolCallCompleted {
            seq: *seq,
            result: load(source, tenant, result).await?,
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
            payload: load(source, tenant, payload).await?,
        },
        Completion::CancelRequested { reason } => Delivery::CancelRequested {
            reason: reason.clone(),
        },
    })
}

#[cfg(test)]
mod tests {
    use ptc_core::{Seq, Timestamp};
    use serde_json::json;

    use super::*;
    use crate::adapters::MemoryBlobs;

    fn entry(value: JsonValue) -> HistoryEntry {
        HistoryEntry {
            index: 1,
            recorded_at: Timestamp(0),
            event: Event::Completion(Completion::ToolCallCompleted {
                seq: Seq(1),
                result: Payload::Value(value),
            }),
        }
    }

    #[tokio::test]
    async fn payloads_go_to_the_tier_their_size_calls_for() {
        let blobs = MemoryBlobs::new();
        let tenant = TenantId::local();
        let tiers = PayloadTiers {
            inline: 16,
            table: 64,
        };
        let small = json!("small");
        let medium = json!("m".repeat(40));
        let large = json!("l".repeat(100));
        let mut entries = vec![entry(small.clone()), entry(medium.clone()), entry(large.clone())];
        let records = offload(&blobs, &tenant, &mut entries, tiers).await.unwrap();

        assert_eq!(payload_of(&entries[0].event), Some(&Payload::Value(small)));
        assert_eq!(
            payload_of(&entries[1].event),
            Some(&Payload::Digest(Payload::digest_of(&medium)))
        );
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].value, Some(medium.clone()));
        assert_eq!(records[0].size, 42);
        assert_eq!(records[1].value, None);
        assert_eq!(records[1].size, 102);
        // Only the large one is in the blob store.
        let medium_digest = Payload::digest_of(&medium);
        let large_digest = Payload::digest_of(&large);
        assert_eq!(blobs.get(&tenant, medium_digest).await.unwrap(), None);
        assert!(blobs.get(&tenant, large_digest).await.unwrap().is_some());
        assert_eq!(
            load(&blobs, &tenant, &Payload::Digest(large_digest))
                .await
                .unwrap(),
            large
        );
    }
}
