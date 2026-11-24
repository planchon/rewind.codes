//! [`Api`] and [`McpApi`] over HTTP: `rewind_api::Client`, with its Bearer, its
//! `Rewind-Organization` and its `Rewind-Namespace`. The CLI's `rewind mcp` reaches the embedded
//! server through it, and `rewind-server` the workers.

use rewind_api::action_sets::{
    ActionSearchResults, ActionSet, ActionSetList, CreateActionSet, SearchQuery,
};
use rewind_api::common::Duration;
use rewind_api::events::Entry;
use rewind_api::headers::IdempotencyKey;
use rewind_api::namespaces::{NamespaceList, NamespaceListQuery, NamespaceName};
use rewind_api::runs::{
    CancelRun, ForkRun, Run, RunFilter, RunPage, SignalAccepted, StartRun, Trace, TraceFormat,
};
use rewind_api::scripts::{CheckRequest, CheckResult, Script, TestRequest};
use rewind_api::secrets::SecretList;
use rewind_api::testing::{Test, TestRun};
use rewind_api::workflows::{
    CreateWorkflow, RecordTests, RecordedTests, Workflow, WorkflowVersions,
};
use rewind_api::{Client, Error};
use serde_json::Value;

use crate::api::{Api, ApiError, McpApi};

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::Problem(problem) => ApiError::Problem(problem),
            other => ApiError::fault(other),
        }
    }
}

impl Api for Client {
    async fn start_run(
        &self,
        start: &StartRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, ApiError> {
        Ok(Client::start_run(self, start, key).await?)
    }

    async fn get_run(&self, run: &str, wait: Option<Duration>) -> Result<Run, ApiError> {
        Ok(Client::get_run(self, run, wait).await?)
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<RunPage, ApiError> {
        Ok(Client::list_runs(self, filter).await?)
    }

    async fn run_trace(&self, run: &str) -> Result<Trace, ApiError> {
        Ok(Client::run_trace(self, run, TraceFormat::Llm).await?)
    }

    async fn run_entry(&self, run: &str, idx: u64) -> Result<Entry, ApiError> {
        Ok(Client::run_entry(self, run, idx).await?)
    }

    async fn signal_run(
        &self,
        run: &str,
        name: &str,
        payload: &Value,
        key: Option<&IdempotencyKey>,
    ) -> Result<SignalAccepted, ApiError> {
        Ok(Client::signal_run(self, run, name, payload, key).await?)
    }

    async fn cancel_run(&self, run: &str, cancel: &CancelRun) -> Result<Run, ApiError> {
        Ok(Client::cancel_run(self, run, cancel).await?)
    }

    async fn fork_run(
        &self,
        run: &str,
        fork: &ForkRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, ApiError> {
        Ok(Client::fork_run(self, run, fork, key).await?)
    }

    async fn check(&self, check: &CheckRequest) -> Result<CheckResult, ApiError> {
        Ok(Client::check(self, check).await?)
    }

    async fn test(&self, test: &TestRequest) -> Result<TestRun, ApiError> {
        Ok(Client::test(self, test).await?)
    }

    async fn get_script(&self, digest: &str) -> Result<Script, ApiError> {
        Ok(Client::get_script(self, digest).await?)
    }

    async fn get_payload(&self, digest: &str, run: &str) -> Result<Value, ApiError> {
        Ok(Client::get_payload(self, digest, run).await?)
    }

    async fn get_test(&self, digest: &str) -> Result<Test, ApiError> {
        Ok(Client::get_test(self, digest).await?)
    }

    async fn create_action_set(&self, action_set: &CreateActionSet) -> Result<ActionSet, ApiError> {
        Ok(Client::create_action_set(self, action_set).await?)
    }

    async fn search_actions(
        &self,
        action_set: &str,
        query: &SearchQuery,
    ) -> Result<ActionSearchResults, ApiError> {
        Ok(Client::search_actions(self, action_set, query).await?)
    }

    async fn create_workflow(&self, workflow: &CreateWorkflow) -> Result<Workflow, ApiError> {
        Ok(Client::create_workflow(self, workflow).await?)
    }

    async fn workflow_versions(&self, name: &str) -> Result<WorkflowVersions, ApiError> {
        Ok(Client::workflow_versions(self, name).await?)
    }

    async fn record_tests(
        &self,
        workflow: &str,
        record: &RecordTests,
    ) -> Result<RecordedTests, ApiError> {
        Ok(Client::record_tests(self, workflow, record).await?)
    }

    async fn list_secrets(&self) -> Result<SecretList, ApiError> {
        Ok(Client::secrets(self).await?)
    }

    async fn namespace_secrets(&self, namespace: &str) -> Result<SecretList, ApiError> {
        let namespace: NamespaceName = namespace.parse().map_err(ApiError::fault)?;
        Ok(Client::namespace_secrets(self, &namespace).await?)
    }

    fn namespace(&self) -> Option<String> {
        Client::namespace(self).map(|namespace| namespace.as_str().to_owned())
    }
}

impl McpApi for Client {
    async fn list_namespaces(&self, query: &NamespaceListQuery) -> Result<NamespaceList, ApiError> {
        Ok(Client::list_namespaces(self, query).await?)
    }

    async fn list_action_sets(&self) -> Result<ActionSetList, ApiError> {
        Ok(Client::list_action_sets(self).await?)
    }
}
