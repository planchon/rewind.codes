//! Tests (`archi/05-testing-and-observability.md`, "Testing and mocks"): `POST /v1/test`,
//! `POST /v1/workflows/{name}/record`, `GET /v1/tests/{digest}`, and the suite a fork of a run
//! of a workflow passes first.
//!
//! Running a test runs model-written code: only a server with the worker role does it. Test
//! documents are written in the stored format v1, which the engine is given here.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use rewind_api::ProblemType;
use rewind_api::access::Permission;
use rewind_api::scripts::TestRequest;
use rewind_api::testing::{
    Test as ApiTest, TestInput as ApiTestInput, TestProblem as ApiTestProblem, TestResult, TestRun,
};
use rewind_api::workflows::{RecordTests, RecordedTest, RecordedTests, SkippedRun};
use rewind_core::{Cassette, Digest, Script, Test, TestOrigin, Toolset, WorkflowSchemas};
use rewind_engine::ports::{JournalStore, ReadModel, RunRecord, StoredWorkflow, TenantId};
use rewind_engine::suites::{
    DEFAULT_RECORDED, MAX_RECORDED, MAX_RECORDING_CANDIDATES, Recorder, SuiteError, SuiteLoadError,
    TestFormat, TestInput,
};
use rewind_engine::testing::{Subject, TestError, TestOptions, TestProblem, TestReport, run_tests};
use rewind_runtime::TsMockLoader;
use serde_json::Value;

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::Body;
use crate::http::{search, toolsets, workflows};

/// A run of tests may not take longer than this, in wall-clock time.
const TESTS_TIMEOUT: Duration = Duration::from_secs(300);

/// The stored format of tests: v1.
pub(crate) fn format() -> TestFormat {
    TestFormat {
        encode: rewind_format::encode_test,
        decode: |document| rewind_format::decode_test(document).map_err(|error| error.to_string()),
    }
}

/// A cassette document, with or without its version.
pub(crate) fn decode_cassette(cassette: &Value) -> Result<Cassette, String> {
    let mut cassette = cassette.clone();
    if let Value::Object(fields) = &mut cassette
        && !fields.contains_key("v")
    {
        fields.insert("v".into(), Value::from(rewind_format::VERSION));
    }
    rewind_format::decode_cassette(&cassette.to_string()).map_err(|error| error.to_string())
}

pub(crate) fn encode_cassette(cassette: &Cassette) -> Value {
    serde_json::from_str(&rewind_format::encode_cassette(cassette)).unwrap_or(Value::Null)
}

/// A test the API was given, as the engine takes it. A test given inline cannot claim to be
/// recorded.
pub(crate) fn input(input: ApiTestInput) -> Result<TestInput, ApiError> {
    match input {
        ApiTestInput::Digest(digest) => Ok(TestInput::Stored(test_digest(&digest)?)),
        ApiTestInput::From(from) => Ok(TestInput::From {
            from: test_digest(&from.from)?,
            expect: from.expect.into_core(),
        }),
        ApiTestInput::Inline(test) => {
            if test.origin.is_some() {
                return Err(ApiError::new(
                    ProblemType::InvalidTest,
                    format!(
                        "test {:?}: `origin` is set by recording, never given: record the run \
                         with POST /v1/workflows/{{name}}/record and give its digest instead",
                        test.name
                    ),
                ));
            }
            test.into_core(decode_cassette)
                .map(|test| TestInput::Inline(Box::new(test)))
                .map_err(|message| ApiError::new(ProblemType::InvalidTest, message))
        }
    }
}

fn test_digest(text: &str) -> Result<Digest, ApiError> {
    Digest::from_hex(text).map_err(|_| {
        ApiError::new(
            ProblemType::InvalidTest,
            format!(
                "{text:?} is not a test's digest: 64 lowercase hex digits, as record_tests \
                 returns them"
            ),
        )
    })
}

/// A test's outcome, as the API shows it.
pub(crate) fn result(report: &TestReport) -> TestResult {
    TestResult {
        name: report.name.clone(),
        passed: report.passed,
        problem: report.problem.map(|problem| match problem {
            TestProblem::Stale => ApiTestProblem::Stale,
            TestProblem::Incompatible => ApiTestProblem::Incompatible,
            TestProblem::Failed => ApiTestProblem::Failed,
        }),
        failures: report.failures.clone(),
        status: report.status.map(Into::into),
        output: report.output.clone(),
        error: report.error.as_ref().map(Into::into),
        abort: report.abort.as_ref().map(Into::into),
        calls: report.calls.clone(),
        trace: report.trace.clone(),
    }
}

/// `tests-failed`: every outcome, the detail naming those that did not pass.
pub(crate) fn tests_failed(what: &str, reports: &[TestReport]) -> ApiError {
    let failed: Vec<String> = reports
        .iter()
        .filter(|report| !report.passed)
        .map(|report| {
            format!(
                "{:?} ({}): {}",
                report.name,
                report.problem.map_or("failed", TestProblem::as_str),
                report.failures.first().cloned().unwrap_or_default()
            )
        })
        .collect();
    ApiError(Box::new(rewind_api::Problem::tests_failed(
        format!(
            "{what}: {} of {} tests did not pass. {}. Fix the script, or retire a test that is \
             deprecated or update one whose state or result is out of date, each with a reason",
            failed.len(),
            reports.len(),
            failed.join("; ")
        ),
        reports.iter().map(result).collect(),
    )))
}

pub(crate) fn suite_error(error: SuiteLoadError) -> ApiError {
    match error {
        SuiteLoadError::Suite(SuiteError::ParentNotFound(version)) => ApiError::not_found(format!(
            "no workflow version {version} that this caller sees: name the parent as \
             name@<version>, or leave it out to derive from the latest version"
        )),
        SuiteLoadError::Suite(error) => ApiError::new(ProblemType::InvalidTest, error.to_string()),
        SuiteLoadError::Store(error) => error.into(),
        SuiteLoadError::Blob(error) => error.into(),
        corrupt @ SuiteLoadError::Corrupt { .. } => ApiError::internal(corrupt),
    }
}

pub(crate) fn test_error(error: TestError) -> ApiError {
    match error {
        TestError::Start(error) => error.into(),
        TestError::Engine(error) => error.into(),
        TestError::Mocks(message) => ApiError::internal(message),
    }
}

/// Refuses to run tests on a server without the worker role: only workers run model-written
/// code.
pub(crate) fn require_worker(app: &App, what: &str) -> Result<(), ApiError> {
    if app.settings.worker() {
        return Ok(());
    }
    Err(ApiError::unavailable(format!(
        "{what} runs tests, and this server has only the api role: send it to a server with the \
         worker role"
    )))
}

/// Runs tests against a subject, within the wall-clock bound.
pub(crate) async fn run(
    app: &App,
    subject: &Subject<'_>,
    tests: &[Test],
    options: &TestOptions,
) -> Result<Vec<TestReport>, ApiError> {
    let loader = TsMockLoader::default();
    let executor = app.engine.executor();
    match tokio::time::timeout(
        TESTS_TIMEOUT,
        run_tests(executor, &loader, subject, tests, options),
    )
    .await
    {
        Ok(reports) => reports.map_err(test_error),
        Err(_) => Err(ApiError::invalid(format!(
            "the tests took longer than {}s of wall-clock time",
            TESTS_TIMEOUT.as_secs()
        ))),
    }
}

/// The tests of a version's suite, those of an erased namespace left out.
pub(crate) async fn suite(
    app: &App,
    tenant: &TenantId,
    stored: &StoredWorkflow,
) -> Result<Vec<Test>, ApiError> {
    Ok(app
        .engine
        .load_suite(tenant, &stored.workflow, format())
        .await
        .map_err(suite_error)?
        .into_iter()
        .filter_map(|loaded| loaded.test)
        .collect())
}

async fn source(app: &App, tenant: &TenantId, digest: Digest) -> Result<Script, ApiError> {
    Ok(app
        .store
        .get_script(tenant, digest)
        .await?
        .ok_or_else(|| ApiError::internal(format!("script {digest} is missing")))?
        .script)
}

/// `POST /v1/test`: the given tests, and a workflow's suite, in this process, in virtual time.
pub(super) async fn test(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<TestRequest>,
) -> Result<Json<TestRun>, ApiError> {
    caller.require(Permission::RunsStart)?;
    require_worker(&app, "POST /v1/test")?;
    let bounds = super::scripts::bounds(&app, &caller).await?;
    let tenant = &caller.tenant;
    let version = match &request.workflow {
        Some(reference) => Some(workflows::resolve(&app, tenant, &caller.labels, reference).await?),
        None => None,
    };
    let script = match (request.script, &version) {
        (Some(source), _) => Script::new(source),
        (None, Some(version)) => source(&app, tenant, version.workflow.script()).await?,
        (None, None) => {
            return Err(ApiError::invalid(
                "give `script`, or a `workflow` whose script the tests run against",
            ));
        }
    };
    let toolset: Toolset = match (&request.toolset, &version) {
        (Some(id), _) => toolsets::stored(&app, tenant, id).await?.toolset,
        (None, Some(version)) => {
            toolsets::stored(&app, tenant, &version.workflow.toolset().to_hex())
                .await?
                .toolset
        }
        (None, None) => app.default_toolset(tenant).await?.toolset,
    };
    super::scripts::check_lines(&script, &bounds)?;
    let mut tests = Vec::with_capacity(request.tests.len());
    for given in request.tests {
        let given = input(given)?;
        tests.push(
            app.engine
                .resolve_test(tenant, &given, format())
                .await
                .map_err(suite_error)?,
        );
    }
    if let Some(version) = &version {
        tests.extend(suite(&app, tenant, version).await?);
    }
    let interface: Option<&WorkflowSchemas> =
        version.as_ref().map(|version| version.workflow.schemas());
    let options = TestOptions {
        ttl: bounds
            .max_ttl
            .map(|max| max.min(app.engine.config().default_ttl)),
        statics: None,
    };
    let reports = run(
        &app,
        &Subject {
            script: &script,
            toolset: &toolset,
            interface,
        },
        &tests,
        &options,
    )
    .await?;
    Ok(Json(TestRun {
        passed: reports.iter().all(|report| report.passed),
        tests: reports.iter().map(result).collect(),
    }))
}

/// `GET /v1/tests/{digest}`: a stored test. A recorded test holds its run's data: reading it
/// also needs `runs:read` in its namespace.
pub(super) async fn get(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(digest): Path<String>,
) -> Result<Json<ApiTest>, ApiError> {
    caller.require(Permission::ToolsetsRead)?;
    let not_found = || ApiError::not_found(format!("no test {digest} for this tenant"));
    let parsed = Digest::from_hex(&digest).map_err(|_| not_found())?;
    let record = app
        .store
        .get_test(&caller.tenant, parsed)
        .await?
        .ok_or_else(not_found)?;
    if let Some(namespace) = &record.namespace {
        caller.require(Permission::RunsRead)?;
        if !caller.reaches(namespace) {
            return Err(not_found());
        }
    }
    let test = app
        .engine
        .load_test(&caller.tenant, parsed, format())
        .await
        .map_err(suite_error)?
        .ok_or_else(not_found)?;
    Ok(Json(ApiTest::from_core(&test, encode_cassette)))
}

/// `POST /v1/workflows/{name}/record`: the workflow's completed and failed runs the caller
/// sees, newest first, one test per path.
pub(super) async fn record(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(name): Path<String>,
    Body(request): Body<RecordTests>,
) -> Result<Json<RecordedTests>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let tenant = &caller.tenant;
    if app
        .store
        .workflow_versions(tenant, &name, &caller.labels)
        .await?
        .is_empty()
    {
        return Err(ApiError::not_found(format!(
            "no workflow {name} for this caller: list them with GET /v1/workflows"
        )));
    }
    let limit = request
        .limit
        .map_or(DEFAULT_RECORDED, |limit| {
            usize::try_from(limit).unwrap_or(MAX_RECORDED)
        })
        .clamp(1, MAX_RECORDED);
    let candidates: Vec<RunRecord> = if request.runs.is_empty() {
        let mut query = search::run_query(&caller, request.query.as_deref(), None, app.now())?;
        query = query
            .with(rewind_engine::ports::Predicate::Workflow(vec![
                rewind_engine::ports::TextMatch::Exact(name.clone()),
            ]))
            .with(rewind_engine::ports::Predicate::Status(
                [
                    rewind_core::RunStatus::Completed,
                    rewind_core::RunStatus::Failed,
                ]
                .into_iter()
                .collect(),
            ));
        let found = app
            .store
            .search_runs(
                &query,
                rewind_engine::ports::RunOrder::Started,
                None,
                MAX_RECORDING_CANDIDATES,
            )
            .await?;
        let mut records = Vec::with_capacity(found.len());
        for found in found {
            if let Some(record) = app.store.get_run(&found.run).await?
                && caller.sees_run(&record)
            {
                records.push(record);
            }
        }
        records
    } else {
        let mut records = Vec::with_capacity(request.runs.len());
        for id in &request.runs {
            let record = app.run(&caller, id).await?;
            if record
                .metadata
                .workflow
                .as_ref()
                .map(|workflow| workflow.name.as_str())
                != Some(name.as_str())
            {
                return Err(ApiError::invalid(format!(
                    "run {id} is not a run of the workflow {name}"
                )));
            }
            records.push(record);
        }
        records
    };

    let views = app.views();
    let mut recorder = Recorder::new(limit, format());
    for record in &candidates {
        if recorder.is_full() {
            break;
        }
        let Some(workflow) = &record.metadata.workflow else {
            continue;
        };
        let mut history = views.history(record).await?;
        views.inline(&record.run.tenant, &mut history).await?;
        recorder.offer(
            &history,
            TestOrigin {
                run: record.run.id,
                version: workflow.version,
                namespace: record.namespace.to_string(),
            },
        );
    }
    let tests: Vec<Test> = recorder
        .recorded
        .iter()
        .map(|recorded| recorded.test.clone())
        .collect();
    let digests = app
        .engine
        .store_tests(tenant, &tests, format())
        .await
        .map_err(suite_error)?;
    Ok(Json(RecordedTests {
        tests: recorder
            .recorded
            .iter()
            .zip(digests)
            .map(|(recorded, digest)| RecordedTest {
                digest: digest.to_hex(),
                name: recorded.test.name.clone(),
                path: recorded.path.to_string(),
                run: recorded.run.to_string(),
                status: recorded.path.status.into(),
            })
            .collect(),
        skipped: recorder
            .skipped
            .iter()
            .map(|skipped| SkippedRun {
                run: skipped.run.to_string(),
                reason: skipped.reason.clone(),
            })
            .collect(),
    }))
}

/// Before a fork of a run of a workflow: its version's suite must pass against the corrected
/// script (`archi/05-testing-and-observability.md`, "Suites and the gate").
pub(crate) async fn gate_fork(
    app: &App,
    parent: &RunRecord,
    script: &Script,
) -> Result<(), ApiError> {
    let Some(workflow) = &parent.metadata.workflow else {
        return Ok(());
    };
    let tenant = &parent.run.tenant;
    let Some(stored) = app.store.get_workflow(tenant, workflow.version).await? else {
        return Ok(());
    };
    let tests = suite(app, tenant, &stored).await?;
    if tests.is_empty() {
        return Ok(());
    }
    require_worker(app, "a fork of a run of a workflow")?;
    let toolset = toolsets::stored(app, tenant, &stored.workflow.toolset().to_hex())
        .await?
        .toolset;
    let reports = run(
        app,
        &Subject {
            script,
            toolset: &toolset,
            interface: Some(stored.workflow.schemas()),
        },
        &tests,
        &TestOptions::default(),
    )
    .await?;
    if reports.iter().any(|report| !report.passed) {
        return Err(tests_failed(
            &format!(
                "the corrected script was not run: {} pins it to its suite",
                stored.workflow
            ),
            &reports,
        ));
    }
    Ok(())
}
