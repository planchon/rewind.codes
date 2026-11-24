//! The `ServerHandler`: `initialize`, `tools/list` and `tools/call`.

use std::sync::Arc;

use rewind_agent_tools::definitions::{McpTool, mcp_tool, mcp_tools};
use rewind_agent_tools::{AgentToolResult, McpTools, Organization};
use rewind_api::namespaces::NamespaceName;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value};

use crate::connector::{Connector, ConnectorError};
use crate::organizations;

/// What `initialize` tells the model: from an organization to a run.
pub const INSTRUCTIONS: &str = "Rewind runs TypeScript scripts that call tools, durably: a \
script can sleep for days and wait for signals, every call it makes is recorded, and a failed run \
is fixed without calling again what it already called. From an organization to a run:\n\
1. list_orgs: the organizations you reach. Give one as `org` to the other tools, by its id or \
its name; leave it out when you reach only one.\n\
2. list_namespaces: the namespaces of the organization, each one end user. A script runs in the \
namespace you give as `namespace`; a new name creates one.\n\
3. list_action_sets: the action sets, the MCP servers and tools a script imports from \
\"rewind:tools\". Give the action set's id as `action_set`.\n\
4. search_tools: the TypeScript declarations of the tools you need, before you write the script.\n\
5. list_secrets: the secrets you may attach, by name, never a value: the end user's own \
(ns/<name>) and the organization's (org/<name>). A server whose calls need one takes it in \
`secrets` of run_script and retry_run: {\"github\": \"ns/github\"}.\n\
6. check_script, then test_script: type-check the script, then test it in virtual time against \
answers you give for its calls, without calling anything or reading any secret.\n\
7. run_script: run it with the secrets its servers need. You get its output, or the run's id and \
what it waits for: follow it with get_run, answer it with signal_run, stop it with cancel_run, \
and fix a failed run with retry_run.\n\
When the person has not said which organization, namespace or action set to use and a list holds \
more than one, ask them.";

/// Builds the caller of an HTTP request from its parts.
type Connect<C> = dyn Fn(&http::request::Parts) -> Option<C> + Send + Sync;

/// Where the handler finds its caller.
enum Source<C> {
    /// One caller for the whole session: `rewind mcp` over stdio.
    Fixed(Arc<C>),
    /// The caller of each HTTP request, from its parts.
    PerRequest(Arc<Connect<C>>),
}

impl<C> Clone for Source<C> {
    fn clone(&self) -> Self {
        match self {
            Source::Fixed(connector) => Source::Fixed(connector.clone()),
            Source::PerRequest(connect) => Source::PerRequest(connect.clone()),
        }
    }
}

/// Rewind's MCP server, for the callers a [`Connector`] gives.
pub struct RewindMcp<C> {
    source: Source<C>,
    tools: McpTools,
}

impl<C> Clone for RewindMcp<C> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            tools: self.tools.clone(),
        }
    }
}

impl<C> std::fmt::Debug for RewindMcp<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match &self.source {
            Source::Fixed(_) => "fixed",
            Source::PerRequest(_) => "per request",
        };
        f.debug_struct("RewindMcp")
            .field("source", &source)
            .finish_non_exhaustive()
    }
}

impl<C: Connector> RewindMcp<C> {
    /// The server for one caller, whatever the request: over stdio.
    pub fn new(connector: C) -> Self {
        Self {
            source: Source::Fixed(Arc::new(connector)),
            tools: McpTools::new(),
        }
    }

    /// The server for the caller of each HTTP request, which `connect` builds from the
    /// request's parts.
    pub fn per_request(
        connect: impl Fn(&http::request::Parts) -> Option<C> + Send + Sync + 'static,
    ) -> Self {
        Self {
            source: Source::PerRequest(Arc::new(connect)),
            tools: McpTools::new(),
        }
    }

    fn connector(&self, context: &RequestContext<RoleServer>) -> Result<Arc<C>, ErrorData> {
        match &self.source {
            Source::Fixed(connector) => Ok(connector.clone()),
            Source::PerRequest(connect) => context
                .extensions
                .get::<http::request::Parts>()
                .and_then(|parts| connect(parts))
                .map(Arc::new)
                .ok_or_else(|| {
                    ErrorData::internal_error(
                        "the request reached the MCP server without a caller: the host \
                         authenticates every request first",
                        None,
                    )
                }),
        }
    }
}

fn fault(error: &ConnectorError) -> ErrorData {
    ErrorData::internal_error(format!("Rewind could not serve the call: {error}"), None)
}

/// A tool of the MCP set as MCP lists it.
fn listed(tool: McpTool) -> Tool {
    let schema = match tool.input_schema {
        Value::Object(schema) => schema,
        _ => Map::new(),
    };
    let annotations = ToolAnnotations::with_title(tool.title)
        .read_only(tool.annotations.read_only)
        .destructive(tool.annotations.destructive)
        .idempotent(tool.annotations.idempotent)
        .open_world(tool.annotations.open_world);
    Tool::new(tool.name, tool.description, Arc::new(schema))
        .with_title(tool.title)
        .with_annotations(annotations)
}

fn answer(result: AgentToolResult) -> CallToolResponse {
    let content = vec![ContentBlock::text(result.content)];
    if result.is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
    .into()
}

fn refused(text: String) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(text)]).into()
}

impl<C: Connector> ServerHandler for RewindMcp<C> {
    fn get_info(&self) -> ServerConfig {
        let mut server = Implementation::new("Rewind", env!("CARGO_PKG_VERSION"));
        server.website_url = Some("https://rewind.codes".into());
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(server)
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let connector = self.connector(&context)?;
        let organizations = connector
            .organizations()
            .await
            .map_err(|error| fault(&error))?;
        let tools = mcp_tools()
            .into_iter()
            .filter(|tool| {
                tool.permission.is_none_or(|permission| {
                    organizations
                        .iter()
                        .any(|organization| organization.allows(permission))
                })
            })
            .map(listed)
            .collect();
        let mut listed = ListToolsResult::with_all_items(tools);
        // What a caller may use is theirs: no one else's client reuses it.
        listed.cache_scope = Some(CacheScope::Private);
        Ok(listed)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = request.name.as_ref();
        let Some(tool) = mcp_tool(name) else {
            return Err(ErrorData::invalid_params(
                format!("{name:?} is not one of Rewind's tools: tools/list gives them"),
                None,
            ));
        };
        let input = Value::Object(request.arguments.unwrap_or_default());
        let connector = self.connector(&context)?;
        let organizations = connector
            .organizations()
            .await
            .map_err(|error| fault(&error))?;
        if tool.name == "list_orgs" {
            return Ok(answer(self.tools.list_orgs(&organizations)));
        }
        let organization = match organizations::resolve(name, &organizations, input.get("org")) {
            Ok(organization) => organization,
            Err(text) => return Ok(refused(text)),
        };
        if let Some(permission) = tool.permission
            && !organization.allows(permission)
        {
            return Ok(refused(not_allowed(name, organization, permission)));
        }
        let namespace = if tool.input_schema["properties"].get("namespace").is_some() {
            match namespace_of(name, input.get("namespace")) {
                Ok(namespace) => namespace,
                Err(text) => return Ok(refused(text)),
            }
        } else {
            None
        };
        let api = connector
            .api(organization, namespace.as_ref())
            .map_err(|error| fault(&error))?;
        self.tools
            .handle(&api, name, &input)
            .await
            .map(answer)
            .map_err(|error| {
                ErrorData::internal_error(format!("Rewind could not serve the call: {error}"), None)
            })
    }
}

/// The tool's permission is not the caller's in this organization.
fn not_allowed(
    tool: &str,
    organization: &Organization,
    permission: rewind_api::access::Permission,
) -> String {
    let which = match &organization.name {
        Some(name) => format!("{name} (id {})", organization.id),
        None => format!("id {}", organization.id),
    };
    format!(
        "{tool}: you may not use it in the organization {which}: it needs the permission \
         {permission}, which you do not hold there. list_orgs says what you may do in each \
         organization."
    )
}

/// The call's namespace, checked as the API names them.
fn namespace_of(tool: &str, namespace: Option<&Value>) -> Result<Option<NamespaceName>, String> {
    match namespace {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) if !name.is_empty() => NamespaceName::new(name.as_str())
            .map(Some)
            .map_err(|error| format!("{tool}: `namespace`: {error}")),
        Some(_) => Err(format!(
            "{tool}: `namespace` must be a non-empty string: a namespace's name, as \
             list_namespaces gives them."
        )),
    }
}
