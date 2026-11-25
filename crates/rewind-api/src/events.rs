//! A run's history over HTTP: one event in full (`GET /v1/runs/{id}/events/{idx}`) and the
//! event stream (`GET /v1/runs/{id}/events`, server-sent events); and the stream of what changes
//! in the organization, its runs and its control plane (`GET /v1/changes`, server-sent events).
//!
//! These are projections of committed events (`archi/05-testing-and-observability.md`). The
//! stored form of the history is `rewind-format`'s, exported by `GET /v1/runs/{id}/export`; the
//! events here are its HTTP view: camelCase fields, RFC 3339 times, payloads with their size
//! and preview, and no seed.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::common::{
    AbortReason, ActionFailure, Callsite, ErrorInfo, LogLevel, Payload, RunMode, RunStatus,
    Timestamp, timestamp,
};
use crate::namespaces::NamespaceName;
use crate::runs::RunSummary;

/// One event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEvent {
    /// Position in the history: from 0, contiguous.
    pub idx: u64,
    /// When the event was committed; monotonic per run. Once a trigger is delivered, it is the
    /// code's logical time.
    pub recorded_at: Timestamp,
    pub event: Event,
}

impl HistoryEvent {
    /// The HTTP view of an event. `payload` turns each payload into its view: the
    /// server knows where values that left the event live, and how much of them to include.
    pub fn from_core(
        entry: &rewind_core::HistoryEvent,
        payload: impl FnMut(&rewind_core::Payload) -> Payload,
    ) -> Self {
        Self {
            idx: entry.index,
            recorded_at: timestamp(entry.recorded_at),
            event: Event::from_core(&entry.event, payload),
        }
    }
}

/// A history event (`archi/02-history.md`): lifecycle, commands, completions and annotations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The run's first event. The seed is left out: it is only in the run export.
    #[serde(rename_all = "camelCase")]
    RunStarted {
        code: String,
        action_set: String,
        input: Payload,
        epoch: u32,
        expires_at: Timestamp,
        mode: RunMode,
    },
    /// The engine ended the run.
    RunAborted {
        reason: AbortReason,
    },
    #[serde(rename_all = "camelCase")]
    ScheduleCall {
        seq: u32,
        action: String,
        args: Payload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<Callsite>,
    },
    #[serde(rename_all = "camelCase")]
    StartTimer {
        seq: u32,
        duration_ms: u64,
        fire_at: Timestamp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<Callsite>,
    },
    CancelTimer {
        seq: u32,
    },
    CompleteRun {
        output: Payload,
    },
    FailRun {
        error: ErrorInfo,
    },
    CallCompleted {
        seq: u32,
        result: Payload,
    },
    CallFailed {
        seq: u32,
        error: ActionFailure,
    },
    TimerFired {
        seq: u32,
    },
    SignalReceived {
        name: String,
        payload: Payload,
    },
    CancelRequested {
        reason: CancelReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Log {
        level: LogLevel,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<Callsite>,
    },
    /// The code started waiting for a signal whose mailbox was empty.
    SignalAwaited {
        name: String,
        /// The `seq` of the timer of its timeout.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<Callsite>,
    },
}

/// Why a run is being cancelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// `POST /v1/runs/{id}/cancel`.
    Requested,
    /// The run's deadline passed.
    Ttl,
}

impl Event {
    /// The event's name in the history, such as `schedule_call`.
    pub fn name(&self) -> &'static str {
        match self {
            Event::RunStarted { .. } => "run_started",
            Event::RunAborted { .. } => "run_aborted",
            Event::ScheduleCall { .. } => "schedule_call",
            Event::StartTimer { .. } => "start_timer",
            Event::CancelTimer { .. } => "cancel_timer",
            Event::CompleteRun { .. } => "complete_run",
            Event::FailRun { .. } => "fail_run",
            Event::CallCompleted { .. } => "call_completed",
            Event::CallFailed { .. } => "call_failed",
            Event::TimerFired { .. } => "timer_fired",
            Event::SignalReceived { .. } => "signal_received",
            Event::CancelRequested { .. } => "cancel_requested",
            Event::Log { .. } => "log",
            Event::SignalAwaited { .. } => "signal_awaited",
        }
    }

    /// The HTTP view of a history event; see [`HistoryEvent::from_core`].
    pub fn from_core(
        event: &rewind_core::Event,
        mut payload: impl FnMut(&rewind_core::Payload) -> Payload,
    ) -> Self {
        use rewind_core::{Annotation, Command, Completion, Lifecycle};

        let callsite = |callsite: &Option<rewind_core::Callsite>| callsite.map(Callsite::from);
        match event {
            rewind_core::Event::Lifecycle(Lifecycle::RunStarted(started)) => Event::RunStarted {
                code: started.code.to_hex(),
                action_set: started.action_set.to_hex(),
                input: payload(&started.input),
                epoch: started.epoch.0,
                expires_at: timestamp(started.expires_at),
                mode: started.mode.into(),
            },
            rewind_core::Event::Lifecycle(Lifecycle::RunAborted(reason)) => Event::RunAborted {
                reason: reason.into(),
            },
            rewind_core::Event::Command(command) => match command {
                Command::ScheduleCall {
                    seq,
                    action,
                    args,
                    callsite: at,
                } => Event::ScheduleCall {
                    seq: seq.0,
                    action: action.clone(),
                    args: payload(args),
                    callsite: callsite(at),
                },
                Command::StartTimer {
                    seq,
                    duration_ms,
                    fire_at,
                    callsite: at,
                } => Event::StartTimer {
                    seq: seq.0,
                    duration_ms: *duration_ms,
                    fire_at: timestamp(*fire_at),
                    callsite: callsite(at),
                },
                Command::CancelTimer { seq } => Event::CancelTimer { seq: seq.0 },
                Command::CompleteRun { output } => Event::CompleteRun {
                    output: payload(output),
                },
                Command::FailRun { error } => Event::FailRun {
                    error: error.into(),
                },
            },
            rewind_core::Event::Completion(completion) => match completion {
                Completion::CallCompleted { seq, result } => Event::CallCompleted {
                    seq: seq.0,
                    result: payload(result),
                },
                Completion::CallFailed { seq, error } => Event::CallFailed {
                    seq: seq.0,
                    error: error.into(),
                },
                Completion::TimerFired { seq } => Event::TimerFired { seq: seq.0 },
                Completion::SignalReceived {
                    name,
                    payload: value,
                } => Event::SignalReceived {
                    name: name.clone(),
                    payload: payload(value),
                },
                Completion::CancelRequested { reason } => match reason {
                    rewind_core::CancelReason::Requested(message) => Event::CancelRequested {
                        reason: CancelReason::Requested,
                        message: message.clone(),
                    },
                    rewind_core::CancelReason::Ttl => Event::CancelRequested {
                        reason: CancelReason::Ttl,
                        message: None,
                    },
                },
            },
            rewind_core::Event::Annotation(annotation) => match annotation {
                Annotation::Log {
                    level,
                    message,
                    callsite: at,
                } => Event::Log {
                    level: (*level).into(),
                    message: message.clone(),
                    callsite: callsite(at),
                },
                Annotation::SignalAwaited {
                    name,
                    timeout,
                    callsite: at,
                } => Event::SignalAwaited {
                    name: name.clone(),
                    timeout: timeout.map(|seq| seq.0),
                    callsite: callsite(at),
                },
            },
        }
    }
}

// -------------------------------------------------------------------------------------------
// The event stream

/// `GET /v1/runs/{id}/events`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EventsQuery {
    /// `1`: keep the stream open and send events as they are committed, until the run ends.
    /// Without it, the stream ends once the events committed so far are sent.
    #[serde(
        default,
        deserialize_with = "crate::runs::flag",
        skip_serializing_if = "Option::is_none"
    )]
    pub follow: Option<bool>,
    /// The first event to send. `0` by default; a reconnecting client sends the `id` of the last
    /// event it received plus one (or the `Last-Event-ID` header).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u64>,
}

/// One message of the stream. On the wire, each is a server-sent event whose `event` field is
/// the `type` below, whose `data` is this JSON object, and, for an event, whose `id` is the
/// event's `idx`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamMessage {
    /// A committed event of the history. Payload values above 256 KiB are left out.
    Event(Box<HistoryEvent>),
    /// The last event of a stream: the run has ended, or, without `follow`, every event
    /// committed so far was sent.
    End { status: RunStatus },
}

impl StreamMessage {
    /// The SSE `event` field.
    pub fn sse_event(&self) -> &'static str {
        match self {
            StreamMessage::Event(_) => "event",
            StreamMessage::End { .. } => "end",
        }
    }

    /// The SSE `id` field: the event's index.
    pub fn sse_id(&self) -> Option<String> {
        match self {
            StreamMessage::Event(entry) => Some(entry.idx.to_string()),
            StreamMessage::End { .. } => None,
        }
    }
}

// -------------------------------------------------------------------------------------------
// The change stream

/// One change of the change stream (`GET /v1/changes`). On the wire, each is a server-sent event
/// whose `event` field names it and whose `data` is this JSON value: for `run`, the run's
/// summary, the very object `GET /v1/runs` lists, pushed by the run's owner once its batch is
/// durable; for `workflow`, `secret`, `namespace` and `connection`, what changed in the
/// control plane, by its name or id (the client reads again the views that show it); for
/// `resync`, `{}`. Each reaches the caller only if it may see what changed. Events have no `id`:
/// the stream is not resumed, the client reads again what it shows. While it sends nothing else,
/// the server writes a comment line (`:`) at least every 15 seconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum Change {
    /// `run`: a run changed. Its summary as it is now, as `GET /v1/runs` lists it.
    Run(Box<RunSummary>),
    /// `workflow`: a workflow was created, named, described, given a version or tests,
    /// promoted, forked or pulled.
    Workflow(WorkflowChanged),
    /// `secret`: a secret was put or deleted.
    Secret(SecretChanged),
    /// `namespace`: a namespace was created, renamed, given other limits or tokens, or erased;
    /// or the organization's default limits or ceiling changed.
    Namespace(NamespaceChanged),
    /// `connection`: a connection was made, changed, used or revoked.
    Connection(ConnectionChanged),
    /// `resync`: more changed at once than the server reads in one pass. Read again what is
    /// shown.
    Resync(Resync),
}

/// The data of a `workflow` event: the workflow that changed, which `GET /v1/workflows/{id}`
/// reads again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowChanged {
    /// Its id, `wf_` and 12 characters.
    pub workflow: String,
}

/// The data of a `secret` event: a secret put (created or given a new value) or deleted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretChanged {
    pub name: String,
    /// The namespace whose secret it is; absent for the organization's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<NamespaceName>,
}

/// The data of a `namespace` event: a namespace created, renamed, given other exceptions or
/// tokens, or being erased or erased; without `name`, the organization's default limits or its
/// ceiling, which every namespace's limits follow.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceChanged {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<NamespaceName>,
}

/// The data of a `connection` event: one of the caller's own connections, or a connection that
/// opens the organization, was made, changed, revoked or taken out of the organization, or its
/// `lastUsedAt` moved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionChanged {
    /// The connection's id.
    pub id: String,
}

/// The data of a `resync` event: an empty object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Resync {}

impl Change {
    /// The SSE `event` field: `run`, `workflow`, `secret`, `namespace`, `connection` or
    /// `resync`.
    pub fn sse_event(&self) -> &'static str {
        match self {
            Change::Run(_) => "run",
            Change::Workflow(_) => "workflow",
            Change::Secret(_) => "secret",
            Change::Namespace(_) => "namespace",
            Change::Connection(_) => "connection",
            Change::Resync(_) => "resync",
        }
    }

    /// An event from its SSE `event` field and its `data`; `None` for an event this version
    /// does not know, which a client skips.
    pub fn from_sse(event: &str, data: &str) -> Result<Option<Self>, serde_json::Error> {
        Ok(match event {
            "run" => Some(Change::Run(Box::new(serde_json::from_str(data)?))),
            "workflow" => Some(Change::Workflow(serde_json::from_str(data)?)),
            "secret" => Some(Change::Secret(serde_json::from_str(data)?)),
            "namespace" => Some(Change::Namespace(serde_json::from_str(data)?)),
            "connection" => Some(Change::Connection(serde_json::from_str(data)?)),
            "resync" => Some(Change::Resync(Resync {})),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::{Command, Seq};
    use serde_json::json;

    use super::*;
    use crate::common::PAYLOAD_VALUE_LIMIT;

    fn view(payload: &rewind_core::Payload) -> Payload {
        match payload {
            rewind_core::Payload::Value(value) => Payload::of(value.clone(), PAYLOAD_VALUE_LIMIT),
            rewind_core::Payload::Digest(digest) => Payload::reference(digest.to_hex(), 0, ""),
        }
    }

    #[test]
    fn an_event_has_an_http_view() {
        let entry = rewind_core::HistoryEvent {
            index: 1,
            recorded_at: rewind_core::Timestamp::from_millis(1_000),
            event: rewind_core::Event::Command(Command::StartTimer {
                seq: Seq(2),
                duration_ms: 3_000,
                fire_at: rewind_core::Timestamp::from_millis(4_000),
                callsite: Some(rewind_core::Callsite {
                    line: 10,
                    column: 3,
                }),
            }),
        };
        let written = serde_json::to_value(HistoryEvent::from_core(&entry, view)).unwrap();
        assert_eq!(
            written,
            json!({
                "idx": 1,
                "recordedAt": "1970-01-01T00:00:01Z",
                "event": {
                    "type": "start_timer",
                    "seq": 2,
                    "durationMs": 3000,
                    "fireAt": "1970-01-01T00:00:04Z",
                    "callsite": { "line": 10, "column": 3 }
                }
            })
        );
        let read: HistoryEvent = serde_json::from_value(written).unwrap();
        assert_eq!(read.event.name(), entry.event.name());
    }

    #[test]
    fn a_stream_event_is_flat_and_tagged() {
        let entry = rewind_core::HistoryEvent {
            index: 4,
            recorded_at: rewind_core::Timestamp::from_millis(0),
            event: rewind_core::Event::Command(Command::CompleteRun {
                output: json!({ "closed": 3 }).into(),
            }),
        };
        let event = StreamMessage::Event(Box::new(HistoryEvent::from_core(&entry, view)));
        assert_eq!(event.sse_event(), "event");
        assert_eq!(event.sse_id().as_deref(), Some("4"));
        let written = serde_json::to_value(&event).unwrap();
        assert_eq!(written["type"], "event");
        assert_eq!(written["idx"], 4);
        assert_eq!(written["event"]["type"], "complete_run");
        assert_eq!(written["event"]["output"]["value"], json!({ "closed": 3 }));
        assert_eq!(
            serde_json::from_value::<StreamMessage>(written).unwrap(),
            event
        );

        let end = StreamMessage::End {
            status: RunStatus::Completed,
        };
        assert_eq!(
            serde_json::to_value(&end).unwrap(),
            json!({ "type": "end", "status": "completed" })
        );
    }

    #[test]
    fn follow_reads_as_a_query_flag() {
        for (text, follow) in [("1", true), ("true", true), ("0", false), ("false", false)] {
            let query: EventsQuery =
                serde_json::from_value(json!({ "follow": text, "from": 3 })).unwrap();
            assert_eq!(query.follow, Some(follow));
            assert_eq!(query.from, Some(3));
        }
    }

    #[test]
    fn a_change_is_the_summary_itself_or_an_empty_object() {
        let summary: RunSummary = serde_json::from_value(json!({
            "id": "0193a5c4-0000-7000-8000-000000000001",
            "handle": "alice/script/06bqx3k9f2mt",
            "namespace": "alice",
            "status": "waiting",
            "quarantined": false,
            "codeDigest": "ab",
            "actionSet": "cd",
            "labels": {},
            "startedAt": "2026-11-09T10:00:00Z",
            "lastActivityAt": "2026-11-09T10:00:01Z",
            "calls": 0,
            "errors": 0,
            "historyLen": 1
        }))
        .unwrap();
        let run = Change::Run(Box::new(summary.clone()));
        assert_eq!(run.sse_event(), "run");
        let data = serde_json::to_string(&run).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&data).unwrap(),
            serde_json::to_value(&summary).unwrap(),
            "the data is the summary, untagged"
        );
        assert_eq!(Change::from_sse("run", &data).unwrap(), Some(run));

        let resync = Change::Resync(Resync {});
        assert_eq!(resync.sse_event(), "resync");
        assert_eq!(serde_json::to_string(&resync).unwrap(), "{}");
        assert_eq!(Change::from_sse("resync", "{}").unwrap(), Some(resync));
        assert_eq!(Change::from_sse("later", "{}").unwrap(), None);
        assert!(Change::from_sse("run", "{}").is_err());
    }

    #[test]
    fn a_control_plane_change_names_what_changed() {
        let cases = [
            (
                "workflow",
                json!({ "workflow": "wf_7k2m9q4x8d1c" }),
                Change::Workflow(WorkflowChanged {
                    workflow: "wf_7k2m9q4x8d1c".into(),
                }),
            ),
            (
                "secret",
                json!({ "name": "github", "namespace": "alice" }),
                Change::Secret(SecretChanged {
                    name: "github".into(),
                    namespace: Some("alice".parse().unwrap()),
                }),
            ),
            (
                "secret",
                json!({ "name": "github" }),
                Change::Secret(SecretChanged {
                    name: "github".into(),
                    namespace: None,
                }),
            ),
            (
                "namespace",
                json!({ "name": "alice" }),
                Change::Namespace(NamespaceChanged {
                    name: Some("alice".parse().unwrap()),
                }),
            ),
            (
                "namespace",
                json!({}),
                Change::Namespace(NamespaceChanged { name: None }),
            ),
            (
                "connection",
                json!({ "id": "conn_1" }),
                Change::Connection(ConnectionChanged {
                    id: "conn_1".into(),
                }),
            ),
        ];
        for (name, data, event) in cases {
            assert_eq!(event.sse_event(), name);
            assert_eq!(serde_json::to_value(&event).unwrap(), data, "{name}");
            assert_eq!(
                Change::from_sse(name, &data.to_string()).unwrap(),
                Some(event),
                "{name}"
            );
        }
        assert!(Change::from_sse("workflow", "{}").is_err());
        assert_eq!(
            Change::from_sse("script", "{}").unwrap(),
            None,
            "an event no more"
        );
        assert!(Change::from_sse("connection", "{}").is_err());
    }
}
