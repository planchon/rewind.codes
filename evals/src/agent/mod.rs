//! The agent tools, as the SDK's `AgentTools` handles them
//! (`sdk/typescript/src/agent-tools/agent-tools.ts`), over the world of an episode instead of
//! the HTTP API: the same inputs, the same checks of labels, the same bounded plain-text
//! results, with what the server would do done by the engine in process.
//!
//! A mistake the model can fix (a bad argument, a refused script, a run that has ended) is a
//! result with `is_error`; a fault of the platform is an [`AgentError`], which ends the episode
//! as an error of the harness, not a failure of the model.

pub mod definitions;
pub mod render;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use ptc_api::common::{RunStatus, ScriptError};
use ptc_api::scripts::{CheckResult, Diagnostic, TestResult};
use ptc_api::toolsets::ToolMatch;
use ptc_core::{Matching, RunId, Script, Timestamp, parse_duration};
use ptc_engine::mock::{
    Expectations, ExpectedError, MockDispatcher, Mocks, Scenario, ScenarioError, ScenarioSignal,
    SignalWhen,
};
use ptc_engine::ports::{Enqueued, RunFilter, RunRef, TenantId, TypeChecker};
use ptc_engine::{EngineError, ForkError, ForkRun, StartError};
use ptc_runtime::{SandboxLimits, TsMocks};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::clock::SharedClock;
use crate::view;
use crate::world::World;
use render::{
    Limits, RunExtras, args, describe_check, describe_matches, describe_page, describe_refusal,
    describe_run, describe_runs, describe_test, entry_text, json,
};

/// What a tool call returns to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentToolResult {
    /// The text for the model, bounded.
    pub content: String,
    /// The call failed, or what it ran failed: the model should look at it.
    pub is_error: bool,
    /// What the script handed in, if the call carried one: for the first-try check rate.
    pub script: Option<ScriptVerdict>,
}

/// Whether a script the model submitted passed the checks before anything ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptVerdict {
    Passed,
    Refused,
}

/// A fault of the platform or of the harness: the model cannot fix it.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("{0} is not one of the agent tools")]
    UnknownTool(String),
    #[error("the type-checker failed: {0}")]
    Checker(String),
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    World(#[from] crate::world::WorldError),
}

/// What a handler fails with.
#[derive(Debug)]
enum Failure {
    /// A bad argument: `{name}: {message}`.
    Input(String),
    /// What the API answers with a 4xx: `{name} failed: {detail}`.
    Problem(String),
    /// `invalid-script`: the script was refused before any run exists.
    Refused {
        detail: String,
        diagnostics: Vec<Diagnostic>,
    },
    Platform(AgentError),
}

impl<E: Into<AgentError>> From<E> for Failure {
    fn from(error: E) -> Self {
        Failure::Platform(error.into())
    }
}

type Handled = Result<AgentToolResult, Failure>;

fn result(content: String, is_error: bool) -> AgentToolResult {
    AgentToolResult {
        content,
        is_error,
        script: None,
    }
}

/// How the agent tools behave: the SDK's options.
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// How long `run_script`, `retry_run` and `cancel_run` wait for the run to end: 30 s.
    pub wait: Duration,
    /// The longest wait the model may ask for: 2 minutes.
    pub max_wait: Duration,
    /// The most of a run's output one answer holds: 256 KiB.
    pub output_limit: usize,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            wait: Duration::from_secs(30),
            max_wait: Duration::from_secs(120),
            output_limit: render::OUTPUT_LIMIT,
        }
    }
}

/// The agent tools of an episode.
#[derive(Debug)]
pub struct AgentTools<'w> {
    world: &'w World,
    options: AgentOptions,
    limits: Limits,
}

impl<'w> AgentTools<'w> {
    pub fn new(world: &'w World, options: AgentOptions) -> Self {
        let limits = Limits::for_output(options.output_limit);
        Self {
            world,
            options,
            limits,
        }
    }

    /// Executes a tool call of the model. `input` is the call's arguments, an object or its
    /// JSON text; `call_id` is the model's tool call id, the idempotency key of what it starts.
    pub async fn handle(
        &self,
        name: &str,
        input: &Value,
        call_id: &str,
    ) -> Result<AgentToolResult, AgentError> {
        if definitions::definition(name).is_none() {
            return Err(AgentError::UnknownTool(name.to_owned()));
        }
        let handled = match parse_input(name, input) {
            Ok(values) => match name {
                "run_script" => self.run_script(&values, call_id).await,
                "check_script" => self.check_script(&values).await,
                "test_script" => self.test_script(&values).await,
                "get_run" => self.get_run(&values).await,
                "list_runs" => self.list_runs(&values).await,
                "signal_run" => self.signal_run(&values, call_id).await,
                "cancel_run" => self.cancel_run(&values).await,
                "retry_run" => self.retry_run(&values, call_id).await,
                _ => self.search_tools(&values),
            },
            Err(failure) => Err(failure),
        };
        let carries_script = matches!(
            name,
            "run_script" | "check_script" | "test_script" | "retry_run"
        );
        match handled {
            Ok(mut handled) => {
                if carries_script && handled.script.is_none() {
                    handled.script = Some(ScriptVerdict::Passed);
                }
                Ok(handled)
            }
            Err(Failure::Input(message)) => Ok(result(format!("{name}: {message}"), true)),
            Err(Failure::Problem(detail)) => Ok(result(format!("{name} failed: {detail}"), true)),
            Err(Failure::Refused {
                detail,
                diagnostics,
            }) => Ok(AgentToolResult {
                content: describe_refusal(&detail, &diagnostics),
                is_error: true,
                script: Some(ScriptVerdict::Refused),
            }),
            Err(Failure::Platform(error)) => Err(error),
        }
    }

    // ---------------------------------------------------------------------------------------
    // The tools

    async fn run_script(&self, input: &Map<String, Value>, call_id: &str) -> Handled {
        let code = require_string(input, "code", "code")?;
        let wait = self.wait_of(input.get("wait"))?;
        let mut request = self.world.start_request(
            Script::new(code),
            input.get("input").cloned().unwrap_or_else(|| json!({})),
        );
        request.idempotency_key = Some(idempotency_key(call_id));
        let started = self
            .world
            .engine
            .start_run_checked(request, &*self.world.checker)
            .await;
        let created = match started {
            Ok(created) => created,
            Err(error) => return Err(start_failure(error)),
        };
        let run = created.run;
        self.world
            .drive(Some(&run), Some(self.deadline(wait)))
            .await?;
        let view = view::run(self.world, &run).await?;
        let failed = view.status == RunStatus::Failed;
        self.report(&run, view, true, failed).await
    }

    async fn check_script(&self, input: &Map<String, Value>) -> Handled {
        let code = require_string(input, "code", "code")?;
        let script = Script::new(code);
        let checked = self.check(&script).await?;
        let verdict = if checked.ok {
            ScriptVerdict::Passed
        } else {
            ScriptVerdict::Refused
        };
        Ok(AgentToolResult {
            content: describe_check(&checked),
            is_error: !checked.ok,
            script: Some(verdict),
        })
    }

    async fn test_script(&self, input: &Map<String, Value>) -> Handled {
        let code = require_string(input, "code", "code")?;
        let Some(Value::Object(scenario)) = input.get("scenario") else {
            return Err(Failure::Input("`scenario` must be an object.".into()));
        };
        let script = Script::new(code);
        let mut dispatcher = MockDispatcher::new();
        if let Some(run_id) = scenario.get("from_run") {
            let run_id = run_id.as_str().filter(|id| !id.is_empty()).ok_or_else(|| {
                Failure::Input("`scenario.from_run` must be a non-empty string.".into())
            })?;
            let run = self.scoped_run(run_id).await?;
            let cassette = self
                .world
                .engine
                .export_cassette(&run, Matching::Sequential)
                .await?;
            dispatcher = dispatcher.with_cassette(cassette);
        }
        if let Some(tools) = scenario.get("tools") {
            if !tools.is_object() {
                return Err(Failure::Input(
                    "`scenario.tools` must be an object of responses by tool name.".into(),
                ));
            }
            dispatcher = dispatcher
                .with_static_responses(tools)
                .map_err(|err| Failure::Problem(format!("tools: {err}")))?;
        }
        let mut signals = Vec::new();
        if let Some(listed) = scenario.get("signals") {
            let invalid = || {
                Failure::Input(
                    "`scenario.signals` must be an array of { name, payload?, when? }.".into(),
                )
            };
            for signal in listed.as_array().ok_or_else(invalid)? {
                let name = signal["name"].as_str().ok_or_else(invalid)?;
                let when = match signal.get("when").and_then(Value::as_str) {
                    None | Some("blocked") => SignalWhen::Blocked,
                    Some(after) => SignalWhen::After(parse_duration(after).map_err(|_| {
                        Failure::Problem(format!(
                            "signal {name}: when must be \"blocked\" or a duration such as \"2h\", not {after:?}"
                        ))
                    })?),
                };
                signals.push(ScenarioSignal {
                    name: name.to_owned(),
                    payload: signal.get("payload").cloned().unwrap_or(Value::Null),
                    when,
                });
            }
        }
        let expect = expectations(scenario.get("expect"))?;
        let mut test = Scenario::new(
            script,
            self.world.toolset.clone(),
            scenario.get("input").cloned().unwrap_or_else(|| json!({})),
        );
        test.signals = signals;
        test.expect = expect;
        let executor = self.world.engine.executor();
        let report = match scenario.get("mocks") {
            Some(source) => {
                let source = source.as_str().filter(|s| !s.is_empty()).ok_or_else(|| {
                    Failure::Input("`scenario.mocks` must be a non-empty string.".into())
                })?;
                let clock: Arc<dyn ptc_engine::ports::Clock> =
                    Arc::new(SharedClock::new(Scenario::DEFAULT_START));
                let handlers = TsMocks::new(&Script::new(source), clock, SandboxLimits::default())
                    .await
                    .map_err(|err| match err {
                        ptc_runtime::MockError::Invalid(message) => {
                            Failure::Problem(format!("the mocks cannot load: {message}"))
                        }
                        ptc_runtime::MockError::Internal(message) => {
                            Failure::Platform(AgentError::Checker(message))
                        }
                    })?;
                let tools = handlers.tools().clone();
                let dispatcher = dispatcher.with_mocks(Mocks { handlers, tools });
                ptc_engine::mock::run_scenario(executor, dispatcher, test).await
            }
            None => ptc_engine::mock::run_scenario(executor, dispatcher, test).await,
        };
        let report = match report {
            Ok(report) => report,
            Err(ScenarioError::Start(error)) => return Err(start_failure(error)),
            Err(ScenarioError::Engine(error)) => return Err(error.into()),
        };
        let result = TestResult {
            passed: report.passed,
            failures: report.failures,
            status: report.status.into(),
            output: report.output,
            error: report.error.as_ref().map(ScriptError::from),
            abort: report.abort.as_ref().map(Into::into),
            calls: report.calls,
            trace: report.trace,
        };
        Ok(result_of_test(&result, &self.limits))
    }

    async fn get_run(&self, input: &Map<String, Value>) -> Handled {
        let run_id = require_string(input, "run_id", "run_id")?;
        let entry = optional_index(input, "entry")?;
        let offset = optional_index(input, "offset")?;
        let wait = match input.get("wait") {
            None | Some(Value::Null) => None,
            Some(value) => Some(self.wait_of(Some(value))?),
        };
        let run = self.scoped_run(&run_id).await?;
        if let (Some(wait), None) = (wait, entry) {
            self.world
                .drive(Some(&run), Some(self.deadline(wait)))
                .await?;
        }
        if let Some(index) = entry {
            let history = self.world.engine.export_history(&run).await?;
            let Some(found) = history.iter().find(|e| e.index == index) else {
                return Err(Failure::Problem(format!(
                    "run {run_id} has no entry {index}: its entries are numbered from 0 to {}",
                    history.len().saturating_sub(1)
                )));
            };
            let entry = ptc_api::events::Entry::from_core(found, |payload| match payload.value() {
                Some(value) => ptc_api::common::Payload::of(value.clone(), u64::MAX),
                None => ptc_api::common::Payload::reference(String::new(), 0, String::new()),
            });
            let content = describe_page(
                &format!("Entry {index} of run {run_id}"),
                &entry_text(&entry),
                usize::try_from(offset.unwrap_or(0)).unwrap_or(usize::MAX),
                self.limits.output,
                |next| {
                    format!(
                        "get_run({})",
                        args(&[
                            ("run_id", json!(run_id)),
                            ("entry", json!(index)),
                            ("offset", json!(next)),
                        ])
                    )
                },
            );
            return Ok(result(content, false));
        }
        let view = view::run(self.world, &run).await?;
        if let Some(offset) = offset {
            if view.status != RunStatus::Completed {
                return Err(Failure::Input(format!(
                    "run {run_id} has no output to page through: it is {}.",
                    view.status.as_str()
                )));
            }
            let content = describe_page(
                &format!("Output of run {run_id}"),
                &self.output_of(&run, &view).await?,
                usize::try_from(offset).unwrap_or(usize::MAX),
                self.limits.output,
                |next| {
                    format!(
                        "get_run({})",
                        args(&[("run_id", json!(run_id)), ("offset", json!(next))])
                    )
                },
            );
            return Ok(result(content, false));
        }
        self.report(&run, view, false, true).await
    }

    async fn list_runs(&self, input: &Map<String, Value>) -> Handled {
        let status = match input.get("status") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                serde_json::from_value::<RunStatus>(value.clone()).map_err(|_| {
                    Failure::Input(
                        "`status` must be one of pending, active, completed, failed, cancelled, aborted."
                            .into(),
                    )
                })?,
            ),
        };
        let mut filter = RunFilter::new(TenantId::local());
        filter.labels = self.world.labels.clone();
        let mut records = self.world.engine.list_runs(&filter).await?;
        if let Some(status) = status {
            records.retain(|record| RunStatus::from(record.status) == status);
        }
        records.reverse();
        let more = records.len() > 50;
        let mut runs = Vec::new();
        for record in records.iter().take(50) {
            runs.push(view::summary(self.world, &record.run).await?);
        }
        Ok(result(describe_runs(&runs, more, status), false))
    }

    async fn signal_run(&self, input: &Map<String, Value>, call_id: &str) -> Handled {
        let run_id = require_string(input, "run_id", "run_id")?;
        let name = require_string(input, "name", "name")?;
        let run = self.scoped_run(&run_id).await?;
        let payload = input.get("payload").cloned().unwrap_or(Value::Null);
        let enqueued = self
            .world
            .engine
            .signal(&run, name.clone(), payload, Some(idempotency_key(call_id)))
            .await?;
        let content = match enqueued {
            Enqueued::RunEnded => return Err(self.run_ended(&run, "a signal").await?),
            Enqueued::Duplicate => format!(
                "This signal {} was already sent to run {run_id}: nothing was added.",
                render::quoted(&name)
            ),
            Enqueued::Accepted => format!(
                "Sent the signal {} to run {run_id}. Read what the run does next with get_run({}).",
                render::quoted(&name),
                args(&[("run_id", json!(run_id)), ("wait", json!("30s"))])
            ),
        };
        // The server's workers take the signal at once.
        self.world.drive(None, Some(self.world.now())).await?;
        Ok(result(content, false))
    }

    async fn cancel_run(&self, input: &Map<String, Value>) -> Handled {
        let run_id = require_string(input, "run_id", "run_id")?;
        let run = self.scoped_run(&run_id).await?;
        if self.world.engine.cancel(&run, None).await? == Enqueued::RunEnded {
            return Err(self.run_ended(&run, "a cancellation").await?);
        }
        self.world
            .drive(Some(&run), Some(self.deadline(self.default_wait())))
            .await?;
        let view = view::run(self.world, &run).await?;
        self.report(&run, view, false, false).await
    }

    async fn retry_run(&self, input: &Map<String, Value>, call_id: &str) -> Handled {
        let run_id = require_string(input, "run_id", "run_id")?;
        let code = require_string(input, "code", "code")?;
        let parent = self.scoped_run(&run_id).await?;
        let mut request = ForkRun::new(parent, Script::new(code));
        request.labels = Some(self.world.labels.clone());
        request.idempotency_key = Some(idempotency_key(call_id));
        let forked = self
            .world
            .engine
            .fork_checked(request, &*self.world.checker)
            .await;
        let created = match forked {
            Ok(created) => created,
            Err(ForkError::Start(error)) => return Err(start_failure(error)),
            Err(ForkError::Engine(error)) => return Err(engine_failure(error)),
            Err(other) => return Err(Failure::Problem(other.to_string())),
        };
        let run = created.run;
        self.world
            .drive(Some(&run), Some(self.deadline(self.default_wait())))
            .await?;
        let view = view::run(self.world, &run).await?;
        let failed = view.status == RunStatus::Failed;
        self.report(&run, view, true, failed).await
    }

    fn search_tools(&self, input: &Map<String, Value>) -> Handled {
        let query = require_string(input, "query", "query")?;
        let matches = search(&self.world.toolset, &self.world.declarations, &query)
            .ok_or_else(|| Failure::Problem("query: give at least one word to look for".into()))?;
        Ok(result(describe_matches(&query, &matches), false))
    }

    // ---------------------------------------------------------------------------------------
    // Shared steps

    /// A run's report: its output when it completed, its trace when asked.
    async fn report(
        &self,
        run: &RunRef,
        view: ptc_api::runs::Run,
        error_when_failed: bool,
        trace: bool,
    ) -> Handled {
        let output = if view.status == RunStatus::Completed {
            Some(self.output_of(run, &view).await?)
        } else {
            None
        };
        let trace = if trace {
            Some(self.world.engine.trace(run).await?)
        } else {
            None
        };
        let ended = matches!(
            view.status,
            RunStatus::Failed | RunStatus::Cancelled | RunStatus::Aborted
        );
        Ok(result(
            describe_run(&view, &RunExtras { output, trace }, &self.limits),
            error_when_failed && ended,
        ))
    }

    /// The run's output as compact JSON, read from its payload when it is too large to be in
    /// the run's state (`GET /v1/payloads/{digest}`).
    async fn output_of(&self, run: &RunRef, view: &ptc_api::runs::Run) -> Result<String, Failure> {
        let Some(payload) = &view.output else {
            return Ok("null".to_owned());
        };
        if let Some(value) = &payload.value {
            return Ok(json(value));
        }
        let history = self.world.engine.export_history(run).await?;
        let state = ptc_core::RunState::from_history(&history).map_err(|error| {
            Failure::Platform(AgentError::Engine(EngineError::CorruptHistory {
                run_id: run.id,
                error,
            }))
        })?;
        Ok(match state.outcome() {
            Some(ptc_core::RunOutcome::Completed { output }) => {
                output.value().map_or_else(|| "null".to_owned(), json)
            }
            _ => "null".to_owned(),
        })
    }

    /// `POST /v1/check`: the checks at start, then tsgo and the checker's rules.
    async fn check(&self, script: &Script) -> Result<CheckResult, Failure> {
        use ptc_engine::ports::ScriptExecutor as _;
        if let Err(invalid) = self
            .world
            .engine
            .executor()
            .compile(script, &self.world.toolset)
        {
            return Ok(CheckResult {
                ok: false,
                diagnostics: vec![view::invalid_script_diagnostic(&invalid)],
            });
        }
        let diagnostics = self
            .world
            .checker
            .check(script, &self.world.tool_declarations())
            .await
            .map_err(|err| Failure::Platform(AgentError::Checker(err.to_string())))?;
        Ok(view::check_result(&diagnostics))
    }

    /// A run that has ended refuses a signal or a cancellation, as the server says it.
    async fn run_ended(&self, run: &RunRef, what: &str) -> Result<Failure, Failure> {
        let status = self
            .world
            .engine
            .get_run(run)
            .await?
            .map_or("ended", |record| record.status.as_str());
        Ok(Failure::Problem(format!(
            "run {} has ended ({status}): {what} can only be sent to a run that is still active",
            run.id
        )))
    }

    /// The run, if it carries the agent's labels; otherwise the same answer as a run that does
    /// not exist.
    async fn scoped_run(&self, run_id: &str) -> Result<RunRef, Failure> {
        let missing = || {
            Failure::Input(format!(
                "Run {run_id} does not exist, or is not one of the runs you started."
            ))
        };
        let id: RunId = run_id.parse().map_err(|_| missing())?;
        let run = self.world.run_ref(id);
        let record = self.world.engine.get_run(&run).await?.ok_or_else(missing)?;
        let labeled = self
            .world
            .labels
            .iter()
            .all(|(key, value)| record.metadata.labels.get(key) == Some(value));
        if labeled { Ok(run) } else { Err(missing()) }
    }

    fn default_wait(&self) -> Duration {
        self.options.wait.min(self.options.max_wait)
    }

    /// The model's wait, bounded by `max_wait`, or the default wait.
    fn wait_of(&self, value: Option<&Value>) -> Result<Duration, Failure> {
        let wait = match value {
            None | Some(Value::Null) => return Ok(self.default_wait()),
            Some(Value::Number(ms)) => ms.as_u64().map(Duration::from_millis).ok_or_else(|| {
                Failure::Input("`wait` must be a duration such as \"30s\".".into())
            })?,
            Some(Value::String(text)) => parse_duration(text)
                .map_err(|err| Failure::Input(format!("`wait` is not a duration: {err}")))?,
            Some(_) => {
                return Err(Failure::Input(
                    "`wait` must be a duration such as \"30s\".".into(),
                ));
            }
        };
        Ok(wait.min(self.options.max_wait))
    }

    fn deadline(&self, wait: Duration) -> Timestamp {
        let ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        Timestamp(self.world.now().0.saturating_add(ms))
    }
}

fn result_of_test(result: &TestResult, limits: &Limits) -> AgentToolResult {
    AgentToolResult {
        content: describe_test(result, limits),
        is_error: !result.passed,
        script: Some(ScriptVerdict::Passed),
    }
}

/// A refused start, as the API answers it (`ptc-server`'s `error.rs`).
fn start_failure(error: StartError) -> Failure {
    match error {
        StartError::InvalidScript(invalid) => Failure::Refused {
            detail: format!("the script cannot run: {}", invalid.message),
            diagnostics: vec![view::invalid_script_diagnostic(&invalid)],
        },
        StartError::TypeErrors(diagnostics) => {
            let errors = diagnostics
                .iter()
                .filter(|d| d.severity == ptc_engine::ports::Severity::Error)
                .count();
            Failure::Refused {
                detail: format!(
                    "the script does not type-check ({errors} error{}): fix the diagnostics and \
                     start it again",
                    if errors == 1 { "" } else { "s" }
                ),
                diagnostics: diagnostics.iter().map(view::diagnostic).collect(),
            }
        }
        StartError::InvalidTtl { .. } | StartError::InputTooLarge { .. } => {
            Failure::Problem(error.to_string())
        }
        StartError::QuotaExceeded { .. } => {
            Failure::Problem(format!("{error}: wait for runs to end, or cancel some"))
        }
        StartError::Checker(error) => Failure::Platform(AgentError::Checker(error.to_string())),
        StartError::Store(error) => Failure::Platform(AgentError::Engine(error.into())),
        other => Failure::Platform(AgentError::Checker(other.to_string())),
    }
}

/// An engine error, as the API answers it: what the model can fix is a problem, the rest a
/// fault of the platform.
fn engine_failure(error: EngineError) -> Failure {
    match error {
        EngineError::RunEnded(run) => Failure::Problem(format!(
            "run {run} has ended: nothing can be sent to it any more"
        )),
        other => Failure::Platform(AgentError::Engine(other)),
    }
}

fn expectations(value: Option<&Value>) -> Result<Expectations, Failure> {
    let Some(value) = value else {
        return Ok(Expectations::default());
    };
    let expect: ptc_api::scripts::Expectations = serde_json::from_value(value.clone())
        .map_err(|err| Failure::Input(format!("`scenario.expect` is invalid: {err}.")))?;
    Ok(Expectations {
        status: expect.status.map(|status| match status {
            RunStatus::Pending => ptc_core::RunStatus::Pending,
            RunStatus::Active => ptc_core::RunStatus::Active,
            RunStatus::Completed => ptc_core::RunStatus::Completed,
            RunStatus::Failed => ptc_core::RunStatus::Failed,
            RunStatus::Cancelled => ptc_core::RunStatus::Cancelled,
            RunStatus::Aborted => ptc_core::RunStatus::Aborted,
        }),
        output: expect.output,
        error: expect.error.map(|error| ExpectedError {
            name: error.name,
            message: error.message,
        }),
        calls: expect.calls,
    })
}

/// The tools matching a query, best first, as `GET /v1/toolsets/{id}/search` ranks them: each
/// word of the query found in a tool's name counts two, in its description one; 10 tools at
/// most. `None` when the query has no word.
pub fn search(
    toolset: &ptc_core::Toolset,
    declarations: &ptc_codegen::Declarations,
    query: &str,
) -> Option<Vec<ToolMatch>> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() {
        return None;
    }
    let mut scored: Vec<(usize, &ptc_core::Tool)> = toolset
        .tools()
        .iter()
        .map(|tool| {
            let name = tool.name.to_lowercase();
            let description = tool
                .description
                .as_deref()
                .unwrap_or_default()
                .to_lowercase();
            let score = words
                .iter()
                .map(|word| {
                    usize::from(name.contains(word.as_str())) * 2
                        + usize::from(description.contains(word.as_str()))
                })
                .sum();
            (score, tool)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by(|(a, tool_a), (b, tool_b)| b.cmp(a).then_with(|| tool_a.name.cmp(&tool_b.name)));
    Some(
        scored
            .into_iter()
            .take(10)
            .map(|(_, tool)| {
                let declared = declarations.tools.iter().find(|d| d.name == tool.name);
                ToolMatch {
                    name: tool.name.clone(),
                    description: tool.description.clone().unwrap_or_default(),
                    prompt: declared.map(|d| d.prompt.clone()).unwrap_or_default(),
                    dts: declared.map(|d| d.typescript.clone()).unwrap_or_default(),
                }
            })
            .collect(),
    )
}

/// The idempotency key of a tool call: its id when the API takes it as a key, otherwise the
/// SHA-256 of the id.
pub fn idempotency_key(call_id: &str) -> String {
    match ptc_api::headers::IdempotencyKey::new(call_id) {
        Ok(key) => key.as_str().to_owned(),
        Err(_) => Sha256::digest(call_id.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    }
}

fn parse_input(name: &str, input: &Value) -> Result<Map<String, Value>, Failure> {
    let values = match input {
        Value::String(text) if text.trim().is_empty() => return Ok(Map::new()),
        Value::String(text) => serde_json::from_str(text)
            .map_err(|_| Failure::Input(format!("the arguments of {name} are not valid JSON.")))?,
        other => other.clone(),
    };
    match values {
        Value::Null => Ok(Map::new()),
        Value::Object(values) => Ok(values),
        _ => Err(Failure::Input(format!(
            "the arguments of {name} must be an object."
        ))),
    }
}

fn require_string(values: &Map<String, Value>, key: &str, label: &str) -> Result<String, Failure> {
    match values.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(Failure::Input(format!(
            "`{label}` must be a non-empty string."
        ))),
    }
}

fn optional_index(values: &Map<String, Value>, key: &str) -> Result<Option<u64>, Failure> {
    match values.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| Failure::Input(format!("`{key}` must be an integer, 0 or more."))),
    }
}

/// Labels of an episode's runs.
pub fn labels(episode: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("eval".to_owned(), episode.to_owned())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_ids_become_keys() {
        assert_eq!(idempotency_key("toolu_01AbC"), "toolu_01AbC");
        let hashed = idempotency_key("call\u{e9}");
        assert_eq!(hashed.len(), 64);
        assert_eq!(hashed, idempotency_key("call\u{e9}"));
    }

    #[test]
    fn inputs_are_objects_or_their_json() {
        assert!(parse_input("get_run", &json!("{\"run_id\":\"r\"}")).is_ok());
        assert!(parse_input("get_run", &json!("")).unwrap().is_empty());
        assert!(matches!(
            parse_input("get_run", &json!([1])),
            Err(Failure::Input(_))
        ));
        assert!(matches!(
            parse_input("get_run", &json!("{")),
            Err(Failure::Input(_))
        ));
    }

    #[test]
    fn search_ranks_names_before_descriptions() {
        let crm = || ptc_core::ToolRoute::Mcp {
            server: "crm".into(),
        };
        let tool = |name: &str, description: &str| {
            let mut tool = ptc_core::Tool::new(name, crm(), json!({ "type": "object" }));
            tool.description = Some(description.into());
            tool
        };
        let toolset = ptc_core::Toolset::new(
            [ptc_core::McpServer::new(
                "crm",
                ptc_core::McpTransport::Http {
                    url: "https://mcp.example.com/crm".into(),
                },
            )],
            vec![
                tool("crm.refund_order", "Refund an order"),
                tool("crm.get_order", "Get an order, refunds included"),
                tool("crm.post_note", "Post a note"),
            ],
        )
        .unwrap();
        let declarations = ptc_codegen::generate(&toolset, &BTreeMap::new());
        let found = search(&toolset, &declarations, "refund").unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "crm.refund_order");
        assert_eq!(found[0].description, "Refund an order");
        assert!(found[0].dts.contains("refund_order"));
        assert!(
            search(&toolset, &declarations, "deploy")
                .unwrap()
                .is_empty()
        );
        assert!(search(&toolset, &declarations, "!?").is_none());
    }
}
