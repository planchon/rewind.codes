//! A thin async client of the API `/v1`: one method per operation, nothing else. Used by
//! `rewind-cli`, against a remote server with a Bearer token, or the embedded server's unix
//! socket without one. The dashboard's sign-in (`/auth`) is for a browser, not for this client.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::{Stream, StreamExt};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sse_stream::SseByteStream;

use crate::Error;
use crate::access::{
    ApiKey, ApiKeyList, CreateApiKey, Grant, PermissionList, PutRole, Role, RoleList, SetGrant,
};
use crate::admin::ServerInfo;
use crate::common::Duration;
use crate::events::{Entry, EventsQuery, StreamEvent};
use crate::headers::{IDEMPOTENCY_KEY, IdempotencyKey, NAMESPACE, ORGANIZATION};
use crate::namespaces::{
    CreateToken, CreateTokenKey, NamespaceDefaults, NamespaceDetail, NamespaceList,
    NamespaceListQuery, NamespaceName, NamespaceQuery, PutCeiling, PutNamespace,
    PutNamespaceDefaults, QuotaAlertList, QuotaAlertQuery, RevokedTokens, Token, TokenKey,
    TokenKeyList,
};
use crate::organization::{
    CreateInvitation, CreateOrganization, Identity, Invitation, InvitationList, MemberList,
    Organization, OrganizationSettings, UpdateOrganization,
};
use crate::problem::Problem;
use crate::reading::{PayloadQuery, ToolStatsFilter, ToolStatsPage};
use crate::routes;
use crate::runs::{
    CancelRun, FailureGroupPage, FailureQuery, ForkRun, GetRunQuery, JournalExport, JournalQuery,
    Run, RunFilter, RunPage, RunStats, RunStatsQuery, SignalAccepted, StartRun, Trace, TraceFormat,
    TraceQuery,
};
use crate::scripts::{CheckRequest, CheckResult, Script, ScriptFilter, ScriptPage, TestRequest};
use crate::tasks::{
    CompleteTask, DeferTask, FailTask, HeartbeatAck, RejectTask, TaskBatch, TaskQuery,
};
use crate::toolsets::{
    CreateToolset, Credentials, DeclarationFormat, DeclarationsQuery, DeclarationsText,
    SearchQuery, ToolSearchResults, Toolset, ToolsetList,
};
use crate::webhooks::{CreateSubscription, Subscription, SubscriptionList};

/// The base URL of the embedded server: requests go to its socket, whatever the host.
const EMBEDDED_BASE: &str = "http://localhost";

/// A client of one Rewind server. Cheap to clone: clones share their connections.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    api_key: Option<HeaderValue>,
    /// Sent as `Rewind-Namespace` on every request.
    namespace: Option<NamespaceName>,
    /// Sent as `Rewind-Organization` on every request.
    organization: Option<HeaderValue>,
}

/// The API key is never printed.
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base.as_str())
            .field("api_key", &self.api_key.as_ref().map(|_| "…"))
            .field("namespace", &self.namespace)
            .field("organization", &self.organization)
            .finish()
    }
}

impl Client {
    /// A client of the server at `base_url` (`https://rewind.example.com`, possibly with a path
    /// prefix), authenticated with `api_key` when given: any Bearer the API takes, an API key
    /// (`rewind_sk_…`), an end user's token (`rewind_nt_…` or a JWT) or a connection's access
    /// token (`rewind_ct_…`, with [`with_organization`](Self::with_organization)).
    pub fn remote(base_url: &str, api_key: Option<&str>) -> Result<Self, Error> {
        let base = Url::parse(base_url)
            .map_err(|error| Error::Config(format!("invalid base URL {base_url:?}: {error}")))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(Error::Config(format!(
                "the base URL must be http or https, not {base_url:?}"
            )));
        }
        let api_key = api_key
            .map(|key| {
                let mut value = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                    Error::invalid("API key", "it holds characters a header cannot")
                })?;
                value.set_sensitive(true);
                Ok::<_, Error>(value)
            })
            .transpose()?;
        Ok(Self {
            http: builder().build().map_err(config_error)?,
            base,
            api_key,
            namespace: None,
            organization: None,
        })
    }

    /// A client of the embedded server listening on this unix socket (`.rewind/server.sock`).
    /// The socket takes no API key.
    #[cfg(unix)]
    pub fn embedded(socket: impl Into<std::path::PathBuf>) -> Result<Self, Error> {
        let http = builder()
            .unix_socket(socket.into())
            .build()
            .map_err(config_error)?;
        Ok(Self {
            http,
            base: Url::parse(EMBEDDED_BASE).map_err(|error| Error::Config(error.to_string()))?,
            api_key: None,
            namespace: None,
            organization: None,
        })
    }

    /// The same client, acting for the end user of `namespace`: every request carries
    /// `Rewind-Namespace`. A key needs it to start a run; on a read, it narrows the answer to
    /// that namespace.
    pub fn with_namespace(&self, namespace: NamespaceName) -> Self {
        Self {
            namespace: Some(namespace),
            ..self.clone()
        }
    }

    /// The namespace this client sends, if any.
    pub fn namespace(&self) -> Option<&NamespaceName> {
        self.namespace.as_ref()
    }

    /// The same client, acting in `organization` (a tenant's id): every request carries
    /// `Rewind-Organization`. A connection's token needs it on the tenant's data; other callers
    /// may repeat their own tenant.
    pub fn with_organization(&self, organization: &str) -> Result<Self, Error> {
        let value = HeaderValue::from_str(organization)
            .map_err(|_| Error::invalid("organization", "it holds characters a header cannot"))?;
        Ok(Self {
            organization: Some(value),
            ..self.clone()
        })
    }

    /// The organization this client sends, if any.
    pub fn organization(&self) -> Option<&str> {
        self.organization
            .as_ref()
            .and_then(|value| value.to_str().ok())
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    // ---------------------------------------------------------------------------------------
    // Toolsets

    /// `POST /v1/toolsets`.
    pub async fn create_toolset(&self, toolset: &CreateToolset) -> Result<Toolset, Error> {
        let request = self
            .request(Method::POST, routes::TOOLSETS, &[])?
            .json(toolset);
        self.json(request).await
    }

    /// `GET /v1/toolsets`.
    pub async fn list_toolsets(&self) -> Result<ToolsetList, Error> {
        let request = self.request(Method::GET, routes::TOOLSETS, &[])?;
        self.json(request).await
    }

    /// `POST /v1/workflows`.
    pub async fn create_workflow(
        &self,
        workflow: &crate::workflows::CreateWorkflow,
    ) -> Result<crate::workflows::Workflow, Error> {
        let request = self
            .request(Method::POST, routes::WORKFLOWS, &[])?
            .json(workflow);
        self.json(request).await
    }

    /// `GET /v1/workflows`.
    pub async fn list_workflows(
        &self,
        query: &crate::workflows::WorkflowQuery,
    ) -> Result<crate::workflows::WorkflowPage, Error> {
        let request = self.request_with(Method::GET, routes::WORKFLOWS, &[], query)?;
        self.json(request).await
    }

    /// `GET /v1/workflows/{name}`.
    pub async fn workflow_versions(
        &self,
        name: &str,
    ) -> Result<crate::workflows::WorkflowVersions, Error> {
        let request = self.request(Method::GET, routes::WORKFLOW, &[name])?;
        self.json(request).await
    }

    /// `GET /v1/toolsets/{id}/declarations`.
    pub async fn toolset_declarations(
        &self,
        toolset: &str,
        format: DeclarationFormat,
    ) -> Result<DeclarationsText, Error> {
        let query = DeclarationsQuery {
            format: Some(format),
        };
        let request = self.request_with(
            Method::GET,
            routes::TOOLSET_DECLARATIONS,
            &[toolset],
            &query,
        )?;
        self.json(request).await
    }

    /// `GET /v1/toolsets/{id}/search`.
    pub async fn search_tools(
        &self,
        toolset: &str,
        query: &SearchQuery,
    ) -> Result<ToolSearchResults, Error> {
        let request = self.request_with(Method::GET, routes::TOOLSET_SEARCH, &[toolset], query)?;
        self.json(request).await
    }

    /// `PUT /v1/toolsets/{id}/credentials`.
    pub async fn set_toolset_credentials(
        &self,
        toolset: &str,
        credentials: &Credentials,
    ) -> Result<(), Error> {
        let request = self
            .request(Method::PUT, routes::TOOLSET_CREDENTIALS, &[toolset])?
            .json(credentials);
        self.empty(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Scripts

    /// `POST /v1/check`.
    pub async fn check(&self, check: &CheckRequest) -> Result<CheckResult, Error> {
        let request = self.request(Method::POST, routes::CHECK, &[])?.json(check);
        self.json(request).await
    }

    /// `POST /v1/test`.
    pub async fn test(&self, test: &TestRequest) -> Result<crate::testing::TestRun, Error> {
        let request = self.request(Method::POST, routes::TEST, &[])?.json(test);
        self.json(request).await
    }

    /// `POST /v1/workflows/{name}/record`.
    pub async fn record_tests(
        &self,
        workflow: &str,
        record: &crate::workflows::RecordTests,
    ) -> Result<crate::workflows::RecordedTests, Error> {
        let request = self
            .request(Method::POST, routes::WORKFLOW_RECORD, &[workflow])?
            .json(record);
        self.json(request).await
    }

    /// `GET /v1/tests/{digest}`.
    pub async fn get_test(&self, digest: &str) -> Result<crate::testing::Test, Error> {
        let request = self.request(Method::GET, routes::TEST_DOCUMENT, &[digest])?;
        self.json(request).await
    }

    /// `GET /v1/scripts`.
    pub async fn list_scripts(&self, filter: &ScriptFilter) -> Result<ScriptPage, Error> {
        let request = self.request_with(Method::GET, routes::SCRIPTS, &[], filter)?;
        self.json(request).await
    }

    /// `GET /v1/scripts/{digest}`.
    pub async fn get_script(&self, digest: &str) -> Result<Script, Error> {
        let request = self.request(Method::GET, routes::SCRIPT, &[digest])?;
        self.json(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Runs

    /// `POST /v1/runs`.
    pub async fn start_run(
        &self,
        start: &StartRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, Error> {
        let request = with_key(self.request(Method::POST, routes::RUNS, &[])?, key).json(start);
        self.json(request).await
    }

    /// `GET /v1/runs`.
    pub async fn list_runs(&self, filter: &RunFilter) -> Result<RunPage, Error> {
        let request = self.request_with(Method::GET, routes::RUNS, &[], filter)?;
        self.json(request).await
    }

    /// `GET /v1/runs/stats`.
    pub async fn run_stats(&self, query: &RunStatsQuery) -> Result<RunStats, Error> {
        let request = self.request_with(Method::GET, routes::RUN_STATS, &[], query)?;
        self.json(request).await
    }

    /// `GET /v1/runs/failures`.
    pub async fn run_failures(&self, query: &FailureQuery) -> Result<FailureGroupPage, Error> {
        let request = self.request_with(Method::GET, routes::RUN_FAILURES, &[], query)?;
        self.json(request).await
    }

    /// `GET /v1/runs/{id}`, waiting up to `wait` for the run to end.
    pub async fn get_run(&self, run: &str, wait: Option<Duration>) -> Result<Run, Error> {
        let request = self.request_with(Method::GET, routes::RUN, &[run], &GetRunQuery { wait })?;
        self.json(request).await
    }

    /// `DELETE /v1/runs/{id}`.
    pub async fn delete_run(&self, run: &str) -> Result<(), Error> {
        let request = self.request(Method::DELETE, routes::RUN, &[run])?;
        self.empty(request).await
    }

    /// `GET /v1/runs/{id}/trace`.
    pub async fn run_trace(&self, run: &str, format: TraceFormat) -> Result<Trace, Error> {
        let query = TraceQuery {
            format: Some(format),
        };
        let request = self.request_with(Method::GET, routes::RUN_TRACE, &[run], &query)?;
        self.json(request).await
    }

    /// `GET /v1/runs/{id}/entries/{idx}`.
    pub async fn run_entry(&self, run: &str, idx: u64) -> Result<Entry, Error> {
        let idx = idx.to_string();
        let request = self.request(Method::GET, routes::RUN_ENTRY, &[run, &idx])?;
        self.json(request).await
    }

    /// `GET /v1/runs/{id}/events`: the run's journal as a stream of events.
    pub async fn run_events(&self, run: &str, query: &EventsQuery) -> Result<EventStream, Error> {
        let request = self
            .request_with(Method::GET, routes::RUN_EVENTS, &[run], query)?
            .header(ACCEPT, "text/event-stream");
        let url = self.base.to_string();
        let response = self.send(request).await?;
        let events = SseByteStream::new(response.bytes_stream()).filter_map(move |item| {
            let url = url.clone();
            async move {
                match item {
                    // A comment, a keep-alive, a `retry:` only: nothing to read.
                    Ok(sse) => sse.data.map(|data| {
                        serde_json::from_str::<StreamEvent>(&data).map_err(|error| {
                            Error::Decode(format!("invalid event of the stream: {error}"))
                        })
                    }),
                    Err(error) => Some(Err(Error::Transport {
                        url,
                        message: format!("the event stream broke: {error}"),
                    })),
                }
            }
        });
        Ok(EventStream {
            inner: Box::pin(events),
        })
    }

    /// `POST /v1/runs/{id}/signals/{name}`.
    pub async fn signal_run(
        &self,
        run: &str,
        name: &str,
        payload: &Value,
        key: Option<&IdempotencyKey>,
    ) -> Result<SignalAccepted, Error> {
        let request = with_key(
            self.request(Method::POST, routes::RUN_SIGNAL, &[run, name])?,
            key,
        )
        .json(payload);
        self.json(request).await
    }

    /// `POST /v1/runs/{id}/cancel`.
    pub async fn cancel_run(&self, run: &str, cancel: &CancelRun) -> Result<Run, Error> {
        let request = self
            .request(Method::POST, routes::RUN_CANCEL, &[run])?
            .json(cancel);
        self.json(request).await
    }

    /// `POST /v1/runs/{id}/fork`.
    pub async fn fork_run(
        &self,
        run: &str,
        fork: &ForkRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, Error> {
        let request =
            with_key(self.request(Method::POST, routes::RUN_FORK, &[run])?, key).json(fork);
        self.json(request).await
    }

    /// `GET /v1/runs/{id}/journal`: a document of the stored format v1.
    pub async fn export_journal(&self, run: &str, export: JournalExport) -> Result<Value, Error> {
        let query = JournalQuery {
            export_as: Some(export),
        };
        let request = self.request_with(Method::GET, routes::RUN_JOURNAL, &[run], &query)?;
        self.json(request).await
    }

    /// `POST /v1/runs/{id}/release`.
    pub async fn release_run(&self, run: &str) -> Result<Run, Error> {
        let request = self.request(Method::POST, routes::RUN_RELEASE, &[run])?;
        self.json(request).await
    }

    /// `POST /v1/runs/{id}/give-up`.
    pub async fn give_up_run(&self, run: &str) -> Result<Run, Error> {
        let request = self.request(Method::POST, routes::RUN_GIVE_UP, &[run])?;
        self.json(request).await
    }

    /// `PUT /v1/runs/{id}/credentials`.
    pub async fn set_run_credentials(
        &self,
        run: &str,
        credentials: &Credentials,
    ) -> Result<(), Error> {
        let request = self
            .request(Method::PUT, routes::RUN_CREDENTIALS, &[run])?
            .json(credentials);
        self.empty(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Host-tool tasks

    /// `GET /v1/tasks`: claims tasks, waiting up to `query.wait` for one.
    pub async fn claim_tasks(&self, query: &TaskQuery) -> Result<TaskBatch, Error> {
        let request = self.request_with(Method::GET, routes::TASKS, &[], query)?;
        self.json(request).await
    }

    /// `POST /v1/tasks/{token}/heartbeat`.
    pub async fn heartbeat_task(&self, token: &str) -> Result<HeartbeatAck, Error> {
        let request = self.request(Method::POST, routes::TASK_HEARTBEAT, &[token])?;
        self.json(request).await
    }

    /// `POST /v1/tasks/{token}/defer`.
    pub async fn defer_task(&self, token: &str, defer: &DeferTask) -> Result<(), Error> {
        let request = self
            .request(Method::POST, routes::TASK_DEFER, &[token])?
            .json(defer);
        self.empty(request).await
    }

    /// `POST /v1/tasks/{token}/complete`.
    pub async fn complete_task(&self, token: &str, complete: &CompleteTask) -> Result<(), Error> {
        let request = self
            .request(Method::POST, routes::TASK_COMPLETE, &[token])?
            .json(complete);
        self.empty(request).await
    }

    /// `POST /v1/tasks/{token}/fail`.
    pub async fn fail_task(&self, token: &str, fail: &FailTask) -> Result<(), Error> {
        let request = self
            .request(Method::POST, routes::TASK_FAIL, &[token])?
            .json(fail);
        self.empty(request).await
    }

    /// `POST /v1/tasks/{id}/approve`.
    pub async fn approve_task(&self, task: &str) -> Result<(), Error> {
        let request = self.request(Method::POST, routes::TASK_APPROVE, &[task])?;
        self.empty(request).await
    }

    /// `POST /v1/tasks/{id}/reject`.
    pub async fn reject_task(&self, task: &str, reject: &RejectTask) -> Result<(), Error> {
        let request = self
            .request(Method::POST, routes::TASK_REJECT, &[task])?
            .json(reject);
        self.empty(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Webhooks

    /// `POST /hooks/{token}`: what an external system does with a URL from `webhook(name)`.
    /// `delivery_id` becomes the signal's idempotency key.
    pub async fn post_hook(
        &self,
        token: &str,
        body: &Value,
        delivery_id: Option<&IdempotencyKey>,
    ) -> Result<(), Error> {
        let request = with_key(
            self.request(Method::POST, routes::HOOK, &[token])?,
            delivery_id,
        )
        .json(body);
        self.empty(request).await
    }

    /// `POST /v1/subscriptions`.
    pub async fn create_subscription(
        &self,
        subscription: &CreateSubscription,
    ) -> Result<Subscription, Error> {
        let request = self
            .request(Method::POST, routes::SUBSCRIPTIONS, &[])?
            .json(subscription);
        self.json(request).await
    }

    /// `GET /v1/subscriptions`.
    pub async fn list_subscriptions(&self) -> Result<SubscriptionList, Error> {
        let request = self.request(Method::GET, routes::SUBSCRIPTIONS, &[])?;
        self.json(request).await
    }

    /// `DELETE /v1/subscriptions/{id}`.
    pub async fn delete_subscription(&self, subscription: &str) -> Result<(), Error> {
        let request = self.request(Method::DELETE, routes::SUBSCRIPTION, &[subscription])?;
        self.empty(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Reading

    /// `GET /v1/tools/stats`.
    pub async fn tool_stats(&self, filter: &ToolStatsFilter) -> Result<ToolStatsPage, Error> {
        let request = self.request_with(Method::GET, routes::TOOL_STATS, &[], filter)?;
        self.json(request).await
    }

    /// `GET /v1/payloads/{digest}`: the value itself, read through `run` when given (a key
    /// limited to labels must give it).
    pub async fn get_payload(&self, digest: &str, run: Option<&str>) -> Result<Value, Error> {
        let query = PayloadQuery {
            run: run.map(str::to_owned),
        };
        let request = self.request_with(Method::GET, routes::PAYLOAD, &[digest], &query)?;
        self.json(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Organization

    /// `GET /v1/whoami`.
    pub async fn whoami(&self) -> Result<Identity, Error> {
        let request = self.request(Method::GET, routes::WHOAMI, &[])?;
        self.json(request).await
    }

    /// `POST /v1/organizations`.
    pub async fn create_organization(
        &self,
        organization: &CreateOrganization,
    ) -> Result<Organization, Error> {
        let request = self
            .request(Method::POST, routes::ORGANIZATIONS, &[])?
            .json(organization);
        self.json(request).await
    }

    /// `PATCH /v1/organization`.
    pub async fn update_organization(
        &self,
        update: &UpdateOrganization,
    ) -> Result<OrganizationSettings, Error> {
        let request = self
            .request(Method::PATCH, routes::ORGANIZATION, &[])?
            .json(update);
        self.json(request).await
    }

    /// `GET /v1/invitations`: the pending invitations sent to the caller.
    pub async fn my_invitations(&self) -> Result<InvitationList, Error> {
        let request = self.request(Method::GET, routes::INVITATIONS, &[])?;
        self.json(request).await
    }

    /// `POST /v1/invitations/{id}/accept`.
    pub async fn accept_invitation(&self, invitation: &str) -> Result<Invitation, Error> {
        let request = self.request(Method::POST, routes::INVITATION_ACCEPT, &[invitation])?;
        self.json(request).await
    }

    /// `GET /v1/organization/members`.
    pub async fn list_members(&self) -> Result<MemberList, Error> {
        let request = self.request(Method::GET, routes::MEMBERS, &[])?;
        self.json(request).await
    }

    /// `PUT /v1/organization/members/{user}/grant`.
    pub async fn set_member_grant(&self, user: &str, grant: &SetGrant) -> Result<Grant, Error> {
        let request = self
            .request(Method::PUT, routes::MEMBER_GRANT, &[user])?
            .json(grant);
        self.json(request).await
    }

    /// `DELETE /v1/organization/members/{user}`.
    pub async fn remove_member(&self, user: &str) -> Result<(), Error> {
        let request = self.request(Method::DELETE, routes::MEMBER, &[user])?;
        self.empty(request).await
    }

    /// `GET /v1/organization/invitations`.
    pub async fn list_invitations(&self) -> Result<InvitationList, Error> {
        let request = self.request(Method::GET, routes::ORGANIZATION_INVITATIONS, &[])?;
        self.json(request).await
    }

    /// `POST /v1/organization/invitations`.
    pub async fn invite(&self, invitation: &CreateInvitation) -> Result<Invitation, Error> {
        let request = self
            .request(Method::POST, routes::ORGANIZATION_INVITATIONS, &[])?
            .json(invitation);
        self.json(request).await
    }

    /// `POST /v1/organization/invitations/{id}/resend`.
    pub async fn resend_invitation(&self, invitation: &str) -> Result<Invitation, Error> {
        let request = self.request(
            Method::POST,
            routes::ORGANIZATION_INVITATION_RESEND,
            &[invitation],
        )?;
        self.json(request).await
    }

    /// `DELETE /v1/organization/invitations/{id}`.
    pub async fn revoke_invitation(&self, invitation: &str) -> Result<(), Error> {
        let request = self.request(
            Method::DELETE,
            routes::ORGANIZATION_INVITATION,
            &[invitation],
        )?;
        self.empty(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Access

    /// `GET /v1/permissions`.
    pub async fn list_permissions(&self) -> Result<PermissionList, Error> {
        let request = self.request(Method::GET, routes::PERMISSIONS, &[])?;
        self.json(request).await
    }

    /// `GET /v1/roles`.
    pub async fn list_roles(&self) -> Result<RoleList, Error> {
        let request = self.request(Method::GET, routes::ROLES, &[])?;
        self.json(request).await
    }

    /// `PUT /v1/roles/{name}`.
    pub async fn put_role(&self, name: &str, role: &PutRole) -> Result<Role, Error> {
        let request = self.request(Method::PUT, routes::ROLE, &[name])?.json(role);
        self.json(request).await
    }

    /// `DELETE /v1/roles/{name}`.
    pub async fn delete_role(&self, name: &str) -> Result<(), Error> {
        let request = self.request(Method::DELETE, routes::ROLE, &[name])?;
        self.empty(request).await
    }

    /// `GET /v1/api-keys`.
    pub async fn list_api_keys(&self) -> Result<ApiKeyList, Error> {
        let request = self.request(Method::GET, routes::API_KEYS, &[])?;
        self.json(request).await
    }

    /// `POST /v1/api-keys`.
    pub async fn create_api_key(&self, key: &CreateApiKey) -> Result<ApiKey, Error> {
        let request = self.request(Method::POST, routes::API_KEYS, &[])?.json(key);
        self.json(request).await
    }

    /// `DELETE /v1/api-keys/{id}`.
    pub async fn revoke_api_key(&self, key: &str) -> Result<(), Error> {
        let request = self.request(Method::DELETE, routes::API_KEY, &[key])?;
        self.empty(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Namespaces

    /// `GET /v1/namespaces`.
    pub async fn list_namespaces(
        &self,
        query: &NamespaceListQuery,
    ) -> Result<NamespaceList, Error> {
        let request = self.request_with(Method::GET, routes::NAMESPACES, &[], query)?;
        self.json(request).await
    }

    /// `GET /v1/namespaces/{name}`.
    pub async fn get_namespace(
        &self,
        name: &NamespaceName,
        query: &NamespaceQuery,
    ) -> Result<NamespaceDetail, Error> {
        let request = self.request_with(Method::GET, routes::NAMESPACE, &[name.as_str()], query)?;
        self.json(request).await
    }

    /// `PUT /v1/namespaces/{name}`.
    pub async fn put_namespace(
        &self,
        name: &NamespaceName,
        namespace: &PutNamespace,
    ) -> Result<NamespaceDetail, Error> {
        let request = self
            .request(Method::PUT, routes::NAMESPACE, &[name.as_str()])?
            .json(namespace);
        self.json(request).await
    }

    /// `DELETE /v1/namespaces/{name}`: the erasure goes on in the background.
    pub async fn delete_namespace(&self, name: &NamespaceName) -> Result<(), Error> {
        let request = self.request(Method::DELETE, routes::NAMESPACE, &[name.as_str()])?;
        self.empty(request).await
    }

    /// `PUT /v1/namespaces/{name}/toolsets/{id}/credentials`.
    pub async fn set_namespace_credentials(
        &self,
        name: &NamespaceName,
        toolset: &str,
        credentials: &Credentials,
    ) -> Result<(), Error> {
        let request = self
            .request(
                Method::PUT,
                routes::NAMESPACE_CREDENTIALS,
                &[name.as_str(), toolset],
            )?
            .json(credentials);
        self.empty(request).await
    }

    /// `GET /v1/organization/namespace-defaults`.
    pub async fn namespace_defaults(
        &self,
        query: &NamespaceQuery,
    ) -> Result<NamespaceDefaults, Error> {
        let request = self.request_with(Method::GET, routes::NAMESPACE_DEFAULTS, &[], query)?;
        self.json(request).await
    }

    /// `PUT /v1/organization/namespace-defaults`.
    pub async fn put_namespace_defaults(
        &self,
        defaults: &PutNamespaceDefaults,
    ) -> Result<NamespaceDefaults, Error> {
        let request = self
            .request(Method::PUT, routes::NAMESPACE_DEFAULTS, &[])?
            .json(defaults);
        self.json(request).await
    }

    /// `GET /v1/organization/quota-alerts`.
    pub async fn quota_alerts(&self, query: &QuotaAlertQuery) -> Result<QuotaAlertList, Error> {
        let request = self.request_with(Method::GET, routes::QUOTA_ALERTS, &[], query)?;
        self.json(request).await
    }

    /// `POST /v1/namespaces/{name}/tokens`: the answer holds the token, once.
    pub async fn create_token(
        &self,
        name: &NamespaceName,
        token: &CreateToken,
    ) -> Result<Token, Error> {
        let request = self
            .request(Method::POST, routes::NAMESPACE_TOKENS, &[name.as_str()])?
            .json(token);
        self.json(request).await
    }

    /// `DELETE /v1/namespaces/{name}/tokens`.
    pub async fn revoke_namespace_tokens(
        &self,
        name: &NamespaceName,
    ) -> Result<RevokedTokens, Error> {
        let request = self.request(Method::DELETE, routes::NAMESPACE_TOKENS, &[name.as_str()])?;
        self.json(request).await
    }

    /// `DELETE /v1/tokens/{id}`.
    pub async fn revoke_token(&self, token: &str) -> Result<(), Error> {
        let request = self.request(Method::DELETE, routes::TOKEN, &[token])?;
        self.empty(request).await
    }

    /// `GET /v1/organization/token-keys`.
    pub async fn list_token_keys(&self) -> Result<TokenKeyList, Error> {
        let request = self.request(Method::GET, routes::TOKEN_KEYS, &[])?;
        self.json(request).await
    }

    /// `POST /v1/organization/token-keys`.
    pub async fn create_token_key(&self, key: &CreateTokenKey) -> Result<TokenKey, Error> {
        let request = self
            .request(Method::POST, routes::TOKEN_KEYS, &[])?
            .json(key);
        self.json(request).await
    }

    /// `DELETE /v1/organization/token-keys/{kid}`.
    pub async fn delete_token_key(&self, kid: &str) -> Result<(), Error> {
        let request = self.request(Method::DELETE, routes::TOKEN_KEY, &[kid])?;
        self.empty(request).await
    }

    /// `PUT /v1/organizations/{tenant}/quotas`: the operator only.
    pub async fn put_ceiling(&self, tenant: &str, ceiling: &PutCeiling) -> Result<(), Error> {
        let request = self
            .request(Method::PUT, routes::ORGANIZATION_QUOTAS, &[tenant])?
            .json(ceiling);
        self.empty(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Server

    /// `GET /v1/server`.
    pub async fn server_info(&self) -> Result<ServerInfo, Error> {
        let request = self.request(Method::GET, routes::SERVER, &[])?;
        self.json(request).await
    }

    /// `POST /v1/server/stop`: only the embedded server stops.
    pub async fn stop_server(&self) -> Result<(), Error> {
        let request = self.request(Method::POST, routes::SERVER_STOP, &[])?;
        self.empty(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Plumbing

    fn url(&self, template: &str, params: &[&str]) -> Result<Url, Error> {
        let mut url = self.base.clone();
        url.set_query(None);
        url.set_fragment(None);
        routes::push(&mut url, template, params)?;
        Ok(url)
    }

    fn request(
        &self,
        method: Method,
        template: &str,
        params: &[&str],
    ) -> Result<RequestBuilder, Error> {
        Ok(self.authorized(method, self.url(template, params)?))
    }

    /// A request whose query string is `query`, serialized as the server reads it.
    fn request_with(
        &self,
        method: Method,
        template: &str,
        params: &[&str],
        query: &impl Serialize,
    ) -> Result<RequestBuilder, Error> {
        let mut url = self.url(template, params)?;
        let pairs = query_pairs(query)?;
        if !pairs.is_empty() {
            url.query_pairs_mut().extend_pairs(&pairs);
        }
        Ok(self.authorized(method, url))
    }

    fn authorized(&self, method: Method, url: Url) -> RequestBuilder {
        let mut request = self.http.request(method, url);
        if let Some(key) = &self.api_key {
            request = request.header(AUTHORIZATION, key.clone());
        }
        if let Some(namespace) = &self.namespace {
            request = request.header(NAMESPACE, namespace.as_str());
        }
        if let Some(organization) = &self.organization {
            request = request.header(ORGANIZATION, organization.clone());
        }
        request
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response, Error> {
        let response = request
            .send()
            .await
            .map_err(|error| self.transport(&error))?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(problem(response).await)
        }
    }

    async fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T, Error> {
        let response = self.send(request).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|error| self.transport(&error))?;
        serde_json::from_slice(&bytes).map_err(|error| Error::Decode(error.to_string()))
    }

    async fn empty(&self, request: RequestBuilder) -> Result<(), Error> {
        let response = self.send(request).await?;
        // Drain the body so that the connection can be reused.
        response
            .bytes()
            .await
            .map_err(|error| self.transport(&error))?;
        Ok(())
    }

    fn transport(&self, error: &reqwest::Error) -> Error {
        let mut message = error.to_string();
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        Error::Transport {
            url: self.base.to_string(),
            message,
        }
    }
}

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .user_agent(concat!("rewind-api/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
}

fn config_error(error: reqwest::Error) -> Error {
    Error::Config(error.to_string())
}

fn with_key(request: RequestBuilder, key: Option<&IdempotencyKey>) -> RequestBuilder {
    match key {
        Some(key) => request.header(IDEMPOTENCY_KEY, key.as_str()),
        None => request,
    }
}

/// The error a failed response carries: its problem, or one made from its status when the
/// body is not `application/problem+json` (a proxy's error page, for instance).
async fn problem(response: Response) -> Error {
    let status = response.status();
    let is_json = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("json"));
    let body = response.text().await.unwrap_or_default();
    if is_json && let Ok(problem) = serde_json::from_str::<Problem>(&body) {
        return problem.into();
    }
    untyped_problem(status, &body).into()
}

fn untyped_problem(status: StatusCode, body: &str) -> Problem {
    let title = status.canonical_reason().unwrap_or("Error").to_owned();
    let excerpt: String = body.trim().chars().take(500).collect();
    Problem {
        r#type: "about:blank".into(),
        detail: if excerpt.is_empty() {
            format!("the server answered {status}")
        } else {
            format!("the server answered {status}: {excerpt}")
        },
        title,
        status: status.as_u16(),
        instance: None,
        diagnostics: Vec::new(),
        query: None,
        tests: Vec::new(),
        quota: None,
    }
}

/// A query DTO as `key=value` pairs: arrays become repeated keys, absent values are left out.
fn query_pairs(query: &impl Serialize) -> Result<Vec<(String, String)>, Error> {
    let value =
        serde_json::to_value(query).map_err(|error| Error::invalid("query", error.to_string()))?;
    let Value::Object(fields) = value else {
        return Err(Error::invalid("query", "expected an object"));
    };
    let mut pairs = Vec::new();
    for (key, value) in fields {
        let values = match value {
            Value::Array(values) => values,
            value => vec![value],
        };
        for value in values {
            match value {
                Value::Null => {}
                Value::String(text) => pairs.push((key.clone(), text)),
                Value::Bool(flag) => pairs.push((key.clone(), flag.to_string())),
                other => pairs.push((key.clone(), other.to_string())),
            }
        }
    }
    // By key, whatever the features of `serde_json`; repeated keys keep their order.
    pairs.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(pairs)
}

/// The events of `GET /v1/runs/{id}/events`, until the server ends the stream.
pub struct EventStream {
    inner: Pin<Box<dyn Stream<Item = Result<StreamEvent, Error>> + Send>>,
}

impl fmt::Debug for EventStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EventStream")
    }
}

impl Stream for EventStream {
    type Item = Result<StreamEvent, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::RunFilter;

    #[test]
    fn query_dtos_become_repeated_pairs() {
        let filter = RunFilter {
            labels: vec!["agent:triage".into(), "conversation:c-42".into()],
            quarantined: Some(false),
            limit: Some(10),
            ..RunFilter::default()
        };
        assert_eq!(
            query_pairs(&filter).unwrap(),
            [
                ("label".to_owned(), "agent:triage".to_owned()),
                ("label".to_owned(), "conversation:c-42".to_owned()),
                ("limit".to_owned(), "10".to_owned()),
                ("quarantined".to_owned(), "false".to_owned()),
            ]
        );
    }

    /// What the client writes, a server reads back with a query deserializer that accepts
    /// repeated keys (`serde_html_form`, behind axum-extra's `Query`).
    #[test]
    fn query_dtos_round_trip_through_a_query_string() {
        fn round_trip<T>(query: &T) -> T
        where
            T: Serialize + DeserializeOwned,
        {
            let pairs = query_pairs(query).unwrap();
            let mut url = Url::parse("http://localhost/").unwrap();
            url.query_pairs_mut().extend_pairs(&pairs);
            serde_html_form::from_str(url.query().unwrap_or_default()).unwrap()
        }

        let filter = RunFilter {
            status: Some(crate::common::RunStatus::Failed),
            labels: vec!["agent:triage".into(), "conversation:c 42".into()],
            quarantined: Some(true),
            started_after: Some("2026-11-09T10:02:11Z".parse().unwrap()),
            limit: Some(10),
            ..RunFilter::default()
        };
        assert_eq!(round_trip(&filter), filter);

        let tasks = TaskQuery {
            toolset: Some("ab".into()),
            tools: vec!["ask_user".into(), "approve".into()],
            limit: Some(5),
            wait: Some("30s".parse().unwrap()),
        };
        assert_eq!(round_trip(&tasks), tasks);

        let events = EventsQuery {
            follow: Some(true),
            from: Some(7),
        };
        assert_eq!(round_trip(&events), events);
        let follow: EventsQuery = serde_html_form::from_str("follow=1").unwrap();
        assert_eq!(follow.follow, Some(true));

        let journal = JournalQuery {
            export_as: Some(JournalExport::Cassette),
        };
        assert_eq!(round_trip(&journal), journal);
        assert_eq!(
            query_pairs(&journal).unwrap(),
            [("as".to_owned(), "cassette".to_owned())]
        );
    }

    #[test]
    fn a_failure_that_is_not_problem_json_still_reads_as_a_problem() {
        let problem = untyped_problem(StatusCode::BAD_GATEWAY, "<html>bad gateway</html>");
        assert_eq!(problem.status, 502);
        assert_eq!(problem.r#type, "about:blank");
        assert!(problem.detail.contains("bad gateway"));
    }

    #[test]
    fn the_api_key_is_not_printed() {
        let client =
            Client::remote("https://rewind.example.com", Some("rewind_sk_secret")).unwrap();
        assert!(!format!("{client:?}").contains("secret"));
        assert!(Client::remote("ftp://example.com", None).is_err());
        assert!(Client::remote("not a url", None).is_err());
    }
}
