//! The ports of `rewind server migrate-runs`: the runs of the tables the migration set aside,
//! and the payload values and archived journals of their tenant-keyed blobs
//! (`archi/08-decisions.md`, "Taken while writing the object-storage spec": the migration;
//! `archi/07-code-architecture.md`, "Embedded server and CLI").
//!
//! ```text
//!  LegacyRuns ── runs(after, limit) ──► [LegacyRun]   the schema `legacy` (Postgres),
//!             ── drop_tables()                         the tables `legacy_*` (Turso)
//!  LegacyBlobs ── payload(tenant, digest) ──► bytes    `{tenant}/sha256/{digest}`
//!              ── journal(tenant, digest) ──► entries  the journal document archived there
//! ```
//!
//! [`crate::migrate`] reads them once, writes each run into its namespace's partition or into
//! the archive, and drops the tables once every run moved. Nothing else uses them.

use std::future::Future;

use rewind_core::{Digest, Epoch, HistoryEntry, Payload, RunId, Seq, Timestamp};

use crate::ports::journal::{InboxItem, PayloadRecord, Quarantine, RunMetadata, TaskState};
use crate::ports::namespaces::NamespaceName;
use crate::ports::partition::Watcher;
use crate::ports::tenancy::{RunRef, TenantId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LegacyError {
    /// A row or a blob that cannot be read back.
    #[error("corrupt record of the set-aside runs: {0}")]
    Corrupt(String),
    /// The backend cannot be reached; the call may be tried again.
    #[error("the set-aside runs are unavailable: {0}")]
    Unavailable(String),
    #[error("backend error of the set-aside runs: {0}")]
    Backend(String),
}

/// A run of the set-aside tables, with everything it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct LegacyRun {
    /// Its version 7 id, which it keeps.
    pub run: RunRef,
    pub namespace: NamespaceName,
    /// What ran and the characters of its handle's suffix, as the tables have them: its handle
    /// stays the same (`archi/06-storage-and-scale.md`, "Run handles").
    pub origin: String,
    pub suffix_len: u8,
    pub script: Digest,
    pub toolset: Digest,
    /// The determinism epoch of its `run_started`.
    pub epoch: Epoch,
    /// The deadline of its `run_started`.
    pub expires_at: Timestamp,
    /// The cancellation of its deadline is already in its inbox or its history.
    pub expiry_enqueued: bool,
    pub quarantine: Option<Quarantine>,
    /// Activations that failed in a row.
    pub failures: u32,
    /// Activations started without their end seen.
    pub attempts: u32,
    pub metadata: RunMetadata,
    /// The start's idempotency key, unique in the namespace.
    pub idempotency_key: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub history: LegacyHistory,
    /// Its inbox, by item id.
    pub inbox: Vec<InboxItem>,
    /// The idempotency keys of the signals it took.
    pub signal_keys: Vec<String>,
    /// Its script's timers not fired yet, by `seq`.
    pub timers: Vec<(Seq, Timestamp)>,
    /// Its tool tasks, by `seq`.
    pub tasks: Vec<LegacyTask>,
    /// Its webhook tokens, by their SHA-256.
    pub webhooks: Vec<LegacyWebhook>,
    /// The runs that wait for its end, by run id.
    pub watchers: Vec<Watcher>,
    /// The payloads it references above the inline tier: their size, preview, and their value
    /// up to 64 KiB; the values above are blobs of the tenant ([`LegacyBlobs::payload`]).
    pub payloads: Vec<PayloadRecord>,
}

/// Where a run's history is.
#[derive(Clone, Debug, PartialEq)]
pub enum LegacyHistory {
    /// In the tables, in order.
    Entries(Vec<HistoryEntry>),
    /// The retention archived it: the journal document is a blob of the tenant, by its
    /// digest ([`LegacyBlobs::journal`]).
    Archived(Digest),
}

/// A tool task of a run, as its row states it; its route and policy are the pinned toolset's.
#[derive(Clone, Debug, PartialEq)]
pub struct LegacyTask {
    pub seq: Seq,
    pub tool: String,
    pub args: Payload,
    /// `Claimed` while a worker held it, `Deferred` while its handler held its token.
    pub state: TaskState,
    /// Claims so far: 0 before the first.
    pub attempt: u32,
    pub schema_change_approved: bool,
    pub created_at: Timestamp,
}

/// A webhook token of a run: the signal it sends, and the SHA-256 of the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyWebhook {
    pub name: String,
    pub hash: Digest,
}

/// The runs of the tables the schema migration set aside: the schema `legacy` on Postgres, the
/// tables `legacy_*` on Turso.
///
/// - `runs` reads the runs in the order of their ids, each with everything it holds. A store
///   whose tables were dropped, or that never had runs, reads none.
/// - `drop_tables` drops the tables; dropping them again changes nothing.
pub trait LegacyRuns: Send + Sync {
    /// Up to `limit` runs whose id is above `after` (from the first with `None`), by id.
    fn runs(
        &self,
        after: Option<RunId>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<LegacyRun>, LegacyError>> + Send;

    /// Drops the set-aside tables, once every run moved.
    fn drop_tables(&self) -> impl Future<Output = Result<(), LegacyError>> + Send;
}

/// The tenant-keyed blobs the set-aside runs reference: payload values above 64 KiB, and the
/// journal documents the retention archived. Both are read, never written.
pub trait LegacyBlobs: Send + Sync {
    /// The canonical JSON of a payload value of the tenant; `None` when it is missing.
    fn payload(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, LegacyError>> + Send;

    /// The entries of an archived journal document of the tenant; `None` when it is missing.
    fn journal(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<Vec<HistoryEntry>>, LegacyError>> + Send;
}
