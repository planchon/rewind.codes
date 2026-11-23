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
use rewind_api::common::Duration;
use rewind_api::events::Entry;
use rewind_api::headers::IdempotencyKey;
use rewind_api::namespaces::{NamespaceList, NamespaceListQuery};
use rewind_api::runs::{
    CancelRun, ForkRun, Run, RunFilter, RunPage, SignalAccepted, StartRun, Trace,
};
use rewind_api::scripts::{CheckRequest, CheckResult, Script, TestRequest};
use rewind_api::secrets::SecretList;
use rewind_api::testing::{Test, TestRun};
use rewind_api::toolsets::{CreateToolset, SearchQuery, ToolSearchResults, Toolset, ToolsetList};
use rewind_api::workflows::{
    CreateWorkflow, RecordTests, RecordedTests, Workflow, WorkflowVersions,
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

    /// `GET /v1/runs/{id}/trace?format=llm`.
    fn run_trace(&self, run: &str) -> impl Future<Output = Result<Trace, ApiError>> + Send;

    /// `GET /v1/runs/{id}/entries/{idx}`.
    fn run_entry(
        &self,
        run: &str,
        idx: u64,
    ) -> impl Future<Output = Result<Entry, ApiError>> + Send;

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
    fn test(&self, test: &TestRequest) -> impl Future<Output = Result<TestRun, ApiError>> + Send;

    /// `GET /v1/scripts/{digest}`.
    fn get_script(&self, digest: &str) -> impl Future<Output = Result<Script, ApiError>> + Send;

    /// `GET /v1/payloads/{digest}`, through `run`, which holds it.
    fn get_payload(
        &self,
        digest: &str,
        run: &str,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    /// `GET /v1/tests/{digest}`.
    fn get_test(&self, digest: &str) -> impl Future<Output = Result<Test, ApiError>> + Send;

    /// `POST /v1/toolsets`.
    fn create_toolset(
        &self,
        toolset: &CreateToolset,
    ) -> impl Future<Output = Result<Toolset, ApiError>> + Send;

    /// `GET /v1/toolsets/{id}/search`.
    fn search_tools(
        &self,
        toolset: &str,
        query: &SearchQuery,
    ) -> impl Future<Output = Result<ToolSearchResults, ApiError>> + Send;

    /// `POST /v1/workflows`.
    fn create_workflow(
        &self,
        workflow: &CreateWorkflow,
    ) -> impl Future<Output = Result<Workflow, ApiError>> + Send;

    /// `GET /v1/workflows/{name}`: its versions, newest first.
    fn workflow_versions(
        &self,
        name: &str,
    ) -> impl Future<Output = Result<WorkflowVersions, ApiError>> + Send;

    /// `POST /v1/workflows/{name}/record`.
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

    /// `GET /v1/toolsets`.
    fn list_toolsets(&self) -> impl Future<Output = Result<ToolsetList, ApiError>> + Send;
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

    fn run_trace(&self, run: &str) -> impl Future<Output = Result<Trace, ApiError>> + Send {
        (**self).run_trace(run)
    }

    fn run_entry(
        &self,
        run: &str,
        idx: u64,
    ) -> impl Future<Output = Result<Entry, ApiError>> + Send {
        (**self).run_entry(run, idx)
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

    fn test(&self, test: &TestRequest) -> impl Future<Output = Result<TestRun, ApiError>> + Send {
        (**self).test(test)
    }

    fn get_script(&self, digest: &str) -> impl Future<Output = Result<Script, ApiError>> + Send {
        (**self).get_script(digest)
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

    fn create_toolset(
        &self,
        toolset: &CreateToolset,
    ) -> impl Future<Output = Result<Toolset, ApiError>> + Send {
        (**self).create_toolset(toolset)
    }

    fn search_tools(
        &self,
        toolset: &str,
        query: &SearchQuery,
    ) -> impl Future<Output = Result<ToolSearchResults, ApiError>> + Send {
        (**self).search_tools(toolset, query)
    }

    fn create_workflow(
        &self,
        workflow: &CreateWorkflow,
    ) -> impl Future<Output = Result<Workflow, ApiError>> + Send {
        (**self).create_workflow(workflow)
    }

    fn workflow_versions(
        &self,
        name: &str,
    ) -> impl Future<Output = Result<WorkflowVersions, ApiError>> + Send {
        (**self).workflow_versions(name)
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

    fn list_toolsets(&self) -> impl Future<Output = Result<ToolsetList, ApiError>> + Send {
        (**self).list_toolsets()
    }
}
