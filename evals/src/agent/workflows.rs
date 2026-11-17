//! The agent tools of tests and workflows: `test_script`, `save_workflow`, `get_workflow`,
//! `record_tests`, and the gate of `retry_run`, as the SDK handles them, with what the server
//! does (`crates/rewind-server/src/http/testing.rs`, `workflows.rs`) done by the engine in
//! process, its answers and messages repeated.

use std::collections::BTreeMap;

use rewind_api::testing::{
    Test as ApiTest, TestInput as ApiTestInput, TestProblem as ApiTestProblem, TestResult,
};
use rewind_api::workflows::{
    RecordedTest, RecordedTests, SkippedRun, TestRetirement as ApiTestRetirement,
    TestUpdate as ApiTestUpdate,
};
use rewind_core::{Cassette, Digest, Script, Test, TestOrigin, WorkflowSchemas};
use rewind_engine::ports::{JournalStore, RunRef, StoredWorkflow, TenantId, WorkflowDeclarations};
use rewind_engine::suites::{
    DEFAULT_RECORDED, MAX_RECORDED, MAX_RECORDING_CANDIDATES, Recorder, SuiteChanges, SuiteError,
    SuiteLoadError, TestInput, TestRetirement, TestUpdate, UpdateWith,
};
use rewind_engine::testing::{Subject, TestError, TestOptions, TestProblem, TestReport, run_tests};
use rewind_engine::{EngineError, RegisterError, RegisterWorkflow, StartError};
use rewind_runtime::TsMockLoader;
use serde_json::{Map, Value};

use super::render::{
    SuiteEntry, WorkflowDetails, describe_recorded, describe_save_refused, describe_test_results,
    describe_workflow, describe_workflow_details, describe_workflow_refusal, truncate_end,
};
use super::{
    AgentError, AgentToolResult, AgentTools, AgentToolset, Failure, Handled, is_object,
    optional_string, optional_strings, require_object, require_string, result, start_failure,
};
use crate::view;
use crate::world::test_format;

/// Hex digits a reference to a version gives at least.
const MIN_VERSION_DIGITS: usize = 12;

impl AgentTools<'_> {
    /// `POST /v1/test`: the tests given, and a workflow's suite, against the script and the
    /// agent's toolset, in virtual time.
    pub(super) async fn test_script(&self, input: &Map<String, Value>) -> Handled {
        let code = require_string(input, "code", "code")?;
        let given = optional_tests(input, "tests")?;
        let workflow = optional_string(input, "workflow")?;
        if given.is_none() && workflow.is_none() {
            return Err(Failure::Input(
                "give `tests`, or a `workflow` whose suite runs against the script, or both."
                    .into(),
            ));
        }
        let tests_given = given.as_ref().map_or(0, Vec::len);
        let reports = match self.run_test_request(code, given, workflow).await {
            Ok(reports) => reports,
            Err(Failure::InvalidTest(detail)) => {
                return Ok(AgentToolResult {
                    tests_given,
                    ..result(
                        format!(
                            "The tests were refused before running: {}",
                            truncate_end(&detail, self.limits.value)
                        ),
                        true,
                    )
                });
            }
            Err(failure) => return Err(failure),
        };
        let results: Vec<TestResult> = reports.iter().map(test_result).collect();
        let passed = results.iter().all(|result| result.passed);
        Ok(AgentToolResult {
            tests_given,
            ..result(describe_test_results(&results, &self.limits), !passed)
        })
    }

    async fn run_test_request(
        &self,
        code: String,
        given: Option<Vec<Value>>,
        workflow: Option<String>,
    ) -> Result<Vec<TestReport>, Failure> {
        let tenant = TenantId::local();
        // The body is read first.
        let given = given
            .unwrap_or_default()
            .into_iter()
            .map(parse_test)
            .collect::<Result<Vec<_>, _>>()?;
        let version = match &workflow {
            Some(reference) => Some(self.resolve(reference).await?),
            None => None,
        };
        let script = Script::new(code);
        // The SDK sends the agent's toolset: the version's is used only when none is given.
        let current = self.toolset();
        let mut tests = Vec::new();
        for given in given {
            let given = test_input(given)?;
            tests.push(
                self.world
                    .engine
                    .resolve_test(&tenant, &given, test_format())
                    .await
                    .map_err(suite_error)?,
            );
        }
        if let Some(version) = &version {
            tests.extend(self.suite(version).await?);
        }
        let interface: Option<&WorkflowSchemas> =
            version.as_ref().map(|version| version.workflow.schemas());
        self.run(
            &Subject {
                script: &script,
                toolset: &current.toolset,
                interface,
            },
            &tests,
        )
        .await
    }

    /// `POST /v1/workflows` with the agent's toolset, then a toolset derived from it that pins
    /// the version: the later calls use it.
    pub(super) async fn save_workflow(&self, input: &Map<String, Value>) -> Handled {
        let name = require_string(input, "name", "name")?;
        let description = require_string(input, "description", "description")?;
        let code = require_string(input, "code", "code")?;
        let schema_input = require_object(input, "input")?.clone();
        let schema_output = require_object(input, "output")?.clone();
        let signals = match input.get("signals") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(Value::Object(signals)) => signals
                .iter()
                .map(|(name, schema)| (name.clone(), schema.clone()))
                .collect(),
            Some(_) => {
                return Err(Failure::Input(
                    "`signals` must be an object of JSON Schemas by signal name.".into(),
                ));
            }
        };
        let tests = optional_tests(input, "tests")?;
        let update = optional_updates(input)?;
        let retire = optional_retirements(input)?;
        let parent = optional_string(input, "parent")?;
        let tests_given = tests.as_ref().map_or(0, Vec::len) + update.as_ref().map_or(0, Vec::len);
        let current = self.toolset();
        let request = SaveRequest {
            name: name.clone(),
            description,
            code,
            schemas: WorkflowSchemas {
                input: schema_input,
                output: schema_output,
                signals,
            },
            tests: tests.unwrap_or_default(),
            update: update.unwrap_or_default(),
            retire: retire.unwrap_or_default(),
            parent,
        };
        let saved = match self.register(request, &current).await {
            Ok(saved) => saved,
            Err(Failure::Refused {
                detail,
                diagnostics,
            }) => {
                return Ok(AgentToolResult {
                    tests_given,
                    script: Some(super::ScriptVerdict::Refused),
                    ..result(
                        describe_workflow_refusal(&detail, &diagnostics, &self.limits),
                        true,
                    )
                });
            }
            Err(Failure::InvalidWorkflow(detail) | Failure::InvalidTest(detail)) => {
                return Ok(AgentToolResult {
                    tests_given,
                    ..result(describe_workflow_refusal(&detail, &[], &self.limits), true)
                });
            }
            Err(Failure::TestsFailed { detail, tests }) => {
                return Ok(AgentToolResult {
                    tests_given,
                    gate_refused: true,
                    ..result(
                        describe_save_refused(&detail, &tests, &name, &self.limits),
                        true,
                    )
                });
            }
            Err(failure) => return Err(failure),
        };
        // `POST /v1/toolsets { base, workflows: [name@version] }`.
        let mut pinned: BTreeMap<String, rewind_core::PinnedWorkflow> = current
            .toolset
            .workflows()
            .iter()
            .map(|workflow| (workflow.name.clone(), workflow.clone()))
            .collect();
        let workflow = saved.workflow.pinned();
        pinned.insert(workflow.name.clone(), workflow);
        let derived = match current.toolset.clone().with_workflows(pinned.into_values()) {
            Ok(derived) => derived,
            Err(error) => {
                return Ok(AgentToolResult {
                    tests_given,
                    ..result(
                        format!(
                            "The workflow {} was saved, but your scripts cannot start it yet: adding it to your tools failed: {}",
                            saved.workflow.name(),
                            truncate_end(&error.to_string(), self.limits.value)
                        ),
                        true,
                    )
                });
            }
        };
        let declarations = self.world.register_toolset(&derived).await?;
        *crate::fake::lock(&self.current) = std::sync::Arc::new(AgentToolset {
            toolset: derived,
            declarations,
        });
        Ok(AgentToolResult {
            tests_given,
            ..result(
                describe_workflow(&view::workflow(&saved), &self.limits),
                false,
            )
        })
    }

    /// The checks and the gate of `POST /v1/workflows`.
    async fn register(
        &self,
        request: SaveRequest,
        current: &AgentToolset,
    ) -> Result<StoredWorkflow, Failure> {
        // The body is read first.
        let given_tests = request
            .tests
            .into_iter()
            .map(parse_test)
            .collect::<Result<Vec<_>, _>>()?;
        let given_updates = request
            .update
            .into_iter()
            .map(serde_json::from_value::<ApiTestUpdate>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(body_invalid)?;
        let retire: Vec<TestRetirement> = request
            .retire
            .into_iter()
            .map(serde_json::from_value::<ApiTestRetirement>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(body_invalid)?
            .into_iter()
            .map(|retire| TestRetirement {
                names: retire.names,
                reason: retire.reason,
            })
            .collect();
        let parent = match &request.parent {
            Some(reference) => {
                let reference = if reference.contains('@') {
                    reference.clone()
                } else {
                    format!("{}@{reference}", request.name)
                };
                Some(self.resolve(&reference).await?.workflow.version())
            }
            None => None,
        };
        let mut tests = Vec::with_capacity(given_tests.len());
        for given in given_tests {
            tests.push(test_input(given)?);
        }
        let mut update = Vec::with_capacity(given_updates.len());
        for given in given_updates {
            let with = match (given.test, given.expect) {
                (Some(test), None) => UpdateWith::Test(test_input(test)?),
                (None, Some(expect)) => UpdateWith::Expect(expect.into_core()),
                _ => {
                    return Err(Failure::InvalidTest(format!(
                        "the update of {:?} gives `test` or `expect`: one of them",
                        given.name
                    )));
                }
            };
            update.push(TestUpdate {
                name: given.name,
                reason: given.reason,
                with,
            });
        }
        let generated = rewind_codegen::workflow_declarations(&request.name, &request.schemas);
        let loader = TsMockLoader::default();
        self.world
            .engine
            .register_workflow(
                RegisterWorkflow {
                    tenant: TenantId::local(),
                    name: request.name,
                    description: Some(request.description),
                    script: Script::new(request.code),
                    toolset: current.toolset.digest(),
                    schemas: request.schemas,
                    labels: self.world.labels.clone(),
                    declarations: WorkflowDeclarations {
                        signals: generated.signals,
                        entry: generated.entry,
                    },
                    suite: SuiteChanges {
                        parent,
                        tests,
                        update,
                        retire,
                    },
                },
                &*self.world.checker,
                &self.world.test_kit(&loader),
            )
            .await
            .map_err(register_error)
    }

    /// `GET /v1/workflows/{name}`, the latest version's script, and each test of its suite.
    pub(super) async fn get_workflow(&self, input: &Map<String, Value>) -> Handled {
        let name = require_string(input, "name", "name")?;
        let tenant = TenantId::local();
        let versions = self.world.workflow_versions(&name).await?;
        let Some(latest) = versions.first() else {
            return Err(Failure::Problem(not_found(&name)));
        };
        let source = self
            .world
            .engine
            .journal()
            .get_script(&tenant, latest.workflow.script())
            .await
            .map_err(EngineError::from)?
            .map(|stored| stored.script.source().to_owned())
            .ok_or_else(|| {
                Failure::Platform(AgentError::Checker(format!(
                    "the script of {} is not stored",
                    latest.workflow
                )))
            })?;
        let mut tests = Vec::new();
        for entry in &latest.workflow.suite().tests {
            let loaded = self
                .world
                .engine
                .load_test(&tenant, entry.test, test_format())
                .await
                .map_err(suite_error)?;
            // The agent reads every run of its namespace: a test it cannot read is one whose
            // namespace was erased since.
            tests.push(SuiteEntry {
                name: entry.name.clone(),
                digest: entry.test.to_hex(),
                unreadable: loaded
                    .is_none()
                    .then(|| format!("no test {} for this tenant", entry.test)),
                test: loaded.map(|test| ApiTest::from_core(&test, encode_cassette)),
            });
        }
        let details = WorkflowDetails {
            versions: versions.iter().map(view::workflow).collect(),
            source,
            tests,
        };
        Ok(result(
            describe_workflow_details(&details, &self.limits),
            false,
        ))
    }

    /// `POST /v1/workflows/{name}/record`: the workflow's completed and failed runs the agent
    /// sees, newest first, one test per path.
    pub(super) async fn record_tests(&self, input: &Map<String, Value>) -> Handled {
        let workflow = require_string(input, "workflow", "workflow")?;
        let query = optional_string(input, "query")?;
        let runs = optional_strings(input, "runs")?;
        let limit = match input.get("limit") {
            None | Some(Value::Null) => None,
            Some(value) => {
                Some(value.as_u64().filter(|limit| *limit >= 1).ok_or_else(|| {
                    Failure::Input("`limit` must be an integer, 1 or more.".into())
                })?)
            }
        };
        let tenant = TenantId::local();
        if self.world.workflow_versions(&workflow).await?.is_empty() {
            return Err(Failure::Problem(format!(
                "no workflow {workflow} for this caller: list them with GET /v1/workflows"
            )));
        }
        let limit = limit
            .map_or(DEFAULT_RECORDED, |limit| {
                usize::try_from(limit).unwrap_or(MAX_RECORDED)
            })
            .clamp(1, MAX_RECORDED);
        let runs = runs.unwrap_or_default();
        let candidates: Vec<RunRef> = if runs.is_empty() {
            // The evals project no read model: the runs `list_runs` finds, newest first, of
            // the workflow, completed or failed, and those the query selects.
            let statuses = match query.as_deref().map(str::trim) {
                None | Some("") => vec![
                    rewind_core::RunStatus::Completed,
                    rewind_core::RunStatus::Failed,
                ],
                Some("status:completed") => vec![rewind_core::RunStatus::Completed],
                Some("status:failed") => vec![rewind_core::RunStatus::Failed],
                Some(other) => {
                    return Err(Failure::Problem(format!(
                        "the evals' world reads only the queries \"status:completed\" and \
                         \"status:failed\", not {other:?}: name the runs in `runs` instead"
                    )));
                }
            };
            let mut filter = rewind_engine::ports::RunFilter::new(tenant.clone());
            filter.labels = self.world.labels.clone();
            let mut records = self.world.engine.list_runs(&filter).await?;
            records.reverse();
            records
                .into_iter()
                .filter(|record| statuses.contains(&record.status))
                .filter(|record| {
                    record
                        .metadata
                        .workflow
                        .as_ref()
                        .is_some_and(|of| of.name == workflow)
                })
                .take(MAX_RECORDING_CANDIDATES)
                .map(|record| record.run)
                .collect()
        } else {
            let mut candidates = Vec::with_capacity(runs.len());
            for id in &runs {
                // The server's `app.run`: a run the caller does not see does not exist.
                let missing = || {
                    Failure::Problem(format!(
                        "no run {id}: check the id (runs are listed by GET /v1/runs)"
                    ))
                };
                let id_parsed: rewind_core::RunId = id.parse().map_err(|_| missing())?;
                let run = self.world.run_ref(id_parsed);
                let record = self
                    .world
                    .engine
                    .get_run(&run)
                    .await?
                    .filter(|record| {
                        self.world
                            .labels
                            .iter()
                            .all(|(key, value)| record.metadata.labels.get(key) == Some(value))
                    })
                    .ok_or_else(missing)?;
                if record.metadata.workflow.as_ref().map(|of| of.name.as_str())
                    != Some(workflow.as_str())
                {
                    return Err(Failure::Problem(format!(
                        "run {id} is not a run of the workflow {workflow}"
                    )));
                }
                candidates.push(run);
            }
            candidates
        };

        let mut recorder = Recorder::new(limit, test_format());
        for run in &candidates {
            if recorder.is_full() {
                break;
            }
            let Some(record) = self.world.engine.get_run(run).await? else {
                continue;
            };
            let Some(of) = &record.metadata.workflow else {
                continue;
            };
            let history = self.world.engine.export_history(run).await?;
            recorder.offer(
                &history,
                TestOrigin {
                    run: run.id,
                    version: of.version,
                    namespace: record.namespace.to_string(),
                },
            );
        }
        let tests: Vec<Test> = recorder
            .recorded
            .iter()
            .map(|recorded| recorded.test.clone())
            .collect();
        let digests = self
            .world
            .engine
            .store_tests(&tenant, &tests, test_format())
            .await
            .map_err(suite_error)?;
        let recorded = RecordedTests {
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
        };
        Ok(result(
            describe_recorded(&workflow, &recorded, &self.limits),
            false,
        ))
    }

    /// Before a fork of a run of a workflow: its version's suite must pass against the
    /// corrected script (`rewind-server`'s `testing::gate_fork`).
    pub(super) async fn gate_fork(&self, parent: &RunRef, script: &Script) -> Result<(), Failure> {
        let Some(record) = self.world.engine.get_run(parent).await? else {
            return Ok(());
        };
        let Some(of) = &record.metadata.workflow else {
            return Ok(());
        };
        let tenant = &parent.tenant;
        let Some(stored) = self
            .world
            .engine
            .journal()
            .get_workflow(tenant, of.version)
            .await
            .map_err(EngineError::from)?
        else {
            return Ok(());
        };
        let tests = self.suite(&stored).await?;
        if tests.is_empty() {
            return Ok(());
        }
        let toolset = self
            .world
            .engine
            .journal()
            .get_toolset(tenant, stored.workflow.toolset())
            .await
            .map_err(EngineError::from)?
            .ok_or_else(|| {
                Failure::Platform(AgentError::Checker(format!(
                    "the toolset of {} is not stored",
                    stored.workflow
                )))
            })?
            .toolset;
        let reports = self
            .run(
                &Subject {
                    script,
                    toolset: &toolset,
                    interface: Some(stored.workflow.schemas()),
                },
                &tests,
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

    /// The version a reference names, among those the agent sees: `name` for the latest,
    /// `name@<version>` for one (`rewind-server`'s `workflows::resolve`).
    async fn resolve(&self, reference: &str) -> Result<StoredWorkflow, Failure> {
        let (name, version) = match reference.split_once('@') {
            Some((name, version)) => (name, Some(version.to_ascii_lowercase())),
            None => (reference, None),
        };
        if let Some(version) = &version
            && (version.len() < MIN_VERSION_DIGITS
                || version.len() > 64
                || !version.chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Err(Failure::Problem(format!(
                "workflow {reference:?}: name a version by its digest or its first \
                 {MIN_VERSION_DIGITS} hex digits at least, as in triage@9f2c4e1a07b3"
            )));
        }
        let versions = self.world.workflow_versions(name).await?;
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
        found.ok_or_else(|| Failure::Problem(not_found(reference)))
    }

    /// The tests of a version's suite, those of an erased namespace left out.
    async fn suite(&self, stored: &StoredWorkflow) -> Result<Vec<Test>, Failure> {
        Ok(self
            .world
            .engine
            .load_suite(&TenantId::local(), &stored.workflow, test_format())
            .await
            .map_err(suite_error)?
            .into_iter()
            .filter_map(|loaded| loaded.test)
            .collect())
    }

    /// Runs tests against a subject with the engine's executor, as a worker does.
    async fn run(&self, subject: &Subject<'_>, tests: &[Test]) -> Result<Vec<TestReport>, Failure> {
        let loader = TsMockLoader::default();
        run_tests(
            self.world.engine.executor(),
            &loader,
            subject,
            tests,
            &TestOptions::default(),
        )
        .await
        .map_err(test_error)
    }
}

/// What `save_workflow` was given, its shapes checked.
struct SaveRequest {
    name: String,
    description: String,
    code: String,
    schemas: WorkflowSchemas,
    /// As the model gave them, their shapes checked as the SDK checks them; the server reads
    /// them.
    tests: Vec<Value>,
    update: Vec<Value>,
    retire: Vec<Value>,
    parent: Option<String>,
}

/// Tests as the model gave them: each an object (in full, or `{ from, expect }`) or a digest.
/// The server checks them.
fn optional_tests(values: &Map<String, Value>, key: &str) -> Result<Option<Vec<Value>>, Failure> {
    match values.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.iter().all(is_test_input) => Ok(Some(items.clone())),
        Some(_) => Err(Failure::Input(format!(
            "`{key}` must be an array of tests: each one an object, or the digest of a recorded test as a string."
        ))),
    }
}

fn is_test_input(value: &Value) -> bool {
    matches!(value, Value::String(text) if !text.is_empty()) || is_object(value)
}

fn optional_updates(values: &Map<String, Value>) -> Result<Option<Vec<Value>>, Failure> {
    let invalid = || {
        Failure::Input(
            "`update` must be an array of { name, reason, test?, expect? }: the name of an inherited test, why it changes, and the test that replaces it or its new expectation."
                .into(),
        )
    };
    let items = match values.get("update") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(invalid()),
    };
    let valid = items.iter().all(|item| {
        item.is_object()
            && item["name"].is_string()
            && item["reason"]
                .as_str()
                .is_some_and(|reason| !reason.trim().is_empty())
            && item.get("test").is_none_or(is_test_input)
            && item.get("expect").is_none_or(is_object)
    });
    if valid {
        Ok(Some(items.clone()))
    } else {
        Err(invalid())
    }
}

fn optional_retirements(values: &Map<String, Value>) -> Result<Option<Vec<Value>>, Failure> {
    let invalid = || {
        Failure::Input(
            "`retire` must be an array of { names, reason }: the names of inherited tests, and why they no longer hold."
                .into(),
        )
    };
    let items = match values.get("retire") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(invalid()),
    };
    let valid = items.iter().all(|item| {
        item.is_object()
            && item["names"]
                .as_array()
                .is_some_and(|names| names.iter().all(Value::is_string))
            && item["reason"]
                .as_str()
                .is_some_and(|reason| !reason.trim().is_empty())
    });
    if valid {
        Ok(Some(items.clone()))
    } else {
        Err(invalid())
    }
}

/// A body the server cannot read: `the body is not valid: …`.
fn body_invalid(error: serde_json::Error) -> Failure {
    Failure::Problem(format!("the body is not valid: {error}"))
}

/// A test as the API reads it from a body.
fn parse_test(value: Value) -> Result<ApiTestInput, Failure> {
    serde_json::from_value(value).map_err(body_invalid)
}

/// A test the API was given, as the engine takes it. A test given inline cannot claim to be
/// recorded (`rewind-server`'s `testing::input`).
pub(crate) fn test_input(input: ApiTestInput) -> Result<TestInput, Failure> {
    match input {
        ApiTestInput::Digest(digest) => Ok(TestInput::Stored(test_digest(&digest)?)),
        ApiTestInput::From(from) => Ok(TestInput::From {
            from: test_digest(&from.from)?,
            expect: from.expect.into_core(),
        }),
        ApiTestInput::Inline(test) => {
            if test.origin.is_some() {
                return Err(Failure::InvalidTest(format!(
                    "test {:?}: `origin` is set by recording, never given: record the run \
                     with POST /v1/workflows/{{name}}/record and give its digest instead",
                    test.name
                )));
            }
            test.into_core(decode_cassette)
                .map(|test| TestInput::Inline(Box::new(test)))
                .map_err(Failure::InvalidTest)
        }
    }
}

fn test_digest(text: &str) -> Result<Digest, Failure> {
    Digest::from_hex(text).map_err(|_| {
        Failure::InvalidTest(format!(
            "{text:?} is not a test's digest: 64 lowercase hex digits, as record_tests \
             returns them"
        ))
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
        trace: report.trace.clone(),
    }
}

/// `tests-failed`: every outcome, the detail naming those that did not pass.
fn tests_failed(what: &str, reports: &[TestReport]) -> Failure {
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
    Failure::TestsFailed {
        detail: format!(
            "{what}: {} of {} tests did not pass. {}. Fix the script, or retire a test that is \
             deprecated or update one whose state or result is out of date, each with a reason",
            failed.len(),
            reports.len(),
            failed.join("; ")
        ),
        tests: reports.iter().map(test_result).collect(),
    }
}

fn suite_error(error: SuiteLoadError) -> Failure {
    match error {
        SuiteLoadError::Suite(SuiteError::ParentNotFound(version)) => Failure::Problem(format!(
            "no workflow version {version} that this caller sees: name the parent as \
             name@<version>, or leave it out to derive from the latest version"
        )),
        SuiteLoadError::Suite(error) => Failure::InvalidTest(error.to_string()),
        SuiteLoadError::Store(error) => Failure::Platform(AgentError::Engine(error.into())),
        other => Failure::Platform(AgentError::Checker(other.to_string())),
    }
}

fn test_error(error: TestError) -> Failure {
    match error {
        TestError::Start(error) => start_failure(error),
        TestError::Engine(error) => Failure::Platform(AgentError::Engine(error)),
        TestError::Mocks(message) => Failure::Platform(AgentError::Checker(message)),
    }
}

/// A workflow refused at registration, as the API answers it (`rewind-server`'s
/// `workflows::register_error`).
fn register_error(error: RegisterError) -> Failure {
    match error {
        RegisterError::Invalid(_) | RegisterError::Schema { .. } => {
            Failure::InvalidWorkflow(error.to_string())
        }
        RegisterError::ToolsetNotFound(digest) => Failure::Problem(format!(
            "no toolset {digest} for this tenant: register it with POST /v1/toolsets"
        )),
        RegisterError::InvalidScript(invalid) => start_failure(StartError::InvalidScript(invalid)),
        RegisterError::TypeErrors(diagnostics) => {
            start_failure(StartError::TypeErrors(diagnostics))
        }
        RegisterError::NulCharacter(error) => Failure::Problem(error.to_string()),
        RegisterError::Suite(error) => suite_error(error.into()),
        RegisterError::TestsFailed(reports) => tests_failed("the workflow was not saved", &reports),
        RegisterError::Tests(error) => test_error(error),
        RegisterError::Store(rewind_engine::ports::StoreError::Conflict(message)) => {
            Failure::Problem(message)
        }
        other => Failure::Platform(AgentError::Checker(other.to_string())),
    }
}

fn not_found(reference: &str) -> String {
    format!(
        "no workflow {reference} for this API key: save it with POST /v1/workflows, or list them \
         with GET /v1/workflows"
    )
}
