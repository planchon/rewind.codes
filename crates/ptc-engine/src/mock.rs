//! Mock mode (`archi/05-testing-and-observability.md`, "Testing and mocks").
//!
//! Mock mode is not a separate system: it is the same engine wired with in-memory adapters, a
//! virtual clock, and a dispatcher that answers from mocks. A call is answered by the first
//! source that has an answer:
//!
//! 1. a **cassette** recorded from a real run ([`CassetteDispatcher`]): `sequential` hands the
//!    recorded calls out in order, `by_args` by tool and canonical args;
//! 2. **mock handlers** ([`Mocks`]): any [`ToolDispatcher`], for the tools it handles. The
//!    TypeScript mock handlers run in a separate isolate, behind that port (`ptc-runtime`);
//! 3. **static responses** per tool, as `ptc.tools.json` holds them;
//! 4. otherwise the call fails with "no recorded response for `tool({…})`".
//!
//! A [`Scenario`] ties it together (`ptc test`, `POST /v1/test`): a script, its input, the
//! signals to send and when, and what must be true at the end. It runs in virtual time, so a
//! 10-day sleep takes no wall-clock time, and reports a trace with the failed expectations.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use ptc_core::{
    AbortReason, Cassette, Command, Event, HistoryEntry, JsonValue, Matching, Payload,
    RecordedCall, RunId, RunMode, RunOutcome, RunState, RunStatus, Script, ScriptError, Seq,
    Timestamp, ToolFailure, ToolFailureKind, ToolOutcome, Toolset,
};

use crate::adapters::{
    MemoryBlobs, MemoryJournal, SeededEntropy, StaticDispatcher, StaticResponsesError, VirtualClock,
};
use crate::driver::DriveOptions;
use crate::engine::{Adapters, Engine, EngineConfig, EngineError, StartError, StartRun};
use crate::ports::{
    Credentials, Dispatched, Enqueued, RunRef, ScriptExecutor, ToolCall, ToolDispatcher,
};

/// Characters of the args shown when no source answers a call.
const ARGS_CHARS: usize = 80;

/// Answers calls from a cassette recorded from a real run.
#[derive(Debug)]
pub struct CassetteDispatcher {
    cassette: Cassette,
    state: Mutex<CassetteState>,
}

#[derive(Debug, Default)]
struct CassetteState {
    /// Recorded calls already handed out.
    used: BTreeSet<usize>,
    /// `sequential`: the next recorded call.
    next: usize,
    /// A call answered once gets the same answer again (an attempt after a lost claim).
    answered: BTreeMap<(RunId, Seq), usize>,
}

impl CassetteDispatcher {
    pub fn new(cassette: Cassette) -> Self {
        Self {
            cassette,
            state: Mutex::new(CassetteState::default()),
        }
    }

    pub fn cassette(&self) -> &Cassette {
        &self.cassette
    }

    /// The recorded outcome for this call, if the cassette has one: under `sequential`, the
    /// next recorded call if it is to the same tool with the same args; under `by_args`, the
    /// first unused recorded call to the same tool with the same canonical args.
    pub fn answer(&self, call: &ToolCall) -> Option<ToolOutcome> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (call.run.id, call.seq);
        if let Some(position) = state.answered.get(&key) {
            return Some(self.cassette.calls[*position].outcome.clone());
        }
        let same = |recorded: &RecordedCall| {
            recorded.tool == call.tool.name
                && Payload::Value(recorded.args.clone()).same_as(&Payload::Value(call.args.clone()))
        };
        let position = match self.cassette.matching {
            Matching::Sequential => {
                let position = state.next;
                let recorded = self.cassette.calls.get(position)?;
                if !same(recorded) {
                    return None;
                }
                state.next += 1;
                position
            }
            Matching::ByArgs => {
                self.cassette
                    .calls
                    .iter()
                    .enumerate()
                    .find(|(position, recorded)| !state.used.contains(position) && same(recorded))?
                    .0
            }
        };
        state.used.insert(position);
        state.answered.insert(key, position);
        Some(self.cassette.calls[position].outcome.clone())
    }
}

impl ToolDispatcher for CassetteDispatcher {
    async fn dispatch(&self, call: ToolCall, _credentials: Option<Credentials>) -> Dispatched {
        match self.answer(&call) {
            Some(outcome) => dispatched(outcome),
            None => Dispatched::Failed(no_response(&call)),
        }
    }
}

fn dispatched(outcome: ToolOutcome) -> Dispatched {
    match outcome {
        ToolOutcome::Result(result) => Dispatched::Result(result),
        ToolOutcome::Failure(failure) => Dispatched::Failed(failure),
    }
}

fn no_response(call: &ToolCall) -> ToolFailure {
    ToolFailure {
        kind: ToolFailureKind::Error,
        message: format!(
            "no recorded response for `{}({})`",
            call.tool.name,
            Payload::Value(call.args.clone()).preview(ARGS_CHARS)
        ),
        data: None,
    }
}

/// Mock handlers: any dispatcher, for the tools it handles. The TypeScript mock handlers of a
/// scenario (`ptc-runtime`, in a separate isolate) are one.
#[derive(Debug)]
pub struct Mocks<M> {
    pub handlers: M,
    /// The tools the handlers answer; the others go to the next source.
    pub tools: BTreeSet<String>,
}

/// The dispatcher of mock mode: a cassette, then mock handlers, then static responses.
#[derive(Debug)]
pub struct MockDispatcher<M> {
    cassette: Option<CassetteDispatcher>,
    mocks: Option<Mocks<M>>,
    statics: StaticDispatcher,
    static_tools: BTreeSet<String>,
}

/// No mock handlers.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMocks;

impl ToolDispatcher for NoMocks {
    async fn dispatch(&self, call: ToolCall, _credentials: Option<Credentials>) -> Dispatched {
        Dispatched::Failed(no_response(&call))
    }
}

impl MockDispatcher<NoMocks> {
    /// A dispatcher that answers nothing until sources are added.
    pub fn new() -> Self {
        Self {
            cassette: None,
            mocks: None,
            statics: StaticDispatcher::new(),
            static_tools: BTreeSet::new(),
        }
    }
}

impl Default for MockDispatcher<NoMocks> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M> MockDispatcher<M> {
    pub fn with_cassette(mut self, cassette: Cassette) -> Self {
        self.cassette = Some(CassetteDispatcher::new(cassette));
        self
    }

    pub fn with_mocks<N>(self, mocks: Mocks<N>) -> MockDispatcher<N> {
        MockDispatcher {
            cassette: self.cassette,
            mocks: Some(mocks),
            statics: self.statics,
            static_tools: self.static_tools,
        }
    }

    /// Static responses in the `ptc.tools.json` format: a tool maps to its result, or to
    /// `{"$error": "message"}`.
    pub fn with_static_responses(
        mut self,
        responses: &JsonValue,
    ) -> Result<Self, StaticResponsesError> {
        self.statics = StaticDispatcher::from_json(responses)?;
        self.static_tools = responses
            .as_object()
            .map(|tools| tools.keys().cloned().collect())
            .unwrap_or_default();
        Ok(self)
    }

    /// The signals of the cassette, if any.
    pub fn recorded_signals(&self) -> &[ptc_core::RecordedSignal] {
        self.cassette
            .as_ref()
            .map_or(&[], |cassette| cassette.cassette().signals.as_slice())
    }
}

impl<M: ToolDispatcher> ToolDispatcher for MockDispatcher<M> {
    async fn dispatch(&self, call: ToolCall, credentials: Option<Credentials>) -> Dispatched {
        if let Some(outcome) = self
            .cassette
            .as_ref()
            .and_then(|cassette| cassette.answer(&call))
        {
            return dispatched(outcome);
        }
        if let Some(mocks) = &self.mocks
            && mocks.tools.contains(&call.tool.name)
        {
            return mocks.handlers.dispatch(call, credentials).await;
        }
        if self.static_tools.contains(&call.tool.name) {
            return self.statics.dispatch(call, credentials).await;
        }
        Dispatched::Failed(no_response(&call))
    }
}

// -------------------------------------------------------------------------------------------
// Scenarios

/// A scenario: what `ptc test` and `POST /v1/test` run.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub script: Script,
    pub toolset: Toolset,
    pub input: JsonValue,
    /// Signals sent to the run, in order. The cassette's signals, if any, are sent first, at the
    /// time they arrived in the recorded run.
    pub signals: Vec<ScenarioSignal>,
    pub expect: Expectations,
    /// When the run starts, in virtual time. 2026-01-01T00:00:00Z by default, so that a report
    /// is the same on every run.
    pub start: Timestamp,
    /// The run's TTL; the engine's default when `None`.
    pub ttl: Option<Duration>,
}

impl Scenario {
    /// 2026-01-01T00:00:00Z.
    pub const DEFAULT_START: Timestamp = Timestamp(1_767_225_600_000);

    pub fn new(script: Script, toolset: Toolset, input: JsonValue) -> Self {
        Self {
            script,
            toolset,
            input,
            signals: Vec::new(),
            expect: Expectations::default(),
            start: Self::DEFAULT_START,
            ttl: None,
        }
    }
}

/// A signal of a scenario.
#[derive(Debug, Clone, PartialEq)]
pub struct ScenarioSignal {
    pub name: String,
    pub payload: JsonValue,
    pub when: SignalWhen,
}

/// When a scenario sends a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalWhen {
    /// Once the run can advance no further without the outside world.
    Blocked,
    /// This much virtual time after the start.
    After(Duration),
}

/// What must be true at the end of a scenario. Everything not given is not checked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Expectations {
    pub status: Option<RunStatus>,
    /// The output contains these values: objects are compared key by key, recursively.
    pub output: Option<JsonValue>,
    pub error: Option<ExpectedError>,
    /// How many times each tool was called.
    pub calls: Option<BTreeMap<String, u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedError {
    pub name: String,
    /// The message contains this text.
    pub message: Option<String>,
}

/// The outcome of a scenario.
#[derive(Debug, Clone, PartialEq)]
pub struct TestReport {
    /// Every expectation holds.
    pub passed: bool,
    /// The expectations that do not hold, one sentence each, written for a model.
    pub failures: Vec<String>,
    pub status: RunStatus,
    pub output: Option<JsonValue>,
    pub error: Option<ScriptError>,
    pub abort: Option<AbortReason>,
    /// How many times each tool was called.
    pub calls: BTreeMap<String, u64>,
    /// The LLM trace of the run.
    pub trace: String,
    /// The run's history, every payload loaded.
    pub history: Vec<HistoryEntry>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ScenarioError {
    /// The script was refused at start: no run exists.
    #[error(transparent)]
    Start(#[from] StartError),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

/// Runs a scenario in virtual time, with in-memory adapters, the executor of the host and the
/// mocks of `dispatcher`, and reports what happened.
pub async fn run_scenario<X, M>(
    executor: &X,
    dispatcher: MockDispatcher<M>,
    scenario: Scenario,
) -> Result<TestReport, ScenarioError>
where
    X: ScriptExecutor,
    M: ToolDispatcher,
{
    let mut signals: Vec<ScenarioSignal> = dispatcher
        .recorded_signals()
        .iter()
        .map(|signal| ScenarioSignal {
            name: signal.name.clone(),
            payload: signal.payload.clone(),
            when: SignalWhen::After(Duration::from_millis(signal.elapsed_ms)),
        })
        .collect();
    signals.extend(scenario.signals);

    let engine = Engine::new(
        Adapters {
            journal: MemoryJournal::new(),
            blobs: MemoryBlobs::new(),
            executor,
            dispatcher,
            clock: VirtualClock::new(scenario.start),
            entropy: SeededEntropy::new(1),
        },
        EngineConfig::default(),
    );
    let mut request = StartRun::new(scenario.script, scenario.toolset, scenario.input);
    request.mode = RunMode::Mock;
    request.ttl = scenario.ttl;
    let run = engine.start_run(request).await?.run;

    let options = DriveOptions::default();
    let mut failures = Vec::new();
    let mut drive_failures = |report: crate::DriveReport| {
        for (run, error) in report.failures {
            failures.push(format!("an activation of run {} failed: {error}", run.id));
        }
        if report.interrupted {
            failures.push(
                "the run never became idle in virtual time: does it start timers forever?".into(),
            );
        }
    };
    // Each signal drives the run until its time; the last drive takes the run as far as it
    // goes.
    let mut undelivered = Vec::new();
    for signal in signals {
        match signal.when {
            SignalWhen::Blocked => drive_failures(engine.run_until_idle_virtual(&options).await?),
            SignalWhen::After(after) => {
                let at = scenario.start.saturating_add(after);
                drive_failures(engine.run_virtual_until(&options, at).await?);
            }
        }
        let enqueued = engine
            .signal(&run, signal.name.clone(), signal.payload, None)
            .await?;
        if enqueued == Enqueued::RunEnded {
            undelivered.push(signal.name);
        }
    }
    drive_failures(engine.run_until_idle_virtual(&options).await?);

    let history = engine.export_history(&run).await?;
    let state = RunState::from_history(&history).map_err(|error| EngineError::CorruptHistory {
        run_id: run.id,
        error,
    })?;
    let trace = engine.trace(&run).await?;
    let mut report = report(&run, &history, &state, trace);
    for name in undelivered {
        report.failures.push(format!(
            "the signal {name:?} was not delivered: the run had already ended"
        ));
    }
    report.failures.splice(0..0, failures);
    check(&scenario.expect, &mut report);
    report.passed = report.failures.is_empty();
    Ok(report)
}

fn report(_run: &RunRef, history: &[HistoryEntry], state: &RunState, trace: String) -> TestReport {
    let mut calls: BTreeMap<String, u64> = BTreeMap::new();
    for entry in history {
        if let Event::Command(Command::ScheduleToolCall { tool, .. }) = &entry.event {
            *calls.entry(tool.clone()).or_default() += 1;
        }
    }
    let (output, error, abort) = match state.outcome() {
        Some(RunOutcome::Completed { output }) => (output.value().cloned(), None, None),
        Some(RunOutcome::Failed { error }) => (None, Some(error.clone()), None),
        Some(RunOutcome::Aborted { reason }) => (None, None, Some(reason.clone())),
        None => (None, None, None),
    };
    TestReport {
        passed: false,
        failures: Vec::new(),
        status: state.status(),
        output,
        error,
        abort,
        calls,
        trace,
        history: history.to_vec(),
    }
}

/// Adds the expectations that do not hold to the report's failures.
fn check(expect: &Expectations, report: &mut TestReport) {
    let mut failures = Vec::new();
    if let Some(status) = expect.status
        && report.status != status
    {
        let detail = match (&report.error, &report.abort) {
            (Some(error), _) => format!(" ({}: {})", error.name, error.message),
            (_, Some(reason)) => format!(" ({reason})"),
            _ => String::new(),
        };
        failures.push(format!(
            "the run is {}{detail}, expected {status}",
            report.status
        ));
    }
    if let Some(expected) = &expect.output {
        match &report.output {
            Some(output) if contains(output, expected) => {}
            Some(output) => failures.push(format!(
                "the output is {output}, which does not contain {expected}"
            )),
            None => failures.push(format!(
                "the run has no output (it is {}), expected one containing {expected}",
                report.status
            )),
        }
    }
    if let Some(expected) = &expect.error {
        match &report.error {
            Some(error) if error.name != expected.name => failures.push(format!(
                "the run failed with {}, expected {}",
                error.name, expected.name
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
                report.status, expected.name
            )),
        }
    }
    if let Some(expected) = &expect.calls {
        let tools: BTreeSet<&String> = expected.keys().chain(report.calls.keys()).collect();
        for tool in tools {
            let actual = report.calls.get(tool).copied().unwrap_or(0);
            let wanted = expected.get(tool).copied().unwrap_or(0);
            if actual != wanted {
                failures.push(format!(
                    "{tool} was called {actual} times, expected {wanted}"
                ));
            }
        }
    }
    report.failures.extend(failures);
}

/// `actual` contains `expected`: equal, or for objects, every expected key is in `actual` with a
/// value that contains the expected one.
fn contains(actual: &JsonValue, expected: &JsonValue) -> bool {
    match (actual, expected) {
        (JsonValue::Object(actual), JsonValue::Object(expected)) => {
            expected.iter().all(|(key, value)| {
                actual
                    .get(key)
                    .is_some_and(|actual| contains(actual, value))
            })
        }
        _ => actual == expected,
    }
}

#[cfg(test)]
mod tests {
    use ptc_core::{RecordedSignal, Tool, ToolRoute};
    use serde_json::json;

    use super::*;
    use crate::ports::{RunRef, TaskToken, TenantId};

    fn call(seq: u32, tool: &str, args: JsonValue) -> ToolCall {
        ToolCall {
            run: RunRef::new(TenantId::local(), RunId::from_u128(1)),
            seq: Seq(seq),
            tool: Tool::new(tool, ToolRoute::Host { group: None }, json!({})),
            server: None,
            args,
            idempotency_key: format!("r/{seq}"),
            attempt: 1,
            token: TaskToken("t".into()),
            trace_context: None,
        }
    }

    fn cassette(matching: Matching) -> Cassette {
        Cassette {
            calls: vec![
                RecordedCall {
                    tool: "a".into(),
                    args: json!({ "n": 1 }),
                    outcome: ToolOutcome::Result(json!("first")),
                },
                RecordedCall {
                    tool: "a".into(),
                    args: json!({ "n": 2 }),
                    outcome: ToolOutcome::Result(json!("second")),
                },
            ],
            signals: vec![RecordedSignal {
                name: "go".into(),
                payload: json!(true),
                elapsed_ms: 10,
            }],
            matching,
        }
    }

    #[test]
    fn sequential_cassettes_answer_in_order() {
        let cassette = CassetteDispatcher::new(cassette(Matching::Sequential));
        // Out of order: the next recorded call is { n: 1 }.
        assert_eq!(cassette.answer(&call(1, "a", json!({ "n": 2 }))), None);
        assert_eq!(
            cassette.answer(&call(1, "a", json!({ "n": 1 }))),
            Some(ToolOutcome::Result(json!("first")))
        );
        // The same call again gets the same answer.
        assert_eq!(
            cassette.answer(&call(1, "a", json!({ "n": 1 }))),
            Some(ToolOutcome::Result(json!("first")))
        );
        assert_eq!(
            cassette.answer(&call(2, "a", json!({ "n": 2 }))),
            Some(ToolOutcome::Result(json!("second")))
        );
        assert_eq!(cassette.answer(&call(3, "a", json!({ "n": 1 }))), None);
    }

    #[test]
    fn by_args_cassettes_answer_in_any_order() {
        let cassette = CassetteDispatcher::new(cassette(Matching::ByArgs));
        assert_eq!(
            cassette.answer(&call(1, "a", json!({ "n": 2 }))),
            Some(ToolOutcome::Result(json!("second")))
        );
        assert_eq!(
            cassette.answer(&call(2, "a", json!({ "n": 1 }))),
            Some(ToolOutcome::Result(json!("first")))
        );
        // Each recorded call is handed out once.
        assert_eq!(cassette.answer(&call(3, "a", json!({ "n": 1 }))), None);
        assert_eq!(cassette.answer(&call(4, "b", json!({ "n": 1 }))), None);
    }

    #[tokio::test]
    async fn sources_are_tried_in_order() {
        let mocks = Mocks {
            handlers: StaticDispatcher::from_json(&json!({ "b": "from the mocks" })).unwrap(),
            tools: BTreeSet::from(["b".to_owned()]),
        };
        let dispatcher = MockDispatcher::new()
            .with_cassette(cassette(Matching::ByArgs))
            .with_static_responses(
                &json!({ "a": "static", "b": "static", "c": { "$error": "nope" } }),
            )
            .unwrap()
            .with_mocks(mocks);
        let answer = |call| dispatcher.dispatch(call, None);
        assert_eq!(
            answer(call(1, "a", json!({ "n": 1 }))).await,
            Dispatched::Result(json!("first"))
        );
        // Not in the cassette: the static response.
        assert_eq!(
            answer(call(2, "a", json!({ "n": 9 }))).await,
            Dispatched::Result(json!("static"))
        );
        // The mocks come before the static responses.
        assert_eq!(
            answer(call(3, "b", json!({}))).await,
            Dispatched::Result(json!("from the mocks"))
        );
        assert!(matches!(
            answer(call(4, "c", json!({}))).await,
            Dispatched::Failed(ToolFailure { message, .. }) if message == "nope"
        ));
        match answer(call(5, "d", json!({ "x": 1 }))).await {
            Dispatched::Failed(failure) => {
                assert_eq!(failure.message, "no recorded response for `d({\"x\":1})`");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(dispatcher.recorded_signals().len(), 1);
    }
}
