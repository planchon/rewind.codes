//! Runs as the store knows them: creation, run metadata, the record kept for queries,
//! quarantine and retention.

use std::collections::BTreeMap;

use rewind_core::{Digest, Epoch, RunId, RunStarted, RunStatus, Timestamp};

use crate::ports::tenancy::{RunRef, TenantId};

/// Given by the harness when it starts a run, and stored with the run. The script never sees
/// it and it is not part of the journal's events, so it is never replayed and never ends up
/// in a cassette (`archi/05-testing-and-observability.md`, "Run metadata"). The tenant is in
/// the run's [`RunRef`], and the idempotency key is given to `create_run` beside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunMetadata {
    /// A bounded string map (agent, conversation, end user…), to filter runs and as span
    /// attributes.
    pub labels: BTreeMap<String, String>,
    pub trace_context: Option<TraceContext>,
    /// For a fork, the run it was forked from, in the same tenant, and where it was forced live
    /// (`archi/03-execution.md`, "Forks").
    pub parent: Option<ForkParent>,
}

/// The run a fork was started from. The fork's first activation copies the parent's history
/// while the fork's commands match it, up to the entry `at` when the request gave one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkParent {
    pub run: RunId,
    /// Go live from this entry of the parent's history, even if the commands still match.
    pub at: Option<u64>,
}

/// W3C trace context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    pub traceparent: String,
    pub tracestate: Option<String>,
}

/// A run to create. Its `run_started` goes to the inbox: the first activation commits it
/// together with the first commands.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRun {
    pub run: RunRef,
    pub started: RunStarted,
    pub metadata: RunMetadata,
    pub created_at: Timestamp,
}

/// What `create_run` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateRun {
    Created(Created),
    /// The tenant already has its maximum of active runs: nothing was created.
    QuotaExceeded {
        quota: u32,
    },
}

/// The run a `create_run` resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub run: RunRef,
    /// `false` when the idempotency key was already used: `run` is the existing run.
    pub created: bool,
}

/// What `enqueue` did with a completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    Accepted,
    /// The idempotency key was already seen for this run: nothing was enqueued.
    Duplicate,
    /// The run has ended: nothing was enqueued.
    RunEnded,
}

/// Why a run is set aside (`archi/03-execution.md`, "Non-determinism and quarantine").
/// Quarantine is service state, like a lease: it is not in the history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quarantine {
    pub kind: QuarantineKind,
    /// The replay mismatch, the last error of the failed activations, or how many activations
    /// never finished.
    pub reason: String,
    pub since: Timestamp,
    /// The build of the last worker that retried the run after a deploy, if any: a worker of
    /// a new build retries each quarantined run of its epochs once.
    pub retried_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuarantineKind {
    /// Replay did not match the history. Given up, the run ends with
    /// `run_aborted { non_determinism }`.
    NonDeterminism,
    /// The run's activations failed [`QUARANTINE_AFTER_FAILURES`] times in a row, or never
    /// finished: their worker died during them. Given up, the run ends with
    /// `run_aborted { internal }`.
    Failures,
}

/// Consecutive failed activations after which a run is quarantined: an activation that never
/// finished, because its worker died during it, counts as a failed one.
pub const QUARANTINE_AFTER_FAILURES: u32 = 5;

/// What `record_failure` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failures {
    /// Activations of the run that failed in a row, this one included.
    pub count: u32,
    /// This failure quarantined the run.
    pub quarantined: bool,
}

/// Where the journal of an archived run lives: a journal document in the `BlobStore`, under
/// the SHA-256 of its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Archive {
    pub document: Digest,
    pub archived_at: Timestamp,
}

/// A run as the store knows it. The status and lengths are copies kept for queries; the
/// history is authoritative.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub run: RunRef,
    pub script: Digest,
    pub toolset: Digest,
    pub status: RunStatus,
    pub epoch: Epoch,
    pub expires_at: Timestamp,
    pub history_len: u64,
    pub quarantine: Option<Quarantine>,
    /// Activations that failed in a row since the last commit, as `record_failure` counted them.
    pub failures: u32,
    /// Activations started since the last commit: the leases `acquire_runnable` handed out, the
    /// current activation included. Those that neither committed nor recorded a failure never
    /// finished: their worker died during them.
    pub attempts: u32,
    /// The worker that committed last and keeps the run's instance in its cache.
    pub sticky_worker: Option<String>,
    pub metadata: RunMetadata,
    pub idempotency_key: Option<String>,
    pub created_at: Timestamp,
    /// `recorded_at` of the last entry, or `created_at` before the first commit.
    pub updated_at: Timestamp,
    /// `recorded_at` of the entry that ended the run.
    pub ended_at: Option<Timestamp>,
    pub archive: Option<Archive>,
}

/// Runs to list, in one tenant, oldest first (by `created_at`, then id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFilter {
    pub tenant: TenantId,
    pub status: Option<RunStatus>,
    /// Runs carrying all of these labels.
    pub labels: BTreeMap<String, String>,
    /// `Some(true)`: only quarantined runs; `Some(false)`: only the others.
    pub quarantined: Option<bool>,
    /// Runs after this one in the listing order: the last run of the previous page.
    pub after: Option<RunCursor>,
    pub limit: Option<usize>,
}

impl RunFilter {
    /// Every run of the tenant.
    pub fn new(tenant: TenantId) -> Self {
        Self {
            tenant,
            status: None,
            labels: BTreeMap::new(),
            quarantined: None,
            after: None,
            limit: None,
        }
    }
}

/// A position in a listing of runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunCursor {
    pub created_at: Timestamp,
    pub id: RunId,
}

impl From<&RunRecord> for RunCursor {
    fn from(record: &RunRecord) -> Self {
        Self {
            created_at: record.created_at,
            id: record.run.id,
        }
    }
}

/// The runs `expire_due_runs` found past their deadline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpiredRuns {
    /// `cancel_requested { reason: "ttl" }` was enqueued for these.
    pub cancelled: Vec<RunRef>,
    /// Quarantined runs past their deadline: nothing was enqueued, and the engine gives them
    /// up. They are reported until then.
    pub quarantined: Vec<RunRef>,
}
