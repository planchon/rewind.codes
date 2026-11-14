//! Host-tool tasks: the harness's SDK worker long-polls for them, and the harness answers
//! the tasks that wait for it (`archi/04-tools.md`, "Tool task lifecycle").
//!
//! Two identifiers: a claimed task has a **token**, which only its worker knows
//! (`/v1/tasks/{token}/heartbeat`, `/defer`, `/complete`, `/fail`); every task has an **id**,
//! listed in the run's state, for the harness's decisions (`/v1/tasks/{id}/approve`,
//! `/reject`).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::common::{Duration, Labels};
use crate::namespaces::NamespaceName;

/// `GET /v1/tasks`: claims host-tool tasks that are ready, waiting up to `wait` for one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TaskQuery {
    /// Only tasks of runs of this toolset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolset: Option<String>,
    /// Only these tools. Repeat for several. All the toolset's host tools when absent.
    #[serde(default, rename = "tool", skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// At most this many tasks (1 by default, 100 at most).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// How long to wait for a task before answering with none. 30 seconds by default, 60 at
    /// most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>)]
    pub wait: Option<Duration>,
}

/// The tasks claimed; empty when `wait` passed without one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskBatch {
    pub tasks: Vec<Task>,
}

/// A claimed host-tool call. The worker keeps the claim alive with heartbeats, and answers
/// with `/complete` or `/fail`, or `/defer` to answer later.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// The claim's secret token: `/v1/tasks/{token}/…`. It changes with each claim.
    pub token: String,
    /// The task's id, as listed in the run's state.
    pub id: String,
    pub run_id: String,
    /// The namespace of the run: the end user the handler acts for.
    pub namespace: NamespaceName,
    pub seq: u32,
    pub tool: String,
    /// The arguments, validated against the tool's input schema.
    pub args: Value,
    /// `{run_id}/{seq}` (a fork's call made again keeps its parent's), the same on every
    /// attempt: the handler uses it to recognize a call it already made.
    pub idempotency_key: String,
    /// From 1. Above 1, an earlier attempt was interrupted.
    pub attempt: u32,
    /// The claim expires if no heartbeat comes within this many milliseconds.
    pub heartbeat_timeout_ms: u64,
    /// The tool's execution timeout, in milliseconds: past it, the call is not waited for any
    /// more. Absent when the tool has none, the default of host tools: the run's TTL bounds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// The run's labels.
    pub labels: Labels,
    /// W3C trace context, with the tool call's span as parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

/// `POST /v1/tasks/{token}/heartbeat`: the worker is still working on the task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatAck {
    /// The claim now expires if no heartbeat comes within this many milliseconds.
    pub heartbeat_timeout_ms: u64,
}

/// `POST /v1/tasks/{token}/defer`: the handler will answer later, possibly days later, with
/// `/complete` or `/fail` and the same token. No heartbeat is needed meanwhile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeferTask {
    /// Fail the call with `timeout` if no answer comes within this time. Bounded by the run's
    /// TTL, which is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}

/// `POST /v1/tasks/{token}/complete`: the tool's result, delivered to the script.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompleteTask {
    pub result: Value,
}

/// `POST /v1/tasks/{token}/fail`: the tool failed. The script gets a `ToolError`, unless the
/// failure is retryable and the tool's policy is `retry`: the call is then made again, with
/// backoff.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FailTask {
    /// The message of the `ToolError`, written for the model.
    pub message: String,
    /// `ToolError.data`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// A transient failure: call again later.
    #[serde(default)]
    pub retryable: bool,
}

/// `POST /v1/tasks/{id}/reject`: the call fails with a `ToolError` carrying this message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RejectTask {
    /// Why, written for the model. "rejected by the harness" when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The state of a tool task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Waits for the harness's approval (`approval` policy).
    Approval,
    /// Waits to be sent: its next attempt, or a concurrency slot.
    Ready,
    /// An attempt is in flight.
    Claimed,
    /// A host tool's handler will answer later.
    Deferred,
    /// Waits for the harness: the tool's schema changed, or its credentials were refused.
    Parked,
    /// Its outcome is in the run's inbox.
    Done,
}

/// Why a task is parked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParkedReason {
    /// The server's schema of the tool no longer matches the pinned one (webhook
    /// `tool.schema_changed`): approve to continue against the new schema, or reject.
    SchemaChanged,
    /// The server answered 401 (webhook `credentials.expired`): renew the credentials.
    CredentialsExpired,
}
