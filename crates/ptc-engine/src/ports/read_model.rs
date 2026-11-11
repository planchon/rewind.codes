use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use ptc_core::{Digest, HistoryEntry, RunId, RunStatus, Shape, Timestamp, ToolFailureKind};

use crate::ports::tenancy::{RunRef, TenantId, TenantScope};

/// What the projector knows of a run besides its entries: the run's metadata, from its record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedRun {
    pub run: RunRef,
    pub script: Digest,
    pub toolset: Digest,
    pub labels: BTreeMap<String, String>,
}

/// One row of `run_summaries`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub run: RunRef,
    pub script: Digest,
    pub toolset: Digest,
    /// `active` until a terminal entry is projected; never `pending`.
    pub status: RunStatus,
    pub labels: BTreeMap<String, String>,
    /// `recorded_at` of `run_started`, once projected.
    pub started_at: Option<Timestamp>,
    /// `recorded_at` of the terminal entry.
    pub ended_at: Option<Timestamp>,
    /// The latest `recorded_at` projected.
    pub last_activity_at: Timestamp,
    /// `schedule_tool_call` entries projected.
    pub tool_calls: u64,
    /// `tool_call_failed` entries projected.
    pub errors: u64,
    /// The highest index projected, plus one.
    pub history_len: u64,
}

/// Which runs `search_runs` returns, newest run id first (run ids are UUID v7: newest created
/// first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSearch {
    pub tenant: TenantId,
    pub status: Option<RunStatus>,
    /// Runs carrying all of these labels.
    pub labels: BTreeMap<String, String>,
    pub script: Option<Digest>,
    pub toolset: Option<Digest>,
    /// Runs that scheduled at least one call to this tool.
    pub tool: Option<String>,
    /// Runs with at least one call that failed with this kind.
    pub error_kind: Option<ToolFailureKind>,
    /// Runs started at or after.
    pub started_after: Option<Timestamp>,
    /// Runs started before.
    pub started_before: Option<Timestamp>,
    /// Runs whose id is below this one: the last run of the previous page.
    pub before: Option<RunId>,
    pub limit: usize,
}

impl RunSearch {
    /// The `limit` newest runs of the tenant.
    pub fn new(tenant: TenantId, limit: usize) -> Self {
        Self {
            tenant,
            status: None,
            labels: BTreeMap::new(),
            script: None,
            toolset: None,
            tool: None,
            error_kind: None,
            started_after: None,
            started_before: None,
            before: None,
            limit,
        }
    }
}

/// Which scripts `scripts` returns: every distinct script of the tenant, ordered by digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptFilter {
    pub tenant: TenantId,
    /// Only the runs started at or after this time count.
    pub since: Option<Timestamp>,
}

/// A script, across its runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptSummary {
    pub script: Digest,
    pub runs: u64,
    /// Runs by status.
    pub statuses: BTreeMap<RunStatus, u64>,
    /// The tools its runs called.
    pub tools: BTreeSet<String>,
    pub first_started_at: Option<Timestamp>,
    pub last_started_at: Option<Timestamp>,
}

/// Which calls `tool_stats` aggregates, per tool, ordered by tool name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolStatsFilter {
    pub tenant: TenantId,
    pub toolset: Option<Digest>,
    /// Calls scheduled at or after.
    pub since: Option<Timestamp>,
    /// Calls scheduled before.
    pub until: Option<Timestamp>,
}

/// The calls of one tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolStats {
    pub tool: String,
    /// Calls scheduled.
    pub calls: u64,
    pub completed: u64,
    pub failed: u64,
    pub errors_by_kind: BTreeMap<ToolFailureKind, u64>,
    /// Over the calls with an outcome; `None` when no call has one.
    pub latency: Option<Latency>,
}

/// Latency of calls with an outcome: logical time from `schedule_tool_call` to the completion.
/// Percentiles are nearest-rank: the p-th percentile of n sorted latencies is the one at
/// position `ceil(p × n / 100)`, counting from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Latency {
    pub count: u64,
    /// Rounded down.
    pub mean_ms: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub max_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadModelError {
    #[error("read model backend error: {0}")]
    Backend(String),
}

/// Projections for questions across runs (`archi/05-testing-and-observability.md`, "Read
/// model"), filled by the projector from committed entries, never sampled.
///
/// The contract, checked by the conformance suite:
///
/// - **Idempotent and order-independent.** `apply` is keyed by `(run, index)`: applying the
///   same entries twice, or the batches of a run in any order, gives the same projections.
/// - **Tenants.** Every query is scoped by a tenant.
/// - **Observed shapes.** The results of `tool_call_completed` entries whose value is held are
///   folded into the shape of their tool ([`Shape::merge`]); a result held only by its digest
///   contributes nothing.
/// - **Retention.** `purge` and `delete_run` remove a run's summary and calls.
pub trait ReadModel: Send + Sync {
    /// Projects committed entries of a run.
    fn apply(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
    ) -> impl Future<Output = Result<(), ReadModelError>> + Send;

    fn search_runs(
        &self,
        search: &RunSearch,
    ) -> impl Future<Output = Result<Vec<RunSummary>, ReadModelError>> + Send;

    fn scripts(
        &self,
        filter: &ScriptFilter,
    ) -> impl Future<Output = Result<Vec<ScriptSummary>, ReadModelError>> + Send;

    fn tool_stats(
        &self,
        filter: &ToolStatsFilter,
    ) -> impl Future<Output = Result<Vec<ToolStats>, ReadModelError>> + Send;

    /// The shapes observed in the results of these tools, for those observed at least once.
    fn observed_shapes(
        &self,
        tenant: &TenantId,
        tools: &[String],
    ) -> impl Future<Output = Result<BTreeMap<String, Shape>, ReadModelError>> + Send;

    /// Removes the runs of the tenants in scope that ended before `ended_before`. Returns how
    /// many runs were removed.
    fn purge(
        &self,
        ended_before: Timestamp,
        tenants: &TenantScope,
    ) -> impl Future<Output = Result<usize, ReadModelError>> + Send;

    /// Removes a run's summary and calls. Returns whether the run was projected.
    fn delete_run(&self, run: &RunRef)
    -> impl Future<Output = Result<bool, ReadModelError>> + Send;
}
