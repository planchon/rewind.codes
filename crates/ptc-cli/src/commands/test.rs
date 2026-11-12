//! `ptc test scenario.json`: runs a scenario in mock mode, in virtual time, in this process
//! (`archi/05-testing-and-observability.md`, "Testing and mocks").
//!
//! A scenario file holds the fields of `POST /v1/test`, with files instead of their contents:
//!
//! ```json
//! {
//!   "script": "triage.ts",
//!   "toolset": "toolset.json",
//!   "input": { "repo": "acme/api" },
//!   "cassette": "cassettes/triage.json",
//!   "mocks": "triage.mocks.ts",
//!   "tools": { "slack.post": { "ok": true } },
//!   "signals": [{ "name": "approval", "when": "blocked", "payload": { "close": true } }],
//!   "expect": { "status": "completed", "output": { "closed": 3 }, "calls": { "github.close_issue": 3 } }
//! }
//! ```
//!
//! Paths are relative to the scenario's directory. Calls are answered by the cassette, then the
//! mock handlers, then the static responses: those of `ptc.tools.json` next to the scenario,
//! overridden tool by tool by the scenario's `tools`. Without a `toolset`, the toolset is the
//! tools the mocks answer, as host tools.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ptc_api::common::{AbortReason, RunStatus as ApiStatus, ScriptError};
use ptc_api::scripts::{Expectations, ScenarioSignal, TestResult};
use ptc_core::{Cassette, RunStatus, Script, Toolset};
use ptc_engine::adapters::VirtualClock;
use ptc_engine::mock::{
    self, ExpectedError, MockDispatcher, Mocks, NoMocks, Scenario, ScenarioError, SignalWhen,
};
use ptc_engine::{EngineError, StartError};
use ptc_runtime::{SandboxLimits, TsMocks, V8Executor};
use serde::Deserialize;
use serde_json::Value;

use crate::args::TestArgs;
use crate::error::CliError;
use crate::exit::Exit;
use crate::input;
use crate::output::Console;
use crate::toolset;

/// The static responses read next to a scenario.
pub(crate) const TOOLS_FILE: &str = "ptc.tools.json";

/// A scenario file.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScenarioFile {
    script: PathBuf,
    #[serde(default)]
    toolset: Option<PathBuf>,
    #[serde(default = "empty_object")]
    input: Value,
    #[serde(default)]
    cassette: Option<PathBuf>,
    #[serde(default)]
    mocks: Option<PathBuf>,
    #[serde(default)]
    tools: BTreeMap<String, Value>,
    #[serde(default)]
    signals: Vec<ScenarioSignal>,
    #[serde(default)]
    expect: Expectations,
    /// The run's TTL, such as `"1h"`. 30 days by default.
    #[serde(default)]
    ttl: Option<ptc_api::common::Duration>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

pub(super) async fn test(args: TestArgs, console: &mut Console) -> Result<Exit, CliError> {
    let file: ScenarioFile = input::read_json(&args.scenario)?;
    let base = args
        .scenario
        .parent()
        .map(Path::to_owned)
        .unwrap_or_default();
    let tools_file = match &args.tools {
        Some(path) => Some(path.clone()),
        None => Some(base.join(TOOLS_FILE)).filter(|path| path.exists()),
    };
    let result = run(&file, &base, tools_file.as_deref()).await?;
    if console.json() {
        console.print_json(&result)?;
    } else {
        let verdict = if result.passed { "PASS" } else { "FAIL" };
        console.line(format!(
            "{verdict} {} (the run is {})",
            args.scenario.display(),
            result.status
        ));
        for failure in &result.failures {
            console.line(format!("  - {failure}"));
        }
        console.line("");
        console.text(&result.trace);
    }
    Ok(if result.passed {
        Exit::Success
    } else {
        Exit::Failed
    })
}

async fn run(
    file: &ScenarioFile,
    base: &Path,
    tools_file: Option<&Path>,
) -> Result<TestResult, CliError> {
    let script = Script::new(input::read_text(&base.join(&file.script))?);

    let mut statics: BTreeMap<String, Value> = match tools_file {
        Some(path) => input::read_json(path)?,
        None => BTreeMap::new(),
    };
    statics.extend(file.tools.clone());
    let statics = Value::Object(statics.into_iter().collect());
    let mut dispatcher = MockDispatcher::new()
        .with_static_responses(&statics)
        .map_err(|error| CliError::usage(format!("the static responses: {error}")))?;

    let mut mocked: BTreeSet<String> = statics
        .as_object()
        .map(|tools| tools.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(path) = &file.cassette {
        let path = base.join(path);
        let cassette: Cassette = ptc_format::decode_cassette(&input::read_text(&path)?)
            .map_err(|error| CliError::usage(format!("{}: {error}", path.display())))?;
        mocked.extend(cassette.calls.iter().map(|call| call.tool.clone()));
        dispatcher = dispatcher.with_cassette(cassette);
    }

    let executor = V8Executor::new(SandboxLimits::default())
        .map_err(|error| CliError::internal(error.to_string()))?;
    let report = match &file.mocks {
        Some(path) => {
            let path = base.join(path);
            let source = Script::new(input::read_text(&path)?);
            let clock = Arc::new(VirtualClock::new(Scenario::DEFAULT_START));
            let handlers = TsMocks::new(&source, clock, SandboxLimits::default())
                .await
                .map_err(|error| CliError::usage(format!("{}: {error}", path.display())))?;
            mocked.extend(handlers.tools().iter().cloned());
            let tools = handlers.tools().clone();
            let toolset = scenario_toolset(file, base, &mocked).await?;
            let scenario = scenario(file, script, toolset)?;
            let dispatcher = dispatcher.with_mocks(Mocks { handlers, tools });
            mock::run_scenario(&executor, dispatcher, scenario).await
        }
        None => {
            let toolset = scenario_toolset(file, base, &mocked).await?;
            let scenario = scenario(file, script, toolset)?;
            let dispatcher: MockDispatcher<NoMocks> = dispatcher;
            mock::run_scenario(&executor, dispatcher, scenario).await
        }
    };
    let report = report.map_err(|error| match error {
        ScenarioError::Start(refused @ StartError::InvalidScript(_)) => CliError::ScriptRefused {
            message: refused.to_string(),
            diagnostics: Vec::new(),
        },
        ScenarioError::Start(
            refused @ (StartError::InvalidTtl { .. } | StartError::InputTooLarge { .. }),
        ) => CliError::usage(format!("the run cannot start: {refused}")),
        ScenarioError::Start(refused @ StartError::QuotaExceeded { .. }) => {
            CliError::Refused(format!("the run cannot start: {refused}"))
        }
        ScenarioError::Start(refused) => {
            CliError::internal(format!("the run cannot start: {refused}"))
        }
        ScenarioError::Engine(error) => engine_error(&error),
    })?;
    Ok(TestResult {
        passed: report.passed,
        failures: report.failures,
        status: report.status.into(),
        output: report.output,
        error: report.error.as_ref().map(ScriptError::from),
        abort: report.abort.as_ref().map(AbortReason::from),
        calls: report.calls,
        trace: report.trace,
    })
}

/// The scenario's toolset file, pinned; else the tools its mocks answer.
async fn scenario_toolset(
    file: &ScenarioFile,
    base: &Path,
    mocked: &BTreeSet<String>,
) -> Result<Toolset, CliError> {
    match &file.toolset {
        Some(path) => toolset::pin_file(&base.join(path)).await,
        None => toolset::of_mocks(mocked),
    }
}

fn engine_error(error: &EngineError) -> CliError {
    CliError::internal(format!("the scenario could not run: {error}"))
}

/// The engine's scenario.
fn scenario(file: &ScenarioFile, script: Script, toolset: Toolset) -> Result<Scenario, CliError> {
    let mut scenario = Scenario::new(script, toolset, file.input.clone());
    scenario.ttl = file.ttl.map(|ttl| ttl.get());
    scenario.signals = file
        .signals
        .iter()
        .map(|signal| {
            let when = match signal.when.as_deref() {
                None | Some("blocked") => SignalWhen::Blocked,
                Some(after) => {
                    let after: ptc_api::common::Duration = after.parse().map_err(|_| {
                        CliError::usage(format!(
                            "the signal {:?}: when is \"blocked\" or a duration such as \"2h\", not {after:?}",
                            signal.name
                        ))
                    })?;
                    SignalWhen::After(after.get())
                }
            };
            Ok(mock::ScenarioSignal {
                name: signal.name.clone(),
                payload: signal.payload.clone(),
                when,
            })
        })
        .collect::<Result<_, CliError>>()?;
    scenario.expect = mock::Expectations {
        status: file.expect.status.map(core_status),
        output: file.expect.output.clone(),
        error: file.expect.error.as_ref().map(|error| ExpectedError {
            name: error.name.clone(),
            message: error.message.clone(),
        }),
        calls: file.expect.calls.clone(),
    };
    Ok(scenario)
}

fn core_status(status: ApiStatus) -> RunStatus {
    match status {
        ApiStatus::Pending => RunStatus::Pending,
        ApiStatus::Active => RunStatus::Active,
        ApiStatus::Completed => RunStatus::Completed,
        ApiStatus::Failed => RunStatus::Failed,
        ApiStatus::Cancelled => RunStatus::Cancelled,
        ApiStatus::Aborted => RunStatus::Aborted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scenario_file_refuses_unknown_fields() {
        let file: ScenarioFile = serde_json::from_str(
            r#"{"script": "a.ts", "signals": [{"name": "go", "when": "2h"}], "expect": {"status": "completed"}}"#,
        )
        .unwrap();
        assert_eq!(file.signals[0].when.as_deref(), Some("2h"));
        assert!(
            serde_json::from_str::<ScenarioFile>(r#"{"script": "a.ts", "steps": []}"#).is_err()
        );
    }

    #[test]
    fn statuses_map_one_to_one() {
        for status in [
            ApiStatus::Pending,
            ApiStatus::Active,
            ApiStatus::Completed,
            ApiStatus::Failed,
            ApiStatus::Cancelled,
            ApiStatus::Aborted,
        ] {
            assert_eq!(ApiStatus::from(core_status(status)), status);
        }
    }
}
