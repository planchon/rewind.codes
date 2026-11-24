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
use crate::action_sets::{
    ActionSearchResults, ActionSet, ActionSetList, CreateActionSet, DeclarationFormat,
    DeclarationsQuery, DeclarationsText, SearchQuery,
};
use crate::admin::ServerInfo;
use crate::calls::{
    CallStreamMessage, CallStreamQuery, CompleteCall, DeferCall, FailCall, RejectCall,
};
use crate::common::Duration;
use crate::events::{ChangeEvent, Entry, EventsQuery, StreamEvent};
use crate::headers::{IDEMPOTENCY_KEY, IdempotencyKey, NAMESPACE, ORGANIZATION};
use crate::namespaces::{
    CreateToken, CreateTokenKey, NamespaceDefaults, NamespaceDetail, NamespaceList,
    NamespaceListQuery, NamespaceName, NamespaceNames, NamespacePatch, NamespaceQuery, PutCeiling,
    PutNamespace, PutNamespaceDefaults, QuotaAlertList, QuotaAlertQuery, RevokedTokens, Token,
    TokenKey, TokenKeyList,
};
use crate::organization::{
    CreateInvitation, CreateOrganization, Identity, Invitation, InvitationList, MemberList,
    Organization, OrganizationSettings, UpdateOrganization,
};
use crate::problem::Problem;
use crate::reading::{ActionStatsFilter, ActionStatsPage, PayloadQuery};
use crate::routes;
use crate::runs::{
    CancelRun, FailureGroupPage, FailureQuery, ForkRun, GetRunQuery, JournalExport, JournalQuery,
    Run, RunFilter, RunPage, RunStats, RunStatsQuery, SignalAccepted, StartRun, Trace, TraceFormat,
    TraceQuery,
};
use crate::scripts::{
    CheckRequest, CheckResult, Script, ScriptFilter, ScriptPage, SetScriptTitle, TestRequest,
};
use crate::secrets::{
    Jwks, PutSecret, Secret, SecretList, SecretUsage, SigningKey, SigningKeyList,
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

    /// The same client, sharing its connections, authenticated with another Bearer: what
    /// `rewind-server` does to call `/v1` as each caller of `/mcp`.
    pub fn with_bearer(&self, bearer: &str) -> Result<Self, Error> {
        let mut value = HeaderValue::from_str(&format!("Bearer {bearer}"))
            .map_err(|_| Error::invalid("Bearer", "it holds characters a header cannot"))?;
        value.set_sensitive(true);
        Ok(Self {
            api_key: Some(value),
            ..self.clone()
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
    // Action sets

    /// `POST /v1/action-sets`.
    pub async fn create_action_set(
        &self,
        action_set: &CreateActionSet,
    ) -> Result<ActionSet, Error> {
        let request = self
            .request(Method::POST, routes::ACTION_SETS, &[])?
            .json(action_set);
        self.json(request).await
    }

    /// `GET /v1/action-sets`.
    pub async fn list_action_sets(&self) -> Result<ActionSetList, Error> {
        let request = self.request(Method::GET, routes::ACTION_SETS, &[])?;
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

    /// `GET /v1/action-sets/{id}/declarations`.
    pub async fn action_set_declarations(
        &self,
        action_set: &str,
        format: DeclarationFormat,
    ) -> Result<DeclarationsText, Error> {
        let query = DeclarationsQuery {
            format: Some(format),
        };
        let request = self.request_with(
            Method::GET,
            routes::ACTION_SET_DECLARATIONS,
            &[action_set],
            &query,
        )?;
        self.json(request).await
    }

    /// `GET /v1/action-sets/{id}/search`.
    pub async fn search_actions(
        &self,
        action_set: &str,
        query: &SearchQuery,
    ) -> Result<ActionSearchResults, Error> {
        let request =
            self.request_with(Method::GET, routes::ACTION_SET_SEARCH, &[action_set], query)?;
        self.json(request).await
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

    /// `PATCH /v1/scripts/{digest}`.
    pub async fn set_script_title(
        &self,
        digest: &str,
        title: &SetScriptTitle,
    ) -> Result<Script, Error> {
        let request = self
            .request(Method::PATCH, routes::SCRIPT, &[digest])?
            .json(title);
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

    /// `GET /v1/changes`: what changes in the organization, its runs and its control plane, as the
    /// caller may see it, until the server ends the stream. Events of a later version are skipped.
    pub async fn changes(&self) -> Result<ChangeStream, Error> {
        let request = self
            .request(Method::GET, routes::CHANGES, &[])?
            .header(ACCEPT, "text/event-stream");
        let inner = self
            .named_events(request, "change stream", ChangeEvent::from_sse)
            .await?;
        Ok(ChangeStream { inner })
    }

    /// The server-sent events of a stream whose events are named by their `event` field: each
    /// decoded from its name and its data, an unknown name skipped, as a comment is.
    async fn named_events<T: Send + 'static>(
        &self,
        request: RequestBuilder,
        stream: &'static str,
        decode: fn(&str, &str) -> Result<Option<T>, serde_json::Error>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<T, Error>> + Send>>, Error> {
        let url = self.base.to_string();
        let response = self.send(request).await?;
        let events = SseByteStream::new(response.bytes_stream()).filter_map(move |item| {
            let url = url.clone();
            async move {
                match item {
                    Ok(sse) => {
                        let (event, data) = (sse.event?, sse.data.unwrap_or_default());
                        decode(&event, &data)
                            .map_err(|error| {
                                Error::Decode(format!("invalid event of the {stream}: {error}"))
                            })
                            .transpose()
                    }
                    Err(error) => Some(Err(Error::Transport {
                        url,
                        message: format!("the {stream} broke: {error}"),
                    })),
                }
            }
        });
        Ok(Box::pin(events))
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

    // ---------------------------------------------------------------------------------------
    // Harness calls

    /// `GET /v1/calls/stream`: the calls handed to this worker as they become ready, until the
    /// server ends the stream or the connection drops. The open stream is the liveness of the
    /// calls handed on it: open it again after a drop, with the same `query.worker`, and the
    /// server keeps them for their grace. Events of a later version and comments are skipped.
    pub async fn call_stream(&self, query: &CallStreamQuery) -> Result<CallStream, Error> {
        let request = self
            .request_with(Method::GET, routes::CALL_STREAM, &[], query)?
            .header(ACCEPT, "text/event-stream");
        let inner = self
            .named_events(request, "call stream", CallStreamMessage::from_sse)
            .await?;
        Ok(CallStream { inner })
    }

    /// `POST /v1/calls/{token}/defer`.
    pub async fn defer_call(&self, token: &str, defer: &DeferCall) -> Result<(), Error> {
        let request = self
            .request(Method::POST, routes::CALL_DEFER, &[token])?
            .json(defer);
        self.empty(request).await
    }

    /// `POST /v1/calls/{token}/complete`.
    pub async fn complete_call(&self, token: &str, complete: &CompleteCall) -> Result<(), Error> {
        let request = self
            .request(Method::POST, routes::CALL_COMPLETE, &[token])?
            .json(complete);
        self.empty(request).await
    }

    /// `POST /v1/calls/{token}/fail`.
    pub async fn fail_call(&self, token: &str, fail: &FailCall) -> Result<(), Error> {
        let request = self
            .request(Method::POST, routes::CALL_FAIL, &[token])?
            .json(fail);
        self.empty(request).await
    }

    /// `POST /v1/calls/{id}/approve`.
    pub async fn approve_call(&self, call: &str) -> Result<(), Error> {
        let request = self.request(Method::POST, routes::CALL_APPROVE, &[call])?;
        self.empty(request).await
    }

    /// `POST /v1/calls/{id}/reject`.
    pub async fn reject_call(&self, call: &str, reject: &RejectCall) -> Result<(), Error> {
        let request = self
            .request(Method::POST, routes::CALL_REJECT, &[call])?
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

    /// `GET /v1/actions/stats`.
    pub async fn action_stats(&self, filter: &ActionStatsFilter) -> Result<ActionStatsPage, Error> {
        let request = self.request_with(Method::GET, routes::ACTION_STATS, &[], filter)?;
        self.json(request).await
    }

    /// `GET /v1/payloads/{digest}`: the value itself, read through `run`, which holds it.
    pub async fn get_payload(&self, digest: &str, run: &str) -> Result<Value, Error> {
        let query = PayloadQuery {
            run: run.to_owned(),
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

    /// `PATCH /v1/namespaces/{name}`: changes what the patch holds of an existing namespace.
    pub async fn patch_namespace(
        &self,
        name: &NamespaceName,
        patch: &NamespacePatch,
    ) -> Result<NamespaceDetail, Error> {
        let request = self
            .request(Method::PATCH, routes::NAMESPACE, &[name.as_str()])?
            .json(patch);
        self.json(request).await
    }

    /// `DELETE /v1/namespaces/{name}`: the erasure goes on in the background.
    pub async fn delete_namespace(&self, name: &NamespaceName) -> Result<(), Error> {
        let request = self.request(Method::DELETE, routes::NAMESPACE, &[name.as_str()])?;
        self.empty(request).await
    }

    // ---------------------------------------------------------------------------------------
    // Secrets

    /// `GET /v1/secrets`: the organization's secrets the caller sees, never a value.
    pub async fn secrets(&self) -> Result<SecretList, Error> {
        let request = self.request(Method::GET, routes::SECRETS, &[])?;
        self.json(request).await
    }

    /// `PUT /v1/secrets/{name}`: creates or replaces an organization's secret.
    pub async fn put_secret(&self, name: &str, secret: &PutSecret) -> Result<Secret, Error> {
        let request = self
            .request(Method::PUT, routes::SECRET, &[name])?
            .json(secret);
        self.json(request).await
    }

    /// `DELETE /v1/secrets/{name}`.
    pub async fn delete_secret(&self, name: &str) -> Result<(), Error> {
        let request = self.request(Method::DELETE, routes::SECRET, &[name])?;
        self.empty(request).await
    }

    /// `GET /v1/secrets/{name}/usage`.
    pub async fn secret_usage(&self, name: &str) -> Result<SecretUsage, Error> {
        let request = self.request(Method::GET, routes::SECRET_USAGE, &[name])?;
        self.json(request).await
    }

    /// `GET /v1/namespaces/{name}/secrets`: an end user's secrets, never a value.
    pub async fn namespace_secrets(&self, namespace: &NamespaceName) -> Result<SecretList, Error> {
        let request = self.request(
            Method::GET,
            routes::NAMESPACE_SECRETS,
            &[namespace.as_str()],
        )?;
        self.json(request).await
    }

    /// `PUT /v1/namespaces/{name}/secrets/{secret}`: creates or replaces an end user's secret.
    pub async fn put_namespace_secret(
        &self,
        namespace: &NamespaceName,
        name: &str,
        secret: &PutSecret,
    ) -> Result<Secret, Error> {
        let request = self
            .request(
                Method::PUT,
                routes::NAMESPACE_SECRET,
                &[namespace.as_str(), name],
            )?
            .json(secret);
        self.json(request).await
    }

    /// `DELETE /v1/namespaces/{name}/secrets/{secret}`.
    pub async fn delete_namespace_secret(
        &self,
        namespace: &NamespaceName,
        name: &str,
    ) -> Result<(), Error> {
        let request = self.request(
            Method::DELETE,
            routes::NAMESPACE_SECRET,
            &[namespace.as_str(), name],
        )?;
        self.empty(request).await
    }

    /// `GET /v1/namespaces/{name}/secrets/{secret}/usage`.
    pub async fn namespace_secret_usage(
        &self,
        namespace: &NamespaceName,
        name: &str,
    ) -> Result<SecretUsage, Error> {
        let request = self.request(
            Method::GET,
            routes::NAMESPACE_SECRET_USAGE,
            &[namespace.as_str(), name],
        )?;
        self.json(request).await
    }

    /// `GET /v1/organization/signing-keys`.
    pub async fn signing_keys(&self) -> Result<SigningKeyList, Error> {
        let request = self.request(Method::GET, routes::SIGNING_KEYS, &[])?;
        self.json(request).await
    }

    /// `POST /v1/organization/signing-keys`: replaces the signing key.
    pub async fn rotate_signing_key(&self) -> Result<SigningKey, Error> {
        let request = self.request(Method::POST, routes::SIGNING_KEYS, &[])?;
        self.json(request).await
    }

    /// `GET /organizations/{tenant}/jwks.json`: the organization's public keys. The server
    /// answers it without authentication.
    pub async fn jwks(&self, tenant: &str) -> Result<Jwks, Error> {
        let request = self.request(Method::GET, routes::JWKS, &[tenant])?;
        self.json(request).await
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

    /// `GET /v1/organization/namespace-names`: the display names of the organization's
    /// namespaces.
    pub async fn namespace_names(&self) -> Result<NamespaceNames, Error> {
        let request = self.request(Method::GET, routes::NAMESPACE_NAMES, &[])?;
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
        attachments: Vec::new(),
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

/// The events of `GET /v1/changes`, until the server ends the stream.
pub struct ChangeStream {
    inner: Pin<Box<dyn Stream<Item = Result<ChangeEvent, Error>> + Send>>,
}

impl fmt::Debug for ChangeStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChangeStream")
    }
}

impl Stream for ChangeStream {
    type Item = Result<ChangeEvent, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// The events of `GET /v1/calls/stream`, until the server ends the stream or the connection
/// drops.
pub struct CallStream {
    inner: Pin<Box<dyn Stream<Item = Result<CallStreamMessage, Error>> + Send>>,
}

impl fmt::Debug for CallStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CallStream")
    }
}

impl Stream for CallStream {
    type Item = Result<CallStreamMessage, Error>;

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

        let calls = CallStreamQuery {
            action_set: Some("ab".into()),
            actions: vec!["ask_user".into(), "approve".into()],
            slots: Some(5),
            worker: Some("w-1".into()),
            ..CallStreamQuery::default()
        };
        assert_eq!(round_trip(&calls), calls);

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
