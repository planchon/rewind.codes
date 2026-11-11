use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use ptc_core::{
    Completion, Digest, Epoch, HistoryEntry, Payload, RunId, RunStarted, RunStatus, Script, Seq,
    Timestamp, Toolset,
};

use super::executor::CompiledScript;

/// The right to write a run's history. Acquiring a lease increments the run's fence, and a
/// write only succeeds while the fence still matches: a worker that lost its lease can never
/// corrupt a history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub run_id: RunId,
    pub fence: u64,
}

/// Given by the harness when it starts a run, and stored with the run. The script never sees
/// it and it is not part of the journal's events, so it is never replayed and never ends up
/// in a cassette (`archi/05-testing-and-observability.md`, "Run metadata").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunMetadata {
    pub labels: BTreeMap<String, String>,
    pub trace_context: Option<TraceContext>,
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
    pub id: RunId,
    pub started: RunStarted,
    pub metadata: RunMetadata,
    pub created_at: Timestamp,
}

/// The run a `create_run` resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Created {
    pub run_id: RunId,
    /// `false` when the idempotency key was already used: `run_id` is the existing run.
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

/// What wakes a run: an item of its inbox.
#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    RunStarted(RunStarted),
    Completion(Completion),
}

#[derive(Debug, Clone, PartialEq)]
pub struct InboxItem {
    /// Unique, and increasing in arrival order.
    pub id: u64,
    pub trigger: Trigger,
    pub received_at: Timestamp,
}

/// Everything an activation commits, in one transaction: the history entries (complete
/// "trigger → response" groups), the inbox items they consumed, and the effects derived from
/// the new commands (outbox pattern).
#[derive(Debug, Clone, PartialEq)]
pub struct Activation {
    pub entries: Vec<HistoryEntry>,
    /// Ids of the inbox items consumed, delivered or dropped.
    pub consumed: Vec<u64>,
    pub tool_calls: Vec<NewToolTask>,
    pub timers: Vec<NewTimer>,
    pub cancelled_timers: Vec<Seq>,
    /// The run's status after the entries. A terminal status clears the run's timers, tool
    /// tasks and inbox.
    pub status: RunStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewToolTask {
    pub seq: Seq,
    pub tool: String,
    pub args: Payload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewTimer {
    pub seq: Seq,
    pub fire_at: Timestamp,
}

/// A tool call to execute, claimed by a dispatcher until `lease_until`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolTask {
    pub run_id: RunId,
    pub seq: Seq,
    pub tool: String,
    pub args: Payload,
    /// 1 on the first claim. A higher attempt means a previous claim expired without an
    /// outcome: the dispatcher may have crashed mid-call.
    pub attempt: u32,
}

/// The runs `expire_due_runs` found past their deadline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpiredRuns {
    /// `cancel_requested { reason: "ttl" }` was enqueued for these.
    pub cancelled: Vec<RunId>,
    /// Quarantined runs past their deadline: nothing was enqueued, and the engine gives them
    /// up with `run_aborted { non_determinism }`.
    pub quarantined: Vec<RunId>,
}

/// A run as the store knows it. The status and lengths are copies kept for queries; the
/// history is authoritative.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub id: RunId,
    pub script: Digest,
    pub toolset: Digest,
    pub status: RunStatus,
    pub epoch: Epoch,
    pub expires_at: Timestamp,
    pub history_len: u64,
    /// The replay mismatch of a quarantined run (service state, not in the history).
    pub quarantine: Option<String>,
    pub metadata: RunMetadata,
    pub idempotency_key: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFilter {
    pub status: Option<RunStatus>,
    /// Runs carrying all of these labels.
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredScript {
    pub script: Script,
    /// Only scripts that compile are stored: one that does not is refused at start.
    pub compiled: CompiledScript,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The lease was lost: another worker acquired the run since.
    #[error("the lease on run {run_id} is lost (fence {fence} is stale)")]
    Fenced { run_id: RunId, fence: u64 },
    #[error("run {0} not found")]
    RunNotFound(RunId),
    /// A write that would break an invariant of the store, such as a history index that is
    /// already taken.
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("storage backend error: {0}")]
    Backend(String),
}

/// Scripts, toolsets, runs, histories, inboxes, timers and tool tasks
/// (`archi/06-storage-and-scale.md`).
///
/// The contract, checked by the conformance suite (`conformance` feature):
///
/// - **Commit invariant.** `commit_activation` writes everything an activation produced in
///   one transaction, and only if the lease's fence still matches the run's.
/// - **Single writer.** `acquire_runnable` and `acquire_run` increment the fence and hand
///   out at most one live lease per run.
/// - **Idempotency keys.** `create_run` with a key already used returns the existing run;
///   `enqueue` with a key already seen for the run does nothing.
/// - **Epochs and quarantine.** `acquire_runnable` only returns runs of the given epochs that
///   are not quarantined. `quarantine` releases the lease without committing anything.
/// - **Terminal runs.** A terminal commit clears the run's timers, tool tasks and inbox, and
///   nothing is enqueued for a terminal run any more.
///
/// Every method that depends on time takes `now`: stores never read a clock.
pub trait JournalStore: Send + Sync {
    /// Stores a script and its compiled form. Idempotent by digest.
    fn put_script(
        &self,
        script: &Script,
        compiled: &CompiledScript,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn get_script(
        &self,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<StoredScript>, StoreError>> + Send;

    /// Stores a toolset. Idempotent by digest.
    fn put_toolset(&self, toolset: &Toolset)
    -> impl Future<Output = Result<(), StoreError>> + Send;

    fn get_toolset(
        &self,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<Toolset>, StoreError>> + Send;

    /// Creates a run and enqueues its `run_started`. With an idempotency key already used,
    /// creates nothing and returns the existing run.
    fn create_run(
        &self,
        run: NewRun,
        idempotency_key: Option<String>,
    ) -> impl Future<Output = Result<Created, StoreError>> + Send;

    /// Enqueues a completion in the run's inbox. A key already seen for the run, or a terminal
    /// run, enqueues nothing. Keys are kept for the life of the run.
    fn enqueue(
        &self,
        run_id: RunId,
        completion: Completion,
        idempotency_key: Option<String>,
        now: Timestamp,
    ) -> impl Future<Output = Result<Enqueued, StoreError>> + Send;

    /// Leases a run that has something in its inbox, is not leased, is not quarantined, and
    /// whose epoch is in `epochs`. The run whose oldest inbox item is oldest goes first.
    fn acquire_runnable(
        &self,
        owner: &str,
        epochs: &[Epoch],
        now: Timestamp,
        lease_duration: Duration,
    ) -> impl Future<Output = Result<Option<Lease>, StoreError>> + Send;

    /// Leases a given run if it is not terminal and not leased, quarantined or not. Used to
    /// end a quarantined run.
    fn acquire_run(
        &self,
        run_id: RunId,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
    ) -> impl Future<Output = Result<Option<Lease>, StoreError>> + Send;

    /// The history from index `from`.
    fn load_history(
        &self,
        run_id: RunId,
        from: u64,
    ) -> impl Future<Output = Result<Vec<HistoryEntry>, StoreError>> + Send;

    /// The run's inbox, in arrival order. Items stay in the inbox until a commit consumes
    /// them.
    fn take_inbox(
        &self,
        lease: &Lease,
    ) -> impl Future<Output = Result<Vec<InboxItem>, StoreError>> + Send;

    /// Commits an activation atomically, if the lease's fence still matches. The entries must
    /// continue the history exactly (the next index first).
    fn commit_activation(
        &self,
        lease: &Lease,
        activation: Activation,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Gives the lease up. Does nothing if the fence no longer matches.
    fn release(&self, lease: &Lease) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Marks the run quarantined with the replay mismatch, and releases the lease without
    /// committing anything.
    fn quarantine(
        &self,
        lease: &Lease,
        mismatch: String,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// Makes a quarantined run runnable again. Returns whether it was quarantined.
    fn release_quarantine(
        &self,
        run_id: RunId,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Enqueues `timer_fired` for every timer due at `now`, and deletes those timers.
    /// Returns how many fired.
    fn fire_due_timers(
        &self,
        now: Timestamp,
    ) -> impl Future<Output = Result<usize, StoreError>> + Send;

    /// The earliest `fire_at` of all timers.
    fn next_timer(&self) -> impl Future<Output = Result<Option<Timestamp>, StoreError>> + Send;

    /// Enqueues `cancel_requested { reason: "ttl" }`, once, for every run past its deadline
    /// at `now` that is neither terminal nor quarantined, and reports the quarantined ones.
    fn expire_due_runs(
        &self,
        now: Timestamp,
    ) -> impl Future<Output = Result<ExpiredRuns, StoreError>> + Send;

    /// The earliest deadline of the runs that are neither terminal nor already expired.
    fn next_deadline(&self) -> impl Future<Output = Result<Option<Timestamp>, StoreError>> + Send;

    /// Claims up to `limit` tool tasks that are not claimed, or whose claim expired, in
    /// `(run, seq)` order. Each claim increments the task's attempt.
    fn claim_tool_tasks(
        &self,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<ToolTask>, StoreError>> + Send;

    /// Deletes the task and enqueues its outcome in the run's inbox, atomically. The outcome
    /// of a task that no longer exists (its run ended) is dropped.
    fn complete_tool_task(
        &self,
        task: &ToolTask,
        outcome: Completion,
        now: Timestamp,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn get_run(
        &self,
        run_id: RunId,
    ) -> impl Future<Output = Result<Option<RunRecord>, StoreError>> + Send;

    /// Runs matching the filter, oldest first.
    fn list_runs(
        &self,
        filter: &RunFilter,
    ) -> impl Future<Output = Result<Vec<RunRecord>, StoreError>> + Send;
}
