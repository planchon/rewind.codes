//! Tools: the dispatcher of the routes ai-ptc calls itself, the discovery of a toolset's MCP
//! servers, the built-ins pinned in every toolset, and the declarations generated for a toolset
//! (`archi/04-tools.md`).
//!
//! | Route | Dispatcher |
//! | --- | --- |
//! | MCP tools | `ptc-mcp`'s `McpDispatcher`: stdio and streamable HTTP, connections pooled; HTTP servers only where the configuration's `McpEgress` lets it go |
//! | Built-ins | `ptc-mcp`'s `HttpTool` for `http`, with its egress rules; `webhook` is answered by the engine |
//! | Host tools | none: the harness's worker claims them (`GET /v1/tasks`) |
//!
//! The declarations (`ptc-tools.d.ts`, the prompt rendering, each tool's) come from
//! `ptc-codegen`.

use std::collections::BTreeMap;

use ptc_core::{Builtin, McpServer, Shape, Tool, ToolRoute, Toolset};
use ptc_engine::adapters::{RoutingDispatcher, Unrouted};
use ptc_engine::ports::{ServerCredentials, TenantId};
use ptc_mcp::{HttpConfig, HttpTool, McpConfig, McpDispatcher, McpEgress};

/// The dispatcher of the routes ai-ptc calls itself.
pub(crate) type Dispatcher = RoutingDispatcher<McpDispatcher, HttpTool, Unrouted>;

/// Why the tools of an MCP server could not be listed: unreachable, credentials refused, too
/// slow, or an answer that is not a tool list. The message names the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveryError(pub(crate) String);

/// What the server needs to call and pin tools.
#[derive(Debug, Clone)]
pub(crate) struct Tools {
    mcp: McpDispatcher,
    http: HttpTool,
}

impl Tools {
    /// The tools of a server whose MCP servers may be where `egress` says, at discovery as at
    /// each call.
    pub(crate) fn new(egress: McpEgress) -> Result<Self, String> {
        Ok(Self {
            mcp: McpDispatcher::new(McpConfig {
                egress,
                ..McpConfig::default()
            }),
            http: HttpTool::new(HttpConfig::default()).map_err(|error| error.to_string())?,
        })
    }

    pub(crate) fn dispatcher(&self) -> Dispatcher {
        RoutingDispatcher::new(self.mcp.clone(), self.http.clone(), Unrouted)
    }

    /// The tools of an MCP server, as a toolset pins them.
    pub(crate) async fn discover(
        &self,
        tenant: &TenantId,
        server: &McpServer,
        credentials: Option<&ServerCredentials>,
    ) -> Result<Vec<Tool>, DiscoveryError> {
        self.mcp
            .list_tools(tenant, server, credentials)
            .await
            .map_err(|error| DiscoveryError(error.to_string()))
    }

    /// The `http` built-in, for these hosts.
    pub(crate) fn http_tool(&self, allowed_hosts: Vec<String>) -> Tool {
        ptc_mcp::http_tool(allowed_hosts)
    }

    /// The `webhook` built-in, pinned in every toolset.
    pub(crate) fn webhook_tool(&self) -> Tool {
        let mut tool = Tool::new(
            "webhook",
            ToolRoute::Builtin(Builtin::Webhook),
            ptc_runtime::builtins::webhook_input_schema(),
        );
        tool.description = Some(
            "A URL bound to the run and a signal name: what is posted to it becomes that \
             signal. Scripts call it through webhook() from \"ptc\"."
                .to_owned(),
        );
        tool
    }

    /// The declarations of a toolset: `ptc-tools.d.ts`, the prompt rendering, each tool's.
    pub(crate) fn declarations(
        &self,
        toolset: &Toolset,
        observed: &BTreeMap<String, Shape>,
    ) -> ptc_codegen::Declarations {
        ptc_codegen::generate(toolset, observed)
    }

    /// Closes the MCP connections and stops the stdio servers.
    pub(crate) fn close(&self) {
        self.mcp.close();
    }
}
