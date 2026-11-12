//! End-to-end corpus of ai-ptc: TypeScript scripts run by the real engine and V8 sandbox, with
//! in-memory storage and virtual time; their journals are recorded in the stored format and
//! replayed (`archi/07-code-architecture.md`, "Tests": the epoch corpus).
//!
//! A case is a directory `corpus/epoch-<n>/<blue|red>/<name>/` holding:
//!
//! - `script.ts`, the script;
//! - `scenario.json`, its input, tools, steps and expectations ([`Scenario`]);
//! - `journal.v1.json`, the journal recorded from it (`ptc-format` v1), committed.
//!
//! **Blue** cases end as they should succeed: the run completes, or waits for the outside
//! world. **Red** cases must fail, exactly as specified: a refused start, a failed, cancelled
//! or aborted run, a replay that diverges from the journal, a tampered journal. A case that
//! does not fail the way it says it does is a failing test, whatever its color.
//!
//! Each case is checked three ways:
//!
//! 1. **Live**: the scenario runs, and its expectations hold.
//! 2. **Recorded**: the journal it produced is byte for byte the committed `journal.v1.json`.
//!    `PTC_E2E_RECORD=1 cargo test -p ptc-e2e` writes the files instead: review the diff.
//! 3. **Replayed**: the committed journal, decoded from its file, replays identically with the
//!    script, on a fresh executor with no store. This is the epoch contract: a build that
//!    stops replaying a journal of an epoch it supports fails here.

#![forbid(unsafe_code)]

pub mod scenario;
pub mod tools;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ptc_core::{
    Builtin, Command, Event, HistoryEntry, Lifecycle, RunId, RunMode, RunOutcome, RunState, Script,
    Timestamp, Tool, ToolRoute, Toolset, parse_duration,
};
use ptc_engine::adapters::{MemoryBlobs, MemoryJournal, SeededEntropy, VirtualClock};
use ptc_engine::ports::{Entropy, RunRef, ScriptExecutor, TenantId, VirtualTime};
use ptc_engine::replay::{self, ReplayEnd, ReplayError, ReplayInput};
use ptc_engine::{
    Adapters, DriveOptions, Engine, EngineConfig, Limits, ReplayVerdict, StartRun, ToolOrder,
};
use ptc_runtime::V8Executor;
use serde_json::Value;

pub use scenario::Scenario;
use scenario::{ReplayCheck, Step};
use tools::ScenarioTools;

/// Every run of the corpus starts at 2026-01-01T00:00:00Z, in virtual time.
pub const T0: Timestamp = Timestamp(1_767_225_600_000);

/// The environment variable that turns recording on, for the corpus tests.
pub const RECORD: &str = "PTC_E2E_RECORD";

/// What to do with the journal a case produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Compare it with the committed `journal.v1.json`.
    Verify,
    /// Write it to `journal.v1.json`.
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
    pub script: Script,
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
        let script = Script::new(read(&dir.join("script.ts"))?);
        let case = Self {
            name: name.to_owned(),
            dir,
            color,
            scenario,
            script,
        };
        case.check_color()?;
        case.try_toolset()
            .map_err(|err| format!("{name}/scenario.json: tools: {err}"))?;
        Ok(case)
    }

    pub fn journal_path(&self) -> PathBuf {
        self.dir.join("journal.v1.json")
    }

    /// The scenario's tools, with the default policy: host tools, and the built-ins `http`
    /// and `webhook`. In mock mode every call goes to the scenario's responses, whatever its
    /// route.
    pub fn toolset(&self) -> Toolset {
        self.try_toolset()
            .expect("the tools were checked when the case was loaded")
    }

    fn try_toolset(&self) -> Result<Toolset, String> {
        let scenario = &self.scenario;
        let tools = scenario.tools.keys().map(|name| {
            let (route, schema) = match name.as_str() {
                "http" => (
                    ToolRoute::Builtin(Builtin::Http {
                        allowed_hosts: scenario.http_hosts.clone(),
                    }),
                    ptc_runtime::builtins::http_input_schema(),
                ),
                "webhook" => (
                    ToolRoute::Builtin(Builtin::Webhook),
                    ptc_runtime::builtins::webhook_input_schema(),
                ),
                _ => (
                    ToolRoute::Host {
                        group: name.split_once('.').map(|(group, _)| group.to_owned()),
                    },
                    Value::Object(serde_json::Map::new()),
                ),
            };
            let schema = scenario.schemas.get(name).cloned().unwrap_or(schema);
            Tool::new(name.clone(), route, schema)
        });
        if let Some(unknown) = scenario
            .schemas
            .keys()
            .find(|name| !scenario.tools.contains_key(*name))
        {
            return Err(format!("schemas: {unknown} is not a tool of the scenario"));
        }
        Toolset::new([], tools).map_err(|err| err.to_string())
    }

    /// The run's id: the first id of the scenario's entropy.
    pub fn run_id(&self) -> RunId {
        SeededEntropy::new(self.scenario.seed).run_id()
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

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))
}

type CorpusEngine =
    Engine<MemoryJournal, MemoryBlobs, V8Executor, ScenarioTools, VirtualClock, SeededEntropy>;

fn executor(scenario: &Scenario) -> V8Executor {
    let mut limits = ptc_runtime::SandboxLimits::default();
    if let Some(ms) = scenario.limits.cpu_per_trigger_ms {
        limits.cpu_per_trigger = Duration::from_millis(ms);
    }
    if let Some(mb) = scenario.limits.heap_mb {
        limits.heap_bytes = mb * 1024 * 1024;
    }
    V8Executor::new(limits).expect("the corpus runs with TZ=UTC (see .cargo/config.toml)")
}

fn engine(case: &Case) -> CorpusEngine {
    let mut config = EngineConfig::default();
    let defaults = Limits::default();
    config.limits = Limits {
        max_commands_per_activation: case
            .scenario
            .limits
            .max_commands
            .unwrap_or(defaults.max_commands_per_activation),
        max_payload_bytes: case
            .scenario
            .limits
            .max_payload_bytes
            .unwrap_or(defaults.max_payload_bytes),
        ..defaults
    };
    Engine::new(
        Adapters {
            journal: MemoryJournal::new(),
            blobs: MemoryBlobs::new(),
            executor: executor(&case.scenario),
            dispatcher: ScenarioTools::new(&case.scenario.tools),
            clock: VirtualClock::new(T0),
            entropy: SeededEntropy::new(case.scenario.seed),
        },
        config,
    )
}

/// What a live run of a case produced.
#[derive(Debug)]
pub struct Recorded {
    pub run_id: RunId,
    /// With every payload loaded.
    pub history: Vec<HistoryEntry>,
    pub state: RunState,
}

/// The outcome of a live run.
#[derive(Debug)]
pub enum Live {
    /// The start was refused: no run exists.
    Refused(String),
    Ran(Box<Recorded>),
}

/// Runs a case live: starts the run, drives it, applies the steps, drives it in virtual time.
pub async fn run_live(case: &Case) -> Result<Live, String> {
    let engine = engine(case);
    let mut request = StartRun::new(
        case.script.clone(),
        case.toolset(),
        case.scenario.input.clone(),
    );
    request.mode = RunMode::Mock;
    if let Some(ttl) = &case.scenario.ttl {
        request.ttl = Some(parse_duration(ttl).map_err(|err| format!("ttl {ttl:?}: {err}"))?);
    }
    let run = match engine.start_run(request).await {
        Ok(created) => created.run,
        Err(refused) => return Ok(Live::Refused(refused.to_string())),
    };

    let options = DriveOptions {
        tool_order: match case.scenario.tool_order {
            scenario::ToolOrder::Scheduled => ToolOrder::Scheduled,
            scenario::ToolOrder::Reversed => ToolOrder::Reversed,
        },
        ..DriveOptions::default()
    };
    let drive = |virtual_time: bool| {
        let engine = &engine;
        let options = &options;
        async move {
            let report = if virtual_time {
                engine.run_until_idle_virtual(options).await
            } else {
                engine.run_until_idle(options).await
            }
            .map_err(|err| err.to_string())?;
            if let Some((run, error)) = report.failures.first() {
                return Err(format!("an activation of run {} failed: {error}", run.id));
            }
            if report.interrupted {
                return Err(format!("the run never became idle: {report:?}"));
            }
            Ok::<(), String>(())
        }
    };
    let fail = |err: ptc_engine::EngineError| err.to_string();
    drive(false).await?;
    for step in &case.scenario.steps {
        match step {
            Step::Signal { name, payload, key } => {
                engine
                    .signal(&run, name.clone(), payload.clone(), key.clone())
                    .await
                    .map_err(fail)?;
            }
            Step::Cancel { message } => {
                engine.cancel(&run, message.clone()).await.map_err(fail)?;
            }
            Step::Advance(by) => {
                let by = parse_duration(by).map_err(|err| format!("advance {by:?}: {err}"))?;
                let now = ptc_engine::ports::Clock::now(engine.clock());
                engine.clock().advance_to(now.saturating_add(by));
            }
        }
        drive(false).await?;
    }
    drive(true).await?;

    // The engine's own replay, from its store, must agree with the live run.
    match engine.replay(&run, None).await.map_err(fail)? {
        ReplayVerdict::Matches(_) => {}
        other => return Err(format!("the engine's replay of the live run: {other:?}")),
    }

    let history = engine.export_history(&run).await.map_err(fail)?;
    let state = RunState::from_history(&history).map_err(|err| err.to_string())?;
    Ok(Live::Ran(Box::new(Recorded {
        run_id: run.id,
        history,
        state,
    })))
}

/// How replaying a journal ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Replayed {
    Matches(ReplayEnd),
    Fails(String),
}

/// Replays a journal with a script on a fresh executor, without any store.
pub async fn replay_journal(case: &Case, history: &[HistoryEntry], script: &Script) -> Replayed {
    let executor = executor(&case.scenario);
    let toolset = case.toolset();
    let compiled = executor.compile(script, &toolset);
    let run = RunRef::new(TenantId::local(), case.run_id());
    let input = ReplayInput {
        run: &run,
        compiled: compiled.as_ref(),
        toolset: &toolset,
        history,
    };
    match replay::replay(&executor, &MemoryBlobs::new(), input).await {
        Ok(rebuilt) => Replayed::Matches(rebuilt.end),
        Err(ReplayError::NonDeterminism(mismatch)) => Replayed::Fails(mismatch.to_string()),
        Err(err) => Replayed::Fails(err.to_string()),
    }
}

/// Checks a case of the corpus: live, recorded, replayed, recording its journal when
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
    if let Some(expected) = &expect.calls {
        let mut calls: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &recorded.history {
            if let Event::Command(Command::ScheduleToolCall { tool, .. }) = &entry.event {
                *calls.entry(tool.clone()).or_default() += 1;
            }
        }
        check(
            &calls == expected,
            format!("calls are {calls:?}, expected {expected:?}"),
        );
    }
    if let Some(expected) = &expect.events {
        let events: Vec<&str> = recorded
            .history
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
    let journal = ptc_format::encode_history(&recorded.history);
    let path = case.journal_path();
    if mode == Mode::Record {
        if let Err(err) = std::fs::write(&path, &journal) {
            failures.push(format!("cannot record {}: {err}", path.display()));
        }
        return;
    }
    match std::fs::read_to_string(&path) {
        Ok(committed) if committed == journal => {}
        Ok(committed) => failures.push(format!(
            "the journal differs from {} at line {}: if the change is intended (and does not \
             require a new determinism epoch), record again with {RECORD}=1",
            path.display(),
            first_difference(&committed, &journal)
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
    // The committed journal, decoded from its file, replays with the script.
    let committed = match std::fs::read_to_string(case.journal_path())
        .map_err(|err| err.to_string())
        .and_then(|json| ptc_format::decode_history(&json).map_err(|err| err.to_string()))
    {
        Ok(history) => history,
        Err(err) => {
            failures.push(format!("cannot read the committed journal: {err}"));
            return;
        }
    };
    let expected_end = match committed.last().map(|entry| &entry.event) {
        Some(Event::Lifecycle(Lifecycle::RunAborted(reason))) => ReplayEnd::Aborted(reason.clone()),
        _ => ReplayEnd::EndOfHistory,
    };
    let replayed = replay_journal(case, &committed, &case.script).await;
    if replayed != Replayed::Matches(expected_end.clone()) {
        failures.push(format!(
            "the committed journal does not replay: {replayed:?} (expected {expected_end:?})"
        ));
    }

    if let Some(check) = &case.scenario.replay {
        check_altered_replay(case, &recorded.history, check, failures).await;
    }
}

async fn check_altered_replay(
    case: &Case,
    history: &[HistoryEntry],
    check: &ReplayCheck,
    failures: &mut Vec<String>,
) {
    let mut history = history.to_vec();
    if let Some(index) = check.drop_entry {
        if index >= history.len() {
            failures.push(format!("drop_entry {index} is past the end of the journal"));
            return;
        }
        history.remove(index);
    }
    let script = match &check.script {
        Some(file) => match read(&case.dir.join(file)) {
            Ok(source) => Script::new(source),
            Err(err) => {
                failures.push(err);
                return;
            }
        },
        None => case.script.clone(),
    };
    let replayed = replay_journal(case, &history, &script).await;
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
