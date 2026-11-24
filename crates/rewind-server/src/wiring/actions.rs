//! Actions: the dispatcher of the routes Rewind calls itself, the discovery of an action set's MCP
//! servers, the reading of its APIs' OpenAPI documents, the built-ins pinned in every action set,
//! and the declarations generated for an action set (`archi/04-tools.md`).
//!
//! | Route | Dispatcher |
//! | --- | --- |
//! | MCP actions | `rewind-mcp`'s `McpDispatcher`: stdio and streamable HTTP, connections pooled; HTTP servers only where the configuration's `ActionEgress` lets it go |
//! | API actions | `rewind-openapi`'s `ApiDispatcher`: APIs only where the same `ActionEgress` lets it go |
//! | Built-ins | `rewind-mcp`'s `HttpAction` for `http`, with its egress rules; `webhook` is answered by the engine |
//! | Harness actions | none: the harness's worker claims them (`GET /v1/calls`) |
//!
//! The declarations (`rewind-actions.d.ts`, the prompt rendering, each action's) come from
//! `rewind-codegen`.

use std::collections::BTreeMap;

use rewind_core::{Action, ActionRoute, ActionSet, Builtin, McpServer, Shape};
use rewind_engine::adapters::{RoutingDispatcher, Unrouted};
use rewind_engine::ports::TenantId;
use rewind_mcp::{HttpAction, HttpConfig, ListingAuth, McpConfig, McpDispatcher};
use rewind_openapi::{ApiConfig, ApiDispatcher, FetchError, Fetched};
use rewind_outbound::ActionEgress;

/// The dispatcher of the routes Rewind calls itself.
pub(crate) type Dispatcher = RoutingDispatcher<McpDispatcher, ApiDispatcher, HttpAction, Unrouted>;

/// Why the actions of an MCP server could not be listed: unreachable, refused (401), too
/// slow, or an answer that is not an action list. The message names the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveryError(pub(crate) String);

/// What the server needs to call and pin actions.
#[derive(Debug, Clone)]
pub(crate) struct Actions {
    mcp: McpDispatcher,
    api: ApiDispatcher,
    http: HttpAction,
    /// Where MCP servers, APIs and OpenAPI documents may be.
    egress: ActionEgress,
}

impl Actions {
    /// The actions of a server whose MCP servers and APIs may be where `egress` says, at
    /// registration as at each call.
    pub(crate) fn new(egress: ActionEgress) -> Result<Self, String> {
        Ok(Self {
            mcp: McpDispatcher::new(McpConfig {
                egress: egress.clone(),
                ..McpConfig::default()
            }),
            api: ApiDispatcher::new(ApiConfig {
                egress: egress.clone(),
                ..ApiConfig::default()
            }),
            http: HttpAction::new(HttpConfig::default()).map_err(|error| error.to_string())?,
            egress,
        })
    }

    pub(crate) fn dispatcher(&self) -> Dispatcher {
        RoutingDispatcher::new(
            self.mcp.clone(),
            self.api.clone(),
            self.http.clone(),
            Unrouted,
        )
    }

    /// The actions of an MCP server, as an action set pins them, listed as `auth` says.
    pub(crate) async fn discover(
        &self,
        tenant: &TenantId,
        server: &McpServer,
        auth: ListingAuth,
    ) -> Result<Vec<Action>, DiscoveryError> {
        self.mcp
            .list_tools(tenant, server, auth)
            .await
            .map_err(|error| DiscoveryError(error.to_string()))
    }

    /// The OpenAPI document at `url`, read once through the egress checks.
    pub(crate) async fn fetch_document(&self, url: &str) -> Result<Fetched, FetchError> {
        rewind_openapi::fetch(url, &self.egress).await
    }

    /// The `http` built-in, for these hosts.
    pub(crate) fn http_action(&self, allowed_hosts: Vec<String>) -> Action {
        rewind_mcp::http_action(allowed_hosts)
    }

    /// The `webhook` built-in, pinned in every action set.
    pub(crate) fn webhook_action(&self) -> Action {
        let mut action = Action::new(
            "webhook",
            ActionRoute::Builtin(Builtin::Webhook),
            rewind_runtime::builtins::webhook_input_schema(),
        );
        action.description = Some(
            "A URL bound to the run and a signal name: what is posted to it becomes that \
             signal. Workflows call it through webhook() from \"rewind\"."
                .to_owned(),
        );
        action
    }

    /// The declarations of an action set: `rewind-actions.d.ts`, the prompt rendering, each
    /// action's.
    pub(crate) fn declarations(
        &self,
        action_set: &ActionSet,
        observed: &BTreeMap<String, Shape>,
    ) -> rewind_codegen::Declarations {
        rewind_codegen::generate(action_set, observed)
    }

    /// Closes the MCP connections and stops the stdio servers.
    pub(crate) fn close(&self) {
        self.mcp.close();
    }
}
