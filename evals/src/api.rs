//! The world as the API `/v1` answers the agent tools: [`Api`] with the run plane in process
//! (`archi/08-decisions.md`, "Taken while building the evals").
//!
//! Each operation does what `rewind-server`'s handler does (`crates/rewind-server/src/http/`) at
//! the node's edge, its answers and problems repeated, for one caller: an API key of the tenant
//! `local`, acting in the namespace `local`, limited to the episode's labels. A run without them
//! is not found; the runs and the workflows it creates carry them; its runs' workflows reach only
//! runs carrying them. A wait is virtual time: the world is driven until the run ends or the
//! wait elapses. A signal or a cancellation is taken at once, at the current virtual time, as
//! the owner of the run's partition takes them.
//!
//! The world answers what the agent tools ask, and refuses with a problem what it cannot answer
//! as the server would: a run list filtered otherwise than by labels and status, a record query
//! other than a status, an action set registered otherwise than from a base.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration as StdDuration;

use rewind_agent_tools::{Api, ApiError};
use rewind_api::action_sets::{
    ActionMatch, ActionSearchResults, ActionSet, CreateActionSet, SearchQuery,
};
use rewind_api::common::{Duration, RunStatus as ApiRunStatus, timestamp};
use rewind_api::events::HistoryEvent;
use rewind_api::headers::IdempotencyKey;
use rewind_api::runs::{
    CancelRun, ForkRun, Run, RunFilter, RunPage, SignalAccepted, StartRun, Transcript,
};
use rewind_api::scripts::{CheckRequest, CheckResult, RunCounts, Script as ApiScript, TestRequest};
use rewind_api::secrets::SecretList;
use rewind_api::testing::{
    Test as ApiTest, TestInput as ApiTestInput, TestProblem as ApiTestProblem,
    TestReport as ApiTestReport, TestResult,
};
use rewind_api::workflows::{
    CreateWorkflow, RecordTests, RecordedTest, RecordedTests, SkippedRun, Workflow,
    WorkflowVersions,
};
use rewind_api::{Problem, ProblemType};
use rewind_core::{
    Cassette, Code, Command, Digest, Event, RunId, Test, Timestamp, WorkflowSchemas, canonical_json,
};
use rewind_engine::edge::{EdgeError, RecordingError};
use rewind_engine::ports::link::Refusal;
use rewind_engine::ports::{
    ActionDeclarations, CatalogError, CatalogStore, CheckerError, Enqueued, RunHead, RunRef,
    StoreError, StoredActionSet, StoredWorkflow, TenantId, TraceContext, TypeChecker,
    WorkflowDeclarations,
};
use rewind_engine::suites::{
    DEFAULT_RECORDED, MAX_RECORDED, MAX_RECORDING_CANDIDATES, ReplaceWith, SuiteChanges,
    SuiteError, SuiteLoadError, TestInput, TestReplacement, TestRetirement,
};
use rewind_engine::testing::{Subject, TestError, TestOptions, TestProblem, TestReport};
use rewind_engine::{ForkError, RegisterError, RegisterWorkflow, StartError};
use rewind_runtime::TsHandlerLoader;
use serde_json::Value;

use crate::view;
use crate::world::{World, WorldError, test_format};

/// Hex digits a reference to a version gives at least.
const MIN_VERSION_DIGITS: usize = 12;

/// The longest wait of a request, as the server bounds it.
const MAX_WAIT: StdDuration = StdDuration::from_secs(300);

/// Runs a page of `GET /v1/runs` holds by default, and at most.
const PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;

fn problem(kind: ProblemType, detail: impl Into<String>) -> ApiError {
    Problem::new(kind, detail).into()
}

fn invalid(detail: impl Into<String>) -> ApiError {
    problem(ProblemType::InvalidRequest, detail)
}

fn not_found(detail: impl Into<String>) -> ApiError {
    problem(ProblemType::NotFound, detail)
}

fn run_not_found(run: &str) -> ApiError {
    not_found(format!(
        "no run {run}: check the id or the handle (runs are listed by GET /v1/runs)"
    ))
}

/// What the world cannot answer as the server would: a fault, never a refusal.
fn fault(error: impl Into<WorldError>) -> ApiError {
    ApiError::fault(error.into())
}

/// What the edge answered, as the server answers it (`rewind-server`'s `error.rs`).
fn edge_error(error: EdgeError) -> ApiError {
    match error {
        EdgeError::Refused(Refusal::RunNotFound(run)) => run_not_found(&run.to_string()),
        EdgeError::Refused(Refusal::RunEnded(run)) => problem(
            ProblemType::RunEnded,
            format!("run {run} has ended: nothing can be sent to it any more"),
        ),
        EdgeError::Refused(Refusal::Signal(message)) => {
            problem(ProblemType::InvalidSignal, message)
        }
        EdgeError::Refused(Refusal::NamespaceDeleting) => problem(
            ProblemType::NamespaceDeleting,
            "this namespace is being erased: it takes no run",
        ),
        EdgeError::NulCharacter(error) => invalid(error.to_string()),
        other => fault(other),
    }
}

/// What a store answered: never the caller's fault.
fn store_error(error: StoreError) -> ApiError {
    fault(WorldError::Engine(error.to_string()))
}

fn catalog_error(error: CatalogError) -> ApiError {
    fault(error)
}

fn checker_error(error: &CheckerError) -> ApiError {
    problem(
        ProblemType::Unavailable,
        format!(
            "{error}. The code was neither accepted nor refused: retry, or start it with \
             `check: false`"
        ),
    )
}

/// A refused start, as the server answers it.
fn start_error(error: StartError) -> ApiError {
    match error {
        StartError::InvalidCode(invalid) => {
            let diagnostic = view::invalid_code_diagnostic(&invalid);
            Problem::new(
                ProblemType::InvalidCode,
                format!("the code cannot run: {}", invalid.message),
            )
            .with_diagnostics(vec![diagnostic])
            .into()
        }
        StartError::TypeErrors(diagnostics) => {
            let errors = diagnostics
                .iter()
                .filter(|d| d.severity == rewind_engine::ports::Severity::Error)
                .count();
            Problem::new(
                ProblemType::InvalidCode,
                format!(
                    "the code does not type-check ({errors} error{}): fix the diagnostics and \
                     start it again",
                    if errors == 1 { "" } else { "s" }
                ),
            )
            .with_diagnostics(diagnostics.iter().map(view::diagnostic).collect())
            .into()
        }
        StartError::InvalidTtl { .. } => invalid(error.to_string()),
        StartError::NulCharacter(error) => invalid(error.to_string()),
        StartError::InputTooLarge { .. } => {
            problem(ProblemType::PayloadTooLarge, error.to_string())
        }
        StartError::QuotaExceeded(_) => problem(
            ProblemType::QuotaExceeded,
            format!("{error}: wait for runs to end, or cancel some"),
        ),
        StartError::NamespaceDeleting => problem(
            ProblemType::NamespaceDeleting,
            "this namespace is being erased: it takes no run",
        ),
        StartError::CodeTooLong { .. } => problem(ProblemType::InvalidCode, error.to_string()),
        StartError::Checker(error) => checker_error(&error),
        StartError::Store(error) => store_error(error),
        StartError::WorkflowNotFound(version) => {
            not_found(format!("no workflow version {version} for this tenant"))
        }
        StartError::InvalidInput(message) => problem(ProblemType::InvalidInput, message),
        other => fault(WorldError::Engine(other.to_string())),
    }
}

fn fork_error(error: ForkError) -> ApiError {
    match error {
        ForkError::SourceNotFound(run) => run_not_found(&run.to_string()),
        ForkError::SourceNotStarted(_) => invalid(error.to_string()),
        ForkError::Start(error) => start_error(error),
        other => fault(WorldError::Engine(other.to_string())),
    }
}

/// A suite that cannot be loaded or composed, as the server answers it.
fn suite_error(error: SuiteLoadError) -> ApiError {
    match error {
        SuiteLoadError::Suite(SuiteError::BaseNotFound(version)) => not_found(format!(
            "no workflow version {version} that this caller sees: name the base as \
             name@<version>, or leave it out to derive from the latest version"
        )),
        SuiteLoadError::Suite(error) => problem(ProblemType::InvalidTest, error.to_string()),
        SuiteLoadError::Store(error) => store_error(error),
        other => fault(WorldError::Engine(other.to_string())),
    }
}

fn test_error(error: TestError) -> ApiError {
    match error {
        TestError::Start(error) => start_error(error),
        other => fault(WorldError::Engine(other.to_string())),
    }
}

/// A workflow refused at registration (`rewind-server`'s `workflows::register_error`).
fn register_error(error: RegisterError) -> ApiError {
    match error {
        RegisterError::Invalid(_) | RegisterError::Schema { .. } => {
            problem(ProblemType::InvalidWorkflow, error.to_string())
        }
        RegisterError::ActionSetNotFound(digest) => not_found(format!(
            "no action set {digest} for this tenant: register it with POST /v1/action-sets"
        )),
        RegisterError::InvalidCode(invalid) => start_error(StartError::InvalidCode(invalid)),
        RegisterError::TypeErrors(diagnostics) => start_error(StartError::TypeErrors(diagnostics)),
        RegisterError::NulCharacter(error) => invalid(error.to_string()),
        RegisterError::Suite(error) => suite_error(error.into()),
        RegisterError::TestsFailed(reports) => tests_failed("the suite", &reports),
        RegisterError::Tests(error) => test_error(error),
        RegisterError::Store(StoreError::Conflict(message)) => {
            problem(ProblemType::InvalidWorkflow, message)
        }
        RegisterError::Store(error) => store_error(error),
        other => fault(WorldError::Engine(other.to_string())),
    }
}

/// A recording refused, as the server answers it.
fn recording_error(error: RecordingError) -> ApiError {
    match error {
        RecordingError::RunNotFound(run) => run_not_found(&run.to_string()),
        RecordingError::NotOfWorkflow { .. } => invalid(error.to_string()),
        RecordingError::Edge(error) => edge_error(error),
        RecordingError::Suite(error) => suite_error(error),
    }
}

/// `tests-failed`: every outcome, the detail naming those that did not pass.
fn tests_failed(suite: &str, reports: &[TestReport]) -> ApiError {
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
    Problem::tests_failed(
        format!(
            "{} of {} tests of {suite} did not pass: {}. Fix the code, or retire a test that is \
             deprecated or replace one whose state or result is out of date, each with a reason",
            failed.len(),
            reports.len(),
            failed.join("; ")
        ),
        reports.iter().map(test_result).collect(),
    )
    .into()
}

/// A test's outcome, as the API shows it.
fn test_result(report: &TestReport) -> TestResult {
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
        transcript: report.transcript.clone(),
    }
}

/// A test the API was given, as the engine takes it. A test given inline cannot claim to be
/// recorded (`rewind-server`'s `testing::input`).
fn test_input(input: ApiTestInput) -> Result<TestInput, ApiError> {
    match input {
        ApiTestInput::Digest(digest) => Ok(TestInput::Stored(test_digest(&digest)?)),
        ApiTestInput::From(from) => Ok(TestInput::From {
            test: test_digest(&from.test)?,
            expect: from.expect.into_core(),
        }),
        ApiTestInput::Inline(test) => {
            if test.from.is_some() {
                return Err(problem(
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
                .map_err(|message| problem(ProblemType::InvalidTest, message))
        }
    }
}

fn test_digest(text: &str) -> Result<Digest, ApiError> {
    Digest::from_hex(text).map_err(|_| {
        problem(
            ProblemType::InvalidTest,
            format!(
                "{text:?} is not a test's digest: 64 lowercase hex digits, as record_tests \
                 returns them"
            ),
        )
    })
}

/// A cassette document, with or without its version.
pub fn decode_cassette(cassette: &Value) -> Result<Cassette, String> {
    let mut cassette = cassette.clone();
    if let Value::Object(fields) = &mut cassette
        && !fields.contains_key("v")
    {
        fields.insert("v".into(), Value::from(rewind_format::VERSION));
    }
    rewind_format::decode_cassette(&cassette.to_string()).map_err(|error| error.to_string())
}

fn encode_cassette(cassette: &Cassette) -> Value {
    serde_json::from_str(&rewind_format::encode_cassette(cassette)).unwrap_or(Value::Null)
}

fn workflow_not_found(reference: &str) -> ApiError {
    not_found(format!(
        "no workflow {reference} for this API key: save it with POST /v1/workflows, or list them \
         with GET /v1/workflows"
    ))
}

/// The labels of a run list's filters (`key:value`).
fn label_filters(filters: &[String]) -> Result<BTreeMap<String, String>, ApiError> {
    filters
        .iter()
        .map(|filter| {
            filter
                .split_once(':')
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .ok_or_else(|| invalid(format!("label {filter:?}: expected key:value")))
        })
        .collect()
}

/// The actions matching a query, best first, as `GET /v1/action-sets/{id}/search` ranks them: each
/// word of the query found in an action's name counts two, in its description one; the workflows
/// the action set pins as `workflows.<name>`. `None` when the query has no word.
pub fn search(stored: &StoredActionSet, query: &str, limit: usize) -> Option<Vec<ActionMatch>> {
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() {
        return None;
    }
    let generated = rewind_codegen::generate(&stored.action_set, &BTreeMap::new());
    let candidates: Vec<(String, String)> = stored
        .action_set
        .actions()
        .iter()
        .map(|action| {
            (
                action.name.clone(),
                action.description.clone().unwrap_or_default(),
            )
        })
        .chain(stored.action_set.workflows().iter().map(|workflow| {
            (
                format!("{}.{}", rewind_core::WORKFLOWS_GROUP, workflow.name),
                workflow.description.clone().unwrap_or_default(),
            )
        }))
        .collect();
    let mut scored: Vec<(usize, &(String, String))> = candidates
        .iter()
        .map(|candidate| {
            let name = candidate.0.to_lowercase();
            let description = candidate.1.to_lowercase();
            let score = words
                .iter()
                .map(|word| {
                    usize::from(name.contains(word.as_str())) * 2
                        + usize::from(description.contains(word.as_str()))
                })
                .sum();
            (score, candidate)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored
        .sort_by(|(a, action_a), (b, action_b)| b.cmp(a).then_with(|| action_a.0.cmp(&action_b.0)));
    Some(
        scored
            .into_iter()
            .take(limit)
            .map(|(_, (name, description))| {
                let declared = generated.actions.iter().find(|action| action.name == *name);
                ActionMatch {
                    name: name.clone(),
                    description: description.clone(),
                    prompt: declared.map(|d| d.prompt.clone()).unwrap_or_default(),
                    dts: declared.map(|d| d.typescript.clone()).unwrap_or_default(),
                }
            })
            .collect(),
    )
}

impl World {
    /// The run, if it exists and carries the caller's labels; otherwise 404, as the server
    /// answers a key limited to labels.
    async fn visible_run(&self, id: &str) -> Result<(RunRef, RunHead), ApiError> {
        let found = match (id.parse::<RunId>(), id.parse::<rewind_core::RunHandle>()) {
            (Ok(run_id), _) => self.run_view(&self.run_ref(run_id)).await,
            (_, Ok(handle)) => self.find_run(&handle).await,
            _ => return Err(run_not_found(id)),
        };
        match found {
            Ok(Some(view)) if self.sees(&view.head.metadata.labels) => {
                Ok((view.head.run.clone(), view.head))
            }
            Ok(_) | Err(EdgeError::Refused(Refusal::RunNotFound(_))) => Err(run_not_found(id)),
            Err(error) => Err(edge_error(error)),
        }
    }

    fn sees(&self, labels: &BTreeMap<String, String>) -> bool {
        self.labels
            .iter()
            .all(|(key, value)| labels.get(key) == Some(value))
    }

    /// The caller's labels added to those given: a contradicting one is refused.
    fn within(
        &self,
        mut labels: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, ApiError> {
        for (key, value) in &self.labels {
            match labels.get(key) {
                Some(given) if given != value => {
                    return Err(problem(
                        ProblemType::Forbidden,
                        format!(
                            "the API key only sees runs labelled {key}:{value}, not {key}:{given}"
                        ),
                    ));
                }
                Some(_) => {}
                None => {
                    labels.insert(key.clone(), value.clone());
                }
            }
        }
        Ok(labels)
    }

    /// An action set of the tenant by its id, or the default one (the built-in `webhook` only).
    async fn stored_action_set(&self, id: Option<&str>) -> Result<StoredActionSet, ApiError> {
        let tenant = TenantId::local();
        let Some(id) = id else {
            let action_set = rewind_core::ActionSet::new(
                [],
                [rewind_core::Action::new(
                    "signalUrl",
                    rewind_core::ActionRoute::Builtin(rewind_core::Builtin::SignalUrl),
                    rewind_runtime::builtins::signal_url_input_schema(),
                )],
            )
            .map_err(|error| fault(WorldError::ActionSet(error.to_string())))?;
            let declarations = self.register_action_set(&action_set).await.map_err(fault)?;
            return Ok(StoredActionSet {
                action_set,
                declarations: ActionDeclarations {
                    typescript: declarations.typescript,
                    prompt: declarations.prompt,
                },
            });
        };
        let digest = Digest::from_hex(id).map_err(|_| {
            not_found(format!(
                "no action set {id:?}: an action set's id is the digest POST /v1/action-sets returned"
            ))
        })?;
        self.catalog()
            .get_action_set(&tenant, digest)
            .await
            .map_err(catalog_error)?
            .ok_or_else(|| {
                not_found(format!(
                    "no action set {id} for this tenant: register it with POST /v1/action-sets"
                ))
            })
    }

    /// Drives the world until the run ends or `wait` has passed in virtual time; nothing
    /// without a wait, as the server answers at once.
    async fn wait_for_end(&self, run: &RunRef, wait: Option<Duration>) -> Result<(), ApiError> {
        let Some(wait) = wait else {
            return Ok(());
        };
        let ms = u64::try_from(wait.get().min(MAX_WAIT).as_millis()).unwrap_or(u64::MAX);
        let deadline = Timestamp(self.now().0.saturating_add(ms));
        self.drive(Some(run), Some(deadline)).await.map_err(fault)?;
        Ok(())
    }

    async fn view(&self, run: &RunRef) -> Result<Run, ApiError> {
        view::run(self, run).await.map_err(edge_error)
    }

    /// The version a reference names, among those the caller sees: `name` for the latest,
    /// `name@<version>` for one (`rewind-server`'s `workflows::resolve`).
    async fn resolve(&self, reference: &str) -> Result<StoredWorkflow, ApiError> {
        let (name, version) = match reference.split_once('@') {
            Some((name, version)) => (name, Some(version.to_ascii_lowercase())),
            None => (reference, None),
        };
        if let Some(version) = &version
            && (version.len() < MIN_VERSION_DIGITS
                || version.len() > 64
                || !version.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Err(invalid(format!(
                "workflow {reference:?}: name a version by its digest or its first \
                 {MIN_VERSION_DIGITS} hex digits at least, as in triage@9f2c4e1a07b3"
            )));
        }
        let versions = self.workflow_versions(name).await.map_err(fault)?;
        let found = match &version {
            None => versions.into_iter().next(),
            Some(prefix) => versions.into_iter().find(|stored| {
                stored
                    .workflow
                    .version()
                    .to_hex()
                    .starts_with(prefix.as_str())
            }),
        };
        found.ok_or_else(|| workflow_not_found(reference))
    }

    /// The tests of a version's suite, those of an erased namespace left out.
    async fn suite(&self, stored: &StoredWorkflow) -> Result<Vec<Test>, ApiError> {
        Ok(self
            .edge
            .suites(test_format())
            .load_suite(&TenantId::local(), &stored.workflow)
            .await
            .map_err(suite_error)?
            .into_iter()
            .filter_map(|loaded| loaded.test)
            .collect())
    }

    /// Runs tests against a subject on the node's executor, as the server does.
    async fn run_suite(
        &self,
        subject: &Subject<'_>,
        tests: &[Test],
    ) -> Result<Vec<TestReport>, ApiError> {
        let loader = TsHandlerLoader::default();
        self.edge
            .run_tests(&loader, subject, tests, &TestOptions::default())
            .await
            .map_err(test_error)
    }

    /// Before a fork of a run of a workflow: its version's suite must pass against the
    /// corrected code (`rewind-server`'s `testing::gate_fork`).
    async fn gate_fork(&self, parent: &RunHead, code: &Code) -> Result<(), ApiError> {
        let Some(of) = &parent.metadata.workflow else {
            return Ok(());
        };
        let tenant = &parent.run.tenant;
        let catalog = self.catalog();
        let Some(stored) = catalog
            .get_workflow(tenant, of.version)
            .await
            .map_err(catalog_error)?
        else {
            return Ok(());
        };
        let tests = self.suite(&stored).await?;
        if tests.is_empty() {
            return Ok(());
        }
        let action_set = catalog
            .get_action_set(tenant, stored.workflow.action_set())
            .await
            .map_err(catalog_error)?
            .ok_or_else(|| {
                fault(WorldError::ActionSet(format!(
                    "the action set of {} is not stored",
                    stored.workflow
                )))
            })?
            .action_set;
        let reports = self
            .run_suite(
                &Subject {
                    code,
                    action_set: &action_set,
                    interface: Some(stored.workflow.schemas()),
                },
                &tests,
            )
            .await?;
        if reports.iter().any(|report| !report.passed) {
            return Err(tests_failed(
                &format!("the suite of {}", stored.workflow),
                &reports,
            ));
        }
        Ok(())
    }
}

/// A traceparent in the W3C form, as the server checks it.
fn trace_context(
    traceparent: Option<&String>,
    tracestate: Option<&String>,
) -> Result<Option<TraceContext>, ApiError> {
    let Some(traceparent) = traceparent else {
        return Ok(None);
    };
    let parts: Vec<&str> = traceparent.split('-').collect();
    let hex =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_hexdigit());
    let valid = parts.len() == 4
        && hex(parts[0], 2)
        && hex(parts[1], 32)
        && hex(parts[2], 16)
        && hex(parts[3], 2);
    if !valid {
        return Err(invalid(format!(
            "traceparent {traceparent:?}: expected the W3C form 00-<trace id>-<span id>-<flags>"
        )));
    }
    Ok(Some(TraceContext {
        traceparent: traceparent.clone(),
        tracestate: tracestate.cloned(),
    }))
}

impl Api for World {
    async fn start_run(
        &self,
        start: &StartRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, ApiError> {
        if start.workflow.is_some() {
            return Err(invalid(
                "the evals' world starts a code with its action set, not a workflow by name: \
                 workflows start workflows from rewind:workflows",
            ));
        }
        if !start.secrets.is_empty() {
            return Err(invalid("the evals' world attaches no secrets"));
        }
        let labels = self.within(start.labels.0.clone())?;
        let stored = self.stored_action_set(start.action_set.as_deref()).await?;
        let mut request = rewind_engine::StartRun::new(
            Code::new(start.code.clone()),
            stored.action_set,
            start.input.clone(),
        );
        request.declarations = stored.declarations;
        request.ttl = start.ttl.map(Duration::get);
        request.metadata.labels = labels;
        request.metadata.trace_context =
            trace_context(start.traceparent.as_ref(), start.tracestate.as_ref())?;
        // What the run's workflows reach: the runs carrying the caller's labels.
        request.metadata.scope = self.labels.clone();
        request.idempotency_key = key.map(|key| key.as_str().to_owned());
        let created = self
            .start(request, start.check == Some(false))
            .await
            .map_err(start_error)?;
        self.wait_for_end(&created.run, start.wait).await?;
        self.view(&created.run).await
    }

    async fn get_run(&self, run: &str, wait: Option<Duration>) -> Result<Run, ApiError> {
        let (run, _) = self.visible_run(run).await?;
        self.wait_for_end(&run, wait).await?;
        self.view(&run).await
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<RunPage, ApiError> {
        let unsupported = filter.q.is_some()
            || filter.sort.is_some()
            || filter.action.is_some()
            || filter.script.is_some()
            || filter.error.is_some()
            || filter.quarantined.is_some()
            || filter.source.is_some()
            || filter.workflow.is_some()
            || filter.started_by.is_some()
            || filter.started_after.is_some()
            || filter.started_before.is_some();
        if unsupported {
            return Err(invalid(
                "the evals' world lists runs by label and status only",
            ));
        }
        let limit = usize::try_from(filter.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
        let offset = match &filter.cursor {
            Some(cursor) => cursor.parse::<usize>().map_err(|_| {
                invalid(format!("cursor {cursor:?}: give the nextCursor of a page"))
            })?,
            None => 0,
        };
        let wanted = label_filters(&filter.labels)?;
        let mut summaries = self.summaries().await.map_err(edge_error)?;
        summaries.retain(|summary| {
            self.sees(&summary.labels)
                && wanted
                    .iter()
                    .all(|(key, value)| summary.labels.get(key) == Some(value))
                && filter
                    .status
                    .is_none_or(|status| ApiRunStatus::from(summary.status) == status)
        });
        // Most recently started first.
        summaries.reverse();
        let more = summaries.len() > offset + limit;
        let mut runs = Vec::new();
        for summary in summaries.iter().skip(offset).take(limit) {
            let mut shown = view::summary(summary).map_err(edge_error)?;
            shown.script_title = view::script_title(self, &summary.run.tenant, summary.code)
                .await
                .map_err(edge_error)?
                .map(|(title, _, _)| title);
            runs.push(shown);
        }
        Ok(RunPage {
            runs,
            next_cursor: more.then(|| (offset + limit).to_string()),
        })
    }

    async fn run_transcript(&self, run: &str) -> Result<Transcript, ApiError> {
        let (run, _) = self.visible_run(run).await?;
        let text = self.transcript(&run).await.map_err(edge_error)?;
        Ok(Transcript {
            run_id: run.id.to_string(),
            text,
        })
    }

    async fn run_event(&self, run: &str, idx: u64) -> Result<HistoryEvent, ApiError> {
        let id = run;
        let (run, _) = self.visible_run(id).await?;
        let history = self.history(&run).await.map_err(edge_error)?;
        let Some(found) = history.iter().find(|entry| entry.index == idx) else {
            return Err(not_found(format!(
                "run {id} has no event {idx}: its events are numbered from 0 to {}",
                history.len().saturating_sub(1)
            )));
        };
        Ok(HistoryEvent::from_core(found, |payload| {
            match payload.value() {
                Some(value) => rewind_api::common::Payload::of(value.clone(), u64::MAX),
                None => rewind_api::common::Payload::reference(String::new(), 0, String::new()),
            }
        }))
    }

    async fn signal_run(
        &self,
        run: &str,
        name: &str,
        payload: &Value,
        key: Option<&IdempotencyKey>,
    ) -> Result<SignalAccepted, ApiError> {
        let (run, _) = self.visible_run(run).await?;
        let size = canonical_json(payload).len();
        let max = self.config.limits.max_payload_bytes;
        if size > max {
            return Err(problem(
                ProblemType::PayloadTooLarge,
                format!("the payload is {size} bytes as canonical JSON, above the limit of {max}"),
            ));
        }
        let enqueued = self
            .signal(
                &run,
                name,
                payload.clone(),
                key.map(|key| key.as_str().to_owned()),
            )
            .await
            .map_err(edge_error)?;
        let duplicate = match enqueued {
            Enqueued::Accepted => false,
            Enqueued::Duplicate => true,
            Enqueued::RunEnded => return Err(self.run_ended(&run, "a signal").await),
        };
        // The owner takes the signal at once.
        self.drive(None, Some(self.now())).await.map_err(fault)?;
        Ok(SignalAccepted {
            run_id: run.id.to_string(),
            name: name.to_owned(),
            duplicate,
        })
    }

    async fn cancel_run(&self, run: &str, cancel: &CancelRun) -> Result<Run, ApiError> {
        let (run, _) = self.visible_run(run).await?;
        let enqueued = self
            .cancel(&run, cancel.reason.clone())
            .await
            .map_err(edge_error)?;
        if enqueued == Enqueued::RunEnded {
            return Err(self.run_ended(&run, "a cancellation").await);
        }
        self.wait_for_end(&run, cancel.wait).await?;
        self.view(&run).await
    }

    async fn fork_run(
        &self,
        run: &str,
        fork: &ForkRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, ApiError> {
        let (parent, record) = self.visible_run(run).await?;
        let labels = fork
            .labels
            .as_ref()
            .map(|labels| self.within(labels.0.clone()))
            .transpose()?;
        let code = Code::new(fork.code.clone());
        self.gate_fork(&record, &code).await?;
        let mut request = rewind_engine::ForkRun::new(parent, code);
        request.at = fork.at;
        request.input.clone_from(&fork.input);
        request.ttl = fork.ttl.map(Duration::get);
        request.labels = labels;
        request.trace_context = trace_context(fork.traceparent.as_ref(), fork.tracestate.as_ref())?;
        request.idempotency_key = key.map(|key| key.as_str().to_owned());
        let created = self
            .fork(request, fork.check == Some(false))
            .await
            .map_err(fork_error)?;
        self.wait_for_end(&created.run, fork.wait).await?;
        self.view(&created.run).await
    }

    async fn check(&self, check: &CheckRequest) -> Result<CheckResult, ApiError> {
        use rewind_engine::ports::CodeExecutor as _;
        let stored = self.stored_action_set(check.action_set.as_deref()).await?;
        let code = Code::new(check.code.clone());
        if let Err(invalid) = self.plane.executor.compile(&code, &stored.action_set) {
            return Ok(CheckResult {
                ok: false,
                diagnostics: vec![view::invalid_code_diagnostic(&invalid)],
            });
        }
        let diagnostics = self
            .checker
            .check(&code, &stored.declarations)
            .await
            .map_err(|error| checker_error(&error))?;
        Ok(view::check_result(&diagnostics))
    }

    async fn test(&self, test: &TestRequest) -> Result<ApiTestReport, ApiError> {
        let tenant = TenantId::local();
        let version = match &test.workflow {
            Some(reference) => Some(self.resolve(reference).await?),
            None => None,
        };
        let Some(code) = &test.code else {
            return Err(invalid("the evals' world tests a code: give `code`"));
        };
        let code = Code::new(code.clone());
        // The action set given; the version's only when none is.
        let action_set = match (&test.action_set, &version) {
            (None, Some(version)) => {
                let digest = version.workflow.action_set().to_hex();
                self.stored_action_set(Some(&digest)).await?
            }
            (given, _) => self.stored_action_set(given.as_deref()).await?,
        };
        let suites = self.edge.suites(test_format());
        let mut tests = Vec::new();
        for given in test.tests.iter().cloned() {
            let given = test_input(given)?;
            tests.push(
                suites
                    .resolve_test(&tenant, &given)
                    .await
                    .map_err(suite_error)?,
            );
        }
        if let Some(version) = &version {
            tests.extend(self.suite(version).await?);
        }
        let interface: Option<&WorkflowSchemas> =
            version.as_ref().map(|version| version.workflow.schemas());
        let reports = self
            .run_suite(
                &Subject {
                    code: &code,
                    action_set: &action_set.action_set,
                    interface,
                },
                &tests,
            )
            .await?;
        let results: Vec<TestResult> = reports.iter().map(test_result).collect();
        Ok(ApiTestReport {
            passed: results.iter().all(|result| result.passed),
            tests: results,
        })
    }

    async fn get_script(&self, digest: &str) -> Result<ApiScript, ApiError> {
        let missing = || not_found(format!("no code {digest} for this tenant"));
        let parsed = Digest::from_hex(digest).map_err(|_| missing())?;
        let tenant = TenantId::local();
        let stored = self
            .catalog()
            .get_code(&tenant, parsed)
            .await
            .map_err(catalog_error)?
            .ok_or_else(missing)?;
        // What the caller's runs of the code did.
        let summaries = self.summaries().await.map_err(edge_error)?;
        let mut runs = RunCounts::default();
        let mut actions = BTreeSet::new();
        let mut first = None;
        let mut last = None;
        for summary in summaries
            .iter()
            .filter(|summary| summary.code == parsed && self.sees(&summary.labels))
        {
            let count = match ApiRunStatus::from(summary.status) {
                ApiRunStatus::Pending => &mut runs.pending,
                ApiRunStatus::Active => &mut runs.active,
                ApiRunStatus::Completed => &mut runs.completed,
                ApiRunStatus::Failed => &mut runs.failed,
                ApiRunStatus::Cancelled => &mut runs.cancelled,
                ApiRunStatus::Aborted => &mut runs.aborted,
            };
            *count += 1;
            let started_at = summary.started_at.unwrap_or(summary.last_activity_at);
            first = first.min(Some(started_at)).or(Some(started_at));
            last = last.max(Some(started_at));
            for entry in self.history(&summary.run).await.map_err(edge_error)? {
                if let Event::Command(Command::ScheduleCall { action, .. }) = &entry.event {
                    actions.insert(action.clone());
                }
            }
        }
        let source = stored.code.source().to_owned();
        let title = crate::view::script_title(self, &tenant, parsed)
            .await
            .map_err(edge_error)?;
        let (title, description, title_source) = match title {
            Some((title, description, source)) => (Some(title), description, Some(source)),
            None => (None, None, None),
        };
        Ok(ApiScript {
            digest: parsed.to_hex(),
            title,
            description,
            title_source,
            headline: rewind_api::scripts::headline(&source),
            source,
            runs,
            actions: actions.into_iter().collect(),
            first_run_at: first.map(timestamp),
            last_run_at: last.map(timestamp),
        })
    }

    async fn get_payload(&self, digest: &str, id: &str) -> Result<Value, ApiError> {
        let missing = || not_found(format!("no payload {digest} for this tenant"));
        let parsed = Digest::from_hex(digest).map_err(|_| missing())?;
        let (run, _) = self.visible_run(id).await?;
        let history = self.history(&run).await.map_err(edge_error)?;
        history
            .iter()
            .filter_map(|entry| view::payload_of(&entry.event))
            .find(|payload| payload.digest() == parsed)
            .and_then(|payload| payload.value().cloned())
            .ok_or_else(|| not_found(format!("no payload {digest} in the history of run {id}")))
    }

    async fn get_test(&self, digest: &str) -> Result<ApiTest, ApiError> {
        let missing = || not_found(format!("no test {digest} for this tenant"));
        let parsed = Digest::from_hex(digest).map_err(|_| missing())?;
        let loaded = self
            .edge
            .suites(test_format())
            .load_test(&TenantId::local(), parsed)
            .await
            .map_err(suite_error)?
            .ok_or_else(missing)?;
        Ok(ApiTest::from_core(&loaded, encode_cassette))
    }

    async fn create_action_set(&self, action_set: &CreateActionSet) -> Result<ActionSet, ApiError> {
        let Some(base) = &action_set.base else {
            return Err(problem(
                ProblemType::InvalidActionSet,
                "the evals' world derives action sets from a base only: its fake servers are \
                 registered when it starts",
            ));
        };
        if !action_set.mcp.is_empty() || !action_set.harness.is_empty() || action_set.http.is_some()
        {
            return Err(problem(
                ProblemType::InvalidActionSet,
                "an action set derived from a base keeps its servers and actions: give only workflows \
                 with base",
            ));
        }
        if !action_set.secrets.is_empty() || !action_set.apis.is_empty() {
            return Err(invalid(
                "the evals' world takes no secrets and no APIs: its servers are MCP servers",
            ));
        }
        let base = self.stored_action_set(Some(base)).await?.action_set;
        let mut pinned: BTreeMap<String, rewind_core::PinnedWorkflow> = base
            .workflows()
            .iter()
            .map(|workflow| (workflow.name.clone(), workflow.clone()))
            .collect();
        for reference in &action_set.workflows {
            let stored = self.resolve(reference).await?;
            let workflow = stored.workflow.pinned();
            pinned.insert(workflow.name.clone(), workflow);
        }
        let derived = base
            .with_workflows(pinned.into_values())
            .map_err(|error| problem(ProblemType::InvalidActionSet, error.to_string()))?;
        let declarations = self.register_action_set(&derived).await.map_err(fault)?;
        Ok(view::action_set(&StoredActionSet {
            action_set: derived,
            declarations: ActionDeclarations {
                typescript: declarations.typescript,
                prompt: declarations.prompt,
            },
        }))
    }

    async fn search_actions(
        &self,
        action_set: &str,
        query: &SearchQuery,
    ) -> Result<ActionSearchResults, ApiError> {
        let stored = self.stored_action_set(Some(action_set)).await?;
        let limit = usize::try_from(query.limit.unwrap_or(10).clamp(1, 100)).unwrap_or(10);
        let actions = search(&stored, &query.query, limit)
            .ok_or_else(|| invalid("query: give at least one word to look for"))?;
        Ok(ActionSearchResults { actions })
    }

    async fn create_workflow(&self, workflow: &CreateWorkflow) -> Result<Workflow, ApiError> {
        let action_set = Digest::from_hex(&workflow.action_set).map_err(|_| {
            not_found(format!(
                "no action set {:?}: an action set's id is the digest POST /v1/action-sets returned",
                workflow.action_set
            ))
        })?;
        let parent = match &workflow.base {
            Some(reference) => {
                let reference = if reference.contains('@') {
                    reference.clone()
                } else {
                    format!("{}@{reference}", workflow.name)
                };
                Some(self.resolve(&reference).await?.workflow.version())
            }
            None => None,
        };
        let mut tests = Vec::with_capacity(workflow.tests.len());
        for given in workflow.tests.iter().cloned() {
            tests.push(test_input(given)?);
        }
        let mut replace = Vec::with_capacity(workflow.replace.len());
        for given in workflow.replace.iter().cloned() {
            let with = match (given.new, given.expect) {
                (Some(test), None) => ReplaceWith::Test(test_input(test)?),
                (None, Some(expect)) => ReplaceWith::Expect(expect.into_core()),
                _ => {
                    return Err(problem(
                        ProblemType::InvalidTest,
                        format!(
                            "the replacement of {:?} gives `new` or `expect`: one of them",
                            given.name
                        ),
                    ));
                }
            };
            replace.push(TestReplacement {
                name: given.name,
                reason: given.reason,
                with,
            });
        }
        let retire = workflow
            .retire
            .iter()
            .map(|retire| TestRetirement {
                names: retire.names.clone(),
                reason: retire.reason.clone(),
            })
            .collect();
        let schemas = WorkflowSchemas {
            input: workflow.input.clone(),
            output: workflow.output.clone(),
            signals: workflow.signals.clone(),
        };
        let generated = rewind_codegen::workflow_declarations(&workflow.name, &schemas);
        let loader = TsHandlerLoader::default();
        let saved = self
            .edge
            .register_workflow(
                RegisterWorkflow {
                    tenant: TenantId::local(),
                    name: workflow.name.clone(),
                    description: workflow.description.clone(),
                    code: Code::new(workflow.code.clone()),
                    action_set,
                    schemas,
                    labels: self.labels.clone(),
                    secrets: Default::default(),
                    attach: rewind_engine::attachments::AttachRights::Organization {
                        labels: Default::default(),
                    },
                    declarations: WorkflowDeclarations {
                        signals: generated.signals,
                        entry: generated.entry,
                    },
                    suite: SuiteChanges {
                        base: parent,
                        tests,
                        replace,
                        retire,
                    },
                },
                &*self.checker,
                &self.test_kit(&loader),
            )
            .await
            .map_err(register_error)?;
        Ok(view::workflow(&saved))
    }

    async fn workflow_versions(&self, name: &str) -> Result<WorkflowVersions, ApiError> {
        let versions = World::workflow_versions(self, name).await.map_err(fault)?;
        if versions.is_empty() {
            return Err(workflow_not_found(name));
        }
        Ok(WorkflowVersions {
            name: name.to_owned(),
            versions: versions.iter().map(view::workflow).collect(),
        })
    }

    async fn record_tests(
        &self,
        workflow: &str,
        record: &RecordTests,
    ) -> Result<RecordedTests, ApiError> {
        if World::workflow_versions(self, workflow)
            .await
            .map_err(fault)?
            .is_empty()
        {
            return Err(not_found(format!(
                "no workflow {workflow} for this caller: list them with GET /v1/workflows"
            )));
        }
        let limit = record
            .limit
            .map_or(DEFAULT_RECORDED, |limit| {
                usize::try_from(limit).unwrap_or(MAX_RECORDED)
            })
            .clamp(1, MAX_RECORDED);
        let candidates: Vec<RunRef> = if record.runs.is_empty() {
            // The runs the caller sees, newest first, of the workflow, completed or failed,
            // and those the query selects.
            let statuses = match record.query.as_deref().map(str::trim) {
                None | Some("") => vec![
                    rewind_core::RunStatus::Completed,
                    rewind_core::RunStatus::Failed,
                ],
                Some("status:completed") => vec![rewind_core::RunStatus::Completed],
                Some("status:failed") => vec![rewind_core::RunStatus::Failed],
                Some(other) => {
                    return Err(problem(
                        ProblemType::InvalidQuery,
                        format!(
                            "the evals' world reads only the queries \"status:completed\" and \
                             \"status:failed\", not {other:?}: name the runs in `runs` instead"
                        ),
                    ));
                }
            };
            let mut summaries = self.summaries().await.map_err(edge_error)?;
            summaries.reverse();
            summaries
                .into_iter()
                .filter(|summary| self.sees(&summary.labels))
                .filter(|summary| statuses.contains(&summary.status))
                .filter(|summary| summary.workflow.as_deref() == Some(workflow))
                .take(MAX_RECORDING_CANDIDATES)
                .map(|summary| summary.run)
                .collect()
        } else {
            let mut candidates = Vec::with_capacity(record.runs.len());
            for id in &record.runs {
                let (run, found) = self.visible_run(id).await?;
                if found.metadata.workflow.as_ref().map(|of| of.name.as_str()) != Some(workflow) {
                    return Err(invalid(format!(
                        "run {id} is not a run of the workflow {workflow}"
                    )));
                }
                candidates.push(run);
            }
            candidates
        };

        let recording = self
            .record_runs(workflow, candidates, limit)
            .await
            .map_err(recording_error)?;
        Ok(RecordedTests {
            tests: recording
                .recorded
                .iter()
                .zip(recording.digests)
                .map(|(recorded, digest)| RecordedTest {
                    digest: digest.to_hex(),
                    name: recorded.test.name.clone(),
                    path: recorded.path.to_string(),
                    run: recorded.run.to_string(),
                    status: recorded.path.status.into(),
                })
                .collect(),
            skipped: recording
                .skipped
                .iter()
                .map(|skipped| SkippedRun {
                    run: skipped.run.to_string(),
                    reason: skipped.reason.clone(),
                })
                .collect(),
        })
    }

    /// The evals' world keeps no secret: its servers take none.
    async fn list_secrets(&self) -> Result<SecretList, ApiError> {
        Ok(SecretList {
            secrets: Vec::new(),
        })
    }

    async fn namespace_secrets(&self, _namespace: &str) -> Result<SecretList, ApiError> {
        Ok(SecretList {
            secrets: Vec::new(),
        })
    }
}

impl World {
    /// A run that has ended refuses a signal or a cancellation, as the server says it.
    async fn run_ended(&self, run: &RunRef, what: &str) -> ApiError {
        let status = match self.run_view(run).await {
            Ok(Some(view)) => view.head.status.as_str(),
            _ => "ended",
        };
        problem(
            ProblemType::RunEnded,
            format!(
                "run {} has ended ({status}): {what} can only be sent to a run that is still active",
                run.id
            ),
        )
    }
}
