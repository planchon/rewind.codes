//! Toolsets: registering and pinning them, with their MCP servers, their APIs read from OpenAPI
//! documents and how each authenticates its calls; their declarations; search
//! (`archi/04-tools.md`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::common::Duration;

/// `POST /v1/toolsets`: MCP servers, APIs, host tools and the `http` tool, with a policy per
/// tool. Registering connects to the MCP servers and lists their tools, reads the APIs' OpenAPI
/// documents, and pins their schemas. The toolset is content-addressed: registering the same
/// toolset twice gives the same id. It holds no secret: a run attaches its secrets when it
/// starts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({
    "mcp": [{
        "name": "github",
        "url": "https://mcp.example.com/github",
        "auth": { "type": "secret", "header": "Authorization", "format": "Bearer {secret}" }
    }],
    "apis": [{ "name": "crm", "openapi": "https://api.example.com/openapi.json", "auth": { "type": "rewind" } }],
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
    /// APIs described by OpenAPI documents: one tool per operation, named
    /// `<api>.<operationId>`, called by Rewind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apis: Vec<Api>,
    /// Host tools: functions of the harness, executed by its SDK worker (`GET /v1/tasks`). A
    /// name `group.tool` puts the tool in the `group` namespace of `rewind:tools`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host: Vec<HostTool>,
    /// Enables the built-in `http` tool, and `fetch` in scripts, for these hosts only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpTool>,
    /// The organization's secrets that list the tools of the MCP servers whose `auth` takes one,
    /// by server: `{ "github": "org/github" }`. Only `org/` secrets; no run keeps them, and they
    /// are not part of the toolset's id. Requires `secrets:use`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: crate::secrets::Attachments,
    /// The workflows its scripts may start, from `rewind:workflows`: `triage` pins the latest
    /// version, `triage@<version>` one version. The versions are pinned: the toolset's scripts
    /// are checked against them, and start them, for their whole life.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<String>,
    /// Derives the toolset from a registered one: its servers, tools and workflows are kept as
    /// pinned, without listing the servers again; `workflows` adds workflows or pins them again.
    /// `mcp`, `host` and `http` must then be empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
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
    /// Environment of a stdio server that is not secret. A secret goes in the variable its
    /// `auth` names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// How its calls prove who calls. None when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    /// Policies of its tools, by tool name as the server defines it. Tools not listed get the
    /// default policy.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub policies: BTreeMap<String, ToolPolicy>,
}

/// An API described by an OpenAPI 3.0 or 3.1 document (`archi/04-tools.md`, "API tools"). Each
/// operation is a tool: its input the path, query and header parameters by name and the JSON
/// body under `body`; its output the schema of its first `2xx` JSON response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Api {
    /// The namespace of its tools in `rewind:tools`, an identifier as a server's name is.
    pub name: String,
    /// Its OpenAPI document in JSON, or the URL it is read from once, at registration (at most
    /// 2 MiB, through the egress checks). The document is pinned and never read again.
    pub openapi: OpenApiDocument,
    /// Where the calls go: the document's first `servers[]` when absent. `https`, or `http` on
    /// the embedded server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// How its calls prove who calls: the document's security requirement when absent. An
    /// `apiKey` in the query or a cookie, or several schemes at once, need it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    /// The `operationId`s to keep, for an API too large for the prompt. Every operation when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operations: Option<Vec<String>>,
    /// Its calls in flight on each worker. 8 by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    /// Policies of its tools, by operation name. An operation not listed gets the default of
    /// its method: `retry` for `GET`, `HEAD`, `OPTIONS`, `PUT` and `DELETE`; `at_most_once`
    /// for `POST` and `PATCH` unless it declares an `Idempotency-Key` header.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub policies: BTreeMap<String, ToolPolicy>,
}

/// An OpenAPI document, or where to read it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum OpenApiDocument {
    /// Its URL.
    Url(String),
    /// The document itself, in JSON.
    #[schema(value_type = Object)]
    Document(Value),
}

/// How the calls to an MCP server or an API prove who calls (`archi/04-tools.md`,
/// "Authenticating calls"). Part of the pinned toolset; never a value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Auth {
    /// Nothing: a public server. For an API, overrides its document's security requirement.
    /// (A struct variant, so that unknown fields are refused.)
    None {},
    /// A JWT signed with the organization's key, in `Authorization: Bearer`: the customer's own
    /// servers check it (`verifyRewindCall`) and learn the end user from `sub`. HTTP servers
    /// and APIs.
    Rewind {},
    /// The secret the run attached for the server: in `header` (with `format`, `{secret}`
    /// replaced by its value, and `encoding`), or in the variable `env` of a stdio server.
    /// Exactly one of `header` and `env`.
    Secret {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header: Option<String>,
        /// `Bearer {secret}`; the value alone when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        /// `base64` writes the secret in base64 into `format` (HTTP basic: `user:password`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encoding: Option<SecretEncoding>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
    },
}

impl Auth {
    /// The `auth` a toolset pins, what the server and the CLI pin alike. Whether it fits the
    /// server's transport is checked when the toolset is built (`rewind_core::Toolset`).
    pub fn to_core(&self) -> Result<rewind_core::ToolAuth, String> {
        use rewind_core::{SecretEncoding as Encoding, SecretPlacement, ToolAuth};
        Ok(match self {
            Auth::None {} => ToolAuth::None,
            Auth::Rewind {} => ToolAuth::Rewind,
            Auth::Secret {
                header: Some(name),
                format,
                encoding,
                env: None,
            } => ToolAuth::Secret(SecretPlacement::Header {
                name: name.clone(),
                format: format.clone(),
                encoding: match encoding.unwrap_or_default() {
                    SecretEncoding::Plain => Encoding::Plain,
                    SecretEncoding::Base64 => Encoding::Base64,
                },
            }),
            Auth::Secret {
                header: None,
                format: None,
                encoding: None,
                env: Some(variable),
            } => ToolAuth::Secret(SecretPlacement::Env {
                variable: variable.clone(),
            }),
            Auth::Secret { .. } => {
                return Err(
                    "a secret's auth has either a header (with an optional format and encoding) \
                     or an env"
                        .into(),
                );
            }
        })
    }

    /// How the API shows a pinned `auth`; `None` for none.
    pub fn of_core(auth: &rewind_core::ToolAuth) -> Option<Self> {
        use rewind_core::{SecretEncoding as Encoding, SecretPlacement, ToolAuth};
        Some(match auth {
            ToolAuth::None => return None,
            ToolAuth::Rewind => Auth::Rewind {},
            ToolAuth::Secret(SecretPlacement::Header {
                name,
                format,
                encoding,
            }) => Auth::Secret {
                header: Some(name.clone()),
                format: format.clone(),
                encoding: match encoding {
                    Encoding::Plain => None,
                    Encoding::Base64 => Some(SecretEncoding::Base64),
                },
                env: None,
            },
            ToolAuth::Secret(SecretPlacement::Env { variable }) => Auth::Secret {
                header: None,
                format: None,
                encoding: None,
                env: Some(variable.clone()),
            },
        })
    }
}

/// How a secret is written into a header's `format`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretEncoding {
    #[default]
    Plain,
    Base64,
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
    /// Its APIs, sorted by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apis: Vec<ApiSummary>,
    /// The workflows its scripts may start, sorted by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<crate::workflows::PinnedWorkflow>,
    pub declarations: Declarations,
    /// The operations of its APIs left out at registration: a body other than JSON, a cookie
    /// parameter. Only in the answer of the registration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedOperation>,
}

/// An API of a toolset, as pinned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiSummary {
    pub name: String,
    pub base_url: String,
    /// How its calls authenticate; absent for none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    /// How many tools it has.
    pub tools: u32,
}

/// An operation of an API that registering left out, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkippedOperation {
    pub api: String,
    /// `post`, as the document writes it.
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Written for the harness developer: `its body is application/xml`.
    pub reason: String,
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
    /// Its APIs, sorted by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apis: Vec<ApiSummary>,
    /// The names of its tools, sorted.
    pub tools: Vec<String>,
    /// The names of the workflows it pins, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerSummary {
    pub name: String,
    pub transport: McpTransport,
    /// How its calls authenticate; absent for none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
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
    /// The API, for an API tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
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
    /// The dispatcher of Rewind, as an HTTP client of an API.
    Api,
    /// The harness's SDK worker.
    Host,
    /// The dispatcher of Rewind: `http` and `webhook`.
    Builtin,
}

/// The toolset's declarations, generated by `rewind-codegen`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Declarations {
    /// `rewind-tools.d.ts`: the `rewind:tools` module, with tool descriptions as JSDoc, and the
    /// `rewind:workflows` module when the toolset pins workflows.
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
    fn apis_and_auth_read() {
        let toolset: CreateToolset = serde_json::from_value(json!({
            "mcp": [{
                "name": "github",
                "url": "https://mcp.example.com/github",
                "auth": { "type": "secret", "header": "Authorization", "format": "Bearer {secret}" }
            }],
            "apis": [
                { "name": "crm", "openapi": "https://api.example.com/openapi.json", "auth": { "type": "rewind" } },
                { "name": "dax", "openapi": { "openapi": "3.1.0", "paths": {} }, "baseUrl": "https://dax.example.com" }
            ],
            "secrets": { "github": "org/github" }
        }))
        .unwrap();
        assert_eq!(
            toolset.mcp[0].auth,
            Some(Auth::Secret {
                header: Some("Authorization".into()),
                format: Some("Bearer {secret}".into()),
                encoding: None,
                env: None,
            })
        );
        assert_eq!(toolset.apis[0].auth, Some(Auth::Rewind {}));
        assert!(matches!(toolset.apis[0].openapi, OpenApiDocument::Url(_)));
        assert!(matches!(
            toolset.apis[1].openapi,
            OpenApiDocument::Document(_)
        ));
        assert_eq!(toolset.secrets["github"], "org/github");
        assert!(
            serde_json::from_value::<Auth>(json!({ "type": "rewind", "header": "X" })).is_err(),
            "unknown fields are refused"
        );
        assert!(
            serde_json::from_value::<CreateToolset>(json!({ "credentials": {} })).is_err(),
            "credentials are gone"
        );
        for auth in [
            Auth::Rewind {},
            toolset.mcp[0].auth.clone().unwrap(),
            Auth::Secret {
                header: Some("Authorization".into()),
                format: Some("Basic {secret}".into()),
                encoding: Some(SecretEncoding::Base64),
                env: None,
            },
            Auth::Secret {
                header: None,
                format: None,
                encoding: None,
                env: Some("TOKEN".into()),
            },
        ] {
            assert_eq!(Auth::of_core(&auth.to_core().unwrap()), Some(auth));
        }
        assert_eq!(Auth::None {}.to_core(), Ok(rewind_core::ToolAuth::None));
        assert!(
            Auth::Secret {
                header: Some("X".into()),
                format: None,
                encoding: None,
                env: Some("X".into()),
            }
            .to_core()
            .is_err()
        );
    }
}
