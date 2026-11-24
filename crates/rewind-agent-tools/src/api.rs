//! The port of the agent tools: the operations of `/v1` they call, with `rewind-api`'s DTOs.
//!
//! [`Api`] holds what the SDK's set calls (`AgentToolsClient` in
//! `sdk/typescript/src/agent-tools/agent-tools.ts`), [`McpApi`] adds what the MCP set calls to
//! find what a caller reaches. An implementation acts as one caller, in one organization, for one
//! namespace or none: `rewind_api::Client` with its Bearer, `Rewind-Organization` and
//! `Rewind-Namespace` (this crate's `client` module), or a test world that answers as the server
//! does (the evals' engine in process).

use std::future::Future;

use rewind_api::Problem;
use rewind_api::action_sets::{ActionSearchResults, ActionSetList, SearchQuery};
use rewind_api::checks::{CheckRequest, CheckResult, TestRequest};
use rewind_api::common::Duration;
use rewind_api::events::HistoryEvent;
use rewind_api::headers::IdempotencyKey;
use rewind_api::namespaces::{NamespaceList, NamespaceListQuery};
use rewind_api::runs::{
    CancelRun, ForkRun, Run, RunFilter, RunPage, SignalAccepted, StartRun, Transcript,
};
use rewind_api::secrets::SecretList;
use rewind_api::testing::{Test, TestReport};
use rewind_api::workflows::{
    ChangeTests, CreateVersion, PatchWorkflow, RecordTests, RecordedTests, Workflow,
};
use serde_json::Value;

/// What an operation of [`Api`] fails with.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The API answered with a problem (`application/problem+json`). A 4xx but 401 is a mistake
    /// the model can fix, and the tools answer it with `is_error`; a 401 or a 5xx is a fault of
    /// the platform.
    #[error("{0}")]
    Problem(Box<Problem>),
    /// The API gave no answer: unreachable, an answer that cannot be read, the failure of what
    /// stands in for the server.
    #[error(transparent)]
    Fault(Box<dyn std::error::Error + Send + Sync>),
}

impl ApiError {
    /// A fault, from any error.
    pub fn fault(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Fault(error.into())
    }

    /// The problem the API answered with, if that is what failed.
    pub fn problem(&self) -> Option<&Problem> {
        match self {
            Self::Problem(problem) => Some(problem),
            Self::Fault(_) => None,
        }
    }
}

impl From<Problem> for ApiError {
    fn from(problem: Problem) -> Self {
        Self::Problem(Box::new(problem))
    }
}

/// The operations of `/v1` the SDK's agent tools call, one method each, as one caller.
pub trait Api: Send + Sync {
    /// `POST /v1/runs`, waiting up to `start.wait` for the run to end.
    fn start_run(
        &self,
        start: &StartRun,
        key: Option<&IdempotencyKey>,
    ) -> impl Future<Output = Result<Run, ApiError>> + Send;

    /// `GET /v1/runs/{id}`, waiting up to `wait` for the run to end.
    fn get_run(
        &self,
        run: &str,
        wait: Option<Duration>,
    ) -> impl Future<Output = Result<Run, ApiError>> + Send;

    /// `GET /v1/runs`.
    fn list_runs(
        &self,
        filter: &RunFilter,
    ) -> impl Future<Output = Result<RunPage, ApiError>> + Send;

    /// `GET /v1/runs/{id}/transcript`.
    fn run_transcript(
        &self,
        run: &str,
    ) -> impl Future<Output = Result<Transcript, ApiError>> + Send;

    /// `GET /v1/runs/{id}/events/{idx}`.
    fn run_event(
        &self,
        run: &str,
        idx: u64,
    ) -> impl Future<Output = Result<HistoryEvent, ApiError>> + Send;

    /// `POST /v1/runs/{id}/signals/{name}`.
    fn signal_run(
        &self,
        run: &str,
        name: &str,
        payload: &Value,
        key: Option<&IdempotencyKey>,
    ) -> impl Future<Output = Result<SignalAccepted, ApiError>> + Send;

    /// `POST /v1/runs/{id}/cancel`, waiting up to `cancel.wait` for the run to end.
    fn cancel_run(
        &self,
        run: &str,
        cancel: &CancelRun,
    ) -> impl Future<Output = Result<Run, ApiError>> + Send;

    /// `POST /v1/runs/{id}/fork`, waiting up to `fork.wait` for the new run to end.
    fn fork_run(
        &self,
        run: &str,
        fork: &ForkRun,
        key: Option<&IdempotencyKey>,
    ) -> impl Future<Output = Result<Run, ApiError>> + Send;

    /// `POST /v1/check`.
    fn check(
        &self,
        check: &CheckRequest,
    ) -> impl Future<Output = Result<CheckResult, ApiError>> + Send;

    /// `POST /v1/test`.
    fn test(&self, test: &TestRequest)
    -> impl Future<Output = Result<TestReport, ApiError>> + Send;

    /// `GET /v1/payloads/{digest}`, through `run`, which holds it.
    fn get_payload(
        &self,
        digest: &str,
        run: &str,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    /// `GET /v1/tests/{digest}`.
    fn get_test(&self, digest: &str) -> impl Future<Output = Result<Test, ApiError>> + Send;

    /// `GET /v1/action-sets/{id}/search`.
    fn search_actions(
        &self,
        action_set: &str,
        query: &SearchQuery,
    ) -> impl Future<Output = Result<ActionSearchResults, ApiError>> + Send;

    /// `GET /v1/workflows/{id}`: a workflow by its id or its name (`@v12` or `@<digest>` for a
    /// version), with its versions, its suite and the code of the version shown.
    fn get_workflow(
        &self,
        workflow: &str,
    ) -> impl Future<Output = Result<Workflow, ApiError>> + Send;

    /// `PATCH /v1/workflows/{id}`: its name and description.
    fn update_workflow(
        &self,
        workflow: &str,
        patch: &PatchWorkflow,
    ) -> impl Future<Output = Result<Workflow, ApiError>> + Send;

    /// `POST /v1/workflows/{id}/versions`: its next version, through its gate.
    fn create_version(
        &self,
        workflow: &str,
        version: &CreateVersion,
    ) -> impl Future<Output = Result<Workflow, ApiError>> + Send;

    /// `POST /v1/workflows/{id}/tests`: tests added to its suite, replaced or retired.
    fn change_tests(
        &self,
        workflow: &str,
        tests: &ChangeTests,
    ) -> impl Future<Output = Result<Workflow, ApiError>> + Send;

    /// `POST /v1/workflows/{id}/record`.
    fn record_tests(
        &self,
        workflow: &str,
        record: &RecordTests,
    ) -> impl Future<Output = Result<RecordedTests, ApiError>> + Send;

    /// `GET /v1/secrets`: the organization's secrets the caller sees, never a value.
    fn list_secrets(&self) -> impl Future<Output = Result<SecretList, ApiError>> + Send;

    /// `GET /v1/namespaces/{name}/secrets`: the namespace's secrets, never a value.
    fn namespace_secrets(
        &self,
        namespace: &str,
    ) -> impl Future<Output = Result<SecretList, ApiError>> + Send;

    /// The namespace this API acts for (`Rewind-Namespace`), whose secrets `list_secrets` lists;
    /// `None` when it names none, or does not know it (an end user's token holds its own).
    fn namespace(&self) -> Option<String> {
        None
    }
}

/// What the MCP set calls beyond the SDK's: what the caller reaches in the organization.
pub trait McpApi: Api {
    /// `GET /v1/namespaces`.
    fn list_namespaces(
        &self,
        query: &NamespaceListQuery,
    ) -> impl Future<Output = Result<NamespaceList, ApiError>> + Send;

    /// `GET /v1/action-sets`.
    fn list_action_sets(&self) -> impl Future<Output = Result<ActionSetList, ApiError>> + Send;
}

/// A borrowed API is an API: a harness keeps its world, and lends it to the tools.
impl<A: Api> Api for &A {
    fn start_run(
        &self,
        start: &StartRun,
        key: Option<&IdempotencyKey>,
    ) -> impl Future<Output = Result<Run, ApiError>> + Send {
        (**self).start_run(start, key)
    }

    fn get_run(
        &self,
        run: &str,
        wait: Option<Duration>,
    ) -> impl Future<Output = Result<Run, ApiError>> + Send {
        (**self).get_run(run, wait)
    }

    fn list_runs(
        &self,
        filter: &RunFilter,
    ) -> impl Future<Output = Result<RunPage, ApiError>> + Send {
        (**self).list_runs(filter)
    }

    fn run_transcript(
        &self,
        run: &str,
    ) -> impl Future<Output = Result<Transcript, ApiError>> + Send {
        (**self).run_transcript(run)
    }

    fn run_event(
        &self,
        run: &str,
        idx: u64,
    ) -> impl Future<Output = Result<HistoryEvent, ApiError>> + Send {
        (**self).run_event(run, idx)
    }

    fn signal_run(
        &self,
        run: &str,
        name: &str,
        payload: &Value,
        key: Option<&IdempotencyKey>,
    ) -> impl Future<Output = Result<SignalAccepted, ApiError>> + Send {
        (**self).signal_run(run, name, payload, key)
    }

    fn cancel_run(
        &self,
        run: &str,
        cancel: &CancelRun,
    ) -> impl Future<Output = Result<Run, ApiError>> + Send {
        (**self).cancel_run(run, cancel)
    }

    fn fork_run(
        &self,
        run: &str,
        fork: &ForkRun,
        key: Option<&IdempotencyKey>,
    ) -> impl Future<Output = Result<Run, ApiError>> + Send {
        (**self).fork_run(run, fork, key)
    }

    fn check(
        &self,
        check: &CheckRequest,
    ) -> impl Future<Output = Result<CheckResult, ApiError>> + Send {
        (**self).check(check)
    }

    fn test(
        &self,
        test: &TestRequest,
    ) -> impl Future<Output = Result<TestReport, ApiError>> + Send {
        (**self).test(test)
    }

    fn get_payload(
        &self,
        digest: &str,
        run: &str,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send {
        (**self).get_payload(digest, run)
    }

    fn get_test(&self, digest: &str) -> impl Future<Output = Result<Test, ApiError>> + Send {
        (**self).get_test(digest)
    }

    fn search_actions(
        &self,
        action_set: &str,
        query: &SearchQuery,
    ) -> impl Future<Output = Result<ActionSearchResults, ApiError>> + Send {
        (**self).search_actions(action_set, query)
    }

    fn get_workflow(
        &self,
        workflow: &str,
    ) -> impl Future<Output = Result<Workflow, ApiError>> + Send {
        (**self).get_workflow(workflow)
    }

    fn update_workflow(
        &self,
        workflow: &str,
        patch: &PatchWorkflow,
    ) -> impl Future<Output = Result<Workflow, ApiError>> + Send {
        (**self).update_workflow(workflow, patch)
    }

    fn create_version(
        &self,
        workflow: &str,
        version: &CreateVersion,
    ) -> impl Future<Output = Result<Workflow, ApiError>> + Send {
        (**self).create_version(workflow, version)
    }

    fn change_tests(
        &self,
        workflow: &str,
        tests: &ChangeTests,
    ) -> impl Future<Output = Result<Workflow, ApiError>> + Send {
        (**self).change_tests(workflow, tests)
    }

    fn record_tests(
        &self,
        workflow: &str,
        record: &RecordTests,
    ) -> impl Future<Output = Result<RecordedTests, ApiError>> + Send {
        (**self).record_tests(workflow, record)
    }

    fn list_secrets(&self) -> impl Future<Output = Result<SecretList, ApiError>> + Send {
        (**self).list_secrets()
    }

    fn namespace_secrets(
        &self,
        namespace: &str,
    ) -> impl Future<Output = Result<SecretList, ApiError>> + Send {
        (**self).namespace_secrets(namespace)
    }

    fn namespace(&self) -> Option<String> {
        (**self).namespace()
    }
}

impl<A: McpApi> McpApi for &A {
    fn list_namespaces(
        &self,
        query: &NamespaceListQuery,
    ) -> impl Future<Output = Result<NamespaceList, ApiError>> + Send {
        (**self).list_namespaces(query)
    }

    fn list_action_sets(&self) -> impl Future<Output = Result<ActionSetList, ApiError>> + Send {
        (**self).list_action_sets()
    }
}
