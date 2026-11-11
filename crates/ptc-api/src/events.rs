//! A run's journal over HTTP: one entry in full (`GET /v1/runs/{id}/entries/{idx}`) and the
//! event stream (`GET /v1/runs/{id}/events`, server-sent events).
//!
//! These are projections of committed entries (`archi/05-testing-and-observability.md`). The
//! stored form of the journal is `ptc-format`'s, exported by `GET /v1/runs/{id}/journal`; the
//! entries here are its HTTP view: camelCase fields, RFC 3339 times, payloads with their size
//! and preview, and no seed.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::common::{
    AbortReason, Callsite, LogLevel, Payload, RunMode, RunStatus, ScriptError, Timestamp,
    ToolFailure, timestamp,
};

/// One journal entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Position in the history: from 0, contiguous.
    pub idx: u64,
    /// When the entry was committed; monotonic per run. Once a trigger is delivered, it is the
    /// script's logical time.
    pub recorded_at: Timestamp,
    pub event: Event,
}

impl Entry {
    /// The HTTP view of a journal entry. `payload` turns each payload into its view: the
    /// server knows where values that left the event live, and how much of them to include.
    pub fn from_core(
        entry: &ptc_core::HistoryEntry,
        payload: impl FnMut(&ptc_core::Payload) -> Payload,
    ) -> Self {
        Self {
            idx: entry.index,
            recorded_at: timestamp(entry.recorded_at),
            event: Event::from_core(&entry.event, payload),
        }
    }
}

/// A journal event (`archi/02-journal.md`): lifecycle, commands, completions and annotations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// The run's first entry. The seed is left out: it is only in the exported journal.
    #[serde(rename_all = "camelCase")]
    RunStarted {
        script: String,
        toolset: String,
        input: Payload,
        epoch: u32,
        expires_at: Timestamp,
        mode: RunMode,
    },
    /// The host ended the run.
    RunAborted {
        reason: AbortReason,
    },
    #[serde(rename_all = "camelCase")]
    ScheduleToolCall {
        seq: u32,
        tool: String,
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
        error: ScriptError,
    },
    ToolCallCompleted {
        seq: u32,
        result: Payload,
    },
    ToolCallFailed {
        seq: u32,
        error: ToolFailure,
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
    /// The script started waiting for a signal whose mailbox was empty.
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
    /// The event's name in the journal, such as `schedule_tool_call`.
    pub fn name(&self) -> &'static str {
        match self {
            Event::RunStarted { .. } => "run_started",
            Event::RunAborted { .. } => "run_aborted",
            Event::ScheduleToolCall { .. } => "schedule_tool_call",
            Event::StartTimer { .. } => "start_timer",
            Event::CancelTimer { .. } => "cancel_timer",
            Event::CompleteRun { .. } => "complete_run",
            Event::FailRun { .. } => "fail_run",
            Event::ToolCallCompleted { .. } => "tool_call_completed",
            Event::ToolCallFailed { .. } => "tool_call_failed",
            Event::TimerFired { .. } => "timer_fired",
            Event::SignalReceived { .. } => "signal_received",
            Event::CancelRequested { .. } => "cancel_requested",
            Event::Log { .. } => "log",
            Event::SignalAwaited { .. } => "signal_awaited",
        }
    }

    /// The HTTP view of a journal event; see [`Entry::from_core`].
    pub fn from_core(
        event: &ptc_core::Event,
        mut payload: impl FnMut(&ptc_core::Payload) -> Payload,
    ) -> Self {
        use ptc_core::{Annotation, Command, Completion, Lifecycle};

        let callsite = |callsite: &Option<ptc_core::Callsite>| callsite.map(Callsite::from);
        match event {
            ptc_core::Event::Lifecycle(Lifecycle::RunStarted(started)) => Event::RunStarted {
                script: started.script.to_hex(),
                toolset: started.toolset.to_hex(),
                input: payload(&started.input),
                epoch: started.epoch.0,
                expires_at: timestamp(started.expires_at),
                mode: started.mode.into(),
            },
            ptc_core::Event::Lifecycle(Lifecycle::RunAborted(reason)) => Event::RunAborted {
                reason: reason.into(),
            },
            ptc_core::Event::Command(command) => match command {
                Command::ScheduleToolCall {
                    seq,
                    tool,
                    args,
                    callsite: at,
                } => Event::ScheduleToolCall {
                    seq: seq.0,
                    tool: tool.clone(),
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
            ptc_core::Event::Completion(completion) => match completion {
                Completion::ToolCallCompleted { seq, result } => Event::ToolCallCompleted {
                    seq: seq.0,
                    result: payload(result),
                },
                Completion::ToolCallFailed { seq, error } => Event::ToolCallFailed {
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
                    ptc_core::CancelReason::Requested(message) => Event::CancelRequested {
                        reason: CancelReason::Requested,
                        message: message.clone(),
                    },
                    ptc_core::CancelReason::Ttl => Event::CancelRequested {
                        reason: CancelReason::Ttl,
                        message: None,
                    },
                },
            },
            ptc_core::Event::Annotation(annotation) => match annotation {
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
    /// `1`: keep the stream open and send entries as they are committed, until the run ends.
    /// Without it, the stream ends once the entries committed so far are sent.
    #[serde(
        default,
        deserialize_with = "crate::runs::flag",
        skip_serializing_if = "Option::is_none"
    )]
    pub follow: Option<bool>,
    /// The first entry to send. `0` by default; a reconnecting client sends the `id` of the last
    /// event it received plus one (or the `Last-Event-ID` header).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u64>,
}

/// One event of the stream. On the wire, each is a server-sent event whose `event` field is
/// the `type` below, whose `data` is this JSON object, and, for an entry, whose `id` is the
/// entry's `idx`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A committed journal entry. Payload values above 256 KiB are left out.
    Entry(Box<Entry>),
    /// The last event of a stream: the run has ended, or, without `follow`, every entry
    /// committed so far was sent.
    End { status: RunStatus },
}

impl StreamEvent {
    /// The SSE `event` field.
    pub fn sse_event(&self) -> &'static str {
        match self {
            StreamEvent::Entry(_) => "entry",
            StreamEvent::End { .. } => "end",
        }
    }

    /// The SSE `id` field: the entry's index.
    pub fn sse_id(&self) -> Option<String> {
        match self {
            StreamEvent::Entry(entry) => Some(entry.idx.to_string()),
            StreamEvent::End { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ptc_core::{Command, HistoryEntry, Seq};
    use serde_json::json;

    use super::*;
    use crate::common::PAYLOAD_VALUE_LIMIT;

    fn view(payload: &ptc_core::Payload) -> Payload {
        match payload {
            ptc_core::Payload::Value(value) => Payload::of(value.clone(), PAYLOAD_VALUE_LIMIT),
            ptc_core::Payload::Digest(digest) => Payload::reference(digest.to_hex(), 0, ""),
        }
    }

    #[test]
    fn a_journal_entry_has_an_http_view() {
        let entry = HistoryEntry {
            index: 1,
            recorded_at: ptc_core::Timestamp::from_millis(1_000),
            event: ptc_core::Event::Command(Command::StartTimer {
                seq: Seq(2),
                duration_ms: 3_000,
                fire_at: ptc_core::Timestamp::from_millis(4_000),
                callsite: Some(ptc_core::Callsite {
                    line: 10,
                    column: 3,
                }),
            }),
        };
        let written = serde_json::to_value(Entry::from_core(&entry, view)).unwrap();
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
        let read: Entry = serde_json::from_value(written).unwrap();
        assert_eq!(read.event.name(), entry.event.name());
    }

    #[test]
    fn a_stream_event_is_flat_and_tagged() {
        let entry = HistoryEntry {
            index: 4,
            recorded_at: ptc_core::Timestamp::from_millis(0),
            event: ptc_core::Event::Command(Command::CompleteRun {
                output: json!({ "closed": 3 }).into(),
            }),
        };
        let event = StreamEvent::Entry(Box::new(Entry::from_core(&entry, view)));
        assert_eq!(event.sse_event(), "entry");
        assert_eq!(event.sse_id().as_deref(), Some("4"));
        let written = serde_json::to_value(&event).unwrap();
        assert_eq!(written["type"], "entry");
        assert_eq!(written["idx"], 4);
        assert_eq!(written["event"]["type"], "complete_run");
        assert_eq!(written["event"]["output"]["value"], json!({ "closed": 3 }));
        assert_eq!(
            serde_json::from_value::<StreamEvent>(written).unwrap(),
            event
        );

        let end = StreamEvent::End {
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
}
