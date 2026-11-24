//! Harness calls: the harness's SDK worker holds a call stream open and is handed the calls of its
//! harness actions as they become ready, and the harness answers the calls that wait for it
//! (`archi/04-tools.md`, "Call lifecycle").
//!
//! Two identifiers: a call handed to a worker has a **token**, which only that worker knows
//! (`/v1/calls/{token}/defer`, `/complete`, `/fail`); every call has an **id**, listed in the
//! run's state, for the harness's decisions (`/v1/calls/{id}/approve`, `/reject`).
//!
//! The call stream (`GET /v1/calls/stream`: [`CallStreamQuery`], [`CallStreamMessage`]) hands the
//! calls, and the open stream is the liveness of every call handed on it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::common::{Duration, Labels};
use crate::namespaces::NamespaceName;

/// `GET /v1/calls/stream`: the harness calls handed to this worker as they become ready, as
/// server-sent events ([`CallStreamMessage`]). A worker is handed only the calls of the runs its
/// key or token sees: an end user's namespace, a grant's labels.
///
/// The open stream is the liveness of every call handed on it: nothing is written or asked
/// while a call is in flight. When the stream closes, its calls wait for the worker to open a
/// stream again with the same `worker`, for the action's grace (60 s by default); after it, each is
/// handed to another worker with its delivery + 1, or fails with `outcomeUnknown` under
/// `idempotent: false` (`archi/04-tools.md`, "Call lifecycle").
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct CallStreamQuery {
    /// The action set whose harness actions this worker executes: only the calls of runs of this
    /// action set. Without it, the calls of the actions named in `actions`, whatever the action set
    /// of their run (an action set derived from another, as saving a workflow does, keeps its
    /// harness actions' names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_set: Option<String>,
    /// Only the calls of runs of this namespace. An end user's token is limited to its own: it
    /// may name it, and naming another is `forbidden`. The header `Rewind-Namespace` narrows the
    /// same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>)]
    pub namespace: Option<NamespaceName>,
    /// Only these actions. Repeat for several. All the harness actions of `actionSet` when absent,
    /// every harness action without `actionSet` either.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<String>,
    /// At most this many calls in flight on this stream: handed, and neither completed, failed,
    /// deferred (by whichever process answers) nor gone. 1 by default, 100 at most. With 0, none
    /// is handed: a worker that stops opens its stream again with 0 slots and its `worker`, which
    /// keeps alive the calls handed to it before while its handlers finish them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<u32>,
    /// The worker's own id, drawn once when it starts (a UUID) and sent again on every
    /// reconnection: a stream opened again with it within the grace keeps the calls handed on the
    /// one that closed, and they are not handed again. 1 to 128 visible ASCII characters. Without
    /// it, the calls of a closed stream go to another worker once the grace has passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

/// A harness call handed to a worker on its call stream. The worker answers it by its token,
/// with `/complete` or `/fail`, or `/defer` to answer later; nothing else is asked of it while it
/// works.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HandedCall {
    /// The hand-out's secret token: `/v1/calls/{token}/…`. A new one at each hand-out. It names
    /// the run, so an answer reaches the run's owner from whichever process receives it.
    pub token: String,
    /// The call's id, as listed in the run's state.
    pub id: String,
    pub run_id: String,
    /// The namespace of the run: the end user the handler acts for.
    pub namespace: NamespaceName,
    pub seq: u32,
    pub action: String,
    /// The arguments, validated against the action's input schema.
    pub args: Value,
    /// `{run_id}/{seq}` (a fork's call made again keeps its source's), the same on every
    /// delivery: the handler uses it to recognize a call it already made.
    pub idempotency_key: String,
    /// From 1. Above 1, an earlier delivery was interrupted: the stream it was handed on closed
    /// and did not come back within its grace.
    pub delivery: u32,
    /// The action's execution timeout, in milliseconds: past it, the call is not waited for any
    /// more. Absent when the action has none, the default of harness actions: the run's TTL bounds
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// The run's labels.
    pub labels: Labels,
    /// W3C trace context, with the call's span as parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

/// The data of a `gone` message: a call handed on this stream no longer waits for its answer. Its
/// run ended, or its call timed out; an answer sent now is dropped (`call-gone`), and its slot is
/// free.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CallGone {
    /// The token the call was handed with.
    pub token: String,
}

/// One message of the call stream (`GET /v1/calls/stream`). On the wire, each is a server-sent
/// event whose `event` field is `call` or `gone` and whose `data` is this JSON object; events
/// have no `id`. While it sends nothing else, the server writes a comment line (`:`) at least
/// every 15 seconds, so that proxies keep the stream open: a client skips comments, and may take
/// a stream silent for longer as broken. The stream closes once the caller stops authenticating
/// or no longer holds `calls:work`, as the change stream does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum CallStreamMessage {
    /// `call`: a call handed to this worker, with its token.
    Call(Box<HandedCall>),
    /// `gone`: a call handed on this stream no longer waits for its answer.
    Gone(CallGone),
}

impl CallStreamMessage {
    /// The SSE `event` field: `call` or `gone`.
    pub fn sse_event(&self) -> &'static str {
        match self {
            CallStreamMessage::Call(_) => "call",
            CallStreamMessage::Gone(_) => "gone",
        }
    }

    /// A message from its SSE `event` field and its `data`; `None` for one this version does not
    /// know, which a client skips.
    pub fn from_sse(event: &str, data: &str) -> Result<Option<Self>, serde_json::Error> {
        Ok(match event {
            "call" => Some(CallStreamMessage::Call(Box::new(serde_json::from_str(
                data,
            )?))),
            "gone" => Some(CallStreamMessage::Gone(serde_json::from_str(data)?)),
            _ => None,
        })
    }
}

/// `POST /v1/calls/{token}/defer`: the handler will answer later, possibly days later, with
/// `/complete` or `/fail` and the same token, whether the call stream it was handed on stays open
/// or not: its slot on the stream is free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeferCall {
    /// Fail the call with `timeout` if no answer comes within this time. Bounded by the run's
    /// TTL, which is the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
}

/// `POST /v1/calls/{token}/complete`: the action's result, delivered to the code.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompleteCall {
    pub result: Value,
}

/// `POST /v1/calls/{token}/fail`: the action failed. The code gets an `ActionError`, unless the
/// failure is retryable and the action is idempotent: the call is then delivered again, with
/// backoff.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FailCall {
    /// The message of the `ActionError`, written for the model.
    pub message: String,
    /// `ActionError.data`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    /// A transient failure: call again later.
    #[serde(default)]
    pub retryable: bool,
}

/// `POST /v1/calls/{id}/reject`: the call fails with an `ActionError` carrying this message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RejectCall {
    /// Why, written for the model. "rejected by the harness" when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The state of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    /// Waits for the harness's approval (`approval` policy).
    Approval,
    /// Waits to be sent: its next delivery, or a concurrency slot.
    Ready,
    /// A delivery is in flight.
    Claimed,
    /// A harness action's handler will answer later.
    Deferred,
    /// Waits for the harness: the action's schema changed, its server refused the call, or the
    /// secret the run attached no longer exists.
    Parked,
    /// Its outcome is in the run's inbox.
    Done,
}

/// Why a call is parked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParkedReason {
    /// The server's schema of the action no longer matches the pinned one (webhook
    /// `action.schema_changed`): approve to continue against the new schema, or reject.
    SchemaChanged,
    /// The server answered 401 to the secret or to the signature (webhook
    /// `credentials.expired`, reason `refused`): put the secret again, or approve once the
    /// server accepts the key.
    CredentialsRefused,
    /// The secret the run attached no longer exists (webhook `credentials.expired`, reason
    /// `missing`): put it again.
    SecretMissing,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_call_message_is_the_handed_call_itself() {
        let data = json!({
            "token": "tk_1",
            "id": "task_1",
            "runId": "0193a5c4-0000-8000-8000-000000000001",
            "namespace": "alice",
            "seq": 3,
            "action": "ask_user",
            "args": { "question": "ok?" },
            "idempotencyKey": "0193a5c4-0000-8000-8000-000000000001/3",
            "delivery": 1,
            "labels": { "agent": "triage" }
        });
        let event = CallStreamMessage::from_sse("call", &data.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(event.sse_event(), "call");
        let CallStreamMessage::Call(task) = &event else {
            panic!("expected a call: {event:?}");
        };
        assert_eq!(task.token, "tk_1");
        assert_eq!(task.timeout_ms, None);
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            data,
            "the data is the call, untagged"
        );
    }

    #[test]
    fn a_gone_event_names_the_token_and_unknown_events_are_skipped() {
        let gone = CallStreamMessage::from_sse("gone", r#"{"token":"tk_1"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(
            gone,
            CallStreamMessage::Gone(CallGone {
                token: "tk_1".into()
            })
        );
        assert_eq!(gone.sse_event(), "gone");
        assert_eq!(CallStreamMessage::from_sse("later", "{}").unwrap(), None);
        assert!(CallStreamMessage::from_sse("call", "{}").is_err());
        assert!(CallStreamMessage::from_sse("gone", "{}").is_err());
    }

    #[test]
    fn the_stream_query_names_its_actions_and_slots() {
        let query: CallStreamQuery = serde_json::from_value(json!({
            "actionSet": "ab",
            "actions": ["ask_user"],
            "slots": 4,
            "worker": "w-1"
        }))
        .unwrap();
        assert_eq!(query.action_set.as_deref(), Some("ab"));
        assert_eq!(query.actions, ["ask_user"]);
        assert_eq!(query.slots, Some(4));
        assert_eq!(
            serde_json::from_value::<CallStreamQuery>(json!({})).unwrap(),
            CallStreamQuery::default()
        );
    }
}
