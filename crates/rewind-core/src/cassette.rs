//! Cassettes: the mocks recorded from a real run (`archi/05-testing-and-observability.md`,
//! "Testing and mocks").
//!
//! A cassette holds a run's calls with their args and outcomes, and the timeline of its
//! signals. It is derived from the run's history, a pure function of it, and replayed in mock
//! mode: `sequential` hands the calls out in `seq` order, `by_args` by action and canonical args.
//! Its stored form is a document of the stored format v1 (`rewind-format`).
//!
//! The start of a workflow run is recorded with the outcome of that run, read from the
//! `rewind:ended:` signal that brought its end, as mock mode answers a start; those signals stay
//! out of the timeline (`archi/03-execution.md`, "Workflows across runs").

use std::collections::BTreeMap;

use serde_json::Value;

use crate::digest::Digest;
use crate::event::{
    ActionFailure, ActionFailureKind, ActionOutcome, Command, Completion, Event, HistoryEvent,
};
use crate::history::{HistoryError, validate_history};
use crate::ids::{RunId, Seq};
use crate::payload::Payload;
use crate::workflow::{RunEnd, WorkflowOp, ended_run, is_reserved_signal};

/// The recorded calls and signals of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Cassette {
    /// The calls that have an outcome, in `seq` order.
    pub calls: Vec<RecordedCall>,
    /// The signals the run received, in arrival order.
    pub signals: Vec<RecordedSignal>,
    pub matching: Matching,
}

/// One call and its outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedCall {
    pub action: String,
    pub args: Value,
    pub outcome: ActionOutcome,
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
    /// The n-th call of the run gets the n-th recorded call, which must be to the same action
    /// with the same args.
    Sequential,
    /// A call gets the first unused recorded call to the same action with the same canonical
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
    #[error("event {index} holds only the digest {digest} of its payload: load it first")]
    PayloadNotLoaded { index: u64, digest: Digest },
}

impl Cassette {
    /// The cassette of a history whose payloads are all loaded. Calls left without an outcome
    /// (a cancelled or aborted run) are not recorded: there is nothing to replay for them.
    pub fn from_history(
        history: &[HistoryEvent],
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
        let mut outcomes: BTreeMap<Seq, ActionOutcome> = BTreeMap::new();
        let mut signals = Vec::new();
        let mut ends: BTreeMap<RunId, Value> = BTreeMap::new();
        for entry in history {
            match &entry.event {
                Event::Command(Command::ScheduleCall {
                    seq, action, args, ..
                }) => {
                    scheduled.insert(*seq, (action.clone(), value(entry.index, args)?));
                }
                Event::Completion(Completion::CallCompleted { seq, result }) => {
                    outcomes.insert(*seq, ActionOutcome::Result(value(entry.index, result)?));
                }
                Event::Completion(Completion::CallFailed { seq, error }) => {
                    outcomes.insert(*seq, ActionOutcome::Failure(error.clone()));
                }
                Event::Completion(Completion::SignalReceived { name, payload })
                    if is_reserved_signal(name) =>
                {
                    if let Some(run) = ended_run(name) {
                        ends.insert(run, value(entry.index, payload)?);
                    }
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
            .filter_map(|(seq, (action, args))| {
                let mut outcome = outcomes.remove(&seq)?;
                if matches!(WorkflowOp::of_action(&action), Some((_, WorkflowOp::Start))) {
                    outcome = started_run_outcome(outcome, &ends);
                }
                Some(RecordedCall {
                    action,
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

/// What a cassette records for a workflow's start whose call had `outcome`: the outcome of the
/// run it started, from its end; a failed start as it failed.
fn started_run_outcome(outcome: ActionOutcome, ends: &BTreeMap<RunId, Value>) -> ActionOutcome {
    let ActionOutcome::Result(result) = &outcome else {
        return outcome;
    };
    let Some(run) = result
        .get("runId")
        .and_then(Value::as_str)
        .and_then(|run| run.parse::<RunId>().ok())
    else {
        return outcome;
    };
    match ends.get(&run).and_then(RunEnd::from_json) {
        Some(end) => end.to_mock(),
        None => ActionOutcome::Failure(ActionFailure {
            kind: ActionFailureKind::Error,
            message: format!("the run {run} had not ended when the cassette was recorded"),
            data: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::event::{
        AbortKind, AbortReason, ActionFailure, ActionFailureKind, Lifecycle, RunMode, RunStarted,
    };
    use crate::ids::{Epoch, Seed};
    use crate::time::Timestamp;

    fn entry(index: u64, at: u64, event: Event) -> HistoryEvent {
        HistoryEvent {
            index,
            recorded_at: Timestamp(at),
            event,
        }
    }

    fn started() -> Event {
        Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
            code: Digest::of(b"s"),
            action_set: Digest::of(b"t"),
            input: json!({}).into(),
            seed: Seed::from_bytes([0; 32]),
            epoch: Epoch(1),
            expires_at: Timestamp(10_000),
            mode: RunMode::Live,
        }))
    }

    fn call(seq: u32, action: &str, args: Value) -> Event {
        Event::Command(Command::ScheduleCall {
            seq: Seq(seq),
            action: action.into(),
            args: args.into(),
            callsite: None,
        })
    }

    #[test]
    fn records_calls_in_seq_order_with_their_outcomes_and_the_signals() {
        let failure = ActionFailure {
            kind: ActionFailureKind::Error,
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
                Event::Completion(Completion::CallFailed {
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
                Event::Completion(Completion::CallCompleted {
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
                        action: "github.get".into(),
                        args: json!({ "n": 1 }),
                        outcome: ActionOutcome::Result(json!({ "title": "crash" })),
                    },
                    RecordedCall {
                        action: "github.get".into(),
                        args: json!({ "n": 2 }),
                        outcome: ActionOutcome::Failure(failure),
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
    fn a_workflow_start_is_recorded_with_the_outcome_of_its_run() {
        let child: RunId = "0193a5c4-0000-7000-8000-000000000001".parse().unwrap();
        let lost: RunId = "0193a5c4-0000-7000-8000-000000000002".parse().unwrap();
        let start = |seq: u32, repo: &str| {
            call(
                seq,
                "workflows.triage.start",
                json!({ "input": { "repo": repo } }),
            )
        };
        let resolved = |seq: u32, run: RunId| {
            Event::Completion(Completion::CallCompleted {
                seq: Seq(seq),
                result: json!({ "runId": run.to_string() }).into(),
            })
        };
        let history = vec![
            entry(0, 0, started()),
            entry(1, 0, start(1, "a")),
            entry(2, 0, start(2, "b")),
            entry(3, 10, resolved(1, child)),
            entry(4, 10, resolved(2, lost)),
            entry(
                5,
                20,
                Event::Completion(Completion::SignalReceived {
                    name: crate::end_signal_name(child),
                    payload: json!({ "status": "completed", "output": { "closed": 2 } }).into(),
                }),
            ),
        ];
        let cassette = Cassette::from_history(&history, Matching::Sequential).unwrap();
        assert_eq!(cassette.signals, []);
        assert_eq!(
            cassette.calls[0].outcome,
            ActionOutcome::Result(json!({ "closed": 2 }))
        );
        let ActionOutcome::Failure(failure) = &cassette.calls[1].outcome else {
            panic!("a run that had not ended is a failure");
        };
        assert!(
            failure.message.contains("had not ended"),
            "{}",
            failure.message
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
                Event::Command(Command::ScheduleCall {
                    seq: Seq(1),
                    action: "t".into(),
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
            ActionOutcome::Result(json!(1)).into_completion(Seq(4)),
            Completion::CallCompleted {
                seq: Seq(4),
                result: json!(1).into()
            }
        );
    }
}
