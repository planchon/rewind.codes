//! Runs: start, state, list, trace, signals, cancellation, forks, journal export, quarantine,
//! credentials, deletion (`archi/01-product.md`, `archi/03-execution.md`).
//!
//! The journal entries and the event stream of a run are in [`crate::events`].

use std::fmt;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::common::{
    AbortReason, Callsite, Duration, Labels, LogLevel, Payload, RunMode, RunStatus, Timestamp,
    ToolFailure,
};
use crate::tasks::{ParkedReason, TaskState};
use crate::toolsets::Credentials;

/// `POST /v1/runs`: starts a run. The script is compiled, checked and type-checked first; a
/// script that cannot run is refused (`invalid-script`) before any run exists.
///
/// With the `Idempotency-Key` header, a request repeated with the same key returns the run the
/// first one started, instead of starting a second one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({
    "script": "import { github } from \"ptc:tools\";\nexport default async function main(input: { repo: string }) {\n  return (await github.list_issues({ repo: input.repo })).length;\n}\n",
    "toolset": "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4",
    "input": { "repo": "acme/api" },
    "wait": "30s",
    "labels": { "agent": "triage", "conversation": "c-42" }
}))]
pub struct StartRun {
    /// The TypeScript source: a single ES module whose default export is the entry point.
    pub script: String,
    /// The toolset the script imports from `ptc:tools`. Without one, `ptc:tools` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolset: Option<String>,
    /// The input of `main`. `{}` when absent.
    #[serde(default = "empty_object")]
    pub input: Value,
    /// Wait up to this long for the run to end before answering. Without it, the answer comes
    /// as soon as the run exists.
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
    /// Credentials of this run, set before its first call (as `PUT /v1/runs/{id}/credentials`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<Credentials>,
}

impl StartRun {
    pub fn new(script: impl Into<String>) -> Self {
        Self {
            script: script.into(),
            toolset: None,
            input: empty_object(),
            wait: None,
            labels: Labels::new(),
            traceparent: None,
            tracestate: None,
            ttl: None,
            check: None,
            credentials: None,
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
    pub status: RunStatus,
    /// Set while the run is quarantined: its activations are stopped until it is released or
    /// given up. Quarantine is not a status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine: Option<Quarantine>,
    /// Digest of the TypeScript source (`GET /v1/scripts/{digest}`).
    pub script: String,
    /// Digest of the pinned toolset.
    pub toolset: String,
    /// The determinism epoch the run started under.
    pub epoch: u32,
    pub mode: RunMode,
    pub labels: Labels,
    /// The OpenTelemetry trace the run belongs to: the caller's if it gave a `traceparent`,
    /// else derived from the run id. 32 hex digits.
    pub trace_id: String,
    /// For a fork, the run it was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ForkParent>,
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
    /// How many times each tool call was sent, by seq, for the calls sent more than once: a
    /// call retried after a transient failure, or sent again after its worker or dispatcher
    /// died. Attempts live in the tool tasks, not in the journal: the read model keeps them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<CallAttempts>,
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
    pub run: String,
    /// The entry from which the fork was forced live, if the request gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
}

/// How a script failed, with where, for a model to fix it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunError {
    /// `ToolError`, `TypeError`, `DeadlockError`, `CancelledError`…
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
    /// The tool call whose failure the script did not catch, if that is how it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_call: Option<FailedCall>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FailedCall {
    pub seq: u32,
    pub tool: String,
    pub args: Payload,
    pub error: ToolFailure,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsite: Option<Callsite>,
}

/// How many times a tool call was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallAttempts {
    pub seq: u32,
    /// From 1.
    pub attempts: u32,
}

/// Something an active run waits for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Waiting {
    /// A tool call in flight, and the state of its task.
    #[serde(rename_all = "camelCase")]
    ToolCall {
        seq: u32,
        tool: String,
        /// The call's arguments: what the harness approves or rejects.
        args: Payload,
        /// For `POST /v1/tasks/{id}/approve` and `/reject`.
        task_id: String,
        state: TaskState,
        /// With `parked`: why.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parked: Option<ParkedReason>,
        /// The attempt in progress, from 1.
        attempt: u32,
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
    /// Wait up to this long for the run to end before answering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>)]
    pub wait: Option<Duration>,
}

/// `GET /v1/runs`: runs, most recently started first. A key limited to labels only sees runs
/// carrying them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct RunFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(inline)]
    pub status: Option<RunStatus>,
    /// Only runs carrying this label, as `key:value`. Repeat to require several.
    #[serde(default, rename = "label", skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Only runs that called this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Only runs of this script (digest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Only runs whose tool calls failed with this kind (`error`, `timeout`…), or that failed
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
    /// Only forks of this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Only runs started at or after this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub started_after: Option<Timestamp>,
    /// Only runs started before this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub started_before: Option<Timestamp>,
    /// At most this many runs (50 by default, 500 at most).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// The `nextCursor` of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunPage {
    pub runs: Vec<RunSummary>,
    /// Pass it as `cursor` to read the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// A run in a list, from the read model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub id: String,
    pub status: RunStatus,
    pub quarantined: bool,
    pub script: String,
    /// The first line of the script that is not blank, for display.
    pub headline: String,
    pub toolset: String,
    pub labels: Labels,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    pub last_activity_at: Timestamp,
    pub tool_calls: u64,
    /// Tool calls that failed.
    pub errors: u64,
    pub history_len: u64,
    /// With `failed` or `cancelled`: the error's name. With `aborted`: the abort's kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
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
    /// Wait up to this long for the run to end before answering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Duration>,
}

/// `POST /v1/runs/{id}/fork` (`retry_run`): a new run with a corrected script. While its
/// commands match the parent's, the parent's recorded results are reused and no tool is called
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
    /// A compact text trace for a model: one line per tool call, timer and signal, with its
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
    /// The journal document: replayable with `ptc replay`, without a server.
    #[default]
    Journal,
    /// A cassette of the run's tool calls and signals, for `ptc test` and `POST /v1/test`.
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
        let call = Waiting::ToolCall {
            seq: 3,
            tool: "ask_user".into(),
            args: Payload::of(
                json!({ "question": "ok?" }),
                crate::common::PAYLOAD_VALUE_LIMIT,
            ),
            task_id: "t-1".into(),
            state: TaskState::Approval,
            parked: None,
            attempt: 1,
            callsite: None,
        };
        let written = serde_json::to_value(&call).unwrap();
        assert_eq!(written["taskId"], "t-1");
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
