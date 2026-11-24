//! End-to-end corpus of Rewind: TypeScript workflows run by the engine's run plane and the V8
//! sandbox, over the memory adapters in virtual time; their histories are recorded in the stored
//! format and replayed (`archi/07-code-architecture.md`, "Tests": the epoch corpus).
//!
//! A case is a directory `corpus/epoch-<n>/<blue|red>/<name>/` holding:
//!
//! - `workflow.ts`, the code;
//! - `scenario.json`, its input, actions, steps and expectations ([`Scenario`]);
//! - `history.v1.json`, the run export recorded from it (`rewind-format` v1): the run's id,
//!   its code, its action set and its events, committed.
//!
//! A case may fork its run with a corrected code (`fork` in the scenario): the expectations,
//! the recorded history and the replays are then the fork's.
//!
//! **Blue** cases end as they should succeed: the run completes, or waits for the outside
//! world. **Red** cases must fail, exactly as specified: a refused start, a failed, cancelled
//! or aborted run, a replay that diverges from the history, a tampered history. A case that
//! does not fail the way it says it does is a failing test, whatever its color.
//!
//! Each case is checked three ways:
//!
//! 1. **Live**: the scenario runs on one node of the run plane in the engine's driver
//!    ([`live`]), and its expectations hold.
//! 2. **Recorded**: the history it produced is byte for byte the committed `history.v1.json`.
//!    `REWIND_E2E_RECORD=1 cargo test -p rewind-e2e` writes the files instead: review the diff.
//! 3. **Replayed**: the committed history, decoded from its file, replays identically with the
//!    code it holds, which must be the case's, on a fresh executor with no store. This is the
//!    epoch contract: a build that stops replaying a history of an epoch it supports fails
//!    here.

#![forbid(unsafe_code)]

pub mod actions;
pub mod live;
pub mod scenario;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rewind_core::{
    Action, ActionAuth, ActionRoute, ActionSet, Attachments, Builtin, Code, Command, Event,
    HistoryEvent, Lifecycle, McpServer, McpTransport, RunExport, RunId, RunOutcome, RunState,
    SecretPlacement, SecretRef, Timestamp, Workflow, WorkflowSchemas,
};
use rewind_engine::adapters::SeededEntropy;
use rewind_engine::ports::{CodeExecutor, Entropy, RunRef, TenantId};
use rewind_engine::replay::{self, ReplayEnd, ReplayError, ReplayInput};
use rewind_runtime::V8Executor;
use serde_json::Value;

pub use live::run_live;
use scenario::ReplayCheck;
pub use scenario::Scenario;

/// Every run of the corpus starts at 2026-01-01T00:00:00Z, in virtual time.
pub const T0: Timestamp = Timestamp(1_767_225_600_000);

/// The environment variable that turns recording on, for the corpus tests.
pub const RECORD: &str = "REWIND_E2E_RECORD";

/// What to do with the history a case produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Compare it with the committed `history.v1.json`.
    Verify,
    /// Write it to `history.v1.json`.
    Record,
}

impl Mode {
    /// `Record` when [`RECORD`] is set.
    pub fn from_env() -> Self {
        if std::env::var_os(RECORD).is_some() {
            Mode::Record
        } else {
            Mode::Verify
        }
    }
}

/// The corpus directory.
pub fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// Must end as it should: completed, or waiting for the outside world.
    Blue,
    /// Must fail, exactly as specified.
    Red,
}

/// A case of the corpus, loaded.
#[derive(Debug, Clone)]
pub struct Case {
    /// `epoch-1/blue/fan_out`.
    pub name: String,
    pub dir: PathBuf,
    pub color: Color,
    pub scenario: Scenario,
    pub code: Code,
    /// The corrected code of the fork, if the case forks its run.
    pub fork_code: Option<Code>,
    /// The workflows of the scenario's workflows, by name.
    pub workflow_scripts: BTreeMap<String, Code>,
}

impl Case {
    /// Loads `corpus/<name>`.
    pub fn load(name: &str) -> Result<Self, String> {
        let dir = corpus_dir().join(name);
        let color = match dir
            .parent()
            .and_then(Path::file_name)
            .and_then(|c| c.to_str())
        {
            Some("blue") => Color::Blue,
            Some("red") => Color::Red,
            other => {
                return Err(format!(
                    "{name}: a case lives in blue/ or red/, not {other:?}"
                ));
            }
        };
        let scenario: Scenario = serde_json::from_str(&read(&dir.join("scenario.json"))?)
            .map_err(|err| format!("{name}/scenario.json: {err}"))?;
        let code = Code::new(read(&dir.join("workflow.ts"))?);
        let fork_code = match &scenario.fork {
            Some(fork) => Some(Code::new(read(&dir.join(&fork.code))?)),
            None => None,
        };
        let mut workflow_scripts = BTreeMap::new();
        for (workflow, case) in &scenario.workflows {
            workflow_scripts.insert(workflow.clone(), Code::new(read(&dir.join(&case.code))?));
        }
        let case = Self {
            name: name.to_owned(),
            dir,
            color,
            scenario,
            code,
            fork_code,
            workflow_scripts,
        };
        case.check_color()?;
        scenario_action_set(&case.scenario)
            .map_err(|err| format!("{name}/scenario.json: {err}"))?;
        case.workflows()?;
        attachments(&case.scenario.secrets)
            .map_err(|err| format!("{name}/scenario.json: secrets: {err}"))?;
        for reference in case.scenario.put_secrets.keys() {
            SecretRef::parse(reference)
                .map_err(|err| format!("{name}/scenario.json: put_secrets: {err}"))?;
        }
        if !matches!(
            case.scenario.attach.as_deref(),
            None | Some("namespace" | "organization")
        ) {
            return Err(format!(
                "{name}/scenario.json: attach is namespace or organization"
            ));
        }
        Ok(case)
    }

    /// The scenario's workflows, each with the scenario's actions as its action set.
    pub fn workflows(&self) -> Result<Vec<Workflow>, String> {
        let action_set = scenario_action_set(&self.scenario)?.digest();
        self.scenario
            .workflows
            .iter()
            .map(|(name, case)| {
                let secrets = attachments(&case.secrets).map_err(|err| {
                    format!("{}/scenario.json: workflow {name}: {err}", self.name)
                })?;
                Workflow::new(
                    name.clone(),
                    case.description.clone(),
                    self.workflow_scripts[name].digest(),
                    action_set,
                    WorkflowSchemas {
                        input: case.input.clone(),
                        output: case.output.clone(),
                        signals: case.signals.clone(),
                    },
                )
                .map(|workflow| workflow.with_secrets(secrets))
                .map_err(|err| format!("{}/scenario.json: workflow {name}: {err}", self.name))
            })
            .collect()
    }

    pub fn history_path(&self) -> PathBuf {
        self.dir.join("history.v1.json")
    }

    /// The scenario's actions: harness actions, MCP actions, and the built-ins `http` and
    /// `webhook`, with the pinned schemas of `schemas` and `output_schemas` and the policies of
    /// `policies` (the default otherwise). In a test run every call goes to the scenario's
    /// responses, whatever its route.
    pub fn action_set(&self) -> ActionSet {
        let workflows = self
            .workflows()
            .expect("the workflows were checked when the case was loaded");
        scenario_action_set(&self.scenario)
            .and_then(|action_set| {
                action_set
                    .with_workflows(workflows.iter().map(Workflow::pinned))
                    .map_err(|err| err.to_string())
            })
            .expect("the actions were checked when the case was loaded")
    }

    /// The id of the run the case records: the first id of the scenario's entropy, or the
    /// fork's, drawn after the parent's id and seed.
    pub fn run_id(&self) -> RunId {
        let entropy = SeededEntropy::new(self.scenario.seed);
        let parent = entropy.run_id();
        if self.scenario.fork.is_none() {
            return parent;
        }
        entropy.seed();
        entropy.run_id()
    }

    /// The code of the run the case records: the fork's, if it forks.
    pub fn recorded_code(&self) -> &Code {
        self.fork_code.as_ref().unwrap_or(&self.code)
    }

    /// A blue case expects no failure; a red case expects one.
    pub fn check_color(&self) -> Result<(), String> {
        let expect = &self.scenario.expect;
        let failing_status = matches!(
            expect.status.as_deref(),
            Some("failed" | "cancelled" | "aborted")
        );
        let failing_replay = self
            .scenario
            .replay
            .as_ref()
            .is_some_and(|check| check.diverges.is_some() || check.error.is_some());
        let expects_failure = expect.start_error.is_some()
            || expect.error.is_some()
            || expect.abort.is_some()
            || failing_status
            || failing_replay;
        match self.color {
            Color::Blue if expects_failure => Err(format!(
                "{}: a blue case cannot expect a failure; move it to red/",
                self.name
            )),
            Color::Blue if !matches!(expect.status.as_deref(), Some("completed" | "active")) => {
                Err(format!(
                    "{}: a blue case must expect the status \"completed\" or \"active\"",
                    self.name
                ))
            }
            Color::Red if !expects_failure => Err(format!(
                "{}: a red case must expect a failure: start_error, error, abort, a failing \
                 status, or a replay that diverges",
                self.name
            )),
            _ => Ok(()),
        }
    }
}

/// The scenario's actions: harness actions, the actions of its MCP servers, and the built-ins:
/// `http` as `rewind_mcp::http_action` pins it, `webhook` with the schema of
/// `rewind_runtime::builtins`. `schemas` pins the input schema of some actions, `output_schemas`
/// the output schema of some MCP actions, and `policies` changes the default policy of some. The
/// responses of `http` and of the MCP actions must be in the format their dispatcher returns.
fn scenario_action_set(scenario: &Scenario) -> Result<ActionSet, String> {
    if let Some(unknown) = scenario
        .schemas
        .keys()
        .find(|name| !scenario.actions.contains_key(*name))
    {
        return Err(format!(
            "schemas: {unknown} is not an action of the scenario"
        ));
    }
    if let Some(unknown) = scenario
        .auth
        .keys()
        .find(|server| !scenario.mcp_servers.contains(server))
    {
        return Err(format!("auth: {unknown} is not one of mcp_servers"));
    }
    let servers: Vec<McpServer> = scenario
        .mcp_servers
        .iter()
        .map(|server| {
            let mut pinned = McpServer::new(
                server.clone(),
                McpTransport::Http {
                    url: format!("https://mcp.example.com/{server}"),
                },
            );
            pinned.auth = match scenario.auth.get(server).map(String::as_str) {
                None => ActionAuth::None,
                Some("secret") => ActionAuth::Secret(SecretPlacement::bearer()),
                Some("rewind") => ActionAuth::Rewind,
                Some(other) => {
                    return Err(format!("auth: {server} is secret or rewind, not {other:?}"));
                }
            };
            Ok(pinned)
        })
        .collect::<Result<_, String>>()?;
    let mut actions = Vec::new();
    for (name, response) in &scenario.actions {
        // The built-ins of a workflow come from `workflows`; in a test run, their answers are
        // here.
        if rewind_core::WorkflowOp::of_action(name).is_some() {
            continue;
        }
        let server = name
            .split_once('.')
            .map(|(namespace, _)| namespace)
            .filter(|namespace| {
                scenario
                    .mcp_servers
                    .iter()
                    .any(|server| server == namespace)
            });
        let mut action = match (name.as_str(), server) {
            ("http", _) => rewind_mcp::http_action(scenario.http_hosts.clone()),
            ("webhook", _) => Action::new(
                "webhook",
                ActionRoute::Builtin(Builtin::Webhook),
                rewind_runtime::builtins::webhook_input_schema(),
            ),
            (_, Some(server)) => {
                let mut action = Action::new(
                    name.clone(),
                    ActionRoute::Mcp {
                        server: server.to_owned(),
                    },
                    Value::Object(serde_json::Map::new()),
                );
                action.output_schema = scenario.output_schemas.get(name).cloned();
                action
            }
            _ => Action::new(
                name.clone(),
                ActionRoute::Harness {
                    group: name.split_once('.').map(|(group, _)| group.to_owned()),
                },
                Value::Object(serde_json::Map::new()),
            ),
        };
        if let Some(schema) = scenario.schemas.get(name) {
            action.input_schema = schema.clone();
        }
        check_responses(&action, response)?;
        actions.push(action);
    }
    if let Some(unknown) = scenario.output_schemas.keys().find(|name| {
        !actions
            .iter()
            .any(|action| &action.name == *name && matches!(action.route, ActionRoute::Mcp { .. }))
    }) {
        return Err(format!(
            "output_schemas: {unknown} is not an MCP action of the scenario"
        ));
    }
    for (name, policy) in &scenario.policies {
        let action = actions
            .iter_mut()
            .find(|action| action.name == *name)
            .ok_or_else(|| format!("policies: {name} is not one of the actions"))?;
        action.policy.approval = policy.approval;
        action.policy.idempotent = policy.idempotent.unwrap_or(true);
    }
    ActionSet::new(servers, actions).map_err(|err| format!("actions: {err}"))
}

/// The responses of an action that are values are in the format its dispatcher returns: the
/// result of the `http` action of `rewind-mcp`, or the content of an MCP action pinned without an
/// output schema.
fn check_responses(action: &Action, response: &Value) -> Result<(), String> {
    let responses = match response.get("$sequence") {
        Some(Value::Array(sequence)) => sequence.iter().collect(),
        _ => vec![response],
    };
    let values = responses.into_iter().filter(|response| {
        !response
            .as_object()
            .is_some_and(|fields| fields.keys().any(|key| key.starts_with('$')))
    });
    for value in values {
        match &action.route {
            ActionRoute::Builtin(Builtin::Http { .. }) => {
                let schema = rewind_mcp::http_output_schema();
                let validator = jsonschema::validator_for(&schema)
                    .map_err(|err| format!("the output schema of http: {err}"))?;
                if let Some(error) = validator.iter_errors(value).next() {
                    return Err(format!(
                        "actions: http: {value} is not a result of the http action: {error} at {}",
                        error.instance_path()
                    ));
                }
            }
            ActionRoute::Mcp { .. } if action.output_schema.is_none() => {
                let content = value.as_object().is_some_and(|fields| {
                    fields.get("content").is_some_and(Value::is_array)
                        && fields
                            .keys()
                            .all(|key| key == "content" || key == "structuredContent")
                });
                if !content {
                    return Err(format!(
                        "actions: {}: {value} is not MCP content, {{\"content\": [blocks], \
                         \"structuredContent\"?: value}}",
                        action.name
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))
}

/// The attachments of a scenario or of a workflow, read.
fn attachments(secrets: &BTreeMap<String, String>) -> Result<Attachments, String> {
    secrets
        .iter()
        .map(|(server, secret)| {
            let secret = SecretRef::parse(secret).map_err(|err| format!("{server}: {err}"))?;
            Ok((server.clone(), secret))
        })
        .collect()
}

fn executor(scenario: &Scenario) -> V8Executor {
    let mut limits = rewind_runtime::SandboxLimits::default();
    if let Some(ms) = scenario.limits.cpu_per_trigger_ms {
        limits.cpu_per_trigger = Duration::from_millis(ms);
    }
    if let Some(mb) = scenario.limits.heap_mb {
        limits.heap_bytes = mb * 1024 * 1024;
    }
    V8Executor::new(limits).expect("the corpus runs with TZ=UTC (see .cargo/config.toml)")
}

/// What a live run of a case produced.
#[derive(Debug)]
pub struct Recorded {
    pub run_id: RunId,
    /// With every payload loaded.
    pub export: RunExport,
    pub state: RunState,
    /// How many times each action answered, across the case.
    pub dispatched: BTreeMap<String, usize>,
    /// The statuses of the runs of workflows the run started, in the order of its calls.
    pub children: Vec<String>,
    /// How each call the actions answered authenticated, in order.
    pub auth: Vec<String>,
    /// The idempotency keys of the calls the actions answered, in order.
    pub keys: Vec<String>,
}

/// The outcome of a live run ([`run_live`]).
#[derive(Debug)]
pub enum Live {
    /// The start was refused: no run exists.
    Refused(String),
    Ran(Box<Recorded>),
}

/// How replaying a history ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Replayed {
    Matches(ReplayEnd),
    Fails(String),
}

/// Replays a run export's events with a code on a fresh executor, without any store:
/// the document holds the run's id and action set.
pub async fn replay_export(case: &Case, export: &RunExport, code: &Code) -> Replayed {
    let executor = executor(&case.scenario);
    let compiled = executor.compile(code, &export.action_set);
    let run = RunRef::new(TenantId::local(), export.run);
    let input = ReplayInput {
        run: &run,
        compiled: compiled.as_ref(),
        action_set: &export.action_set,
        history: &export.events,
    };
    match replay::replay(&executor, &replay::InExport, input).await {
        Ok(rebuilt) => Replayed::Matches(rebuilt.end),
        Err(ReplayError::NonDeterminism(mismatch)) => Replayed::Fails(mismatch.to_string()),
        Err(err) => Replayed::Fails(err.to_string()),
    }
}

/// Checks a case of the corpus: live, recorded, replayed, recording its history when
/// [`RECORD`] is set. Panics with every failed expectation.
pub async fn check_case(name: &str) {
    let failures = match Case::load(name) {
        Ok(case) => check(&case, Mode::from_env()).await,
        Err(err) => vec![err],
    };
    if !failures.is_empty() {
        let mut report = format!("{name}:\n");
        for failure in failures {
            let _ = writeln!(report, "  - {failure}");
        }
        panic!("{report}");
    }
}

/// Checks a case: live, recorded, replayed. Returns every failed expectation.
pub async fn check(case: &Case, mode: Mode) -> Vec<String> {
    let mut failures = Vec::new();
    let expect = &case.scenario.expect;
    let recorded = match run_live(case).await {
        Err(err) => return vec![format!("the live run failed: {err}")],
        Ok(Live::Refused(refused)) => {
            match &expect.start_error {
                Some(expected) if refused.contains(expected.as_str()) => {}
                Some(expected) => failures.push(format!(
                    "the start was refused with {refused:?}, which does not contain {expected:?}"
                )),
                None => failures.push(format!("the start was refused: {refused}")),
            }
            return failures;
        }
        Ok(Live::Ran(recorded)) => recorded,
    };
    if let Some(expected) = &expect.start_error {
        failures.push(format!(
            "the start was accepted, but it must be refused with {expected:?}"
        ));
        return failures;
    }

    check_expectations(case, &recorded, &mut failures);
    check_recording(case, &recorded, mode, &mut failures);
    check_replays(case, &recorded, &mut failures).await;
    failures
}

fn check_expectations(case: &Case, recorded: &Recorded, failures: &mut Vec<String>) {
    let expect = &case.scenario.expect;
    let state = &recorded.state;
    let mut check = |ok: bool, message: String| {
        if !ok {
            failures.push(message);
        }
    };

    if let Some(children) = &expect.children {
        check(
            &recorded.children == children,
            format!(
                "the runs it started are {:?}, expected {children:?}",
                recorded.children
            ),
        );
    }

    if let Some(status) = &expect.status {
        check(
            state.status().as_str() == status,
            format!("status is {}, expected {status}", state.status()),
        );
    }
    if let Some(expected) = &expect.output {
        match state.outcome() {
            Some(RunOutcome::Completed { output }) => {
                let output = output.value().cloned().unwrap_or(Value::Null);
                check(
                    contains(&output, expected),
                    format!("output is {output}, which does not contain {expected}"),
                );
            }
            other => check(false, format!("no output: the run ended with {other:?}")),
        }
    }
    if let Some(expected) = &expect.error {
        match state.outcome() {
            Some(RunOutcome::Failed { error }) => {
                check(
                    error.name == expected.name,
                    format!("error is {}, expected {}", error.name, expected.name),
                );
                if let Some(message) = &expected.message {
                    check(
                        error.message.contains(message.as_str()),
                        format!(
                            "error message {:?} does not contain {message:?}",
                            error.message
                        ),
                    );
                }
            }
            other => check(false, format!("the run did not fail: {other:?}")),
        }
    }
    if let Some(expected) = &expect.abort {
        match state.outcome() {
            Some(RunOutcome::Aborted { reason }) => {
                check(
                    reason.kind.as_str() == expected.kind,
                    format!(
                        "abort kind is {}, expected {}",
                        reason.kind.as_str(),
                        expected.kind
                    ),
                );
                check(
                    reason.message.contains(expected.message.as_str()),
                    format!(
                        "abort message {:?} does not contain {:?}",
                        reason.message, expected.message
                    ),
                );
            }
            other => check(false, format!("the run was not aborted: {other:?}")),
        }
    }
    if let Some(expected) = &expect.auth {
        check(
            &recorded.auth == expected,
            format!(
                "the calls authenticated with {:?}, expected {expected:?}",
                recorded.auth
            ),
        );
    }
    if let Some(expected) = &expect.keys {
        let run = case.run_id().to_string();
        let expected: Vec<String> = expected
            .iter()
            .map(|key| key.replace("{run}", &run))
            .collect();
        check(
            recorded.keys == expected,
            format!(
                "the calls carried the keys {:?}, expected {expected:?}",
                recorded.keys
            ),
        );
    }
    if let Some(expected) = &expect.dispatched {
        check(
            &recorded.dispatched == expected,
            format!(
                "the actions answered {:?}, expected {expected:?}",
                recorded.dispatched
            ),
        );
    }
    if let Some(expected) = &expect.calls {
        let mut calls: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &recorded.export.events {
            if let Event::Command(Command::ScheduleCall { action, .. }) = &entry.event {
                *calls.entry(action.clone()).or_default() += 1;
            }
        }
        check(
            &calls == expected,
            format!("calls are {calls:?}, expected {expected:?}"),
        );
    }
    if let Some(expected) = &expect.events {
        let events: Vec<&str> = recorded
            .export
            .events
            .iter()
            .map(|entry| entry.event.name())
            .collect();
        check(
            events == *expected,
            format!("events are {events:?}, expected {expected:?}"),
        );
    }
    if let Some(expected) = &expect.awaiting {
        let awaiting: Vec<&str> = state
            .awaited_signals()
            .iter()
            .map(|wait| wait.name.as_str())
            .collect();
        check(
            awaiting == *expected,
            format!("awaiting {awaiting:?}, expected {expected:?}"),
        );
    }
}

/// `actual` contains `expected`: equal, or for objects, every expected key is in `actual`
/// with a value that contains the expected one.
fn contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|actual| contains(actual, value))
        }),
        _ => actual == expected,
    }
}

fn check_recording(case: &Case, recorded: &Recorded, mode: Mode, failures: &mut Vec<String>) {
    if recorded.run_id != case.run_id() {
        failures.push(format!(
            "the run id {} is not the scenario's first id {}",
            recorded.run_id,
            case.run_id()
        ));
    }
    let export = rewind_format::encode_run_export(&recorded.export);
    let path = case.history_path();
    if mode == Mode::Record {
        if let Err(err) = std::fs::write(&path, &export) {
            failures.push(format!("cannot record {}: {err}", path.display()));
        }
        return;
    }
    match std::fs::read_to_string(&path) {
        Ok(committed) if committed == export => {}
        Ok(committed) => failures.push(format!(
            "the history differs from {} at line {}: if the change is intended (and does not \
             require a new determinism epoch), record again with {RECORD}=1",
            path.display(),
            first_difference(&committed, &export)
        )),
        Err(_) => failures.push(format!(
            "{} is missing: record it with {RECORD}=1",
            path.display()
        )),
    }
}

fn first_difference(a: &str, b: &str) -> usize {
    a.lines()
        .zip(b.lines())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| a.lines().count().min(b.lines().count()))
        + 1
}

async fn check_replays(case: &Case, recorded: &Recorded, failures: &mut Vec<String>) {
    // The committed history, decoded from its file, holds the case's code and replays with
    // it.
    let committed = match std::fs::read_to_string(case.history_path())
        .map_err(|err| err.to_string())
        .and_then(|json| rewind_format::decode_run_export(&json).map_err(|err| err.to_string()))
    {
        Ok(export) => export,
        Err(err) => {
            failures.push(format!("cannot read the committed history: {err}"));
            return;
        }
    };
    if let Err(err) = committed.check() {
        failures.push(format!("the committed history is inconsistent: {err}"));
    }
    if &committed.code != case.recorded_code() {
        failures.push("the committed history holds another code than the case's".into());
    }
    let expected_end = match committed.events.last().map(|entry| &entry.event) {
        Some(Event::Lifecycle(Lifecycle::RunAborted(reason))) => ReplayEnd::Aborted(reason.clone()),
        _ => ReplayEnd::EndOfHistory,
    };
    let replayed = replay_export(case, &committed, case.recorded_code()).await;
    if replayed != Replayed::Matches(expected_end.clone()) {
        failures.push(format!(
            "the committed history does not replay: {replayed:?} (expected {expected_end:?})"
        ));
    }

    if let Some(check) = &case.scenario.replay {
        check_altered_replay(case, &recorded.export, check, failures).await;
    }
}

async fn check_altered_replay(
    case: &Case,
    export: &RunExport,
    check: &ReplayCheck,
    failures: &mut Vec<String>,
) {
    let mut export = export.clone();
    let history: &mut Vec<HistoryEvent> = &mut export.events;
    if let Some(index) = check.drop_event {
        if index >= history.len() {
            failures.push(format!("drop_event {index} is past the end of the history"));
            return;
        }
        history.remove(index);
    }
    let code = match &check.code {
        Some(file) => match read(&case.dir.join(file)) {
            Ok(source) => Code::new(source),
            Err(err) => {
                failures.push(err);
                return;
            }
        },
        None => case.recorded_code().clone(),
    };
    let replayed = replay_export(case, &export, &code).await;
    let expected = check.diverges.as_ref().or(check.error.as_ref());
    match (&replayed, expected) {
        (Replayed::Fails(message), Some(expected)) if message == expected => {}
        (_, Some(expected)) => failures.push(format!(
            "the altered replay must fail with {expected:?}, got {replayed:?}"
        )),
        (Replayed::Matches(_), None) => {}
        (Replayed::Fails(message), None) => {
            failures.push(format!("the altered replay must match, got {message:?}"));
        }
    }
}
