//! Scripts: type-checking (`POST /v1/check`) and scenarios in mock mode (`POST /v1/test`).

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use rewind_api::common::{AbortReason, RunStatus, ScriptError};
use rewind_api::scripts::{CheckRequest, CheckResult, Severity, TestRequest, TestResult};
use rewind_core::Script;
use rewind_engine::adapters::VirtualClock;
use rewind_engine::mock::{
    Expectations, ExpectedError, MockDispatcher, Mocks, Scenario, ScenarioError, ScenarioSignal,
    SignalWhen, TestReport, run_scenario,
};
use rewind_engine::ports::{ScriptExecutor, TypeChecker};
use rewind_runtime::{MockError, SandboxLimits, TsMocks};

use crate::app::App;
use crate::auth::{Access, Caller};
use crate::error::{ApiError, diagnostic, invalid_script_diagnostic};
use crate::extract::Body;
use crate::http::toolsets;

/// `POST /v1/check`: the checks at start, then tsgo and the checker's rules.
pub(super) async fn check(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<CheckRequest>,
) -> Result<Json<CheckResult>, ApiError> {
    caller.allow(Access::Write)?;
    let stored = match &request.toolset {
        Some(id) => toolsets::stored(&app, &caller.tenant, id).await?,
        None => app.default_toolset(&caller.tenant).await?,
    };
    let script = Script::new(request.script);
    if let Err(invalid) = app.engine.executor().compile(&script, &stored.toolset) {
        return Ok(Json(CheckResult {
            ok: false,
            diagnostics: vec![invalid_script_diagnostic(&invalid)],
        }));
    }
    let diagnostics: Vec<_> = app
        .checker
        .check(&script, &stored.declarations)
        .await?
        .iter()
        .map(diagnostic)
        .collect();
    Ok(Json(CheckResult {
        ok: !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error),
        diagnostics,
    }))
}

/// `POST /v1/test`: the scenario runs in this process, in virtual time, with in-memory adapters
/// and the mocks of the request. It runs the script: only a server with the worker role does.
pub(super) async fn test(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<TestRequest>,
) -> Result<Json<TestResult>, ApiError> {
    caller.allow(Access::Write)?;
    if !app.settings.worker() {
        return Err(ApiError::unavailable(
            "POST /v1/test runs the script, and this server has only the api role: send it to a \
             server with the worker role",
        ));
    }
    let stored = match &request.toolset {
        Some(id) => toolsets::stored(&app, &caller.tenant, id).await?,
        None => app.default_toolset(&caller.tenant).await?,
    };
    let mut scenario = Scenario::new(Script::new(request.script), stored.toolset, request.input);
    for signal in request.signals {
        let when = match signal.when.as_deref() {
            None | Some("blocked") => SignalWhen::Blocked,
            Some(after) => SignalWhen::After(
                after
                    .parse::<rewind_api::common::Duration>()
                    .map(Into::into)
                    .map_err(|_| {
                        ApiError::invalid(format!(
                            "signal {}: when must be \"blocked\" or a duration such as \"2h\", \
                             not {after:?}",
                            signal.name
                        ))
                    })?,
            ),
        };
        scenario.signals.push(ScenarioSignal {
            name: signal.name,
            payload: signal.payload,
            when,
        });
    }
    scenario.expect = Expectations {
        status: request.expect.status.map(super::runs_status),
        output: request.expect.output,
        error: request.expect.error.map(|error| ExpectedError {
            name: error.name,
            message: error.message,
        }),
        calls: request.expect.calls,
    };

    let mut dispatcher = MockDispatcher::new();
    if let Some(cassette) = request.cassette {
        let cassette = rewind_format::decode_cassette(&cassette.to_string())
            .map_err(|error| ApiError::invalid(format!("the cassette is not valid: {error}")))?;
        dispatcher = dispatcher.with_cassette(cassette);
    }
    if !request.tools.is_empty() {
        let tools = serde_json::Value::Object(request.tools.into_iter().collect());
        dispatcher = dispatcher
            .with_static_responses(&tools)
            .map_err(|error| ApiError::invalid(format!("tools: {error}")))?;
    }
    let executor = app.engine.executor();
    let report = match request.mocks {
        Some(source) => {
            let clock = Arc::new(VirtualClock::new(scenario.start));
            let mocks = TsMocks::new(&Script::new(source), clock, SandboxLimits::default())
                .await
                .map_err(|error| match error {
                    MockError::Invalid(message) => {
                        ApiError::invalid(format!("the mocks cannot load: {message}"))
                    }
                    MockError::Internal(message) => ApiError::internal(message),
                })?;
            let tools = mocks.tools().clone();
            let dispatcher = dispatcher.with_mocks(Mocks {
                handlers: mocks,
                tools,
            });
            run(executor, dispatcher, scenario).await?
        }
        None => run(executor, dispatcher, scenario).await?,
    };
    Ok(Json(TestResult {
        passed: report.passed,
        failures: report.failures,
        status: RunStatus::from(report.status),
        output: report.output,
        error: report.error.as_ref().map(ScriptError::from),
        abort: report.abort.as_ref().map(AbortReason::from),
        calls: report.calls,
        trace: report.trace,
    }))
}

/// A scenario may not take longer than this, in wall-clock time.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(120);

async fn run<X: ScriptExecutor, M: rewind_engine::ports::ToolDispatcher>(
    executor: &X,
    dispatcher: MockDispatcher<M>,
    scenario: Scenario,
) -> Result<TestReport, ApiError> {
    match tokio::time::timeout(
        SCENARIO_TIMEOUT,
        run_scenario(executor, dispatcher, scenario),
    )
    .await
    {
        Ok(Ok(report)) => Ok(report),
        Ok(Err(ScenarioError::Start(error))) => Err(error.into()),
        Ok(Err(ScenarioError::Engine(error))) => Err(error.into()),
        Err(_) => Err(ApiError::invalid(format!(
            "the scenario took longer than {}s of wall-clock time",
            SCENARIO_TIMEOUT.as_secs()
        ))),
    }
}
