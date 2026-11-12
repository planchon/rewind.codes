//! Tools: the dispatcher of the routes ai-ptc calls itself, the discovery of a toolset's MCP
//! servers, the built-ins pinned in every toolset, and the declarations generated for a toolset
//! (`archi/04-tools.md`).
//!
//! | Route | Dispatcher |
//! | --- | --- |
//! | MCP tools | none in this build: calls fail with `unavailable` |
//! | Built-ins | `http`: none in this build; `webhook`: answered by the engine |
//! | Host tools | none: the harness's worker claims them (`GET /v1/tasks`) |

use std::collections::BTreeMap;

use ptc_core::{Builtin, McpServer, Shape, Tool, ToolRoute, Toolset};
use ptc_engine::adapters::{RoutingDispatcher, Unrouted};
use ptc_engine::ports::{ServerCredentials, TenantId};

/// The dispatcher of the routes ai-ptc calls itself.
pub(crate) type Dispatcher = RoutingDispatcher<Unrouted, Unrouted, Unrouted>;

/// Why the tools of an MCP server could not be listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscoveryError {
    /// The server cannot be reached, or answered with an error.
    Unavailable(String),
}

/// What the server needs to call and pin tools.
#[derive(Debug, Clone, Default)]
pub(crate) struct Tools;

impl Tools {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn dispatcher(&self) -> Dispatcher {
        RoutingDispatcher::new(Unrouted, Unrouted, Unrouted)
    }

    /// The tools of an MCP server, as a toolset pins them.
    pub(crate) async fn discover(
        &self,
        _tenant: &TenantId,
        server: &McpServer,
        _credentials: Option<&ServerCredentials>,
    ) -> Result<Vec<Tool>, DiscoveryError> {
        Err(DiscoveryError::Unavailable(format!(
            "MCP server {}: this build of the server has no MCP client",
            server.name
        )))
    }

    /// The `http` built-in, for these hosts.
    pub(crate) fn http_tool(&self, allowed_hosts: Vec<String>) -> Tool {
        Tool::new(
            "http",
            ToolRoute::Builtin(Builtin::Http { allowed_hosts }),
            ptc_runtime::builtins::http_input_schema(),
        )
    }

    /// The `webhook` built-in, pinned in every toolset.
    pub(crate) fn webhook_tool(&self) -> Tool {
        Tool::new(
            "webhook",
            ToolRoute::Builtin(Builtin::Webhook),
            ptc_runtime::builtins::webhook_input_schema(),
        )
    }

    /// The declarations of a toolset: `ptc-tools.d.ts`, the prompt rendering, each tool's.
    pub(crate) fn declarations(
        &self,
        toolset: &Toolset,
        _observed: &BTreeMap<String, Shape>,
    ) -> Generated {
        let http = toolset
            .tools()
            .iter()
            .any(|tool| matches!(tool.route, ToolRoute::Builtin(Builtin::Http { .. })));
        let mut typescript = String::new();
        if http {
            typescript.push_str(ptc_check::HTTP_TOOL_REFERENCE);
            typescript.push('\n');
        }
        typescript.push_str("declare module \"ptc:tools\" {}\n");
        Generated {
            typescript,
            prompt: String::new(),
            tools: Vec::new(),
        }
    }
}

/// What codegen generated for a toolset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Generated {
    pub(crate) typescript: String,
    pub(crate) prompt: String,
    /// Each tool on its own, sorted by name: what `search_tools` returns.
    pub(crate) tools: Vec<ToolDeclaration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolDeclaration {
    pub(crate) name: String,
    pub(crate) prompt: String,
    pub(crate) typescript: String,
}
