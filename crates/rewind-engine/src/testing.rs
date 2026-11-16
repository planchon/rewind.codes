//! Running tests (`archi/05-testing-and-observability.md`, "Testing and mocks").
//!
//! ```text
//! check_test ── stale? incompatible? ── no isolate started ──────────────► report (not run)
//! run_test ──── world → MockDispatcher ── Engine (in memory, virtual clock, seed) ──┐
//!               signals: the cassette's at their time, the test's in order          │
//!               history ──► Observed { status, output, error, calls } ──► expect ───┴► report
//! run_tests ─── every test of a suite, several at once, in the order given
//! ```
//!
//! A test runs in its own engine wired with in-memory adapters, a virtual clock started at the
//! test's start and an entropy giving the test's seed: its result is a function of the script,
//! the toolset, the test and the epoch. The script under test is a parameter ([`Subject`]).
//! TypeScript mock handlers come through a [`MockLoader`]: `rewind-runtime` runs them in an
//! isolate of their own.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream;
use rewind_core::{
    AbortReason, Command, Event, HistoryEntry, JsonValue, Observed, ObservedCall, RunId, RunMode,
    RunOutcome, RunState, RunStatus, Script, ScriptError, Seed, SignalWhen, Test, Timestamp,
    ToolOutcome, Toolset, WorkflowOp, WorkflowSchemas,
};

use crate::adapters::{
    MemoryBlobs, MemoryJournal, MemorySecrets, MemoryWebhooks, SeededEntropy, VirtualClock,
};
use crate::driver::DriveOptions;
use crate::engine::{Adapters, Engine, EngineConfig, EngineError, StartError, StartRun};
use crate::mock::{MockDispatcher, Mocks, NoMocks};
use crate::ports::{Enqueued, Entropy, ScriptExecutor, ToolDispatcher};
use crate::schema::{Refused, Validator};

/// When a test starts when it does not say: 2026-01-01T00:00:00Z, so that a report is the same
/// on every run.
pub const DEFAULT_START: Timestamp = Timestamp(1_767_225_600_000);

/// How many tests of a suite run at once.
pub const PARALLEL_TESTS: usize = 4;

/// What a test runs against: a script, its toolset, and for a test of a workflow, the
/// workflow's interface, against which the test's input and signals are checked.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    pub script: &'a Script,
    pub toolset: &'a Toolset,
    pub interface: Option<&'a WorkflowSchemas>,
}

/// How tests run, the same for every test of a run of [`run_tests`].
#[derive(Debug, Clone, Default)]
pub struct TestOptions {
    /// The run's TTL; the engine's default when `None`.
    pub ttl: Option<Duration>,
    /// Static responses in the `rewind.tools.json` format, tried after every source of the
    /// test's world (`rewind test`).
    pub statics: Option<JsonValue>,
}

/// Why a test did not pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestProblem {
    /// The test names tools, answers or args its toolset no longer has. It is not run.
    Stale,
    /// The workflow's interface refuses the test's input or signals. It is not run.
    Incompatible,
    /// It ran, and an expectation does not hold.
    Failed,
}

impl TestProblem {
    pub fn as_str(self) -> &'static str {
        match self {
            TestProblem::Stale => "stale",
            TestProblem::Incompatible => "incompatible",
            TestProblem::Failed => "failed",
        }
    }
}

/// The outcome of a test.
#[derive(Debug, Clone, PartialEq)]
pub struct TestReport {
    pub name: String,
    /// Every expectation holds, and nothing was refused.
    pub passed: bool,
    pub problem: Option<TestProblem>,
    /// What does not hold, one sentence each, written for a model.
    pub failures: Vec<String>,
    /// The run's status; `None` for a test that was not run.
    pub status: Option<RunStatus>,
    pub output: Option<JsonValue>,
    pub error: Option<ScriptError>,
    pub abort: Option<AbortReason>,
    /// How many times each tool was called.
    pub calls: BTreeMap<String, u64>,
    /// The LLM trace of the run; empty for a test that was not run.
    pub trace: String,
    /// The run's history, every payload loaded.
    pub history: Vec<HistoryEntry>,
}

impl TestReport {
    fn not_run(test: &Test, problem: TestProblem, failures: Vec<String>) -> Self {
        Self {
            name: test.name.clone(),
            passed: false,
            problem: Some(problem),
            failures,
            status: None,
            output: None,
            error: None,
            abort: None,
            calls: BTreeMap::new(),
            trace: String::new(),
            history: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TestError {
    /// The script was refused at start: no test can run.
    #[error(transparent)]
    Start(#[from] StartError),
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// The mock handlers could not be run, for a reason that is not the test's.
    #[error("the mock handlers could not run: {0}")]
    Mocks(String),
}

/// Why mock handlers cannot be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockLoadError {
    /// The test's module is at fault: the test fails with this message.
    Invalid(String),
    /// The platform is at fault.
    Internal(String),
}

/// Loads the TypeScript mock handlers of a test. `rewind-runtime` runs them in an isolate of
/// their own, with the sandbox's globals and the time of a clock started at `start`.
pub trait MockLoader: Sync {
    type Handlers: ToolDispatcher;

    fn load(
        &self,
        source: &str,
        start: Timestamp,
    ) -> impl Future<Output = Result<Mocks<Self::Handlers>, MockLoadError>> + Send;
}

/// Where mock handlers cannot run: a test that has some fails.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMockLoader;

impl MockLoader for NoMockLoader {
    type Handlers = NoMocks;

    async fn load(
        &self,
        _source: &str,
        _start: Timestamp,
    ) -> Result<Mocks<NoMocks>, MockLoadError> {
        Err(MockLoadError::Invalid(
            "TypeScript mock handlers cannot run here".into(),
        ))
    }
}

/// Checks a test against what it runs with, without starting an isolate: the tools it names,
/// the answers and args it gives them, and for a test of a workflow its input and signals. The
/// first problem found, with every reason for it.
pub fn check_test(test: &Test, subject: &Subject<'_>) -> Option<(TestProblem, Vec<String>)> {
    let stale = stale(test, subject.toolset);
    if !stale.is_empty() {
        return Some((TestProblem::Stale, stale));
    }
    let incompatible = subject
        .interface
        .map(|interface| incompatible(test, interface))
        .unwrap_or_default();
    if !incompatible.is_empty() {
        return Some((TestProblem::Incompatible, incompatible));
    }
    None
}

fn stale(test: &Test, toolset: &Toolset) -> Vec<String> {
    let mut reasons = Vec::new();
    let mut missing: Vec<&str> = test
        .tools()
        .filter(|tool| !toolset.contains(tool))
        .collect();
    missing.sort_unstable();
    missing.dedup();
    for tool in missing {
        reasons.push(format!("the toolset has no tool {tool}"));
    }
    for (name, rules) in &test.world.tools {
        let Some(tool) = toolset.tool(name) else {
            continue;
        };
        // The answer to a start of a workflow is the child's output.
        let output = match WorkflowOp::of_tool(name) {
            Some((workflow, WorkflowOp::Start)) => toolset
                .workflow(workflow)
                .map(|pinned| pinned.schemas.output.clone()),
            Some((_, WorkflowOp::Signal)) => None,
            None => tool.output_schema.clone(),
        };
        let output = output.map(|schema| Validator::compile(&schema));
        for (position, rule) in rules.iter().enumerate() {
            if let (Some(validator), ToolOutcome::Result(result)) = (&output, &rule.answer)
                && let Err(refused) = validator.check("result", result)
            {
                reasons.push(format!(
                    "rule {} of {name} answers what its output schema refuses: {}",
                    position + 1,
                    refused_text(refused)
                ));
            }
            if let Some(args) = &rule.args {
                for key in undeclared(args, &tool.input_schema) {
                    reasons.push(format!(
                        "rule {} of {name} matches the args key {key:?}, which its input schema \
                         does not declare",
                        position + 1
                    ));
                }
            }
        }
    }
    for call in &test.expect.calls {
        let (Some(tool), Some(args)) = (toolset.tool(&call.tool), &call.args) else {
            continue;
        };
        for key in undeclared(args, &tool.input_schema) {
            reasons.push(format!(
                "the test expects a call of {} with the args key {key:?}, which its input schema \
                 does not declare",
                call.tool
            ));
        }
    }
    reasons
}

/// The top-level keys of `args` an input schema that declares `properties` does not declare.
fn undeclared<'a>(args: &'a JsonValue, schema: &JsonValue) -> Vec<&'a str> {
    let (Some(args), Some(properties)) = (
        args.as_object(),
        schema.get("properties").and_then(JsonValue::as_object),
    ) else {
        return Vec::new();
    };
    args.keys()
        .filter(|key| !properties.contains_key(*key))
        .map(String::as_str)
        .collect()
}

fn incompatible(test: &Test, interface: &WorkflowSchemas) -> Vec<String> {
    let mut reasons = Vec::new();
    if let Err(refused) = Validator::compile(&interface.input).check("input", &test.input) {
        reasons.push(format!(
            "the workflow's input schema refuses the test's input: {}",
            refused_text(refused)
        ));
    }
    for signal in &test.signals {
        match interface.signals.get(&signal.name) {
            None => reasons.push(format!(
                "the test sends the signal {:?}, which the workflow does not declare",
                signal.name
            )),
            Some(schema) => {
                if let Err(refused) = Validator::compile(schema).check("payload", &signal.payload) {
                    reasons.push(format!(
                        "the workflow's schema of the signal {:?} refuses the test's payload: {}",
                        signal.name,
                        refused_text(refused)
                    ));
                }
            }
        }
    }
    reasons
}

fn refused_text(refused: Refused) -> String {
    match refused {
        Refused::Unusable(error) => format!("the schema cannot be used: {error}"),
        Refused::Invalid(problems) => problems,
    }
}

/// Runs every test against the subject, [`PARALLEL_TESTS`] at once, and reports them in the
/// order given. The script is compiled once first: a script refused at start fails them all.
pub async fn run_tests<X, L>(
    executor: &X,
    loader: &L,
    subject: &Subject<'_>,
    tests: &[Test],
    options: &TestOptions,
) -> Result<Vec<TestReport>, TestError>
where
    X: ScriptExecutor,
    L: MockLoader,
{
    if tests.is_empty() {
        return Ok(Vec::new());
    }
    executor
        .compile(subject.script, subject.toolset)
        .map_err(StartError::InvalidScript)?;
    // The futures are collected before they are streamed: a closure kept in the stream's state
    // would make the future's `Send` depend on a higher-ranked lifetime the compiler cannot
    // prove.
    let mut runs = Vec::with_capacity(tests.len());
    for test in tests {
        runs.push(run_test(executor, loader, subject, test, options));
    }
    stream::iter(runs)
        .buffered(PARALLEL_TESTS)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect()
}

/// Runs one test: checks it, then runs the subject in mock mode and virtual time against the
/// test's world, and checks its expectations against what left the run.
pub async fn run_test<X, L>(
    executor: &X,
    loader: &L,
    subject: &Subject<'_>,
    test: &Test,
    options: &TestOptions,
) -> Result<TestReport, TestError>
where
    X: ScriptExecutor,
    L: MockLoader,
{
    if let Some((problem, failures)) = check_test(test, subject) {
        return Ok(TestReport::not_run(test, problem, failures));
    }
    let start = test.start.unwrap_or(DEFAULT_START);
    let mut dispatcher =
        MockDispatcher::<L::Handlers>::without_mocks().with_rules(test.world.tools.clone());
    if let Some(cassette) = &test.world.cassette {
        dispatcher = dispatcher.with_cassette(cassette.clone());
    }
    if let Some(statics) = &options.statics {
        dispatcher = match dispatcher.with_static_responses(statics) {
            Ok(dispatcher) => dispatcher,
            Err(error) => {
                return Ok(TestReport::not_run(
                    test,
                    TestProblem::Failed,
                    vec![format!("the static responses cannot be used: {error}")],
                ));
            }
        };
    }
    if let Some(source) = &test.world.mocks {
        match loader.load(source, start).await {
            Ok(mocks) => dispatcher = dispatcher.with_mocks(mocks),
            Err(MockLoadError::Invalid(message)) => {
                return Ok(TestReport::not_run(
                    test,
                    TestProblem::Failed,
                    vec![format!("the mock handlers cannot load: {message}")],
                ));
            }
            Err(MockLoadError::Internal(message)) => return Err(TestError::Mocks(message)),
        }
    }
    let unanswered = dispatcher.unanswered();
    let mut signals: Vec<(String, JsonValue, SignalWhen)> = dispatcher
        .recorded_signals()
        .iter()
        .map(|signal| {
            (
                signal.name.clone(),
                signal.payload.clone(),
                SignalWhen::After(Duration::from_millis(signal.elapsed_ms)),
            )
        })
        .collect();
    signals.extend(
        test.signals
            .iter()
            .map(|signal| (signal.name.clone(), signal.payload.clone(), signal.when)),
    );

    let engine = Engine::new(
        Adapters {
            journal: MemoryJournal::new(),
            blobs: MemoryBlobs::new(),
            executor,
            dispatcher,
            clock: VirtualClock::new(start),
            entropy: TestEntropy {
                inner: SeededEntropy::new(1),
                seed: test.seed,
            },
            secrets: MemorySecrets::new(),
            webhooks: MemoryWebhooks::new(),
        },
        EngineConfig::default(),
    );
    let mut request = StartRun::new(
        subject.script.clone(),
        subject.toolset.clone(),
        test.input.clone(),
    );
    request.mode = RunMode::Mock;
    request.ttl = options.ttl;
    let run = engine.start_run(request).await?.run;

    let drive = DriveOptions::default();
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
    for (name, payload, when) in signals {
        match when {
            SignalWhen::Blocked => drive_failures(engine.run_until_idle_virtual(&drive).await?),
            SignalWhen::After(after) => {
                drive_failures(
                    engine
                        .run_virtual_until(&drive, start.saturating_add(after))
                        .await?,
                );
            }
        }
        if engine.signal(&run, name.clone(), payload, None).await? == Enqueued::RunEnded {
            undelivered.push(name);
        }
    }
    drive_failures(engine.run_until_idle_virtual(&drive).await?);

    let history = engine.export_history(&run).await?;
    let state = RunState::from_history(&history).map_err(|error| EngineError::CorruptHistory {
        run_id: run.id,
        error,
    })?;
    let trace = engine.trace(&run).await?;
    for name in undelivered {
        failures.push(format!(
            "the signal {name:?} was not delivered: the run had already ended"
        ));
    }
    for call in unanswered.list() {
        failures.push(format!(
            "the script called {call}, which the test's world does not answer"
        ));
    }

    let calls: Vec<ObservedCall> = history
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Command(Command::ScheduleToolCall { tool, args, .. }) => Some(ObservedCall {
                tool: tool.clone(),
                args: args.value().cloned().unwrap_or(JsonValue::Null),
            }),
            _ => None,
        })
        .collect();
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for call in &calls {
        *counts.entry(call.tool.clone()).or_default() += 1;
    }
    let (output, error, abort) = match state.outcome() {
        Some(RunOutcome::Completed { output }) => (output.value().cloned(), None, None),
        Some(RunOutcome::Failed { error }) => (None, Some(error.clone()), None),
        Some(RunOutcome::Aborted { reason }) => (None, None, Some(reason.clone())),
        None => (None, None, None),
    };
    if let (Some(interface), Some(output)) = (subject.interface, &output)
        && let Err(refused) = Validator::compile(&interface.output).check("output", output)
    {
        failures.push(format!(
            "the workflow's output schema refuses the run's output: {}",
            refused_text(refused)
        ));
    }
    failures.extend(test.expect.check(&Observed {
        status: state.status(),
        output: output.as_ref(),
        error: error.as_ref(),
        abort: abort.as_ref(),
        calls: &calls,
    }));
    Ok(TestReport {
        name: test.name.clone(),
        passed: failures.is_empty(),
        problem: (!failures.is_empty()).then_some(TestProblem::Failed),
        failures,
        status: Some(state.status()),
        output,
        error,
        abort,
        calls: counts,
        trace,
        history,
    })
}

/// The entropy of a test: run ids from a seeded generator, and the test's seed for the run.
struct TestEntropy {
    inner: SeededEntropy,
    seed: Option<Seed>,
}

impl Entropy for TestEntropy {
    fn run_id(&self) -> RunId {
        self.inner.run_id()
    }

    fn seed(&self) -> Seed {
        self.seed.unwrap_or_else(|| self.inner.seed())
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::{ExpectedCall, Tool, ToolRoute, ToolRule};
    use serde_json::json;

    use super::*;

    fn toolset() -> Toolset {
        let mut list = Tool::new(
            "github.list_issues",
            ToolRoute::Host {
                group: Some("github".into()),
            },
            json!({ "type": "object", "properties": { "state": { "type": "string" } } }),
        );
        list.output_schema = Some(json!({ "type": "array" }));
        Toolset::new(Vec::new(), vec![list]).unwrap()
    }

    #[test]
    fn stale_tests_name_what_the_toolset_lost() {
        let toolset = toolset();
        let script = Script::new("export default async () => 1;");
        let subject = Subject {
            script: &script,
            toolset: &toolset,
            interface: None,
        };
        let mut test = Test::new("lists");
        test.world.tools.insert(
            "github.list_issues".into(),
            vec![ToolRule {
                args: Some(json!({ "status": "open" })),
                answer: ToolOutcome::Result(json!({ "issues": [] })),
                times: None,
            }],
        );
        test.expect.calls.push(ExpectedCall {
            tool: "github.close_issue".into(),
            args: None,
            times: Some(0),
        });
        let (problem, reasons) = check_test(&test, &subject).unwrap();
        assert_eq!(problem, TestProblem::Stale);
        assert_eq!(
            reasons,
            vec![
                "the toolset has no tool github.close_issue",
                "rule 1 of github.list_issues answers what its output schema refuses: result: \
                 {\"issues\":[]} is not of type \"array\"",
                "rule 1 of github.list_issues matches the args key \"status\", which its input \
                 schema does not declare",
            ]
        );
    }

    #[test]
    fn incompatible_tests_name_what_the_interface_refuses() {
        let toolset = Toolset::empty();
        let script = Script::new("export default async () => 1;");
        let interface = WorkflowSchemas {
            input: json!({ "type": "object", "required": ["repo"] }),
            output: json!({}),
            signals: BTreeMap::from([(
                "approval".to_owned(),
                json!({ "type": "object", "required": ["close"] }),
            )]),
        };
        let subject = Subject {
            script: &script,
            toolset: &toolset,
            interface: Some(&interface),
        };
        let mut test = Test::new("approves");
        test.signals.push(rewind_core::TestSignal {
            name: "approval".into(),
            payload: json!({}),
            when: SignalWhen::Blocked,
        });
        test.signals.push(rewind_core::TestSignal {
            name: "merged".into(),
            payload: JsonValue::Null,
            when: SignalWhen::Blocked,
        });
        let (problem, reasons) = check_test(&test, &subject).unwrap();
        assert_eq!(problem, TestProblem::Incompatible);
        assert_eq!(reasons.len(), 3, "{reasons:?}");
        assert!(reasons[0].starts_with("the workflow's input schema refuses the test's input"));
        assert!(reasons[1].starts_with("the workflow's schema of the signal \"approval\""));
        assert_eq!(
            reasons[2],
            "the test sends the signal \"merged\", which the workflow does not declare"
        );
    }
}
