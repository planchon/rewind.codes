//! The handlers of the agent tools: what each sends to `/v1`, the checks of labels, the bounded
//! plain-text results.
//!
//! A mistake the model can fix (a bad argument, a refused code, a run that has ended: a
//! problem of `/v1` with a 4xx status but 401) is a result with `is_error`; a fault of the
//! platform or of the harness is an [`AgentError`].

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use rewind_api::action_sets::SearchQuery;
use rewind_api::checks::{CheckRequest, TestRequest};
use rewind_api::common::{Labels, RunStatus};
use rewind_api::headers::IdempotencyKey;
use rewind_api::runs::{CancelRun, ForkRun, Run, RunFilter, StartRun};
use rewind_api::testing::{TestInput, TestResult};
use rewind_api::workflows::{
    ChangeTests, CreateVersion, PatchWorkflow, RecordTests, RunWorkflow, TestReplacement,
    TestRetirement, Workflow,
};
use rewind_api::{Problem, ProblemType};
use serde_json::{Value, json};

use crate::api::{Api, ApiError};
use crate::definitions::{AGENT_TOOLS, NEEDS_ACTION_SET, ToolDefinition, definition};
use crate::input::{
    InputError, Values, optional_index, optional_patch, optional_replacements,
    optional_retirements, optional_schema, optional_secrets, optional_signals, optional_string,
    optional_strings, optional_tests, require_string,
};
use crate::keys::{idempotency_key, run_key, sha256_hex};
use crate::render::{
    COMMENT_NOTE, Limits, OUTPUT_LIMIT, ReadTest, RunExtras, Updated, WorkflowDetails, args,
    describe_check, describe_gate_refused, describe_matches, describe_page, describe_patch,
    describe_recorded, describe_refusal, describe_refused, describe_retry_refused, describe_run,
    describe_runs, describe_secrets, describe_test_results, describe_update,
    describe_workflow_details, event_text, json, quoted, run_workflow_reference, truncate_end,
    truncate_middle, workflow_label, workflow_reference,
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
    /// The workflow `update_workflow` changed, or `get_workflow` read.
    pub workflow: Option<Workflow>,
    /// The outcome of each test: those `test_workflow` ran, or those of the suite that refused a
    /// version or a change of tests (`update_workflow`) or a fork (`retry_run`).
    pub tests: Option<Vec<TestResult>>,
    /// For a call that hands in a code (`code` of `run_workflow`, `check_workflow`,
    /// `test_workflow`, `retry_run`, `update_workflow`): whether the checks before anything runs
    /// passed it. `None` when the call carried no code, or failed before the checks (a bad
    /// argument, another refusal): what a harness measures as the first-try check rate.
    pub code: Option<CodeVerdict>,
}

/// What the checks before anything runs said of a code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeVerdict {
    Passed,
    Refused,
}

pub(crate) fn result(content: String, is_error: bool) -> AgentToolResult {
    AgentToolResult {
        content,
        is_error,
        run: None,
        workflow: None,
        tests: None,
        code: None,
    }
}

/// A fault of the platform, of the harness or of its configuration: the model cannot fix it.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The tool is not one of those given.
    #[error("{0:?} is not one of these agent tools")]
    UnknownTool(String),
    /// The options name a tool that does not exist, or one that needs an action set without one.
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
    /// A refusal the handler wrote for the model, before the checks of a code or beside them:
    /// a result with `is_error`, without a verdict on the code.
    Refused(String),
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

/// Whether the call hands in a code: the tools that take one, given one.
pub(crate) fn carries_code(name: &str, values: &Values) -> bool {
    matches!(
        name,
        "run_workflow" | "check_workflow" | "test_workflow" | "retry_run" | "update_workflow"
    ) && values
        .get("code")
        .and_then(Value::as_str)
        .is_some_and(|code| !code.is_empty())
}

/// A handler's outcome as the model reads it: a mistake it can fix is a result with
/// `is_error`, the rest an error. Every result is bounded. `carried` when the call handed in a
/// code.
pub(crate) fn finish(
    name: &str,
    carried: bool,
    handled: Handled,
    limits: &Limits,
) -> Result<AgentToolResult, AgentError> {
    let mut finished = match handled {
        Ok(mut handled) => {
            if carried && handled.code.is_none() {
                handled.code = Some(CodeVerdict::Passed);
            }
            handled
        }
        Err(Failure::Input(error)) => result(format!("{name}: {}", error.0), true),
        Err(Failure::Refused(text)) => result(text, true),
        Err(Failure::Api(ApiError::Problem(problem))) if problem.is(ProblemType::InvalidCode) => {
            AgentToolResult {
                code: Some(CodeVerdict::Refused),
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
    // Each part is bounded; this bounds what a code, a server or a tool wrote unbounded.
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

/// What a run the tools start runs: a code, or a workflow named by a reference.
#[derive(Debug, Clone, Copy)]
enum Started<'a> {
    Code(&'a str),
    Workflow(&'a str),
}

/// Whether a run's workflow is the one `reference` names: its id or its name, and the version
/// after `@` (`v12`, or the first digits of its digest) when it names one.
fn names(workflow: &RunWorkflow, reference: &str) -> bool {
    let (base, version) = match reference.split_once('@') {
        Some((base, version)) => (base, Some(version)),
        None => (reference, None),
    };
    let same = workflow.id == base || workflow.name.as_deref() == Some(base);
    same && version.is_none_or(|version| {
        match version
            .strip_prefix('v')
            .and_then(|number| number.parse::<u32>().ok())
        {
            Some(number) => workflow.version == number,
            None => workflow.digest.starts_with(version),
        }
    })
}

/// Per call: the model's tool call id, used as the idempotency key, and the call's trace
/// context.
#[derive(Debug, Clone, Default)]
pub struct HandleOptions {
    /// The model's tool call id. It becomes the `Idempotency-Key` of `run_workflow`, `retry_run`
    /// and `signal_run`, so handling the same tool call again returns the same run or delivers
    /// one signal.
    pub tool_call_id: Option<String>,
    /// W3C trace context of the LLM call that wrote the code.
    pub traceparent: Option<String>,
    pub tracestate: Option<String>,
}

/// One call's view of the tools: the API, the action set and labels it acts with, its bounds.
pub(crate) struct Session<'a, A> {
    pub(crate) api: &'a A,
    pub(crate) action_set: Option<String>,
    pub(crate) labels: &'a BTreeMap<String, String>,
    pub(crate) limits: Limits,
    pub(crate) default_wait: Duration,
    pub(crate) max_wait: Duration,
    pub(crate) ttl: Option<Duration>,
    pub(crate) call: &'a HandleOptions,
    /// `update_workflow` is one of the tools: the results point to it.
    pub(crate) updates: bool,
}

impl<A: Api> Session<'_, A> {
    /// Runs code, or a workflow by its id or its name.
    pub(crate) async fn run_workflow(&self, input: &Values) -> Handled {
        let code = optional_string(input, "code")?;
        let workflow = optional_string(input, "workflow")?;
        let wait = self.wait_of(input.get("wait"))?;
        let secrets = optional_secrets(input)?;
        let mut start = match (&code, &workflow) {
            (Some(_), Some(_)) => {
                return Err(InputError::new(
                    "give `code` or `workflow`, not both: `code` runs new code, `workflow` an existing workflow by its id or its name.",
                )
                .into());
            }
            (None, None) => {
                return Err(InputError::new(
                    "give the `code` to run, or a `workflow` by its id or its name.",
                )
                .into());
            }
            (Some(code), None) => {
                let mut start = StartRun::new(code.clone());
                start.action_set.clone_from(&self.action_set);
                start.secrets = secrets.unwrap_or_default();
                start
            }
            (None, Some(workflow)) => {
                if secrets.is_some_and(|secrets| !secrets.is_empty()) {
                    return Err(InputError::new(
                        "`secrets` goes with `code`: a run of a workflow uses the secrets its version names.",
                    )
                    .into());
                }
                let mut start = StartRun::new(String::new());
                start.workflow = Some(workflow.clone());
                start
            }
        };
        let key = run_key(self.labels, self.call.tool_call_id.as_deref());
        start.input = input.get("input").cloned().unwrap_or_else(|| json!({}));
        start.wait = Some(wait.into());
        start.labels = Labels(self.labels.clone());
        start.ttl = self.ttl.map(Into::into);
        start.traceparent.clone_from(&self.call.traceparent);
        start.tracestate.clone_from(&self.call.tracestate);
        let run = self.api.start_run(&start, key.as_ref()).await?;
        let started = match (&code, &workflow) {
            (Some(code), _) => Started::Code(code),
            (None, workflow) => Started::Workflow(workflow.as_deref().unwrap_or_default()),
        };
        self.check_answered(&run, started, key.as_ref())?;
        let failed = run.status == RunStatus::Failed;
        self.report(run, true, failed).await
    }

    /// Diagnostics, the workflow that already has the code, and a note for a code without a
    /// comment at its top.
    pub(crate) async fn check_workflow(&self, input: &Values) -> Handled {
        let check = CheckRequest {
            code: require_string(input, "code")?,
            action_set: self.action_set.clone(),
        };
        let checked = self.api.check(&check).await?;
        let mut content = describe_check(&checked, &self.limits);
        if rewind_core::code_doc(&check.code).is_none() {
            content = format!("{content}\n{COMMENT_NOTE}");
        }
        Ok(AgentToolResult {
            code: Some(if checked.ok {
                CodeVerdict::Passed
            } else {
                CodeVerdict::Refused
            }),
            ..result(content, !checked.ok)
        })
    }

    /// Tests against code, or against a workflow's version; the workflow's suite too.
    pub(crate) async fn test_workflow(&self, input: &Values) -> Handled {
        let code = optional_string(input, "code")?;
        let tests = optional_tests(input, "tests")?;
        let workflow = optional_string(input, "workflow")?;
        if code.is_none() && workflow.is_none() {
            return Err(InputError::new(
                "give the `code` to test, or a `workflow` whose version the tests run against.",
            )
            .into());
        }
        if tests.is_none() && workflow.is_none() {
            return Err(InputError::new(
                "give `tests`, or a `workflow` whose suite runs against the code, or both.",
            )
            .into());
        }
        let request = TestRequest {
            // A workflow's version runs with its own action set.
            action_set: code.as_ref().and(self.action_set.clone()),
            code,
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
        let entry = optional_index(input, "event")?;
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
            let text = event_text(&self.api.run_event(&run_id, index).await?);
            let content = describe_page(
                &format!("Event {index} of run {handle}"),
                &text,
                usize::try_from(offset.unwrap_or(0)).unwrap_or(usize::MAX),
                self.limits.output,
                |next| {
                    format!(
                        "get_run({})",
                        args(&[
                            ("run_id", json!(handle)),
                            ("event", json!(index)),
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
        let secrets = optional_secrets(input)?;
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
                let workflow = parent.workflow.as_ref().map(run_workflow_reference);
                let content = describe_retry_refused(
                    &parent.handle,
                    &problem.detail,
                    &problem.tests,
                    workflow.as_deref(),
                    self.updates,
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
        self.check_answered(&run, Started::Code(&code), key.as_ref())?;
        let failed = run.status == RunStatus::Failed;
        self.report(run, true, failed).await
    }

    /// `list_secrets`: the organization's secrets the caller sees, and those of `namespace`. A
    /// list the caller may not read (403) is left out: its key may use none of them.
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

    pub(crate) async fn search_actions(&self, input: &Values) -> Handled {
        let query = require_string(input, "query")?;
        let action_set = self.needs_action_set("search_actions")?;
        let found = self
            .api
            .search_actions(
                &action_set,
                &SearchQuery {
                    query: query.clone(),
                    limit: None,
                },
            )
            .await?;
        Ok(result(describe_matches(&query, &found.actions), false))
    }

    /// Changes a workflow: its name and description (a merge patch), then its next version
    /// (code, interface, secrets, with tests through its gate) or its suite alone.
    pub(crate) async fn update_workflow(&self, input: &Values) -> Handled {
        let reference = require_string(input, "workflow")?;
        let patch = PatchWorkflow {
            name: optional_patch(input, "name")?,
            description: optional_patch(input, "description")?,
        };
        let code = optional_string(input, "code")?;
        let schema_input = optional_schema(input, "input")?;
        let schema_output = optional_schema(input, "output")?;
        let signals = optional_signals(input)?;
        let secrets = optional_secrets(input)?;
        let tests = optional_tests(input, "tests")?;
        let replace = optional_replacements(input)?;
        let retire = optional_retirements(input)?;
        // The bodies, as the API reads them, before anything changes.
        let tests = from_values::<TestInput>(tests.unwrap_or_default())?;
        let replace = from_values::<TestReplacement>(replace.unwrap_or_default())?;
        let retire = from_values::<TestRetirement>(retire.unwrap_or_default())?;
        let patched = patch != PatchWorkflow::default();
        let interface = schema_input.is_some() || schema_output.is_some() || signals.is_some();
        let versioned = code.is_some() || interface || secrets.is_some();
        let tested = !tests.is_empty() || !replace.is_empty() || !retire.is_empty();
        if !patched && !versioned && !tested {
            return Err(InputError::new(
                "give what changes: a `name` or a `description`; `code`, an interface (`input`, `output`, `signals`) or `secrets` for a new version; or `tests`, `replace` and `retire` for its suite.",
            )
            .into());
        }
        // The workflow, by its id from here on: a new name does not lose it.
        let before = if patched {
            self.api.update_workflow(&reference, &patch).await?
        } else {
            self.api.get_workflow(&reference).await?
        };
        if !versioned && !tested {
            let content = describe_update(&before, &patch, Updated::Patched, &self.limits);
            return Ok(AgentToolResult {
                workflow: Some(before),
                ..result(content, false)
            });
        }
        let latest = before.versions.first().unwrap_or(&before.version);
        let latest_number = latest.version;
        let written = if versioned {
            // Each part of the interface left out keeps its latest version's.
            let (input, output, signals) = if interface {
                (
                    schema_input.or_else(|| latest.input.clone()),
                    schema_output.or_else(|| latest.output.clone()),
                    signals.or_else(|| latest.signals.clone()),
                )
            } else {
                (None, None, None)
            };
            let version = CreateVersion {
                // New code imports the actions of the tools' action set.
                action_set: code.as_ref().and(self.action_set.clone()),
                code,
                input,
                output,
                signals,
                secrets,
                tests,
                replace,
                retire,
            };
            self.api.create_version(&before.id, &version).await
        } else {
            let change = ChangeTests {
                tests,
                replace,
                retire,
            };
            self.api.change_tests(&before.id, &change).await
        };
        let problem = match written {
            Ok(after) => {
                let updated = if versioned {
                    Updated::Version {
                        made: after.version.version != latest_number,
                    }
                } else {
                    Updated::Tests
                };
                let content = describe_update(&after, &patch, updated, &self.limits);
                return Ok(AgentToolResult {
                    workflow: Some(after),
                    ..result(content, false)
                });
            }
            Err(ApiError::Problem(problem)) if is_refusal(&problem) => problem,
            Err(error) => return Err(error.into()),
        };
        // What the patch changed stays changed: the model reads it first.
        let done = describe_patch(&patch).map(|patched| {
            format!(
                "Updated the workflow {}: {patched}.",
                workflow_label(&before)
            )
        });
        let refused = if problem.is(ProblemType::TestsFailed) {
            describe_gate_refused(
                &problem.detail,
                &problem.tests,
                workflow_reference(&before),
                versioned,
                latest_number,
                &self.limits,
            )
        } else {
            let what = if problem.is(ProblemType::InvalidCode) {
                "No version was made: the code was refused before anything ran"
            } else if versioned {
                "No version was made"
            } else {
                "The suite was not changed"
            };
            describe_refused(what, &problem.detail, &problem.diagnostics, &self.limits)
        };
        let content = match done {
            Some(done) => format!("{done}\n{refused}"),
            None => refused,
        };
        if problem.is(ProblemType::TestsFailed) || problem.is(ProblemType::InvalidCode) {
            return Ok(AgentToolResult {
                workflow: Some(before),
                tests: problem
                    .is(ProblemType::TestsFailed)
                    .then(|| problem.tests.clone()),
                code: problem
                    .is(ProblemType::InvalidCode)
                    .then_some(CodeVerdict::Refused),
                ..result(content, true)
            });
        }
        Err(Failure::Refused(content))
    }

    /// A workflow for the model to repair or extend: who it is, its version's code and
    /// interface, its suite with each test (a test it may not read is said so), the tests that
    /// left it, and its versions.
    pub(crate) async fn get_workflow(&self, input: &Values) -> Handled {
        let reference = require_string(input, "workflow")?;
        let workflow = self.api.get_workflow(&reference).await?;
        // Collected first: the future that reads them stays `Send`.
        let suite: Vec<_> = workflow
            .suite
            .iter()
            .filter(|entry| entry.retired_at.is_none())
            .cloned()
            .collect();
        let tests = stream::iter(suite)
            .map(|entry| async move {
                match self.api.get_test(&entry.digest).await {
                    Ok(read) => Ok(ReadTest {
                        name: entry.name,
                        digest: entry.digest,
                        test: Some(read),
                        unreadable: None,
                    }),
                    Err(ApiError::Problem(problem)) if matches!(problem.status, 403 | 404) => {
                        Ok(ReadTest {
                            name: entry.name,
                            digest: entry.digest,
                            test: None,
                            unreadable: Some(problem.detail),
                        })
                    }
                    Err(error) => Err(error),
                }
            })
            .buffered(8)
            .try_collect::<Vec<_>>()
            .await?;
        let details = WorkflowDetails {
            workflow,
            tests,
            updates: self.updates,
        };
        let content = describe_workflow_details(&details, &self.limits);
        Ok(AgentToolResult {
            workflow: Some(details.workflow),
            ..result(content, false)
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
            describe_recorded(&workflow, &recorded, self.updates, &self.limits),
            false,
        ))
    }

    /// A run's report: its output when it completed, its transcript when asked.
    async fn report(&self, run: Run, error_when_failed: bool, transcript: bool) -> Handled {
        let output = if run.status == RunStatus::Completed {
            Some(self.output_of(&run).await?)
        } else {
            None
        };
        let transcript = if transcript {
            Some(self.api.run_transcript(&run.id).await?.text)
        } else {
            None
        };
        let ended = matches!(
            run.status,
            RunStatus::Failed | RunStatus::Cancelled | RunStatus::Aborted
        );
        let content = describe_run(&run, &RunExtras { output, transcript }, &self.limits);
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
        let value = self.api.get_payload(&payload.digest, &run.id).await?;
        Ok(json(&value))
    }

    /// A key the server already knows returns the run it started, whoever started it. The key
    /// of a run is scoped by the agent's labels, so this is a safety net: an agent without
    /// labels, or two agents with the same labels whose tool call ids repeat, would get another
    /// conversation's run, or the run of an earlier code. Such a run is the harness's fault,
    /// never shown to the model: it must carry the agent's labels and be a run of this very
    /// code, or of the workflow named.
    fn check_answered(
        &self,
        run: &Run,
        started: Started<'_>,
        key: Option<&IdempotencyKey>,
    ) -> Result<(), Failure> {
        let same = match started {
            Started::Code(code) => run.code_digest == sha256_hex(code),
            Started::Workflow(reference) => run
                .workflow
                .as_ref()
                .is_some_and(|workflow| names(workflow, reference)),
        };
        if key.is_none() || (has_labels(&run.labels, self.labels) && same) {
            return Ok(());
        }
        Err(Failure::Agent(AgentError::Harness(format!(
            "the tool call id {:?} is already the idempotency key of another run (of another \
             code or workflow, or with other labels): give each tool call a unique id",
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

    fn needs_action_set(&self, tool: &str) -> Result<String, Failure> {
        self.action_set.clone().ok_or_else(|| {
            Failure::Agent(AgentError::Options(format!("{tool} needs an action set")))
        })
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
    /// The action set the model's code imports from `rewind:actions`: its runs, checks and
    /// tests, and the versions `update_workflow` makes of new code, use it. `search_actions`
    /// needs one.
    pub action_set: Option<String>,
    /// Set on every run the model starts. They scope `list_runs`, and the tools that take a run
    /// id refuse a run that does not carry them.
    pub labels: BTreeMap<String, String>,
    /// The tools to give the model; all of them by default, those that need an action set only with
    /// one.
    pub tools: Option<Vec<String>>,
    /// How long `run_workflow`, `retry_run` and `cancel_run` wait for the run to end: 30 s.
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
            action_set: None,
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
}

impl<A: Api> AgentTools<A> {
    /// The tools of `options.tools`, or all of them (those that need an action set only with one).
    pub fn new(api: A, options: AgentOptions) -> Result<Self, AgentError> {
        let has_action_set = options.action_set.is_some();
        let names: Vec<&'static str> = match &options.tools {
            None => AGENT_TOOLS
                .into_iter()
                .filter(|name| has_action_set || !NEEDS_ACTION_SET.contains(name))
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
            .find(|name| !has_action_set && NEEDS_ACTION_SET.contains(*name))
        {
            return Err(AgentError::Options(format!("{name} needs an action set")));
        }
        let wait = options.wait.min(options.max_wait);
        Ok(Self {
            limits: Limits::for_output(options.output_limit),
            options: AgentOptions { wait, ..options },
            api,
            names,
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
            action_set: self.options.action_set.clone(),
            labels: &self.options.labels,
            limits: self.limits,
            default_wait: self.options.wait,
            max_wait: self.options.max_wait,
            ttl: self.options.ttl,
            call,
            updates: self.has("update_workflow"),
        };
        let (handled, carried) = match crate::input::parse(name, input) {
            Ok(values) => {
                let handled = match name {
                    "run_workflow" => session.run_workflow(&values).await,
                    "check_workflow" => session.check_workflow(&values).await,
                    "test_workflow" => session.test_workflow(&values).await,
                    "get_run" => session.get_run(&values).await,
                    "list_runs" => session.list_runs(&values).await,
                    "signal_run" => session.signal_run(&values).await,
                    "cancel_run" => session.cancel_run(&values).await,
                    "retry_run" => session.retry_run(&values).await,
                    "search_actions" => session.search_actions(&values).await,
                    "list_secrets" => {
                        let namespace = self.api.namespace();
                        session.list_secrets(namespace.as_deref()).await
                    }
                    "update_workflow" => session.update_workflow(&values).await,
                    "get_workflow" => session.get_workflow(&values).await,
                    "record_tests" => session.record_tests(&values).await,
                    other => Err(Failure::Agent(AgentError::UnknownTool(other.to_owned()))),
                };
                (handled, carries_code(name, &values))
            }
            Err(error) => (Err(error.into()), false),
        };
        finish(name, carried, handled, &self.limits)
    }
}
