//! Action sets: registering and pinning them, with their MCP servers, their APIs read from OpenAPI
//! documents and how each authenticates its calls; their declarations; search
//! (`archi/04-tools.md`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::common::Duration;

/// `POST /v1/action-sets`: MCP servers, APIs, harness actions and the `http` action, with a policy per
/// action. Registering connects to the MCP servers and lists their actions, reads the APIs' OpenAPI
/// documents, and pins their schemas. The action set is content-addressed: registering the same
/// action set twice gives the same id. It holds no secret: a run attaches its secrets when it
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
    "harness": [{
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
pub struct CreateActionSet {
    /// MCP servers. Their actions are called by Rewind and named `<server>.<action>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<McpServer>,
    /// APIs described by OpenAPI documents: one action per operation, named
    /// `<api>.<operationId>`, called by Rewind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apis: Vec<Api>,
    /// Harness actions: functions of the harness, executed by its SDK worker (`GET /v1/calls`). A
    /// name `group.action` puts the action in the `group` namespace of `rewind:actions`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harness: Vec<HarnessAction>,
    /// Enables the built-in `http` action, and `fetch` in scripts, for these hosts only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpAction>,
    /// The organization's secrets that list the actions of the MCP servers whose `auth` takes one,
    /// by server: `{ "github": "org/github" }`. Only `org/` secrets; no run keeps them, and they
    /// are not part of the action set's id. Requires `secrets:use`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: crate::secrets::Attachments,
    /// The workflows its scripts may start, from `rewind:workflows`: `triage` pins the latest
    /// version, `triage@<version>` or `triage@v12` one version. The versions are pinned: the
    /// action set's scripts are checked against them, and start them, for their whole life.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflows: Vec<String>,
    /// Derives the action set from a registered one: its servers, actions and workflows are kept as
    /// pinned, without listing the servers again; `workflows` adds workflows or pins them again.
    /// `mcp`, `harness` and `http` must then be empty.
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
    /// The namespace of its actions in `rewind:actions`: an identifier that is not a reserved word of
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
    /// Policies of its actions, by action name as the server defines it. Actions not listed get the
    /// default policy.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub policies: BTreeMap<String, ActionPolicy>,
}

/// An API described by an OpenAPI 3.0 or 3.1 document (`archi/04-tools.md`, "API actions"). Each
/// operation is an action: its input the path, query and header parameters by name and the JSON
/// body under `body`; its output the schema of its first `2xx` JSON response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Api {
    /// The namespace of its actions in `rewind:actions`, an identifier as a server's name is.
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
    /// Its calls in flight in the organization. 8 by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    /// Policies of its actions, by operation name. An operation not listed gets the default of
    /// its method: `retry` for `GET`, `HEAD`, `OPTIONS`, `PUT` and `DELETE`; `at_most_once`
    /// for `POST` and `PATCH` unless it declares an `Idempotency-Key` header.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub policies: BTreeMap<String, ActionPolicy>,
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
/// "Authenticating calls"). Part of the pinned action set; never a value.
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
    /// The `auth` an action set pins, what the server and the CLI pin alike. Whether it fits the
    /// server's transport is checked when the action set is built (`rewind_core::ActionSet`).
    pub fn to_core(&self) -> Result<rewind_core::ActionAuth, String> {
        use rewind_core::{ActionAuth, SecretEncoding as Encoding, SecretPlacement};
        Ok(match self {
            Auth::None {} => ActionAuth::None,
            Auth::Rewind {} => ActionAuth::Rewind,
            Auth::Secret {
                header: Some(name),
                format,
                encoding,
                env: None,
            } => ActionAuth::Secret(SecretPlacement::Header {
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
            } => ActionAuth::Secret(SecretPlacement::Env {
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
    pub fn of_core(auth: &rewind_core::ActionAuth) -> Option<Self> {
        use rewind_core::{ActionAuth, SecretEncoding as Encoding, SecretPlacement};
        Some(match auth {
            ActionAuth::None => return None,
            ActionAuth::Rewind => Auth::Rewind {},
            ActionAuth::Secret(SecretPlacement::Header {
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
            ActionAuth::Secret(SecretPlacement::Env { variable }) => Auth::Secret {
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
pub struct HarnessAction {
    /// `group.action` puts the action in a group, the namespace `rewind:actions` exports it under. The
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
    pub policy: Option<ActionPolicy>,
}

/// The built-in `http` action: requests leave from the dispatcher, never from the isolate, and
/// only toward these hosts (`archi/04-tools.md`, "The `http` action and egress").
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpAction {
    /// Host names, such as `api.example.com`. `*.example.com` allows the subdomains.
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ActionPolicy>,
}

/// An action's policy, pinned in the action set (`archi/04-tools.md`, "Policies"). Every field is
/// optional: what is not given takes its default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionPolicy {
    /// What happens to a call interrupted mid-flight. `retry` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_interruption: Option<OnInterruption>,
    /// The call waits for the harness's approval (`POST /v1/calls/{id}/approve`) before it is
    /// sent. Off by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<bool>,
    /// How long one attempt may take. 5 minutes by default for MCP actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Duration>,
    /// The grace after a stream closes: how long a harness call handed on a stream that closed
    /// waits for its worker to come back before it is handed again, as attempt + 1. 60 seconds
    /// by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_timeout: Option<Duration>,
    /// At most this many calls of the action in flight per run. The run's own limit (16) still
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
    /// Fail with `outcomeUnknown` at once. For actions that ignore idempotency keys.
    AtMostOnce,
}

/// A registered action set, as pinned.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActionSet {
    /// The digest of the pinned action set (SHA-256, lowercase hex). Runs record it.
    pub id: String,
    pub servers: Vec<ServerSummary>,
    /// Every action a script of this action set can call, sorted by name.
    pub actions: Vec<Action>,
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

/// An API of an action set, as pinned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiSummary {
    pub name: String,
    pub base_url: String,
    /// How its calls authenticate; absent for none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Auth>,
    /// How many actions it has.
    pub actions: u32,
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

/// `GET /v1/action-sets`: the action sets registered by the tenant, by id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActionSetList {
    pub action_sets: Vec<ActionSetSummary>,
}

/// A registered action set, without its schemas and declarations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActionSetSummary {
    pub id: String,
    pub servers: Vec<ServerSummary>,
    /// Its APIs, sorted by name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apis: Vec<ApiSummary>,
    /// The names of its actions, sorted.
    pub actions: Vec<String>,
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
    /// How many actions it has.
    pub actions: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Http,
    Stdio,
}

/// A pinned action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    /// As scripts call it: `github.list_issues`, `ask_user`, `http`.
    pub name: String,
    pub route: Route,
    /// The MCP server, for an MCP action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// The API, for an API action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    pub description: String,
    #[schema(value_type = Object)]
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub output_schema: Option<Value>,
    /// The policy with its defaults filled in.
    pub policy: ActionPolicy,
}

/// Who calls the action (`archi/04-tools.md`, "Routes").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// The dispatcher of Rewind, as an MCP client.
    Mcp,
    /// The dispatcher of Rewind, as an HTTP client of an API.
    Api,
    /// The harness's SDK worker.
    Harness,
    /// The dispatcher of Rewind: `http` and `webhook`.
    Builtin,
}

/// The action set's declarations, generated by `rewind-codegen`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Declarations {
    /// `rewind-actions.d.ts`: the `rewind:actions` module, with action descriptions as JSDoc, and the
    /// `rewind:workflows` module when the action set pins workflows.
    pub dts: String,
    /// The compact rendering for the model's prompt, one line per action.
    pub prompt: String,
}

/// `GET /v1/action-sets/{id}/declarations`.
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
    /// `rewind-actions.d.ts`.
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

/// `GET /v1/action-sets/{id}/search`: `search_actions`, for action sets too large for the prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchQuery {
    /// Words to look for in action names and descriptions.
    pub query: String,
    /// At most this many actions (10 by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// The actions matching a search, best first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActionSearchResults {
    pub actions: Vec<ActionMatch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActionMatch {
    pub name: String,
    pub description: String,
    /// The action's line of the prompt rendering.
    pub prompt: String,
    /// The action's declaration in `rewind-actions.d.ts`, with the types it uses.
    pub dts: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_action_set_of_the_product_overview_reads() {
        let action_set: CreateActionSet = serde_json::from_value(json!({
            "mcp": [{ "name": "github", "url": "https://mcp.example.com/github" }],
            "harness": [{
                "name": "ask_user",
                "description": "Ask the end user a question and return their answer.",
                "inputSchema": { "type": "object" },
                "policy": { "onInterruption": "at_most_once", "heartbeatTimeout": "2m" }
            }],
            "http": { "allow": ["api.example.com"] }
        }))
        .unwrap();
        let policy = action_set.harness[0].policy.unwrap();
        assert_eq!(policy.on_interruption, Some(OnInterruption::AtMostOnce));
        assert_eq!(policy.heartbeat_timeout.unwrap().as_millis(), 120_000);
        assert!(
            serde_json::from_value::<CreateActionSet>(json!({ "mcp": [], "tool": [] })).is_err(),
            "unknown fields are refused"
        );
    }

    #[test]
    fn apis_and_auth_read() {
        let action_set: CreateActionSet = serde_json::from_value(json!({
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
            action_set.mcp[0].auth,
            Some(Auth::Secret {
                header: Some("Authorization".into()),
                format: Some("Bearer {secret}".into()),
                encoding: None,
                env: None,
            })
        );
        assert_eq!(action_set.apis[0].auth, Some(Auth::Rewind {}));
        assert!(matches!(
            action_set.apis[0].openapi,
            OpenApiDocument::Url(_)
        ));
        assert!(matches!(
            action_set.apis[1].openapi,
            OpenApiDocument::Document(_)
        ));
        assert_eq!(action_set.secrets["github"], "org/github");
        assert!(
            serde_json::from_value::<Auth>(json!({ "type": "rewind", "header": "X" })).is_err(),
            "unknown fields are refused"
        );
        assert!(
            serde_json::from_value::<CreateActionSet>(json!({ "credentials": {} })).is_err(),
            "credentials are gone"
        );
        for auth in [
            Auth::Rewind {},
            action_set.mcp[0].auth.clone().unwrap(),
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
        assert_eq!(Auth::None {}.to_core(), Ok(rewind_core::ActionAuth::None));
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
