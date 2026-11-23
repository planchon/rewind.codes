//! Runs as the store knows them: creation, run metadata, the record kept for queries,
//! quarantine and retention.

use std::collections::BTreeMap;

use rewind_core::{Attachments, RunId, Timestamp};

use super::workflows::{RunWorkflow, StartedBy};
use crate::ports::tenancy::RunRef;

/// Given by the harness when it starts a run, and stored with the run. The script never sees
/// it and it is not part of the journal's events, so it is never replayed and never ends up
/// in a cassette (`archi/05-testing-and-observability.md`, "Run metadata"). The tenant is in
/// the run's [`RunRef`], and the idempotency key is given to `create_run` beside it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunMetadata {
    /// A bounded string map (agent, conversation…), to filter runs and as span attributes.
    pub labels: BTreeMap<String, String>,
    pub trace_context: Option<TraceContext>,
    /// For a fork, the run it was forked from, in the same tenant, and where it was forced live
    /// (`archi/03-execution.md`, "Forks").
    pub parent: Option<ForkParent>,
    /// For a run of a workflow, the workflow and the version it executes.
    pub workflow: Option<RunWorkflow>,
    /// For a run a script started, that run and the call (`archi/03-execution.md`, "Workflows
    /// across runs"). `create_run` makes the starter its first watcher.
    pub started_by: Option<StartedBy>,
    /// The labels every run this run's scripts reach carries: the allowed labels of the API key
    /// that started the first run of its tree, inherited by every run it starts. Empty: any run
    /// of the tenant.
    pub scope: BTreeMap<String, String>,
    /// The secrets the run attached at start, by server or API: the request's, a fork's parent's
    /// unless the fork gave its own, a workflow version's (`archi/04-tools.md`, "Secrets"). Its
    /// calls read these and no other. Names, never values: the `runs.secrets` column.
    pub secrets: Attachments,
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
