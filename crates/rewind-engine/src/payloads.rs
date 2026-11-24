//! Where payload values live (`archi/06-storage-and-scale.md`, "Payloads").
//!
//! At commit, a payload goes to one of three tiers by the size of its canonical JSON:
//!
//! | Size | Where the value lives |
//! | --- | --- |
//! | up to [`PayloadTiers::inline`] (1 KiB) | in the event |
//! | up to [`PayloadTiers::table`] (64 KiB) | in the run's payload records, in its partition, committed with the batch |
//! | above | in a blob of the run (`BlobOwner::Run`), put before the batch |
//!
//! A payload that leaves the event keeps only its digest there, and gets a payload record
//! (digest, size, preview, and the value in the table tier). On delivery to the code, values
//! are loaded back from where they live: the records first, then the run's blobs. A run's
//! sealed attempt inlines every value up to the table tier; its blobs are erased with it.

use std::collections::BTreeSet;
use std::future::Future;

use rewind_core::{
    ActionFailure, Command, Completion, Digest, Event, HistoryEvent, JsonValue, Lifecycle, Payload,
    canonical_json, canonicalize,
};

use crate::ports::{BlobError, BlobStore, Handoff, PayloadRecord, TenantId};

/// Characters of a payload kept as its preview.
pub(crate) const PREVIEW_CHARS: usize = 256;

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

/// The payload an event carries, if any (an event carries at most one).
pub(crate) fn payload_of(event: &Event) -> Option<&Payload> {
    match event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Some(&started.input),
        Event::Command(Command::ScheduleCall { args, .. }) => Some(args),
        Event::Command(Command::CompleteRun { output }) => Some(output),
        Event::Completion(Completion::CallCompleted { result, .. }) => Some(result),
        Event::Completion(Completion::SignalReceived { payload, .. }) => Some(payload),
        _ => None,
    }
}

/// Mutable form of [`payload_of`].
pub(crate) fn payload_mut(event: &mut Event) -> Option<&mut Payload> {
    match event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Some(&mut started.input),
        Event::Command(Command::ScheduleCall { args, .. }) => Some(args),
        Event::Command(Command::CompleteRun { output }) => Some(output),
        Event::Completion(Completion::CallCompleted { result, .. }) => Some(result),
        Event::Completion(Completion::SignalReceived { payload, .. }) => Some(payload),
        _ => None,
    }
}

/// Size of a payload's canonical JSON, if its value is held.
pub(crate) fn value_size(payload: &Payload) -> Option<usize> {
    payload.value().map(|value| canonical_json(value).len())
}

/// Moves the values above the inline tier out of the events of a run, keeping
/// their digests (`archi/06-storage-and-scale.md`, "Payloads"): those up to the table tier go to
/// the run's payload records, committed with the batch; the larger ones to blobs under the run
/// (`BlobOwner::Run`), put before the batch. A blob listed in `copied` (a fork's, from its
/// source) is copied from its owner instead of written again. Returns the records, one per
/// digest.
pub(crate) async fn offload_into_run<B: BlobStore>(
    blobs: &B,
    run: &crate::ports::RunRef,
    events: &mut [HistoryEvent],
    tiers: PayloadTiers,
    copied: &std::collections::BTreeMap<Digest, crate::ports::BlobOwner>,
) -> Result<Vec<PayloadRecord>, BlobError> {
    let owner = crate::ports::BlobOwner::Run(run.clone());
    let mut records = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in events {
        let Some(payload) = payload_mut(&mut entry.event) else {
            continue;
        };
        let canonical = match &*payload {
            Payload::Value(value) => canonical_json(value),
            Payload::Digest(_) => continue,
        };
        if canonical.len() <= tiers.inline {
            continue;
        }
        let preview = payload.preview(PREVIEW_CHARS);
        let Payload::Value(value) = payload else {
            continue;
        };
        let value = std::mem::take(value);
        let digest = Payload::digest_of(&value);
        *payload = Payload::Digest(digest);
        if !seen.insert(digest) {
            continue;
        }
        let in_table = canonical.len() <= tiers.table;
        if !in_table {
            match copied.get(&digest) {
                Some(from) => blobs.copy(from, &owner, digest).await?,
                None => {
                    blobs
                        .put(&owner, digest, canonical.clone().into_bytes())
                        .await?
                }
            }
        }
        records.push(PayloadRecord {
            digest,
            size: canonical.len() as u64,
            preview,
            value: in_table.then(|| canonicalize(&value)),
        });
    }
    Ok(records)
}

/// The value of a payload, loaded from `source` if only its digest is held, with its keys in
/// canonical order: a code observes the same value live and on replay, wherever it was loaded
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

/// Loads every payload of the events: the history as a self-contained document.
pub(crate) async fn load_all<P: PayloadSource>(
    source: &P,
    tenant: &TenantId,
    events: &mut [HistoryEvent],
) -> Result<(), BlobError> {
    for entry in events {
        if let Some(payload) = payload_mut(&mut entry.event)
            && let Payload::Digest(_) = payload
        {
            *payload = Payload::Value(load(source, tenant, payload).await?);
        }
    }
    Ok(())
}

/// The handoff of a completion to the code, its payloads loaded.
pub(crate) async fn handoff<P: PayloadSource>(
    source: &P,
    tenant: &TenantId,
    completion: &Completion,
) -> Result<Handoff, BlobError> {
    Ok(match completion {
        Completion::CallCompleted { seq, result } => Handoff::CallCompleted {
            seq: *seq,
            result: load(source, tenant, result).await?,
        },
        Completion::CallFailed { seq, error } => Handoff::CallFailed {
            seq: *seq,
            error: ActionFailure {
                data: error.data.as_ref().map(canonicalize),
                ..error.clone()
            },
        },
        Completion::TimerFired { seq } => Handoff::TimerFired { seq: *seq },
        Completion::SignalReceived { name, payload } => Handoff::SignalReceived {
            name: name.clone(),
            payload: load(source, tenant, payload).await?,
        },
        Completion::CancelRequested { reason } => Handoff::CancelRequested {
            reason: reason.clone(),
        },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rewind_core::{PartitionId, RunId, Seq, Timestamp};
    use serde_json::json;

    use super::*;
    use crate::adapters::MemoryBlobs;
    use crate::executors::activation::Carried;
    use crate::ports::{BlobOwner, RunRef};

    fn entry(value: JsonValue) -> HistoryEvent {
        HistoryEvent {
            index: 0,
            recorded_at: Timestamp(0),
            event: Event::Completion(Completion::CallCompleted {
                seq: Seq(1),
                result: Payload::Value(value),
            }),
        }
    }

    #[tokio::test]
    async fn payloads_go_to_the_tier_their_size_calls_for() {
        let blobs = MemoryBlobs::new();
        let run = RunRef::new(
            TenantId::local(),
            RunId::in_partition(PartitionId::new(0), Timestamp(0), 1),
        );
        let tiers = PayloadTiers {
            inline: 16,
            table: 64,
        };
        let small = json!("small");
        let medium = json!("m".repeat(40));
        let large = json!("l".repeat(100));
        let mut events = vec![
            entry(small.clone()),
            entry(medium.clone()),
            entry(large.clone()),
        ];
        let records = offload_into_run(&blobs, &run, &mut events, tiers, &BTreeMap::new())
            .await
            .unwrap();

        assert_eq!(payload_of(&events[0].event), Some(&Payload::Value(small)));
        assert_eq!(
            payload_of(&events[1].event),
            Some(&Payload::Digest(Payload::digest_of(&medium)))
        );
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].value, Some(medium.clone()));
        assert_eq!(records[0].size, 42);
        assert_eq!(records[1].value, None);
        assert_eq!(records[1].size, 102);
        // Only the large one is a blob, of the run.
        let owner = BlobOwner::Run(run.clone());
        assert_eq!(
            blobs
                .get(&owner, Payload::digest_of(&medium))
                .await
                .unwrap(),
            None
        );
        let source = Carried {
            records: records
                .iter()
                .map(|record| (record.digest, record.clone()))
                .collect(),
            blobs: &blobs,
            owners: vec![owner],
        };
        for (entry, value) in [(&events[1], medium), (&events[2], large)] {
            let held = payload_of(&entry.event).unwrap();
            assert!(matches!(held, Payload::Digest(_)));
            assert_eq!(load(&source, &run.tenant, held).await.unwrap(), value);
        }
    }
}
