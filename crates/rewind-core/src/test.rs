//! Tests: the world a run meets, and what the outside must see at the end
//! (`archi/05-testing-and-observability.md`, "Testing and mocks").
//!
//! ```text
//! Test                    never names its script: the script under test is a parameter
//! ├── name, input, start?, seed?
//! ├── world               actions { <action>: [ActionRule] }, mocks?, cassette?
//! ├── signals             [{ name, payload, when }]
//! ├── expect              status?, output?, error?, calls [{ action, args?, times? }], order
//! └── origin?             run, version, namespace: a test recorded from a run
//! ```
//!
//! What is pure lives here: a test's own invariants ([`Test::check`]), its expectations checked
//! against what a run did ([`Expectations::check`]), the [`RunPath`] of a run, and a test
//! recorded from a run ([`Test::record`]). Running a test is the engine's; its stored form is a
//! document of the stored format v1 (`rewind-format`).

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde_json::{Value, json};

use crate::canonical::canonical_json;
use crate::cassette::{Cassette, CassetteError, Matching};
use crate::digest::Digest;
use crate::event::{
    AbortReason, ActionOutcome, Annotation, Command, Event, HistoryEntry, ScriptError,
};
use crate::history::{HistoryError, validate_history};
use crate::ids::{RunId, Seed};
use crate::payload::Payload;
use crate::state::{RunOutcome, RunState, RunStatus};
use crate::time::Timestamp;
use crate::workflow::is_reserved_signal;

/// A test: the world a run meets and what must hold at the end.
#[derive(Clone, Debug, PartialEq)]
pub struct Test {
    /// Unique within a suite.
    pub name: String,
    /// The run's input; `{}` by default.
    pub input: Value,
    /// When the run starts, in virtual time; the engine's default when `None`.
    pub start: Option<Timestamp>,
    /// The run's seed; a fixed one when `None`, the same for every test.
    pub seed: Option<Seed>,
    pub world: World,
    /// Sent to the run in order.
    pub signals: Vec<TestSignal>,
    pub expect: Expectations,
    /// Where a recorded test comes from.
    pub origin: Option<TestOrigin>,
}

/// What answers a run's calls during a test. A call is answered by the first source that has an
/// answer: the rules, then the mock handlers, then the cassette.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct World {
    /// By action, the rules tried in order.
    pub actions: BTreeMap<String, Vec<ActionRule>>,
    /// TypeScript mock handlers, run in a separate isolate.
    pub mocks: Option<String>,
    /// Calls and signals recorded from a run.
    pub cassette: Option<Cassette>,
}

impl World {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.mocks.is_none() && self.cassette.is_none()
    }
}

/// One answer of an action, to the calls whose args contain `args`, `times` times at most.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionRule {
    /// The call's args contain them ([`contains`]); any call when `None`.
    pub args: Option<Value>,
    pub answer: ActionOutcome,
    /// How many calls the rule answers; any number when `None`.
    pub times: Option<u32>,
}

impl ActionRule {
    /// A rule answering every call with `answer`.
    pub fn always(answer: ActionOutcome) -> Self {
        Self {
            args: None,
            answer,
            times: None,
        }
    }

    /// Whether the rule takes a call with these args.
    pub fn matches(&self, args: &Value) -> bool {
        self.args
            .as_ref()
            .is_none_or(|expected| contains(args, expected))
    }
}

/// A signal a test sends.
#[derive(Clone, Debug, PartialEq)]
pub struct TestSignal {
    pub name: String,
    pub payload: Value,
    pub when: SignalWhen,
}

/// When a test sends a signal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SignalWhen {
    /// Once the run can advance no further without the outside world.
    #[default]
    Blocked,
    /// This much virtual time after the start.
    After(Duration),
}

/// What must be true at the end of a test. Everything not given is not checked.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Expectations {
    pub status: Option<RunStatus>,
    /// The output contains it ([`contains`]).
    pub output: Option<Value>,
    pub error: Option<ExpectedError>,
    pub calls: Vec<ExpectedCall>,
    pub order: CallOrder,
}

impl Expectations {
    pub fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.output.is_none()
            && self.error.is_none()
            && self.calls.is_empty()
            && self.order == CallOrder::Any
    }
}

/// The error a run must fail with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedError {
    pub name: String,
    /// The message contains this text.
    pub message: Option<String>,
}

/// Calls a run must make: those to `action` whose args contain `args`.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpectedCall {
    pub action: String,
    pub args: Option<Value>,
    /// Exactly this many; at least one when `None`. `Some(0)`: never.
    pub times: Option<u32>,
}

impl ExpectedCall {
    fn matches(&self, call: &ObservedCall) -> bool {
        call.action == self.action
            && self
                .args
                .as_ref()
                .is_none_or(|expected| contains(&call.args, expected))
    }

    /// `github.close_issue`, or `github.close_issue with args containing {"number":4}`.
    fn describe(&self) -> String {
        match &self.args {
            Some(args) => format!(
                "{} with args containing {}",
                self.action,
                canonical_json(args)
            ),
            None => self.action.clone(),
        }
    }
}

/// Whether the order of the expected calls is checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallOrder {
    #[default]
    Any,
    /// The first call matching each entry comes in the order of the entries.
    Listed,
}

impl CallOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            CallOrder::Any => "any",
            CallOrder::Listed => "listed",
        }
    }
}

/// Where a recorded test comes from: it holds that run's data, and is read like the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestOrigin {
    pub run: RunId,
    /// The workflow version the run executed.
    pub version: Digest,
    /// The end user the run acted for.
    pub namespace: String,
}

/// A test that breaks an invariant. The messages are written for whoever wrote it, a harness
/// developer or a model.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidTest {
    #[error("a test needs a name")]
    EmptyName,
    #[error("invalid test name {0:?}: expected a name without control characters")]
    Name(String),
    #[error("the world of test {test:?} names an action with an empty name")]
    EmptyActionName { test: String },
    #[error("the world of test {test:?} gives {action} no rule: give one, or leave the action out")]
    NoRule { test: String, action: String },
    #[error(
        "rule {rule} of {action} in test {test:?} answers 0 calls: give `times` of 1 or more, or \
         leave the rule out"
    )]
    RuleTimes {
        test: String,
        action: String,
        rule: usize,
    },
    #[error("test {test:?} sends a signal with an empty name")]
    EmptySignalName { test: String },
    #[error(
        "test {test:?} sends the signal {name:?}: names starting with \"rewind:\" are reserved"
    )]
    ReservedSignal { test: String, name: String },
    #[error("test {test:?} expects a call to an action with an empty name")]
    EmptyExpectedAction { test: String },
}

impl Test {
    /// A test named `name` with the input `{}`, an empty world and no expectation.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            input: json!({}),
            start: None,
            seed: None,
            world: World::default(),
            signals: Vec::new(),
            expect: Expectations::default(),
            origin: None,
        }
    }

    /// Checks the test's own invariants: a name, rules that answer, signals a test may send.
    pub fn check(&self) -> Result<(), InvalidTest> {
        let test = || self.name.clone();
        if self.name.trim().is_empty() {
            return Err(InvalidTest::EmptyName);
        }
        if self.name.chars().any(char::is_control) {
            return Err(InvalidTest::Name(self.name.clone()));
        }
        for (action, rules) in &self.world.actions {
            if action.is_empty() {
                return Err(InvalidTest::EmptyActionName { test: test() });
            }
            if rules.is_empty() {
                return Err(InvalidTest::NoRule {
                    test: test(),
                    action: action.clone(),
                });
            }
            if let Some(rule) = rules.iter().position(|rule| rule.times == Some(0)) {
                return Err(InvalidTest::RuleTimes {
                    test: test(),
                    action: action.clone(),
                    rule: rule + 1,
                });
            }
        }
        for signal in &self.signals {
            if signal.name.is_empty() {
                return Err(InvalidTest::EmptySignalName { test: test() });
            }
            if is_reserved_signal(&signal.name) {
                return Err(InvalidTest::ReservedSignal {
                    test: test(),
                    name: signal.name.clone(),
                });
            }
        }
        if self.expect.calls.iter().any(|call| call.action.is_empty()) {
            return Err(InvalidTest::EmptyExpectedAction { test: test() });
        }
        Ok(())
    }

    /// The actions the test names: those its rules answer and those it expects.
    pub fn actions(&self) -> impl Iterator<Item = &str> {
        self.world
            .actions
            .keys()
            .map(String::as_str)
            .chain(self.expect.calls.iter().map(|call| call.action.as_str()))
    }

    /// The test recorded from a run that ended by itself, completed or failed: its input, start
    /// and seed, its calls as a cassette matched by args with its signal timeline, and as
    /// expectation its status, its output or its error, and how many times it called each action
    /// (`archi/05-testing-and-observability.md`, "Tests recorded from runs"). Every payload of
    /// the history must be loaded.
    pub fn record(history: &[HistoryEntry], origin: TestOrigin) -> Result<Self, RecordError> {
        let started = validate_history(history)?;
        let input = loaded(0, &started.input)?;
        let state = RunState::from_history(history)?;
        let mut expect = Expectations {
            status: Some(state.status()),
            ..Expectations::default()
        };
        match state.outcome() {
            Some(RunOutcome::Completed { output }) if state.status() == RunStatus::Completed => {
                let index = history.last().map_or(0, |entry| entry.index);
                expect.output = Some(loaded(index, output)?);
            }
            Some(RunOutcome::Failed { error }) if state.status() == RunStatus::Failed => {
                expect.error = Some(ExpectedError {
                    name: error.name.clone(),
                    message: Some(error.message.clone()),
                });
            }
            _ => return Err(RecordError::Status(state.status())),
        }
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        for entry in history {
            if let Event::Command(Command::ScheduleCall { action, .. }) = &entry.event {
                *counts.entry(action.clone()).or_default() += 1;
            }
        }
        expect.calls = counts
            .into_iter()
            .map(|(action, times)| ExpectedCall {
                action,
                args: None,
                times: Some(times),
            })
            .collect();
        let cassette = Cassette::from_history(history, Matching::ByArgs)?;
        Ok(Self {
            name: format!("recorded {}", origin.run),
            input,
            start: Some(history[0].recorded_at),
            seed: Some(started.seed),
            world: World {
                actions: BTreeMap::new(),
                mocks: None,
                cassette: Some(cassette),
            },
            signals: Vec::new(),
            expect,
            origin: Some(origin),
        })
    }
}

fn loaded(index: u64, payload: &Payload) -> Result<Value, RecordError> {
    match payload {
        Payload::Value(value) => Ok(value.clone()),
        Payload::Digest(digest) => Err(RecordError::Cassette(CassetteError::PayloadNotLoaded {
            index,
            digest: *digest,
        })),
    }
}

/// Why a run cannot be recorded as a test.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("invalid history: {0}")]
    History(#[from] HistoryError),
    #[error(transparent)]
    Cassette(#[from] CassetteError),
    /// Only a run that completed or failed by itself is recorded.
    #[error(
        "the run is {0}: only a run that completed or failed is recorded, since a cancellation, \
         an abort or a wait does not come from the script"
    )]
    Status(RunStatus),
}

// -------------------------------------------------------------------------------------------
// Checking expectations

/// A call a run made, as a test sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedCall {
    pub action: String,
    pub args: Value,
}

/// What a run did, seen from the outside: how it ended, and every call it scheduled in `seq`
/// order.
#[derive(Clone, Copy, Debug)]
pub struct Observed<'a> {
    pub status: RunStatus,
    pub output: Option<&'a Value>,
    pub error: Option<&'a ScriptError>,
    pub abort: Option<&'a AbortReason>,
    pub calls: &'a [ObservedCall],
}

impl Expectations {
    /// The expectations that do not hold, one sentence each, written for a model.
    pub fn check(&self, observed: &Observed<'_>) -> Vec<String> {
        let mut failures = Vec::new();
        if let Some(status) = self.status
            && observed.status != status
        {
            let detail = match (observed.error, observed.abort) {
                (Some(error), _) => format!(" ({}: {})", error.name, error.message),
                (_, Some(reason)) => format!(" ({reason})"),
                _ => String::new(),
            };
            failures.push(format!(
                "the run is {}{detail}, expected {status}",
                observed.status
            ));
        }
        if let Some(expected) = &self.output {
            match observed.output {
                Some(output) if contains(output, expected) => {}
                Some(output) => failures.push(format!(
                    "the output is {}, which does not contain {}",
                    canonical_json(output),
                    canonical_json(expected)
                )),
                None => failures.push(format!(
                    "the run has no output (it is {}), expected one containing {}",
                    observed.status,
                    canonical_json(expected)
                )),
            }
        }
        if let Some(expected) = &self.error {
            match observed.error {
                Some(error) if error.name != expected.name => failures.push(format!(
                    "the run failed with {}: {}, expected {}",
                    error.name, error.message, expected.name
                )),
                Some(error) => {
                    if let Some(message) = &expected.message
                        && !error.message.contains(message.as_str())
                    {
                        failures.push(format!(
                            "the error message {:?} does not contain {message:?}",
                            error.message
                        ));
                    }
                }
                None => failures.push(format!(
                    "the run did not fail (it is {}), expected {}",
                    observed.status, expected.name
                )),
            }
        }
        let mut firsts: Vec<(usize, Option<usize>)> = Vec::new();
        for (entry, expected) in self.calls.iter().enumerate() {
            let matching: Vec<usize> = observed
                .calls
                .iter()
                .enumerate()
                .filter(|(_, call)| expected.matches(call))
                .map(|(position, _)| position)
                .collect();
            let count = matching.len();
            match expected.times {
                Some(0) if count > 0 => failures.push(format!(
                    "{} was called {}, expected never",
                    expected.describe(),
                    times(count)
                )),
                Some(wanted) if count != wanted as usize => failures.push(format!(
                    "{} was called {}, expected {}",
                    expected.describe(),
                    times(count),
                    times(wanted as usize)
                )),
                None if count == 0 => failures.push(format!(
                    "{} was never called, expected at least once",
                    expected.describe()
                )),
                _ => {}
            }
            if expected.times != Some(0) {
                firsts.push((entry, matching.first().copied()));
            }
        }
        if self.order == CallOrder::Listed {
            let mut previous: Option<(usize, usize)> = None;
            for (entry, first) in firsts {
                let Some(first) = first else { continue };
                if let Some((before, at)) = previous
                    && first < at
                {
                    failures.push(format!(
                        "the first call of {} comes before the first call of {}, expected the \
                         order listed",
                        self.calls[entry].describe(),
                        self.calls[before].describe()
                    ));
                }
                previous = Some((entry, first));
            }
        }
        failures
    }
}

fn times(count: usize) -> String {
    match count {
        1 => "once".to_owned(),
        count => format!("{count} times"),
    }
}

/// `actual` contains `expected`: for objects, every expected key is in `actual` with a value
/// that contains the expected one; otherwise the same value as JavaScript sees it, compared by
/// canonical form (`2` and `2.0` are the same number).
pub fn contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|actual| contains(actual, value))
        }),
        _ => canonical_json(actual) == canonical_json(expected),
    }
}

// -------------------------------------------------------------------------------------------
// Paths

/// The path a run took: the actions it called in `seq` order and the signals it waited for, with
/// consecutive repeats collapsed, then how it ended. Runs of a workflow that took the same path
/// are recorded as one test (`archi/05-testing-and-observability.md`, "Tests recorded from
/// runs").
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunPath {
    pub steps: Vec<PathStep>,
    pub status: RunStatus,
}

/// One step of a [`RunPath`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PathStep {
    pub kind: StepKind,
    pub name: String,
    /// The step came several times in a row.
    pub repeated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StepKind {
    /// A call of the action `name`.
    Action,
    /// A wait on the signal `name` whose mailbox was empty.
    Signal,
}

impl RunPath {
    /// The path of a history. The signals Rewind sends itself (`rewind:ended:…`) are left out.
    pub fn of(history: &[HistoryEntry]) -> Result<Self, HistoryError> {
        let state = RunState::from_history(history)?;
        let mut steps: Vec<PathStep> = Vec::new();
        for entry in history {
            let (kind, name) = match &entry.event {
                Event::Command(Command::ScheduleCall { action, .. }) => (StepKind::Action, action),
                Event::Annotation(Annotation::SignalAwaited { name, .. })
                    if !is_reserved_signal(name) =>
                {
                    (StepKind::Signal, name)
                }
                _ => continue,
            };
            match steps.last_mut() {
                Some(last) if last.kind == kind && last.name == *name => last.repeated = true,
                _ => steps.push(PathStep {
                    kind,
                    name: name.clone(),
                    repeated: false,
                }),
            }
        }
        Ok(Self {
            steps,
            status: state.status(),
        })
    }

    /// Identifies the path: SHA-256 of its canonical JSON.
    pub fn digest(&self) -> Digest {
        let steps: Vec<Value> = self
            .steps
            .iter()
            .map(|step| {
                json!({
                    "kind": match step.kind { StepKind::Action => "action", StepKind::Signal => "signal" },
                    "name": step.name,
                    "repeated": step.repeated,
                })
            })
            .collect();
        let document = json!({ "steps": steps, "status": self.status.as_str() });
        Digest::of(canonical_json(&document).as_bytes())
    }
}

impl fmt::Display for RunPath {
    /// `github.list_issues · signal approval · github.close_issue×n → completed`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, step) in self.steps.iter().enumerate() {
            if i > 0 {
                f.write_str(" · ")?;
            }
            if step.kind == StepKind::Signal {
                f.write_str("signal ")?;
            }
            f.write_str(&step.name)?;
            if step.repeated {
                f.write_str("×n")?;
            }
        }
        if !self.steps.is_empty() {
            f.write_str(" ")?;
        }
        write!(f, "→ {}", self.status)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::event::{
        ActionFailure, ActionFailureKind, Completion, Lifecycle, RunMode, RunStarted,
    };
    use crate::ids::{Epoch, Seq};

    /// Entry `index` is recorded at `100 + index × 1000`.
    fn entry(index: u64, event: Event) -> HistoryEntry {
        HistoryEntry {
            index,
            recorded_at: Timestamp(100 + index * 1_000),
            event,
        }
    }

    /// Numbers the entries again after one was inserted or removed.
    fn renumbered(mut history: Vec<HistoryEntry>) -> Vec<HistoryEntry> {
        for (index, entry) in history.iter_mut().enumerate() {
            entry.index = index as u64;
            entry.recorded_at = Timestamp(100 + index as u64 * 1_000);
        }
        history
    }

    fn started() -> Event {
        Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
            script: Digest::of(b"script"),
            action_set: Digest::of(b"action_set"),
            input: Payload::Value(json!({ "repo": "acme/api" })),
            seed: Seed::from_bytes([7; 32]),
            epoch: Epoch(1),
            expires_at: Timestamp(1_000_000),
            mode: RunMode::Live,
        }))
    }

    fn call(index: u64, seq: u32, action: &str, args: Value) -> HistoryEntry {
        entry(
            index,
            Event::Command(Command::ScheduleCall {
                seq: Seq(seq),
                action: action.into(),
                args: Payload::Value(args),
                callsite: None,
            }),
        )
    }

    fn done(index: u64, seq: u32, result: Value) -> HistoryEntry {
        entry(
            index,
            Event::Completion(Completion::CallCompleted {
                seq: Seq(seq),
                result: Payload::Value(result),
            }),
        )
    }

    fn awaited(index: u64, name: &str) -> HistoryEntry {
        entry(
            index,
            Event::Annotation(Annotation::SignalAwaited {
                name: name.into(),
                timeout: None,
                callsite: None,
            }),
        )
    }

    fn signal(index: u64, name: &str, payload: Value) -> HistoryEntry {
        entry(
            index,
            Event::Completion(Completion::SignalReceived {
                name: name.into(),
                payload: Payload::Value(payload),
            }),
        )
    }

    fn completed(index: u64, output: Value) -> HistoryEntry {
        entry(
            index,
            Event::Command(Command::CompleteRun {
                output: Payload::Value(output),
            }),
        )
    }

    /// A run that lists issues, waits for approval, closes two and completes.
    fn triage() -> Vec<HistoryEntry> {
        vec![
            entry(0, started()),
            call(1, 1, "github.list_issues", json!({ "state": "open" })),
            done(2, 1, json!([{ "number": 4 }, { "number": 7 }])),
            awaited(3, "approval"),
            signal(4, "approval", json!({ "close": true })),
            call(5, 2, "github.close_issue", json!({ "number": 4 })),
            call(6, 3, "github.close_issue", json!({ "number": 7 })),
            done(7, 2, json!({})),
            done(8, 3, json!({})),
            completed(9, json!({ "closed": 2 })),
        ]
    }

    fn origin() -> TestOrigin {
        TestOrigin {
            run: RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0001),
            version: Digest::of(b"version"),
            namespace: "alice".into(),
        }
    }

    fn observed_calls(history: &[HistoryEntry]) -> Vec<ObservedCall> {
        history
            .iter()
            .filter_map(|entry| match &entry.event {
                Event::Command(Command::ScheduleCall { action, args, .. }) => Some(ObservedCall {
                    action: action.clone(),
                    args: args.value().cloned().unwrap_or_default(),
                }),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_completed_run_is_recorded_with_what_it_did() {
        let test = Test::record(&triage(), origin()).unwrap();
        assert_eq!(test.name, "recorded 01930000-0000-7000-8000-000000000001");
        assert_eq!(test.input, json!({ "repo": "acme/api" }));
        assert_eq!(test.start, Some(Timestamp(100)));
        assert_eq!(test.seed, Some(Seed::from_bytes([7; 32])));
        let cassette = test.world.cassette.as_ref().unwrap();
        assert_eq!(cassette.matching, Matching::ByArgs);
        assert_eq!(cassette.calls.len(), 3);
        assert_eq!(cassette.signals[0].elapsed_ms, 4_000);
        assert_eq!(test.expect.status, Some(RunStatus::Completed));
        assert_eq!(test.expect.output, Some(json!({ "closed": 2 })));
        assert_eq!(
            test.expect.calls,
            vec![
                ExpectedCall {
                    action: "github.close_issue".into(),
                    args: None,
                    times: Some(2),
                },
                ExpectedCall {
                    action: "github.list_issues".into(),
                    args: None,
                    times: Some(1),
                },
            ]
        );
        assert_eq!(test.origin, Some(origin()));
        test.check().unwrap();
        // The run passes its own recorded expectations.
        let history = triage();
        let calls = observed_calls(&history);
        let output = json!({ "closed": 2 });
        let observed = Observed {
            status: RunStatus::Completed,
            output: Some(&output),
            error: None,
            abort: None,
            calls: &calls,
        };
        assert_eq!(test.expect.check(&observed), Vec::<String>::new());
    }

    #[test]
    fn a_failed_run_is_recorded_with_its_error_and_a_waiting_one_is_not() {
        let mut history = triage();
        history.truncate(9);
        history.push(entry(
            9,
            Event::Command(Command::FailRun {
                error: ScriptError {
                    name: "TypeError".into(),
                    message: "Cannot read properties of undefined".into(),
                    stack: None,
                },
            }),
        ));
        let test = Test::record(&history, origin()).unwrap();
        assert_eq!(test.expect.status, Some(RunStatus::Failed));
        assert_eq!(
            test.expect.error,
            Some(ExpectedError {
                name: "TypeError".into(),
                message: Some("Cannot read properties of undefined".into()),
            })
        );
        assert_eq!(test.expect.output, None);

        let mut waiting = triage();
        waiting.truncate(4);
        assert_eq!(
            Test::record(&waiting, origin()),
            Err(RecordError::Status(RunStatus::Active))
        );
    }

    #[test]
    fn paths_collapse_repeats_and_leave_reserved_signals_out() {
        let path = RunPath::of(&triage()).unwrap();
        assert_eq!(
            path.to_string(),
            "github.list_issues · signal approval · github.close_issue×n → completed"
        );
        // Three closes or two: the same path.
        let mut more = triage();
        more.insert(7, call(0, 4, "github.close_issue", json!({ "number": 9 })));
        more.insert(10, done(0, 4, json!({})));
        let more = renumbered(more);
        assert_eq!(RunPath::of(&more).unwrap().digest(), path.digest());

        let mut child = triage();
        child.insert(
            3,
            awaited(0, "rewind:ended:01930000-0000-7000-8000-000000000002"),
        );
        assert_eq!(RunPath::of(&renumbered(child)).unwrap(), path);

        let mut without_approval = triage();
        without_approval.remove(3);
        let without_approval = renumbered(without_approval);
        assert_ne!(
            RunPath::of(&without_approval).unwrap().digest(),
            path.digest()
        );
    }

    fn observe<'a>(calls: &'a [ObservedCall], output: Option<&'a Value>) -> Observed<'a> {
        Observed {
            status: RunStatus::Completed,
            output,
            error: None,
            abort: None,
            calls,
        }
    }

    fn calls(list: &[(&str, Value)]) -> Vec<ObservedCall> {
        list.iter()
            .map(|(action, args)| ObservedCall {
                action: (*action).into(),
                args: args.clone(),
            })
            .collect()
    }

    #[test]
    fn calls_are_checked_by_action_args_and_count() {
        let made = calls(&[
            (
                "slack.post_message",
                json!({ "channel": "#triage", "text": "2 stale" }),
            ),
            ("github.close_issue", json!({ "number": 4 })),
            ("github.close_issue", json!({ "number": 7 })),
        ]);
        let expect = |calls: Vec<ExpectedCall>| Expectations {
            calls,
            ..Expectations::default()
        };
        let call = |action: &str, args: Option<Value>, times: Option<u32>| ExpectedCall {
            action: action.into(),
            args,
            times,
        };
        let holds = expect(vec![
            call(
                "slack.post_message",
                Some(json!({ "channel": "#triage" })),
                Some(1),
            ),
            call("github.close_issue", None, Some(2)),
            call("github.close_issue", Some(json!({ "number": 7 })), None),
            call("github.delete_repo", None, Some(0)),
        ]);
        assert_eq!(holds.check(&observe(&made, None)), Vec::<String>::new());

        let fails = expect(vec![
            call("github.close_issue", None, Some(3)),
            call("github.close_issue", Some(json!({ "number": 9 })), None),
            call("slack.post_message", None, Some(0)),
        ]);
        assert_eq!(
            fails.check(&observe(&made, None)),
            vec![
                "github.close_issue was called 2 times, expected 3 times",
                "github.close_issue with args containing {\"number\":9} was never called, \
                 expected at least once",
                "slack.post_message was called once, expected never",
            ]
        );
    }

    #[test]
    fn listed_order_compares_the_first_matching_calls() {
        let made = calls(&[
            ("github.list_issues", json!({})),
            ("slack.post_message", json!({})),
            ("github.close_issue", json!({ "number": 4 })),
        ]);
        let call = |action: &str| ExpectedCall {
            action: action.into(),
            args: None,
            times: None,
        };
        let mut expect = Expectations {
            calls: vec![call("slack.post_message"), call("github.close_issue")],
            order: CallOrder::Listed,
            ..Expectations::default()
        };
        assert!(expect.check(&observe(&made, None)).is_empty());
        expect.calls.reverse();
        assert_eq!(
            expect.check(&observe(&made, None)),
            vec![
                "the first call of slack.post_message comes before the first call of \
                 github.close_issue, expected the order listed"
            ]
        );
        // In any order, the same calls hold.
        expect.order = CallOrder::Any;
        assert!(expect.check(&observe(&made, None)).is_empty());
    }

    #[test]
    fn status_output_and_error_are_checked() {
        let output = json!({ "closed": 2, "repo": "acme/api" });
        let expect = Expectations {
            status: Some(RunStatus::Completed),
            output: Some(json!({ "closed": 2.0 })),
            ..Expectations::default()
        };
        assert!(expect.check(&observe(&[], Some(&output))).is_empty());
        let expect = Expectations {
            output: Some(json!({ "closed": 3 })),
            error: Some(ExpectedError {
                name: "TypeError".into(),
                message: None,
            }),
            ..Expectations::default()
        };
        assert_eq!(
            expect.check(&observe(&[], Some(&output))),
            vec![
                "the output is {\"closed\":2,\"repo\":\"acme/api\"}, which does not contain \
                 {\"closed\":3}",
                "the run did not fail (it is completed), expected TypeError",
            ]
        );
        let error = ScriptError {
            name: "ActionError".into(),
            message: "rate limited".into(),
            stack: None,
        };
        let failed = Observed {
            status: RunStatus::Failed,
            output: None,
            error: Some(&error),
            abort: None,
            calls: &[],
        };
        let expect = Expectations {
            status: Some(RunStatus::Completed),
            ..Expectations::default()
        };
        assert_eq!(
            expect.check(&failed),
            vec!["the run is failed (ActionError: rate limited), expected completed"]
        );
    }

    #[test]
    fn expected_values_are_compared_as_javascript_values() {
        // A script's `4 / 2` is the number 2, whether the test writes 2 or 2.0.
        assert!(contains(&json!({ "ratio": 2 }), &json!({ "ratio": 2.0 })));
        assert!(contains(
            &json!([1, { "b": 2.0, "a": 1 }]),
            &json!([1.0, { "a": 1, "b": 2 }])
        ));
        assert!(contains(&json!(-0.0), &json!(0)));
        assert!(!contains(&json!({ "ratio": 2 }), &json!({ "ratio": 2.5 })));
        assert!(!contains(&json!([1, 2]), &json!([1])));
        // Objects are compared key by key; the run's output may hold more.
        assert!(contains(&json!({ "a": 1, "b": 2 }), &json!({ "a": 1.0 })));
    }

    #[test]
    fn invalid_tests_are_refused_with_a_reason() {
        let mut test = Test::new("closes once approved");
        test.check().unwrap();
        test.world.actions.insert(
            "github.close_issue".into(),
            vec![ActionRule {
                args: None,
                answer: ActionOutcome::Result(json!({})),
                times: Some(0),
            }],
        );
        assert_eq!(
            test.check().unwrap_err().to_string(),
            "rule 1 of github.close_issue in test \"closes once approved\" answers 0 calls: give \
             `times` of 1 or more, or leave the rule out"
        );
        test.world.actions.clear();
        test.signals.push(TestSignal {
            name: "rewind:ended:x".into(),
            payload: Value::Null,
            when: SignalWhen::Blocked,
        });
        assert!(matches!(
            test.check(),
            Err(InvalidTest::ReservedSignal { .. })
        ));
        assert_eq!(Test::new(" ").check(), Err(InvalidTest::EmptyName));
        let failure = ActionOutcome::Failure(ActionFailure {
            kind: ActionFailureKind::Error,
            message: "nope".into(),
            data: None,
        });
        let rule = ActionRule {
            args: Some(json!({ "number": 4 })),
            answer: failure,
            times: None,
        };
        assert!(rule.matches(&json!({ "number": 4, "repo": "acme/api" })));
        assert!(!rule.matches(&json!({ "number": 5 })));
    }
}
