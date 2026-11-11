//! Cassettes: the mocks recorded from a real run (`archi/05-testing-and-observability.md`,
//! "Testing and mocks").
//!
//! A cassette holds a run's tool calls with their args and outcomes, and the timeline of its
//! signals. It is derived from the run's history, a pure function of it, and replayed in mock
//! mode: `sequential` hands the calls out in `seq` order, `by_args` by tool and canonical args.
//! Its stored form is a document of the stored format v1 (`ptc-format`).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::digest::Digest;
use crate::event::{Command, Completion, Event, HistoryEntry, ToolOutcome};
use crate::history::{HistoryError, validate_history};
use crate::ids::Seq;
use crate::payload::Payload;

/// The recorded calls and signals of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Cassette {
    /// The calls that have an outcome, in `seq` order.
    pub calls: Vec<RecordedCall>,
    /// The signals the run received, in arrival order.
    pub signals: Vec<RecordedSignal>,
    pub matching: Matching,
}

/// One tool call and its outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedCall {
    pub tool: String,
    pub args: Value,
    pub outcome: ToolOutcome,
}

/// One signal, and when it arrived.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedSignal {
    pub name: String,
    pub payload: Value,
    /// Logical time between the start of the run and the signal's arrival, in milliseconds.
    pub elapsed_ms: u64,
}

/// How a cassette answers a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Matching {
    /// The n-th call of the run gets the n-th recorded call, which must be to the same tool
    /// with the same args.
    Sequential,
    /// A call gets the first unused recorded call to the same tool with the same canonical
    /// args, whatever its position.
    ByArgs,
}

impl Matching {
    pub fn as_str(self) -> &'static str {
        match self {
            Matching::Sequential => "sequential",
            Matching::ByArgs => "by_args",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CassetteError {
    #[error("invalid history: {0}")]
    History(#[from] HistoryError),
    /// A cassette is self-contained: every payload must be loaded before it is recorded.
    #[error("entry {index} holds only the digest {digest} of its payload: load it first")]
    PayloadNotLoaded { index: u64, digest: Digest },
}

impl Cassette {
    /// The cassette of a history whose payloads are all loaded. Calls left without an outcome
    /// (a cancelled or aborted run) are not recorded: there is nothing to replay for them.
    pub fn from_history(
        history: &[HistoryEntry],
        matching: Matching,
    ) -> Result<Self, CassetteError> {
        validate_history(history)?;
        let started_at = history[0].recorded_at;
        let value = |index: u64, payload: &Payload| -> Result<Value, CassetteError> {
            match payload {
                Payload::Value(value) => Ok(value.clone()),
                Payload::Digest(digest) => Err(CassetteError::PayloadNotLoaded {
                    index,
                    digest: *digest,
                }),
            }
        };

        let mut scheduled: BTreeMap<Seq, (String, Value)> = BTreeMap::new();
        let mut outcomes: BTreeMap<Seq, ToolOutcome> = BTreeMap::new();
        let mut signals = Vec::new();
        for entry in history {
            match &entry.event {
                Event::Command(Command::ScheduleToolCall {
                    seq, tool, args, ..
                }) => {
                    scheduled.insert(*seq, (tool.clone(), value(entry.index, args)?));
                }
                Event::Completion(Completion::ToolCallCompleted { seq, result }) => {
                    outcomes.insert(*seq, ToolOutcome::Result(value(entry.index, result)?));
                }
                Event::Completion(Completion::ToolCallFailed { seq, error }) => {
                    outcomes.insert(*seq, ToolOutcome::Failure(error.clone()));
                }
                Event::Completion(Completion::SignalReceived { name, payload }) => {
                    signals.push(RecordedSignal {
                        name: name.clone(),
                        payload: value(entry.index, payload)?,
                        elapsed_ms: entry.recorded_at.0.saturating_sub(started_at.0),
                    });
                }
                _ => {}
            }
        }
        let calls = scheduled
            .into_iter()
            .filter_map(|(seq, (tool, args))| {
                let outcome = outcomes.remove(&seq)?;
                Some(RecordedCall {
                    tool,
                    args,
                    outcome,
                })
            })
            .collect();
        Ok(Self {
            calls,
            signals,
            matching,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::event::{
        AbortKind, AbortReason, Lifecycle, RunMode, RunStarted, ToolFailure, ToolFailureKind,
    };
    use crate::ids::{Epoch, Seed};
    use crate::time::Timestamp;

    fn entry(index: u64, at: u64, event: Event) -> HistoryEntry {
        HistoryEntry {
            index,
            recorded_at: Timestamp(at),
            event,
        }
    }

    fn started() -> Event {
        Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
            script: Digest::of(b"s"),
            toolset: Digest::of(b"t"),
            input: json!({}).into(),
            seed: Seed::from_bytes([0; 32]),
            epoch: Epoch(1),
            expires_at: Timestamp(10_000),
            mode: RunMode::Live,
        }))
    }

    fn call(seq: u32, tool: &str, args: Value) -> Event {
        Event::Command(Command::ScheduleToolCall {
            seq: Seq(seq),
            tool: tool.into(),
            args: args.into(),
            callsite: None,
        })
    }

    #[test]
    fn records_calls_in_seq_order_with_their_outcomes_and_the_signals() {
        let failure = ToolFailure {
            kind: ToolFailureKind::Error,
            message: "not found".into(),
            data: None,
        };
        let history = vec![
            entry(0, 1_000, started()),
            entry(1, 1_000, call(1, "github.get", json!({ "n": 1 }))),
            entry(2, 1_000, call(2, "github.get", json!({ "n": 2 }))),
            entry(
                3,
                1_200,
                Event::Completion(Completion::ToolCallFailed {
                    seq: Seq(2),
                    error: failure.clone(),
                }),
            ),
            entry(
                4,
                1_500,
                Event::Completion(Completion::SignalReceived {
                    name: "approval".into(),
                    payload: json!(true).into(),
                }),
            ),
            entry(
                5,
                1_700,
                Event::Completion(Completion::ToolCallCompleted {
                    seq: Seq(1),
                    result: json!({ "title": "crash" }).into(),
                }),
            ),
            entry(6, 1_700, call(3, "slack.post", json!({}))),
            entry(
                7,
                1_800,
                Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
                    kind: AbortKind::Internal,
                    message: "x".into(),
                })),
            ),
        ];
        let cassette = Cassette::from_history(&history, Matching::ByArgs).unwrap();
        assert_eq!(
            cassette,
            Cassette {
                calls: vec![
                    RecordedCall {
                        tool: "github.get".into(),
                        args: json!({ "n": 1 }),
                        outcome: ToolOutcome::Result(json!({ "title": "crash" })),
                    },
                    RecordedCall {
                        tool: "github.get".into(),
                        args: json!({ "n": 2 }),
                        outcome: ToolOutcome::Failure(failure),
                    },
                ],
                signals: vec![RecordedSignal {
                    name: "approval".into(),
                    payload: json!(true),
                    elapsed_ms: 500,
                }],
                matching: Matching::ByArgs,
            }
        );
    }

    #[test]
    fn payloads_must_be_loaded() {
        let digest = Digest::of(b"{}");
        let history = vec![
            entry(0, 0, started()),
            entry(
                1,
                0,
                Event::Command(Command::ScheduleToolCall {
                    seq: Seq(1),
                    tool: "t".into(),
                    args: Payload::Digest(digest),
                    callsite: None,
                }),
            ),
        ];
        assert_eq!(
            Cassette::from_history(&history, Matching::Sequential),
            Err(CassetteError::PayloadNotLoaded { index: 1, digest })
        );
        assert_eq!(
            Cassette::from_history(&[], Matching::Sequential),
            Err(CassetteError::History(HistoryError::Empty))
        );
    }

    #[test]
    fn an_outcome_becomes_the_completion_of_its_call() {
        assert_eq!(
            ToolOutcome::Result(json!(1)).into_completion(Seq(4)),
            Completion::ToolCallCompleted {
                seq: Seq(4),
                result: json!(1).into()
            }
        );
    }
}
