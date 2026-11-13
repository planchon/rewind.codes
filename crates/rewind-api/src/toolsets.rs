//! Toolsets: registering and pinning them, their declarations, search, credentials
//! (`archi/04-tools.md`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::common::Duration;

/// `POST /v1/toolsets`: MCP servers, host tools and the `http` tool, with a policy per tool.
/// Registering connects to the MCP servers, lists their tools and pins their schemas. The
/// toolset is content-addressed: registering the same toolset twice gives the same id.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({
    "mcp": [{ "name": "github", "url": "https://mcp.example.com/github" }],
    "host": [{
        "name": "ask_user",
        "description": "Ask the end user a question and return their answer.",
        "inputSchema": {
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"]
        },
        "policy": { "onInterruption": "at_most_once" }
    }]
}))]
pub struct CreateToolset {
    /// MCP servers. Their tools are called by Rewind and named `<server>.<tool>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<McpServer>,
    /// Host tools: functions of the harness, executed by its SDK worker (`GET /v1/tasks`). A
    /// name `group.tool` puts the tool in the `group` namespace of `rewind:tools`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host: Vec<HostTool>,
    /// Enables the built-in `http` tool, and `fetch` in scripts, for these hosts only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpTool>,
    /// Credentials of the MCP servers: used to list their tools, and kept as the toolset's
    /// credentials, as `PUT /v1/toolsets/{id}/credentials` sets them. They are not part of the
    /// toolset's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<Credentials>,
}

/// An MCP server: exactly one of `url` (streamable HTTP) and `command` (stdio).
///
/// A server of a deployment refuses `command` unless its configuration allows it: a stdio
/// server is a process started by Rewind. The embedded server accepts it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServer {
    /// The namespace of its tools in `rewind:tools`: an identifier that is not a reserved word of
    /// JavaScript (`default`, `delete`, `let`…).
    pub name: String,
    /// The URL of a streamable HTTP server. A server of a deployment reaches it only at a public
    /// address, unless its configuration allows the internal host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The program of a stdio server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment of a stdio server that is not secret. Secrets go in credentials.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Policies of its tools, by tool name as the server defines it. Tools not listed get the
    /// default policy.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub policies: BTreeMap<String, ToolPolicy>,
}

/// A function of the harness, called by scripts and executed by the harness's SDK worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostTool {
    /// `group.tool` puts the tool in a group, the namespace `rewind:tools` exports it under. The
    /// group, and a name without a dot, may not be a reserved word of JavaScript.
    pub name: String,
    /// Shown to the model in the declarations.
    pub description: String,
    /// JSON Schema of the arguments. Arguments are validated against it before the call.
    #[schema(value_type = Object)]
    pub input_schema: Value,
    /// JSON Schema of the result, for the declarations. Results are not validated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ToolPolicy>,
}

/// The built-in `http` tool: requests leave from the dispatcher, never from the isolate, and
/// only toward these hosts (`archi/04-tools.md`, "The `http` tool and egress").
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpTool {
    /// Host names, such as `api.example.com`. `*.example.com` allows the subdomains.
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ToolPolicy>,
}

/// A tool's policy, pinned in the toolset (`archi/04-tools.md`, "Policies"). Every field is
/// optional: what is not given takes its default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolPolicy {
    /// What happens to a call interrupted mid-flight. `retry` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_interruption: Option<OnInterruption>,
    /// The call waits for the harness's approval (`POST /v1/tasks/{id}/approve`) before it is
    /// sent. Off by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<bool>,
    /// How long one attempt may take. 5 minutes by default for MCP tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// How long a host tool's worker may go without a heartbeat. 60 seconds by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_timeout: Option<Duration>,
    /// At most this many calls of the tool in flight per run. The run's own limit (16) still
    /// applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
}

/// After an interruption (the dispatcher or the worker died mid-call).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnInterruption {
    /// Call again with the same idempotency key, up to 5 attempts, then fail with
    /// `outcomeUnknown`.
    #[default]
    Retry,
    /// Fail with `outcomeUnknown` at once. For tools that ignore idempotency keys.
    AtMostOnce,
}

/// A registered toolset, as pinned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Toolset {
    /// The digest of the pinned toolset (SHA-256, lowercase hex). Runs record it.
    pub id: String,
    pub servers: Vec<ServerSummary>,
    /// Every tool a script of this toolset can call, sorted by name.
    pub tools: Vec<Tool>,
    pub declarations: Declarations,
}

/// `GET /v1/toolsets`: the toolsets registered by the tenant, by id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolsetList {
    pub toolsets: Vec<ToolsetSummary>,
}

/// A registered toolset, without its schemas and declarations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolsetSummary {
    pub id: String,
    pub servers: Vec<ServerSummary>,
    /// The names of its tools, sorted.
    pub tools: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerSummary {
    pub name: String,
    pub transport: McpTransport,
    /// How many tools it has.
    pub tools: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Http,
    Stdio,
}

/// A pinned tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    /// As scripts call it: `github.list_issues`, `ask_user`, `http`.
    pub name: String,
    pub route: Route,
    /// The MCP server, for an MCP tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    pub description: String,
    #[schema(value_type = Object)]
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub output_schema: Option<Value>,
    /// The policy with its defaults filled in.
    pub policy: ToolPolicy,
}

/// Who calls the tool (`archi/04-tools.md`, "Routes").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// The dispatcher of Rewind, as an MCP client.
    Mcp,
    /// The harness's SDK worker.
    Host,
    /// The dispatcher of Rewind: `http` and `webhook`.
    Builtin,
}

/// The toolset's declarations, generated by `rewind-codegen`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Declarations {
    /// `rewind-tools.d.ts`: the `rewind:tools` module, with tool descriptions as JSDoc.
    pub dts: String,
    /// The compact rendering for the model's prompt, one line per tool.
    pub prompt: String,
}

/// `GET /v1/toolsets/{id}/declarations`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeclarationsQuery {
    /// `dts` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(inline)]
    pub format: Option<DeclarationFormat>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeclarationFormat {
    /// `rewind-tools.d.ts`.
    #[default]
    Dts,
    /// The compact prompt rendering.
    Prompt,
}

impl DeclarationFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            DeclarationFormat::Dts => "dts",
            DeclarationFormat::Prompt => "prompt",
        }
    }
}

/// The declarations in one format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeclarationsText {
    pub format: DeclarationFormat,
    pub text: String,
}

/// `GET /v1/toolsets/{id}/search`: `search_tools`, for toolsets too large for the prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchQuery {
    /// Words to look for in tool names and descriptions.
    pub query: String,
    /// At most this many tools (10 by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// The tools matching a search, best first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolSearchResults {
    pub tools: Vec<ToolMatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolMatch {
    pub name: String,
    pub description: String,
    /// The tool's line of the prompt rendering.
    pub prompt: String,
    /// The tool's declaration in `rewind-tools.d.ts`, with the types it uses.
    pub dts: String,
}

/// `PUT /v1/toolsets/{id}/credentials` and `PUT /v1/runs/{id}/credentials`: secrets injected
/// into the MCP transport at call time. They never enter the journal, the isolate, a cassette
/// or OpenTelemetry; a call uses the run's credentials if it has some, else the toolset's.
/// Setting them replaces the previous ones, and makes ready again the calls parked by
/// `credentials.expired`.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Credentials {
    /// By MCP server name.
    pub servers: BTreeMap<String, ServerCredentials>,
}

/// Never printed by `Debug`: only the server names are.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServerCredentials {
    /// HTTP headers sent to a streamable HTTP server, such as `Authorization`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Environment variables of a stdio server.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl std::fmt::Debug for ServerCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerCredentials")
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_toolset_of_the_product_overview_reads() {
        let toolset: CreateToolset = serde_json::from_value(json!({
            "mcp": [{ "name": "github", "url": "https://mcp.example.com/github" }],
            "host": [{
                "name": "ask_user",
                "description": "Ask the end user a question and return their answer.",
                "inputSchema": { "type": "object" },
                "policy": { "onInterruption": "at_most_once", "heartbeatTimeout": "2m" }
            }],
            "http": { "allow": ["api.example.com"] }
        }))
        .unwrap();
        let policy = toolset.host[0].policy.unwrap();
        assert_eq!(policy.on_interruption, Some(OnInterruption::AtMostOnce));
        assert_eq!(policy.heartbeat_timeout.unwrap().as_millis(), 120_000);
        assert!(
            serde_json::from_value::<CreateToolset>(json!({ "mcp": [], "tool": [] })).is_err(),
            "unknown fields are refused"
        );
    }

    #[test]
    fn credentials_are_not_printed() {
        let credentials: Credentials = serde_json::from_value(json!({
            "servers": { "github": { "headers": { "Authorization": "Bearer secret-token" } } }
        }))
        .unwrap();
        let printed = format!("{credentials:?}");
        assert!(printed.contains("github"));
        assert!(!printed.contains("secret-token"));
    }
}
