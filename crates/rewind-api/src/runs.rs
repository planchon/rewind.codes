//! Runs: start with the secrets they attach, state, list, trace, signals, cancellation, forks,
//! journal export, quarantine, deletion (`archi/01-product.md`, `archi/03-execution.md`).
//!
//! The journal entries and the event stream of a run are in [`crate::events`].

use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::calls::{CallState, ParkedReason};
use crate::common::{
    AbortReason, ActionFailure, Callsite, Duration, Labels, LogLevel, Payload, RunMode, RunStatus,
    Timestamp,
};
use crate::namespaces::NamespaceName;
pub use crate::query::WaitKind;
use crate::secrets::Attachments;
use crate::workflows::{RunWorkflow, StartedBy};

/// `POST /v1/runs`: starts a run. The script is compiled, checked and type-checked first; a
/// script that cannot run is refused (`invalid-script`) before any run exists.
///
/// With the `Idempotency-Key` header, a request repeated with the same key returns the run the
/// first one started, instead of starting a second one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({
    "script": "import { github } from \"rewind:actions\";\nexport default async function main(input: { repo: string }) {\n  return (await github.list_issues({ repo: input.repo })).length;\n}\n",
    "actionSet": "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4",
    "input": { "repo": "acme/api" },
    "wait": "30s",
    "labels": { "agent": "triage", "conversation": "c-42" },
    "secrets": { "github": "ns/github" }
}))]
pub struct StartRun {
    /// The TypeScript source: a single ES module whose default export is the entry point.
    /// Without it, `workflow` names what the run executes.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub script: String,
    /// The action set the script imports from `rewind:actions`. Without one, `rewind:actions` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_set: Option<String>,
    /// Instead of `script` and `actionSet`: a run of this workflow, `triage` for its latest version
    /// or `triage@<version>` (12 hex digits at least) or `triage@v12` (its number) for one
    /// version. Its input is checked against the workflow's input schema (`invalid-input`); the
    /// script is not checked again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// The input of `main`. `{}` when absent.
    #[serde(default = "empty_object")]
    pub input: Value,
    /// Hold the answer until the run ends or is quarantined, or until this long has passed,
    /// however long it is: one request waits, and nothing is asked again meanwhile. Without it,
    /// the answer comes as soon as the run exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Duration>,
    /// Run metadata: filters runs, scopes `list_runs`, API keys and webhooks. The script never
    /// sees them.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
    /// W3C trace context of the caller (typically the LLM call that wrote the script): the run
    /// appears under it in the caller's trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
    /// The run's TTL: it is cancelled at its deadline. 30 days by default, up to the
    /// deployment's maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Duration>,
    /// Type-check the script before starting it. `true` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<bool>,
    /// The secrets the run reads, by MCP server or API: `{ "github": "ns/github", "zendesk":
    /// "org/zendesk" }` (`org/<name>`, the organization's; `ns/<name>`, the run's namespace's).
    /// Required for each server the script imports whose `auth` takes a secret; checked against
    /// the caller's rights (`secret-required`). A run of a `workflow` gives none: its version's
    /// are read.
    #[serde(default, skip_serializing_if = "Attachments::is_empty")]
    pub secrets: Attachments,
}

impl StartRun {
    pub fn new(script: impl Into<String>) -> Self {
        Self {
            script: script.into(),
            action_set: None,
            workflow: None,
            input: empty_object(),
            wait: None,
            labels: Labels::new(),
            traceparent: None,
            tracestate: None,
            ttl: None,
            check: None,
            secrets: Attachments::new(),
        }
    }
}

pub(crate) fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// The state of a run: a projection of its journal, with its metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    /// The run id (a UUID v7).
    pub id: String,
    /// The run's handle, `<namespace>/<origin>/<suffix>`: what people read, copy and type to
    /// name it. Every path and query parameter that takes a run's id takes its handle too,
    /// percent-encoded in a path (`dana%2Ftriage%2F06bqx3k9f2mt`).
    #[schema(example = "dana/triage/06bqx3k9f2mt")]
    pub handle: String,
    /// The end user the run belongs to. A fork has its parent's.
    pub namespace: NamespaceName,
    pub status: RunStatus,
    /// Set while the run is quarantined: its activations are stopped until it is released or
    /// given up. Quarantine is not a status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine: Option<Quarantine>,
    /// Digest of the TypeScript source (`GET /v1/scripts/{digest}`).
    pub script: String,
    /// The script's title: the one a person gave it (`PATCH /v1/scripts/{digest}`), else the
    /// first line of the comment at its top. Absent when it has neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_title: Option<String>,
    /// Digest of the pinned action set.
    pub action_set: String,
    /// The determinism epoch the run started under.
    pub epoch: u32,
    pub mode: RunMode,
    pub labels: Labels,
    /// The secrets it attached at start, by MCP server or API (`org/<name>`, `ns/<name>`):
    /// names, never values.
    #[serde(default, skip_serializing_if = "Attachments::is_empty")]
    pub secrets: Attachments,
    /// The OpenTelemetry trace the run belongs to: the caller's if it gave a `traceparent`,
    /// else derived from the run id. 32 hex digits.
    pub trace_id: String,
    /// For a fork, the run it was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ForkParent>,
    /// For a run of a workflow, the workflow and its version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<RunWorkflow>,
    /// For a run a script started, that run and the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<StartedBy>,
    /// The input of `main`. Absent while the run is `pending`: its `run_started` waits in its
    /// inbox for the first activation, which commits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Payload>,
    /// With `completed`: what `main` returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Payload>,
    /// With `failed` or `cancelled`: how the script failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RunError>,
    /// With `aborted`: why the host ended the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort: Option<AbortReason>,
    /// With `active`: what the run waits for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_for: Vec<Waiting>,
    /// The last log lines of the script (`console.*`), oldest first, at most 20.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<LogLine>,
    /// How many times each call was delivered, by seq, for the calls delivered more than once: a
    /// call delivered again after a transient failure, or after its worker or dispatcher died.
    /// Deliveries live in the calls' tasks, not in the history: the read model keeps them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliveries: Vec<Deliveries>,
    /// Number of journal entries.
    pub history_len: u64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    /// The deadline: the run is cancelled then (reason `ttl`).
    pub expires_at: Timestamp,
}

/// Why a run is quarantined (`archi/03-execution.md`, "Non-determinism and quarantine").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Quarantine {
    pub cause: QuarantineCause,
    /// The replay mismatch, or the last error.
    pub reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineCause {
    /// A replay diverged from the journal: a fault of the platform, never of the script.
    NonDeterminism,
    /// Its activations failed 5 times in a row.
    RepeatedFailures,
}

/// The run a fork was started from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ForkParent {
    /// The parent's id.
    pub run: String,
    /// The parent's handle; its id once it is erased.
    pub handle: String,
    /// The entry from which the fork was forced live, if the request gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
}

/// How a script failed, with where, for a model to fix it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunError {
    /// `ActionError`, `TypeError`, `DeadlockError`, `CancelledError`…
    pub name: String,
    pub message: String,
    /// Source-mapped to `script.ts`, without the runtime's frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// The innermost frame of the stack in `script.ts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Callsite>,
    /// A few lines of the source around `location`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    /// The call whose failure the script did not catch, if that is how it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_call: Option<FailedCall>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailedCall {
    pub seq: u32,
    pub action: String,
    pub args: Payload,
    pub error: ActionFailure,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsite: Option<Callsite>,
}

/// How many times a call was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Deliveries {
    pub seq: u32,
    /// From 1.
    pub deliveries: u32,
}

/// Something an active run waits for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Waiting {
    /// A call in flight, and its state.
    #[serde(rename_all = "camelCase")]
    Call {
        seq: u32,
        action: String,
        /// The call's arguments: what the harness approves or rejects.
        args: Payload,
        /// For `POST /v1/calls/{id}/approve` and `/reject`.
        call_id: String,
        state: CallState,
        /// With `parked`: why.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parked: Option<ParkedReason>,
        /// The delivery in progress, from 1.
        delivery: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<Callsite>,
    },
    /// A durable timer: `sleep`, `setTimeout`, `setInterval`, or a signal's timeout.
    #[serde(rename_all = "camelCase")]
    Timer {
        seq: u32,
        fire_at: Timestamp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<Callsite>,
    },
    /// `await signal(name)` with an empty mailbox.
    #[serde(rename_all = "camelCase")]
    Signal {
        name: String,
        /// When the wait gives up and returns `undefined`, if it has a timeout.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_at: Option<Timestamp>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<Callsite>,
    },
}

/// One line of the script's console.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    /// The journal entry.
    pub idx: u64,
    pub at: Timestamp,
    pub level: LogLevel,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsite: Option<Callsite>,
}

/// `GET /v1/runs/{id}`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GetRunQuery {
    /// Hold the answer until the run ends or is quarantined, or until this long has passed,
    /// however long it is: one request waits, and nothing is asked again meanwhile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>)]
    pub wait: Option<Duration>,
}

/// `GET /v1/runs`: runs, most recently started first unless `sort` says otherwise. A key
/// limited to labels only sees runs carrying them.
///
/// A query runs where its runs are (`archi/05-testing-and-observability.md`, "Query language"):
/// in the partition of its namespace when it names one, in every partition otherwise, each
/// reading at most its scan budget of summaries for one page. A page cut by the budget holds
/// fewer runs than `limit`, possibly none, and a `nextCursor` that resumes where it stopped.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct RunFilter {
    /// A query (`archi/05-testing-and-observability.md`, "Query language"): terms separated by
    /// spaces, all of which must hold, as `@agent:triage status:failed,aborted -@env:test
    /// "rate limit"`. Joined to the other parameters by "and". A query that cannot be read is
    /// refused with `invalid-query`, which says where the term at fault is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// `started` (the default): the most recently started first; `oldest`: the first started
    /// first; `activity`: the most recent activity first; `wakes`: the soonest to wake by itself
    /// first, the runs that do not wake by themselves last; `duration`: the longest from start to
    /// end first, the runs that have not ended last; `calls`: the most calls first. Ties go
    /// to the most recently started (`wakes`: the first started).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(inline)]
    pub sort: Option<RunSort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(inline)]
    pub status: Option<RunStatus>,
    /// Only runs carrying this label, as `key:value`. Repeat to require several.
    #[serde(default, rename = "label", skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Only runs that called this action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Only runs of this script (digest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Only runs whose calls failed with this kind (`error`, `timeout`…), or that failed
    /// with this error name (`TypeError`, `DeadlockError`…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Only quarantined runs, or only runs that are not.
    #[serde(
        default,
        deserialize_with = "flag",
        skip_serializing_if = "Option::is_none"
    )]
    pub quarantined: Option<bool>,
    /// Only forks of this run: its id or its handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Only runs of this workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// Only runs this run started: its id or its handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<String>,
    /// Only runs started at or after this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub started_after: Option<Timestamp>,
    /// Only runs started before this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub started_before: Option<Timestamp>,
    /// At most this many runs (50 by default, 500 at most). A page may hold fewer, even when
    /// more match: see `nextCursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// The `nextCursor` of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// A page of runs. It may hold fewer runs than `limit`, possibly none, and still have a
/// `nextCursor`: the server stopped at its scan budget. Only a page without `nextCursor` is the
/// last.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunPage {
    pub runs: Vec<RunSummary>,
    /// Pass it as `cursor` to read the next page; absent on the last page only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// The order of `GET /v1/runs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunSort {
    /// The most recently started first.
    #[default]
    Started,
    /// The first started first.
    Oldest,
    /// The most recent activity first.
    Activity,
    /// The soonest to wake by itself first (a timer, a signal's timeout); the runs that do not
    /// wake by themselves last.
    Wakes,
    /// The longest first, from start to end; the runs that have not ended last, the most
    /// recently started first.
    Duration,
    /// The most calls first.
    Calls,
}

impl RunSort {
    pub fn as_str(self) -> &'static str {
        match self {
            RunSort::Started => "started",
            RunSort::Oldest => "oldest",
            RunSort::Activity => "activity",
            RunSort::Wakes => "wakes",
            RunSort::Duration => "duration",
            RunSort::Calls => "calls",
        }
    }
}

/// A run in a list, from the read model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub id: String,
    /// The run's handle, `<namespace>/<origin>/<suffix>`, as `Run.handle`.
    #[schema(example = "dana/triage/06bqx3k9f2mt")]
    pub handle: String,
    pub namespace: NamespaceName,
    pub status: RunStatus,
    pub quarantined: bool,
    pub script: String,
    /// The script's title, as `Run.scriptTitle`. Absent when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_title: Option<String>,
    /// The first line of the script that is not blank, for display.
    pub headline: String,
    pub action_set: String,
    pub labels: Labels,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    pub last_activity_at: Timestamp,
    pub calls: u64,
    /// Calls that failed.
    pub errors: u64,
    pub history_len: u64,
    /// With `failed` or `cancelled`: the error's name. With `aborted`: the abort's kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// With `error`: the error's or the abort's message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// With `error`: the innermost line of `script.ts` in the error's stack, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_line: Option<u32>,
    /// With `error`: the failure group (`GET /v1/runs/failures`, `fingerprint:` in a query).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// With `active`: what the run waits for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_for: Vec<RunWait>,
    /// With `active`: when the run wakes by itself, if a timer or a signal's timeout wakes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wakes_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// For a run of a workflow, the workflow's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// For a run a script started, that run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<String>,
}

/// Something a run in a list waits for. `GET /v1/runs/{id}` has the details (`waitingFor`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunWait {
    pub kind: WaitKind,
    /// The timer's or the call's seq.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u32>,
    /// The signal's name, or the call's action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// When the wait began.
    pub since: Timestamp,
    /// When the wait ends by itself: a timer's due time, a signal's timeout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsite: Option<Callsite>,
}

/// `GET /v1/runs/stats`: the runs of a scope started in a window, from the hourly counters
/// (`archi/05-testing-and-observability.md`, "Statistics").
///
/// The statistics routes (`GET /v1/runs/stats`, `GET /v1/runs/failures`, `GET /v1/scripts`,
/// `GET /v1/actions/stats`) take a scope and a window only. The scope is the caller's tenant, or one
/// namespace: a token's, the `Rewind-Namespace` header's, or the one `q` names with a single
/// `namespace:` term. The window is made of whole hours in UTC: its bounds are rounded out to the
/// hour. A grant or a token limited to labels reads none of them (`forbidden`): counters do not
/// know labels.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct RunStatsQuery {
    /// The scope, as a query: one term `namespace:<name>` of one value, or nothing for the
    /// tenant. Any other term is refused with its position (`invalid-query`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Runs started at or after this time, rounded down to the hour. No lower bound by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub since: Option<Timestamp>,
    /// Runs started before this time, rounded up to the hour. Now by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub until: Option<Timestamp>,
    /// The width of the series' buckets (`"1h"`, `"1d"`), from `since`, which it requires; whole
    /// hours, a shorter or uneven width rounded up to them; 500 buckets at most. No series
    /// without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>)]
    pub bucket: Option<Duration>,
    /// Values per facet (10 by default, 50 at most).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<u32>,
}

/// The runs of a scope. Counts, series, durations and facets are over the runs started in the
/// window, from the hourly counters; `since`, `until` and `bucket` are the window and the width
/// the answer used, in whole hours. `waiting` and `quarantined` are over the active runs of the
/// scope now, whenever they started, from the gauge of the active runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Timestamp>,
    pub until: Timestamp,
    /// The width of the series' buckets, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub bucket: Option<Duration>,
    pub runs: u64,
    pub statuses: StatusCounts,
    /// One bucket per `bucket` from `since`, the runs started in it by status.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<SeriesBucket>,
    /// From start to end, in milliseconds, over the runs that ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durations: Option<DurationStats>,
    /// Waiting runs by what they wait for; a run waiting for several kinds counts in each.
    pub waiting: WaitCounts,
    pub quarantined: u64,
    pub facets: Facets,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatusCounts {
    pub pending: u64,
    pub active: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub aborted: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WaitCounts {
    pub signal: u64,
    pub timer: u64,
    pub call: u64,
    pub harness: u64,
    pub approval: u64,
    pub parked: u64,
    pub deferred: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeriesBucket {
    /// The bucket's start.
    pub at: Timestamp,
    pub statuses: StatusCounts,
}

/// Nearest-rank percentiles, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DurationStats {
    pub count: u64,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

/// The most frequent values among the runs, most frequent first: what to suggest in a query
/// bar, and the counts beside a list. The counters count the runs of the window by status, by
/// workflow and by namespace; the facets they do not count are empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Facets {
    pub statuses: Vec<FacetValue>,
    /// Workflows, by name: the runs of each.
    #[serde(default)]
    pub workflows: Vec<FacetValue>,
    /// The most frequent label keys, each with its most frequent values.
    pub labels: Vec<LabelFacet>,
    pub actions: Vec<FacetValue>,
    /// Errors the runs ended with (names and abort kinds), and kinds their calls failed with.
    pub errors: Vec<FacetValue>,
    /// Script digests.
    pub scripts: Vec<FacetValue>,
    pub waiting: Vec<FacetValue>,
    /// Signals waited for.
    pub signals: Vec<FacetValue>,
    /// Namespaces, for a reader of every namespace; a reader of one sees only its own.
    #[serde(default)]
    pub namespaces: Vec<FacetValue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FacetValue {
    pub value: String,
    pub runs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LabelFacet {
    pub key: String,
    /// Runs carrying the key.
    pub runs: u64,
    pub values: Vec<FacetValue>,
}

/// `GET /v1/runs/failures`: the failed, cancelled and aborted runs of a scope, ended in a window,
/// grouped by fingerprint, from the hourly counters. A scope and a window only, as
/// [`RunStatsQuery`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct FailureQuery {
    /// The scope, as a query: one term `namespace:<name>` of one value, or nothing for the
    /// tenant. Any other term is refused with its position (`invalid-query`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Runs ended at or after this time, rounded down to the hour. No lower bound by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub since: Option<Timestamp>,
    /// Runs ended before this time, rounded up to the hour. Now by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub until: Option<Timestamp>,
    /// The width of each group's series' buckets, from `since`, which it requires; whole hours,
    /// a shorter or uneven width rounded up to them; 500 buckets at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>)]
    pub bucket: Option<Duration>,
    /// Groups at most (50 by default, 500 at most).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailureGroupPage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<Timestamp>,
    pub until: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub bucket: Option<Duration>,
    /// Runs in every group.
    pub runs: u64,
    /// Groups in all, beyond `limit` included.
    pub total_groups: u64,
    /// The most runs first, then the latest ending first.
    pub groups: Vec<FailureGroup>,
}

/// The runs that ended the same way: the same error name, and the same message once numbers,
/// ids, times and double-quoted strings are set aside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailureGroup {
    /// 16 hexadecimal digits: `fingerprint:` in a query.
    pub fingerprint: String,
    /// The error's name and message with placeholders (`<n>`, `<id>`, `<uuid>`, `<time>`,
    /// `<str>`) where they vary.
    pub signature: String,
    /// The error's name, or the abort's kind.
    pub error: String,
    pub runs: u64,
    /// One count per bucket from `since`: the runs that ended in it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<u64>,
    /// Distinct scripts.
    pub scripts: u64,
    pub first_ended_at: Timestamp,
    pub last_ended_at: Timestamp,
    /// The line of `script.ts` most of its runs ended at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Its runs' most frequent labels, as `key:value`.
    pub labels: Vec<FacetValue>,
    /// The run that ended last.
    pub latest: FailureSample,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailureSample {
    pub id: String,
    /// Its handle, `<namespace>/<origin>/<suffix>`.
    pub handle: String,
    pub status: RunStatus,
    pub message: String,
}

/// `POST /v1/runs/{id}/signals/{name}`: the signal was put in the run's mailbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SignalAccepted {
    pub run_id: String,
    pub name: String,
    /// The `Idempotency-Key` was already used for this run: nothing was added.
    pub duplicate: bool,
}

/// `POST /v1/runs/{id}/cancel`: every pending await of the script rejects with
/// `CancelledError`; the script gets one activation to clean up.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelRun {
    /// The message of the `CancelledError`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Hold the answer until the run ends or is quarantined, or until this long has passed, as
    /// `POST /v1/runs` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Duration>,
}

/// `POST /v1/runs/{id}/fork` (`retry_run`): a new run with a corrected script. While its
/// commands match the parent's, the parent's recorded results are reused and no action is called
/// again; it continues live at the first difference, at `at`, or at the end of the parent's
/// history (`archi/03-execution.md`, "Forks"). Takes `Idempotency-Key` as `POST /v1/runs`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForkRun {
    /// The corrected TypeScript source.
    pub script: String,
    /// Go live from this journal entry even if the commands still match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
    /// The fork's input. The parent's when absent.
    #[serde(
        default,
        deserialize_with = "crate::common::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub input: Option<Value>,
    /// Hold the answer until the fork ends or is quarantined, or until this long has passed, as
    /// `POST /v1/runs` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Duration>,
    /// The fork's labels. Absent, the fork keeps its parent's labels, so that whoever sees the
    /// parent (the agent tools of a conversation, a key limited to labels) sees the fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Labels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
    /// A new TTL, from the fork's start. 30 days by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Duration>,
    /// Type-check the corrected script first. `true` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<bool>,
    /// The fork's attachments, as `POST /v1/runs` takes them. The parent's when absent. Either
    /// way they are checked against the caller: the script is new code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Attachments>,
}

impl ForkRun {
    pub fn new(script: impl Into<String>) -> Self {
        Self {
            script: script.into(),
            at: None,
            input: None,
            wait: None,
            labels: None,
            traceparent: None,
            tracestate: None,
            ttl: None,
            check: None,
            secrets: None,
        }
    }
}

/// `GET /v1/runs/{id}/trace`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TraceQuery {
    /// `llm` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(inline)]
    pub format: Option<TraceFormat>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceFormat {
    /// A compact text trace for a model: one line per call, timer and signal, with its
    /// line in `script.ts`; payloads truncated (`archi/05-testing-and-observability.md`).
    #[default]
    Llm,
}

impl TraceFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            TraceFormat::Llm => "llm",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Trace {
    pub run_id: String,
    pub format: TraceFormat,
    pub text: String,
}

/// `GET /v1/runs/{id}/journal`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct JournalQuery {
    /// `journal` by default.
    #[serde(default, rename = "as", skip_serializing_if = "Option::is_none")]
    #[param(inline)]
    pub export_as: Option<JournalExport>,
}

/// What `GET /v1/runs/{id}/journal` exports, as a document of the stored format v1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JournalExport {
    /// The journal document: replayable with `rewind replay`, without a server.
    #[default]
    Journal,
    /// A cassette of the run's calls and signals, for `rewind test` and `POST /v1/test`.
    Cassette,
}

impl JournalExport {
    pub fn as_str(self) -> &'static str {
        match self {
            JournalExport::Journal => "journal",
            JournalExport::Cassette => "cassette",
        }
    }
}

/// A query flag: `1`, `true`, `0` or `false`.
pub(crate) fn flag<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    struct FlagVisitor;

    impl Visitor<'_> for FlagVisitor {
        type Value = Option<bool>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("1, 0, true or false")
        }

        fn visit_bool<E: de::Error>(self, value: bool) -> Result<Option<bool>, E> {
            Ok(Some(value))
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> Result<Option<bool>, E> {
            match value {
                0 => Ok(Some(false)),
                1 => Ok(Some(true)),
                _ => Err(E::custom("expected 1 or 0")),
            }
        }

        fn visit_str<E: de::Error>(self, value: &str) -> Result<Option<bool>, E> {
            match value {
                "1" | "true" => Ok(Some(true)),
                "0" | "false" => Ok(Some(false)),
                other => Err(E::custom(format!(
                    "expected 1, 0, true or false, not {other:?}"
                ))),
            }
        }
    }

    deserializer.deserialize_any(FlagVisitor)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn waiting_is_tagged_by_kind_with_camel_case_fields() {
        let waiting = Waiting::Signal {
            name: "approval".into(),
            timeout_at: Some(Timestamp::from_millisecond(1_000).unwrap()),
            callsite: Some(Callsite {
                line: 10,
                column: 26,
            }),
        };
        assert_eq!(
            serde_json::to_value(&waiting).unwrap(),
            json!({
                "kind": "signal",
                "name": "approval",
                "timeoutAt": "1970-01-01T00:00:01Z",
                "callsite": { "line": 10, "column": 26 }
            })
        );
        let call = Waiting::Call {
            seq: 3,
            action: "ask_user".into(),
            args: Payload::of(
                json!({ "question": "ok?" }),
                crate::common::PAYLOAD_VALUE_LIMIT,
            ),
            call_id: "t-1".into(),
            state: CallState::Approval,
            parked: None,
            delivery: 1,
            callsite: None,
        };
        let written = serde_json::to_value(&call).unwrap();
        assert_eq!(written["callId"], "t-1");
        assert_eq!(written["args"]["value"], json!({ "question": "ok?" }));
        assert_eq!(written["state"], "approval");
        assert_eq!(serde_json::from_value::<Waiting>(written).unwrap(), call);
    }

    #[test]
    fn a_fork_keeps_a_null_input_apart_from_no_input() {
        let fork: ForkRun =
            serde_json::from_value(json!({ "script": "x", "input": null })).unwrap();
        assert_eq!(fork.input, Some(Value::Null));
        let fork: ForkRun = serde_json::from_value(json!({ "script": "x" })).unwrap();
        assert_eq!(fork.input, None);
    }

    #[test]
    fn a_start_reads_durations_and_refuses_unknown_fields() {
        let start: StartRun =
            serde_json::from_value(json!({ "script": "x", "wait": "30s", "ttl": 60000 })).unwrap();
        assert_eq!(start.wait.unwrap().as_millis(), 30_000);
        assert_eq!(start.ttl.unwrap().as_millis(), 60_000);
        assert_eq!(start.input, json!({}));
        assert!(
            serde_json::from_value::<StartRun>(json!({ "script": "x", "idempotencyKey": "k" }))
                .is_err()
        );
    }
}
