//! The handlers of the agent tools, as the SDK's `AgentTools` handles them
//! (`sdk/typescript/src/agent-tools/agent-tools.ts`): the same inputs, the same calls of `/v1`,
//! the same checks of labels, the same bounded plain-text results.
//!
//! A mistake the model can fix (a bad argument, a refused script, a run that has ended: a
//! problem of `/v1` with a 4xx status but 401) is a result with `is_error`; a fault of the
//! platform or of the harness is an [`AgentError`].

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use rewind_api::common::{Labels, RunStatus};
use rewind_api::headers::IdempotencyKey;
use rewind_api::runs::{CancelRun, ForkRun, Run, RunFilter, StartRun};
use rewind_api::scripts::{CheckRequest, TestRequest};
use rewind_api::testing::{TestInput, TestResult};
use rewind_api::toolsets::{CreateToolset, SearchQuery, Toolset};
use rewind_api::workflows::{CreateWorkflow, RecordTests, TestRetirement, TestUpdate, Workflow};
use rewind_api::{Problem, ProblemType};
use serde_json::{Value, json};

use crate::api::{Api, ApiError};
use crate::definitions::{AGENT_TOOLS, NEEDS_TOOLSET, ToolDefinition, definition};
use crate::input::{
    InputError, Values, optional_attachments, optional_index, optional_retirements,
    optional_string, optional_strings, optional_tests, optional_updates, require_object,
    require_string,
};
use crate::keys::{idempotency_key, run_key, sha256_hex};
use crate::render::{
    Limits, OUTPUT_LIMIT, RunExtras, SuiteEntry, UNTITLED_NOTE, WorkflowDetails, args,
    describe_check, describe_matches, describe_page, describe_recorded, describe_refusal,
    describe_retry_refused, describe_run, describe_runs, describe_save_refused, describe_secrets,
    describe_test_results, describe_workflow, describe_workflow_details, describe_workflow_refusal,
    entry_text, json, quoted, truncate_end, truncate_middle, workflow_ref,
};

/// What a tool call returns to the model, and to the harness.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentToolResult {
    /// The text for the model, bounded.
    pub content: String,
    /// The call failed, or what it ran failed: the model should look at it.
    pub is_error: bool,
    /// The run the call started or read (pending approvals, for example).
    pub run: Option<Run>,
    /// The workflow `save_workflow` saved, or `get_workflow` read.
    pub workflow: Option<Workflow>,
    /// The toolset `save_workflow` derived: the one the tools use from then on.
    pub toolset: Option<Toolset>,
    /// The outcome of each test: those `test_script` ran, or those of the suite that refused a
    /// save (`save_workflow`) or a fork (`retry_run`).
    pub tests: Option<Vec<TestResult>>,
    /// For a call that hands in a script (`run_script`, `check_script`, `test_script`,
    /// `retry_run`, `save_workflow`): whether the checks before anything runs passed it. `None`
    /// when the call carried no script, or failed before the checks (a bad argument, another
    /// refusal): what a harness measures as the first-try check rate.
    pub script: Option<ScriptVerdict>,
}

/// What the checks before anything runs said of a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptVerdict {
    Passed,
    Refused,
}

pub(crate) fn result(content: String, is_error: bool) -> AgentToolResult {
    AgentToolResult {
        content,
        is_error,
        run: None,
        workflow: None,
        toolset: None,
        tests: None,
        script: None,
    }
}

/// A fault of the platform, of the harness or of its configuration: the model cannot fix it.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The tool is not one of those given.
    #[error("{0:?} is not one of these agent tools")]
    UnknownTool(String),
    /// The options name a tool that does not exist, or one that needs a toolset without one.
    #[error("{0}")]
    Options(String),
    /// The API could not answer, or answered a 401 or a 5xx.
    #[error(transparent)]
    Api(ApiError),
    /// What the harness did wrong: a tool call id reused for another run.
    #[error("{0}")]
    Harness(String),
}

/// How a handler fails.
#[derive(Debug)]
pub(crate) enum Failure {
    Input(InputError),
    Api(ApiError),
    Agent(AgentError),
}

impl From<InputError> for Failure {
    fn from(error: InputError) -> Self {
        Failure::Input(error)
    }
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Failure::Api(error)
    }
}

pub(crate) type Handled = Result<AgentToolResult, Failure>;

/// The tools that hand in a script.
fn carries_script(name: &str) -> bool {
    matches!(
        name,
        "run_script" | "check_script" | "test_script" | "retry_run" | "save_workflow"
    )
}

/// A handler's outcome as the model reads it: a mistake it can fix is a result with
/// `is_error`, the rest an error. Every result is bounded.
pub(crate) fn finish(
    name: &str,
    handled: Handled,
    limits: &Limits,
) -> Result<AgentToolResult, AgentError> {
    let mut finished = match handled {
        Ok(mut handled) => {
            if carries_script(name) && handled.script.is_none() {
                handled.script = Some(ScriptVerdict::Passed);
            }
            handled
        }
        Err(Failure::Input(error)) => result(format!("{name}: {}", error.0), true),
        Err(Failure::Api(ApiError::Problem(problem))) if problem.is(ProblemType::InvalidScript) => {
            AgentToolResult {
                script: Some(ScriptVerdict::Refused),
                ..result(
                    describe_refusal(&problem.detail, &problem.diagnostics, limits),
                    true,
                )
            }
        }
        Err(Failure::Api(ApiError::Problem(problem))) if is_refusal(&problem) => result(
            format!(
                "{name} failed: {}",
                truncate_end(&problem.detail, limits.value)
            ),
            true,
        ),
        Err(Failure::Api(error)) => return Err(AgentError::Api(error)),
        Err(Failure::Agent(error)) => return Err(error),
    };
    // Each part is bounded; this bounds what a script, a server or a tool wrote unbounded.
    finished.content = truncate_middle(&finished.content, limits.total);
    Ok(finished)
}

/// A problem the model can fix: a 4xx but 401.
fn is_refusal(problem: &Problem) -> bool {
    problem.status != 401 && problem.status < 500
}

/// What the API answers to a body it cannot read: the tools read the model's tests as the API
/// reads them, and say the same.
fn body_invalid(error: &serde_json::Error) -> Failure {
    Failure::Api(
        Problem::new(
            ProblemType::InvalidRequest,
            format!("the body is not valid: {error}"),
        )
        .into(),
    )
}

fn from_values<T: serde::de::DeserializeOwned>(values: Vec<Value>) -> Result<Vec<T>, Failure> {
    values
        .into_iter()
        .map(|value| serde_json::from_value(value).map_err(|error| body_invalid(&error)))
        .collect()
}

/// Per call: the model's tool call id, used as the idempotency key, and the call's trace
/// context.
#[derive(Debug, Clone, Default)]
pub struct HandleOptions {
    /// The model's tool call id. It becomes the `Idempotency-Key` of `run_script`, `retry_run`
    /// and `signal_run`, so handling the same tool call again returns the same run or delivers
    /// one signal.
    pub tool_call_id: Option<String>,
    /// W3C trace context of the LLM call that wrote the script.
    pub traceparent: Option<String>,
    pub tracestate: Option<String>,
}

/// One call's view of the tools: the API, the toolset and labels it acts with, its bounds.
pub(crate) struct Session<'a, A> {
    pub(crate) api: &'a A,
    pub(crate) toolset: Option<String>,
    pub(crate) labels: &'a BTreeMap<String, String>,
    pub(crate) limits: Limits,
    pub(crate) default_wait: Duration,
    pub(crate) max_wait: Duration,
    pub(crate) ttl: Option<Duration>,
    pub(crate) call: &'a HandleOptions,
}

impl<A: Api> Session<'_, A> {
    pub(crate) async fn run_script(&self, input: &Values) -> Handled {
        let code = require_string(input, "code")?;
        let wait = self.wait_of(input.get("wait"))?;
        let secrets = optional_attachments(input)?;
        let key = run_key(self.labels, self.call.tool_call_id.as_deref());
        let mut start = StartRun::new(code.clone());
        start.secrets = secrets.unwrap_or_default();
        start.toolset.clone_from(&self.toolset);
        start.input = input.get("input").cloned().unwrap_or_else(|| json!({}));
        start.wait = Some(wait.into());
        start.labels = Labels(self.labels.clone());
        start.ttl = self.ttl.map(Into::into);
        start.traceparent.clone_from(&self.call.traceparent);
        start.tracestate.clone_from(&self.call.tracestate);
        let run = self.api.start_run(&start, key.as_ref()).await?;
        self.check_answered(&run, &code, key.as_ref())?;
        let failed = run.status == RunStatus::Failed;
        self.report(run, true, failed).await
    }

    pub(crate) async fn check_script(&self, input: &Values) -> Handled {
        let check = CheckRequest {
            script: require_string(input, "code")?,
            toolset: self.toolset.clone(),
        };
        let checked = self.api.check(&check).await?;
        let mut content = describe_check(&checked, &self.limits);
        if rewind_core::script_doc(&check.script).is_none() {
            content = format!("{content}\n{UNTITLED_NOTE}");
        }
        Ok(AgentToolResult {
            script: Some(if checked.ok {
                ScriptVerdict::Passed
            } else {
                ScriptVerdict::Refused
            }),
            ..result(content, !checked.ok)
        })
    }

    pub(crate) async fn test_script(&self, input: &Values) -> Handled {
        let code = require_string(input, "code")?;
        let tests = optional_tests(input, "tests")?;
        let workflow = optional_string(input, "workflow")?;
        if tests.is_none() && workflow.is_none() {
            return Err(InputError::new(
                "give `tests`, or a `workflow` whose suite runs against the script, or both.",
            )
            .into());
        }
        let request = TestRequest {
            script: Some(code),
            toolset: self.toolset.clone(),
            tests: from_values::<TestInput>(tests.unwrap_or_default())?,
            workflow,
        };
        let run = match self.api.test(&request).await {
            Ok(run) => run,
            Err(ApiError::Problem(problem)) if problem.is(ProblemType::InvalidTest) => {
                return Ok(result(
                    format!(
                        "The tests were refused before running: {}",
                        truncate_end(&problem.detail, self.limits.value)
                    ),
                    true,
                ));
            }
            Err(error) => return Err(error.into()),
        };
        Ok(AgentToolResult {
            tests: Some(run.tests.clone()),
            ..result(describe_test_results(&run.tests, &self.limits), !run.passed)
        })
    }

    pub(crate) async fn get_run(&self, input: &Values) -> Handled {
        let run_id = require_string(input, "run_id")?;
        let entry = optional_index(input, "entry")?;
        let offset = optional_index(input, "offset")?;
        let wait = match input.get("wait") {
            None => None,
            Some(value) => Some(self.wait_of(Some(value))?),
        };
        let mut run = self.scoped_run(&run_id).await?;
        if let (Some(wait), None) = (wait, entry) {
            run = self.api.get_run(&run_id, Some(wait.into())).await?;
        }
        // The run is named by its handle from here on, whatever the model gave.
        let handle = run.handle.clone();
        if let Some(index) = entry {
            let text = entry_text(&self.api.run_entry(&run_id, index).await?);
            let content = describe_page(
                &format!("Entry {index} of run {handle}"),
                &text,
                usize::try_from(offset.unwrap_or(0)).unwrap_or(usize::MAX),
                self.limits.output,
                |next| {
                    format!(
                        "get_run({})",
                        args(&[
                            ("run_id", json!(handle)),
                            ("entry", json!(index)),
                            ("offset", json!(next)),
                        ])
                    )
                },
            );
            return Ok(AgentToolResult {
                run: Some(run),
                ..result(content, false)
            });
        }
        if let Some(offset) = offset {
            if run.status != RunStatus::Completed {
                return Err(InputError::new(format!(
                    "run {handle} has no output to page through: it is {}.",
                    run.status.as_str()
                ))
                .into());
            }
            let content = describe_page(
                &format!("Output of run {handle}"),
                &self.output_of(&run).await?,
                usize::try_from(offset).unwrap_or(usize::MAX),
                self.limits.output,
                |next| {
                    format!(
                        "get_run({})",
                        args(&[("run_id", json!(handle)), ("offset", json!(next))])
                    )
                },
            );
            return Ok(AgentToolResult {
                run: Some(run),
                ..result(content, false)
            });
        }
        self.report(run, false, true).await
    }

    pub(crate) async fn list_runs(&self, input: &Values) -> Handled {
        let status = match input.get("status") {
            None | Some(Value::Null) => None,
            Some(value) => Some(serde_json::from_value::<RunStatus>(value.clone()).map_err(|_| {
                InputError::new(
                    "`status` must be one of pending, active, completed, failed, cancelled, aborted.",
                )
            })?),
        };
        let filter = RunFilter {
            labels: Labels(self.labels.clone()).to_filters(),
            status,
            limit: Some(50),
            ..RunFilter::default()
        };
        let page = self.api.list_runs(&filter).await?;
        Ok(result(
            describe_runs(&page.runs, page.next_cursor.is_some(), status),
            false,
        ))
    }

    pub(crate) async fn signal_run(&self, input: &Values) -> Handled {
        let run_id = require_string(input, "run_id")?;
        let name = require_string(input, "name")?;
        let run = self.scoped_run(&run_id).await?;
        let payload = input.get("payload").cloned().unwrap_or(Value::Null);
        let key = idempotency_key(self.call.tool_call_id.as_deref());
        let accepted = match self
            .api
            .signal_run(&run_id, &name, &payload, key.as_ref())
            .await
        {
            Ok(accepted) => accepted,
            Err(ApiError::Problem(problem)) if problem.is(ProblemType::InvalidSignal) => {
                let content = format!(
                    "Run {} refused the signal {}: {}",
                    run.handle,
                    quoted(&name),
                    truncate_end(&problem.detail, self.limits.value)
                );
                return Ok(AgentToolResult {
                    run: Some(run),
                    ..result(content, true)
                });
            }
            Err(error) => return Err(error.into()),
        };
        let handle = &run.handle;
        let content = if accepted.duplicate {
            format!(
                "This signal {} was already sent to run {handle}: nothing was added.",
                quoted(&name)
            )
        } else {
            format!(
                "Sent the signal {} to run {handle}. Read what the run does next with get_run({}).",
                quoted(&name),
                args(&[("run_id", json!(handle)), ("wait", json!("30s"))])
            )
        };
        Ok(AgentToolResult {
            run: Some(run),
            ..result(content, false)
        })
    }

    pub(crate) async fn cancel_run(&self, input: &Values) -> Handled {
        let run_id = require_string(input, "run_id")?;
        self.scoped_run(&run_id).await?;
        let cancel = CancelRun {
            reason: None,
            wait: Some(self.default_wait.into()),
        };
        let run = self.api.cancel_run(&run_id, &cancel).await?;
        self.report(run, false, false).await
    }

    pub(crate) async fn retry_run(&self, input: &Values) -> Handled {
        let run_id = require_string(input, "run_id")?;
        let code = require_string(input, "code")?;
        let secrets = optional_attachments(input)?;
        let parent = self.scoped_run(&run_id).await?;
        let key = run_key(self.labels, self.call.tool_call_id.as_deref());
        let mut fork = ForkRun::new(code.clone());
        fork.secrets = secrets;
        fork.labels = (!self.labels.is_empty()).then(|| Labels(self.labels.clone()));
        fork.wait = Some(self.default_wait.into());
        fork.ttl = self.ttl.map(Into::into);
        fork.traceparent.clone_from(&self.call.traceparent);
        fork.tracestate.clone_from(&self.call.tracestate);
        let run = match self.api.fork_run(&run_id, &fork, key.as_ref()).await {
            Ok(run) => run,
            Err(ApiError::Problem(problem)) if problem.is(ProblemType::TestsFailed) => {
                let workflow = parent
                    .workflow
                    .as_ref()
                    .map(|workflow| workflow_ref(&workflow.name, &workflow.version));
                let content = describe_retry_refused(
                    &parent.handle,
                    &problem.detail,
                    &problem.tests,
                    workflow.as_deref(),
                    &self.limits,
                );
                return Ok(AgentToolResult {
                    run: Some(parent),
                    tests: Some(problem.tests.clone()),
                    ..result(content, true)
                });
            }
            Err(error) => return Err(error.into()),
        };
        self.check_answered(&run, &code, key.as_ref())?;
        let failed = run.status == RunStatus::Failed;
        self.report(run, true, failed).await
    }

    /// `list_secrets`: the organization's secrets the caller sees, and those of `namespace`. A
    /// list the caller may not read (403) is left out: its key may attach none of them.
    pub(crate) async fn list_secrets(&self, namespace: Option<&str>) -> Handled {
        let readable = |listed: Result<rewind_api::secrets::SecretList, ApiError>| match listed {
            Ok(list) => Ok(list.secrets),
            Err(ApiError::Problem(problem)) if problem.status == 403 => Ok(Vec::new()),
            Err(error) => Err(error),
        };
        let organization = readable(self.api.list_secrets().await)?;
        let of_namespace = match namespace {
            Some(name) => Some((name, readable(self.api.namespace_secrets(name).await)?)),
            None => None,
        };
        let content = describe_secrets(
            &organization,
            of_namespace
                .as_ref()
                .map(|(name, secrets)| (*name, secrets.as_slice())),
        );
        Ok(result(content, false))
    }

    pub(crate) async fn search_tools(&self, input: &Values) -> Handled {
        let query = require_string(input, "query")?;
        let toolset = self.needs_toolset("search_tools")?;
        let found = self
            .api
            .search_tools(
                &toolset,
                &SearchQuery {
                    query: query.clone(),
                    limit: None,
                },
            )
            .await?;
        Ok(result(describe_matches(&query, &found.tools), false))
    }

    /// `POST /v1/workflows` with the session's toolset. The workflow, or what the model reads of
    /// its refusal.
    async fn create_workflow(
        &self,
        input: &Values,
    ) -> Result<Result<Workflow, AgentToolResult>, Failure> {
        let name = require_string(input, "name")?;
        let description = require_string(input, "description")?;
        let code = require_string(input, "code")?;
        let schema_input = require_object(input, "input")?.clone();
        let schema_output = require_object(input, "output")?.clone();
        let signals = match input.get("signals") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(Value::Object(signals)) => signals
                .iter()
                .map(|(name, schema)| (name.clone(), schema.clone()))
                .collect(),
            Some(_) => {
                return Err(InputError::new(
                    "`signals` must be an object of JSON Schemas by signal name.",
                )
                .into());
            }
        };
        let tests = optional_tests(input, "tests")?;
        let update = optional_updates(input)?;
        let retire = optional_retirements(input)?;
        let parent = optional_string(input, "parent")?;
        let secrets = optional_attachments(input)?;
        let toolset = self.needs_toolset("save_workflow")?;
        // The body, as the API reads it.
        let request = CreateWorkflow {
            name: name.clone(),
            description: Some(description),
            toolset,
            script: code,
            input: schema_input,
            output: schema_output,
            signals,
            parent,
            tests: from_values::<TestInput>(tests.unwrap_or_default())?,
            update: from_values::<TestUpdate>(update.unwrap_or_default())?,
            retire: from_values::<TestRetirement>(retire.unwrap_or_default())?,
            secrets: secrets.unwrap_or_default(),
        };
        match self.api.create_workflow(&request).await {
            Ok(workflow) => Ok(Ok(workflow)),
            Err(ApiError::Problem(problem)) if problem.is(ProblemType::InvalidScript) => {
                Ok(Err(AgentToolResult {
                    script: Some(ScriptVerdict::Refused),
                    ..result(
                        describe_workflow_refusal(
                            &problem.detail,
                            &problem.diagnostics,
                            &self.limits,
                        ),
                        true,
                    )
                }))
            }
            Err(ApiError::Problem(problem))
                if problem.is(ProblemType::InvalidWorkflow)
                    || problem.is(ProblemType::InvalidTest) =>
            {
                Ok(Err(result(
                    describe_workflow_refusal(&problem.detail, &problem.diagnostics, &self.limits),
                    true,
                )))
            }
            Err(ApiError::Problem(problem)) if problem.is(ProblemType::TestsFailed) => {
                Ok(Err(AgentToolResult {
                    tests: Some(problem.tests.clone()),
                    ..result(
                        describe_save_refused(&problem.detail, &problem.tests, &name, &self.limits),
                        true,
                    )
                }))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// A workflow for the model to repair or extend: the latest version's source and schemas,
    /// its suite with each test (a test it may not read is said so), and the lineage.
    pub(crate) async fn get_workflow(&self, input: &Values) -> Handled {
        let name = require_string(input, "name")?;
        let versions = self.api.workflow_versions(&name).await?.versions;
        let Some(latest) = versions.first().cloned() else {
            return Err(InputError::new(format!("the workflow {name} has no version.")).into());
        };
        let script = self.api.get_script(&latest.script);
        let tests = stream::iter(latest.tests.iter().cloned())
            .map(|test| async move {
                match self.api.get_test(&test.digest).await {
                    Ok(read) => Ok(SuiteEntry {
                        name: test.name,
                        digest: test.digest,
                        test: Some(read),
                        unreadable: None,
                    }),
                    Err(ApiError::Problem(problem)) if matches!(problem.status, 403 | 404) => {
                        Ok(SuiteEntry {
                            name: test.name,
                            digest: test.digest,
                            test: None,
                            unreadable: Some(problem.detail),
                        })
                    }
                    Err(error) => Err(error),
                }
            })
            .buffered(8)
            .try_collect::<Vec<_>>();
        let (script, tests) = futures_util::future::try_join(script, tests).await?;
        let details = WorkflowDetails {
            versions,
            source: script.source,
            tests,
        };
        Ok(AgentToolResult {
            workflow: Some(latest),
            ..result(describe_workflow_details(&details, &self.limits), false)
        })
    }

    /// Tests recorded from the workflow's past runs.
    pub(crate) async fn record_tests(&self, input: &Values) -> Handled {
        let workflow = require_string(input, "workflow")?;
        let query = optional_string(input, "query")?;
        let runs = optional_strings(input, "runs")?;
        let limit = match input.get("limit") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .filter(|limit| *limit >= 1)
                    .ok_or_else(|| InputError::new("`limit` must be an integer, 1 or more."))?,
            ),
        };
        let record = RecordTests {
            query,
            runs: runs.unwrap_or_default(),
            limit: limit.map(|limit| u32::try_from(limit).unwrap_or(u32::MAX)),
        };
        let recorded = self.api.record_tests(&workflow, &record).await?;
        Ok(result(
            describe_recorded(&workflow, &recorded, &self.limits),
            false,
        ))
    }

    /// A run's report: its output when it completed, its trace when asked.
    async fn report(&self, run: Run, error_when_failed: bool, trace: bool) -> Handled {
        let output = if run.status == RunStatus::Completed {
            Some(self.output_of(&run).await?)
        } else {
            None
        };
        let trace = if trace {
            Some(self.api.run_trace(&run.id).await?.text)
        } else {
            None
        };
        let ended = matches!(
            run.status,
            RunStatus::Failed | RunStatus::Cancelled | RunStatus::Aborted
        );
        let content = describe_run(&run, &RunExtras { output, trace }, &self.limits);
        Ok(AgentToolResult {
            run: Some(run),
            ..result(content, error_when_failed && ended)
        })
    }

    /// The run's output as compact JSON, read from its payload when it is too large to be
    /// inline.
    async fn output_of(&self, run: &Run) -> Result<String, Failure> {
        let Some(payload) = &run.output else {
            return Ok("null".to_owned());
        };
        if let Some(value) = &payload.value {
            return Ok(json(value));
        }
        let value = self.api.get_payload(&payload.digest, Some(&run.id)).await?;
        Ok(json(&value))
    }

    /// A key the server already knows returns the run it started, whoever started it. The key
    /// of a run is scoped by the agent's labels, so this is a safety net: an agent without
    /// labels, or two agents with the same labels whose tool call ids repeat, would get another
    /// conversation's run, or the run of an earlier script. Such a run is the harness's fault,
    /// never shown to the model: it must carry the agent's labels and be a run of this very
    /// script.
    fn check_answered(
        &self,
        run: &Run,
        code: &str,
        key: Option<&IdempotencyKey>,
    ) -> Result<(), Failure> {
        if key.is_none() || (has_labels(&run.labels, self.labels) && run.script == sha256_hex(code))
        {
            return Ok(());
        }
        Err(Failure::Agent(AgentError::Harness(format!(
            "the tool call id {:?} is already the idempotency key of another run (of another \
             script, or with other labels): give each tool call a unique id",
            self.call.tool_call_id.as_deref().unwrap_or_default()
        ))))
    }

    /// The run, if it carries the agent's labels; otherwise the same answer as a run that does
    /// not exist.
    async fn scoped_run(&self, run_id: &str) -> Result<Run, Failure> {
        let run = self.api.get_run(run_id, None).await?;
        if !has_labels(&run.labels, self.labels) {
            return Err(InputError::new(format!(
                "Run {run_id} does not exist, or is not one of the runs you started."
            ))
            .into());
        }
        Ok(run)
    }

    fn needs_toolset(&self, tool: &str) -> Result<String, Failure> {
        self.toolset
            .clone()
            .ok_or_else(|| Failure::Agent(AgentError::Options(format!("{tool} needs a toolset"))))
    }

    /// The model's wait, bounded by the longest one, or the default wait.
    fn wait_of(&self, value: Option<&Value>) -> Result<Duration, InputError> {
        let invalid = || InputError::new("`wait` must be a duration such as \"30s\".");
        let wait = match value {
            None | Some(Value::Null) => return Ok(self.default_wait),
            Some(Value::Number(ms)) => {
                ms.as_u64().map(Duration::from_millis).ok_or_else(invalid)?
            }
            Some(Value::String(text)) => rewind_core::parse_duration(text)
                .map_err(|error| InputError::new(format!("`wait` is not a duration: {error}")))?,
            Some(_) => return Err(invalid()),
        };
        Ok(wait.min(self.max_wait))
    }
}

/// Whether `labels` holds every label of `required`.
fn has_labels(labels: &Labels, required: &BTreeMap<String, String>) -> bool {
    required
        .iter()
        .all(|(key, value)| labels.0.get(key) == Some(value))
}

// -------------------------------------------------------------------------------------------
// The SDK's set

/// How the agent tools of the SDK's set behave: the SDK's `AgentToolsOptions`.
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// The toolset the model's scripts import from `rewind:tools`. `search_tools`,
    /// `save_workflow`, `get_workflow` and `record_tests` need one. Each workflow
    /// `save_workflow` saves derives a new toolset from it, which the later calls use.
    pub toolset: Option<String>,
    /// Set on every run the model starts. They scope `list_runs`, and the tools that take a run
    /// id refuse a run that does not carry them.
    pub labels: BTreeMap<String, String>,
    /// The tools to give the model; all of them by default, those that need a toolset only with
    /// one.
    pub tools: Option<Vec<String>>,
    /// How long `run_script`, `retry_run` and `cancel_run` wait for the run to end: 30 s.
    pub wait: Duration,
    /// The longest wait the model may ask for: 2 minutes.
    pub max_wait: Duration,
    /// The time to live of the runs the model starts; the server's default (30 days) otherwise.
    pub ttl: Option<Duration>,
    /// The most of a run's output one answer holds, in bytes: 256 KiB.
    pub output_limit: usize,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            toolset: None,
            labels: BTreeMap::new(),
            tools: None,
            wait: Duration::from_secs(30),
            max_wait: Duration::from_secs(120),
            ttl: None,
            output_limit: OUTPUT_LIMIT,
        }
    }
}

/// The agent tools of the SDK's set over an [`Api`]: their definitions for the model, and
/// their handlers.
#[derive(Debug)]
pub struct AgentTools<A> {
    api: A,
    options: AgentOptions,
    names: Vec<&'static str>,
    limits: Limits,
    /// The toolset of the model's scripts: the one given, then the last one `save_workflow`
    /// derived.
    current: Mutex<Option<String>>,
    /// One derivation of the toolset at a time, so concurrent saves all end up in it.
    derivations: tokio::sync::Mutex<()>,
}

impl<A: Api> AgentTools<A> {
    /// The tools of `options.tools`, or all of them (those that need a toolset only with one).
    pub fn new(api: A, options: AgentOptions) -> Result<Self, AgentError> {
        let has_toolset = options.toolset.is_some();
        let names: Vec<&'static str> = match &options.tools {
            None => AGENT_TOOLS
                .into_iter()
                .filter(|name| has_toolset || !NEEDS_TOOLSET.contains(name))
                .collect(),
            Some(tools) => tools
                .iter()
                .map(|name| {
                    AGENT_TOOLS
                        .into_iter()
                        .find(|known| *known == name.as_str())
                        .ok_or_else(|| {
                            AgentError::Options(format!(
                                "unknown agent tool {name:?}: expected one of {}",
                                AGENT_TOOLS.join(", ")
                            ))
                        })
                })
                .collect::<Result<_, _>>()?,
        };
        if let Some(name) = names
            .iter()
            .find(|name| !has_toolset && NEEDS_TOOLSET.contains(*name))
        {
            return Err(AgentError::Options(format!("{name} needs a toolset")));
        }
        let wait = options.wait.min(options.max_wait);
        Ok(Self {
            limits: Limits::for_output(options.output_limit),
            current: Mutex::new(options.toolset.clone()),
            options: AgentOptions { wait, ..options },
            api,
            names,
            derivations: tokio::sync::Mutex::new(()),
        })
    }

    /// The tools given, for the model.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.names
            .iter()
            .filter_map(|name| definition(name))
            .collect()
    }

    /// Whether `name` is one of these tools.
    pub fn has(&self, name: &str) -> bool {
        self.names.contains(&name)
    }

    /// The toolset the model's scripts run with: the one given, or the last one `save_workflow`
    /// derived from it. A harness that creates the agent tools again for each turn keeps it with
    /// the conversation, and passes it as `toolset` the next time.
    pub fn toolset(&self) -> Option<String> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Executes a tool call of the model. `input` is the call's arguments, an object or its JSON
    /// text. Mistakes the model can fix come back as a result with `is_error`; a fault of the
    /// platform or of the harness is an error.
    pub async fn handle(
        &self,
        name: &str,
        input: &Value,
        call: &HandleOptions,
    ) -> Result<AgentToolResult, AgentError> {
        if !self.has(name) {
            return Err(AgentError::UnknownTool(name.to_owned()));
        }
        let session = Session {
            api: &self.api,
            toolset: self.toolset(),
            labels: &self.options.labels,
            limits: self.limits,
            default_wait: self.options.wait,
            max_wait: self.options.max_wait,
            ttl: self.options.ttl,
            call,
        };
        let handled = match crate::input::parse(name, input) {
            Ok(values) => match name {
                "run_script" => session.run_script(&values).await,
                "check_script" => session.check_script(&values).await,
                "test_script" => session.test_script(&values).await,
                "get_run" => session.get_run(&values).await,
                "list_runs" => session.list_runs(&values).await,
                "signal_run" => session.signal_run(&values).await,
                "cancel_run" => session.cancel_run(&values).await,
                "retry_run" => session.retry_run(&values).await,
                "search_tools" => session.search_tools(&values).await,
                "list_secrets" => {
                    let namespace = self.api.namespace();
                    session.list_secrets(namespace.as_deref()).await
                }
                "save_workflow" => self.save_workflow(&session, &values).await,
                "get_workflow" => session.get_workflow(&values).await,
                "record_tests" => session.record_tests(&values).await,
                other => Err(Failure::Agent(AgentError::UnknownTool(other.to_owned()))),
            },
            Err(error) => Err(error.into()),
        };
        finish(name, handled, &self.limits)
    }

    /// Saves a workflow with the agent's toolset, then derives from that toolset one that pins
    /// this version of it: the later scripts of the model import it from `rewind:workflows`.
    async fn save_workflow(&self, session: &Session<'_, A>, input: &Values) -> Handled {
        let workflow = match session.create_workflow(input).await? {
            Ok(workflow) => workflow,
            Err(refused) => return Ok(refused),
        };
        let derived = match self.derive(&workflow).await {
            Ok(derived) => derived,
            Err(Failure::Api(ApiError::Problem(problem))) if is_refusal(&problem) => {
                return Ok(AgentToolResult {
                    workflow: Some(workflow.clone()),
                    ..result(
                        format!(
                            "The workflow {} was saved, but your scripts cannot start it yet: \
                             adding it to your tools failed: {}",
                            workflow.name,
                            truncate_end(&problem.detail, self.limits.value)
                        ),
                        true,
                    )
                });
            }
            Err(failure) => return Err(failure),
        };
        Ok(AgentToolResult {
            toolset: Some(derived),
            ..result(describe_workflow(&workflow, &self.limits), false)
        }
        .with_workflow(workflow))
    }

    /// The current toolset with this version of the workflow pinned, which becomes the current
    /// toolset. One derivation at a time: each starts from the toolset the previous one gave.
    async fn derive(&self, workflow: &Workflow) -> Result<Toolset, Failure> {
        let _one_at_a_time = self.derivations.lock().await;
        let base = self.toolset().ok_or_else(|| {
            Failure::Agent(AgentError::Options("save_workflow needs a toolset".into()))
        })?;
        let toolset = self
            .api
            .create_toolset(&CreateToolset {
                base: Some(base),
                workflows: vec![format!("{}@{}", workflow.name, workflow.version)],
                ..CreateToolset::default()
            })
            .await?;
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(toolset.id.clone());
        Ok(toolset)
    }
}

impl AgentToolResult {
    fn with_workflow(mut self, workflow: Workflow) -> Self {
        self.workflow = Some(workflow);
        self
    }
}
