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
//!
//! A large payload's row is reserved before its blob is written, so that the payload cleanup
//! never deletes a blob that a commit is about to reference. When the cleanup has already
//! started deleting the payload, its value is committed in its row instead of the blob store.

use std::collections::BTreeSet;
use std::future::Future;

use rewind_core::{
    Command, Completion, Digest, Event, HistoryEntry, JsonValue, Lifecycle, Payload, Timestamp,
    ToolFailure, canonical_json, canonicalize,
};

use crate::engine::{Engine, EngineError};
use crate::ports::{
    BlobError, BlobStore, Clock, Delivery, Entropy, JournalStore, PayloadRecord, ScriptExecutor,
    SecretStore, TenantId, ToolDispatcher, WebhookSender,
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

impl<J, B, X, D, C, E, S, W> Engine<J, B, X, D, C, E, S, W>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: Clock,
    E: Entropy,
    S: SecretStore,
    W: WebhookSender,
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
///
/// The rows of the blob tier are reserved before their blobs are written, so that no cleanup
/// deletes a blob between its write and the commit. A payload whose deletion has already
/// started keeps its value in its row instead: its blob may be deleted at any time, even after
/// this write (`archi/08-decisions.md`).
pub(crate) async fn offload<J: JournalStore, B: BlobStore>(
    journal: &J,
    blobs: &B,
    tenant: &TenantId,
    entries: &mut [HistoryEntry],
    tiers: PayloadTiers,
    now: Timestamp,
) -> Result<Vec<PayloadRecord>, EngineError> {
    let mut records = Vec::new();
    // The payloads of the blob tier: their record, canonical JSON and value.
    let mut large = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
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
        let record = PayloadRecord {
            digest,
            size: canonical.len() as u64,
            preview,
            value: in_table.then(|| canonicalize(&value)),
        };
        if in_table {
            records.push(record);
        } else {
            large.push((record, canonical, value));
        }
    }
    if large.is_empty() {
        return Ok(records);
    }

    let reserved: Vec<PayloadRecord> = large.iter().map(|(record, ..)| record.clone()).collect();
    let deleting = journal.reserve_payloads(tenant, &reserved, now).await?;
    for (mut record, canonical, value) in large {
        if deleting.contains(&record.digest) {
            record.value = Some(canonicalize(&value));
        } else {
            blobs
                .put(tenant, record.digest, canonical.into_bytes())
                .await?;
        }
        records.push(record);
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
    use std::sync::Mutex;

    use rewind_core::{Epoch, RunId, RunMode, RunStarted, RunStatus, Seed, Seq, Timestamp};
    use serde_json::json;

    use super::*;
    use crate::adapters::{MemoryBlobs, MemoryJournal, MemoryReadModel, MemorySecrets};
    use crate::ports::{Activation, Lease, NewRun, Retention, RunMetadata, RunRef};
    use crate::retention::Housekeeper;

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
        let mut entries = vec![
            entry(small.clone()),
            entry(medium.clone()),
            entry(large.clone()),
        ];
        let journal = MemoryJournal::new();
        let records = offload(&journal, &blobs, &tenant, &mut entries, tiers, T0)
            .await
            .unwrap();

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

    // ---------------------------------------------------------------------------------------
    // Payload cleanup racing a commit

    /// 2026-01-01T00:00:00Z.
    const T0: Timestamp = Timestamp(1_767_225_600_000);

    /// Every payload above 8 bytes goes to the blob store.
    const TINY: PayloadTiers = PayloadTiers {
        inline: 8,
        table: 8,
    };

    fn large() -> JsonValue {
        json!({ "text": "a payload of the blob tier" })
    }

    fn at(index: u64, value: JsonValue) -> HistoryEntry {
        HistoryEntry {
            index,
            ..entry(value)
        }
    }

    /// Creates a run and leases it.
    async fn leased_run(journal: &MemoryJournal, n: u128, now: Timestamp) -> Lease {
        let run = RunRef::new(TenantId::local(), RunId::from_u128(n));
        journal
            .create_run(
                NewRun {
                    run: run.clone(),
                    namespace: crate::ports::NamespaceName::migrated(),
                    origin: "script".into(),
                    started: RunStarted {
                        script: Digest::of(b"script"),
                        toolset: Digest::of(b"toolset"),
                        input: Payload::Value(json!(null)),
                        seed: Seed::from_bytes([1; 32]),
                        epoch: Epoch(1),
                        expires_at: Timestamp(now.0 + 86_400_000),
                        mode: RunMode::Mock,
                    },
                    metadata: RunMetadata::default(),
                    created_at: now,
                },
                None,
            )
            .await
            .unwrap();
        journal
            .acquire_run(&run, "worker", now, std::time::Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap()
    }

    fn housekeeper<'a, B: BlobStore>(
        journal: &'a MemoryJournal,
        blobs: &'a B,
        read_model: &'a MemoryReadModel,
        secrets: &'a MemorySecrets,
    ) -> Housekeeper<'a, MemoryJournal, B, MemoryReadModel, MemorySecrets> {
        Housekeeper {
            journal,
            blobs,
            read_model,
            secrets,
            batch: 100,
        }
    }

    /// A run referenced the large payload, then was deleted: at the returned time the payload
    /// is past its grace, and cleanup deletes it.
    async fn unreferenced_large_payload(journal: &MemoryJournal, blobs: &MemoryBlobs) -> Timestamp {
        let lease = leased_run(journal, 1, T0).await;
        let mut entries = vec![at(0, large())];
        let records = offload(journal, blobs, &TenantId::local(), &mut entries, TINY, T0)
            .await
            .unwrap();
        let mut activation = Activation::new(entries, vec![], RunStatus::Active);
        activation.payloads = records;
        journal
            .commit_activation(&lease, activation, T0)
            .await
            .unwrap();
        assert!(journal.delete_run(&lease.run, T0).await.unwrap());
        T0.saturating_add(Retention::default().payload_grace)
    }

    /// The large payload as the run `lease` reads it back.
    async fn read_back(journal: &MemoryJournal, blobs: &MemoryBlobs, lease: &Lease) -> JsonValue {
        let history = journal.load_history(&lease.run, 0).await.unwrap();
        let payload = payload_of(&history[0].event).unwrap();
        assert_eq!(payload, &Payload::Digest(Payload::digest_of(&large())));
        let source = StoredPayloads { journal, blobs };
        load(&source, &lease.run.tenant, payload).await.unwrap()
    }

    /// Runs the payload cleanup right after each blob is written.
    struct CleanupAfterPut<'a> {
        blobs: &'a MemoryBlobs,
        journal: &'a MemoryJournal,
        now: Timestamp,
    }

    impl BlobStore for CleanupAfterPut<'_> {
        async fn put(
            &self,
            tenant: &TenantId,
            digest: Digest,
            bytes: Vec<u8>,
        ) -> Result<(), BlobError> {
            self.blobs.put(tenant, digest, bytes).await?;
            let (read_model, secrets) = (MemoryReadModel::new(), MemorySecrets::new());
            housekeeper(self.journal, self.blobs, &read_model, &secrets)
                .clean_payloads(self.now)
                .await
                .unwrap();
            Ok(())
        }

        async fn get(
            &self,
            tenant: &TenantId,
            digest: Digest,
        ) -> Result<Option<Vec<u8>>, BlobError> {
            self.blobs.get(tenant, digest).await
        }

        async fn delete(&self, tenant: &TenantId, digest: Digest) -> Result<bool, BlobError> {
            self.blobs.delete(tenant, digest).await
        }
    }

    #[tokio::test]
    async fn a_cleanup_between_the_put_and_the_commit_keeps_the_blob() {
        let (journal, blobs) = (MemoryJournal::new(), MemoryBlobs::new());
        let now = unreferenced_large_payload(&journal, &blobs).await;

        // Another run produces the same payload: its blob is already there when the cleanup
        // runs, before the commit.
        let lease = leased_run(&journal, 2, now).await;
        let writer = CleanupAfterPut {
            blobs: &blobs,
            journal: &journal,
            now,
        };
        let mut entries = vec![at(0, large())];
        let records = offload(
            &journal,
            &writer,
            &lease.run.tenant,
            &mut entries,
            TINY,
            now,
        )
        .await
        .unwrap();
        let mut activation = Activation::new(entries, vec![], RunStatus::Active);
        activation.payloads = records;
        journal
            .commit_activation(&lease, activation, now)
            .await
            .unwrap();

        assert_eq!(read_back(&journal, &blobs, &lease).await, large());
    }

    /// Commits an activation of the run `lease` with the large payload the first time a blob is
    /// deleted, before deleting it: the cleanup had decided to delete the payload, and its
    /// delete reaches the blob store after the commit.
    struct CommitBeforeDelete<'a> {
        blobs: &'a MemoryBlobs,
        journal: &'a MemoryJournal,
        lease: &'a Lease,
        now: Timestamp,
        committed: Mutex<Option<Result<(), String>>>,
    }

    impl BlobStore for CommitBeforeDelete<'_> {
        async fn put(
            &self,
            tenant: &TenantId,
            digest: Digest,
            bytes: Vec<u8>,
        ) -> Result<(), BlobError> {
            self.blobs.put(tenant, digest, bytes).await
        }

        async fn get(
            &self,
            tenant: &TenantId,
            digest: Digest,
        ) -> Result<Option<Vec<u8>>, BlobError> {
            self.blobs.get(tenant, digest).await
        }

        async fn delete(&self, tenant: &TenantId, digest: Digest) -> Result<bool, BlobError> {
            let first = self.committed.lock().unwrap().is_none();
            if first {
                let mut entries = vec![at(0, large())];
                let committed = async {
                    let records = offload(
                        self.journal,
                        self.blobs,
                        tenant,
                        &mut entries,
                        TINY,
                        self.now,
                    )
                    .await
                    .map_err(|err| err.to_string())?;
                    let mut activation = Activation::new(entries, vec![], RunStatus::Active);
                    activation.payloads = records;
                    self.journal
                        .commit_activation(self.lease, activation, self.now)
                        .await
                        .map_err(|err| err.to_string())
                }
                .await;
                *self.committed.lock().unwrap() = Some(committed);
            }
            self.blobs.delete(tenant, digest).await
        }
    }

    #[tokio::test]
    async fn a_commit_while_the_cleanup_deletes_the_blob_keeps_the_value() {
        let (journal, blobs) = (MemoryJournal::new(), MemoryBlobs::new());
        let now = unreferenced_large_payload(&journal, &blobs).await;

        let lease = leased_run(&journal, 2, now).await;
        let racing = CommitBeforeDelete {
            blobs: &blobs,
            journal: &journal,
            lease: &lease,
            now,
            committed: Mutex::new(None),
        };
        let (read_model, secrets) = (MemoryReadModel::new(), MemorySecrets::new());
        housekeeper(&journal, &racing, &read_model, &secrets)
            .clean_payloads(now)
            .await
            .unwrap();
        assert_eq!(
            racing.committed.lock().unwrap().clone(),
            Some(Ok(())),
            "the commit raced the blob's delete"
        );

        assert_eq!(read_back(&journal, &blobs, &lease).await, large());
    }
}
