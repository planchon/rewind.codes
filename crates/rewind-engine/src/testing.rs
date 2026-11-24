//! Running tests (`archi/05-testing-and-observability.md`, "Test runs").
//!
//! ```text
//! check_test ── stale? incompatible? ── no isolate started ──────────────► report (not run)
//! run_test ──── world → MockDispatcher ── Driver: one node, one partition ───────────┐
//!               (memory adapters, virtual clock at the test's start, its seed)       │
//!               signals: the cassette's at their time, the test's in order          │
//!               history ──► Observed { status, output, error, calls } ──► expect ───┴► report
//! run_tests ─── every test of a suite, several at once, in the order given
//! ```
//!
//! A test runs on a run plane of its own, stepped by the [`Driver`]: one node holding one
//! partition, wired with in-memory adapters, a virtual clock started at the test's start and an
//! entropy giving the test's seed; harness actions are dispatched to the test's world as every
//! other call. Its result is a function of the code, the action set, the test and the epoch. The
//! code under test is a parameter ([`Subject`]).
//! TypeScript handlers come through a [`MockLoader`]: `rewind-runtime` runs them in an
//! isolate of their own.

use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream;
use rewind_core::{
    AbortReason, ActionOutcome, ActionSet, Code, Command, Event, HistoryEvent, JsonValue, Observed,
    ObservedCall, PartitionId, RunId, RunMode, RunOutcome, RunState, RunStatus, ScriptError, Seed,
    SignalWhen, Test, Timestamp, WorkflowOp, WorkflowSchemas,
};

use crate::adapters::{
    MemoryArchive, MemoryBlobs, MemoryCatalog, MemoryCluster, MemoryPartitions, MemorySecrets,
    MemorySpanExporter, MemoryWebhooks, SeededEntropy, VirtualClock,
};
use crate::driver::{DriveError, Driver, PlaneClock, Steps};
use crate::edge::Edge;
use crate::plane::{HarnessActions, NodeConfig, Wiring};
use crate::ports::{ActionDispatcher, CodeExecutor, Enqueued, Entropy, NodeId, QuarantineKind};
use crate::schema::{Refused, Validator};
use crate::start::{StartError, StartRun};
use crate::world::{Handlers, NoHandlers, WorldDispatcher};

/// When a test starts when it does not say: 2026-01-01T00:00:00Z, so that a report is the same
/// on every run.
pub const DEFAULT_START: Timestamp = Timestamp(1_767_225_600_000);

/// How many tests of a suite run at once.
pub const PARALLEL_TESTS: usize = 4;

/// What a test runs against: a code, its action set, and for a test of a workflow, the
/// workflow's interface, against which the test's input and signals are checked.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    pub code: &'a Code,
    pub action_set: &'a ActionSet,
    pub interface: Option<&'a WorkflowSchemas>,
}

/// How tests run, the same for every test of a run of [`run_tests`].
#[derive(Debug, Clone, Default)]
pub struct TestOptions {
    /// The run's TTL; the engine's default when `None`.
    pub ttl: Option<Duration>,
    /// Static responses in the `rewind.actions.json` format, tried after every source of the
    /// test's world (`rewind test`).
    pub statics: Option<JsonValue>,
}

/// Why a test did not pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestProblem {
    /// The test names actions, answers or args its action set no longer has. It is not run.
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
    /// How many times each action was called.
    pub calls: BTreeMap<String, u64>,
    /// The run's transcript; empty for a test that was not run.
    pub transcript: String,
    /// The run's history, every payload loaded.
    pub history: Vec<HistoryEvent>,
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
            transcript: String::new(),
            history: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TestError {
    /// The code was refused at start: no test can run.
    #[error(transparent)]
    Start(#[from] StartError),
    /// The run plane a test runs on failed, for a reason that is not the test's.
    #[error("the test could not run: {0}")]
    Unavailable(String),
    /// The handlers could not be run, for a reason that is not the test's.
    #[error("the handlers could not run: {0}")]
    Handlers(String),
}

/// Why handlers cannot be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerLoadError {
    /// The test's module is at fault: the test fails with this message.
    Invalid(String),
    /// The platform is at fault.
    Internal(String),
}

/// Loads the TypeScript handlers of a test. `rewind-runtime` runs them in an isolate of
/// their own, with the sandbox's globals and the time of a clock started at `start`.
pub trait HandlerLoader: Sync {
    type Handlers: ActionDispatcher + 'static;

    fn load(
        &self,
        source: &str,
        start: Timestamp,
    ) -> impl Future<Output = Result<Handlers<Self::Handlers>, HandlerLoadError>> + Send;
}

/// Where handlers cannot run: a test that has some fails.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHandlerLoader;

impl HandlerLoader for NoHandlerLoader {
    type Handlers = NoHandlers;

    async fn load(
        &self,
        _source: &str,
        _start: Timestamp,
    ) -> Result<Handlers<NoHandlers>, HandlerLoadError> {
        Err(HandlerLoadError::Invalid(
            "TypeScript handlers cannot run here".into(),
        ))
    }
}

/// Checks a test against what it runs with, without starting an isolate: the actions it names,
/// the answers and args it gives them, and for a test of a workflow its input and signals. The
/// first problem found, with every reason for it.
pub fn check_test(test: &Test, subject: &Subject<'_>) -> Option<(TestProblem, Vec<String>)> {
    let stale = stale(test, subject.action_set);
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

fn stale(test: &Test, action_set: &ActionSet) -> Vec<String> {
    let mut reasons = Vec::new();
    let mut missing: Vec<&str> = test
        .actions()
        .filter(|action| !action_set.contains(action))
        .collect();
    missing.sort_unstable();
    missing.dedup();
    for action in missing {
        reasons.push(format!("the action set has no action {action}"));
    }
    for (name, rules) in &test.world.actions {
        let Some(action) = action_set.action(name) else {
            continue;
        };
        // The answer to a start of a workflow is the child's output.
        let output = match WorkflowOp::of_action(name) {
            Some((workflow, WorkflowOp::Start)) => action_set
                .workflow(workflow)
                .map(|pinned| pinned.schemas.output.clone()),
            Some((_, WorkflowOp::Signal)) => None,
            None => action.output_schema.clone(),
        };
        let output = output.map(|schema| Validator::compile(&schema));
        for (position, rule) in rules.iter().enumerate() {
            if let (Some(validator), ActionOutcome::Result(result)) = (&output, &rule.answer)
                && let Err(refused) = validator.check("result", result)
            {
                reasons.push(format!(
                    "rule {} of {name} answers what its output schema refuses: {}",
                    position + 1,
                    refused_text(refused)
                ));
            }
            if let Some(args) = &rule.args {
                for key in undeclared(args, &action.input_schema) {
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
        let (Some(action), Some(args)) = (action_set.action(&call.action), &call.args) else {
            continue;
        };
        for key in undeclared(args, &action.input_schema) {
            reasons.push(format!(
                "the test expects a call of {} with the args key {key:?}, which its input schema \
                 does not declare",
                call.action
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
/// order given. The code is compiled once first: a code refused at start fails them all.
/// Each test runs on a clone of `executor`.
pub async fn run_tests<X, L>(
    executor: &X,
    loader: &L,
    subject: &Subject<'_>,
    tests: &[Test],
    options: &TestOptions,
) -> Result<Vec<TestReport>, TestError>
where
    X: CodeExecutor<Instance: 'static> + Clone + 'static,
    L: HandlerLoader,
{
    if tests.is_empty() {
        return Ok(Vec::new());
    }
    executor
        .compile(subject.code, subject.action_set)
        .map_err(StartError::InvalidCode)?;
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

/// Runs one test: checks it, then runs the subject in a test run and virtual time against the
/// test's world, and checks its expectations against what left the run.
pub async fn run_test<X, L>(
    executor: &X,
    loader: &L,
    subject: &Subject<'_>,
    test: &Test,
    options: &TestOptions,
) -> Result<TestReport, TestError>
where
    X: CodeExecutor<Instance: 'static> + Clone + 'static,
    L: HandlerLoader,
{
    if let Some((problem, failures)) = check_test(test, subject) {
        return Ok(TestReport::not_run(test, problem, failures));
    }
    let start = test.start.unwrap_or(DEFAULT_START);
    let mut dispatcher =
        WorldDispatcher::<L::Handlers>::without_handlers().with_rules(test.world.actions.clone());
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
    if let Some(source) = &test.world.handlers {
        match loader.load(source, start).await {
            Ok(handlers) => dispatcher = dispatcher.with_handlers(handlers),
            Err(HandlerLoadError::Invalid(message)) => {
                return Ok(TestReport::not_run(
                    test,
                    TestProblem::Failed,
                    vec![format!("the handlers cannot load: {message}")],
                ));
            }
            Err(HandlerLoadError::Internal(message)) => return Err(TestError::Handlers(message)),
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

    let mut world = TestWorld::new(executor.clone(), dispatcher, start, test.seed).await?;
    let mut request = StartRun::new(
        subject.code.clone(),
        subject.action_set.clone(),
        test.input.clone(),
    );
    request.mode = RunMode::Test;
    request.ttl = options.ttl;
    let run = {
        let mut gateway = world.driver.gateway(&world.node);
        world.edge.start_run(&mut gateway, request).await?.run
    };

    let mut failures = Vec::new();
    // Each signal drives the run until its time; the last drive takes the run as far as it
    // goes. A run that never becomes idle is driven no further.
    let mut undelivered = Vec::new();
    let mut idle = true;
    for (name, payload, when) in signals {
        let driven = match when {
            SignalWhen::Blocked => world.driver.run_until_idle_virtual().await,
            SignalWhen::After(after) => world.driver.run_until(start.saturating_add(after)).await,
        };
        idle = drove(driven, &mut failures)?;
        if !idle {
            break;
        }
        let mut gateway = world.driver.gateway(&world.node);
        let enqueued = world
            .edge
            .signal(&mut gateway, &run, name.clone(), payload, None)
            .await
            .map_err(unavailable)?;
        if enqueued == Enqueued::RunEnded {
            undelivered.push(name);
        }
    }
    if idle {
        drove(world.driver.run_until_idle_virtual().await, &mut failures)?;
    }

    let mut gateway = world.driver.gateway(&world.node);
    let view = world
        .edge
        .get_run(&mut gateway, &run)
        .await
        .map_err(unavailable)?
        .ok_or_else(|| TestError::Unavailable(format!("run {} is not found", run.id)))?;
    let history = world
        .edge
        .export_run(&mut gateway, &run)
        .await
        .map_err(unavailable)?
        .events;
    let transcript = world
        .edge
        .transcript(&mut gateway, &run)
        .await
        .map_err(unavailable)?;
    let state = RunState::from_history(&history).map_err(|error| {
        TestError::Unavailable(format!("the history of run {} is invalid: {error}", run.id))
    })?;
    if let Some(quarantine) = &view.head.quarantine {
        let kind = match quarantine.kind {
            QuarantineKind::NonDeterminism => "non_determinism",
            QuarantineKind::Failures => "failures",
        };
        failures.push(format!(
            "the run is quarantined ({kind}): {}",
            quarantine.reason
        ));
    }
    for name in undelivered {
        failures.push(format!(
            "the signal {name:?} was not delivered: the run had already ended"
        ));
    }
    for call in unanswered.list() {
        failures.push(format!(
            "the code called {call}, which the test's world does not answer"
        ));
    }

    let calls: Vec<ObservedCall> = history
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Command(Command::ScheduleCall { action, args, .. }) => Some(ObservedCall {
                action: action.clone(),
                args: args.value().cloned().unwrap_or(JsonValue::Null),
            }),
            _ => None,
        })
        .collect();
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for call in &calls {
        *counts.entry(call.action.clone()).or_default() += 1;
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
        transcript,
        history,
    })
}

/// Steps a test's run is driven at most: a guard against workflows that never stop (an endless
/// `setInterval` in virtual time).
pub const MAX_TEST_STEPS: usize = 100_000;

/// The plane a test runs on: the memory adapters, the subject's executor, the test's world as
/// the dispatcher, a virtual clock and the test's entropy.
type TestPlane<X, M> = Wiring<
    MemoryCatalog,
    MemoryPartitions,
    MemoryArchive,
    MemoryBlobs,
    X,
    WorldDispatcher<M>,
    MemorySecrets,
    MemorySpanExporter,
    MemoryWebhooks,
    VirtualClock,
    TestEntropy,
>;

/// One node holding the one partition of a test, stepped by the driver.
struct TestWorld<X, M>
where
    X: CodeExecutor<Instance: 'static> + 'static,
    M: ActionDispatcher + 'static,
{
    driver: Driver<TestPlane<X, M>, MemoryCluster>,
    edge: Edge<TestPlane<X, M>>,
    node: NodeId,
}

impl<X, M> TestWorld<X, M>
where
    X: CodeExecutor<Instance: 'static> + 'static,
    M: ActionDispatcher + 'static,
{
    async fn new(
        executor: X,
        dispatcher: WorldDispatcher<M>,
        start: Timestamp,
        seed: Option<Seed>,
    ) -> Result<Self, TestError> {
        let blobs = MemoryBlobs::new();
        let plane = Arc::new(Wiring {
            catalog: MemoryCatalog::new(),
            partitions: MemoryPartitions::new(),
            archive: MemoryArchive::with_blobs(blobs.clone()),
            blobs,
            executor,
            dispatcher,
            secrets: MemorySecrets::new(),
            spans: MemorySpanExporter::new(),
            webhooks: MemoryWebhooks::new(),
            clock: VirtualClock::new(start),
            entropy: TestEntropy {
                inner: SeededEntropy::new(1),
                seed,
            },
        });
        let node = NodeId::new("test").map_err(unavailable)?;
        let mut config = NodeConfig::new(node.clone(), NonZeroU16::MIN);
        config.harness_actions = HarnessActions::Dispatched;
        // Every timer of the run is held by the wheel: none is read again.
        config.horizon = Duration::from_secs(100 * 365 * 86_400);
        let mut driver = Driver::new(
            MemoryCluster::new(1),
            Arc::new(PlaneClock(Arc::clone(&plane))),
        );
        driver.max_steps = MAX_TEST_STEPS;
        driver
            .add_node(Arc::clone(&plane), config.clone())
            .await
            .map_err(unavailable)?;
        driver.run_until_idle().await.map_err(unavailable)?;
        Ok(Self {
            driver,
            edge: Edge::new(plane, config),
            node,
        })
    }
}

/// Whether a drive left the run idle; a run that never becomes idle is a failure of the test.
fn drove(driven: Result<Steps, DriveError>, failures: &mut Vec<String>) -> Result<bool, TestError> {
    match driven {
        Ok(_) => Ok(true),
        Err(DriveError::NeverIdle(_)) => {
            failures.push(
                "the run never became idle in virtual time: does it start timers forever?".into(),
            );
            Ok(false)
        }
        Err(error) => Err(unavailable(error)),
    }
}

fn unavailable(error: impl std::fmt::Display) -> TestError {
    TestError::Unavailable(error.to_string())
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

    fn run_id_in(&self, partition: PartitionId) -> RunId {
        self.inner.run_id_in(partition)
    }

    fn seed(&self) -> Seed {
        self.seed.unwrap_or_else(|| self.inner.seed())
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::{Action, ActionRoute, ActionRule, ExpectedCall};
    use serde_json::json;

    use super::*;

    fn action_set() -> ActionSet {
        let mut list = Action::new(
            "github.list_issues",
            ActionRoute::Harness {
                group: Some("github".into()),
            },
            json!({ "type": "object", "properties": { "state": { "type": "string" } } }),
        );
        list.output_schema = Some(json!({ "type": "array" }));
        ActionSet::new(Vec::new(), vec![list]).unwrap()
    }

    #[test]
    fn stale_tests_name_what_the_action_set_lost() {
        let action_set = action_set();
        let code = Code::new("export default async () => 1;");
        let subject = Subject {
            code: &code,
            action_set: &action_set,
            interface: None,
        };
        let mut test = Test::new("lists");
        test.world.actions.insert(
            "github.list_issues".into(),
            vec![ActionRule {
                args: Some(json!({ "status": "open" })),
                answer: ActionOutcome::Result(json!({ "issues": [] })),
                times: None,
            }],
        );
        test.expect.calls.push(ExpectedCall {
            action: "github.close_issue".into(),
            args: None,
            times: Some(0),
        });
        let (problem, reasons) = check_test(&test, &subject).unwrap();
        assert_eq!(problem, TestProblem::Stale);
        assert_eq!(
            reasons,
            vec![
                "the action set has no action github.close_issue",
                "rule 1 of github.list_issues answers what its output schema refuses: result: \
                 {\"issues\":[]} is not of type \"array\"",
                "rule 1 of github.list_issues matches the args key \"status\", which its input \
                 schema does not declare",
            ]
        );
    }

    #[test]
    fn incompatible_tests_name_what_the_interface_refuses() {
        let action_set = ActionSet::empty();
        let code = Code::new("export default async () => 1;");
        let interface = WorkflowSchemas {
            input: json!({ "type": "object", "required": ["repo"] }),
            output: json!({}),
            signals: BTreeMap::from([(
                "approval".to_owned(),
                json!({ "type": "object", "required": ["close"] }),
            )]),
        };
        let subject = Subject {
            code: &code,
            action_set: &action_set,
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
