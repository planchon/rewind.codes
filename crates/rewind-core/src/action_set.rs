//! The pinned action set (`archi/04-tools.md`, "Action sets and code generation").
//!
//! Registering an action set connects to its MCP servers, lists their tools, reads its APIs' OpenAPI
//! documents and **pins** them: the servers and APIs, with how each one authenticates its calls,
//! and for each tool its name, route, description, schemas and policy. The pinned action set is
//! immutable and content-addressed: its digest is the SHA-256 of its canonical JSON, the pinned
//! action set document of the stored format v1 (`archi/02-journal.md`), and every run records the
//! digest it was started with.
//!
//! ```text
//! Action set
//! ├── servers:   McpServer { name, transport: http(url) | stdio(command, args, env), concurrency,
//! │                          auth }
//! ├── apis:      Api { name, base_url, auth, concurrency }
//! ├── tools:     Tool { name, route, operation?, description?, input_schema, output_schema?,
//! │                     policy }
//! │                     route  = mcp(server) | api(api) | host(group?)
//! │                              | builtin(http(allowed_hosts) | webhook)
//! │                     operation (API tools only) = { method, path, parameters, body,
//! │                                                    idempotency_key }
//! │                     policy = { on_interruption: retry | at_most_once, approval,
//! │                                execution_timeout?, heartbeat_timeout, concurrency }
//! └── workflows: PinnedWorkflow { name, version, description?, schemas }
//!                  each gives two built-ins, not in the document:
//!                  workflows.<name>.start and workflows.<name>.signal (builtin(workflow))
//! auth = none | rewind | secret(header(name, format?, encoding) | env(variable))
//! ```
//!
//! A tool is named as its server defines it, behind its namespace: `github.list_issues` for the
//! tool `list_issues` of the MCP server `github`, `dax.createMeasure` for the operation
//! `createMeasure` of the API `dax`, `harness.ask_user` or `ask_user` for a host tool, `http` and
//! `webhook` for the built-ins. The namespace is what `rewind:tools` exports. The namespace
//! `workflows` is reserved for the workflows' built-ins; `rewind:workflows` exports one name per
//! pinned workflow.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::canonical::canonical_json;
use crate::digest::Digest;
use crate::secret::{
    RESERVED_HEADERS, SECRET_PLACEHOLDER, SecretEncoding, SecretPlacement, ToolAuth, is_env_name,
    is_header_name,
};
use crate::workflow::{InvalidWorkflow, PinnedWorkflow, WORKFLOWS_NAMESPACE, WorkflowOp};

/// The tools a run may call, pinned when the action set was registered.
///
/// Built with [`ActionSet::new`] or [`ActionSet::from_parts`], which check every invariant: unique
/// names, MCP and API tools of known servers and APIs and inside their namespace, an operation
/// for each API tool, valid policies and `auth`. Servers, APIs and tools are kept sorted by name,
/// so the digest does not depend on the order they were given in.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ActionSet {
    servers: Vec<McpServer>,
    /// Sorted by name.
    apis: Vec<Api>,
    tools: Vec<Tool>,
    /// Sorted by name.
    workflows: Vec<PinnedWorkflow>,
    /// The built-ins of the workflows, derived from them, sorted by name.
    workflow_tools: Vec<Tool>,
}

/// An MCP server of an action set, as it was pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServer {
    /// The namespace of its tools in `rewind:tools`: an identifier, such as `github`.
    pub name: String,
    pub transport: McpTransport,
    /// Calls in flight to this server, on each worker. The claim of tool tasks enforces it.
    pub concurrency: u32,
    /// How its calls prove who calls (`archi/04-tools.md`, "Authenticating calls"): a header or
    /// a signature for an HTTP server, a variable for a stdio server.
    pub auth: ToolAuth,
}

impl McpServer {
    /// Calls in flight to one MCP server on each worker, unless the action set says otherwise.
    pub const DEFAULT_CONCURRENCY: u32 = 8;

    /// A public server with the default concurrency.
    pub fn new(name: impl Into<String>, transport: McpTransport) -> Self {
        Self {
            name: name.into(),
            transport,
            concurrency: Self::DEFAULT_CONCURRENCY,
            auth: ToolAuth::None,
        }
    }
}

/// An API of an action set, described by an OpenAPI document read once at registration
/// (`archi/04-tools.md`, "API tools"). The document is not kept: each of its operations is a
/// tool, with its [`ApiOperation`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Api {
    /// The namespace of its tools in `rewind:tools`, an identifier as a server's name is.
    pub name: String,
    /// Where the calls go, the operations' paths joined to it: `https://api.example.com/v1`.
    pub base_url: String,
    /// How its calls prove who calls: the `auth` given at registration, or what the document's
    /// security requirement means, resolved then.
    pub auth: ToolAuth,
    /// Calls in flight to this API, on each worker. The claim of tool tasks enforces it.
    pub concurrency: u32,
}

impl Api {
    /// Calls in flight to one API on each worker, unless the action set says otherwise.
    pub const DEFAULT_CONCURRENCY: u32 = 8;

    /// A public API with the default concurrency.
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            auth: ToolAuth::None,
            concurrency: Self::DEFAULT_CONCURRENCY,
        }
    }
}

/// How the dispatcher builds the request of an API tool from its args: the operation of the
/// OpenAPI document the tool was read from (`archi/04-tools.md`, "API tools").
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ApiOperation {
    pub method: HttpMethod,
    /// The path template, as the document writes it, joined to the API's `base_url`:
    /// `/v1/measures/{id}`. Each `{name}` is a path parameter, percent-encoded into it.
    pub path: String,
    /// Its path, query and header parameters, which are args of the same names. Sorted by name;
    /// no two share one, and none is named `body`.
    pub parameters: Vec<ApiParameter>,
    /// It takes a JSON request body: the arg `body`.
    pub body: bool,
    /// It declares the header `Idempotency-Key`, which Rewind fills with `{run_id}/{seq}`.
    pub idempotency_key: bool,
}

/// A parameter of an operation: an arg of the tool, put where the document says.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ApiParameter {
    pub name: String,
    pub location: ParameterLocation,
}

/// Where a parameter goes. Cookies are not supported: an operation with one is left out at
/// registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ParameterLocation {
    Path,
    Query,
    Header,
}

impl ParameterLocation {
    /// As OpenAPI writes it in `in`.
    pub fn as_str(self) -> &'static str {
        match self {
            ParameterLocation::Path => "path",
            ParameterLocation::Query => "query",
            ParameterLocation::Header => "header",
        }
    }
}

/// The methods of OpenAPI's path items.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum HttpMethod {
    Get,
    Put,
    Post,
    Delete,
    Options,
    Head,
    Patch,
    Trace,
}

impl HttpMethod {
    /// Lowercase, as OpenAPI's path items name them: `post`.
    pub fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "get",
            HttpMethod::Put => "put",
            HttpMethod::Post => "post",
            HttpMethod::Delete => "delete",
            HttpMethod::Options => "options",
            HttpMethod::Head => "head",
            HttpMethod::Patch => "patch",
            HttpMethod::Trace => "trace",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "get" => HttpMethod::Get,
            "put" => HttpMethod::Put,
            "post" => HttpMethod::Post,
            "delete" => HttpMethod::Delete,
            "options" => HttpMethod::Options,
            "head" => HttpMethod::Head,
            "patch" => HttpMethod::Patch,
            "trace" => HttpMethod::Trace,
            _ => return None,
        })
    }

    /// Whether HTTP defines a request sent twice to have the effect of one (RFC 9110, 9.2.2).
    pub fn is_idempotent(self) -> bool {
        !matches!(self, HttpMethod::Post | HttpMethod::Patch)
    }
}

/// How the dispatcher reaches an MCP server. How its calls authenticate is the server's
/// [`auth`](McpServer::auth); no value is part of the transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransport {
    /// Streamable HTTP.
    Http { url: String },
    /// A process started by the dispatcher, speaking MCP on its standard input and output.
    /// `env` holds configuration, never secrets: an attached secret goes in the variable the
    /// server's `auth` names.
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

/// One tool, as pinned.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    /// The full name scripts call it by, such as `github.list_issues`.
    pub name: String,
    pub route: ToolRoute,
    /// For an API tool, and only for one: how its request is built.
    pub operation: Option<ApiOperation>,
    /// Shown to the model, as JSDoc and in the prompt rendering.
    pub description: Option<String>,
    /// JSON Schema of the arguments. Args are validated against it inside the sandbox, before
    /// the call is recorded.
    pub input_schema: Value,
    /// JSON Schema of the result, when the tool declares one.
    pub output_schema: Option<Value>,
    pub policy: ToolPolicy,
}

impl Tool {
    /// A tool with the default policy of its route, no description and no output schema.
    pub fn new(name: impl Into<String>, route: ToolRoute, input_schema: Value) -> Self {
        let policy = ToolPolicy::default_for(&route);
        Self {
            name: name.into(),
            route,
            operation: None,
            description: None,
            input_schema,
            output_schema: None,
            policy,
        }
    }

    /// The tool of an API's operation, with the default policy of its method
    /// ([`ToolPolicy::default_for_operation`]), no description and no output schema.
    pub fn api(
        name: impl Into<String>,
        api: impl Into<String>,
        operation: ApiOperation,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            route: ToolRoute::Api { api: api.into() },
            policy: ToolPolicy::default_for_operation(&operation),
            operation: Some(operation),
            description: None,
            input_schema,
            output_schema: None,
        }
    }

    /// The namespace `rewind:tools` exports the tool under: its MCP server, its API or its
    /// host-tool group. `None` for a bare host tool and for the built-ins.
    pub fn namespace(&self) -> Option<&str> {
        match &self.route {
            ToolRoute::Mcp { server } => Some(server),
            ToolRoute::Api { api } => Some(api),
            ToolRoute::Host { group } => group.as_deref(),
            ToolRoute::Builtin(_) => None,
        }
    }

    /// The name of the tool inside its namespace: `list_issues` for `github.list_issues`. For
    /// an MCP tool, the name its server defines.
    pub fn local_name(&self) -> &str {
        match self.namespace() {
            Some(namespace) => &self.name[namespace.len() + 1..],
            None => &self.name,
        }
    }
}

/// Who calls a tool (`archi/04-tools.md`, "Routes").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolRoute {
    /// A tool of one of the action set's MCP servers, called by the dispatcher of Rewind.
    Mcp { server: String },
    /// An operation of one of the action set's APIs, called by the dispatcher of Rewind as an HTTP
    /// client (`archi/04-tools.md`, "API tools").
    Api { api: String },
    /// A built-in, called by the dispatcher of Rewind.
    Builtin(Builtin),
    /// A function of the harness, executed by its SDK worker, which claims the task through
    /// `GET /v1/tasks`. A grouped host tool is named `{group}.{tool}`.
    Host { group: Option<String> },
}

impl ToolRoute {
    /// Whether Rewind calls the tool itself: MCP tools, API tools and built-ins. Host tools are
    /// claimed by the harness's worker instead.
    pub fn is_dispatched(&self) -> bool {
        !matches!(self, ToolRoute::Host { .. })
    }

    /// The MCP server or the API the tool belongs to, whose `auth` its calls carry.
    pub fn server(&self) -> Option<&str> {
        match self {
            ToolRoute::Mcp { server } => Some(server),
            ToolRoute::Api { api } => Some(api),
            ToolRoute::Builtin(_) | ToolRoute::Host { .. } => None,
        }
    }
}

/// The built-in tools. An action set enables one by listing it, named as the built-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Builtin {
    /// `http`: the only way a script reaches the network, through `fetch`. Requests leave from
    /// the dispatcher, only toward these hosts (`archi/04-tools.md`, "The http tool and
    /// egress"). Hosts are lowercase, sorted and unique, and there is at least one.
    Http { allowed_hosts: Vec<String> },
    /// `webhook`: `webhook(name)` returns a URL bound to the run and the signal name.
    Webhook,
    /// `workflows.<workflow>.start` or `workflows.<workflow>.signal`: starts or signals a run of
    /// a workflow the action set pins (`archi/03-execution.md`, "Workflows across runs"). Derived
    /// from the pinned workflow, never declared as a tool.
    Workflow { workflow: String, op: WorkflowOp },
}

impl Builtin {
    /// The name of the tool, as scripts call it.
    pub fn name(&self) -> String {
        match self {
            Builtin::Http { .. } => "http".into(),
            Builtin::Webhook => "webhook".into(),
            Builtin::Workflow { workflow, op } => {
                format!("{WORKFLOWS_NAMESPACE}.{workflow}.{}", op.as_str())
            }
        }
    }
}

/// What happens to a call whose outcome is unknown (`archi/04-tools.md`, "Policies").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Interruption {
    /// Call again with the same idempotency key: transient failures are retried with backoff,
    /// and an expired claim is claimed again, up to 5 attempts. The default.
    Retry,
    /// Never call twice: a transient failure fails the call, and an expired claim fails it with
    /// `outcome_unknown`. For tools that ignore the idempotency key.
    AtMostOnce,
}

impl Interruption {
    pub fn as_str(self) -> &'static str {
        match self {
            Interruption::Retry => "retry",
            Interruption::AtMostOnce => "at_most_once",
        }
    }
}

/// The policy of one tool, pinned in the action set and set per tool by the harness when it
/// registers the action set (`archi/04-tools.md`, "Policies").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ToolPolicy {
    pub on_interruption: Interruption,
    /// The task waits for the harness's approval before it is sent.
    pub approval: bool,
    /// How long one attempt may take. `None`: no limit but the run's TTL, the default of host
    /// tools, which may last days.
    pub execution_timeout: Option<Duration>,
    /// How long a claim lives without a heartbeat. The dispatcher and the harness's workers
    /// renew their claims before it passes.
    pub heartbeat_timeout: Duration,
    /// Calls of this tool in flight at once in one run. The claim of tool tasks enforces it.
    pub concurrency: u32,
}

impl ToolPolicy {
    /// Calls of one tool in flight in one run, unless the action set says otherwise.
    pub const DEFAULT_CONCURRENCY: u32 = 16;
    /// The execution timeout of the tools Rewind calls itself (MCP tools, API tools and
    /// built-ins).
    pub const DEFAULT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
    pub const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);

    /// The default policy of a route: `retry`, no approval, 16 calls in flight per run, a
    /// heartbeat timeout of 60 s, and an execution timeout of 5 min except for host tools.
    pub fn default_for(route: &ToolRoute) -> Self {
        Self {
            on_interruption: Interruption::Retry,
            approval: false,
            execution_timeout: match route {
                ToolRoute::Host { .. } => None,
                ToolRoute::Mcp { .. } | ToolRoute::Api { .. } | ToolRoute::Builtin(_) => {
                    Some(Self::DEFAULT_EXECUTION_TIMEOUT)
                }
            },
            heartbeat_timeout: Self::DEFAULT_HEARTBEAT_TIMEOUT,
            concurrency: Self::DEFAULT_CONCURRENCY,
        }
    }

    /// The default policy of an API's operation: as an MCP tool's, but `at_most_once` for a
    /// `POST` or a `PATCH` that declares no `Idempotency-Key`, which HTTP does not let a client
    /// send twice (`archi/04-tools.md`, "API tools").
    pub fn default_for_operation(operation: &ApiOperation) -> Self {
        let mut policy = Self::default_for(&ToolRoute::Api { api: String::new() });
        if !operation.method.is_idempotent() && !operation.idempotency_key {
            policy.on_interruption = Interruption::AtMostOnce;
        }
        policy
    }
}

/// An action set that breaks an invariant. The messages are written for the harness developer who
/// registers the action set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActionSetError {
    #[error(
        "invalid server name {0:?}: expected an identifier (letters, digits, _ and $) that is \
         not a reserved word of JavaScript"
    )]
    InvalidServerName(String),
    #[error("server {0:?} is declared twice")]
    DuplicateServer(String),
    #[error("server {server:?}: {message}")]
    InvalidServer { server: String, message: String },
    #[error(
        "invalid API name {0:?}: expected an identifier (letters, digits, _ and $) that is not a \
         reserved word of JavaScript"
    )]
    InvalidApiName(String),
    #[error("API {0:?} is declared twice, or has the name of a server")]
    DuplicateApi(String),
    #[error("API {api:?}: {message}")]
    InvalidApi { api: String, message: String },
    #[error("invalid tool name {0:?}")]
    InvalidToolName(String),
    #[error("tool {0:?} is declared twice")]
    DuplicateTool(String),
    #[error("tool {tool:?} belongs to server {server:?}, which the action set does not declare")]
    UnknownServer { tool: String, server: String },
    #[error("tool {tool:?} belongs to API {api:?}, which the action set does not declare")]
    UnknownApi { tool: String, api: String },
    #[error("tool {tool:?} must be named {expected}")]
    MisplacedTool { tool: String, expected: String },
    #[error("tool {tool:?}: {message}")]
    InvalidTool { tool: String, message: String },
    #[error("workflow {0:?} is pinned twice")]
    DuplicateWorkflow(String),
    #[error(transparent)]
    InvalidWorkflow(#[from] InvalidWorkflow),
}

impl ActionSet {
    /// An action set of these MCP servers and tools, without APIs: [`from_parts`](Self::from_parts).
    pub fn new(
        servers: impl IntoIterator<Item = McpServer>,
        tools: impl IntoIterator<Item = Tool>,
    ) -> Result<Self, ActionSetError> {
        Self::from_parts(servers, [], tools)
    }

    /// An action set of these MCP servers, APIs and tools, sorted by name. The allowed hosts of
    /// `http` are lowercased, sorted and deduplicated; an operation's parameters are sorted by
    /// name.
    pub fn from_parts(
        servers: impl IntoIterator<Item = McpServer>,
        apis: impl IntoIterator<Item = Api>,
        tools: impl IntoIterator<Item = Tool>,
    ) -> Result<Self, ActionSetError> {
        let mut servers: Vec<McpServer> = servers.into_iter().collect();
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        for pair in servers.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(ActionSetError::DuplicateServer(pair[0].name.clone()));
            }
        }
        for server in &servers {
            check_server(server)?;
        }
        let mut apis: Vec<Api> = apis.into_iter().collect();
        apis.sort_by(|a, b| a.name.cmp(&b.name));
        for pair in apis.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(ActionSetError::DuplicateApi(pair[0].name.clone()));
            }
        }
        for api in &apis {
            if servers
                .binary_search_by(|server| server.name.as_str().cmp(&api.name))
                .is_ok()
            {
                return Err(ActionSetError::DuplicateApi(api.name.clone()));
            }
            check_api(api)?;
        }

        let mut tools: Vec<Tool> = tools.into_iter().collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        for pair in tools.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(ActionSetError::DuplicateTool(pair[0].name.clone()));
            }
        }
        for tool in &mut tools {
            if let ToolRoute::Builtin(Builtin::Http { allowed_hosts }) = &mut tool.route {
                let hosts: BTreeSet<String> = allowed_hosts
                    .iter()
                    .map(|host| host.to_lowercase())
                    .collect();
                *allowed_hosts = hosts.into_iter().collect();
            }
            if let Some(operation) = &mut tool.operation {
                operation.parameters.sort_by(|a, b| a.name.cmp(&b.name));
            }
            check_tool(tool, &servers, &apis)?;
        }
        Ok(Self {
            servers,
            apis,
            tools,
            workflows: Vec::new(),
            workflow_tools: Vec::new(),
        })
    }

    /// This action set, pinning these workflows instead of those it pinned: each gives the
    /// built-ins `workflows.<name>.start` and `workflows.<name>.signal`.
    pub fn with_workflows(
        mut self,
        workflows: impl IntoIterator<Item = PinnedWorkflow>,
    ) -> Result<Self, ActionSetError> {
        let mut workflows: Vec<PinnedWorkflow> = workflows.into_iter().collect();
        workflows.sort_by(|a, b| a.name.cmp(&b.name));
        for pair in workflows.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(ActionSetError::DuplicateWorkflow(pair[0].name.clone()));
            }
        }
        for workflow in &workflows {
            workflow.check()?;
        }
        let mut workflow_tools: Vec<Tool> = workflows
            .iter()
            .flat_map(|workflow| {
                [WorkflowOp::Start, WorkflowOp::Signal].map(|op| {
                    let route = ToolRoute::Builtin(Builtin::Workflow {
                        workflow: workflow.name.clone(),
                        op,
                    });
                    Tool::new(workflow.tool_name(op), route, json!({}))
                })
            })
            .collect();
        workflow_tools.sort_by(|a, b| a.name.cmp(&b.name));
        self.workflows = workflows;
        self.workflow_tools = workflow_tools;
        Ok(self)
    }

    /// An action set with no tool.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Host tools with these names, the default policy, and an input schema that accepts any
    /// value: what mock mode and tests use when the action set does not matter. A name
    /// `group.tool` puts the tool in the host-tool group `group`. Duplicates are ignored.
    pub fn from_host_tools<S: Into<String>>(
        names: impl IntoIterator<Item = S>,
    ) -> Result<Self, ActionSetError> {
        let names: BTreeSet<String> = names.into_iter().map(Into::into).collect();
        let tools = names.into_iter().map(|name| {
            let group = name.split_once('.').map(|(group, _)| group.to_owned());
            Tool::new(name, ToolRoute::Host { group }, json!({}))
        });
        Self::new([], tools)
    }

    /// The MCP servers, sorted by name.
    pub fn servers(&self) -> &[McpServer] {
        &self.servers
    }

    /// The APIs, sorted by name.
    pub fn apis(&self) -> &[Api] {
        &self.apis
    }

    pub fn api(&self, name: &str) -> Option<&Api> {
        self.apis
            .binary_search_by(|api| api.name.as_str().cmp(name))
            .ok()
            .map(|index| &self.apis[index])
    }

    /// How the calls to the MCP server or the API `name` authenticate, if the action set has one of
    /// that name.
    pub fn auth(&self, name: &str) -> Option<&ToolAuth> {
        self.server(name)
            .map(|server| &server.auth)
            .or_else(|| self.api(name).map(|api| &api.auth))
    }

    /// The MCP servers and APIs whose `auth` takes a secret, by name, sorted: those a run that
    /// imports them must attach a secret for.
    pub fn taking_secrets(&self) -> impl Iterator<Item = &str> {
        let servers = self
            .servers
            .iter()
            .filter(|server| server.auth.takes_secret())
            .map(|server| server.name.as_str());
        let apis = self
            .apis
            .iter()
            .filter(|api| api.auth.takes_secret())
            .map(|api| api.name.as_str());
        let mut names: Vec<&str> = servers.chain(apis).collect();
        names.sort_unstable();
        names.into_iter()
    }

    /// The tools, sorted by name: those the action set declares, without the workflows'
    /// built-ins.
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// The pinned workflows, sorted by name.
    pub fn workflows(&self) -> &[PinnedWorkflow] {
        &self.workflows
    }

    pub fn workflow(&self, name: &str) -> Option<&PinnedWorkflow> {
        self.workflows
            .binary_search_by(|workflow| workflow.name.as_str().cmp(name))
            .ok()
            .map(|index| &self.workflows[index])
    }

    /// A tool scripts may call: one the action set declares, or a built-in of a pinned workflow.
    pub fn tool(&self, name: &str) -> Option<&Tool> {
        let find = |tools: &'_ [Tool]| {
            tools
                .binary_search_by(|tool| tool.name.as_str().cmp(name))
                .ok()
        };
        if let Some(index) = find(&self.tools) {
            return Some(&self.tools[index]);
        }
        find(&self.workflow_tools).map(|index| &self.workflow_tools[index])
    }

    pub fn server(&self, name: &str) -> Option<&McpServer> {
        self.servers
            .binary_search_by(|server| server.name.as_str().cmp(name))
            .ok()
            .map(|index| &self.servers[index])
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tool(name).is_some()
    }

    /// SHA-256 of the action set's canonical JSON (RFC 8785), which is the pinned action set document
    /// of the stored format v1, `"v": 1` included. The document `rewind-format` writes for a
    /// action set hashes to this digest.
    pub fn digest(&self) -> Digest {
        Digest::of(canonical_json(&self.document()).as_bytes())
    }

    /// The pinned action set document v1, as a JSON value. `workflows` is only there when the
    /// action set pins some, `apis` when it has some, a server's `auth` when it is not none and a
    /// tool's `operation` for an API tool: the digests of the action sets without them did not
    /// change when they came.
    fn document(&self) -> Value {
        let servers: Vec<Value> = self.servers.iter().map(server_json).collect();
        let tools: Vec<Value> = self.tools.iter().map(tool_json).collect();
        let mut document = json!({ "v": 1, "servers": servers, "tools": tools });
        if !self.apis.is_empty() {
            let apis: Vec<Value> = self.apis.iter().map(api_json).collect();
            document["apis"] = Value::Array(apis);
        }
        if !self.workflows.is_empty() {
            let workflows: Vec<Value> = self
                .workflows
                .iter()
                .map(PinnedWorkflow::document)
                .collect();
            document["workflows"] = Value::Array(workflows);
        }
        document
    }
}

impl fmt::Display for ActionSet {
    /// `action_set 9f2c4e1a07b3 (3 tools)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "action set {} ({} tools",
            self.digest().short(),
            self.tools.len()
        )?;
        if !self.workflows.is_empty() {
            write!(f, ", {} workflows", self.workflows.len())?;
        }
        f.write_str(")")
    }
}

// -------------------------------------------------------------------------------------------
// Invariants

fn check_server(server: &McpServer) -> Result<(), ActionSetError> {
    let invalid = |message: &str| ActionSetError::InvalidServer {
        server: server.name.clone(),
        message: message.to_owned(),
    };
    if !is_identifier(&server.name) || is_reserved(&server.name) {
        return Err(ActionSetError::InvalidServerName(server.name.clone()));
    }
    if server.name == WORKFLOWS_NAMESPACE {
        return Err(invalid(
            "the namespace \"workflows\" is reserved for the built-ins of the workflows",
        ));
    }
    if server.concurrency == 0 {
        return Err(invalid("its concurrency must be at least 1"));
    }
    match &server.transport {
        McpTransport::Http { url } if url.is_empty() => return Err(invalid("its URL is empty")),
        McpTransport::Stdio { command, .. } if command.is_empty() => {
            return Err(invalid("its command is empty"));
        }
        _ => {}
    }
    let stdio = match &server.transport {
        McpTransport::Stdio { env, .. } => Some(env),
        McpTransport::Http { .. } => None,
    };
    match (&server.auth, stdio) {
        (ToolAuth::None, _) => Ok(()),
        (ToolAuth::Rewind, Some(_)) => Err(invalid(
            "a stdio server cannot check a signature: its auth can be a secret in a variable",
        )),
        (ToolAuth::Secret(SecretPlacement::Header { .. }), Some(_)) => Err(invalid(
            "a stdio server takes its secret in a variable (\"env\"), not in a header",
        )),
        (ToolAuth::Secret(SecretPlacement::Env { .. }), None) => Err(invalid(
            "an HTTP server takes its secret in a header, not in a variable",
        )),
        (ToolAuth::Secret(SecretPlacement::Env { variable }), Some(env)) => {
            if !is_env_name(variable) {
                Err(invalid(&format!(
                    "{variable:?} is not the name of an environment variable"
                )))
            } else if env.contains_key(variable) {
                Err(invalid(&format!(
                    "its configuration already sets {variable:?}, which its auth takes"
                )))
            } else {
                Ok(())
            }
        }
        (auth, None) => check_header_auth(auth).map_err(|message| invalid(&message)),
    }
}

fn check_api(api: &Api) -> Result<(), ActionSetError> {
    let invalid = |message: String| ActionSetError::InvalidApi {
        api: api.name.clone(),
        message,
    };
    if !is_identifier(&api.name) || is_reserved(&api.name) {
        return Err(ActionSetError::InvalidApiName(api.name.clone()));
    }
    if api.name == WORKFLOWS_NAMESPACE {
        return Err(invalid(
            "the namespace \"workflows\" is reserved for the built-ins of the workflows".into(),
        ));
    }
    if api.concurrency == 0 {
        return Err(invalid("its concurrency must be at least 1".into()));
    }
    let base = api
        .base_url
        .strip_prefix("https://")
        .or_else(|| api.base_url.strip_prefix("http://"));
    if base.is_none_or(|rest| rest.is_empty() || rest.contains(['?', '#'])) {
        return Err(invalid(format!(
            "its base URL {:?} must be an http or https URL, without a query or a fragment",
            api.base_url
        )));
    }
    if let ToolAuth::Secret(SecretPlacement::Env { .. }) = api.auth {
        return Err(invalid(
            "an API takes its secret in a header, not in a variable".into(),
        ));
    }
    check_header_auth(&api.auth).map_err(invalid)
}

/// The `auth` of an HTTP server or an API.
fn check_header_auth(auth: &ToolAuth) -> Result<(), String> {
    let ToolAuth::Secret(SecretPlacement::Header { name, format, .. }) = auth else {
        return Ok(());
    };
    if !is_header_name(name) {
        return Err(format!("{name:?} is not the name of an HTTP header"));
    }
    if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(format!("Rewind sets the header {name:?} itself"));
    }
    if let Some(format) = format
        && (!format.contains(SECRET_PLACEHOLDER) || format.chars().any(char::is_control))
    {
        return Err(format!(
            "the format {format:?} of its header must hold {SECRET_PLACEHOLDER} and no control \
             character"
        ));
    }
    Ok(())
}

fn check_tool(tool: &Tool, servers: &[McpServer], apis: &[Api]) -> Result<(), ActionSetError> {
    let name = tool.name.as_str();
    let invalid = |message: String| ActionSetError::InvalidTool {
        tool: name.to_owned(),
        message,
    };
    let misplaced = |expected: String| ActionSetError::MisplacedTool {
        tool: name.to_owned(),
        expected,
    };
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(ActionSetError::InvalidToolName(name.to_owned()));
    }
    match &tool.route {
        ToolRoute::Mcp { server } => {
            if servers
                .binary_search_by(|known| known.name.as_str().cmp(server))
                .is_err()
            {
                return Err(ActionSetError::UnknownServer {
                    tool: name.to_owned(),
                    server: server.clone(),
                });
            }
            in_namespace(name, server).ok_or_else(|| misplaced(format!("{server}.<tool>")))?;
        }
        ToolRoute::Api { api } => {
            if apis
                .binary_search_by(|known| known.name.as_str().cmp(api))
                .is_err()
            {
                return Err(ActionSetError::UnknownApi {
                    tool: name.to_owned(),
                    api: api.clone(),
                });
            }
            in_namespace(name, api).ok_or_else(|| misplaced(format!("{api}.<operation>")))?;
            let Some(operation) = &tool.operation else {
                return Err(invalid("an API tool needs its operation".into()));
            };
            check_operation(operation).map_err(invalid)?;
        }
        ToolRoute::Host { group: Some(group) } => {
            if !is_identifier(group) {
                return Err(invalid(format!(
                    "its group {group:?} is not an identifier (letters, digits, _ and $)"
                )));
            }
            if is_reserved(group) {
                return Err(invalid(format!(
                    "its group {group:?} is a reserved word of JavaScript: scripts could not \
                     import it"
                )));
            }
            if group == WORKFLOWS_NAMESPACE {
                return Err(invalid(
                    "the group \"workflows\" is reserved for the built-ins of the workflows".into(),
                ));
            }
            in_namespace(name, group).ok_or_else(|| misplaced(format!("{group}.<tool>")))?;
        }
        ToolRoute::Host { group: None } => {
            if name.contains('.') {
                let group = name.split('.').next().unwrap_or_default();
                return Err(misplaced(format!(
                    "without a dot, or be put in the host-tool group {group:?}"
                )));
            }
            if is_reserved(name) {
                return Err(invalid(
                    "its name is a reserved word of JavaScript: scripts could not import it, \
                     unless it is put in a host-tool group"
                        .into(),
                ));
            }
        }
        ToolRoute::Builtin(Builtin::Workflow { .. }) => {
            return Err(invalid(
                "the built-ins of a workflow come from the workflows the action set pins".into(),
            ));
        }
        ToolRoute::Builtin(builtin) => {
            if name != builtin.name() {
                return Err(misplaced(format!("{:?}", builtin.name())));
            }
            if let Builtin::Http { allowed_hosts } = builtin
                && (allowed_hosts.is_empty() || allowed_hosts.iter().any(String::is_empty))
            {
                return Err(invalid(
                    "the http tool needs an allow-list of hosts, without empty names".into(),
                ));
            }
        }
    }
    if !matches!(tool.route, ToolRoute::Api { .. }) && tool.operation.is_some() {
        return Err(invalid("only an API tool has an operation".into()));
    }
    if matches!(
        tool.route,
        ToolRoute::Mcp { .. } | ToolRoute::Api { .. } | ToolRoute::Host { .. }
    ) && matches!(name, "http" | "webhook")
    {
        return Err(misplaced("as the built-in, with the built-in route".into()));
    }

    let policy = &tool.policy;
    if policy.concurrency == 0 {
        return Err(invalid("its concurrency must be at least 1".into()));
    }
    let whole_millis = |duration: Duration| {
        !duration.is_zero() && duration.subsec_nanos().is_multiple_of(1_000_000)
    };
    if !whole_millis(policy.heartbeat_timeout)
        || policy
            .execution_timeout
            .is_some_and(|timeout| !whole_millis(timeout))
    {
        return Err(invalid(
            "its timeouts must be positive whole numbers of milliseconds".into(),
        ));
    }
    Ok(())
}

/// An operation's path and parameters: each path parameter is a `{name}` of the path and the
/// reverse, no two parameters share a name, and none is named `body`.
fn check_operation(operation: &ApiOperation) -> Result<(), String> {
    if !operation.path.starts_with('/') || operation.path.contains(['?', '#']) {
        return Err(format!(
            "its path {:?} must start with / and hold no query or fragment",
            operation.path
        ));
    }
    let mut names = BTreeSet::new();
    for parameter in &operation.parameters {
        if parameter.name.is_empty() || parameter.name == "body" {
            return Err(format!(
                "a parameter cannot be named {:?}: the request body is the arg \"body\"",
                parameter.name
            ));
        }
        if !names.insert(parameter.name.as_str()) {
            return Err(format!("two parameters are named {:?}", parameter.name));
        }
        if parameter.location == ParameterLocation::Header && !is_header_name(&parameter.name) {
            return Err(format!(
                "the header parameter {:?} is not the name of an HTTP header",
                parameter.name
            ));
        }
    }
    let mut templated = BTreeSet::new();
    let mut rest = operation.path.as_str();
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            return Err(format!("its path {:?} has an unclosed {{", operation.path));
        };
        templated.insert(&after[..end]);
        rest = &after[end + 1..];
    }
    let in_path: BTreeSet<&str> = operation
        .parameters
        .iter()
        .filter(|parameter| parameter.location == ParameterLocation::Path)
        .map(|parameter| parameter.name.as_str())
        .collect();
    if templated != in_path {
        return Err(format!(
            "the parameters of its path {:?} are not its path parameters",
            operation.path
        ));
    }
    Ok(())
}

/// The tool's local name if `name` is `{namespace}.{local}` with a non-empty local name.
fn in_namespace<'a>(name: &'a str, namespace: &str) -> Option<&'a str> {
    name.strip_prefix(namespace)?
        .strip_prefix('.')
        .filter(|local| !local.is_empty())
}

/// An ECMAScript identifier name in ASCII. What `rewind:tools` exports a namespace or a host tool
/// as must also not be a [reserved word](RESERVED_WORDS).
pub(crate) fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// The names no declaration of `rewind:tools` can take: an ES module is strict mode code, and its
/// exports are declarations (`export const github`, `export function ask_user`). They are the
/// reserved words of ECMAScript 2025, those of strict mode, and `eval` and `arguments`, which
/// strict mode does not let a declaration take. The contextual keywords (`async`, `of`, `type`…)
/// and the global names (`undefined`, `globalThis`…) are identifiers a module may declare.
pub const RESERVED_WORDS: &[&str] = &[
    // ReservedWord.
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
    // Reserved in strict mode code.
    "implements",
    "interface",
    "let",
    "package",
    "private",
    "protected",
    "public",
    "static",
    // Not the name of a declaration in strict mode code.
    "arguments",
    "eval",
];

pub(crate) fn is_reserved(name: &str) -> bool {
    RESERVED_WORDS.contains(&name)
}

// -------------------------------------------------------------------------------------------
// The document v1, as the digest sees it. `rewind-format` writes the same JSON, which its golden
// tests check.

fn server_json(server: &McpServer) -> Value {
    let transport = match &server.transport {
        McpTransport::Http { url } => json!({ "type": "http", "url": url }),
        McpTransport::Stdio { command, args, env } => {
            json!({ "type": "stdio", "command": command, "args": args, "env": env })
        }
    };
    let mut object =
        json!({ "name": server.name, "transport": transport, "concurrency": server.concurrency });
    if let Some(auth) = auth_json(&server.auth) {
        object["auth"] = auth;
    }
    object
}

fn api_json(api: &Api) -> Value {
    let mut object =
        json!({ "name": api.name, "base_url": api.base_url, "concurrency": api.concurrency });
    if let Some(auth) = auth_json(&api.auth) {
        object["auth"] = auth;
    }
    object
}

/// `None` for no auth: the document leaves it out.
fn auth_json(auth: &ToolAuth) -> Option<Value> {
    Some(match auth {
        ToolAuth::None => return None,
        ToolAuth::Rewind => json!({ "type": "rewind" }),
        ToolAuth::Secret(SecretPlacement::Header {
            name,
            format,
            encoding,
        }) => {
            let mut object = json!({ "type": "secret", "header": name });
            if let Some(format) = format {
                object["format"] = json!(format);
            }
            if *encoding != SecretEncoding::Plain {
                object["encoding"] = json!(encoding.as_str());
            }
            object
        }
        ToolAuth::Secret(SecretPlacement::Env { variable }) => {
            json!({ "type": "secret", "env": variable })
        }
    })
}

fn operation_json(operation: &ApiOperation) -> Value {
    let parameters: Vec<Value> = operation
        .parameters
        .iter()
        .map(|parameter| json!({ "name": parameter.name, "in": parameter.location.as_str() }))
        .collect();
    json!({
        "method": operation.method.as_str(),
        "path": operation.path,
        "parameters": parameters,
        "body": operation.body,
        "idempotency_key": operation.idempotency_key,
    })
}

fn tool_json(tool: &Tool) -> Value {
    let route = match &tool.route {
        ToolRoute::Mcp { server } => json!({ "type": "mcp", "server": server }),
        ToolRoute::Api { api } => json!({ "type": "api", "api": api }),
        ToolRoute::Host { group: None } => json!({ "type": "host" }),
        ToolRoute::Host { group: Some(group) } => json!({ "type": "host", "group": group }),
        ToolRoute::Builtin(Builtin::Http { allowed_hosts }) => {
            json!({ "type": "builtin", "builtin": "http", "allowed_hosts": allowed_hosts })
        }
        ToolRoute::Builtin(Builtin::Webhook) => json!({ "type": "builtin", "builtin": "webhook" }),
        // Never written: an action set declares no such tool (`check_tool`).
        ToolRoute::Builtin(Builtin::Workflow { workflow, op }) => json!({
            "type": "builtin", "builtin": "workflow", "workflow": workflow, "op": op.as_str(),
        }),
    };
    let mut schemas = Map::new();
    schemas.insert("input".into(), tool.input_schema.clone());
    if let Some(output) = &tool.output_schema {
        schemas.insert("output".into(), output.clone());
    }
    let mut policy = Map::new();
    let p = &tool.policy;
    policy.insert("on_interruption".into(), json!(p.on_interruption.as_str()));
    policy.insert("approval".into(), json!(p.approval));
    if let Some(timeout) = p.execution_timeout {
        policy.insert("execution_timeout_ms".into(), json!(millis(timeout)));
    }
    policy.insert(
        "heartbeat_timeout_ms".into(),
        json!(millis(p.heartbeat_timeout)),
    );
    policy.insert("concurrency".into(), json!(p.concurrency));

    let mut object = Map::new();
    object.insert("name".into(), json!(tool.name));
    object.insert("route".into(), route);
    if let Some(operation) = &tool.operation {
        object.insert("operation".into(), operation_json(operation));
    }
    if let Some(description) = &tool.description {
        object.insert("description".into(), json!(description));
    }
    object.insert("schemas".into(), Value::Object(schemas));
    object.insert("policy".into(), Value::Object(policy));
    Value::Object(object)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github() -> McpServer {
        McpServer::new(
            "github",
            McpTransport::Http {
                url: "https://mcp.example.com/github".into(),
            },
        )
    }

    fn mcp_tool(name: &str) -> Tool {
        Tool::new(
            name,
            ToolRoute::Mcp {
                server: "github".into(),
            },
            json!({ "type": "object" }),
        )
    }

    #[test]
    fn tools_are_sorted_and_found_by_name() {
        let action_set = ActionSet::new(
            [github()],
            [
                mcp_tool("github.list_issues"),
                mcp_tool("github.close_issue"),
            ],
        )
        .unwrap();
        let names: Vec<&str> = action_set.tools().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["github.close_issue", "github.list_issues"]);
        let tool = action_set.tool("github.list_issues").unwrap();
        assert_eq!(tool.namespace(), Some("github"));
        assert_eq!(tool.local_name(), "list_issues");
        assert!(action_set.contains("github.close_issue"));
        assert!(!action_set.contains("github"));
        assert_eq!(action_set.server("github"), Some(&github()));
    }

    #[test]
    fn the_digest_ignores_order_and_is_the_digest_of_the_document() {
        let a = ActionSet::new([github()], [mcp_tool("github.a"), mcp_tool("github.b")]).unwrap();
        let b = ActionSet::new([github()], [mcp_tool("github.b"), mcp_tool("github.a")]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.digest(), b.digest());
        assert_eq!(
            ActionSet::empty().digest(),
            Digest::of(br#"{"servers":[],"tools":[],"v":1}"#)
        );
        let host = ActionSet::from_host_tools(["ask_user"]).unwrap();
        assert_eq!(
            host.digest(),
            Digest::of(
                concat!(
                    r#"{"servers":[],"tools":[{"name":"ask_user","policy":{"approval":false,"concurrency":16,"#,
                    r#""heartbeat_timeout_ms":60000,"on_interruption":"retry"},"route":{"type":"host"},"#,
                    r#""schemas":{"input":{}}}],"v":1}"#
                )
                .as_bytes()
            )
        );
    }

    #[test]
    fn every_pinned_field_changes_the_digest() {
        let base = ActionSet::new([github()], [mcp_tool("github.a")]).unwrap();
        let with = |change: &dyn Fn(&mut Tool)| {
            let mut tool = mcp_tool("github.a");
            change(&mut tool);
            ActionSet::new([github()], [tool]).unwrap().digest()
        };
        let digests = [
            base.digest(),
            with(&|tool| tool.description = Some("Lists".into())),
            with(&|tool| tool.input_schema = json!({ "type": "string" })),
            with(&|tool| tool.output_schema = Some(json!({}))),
            with(&|tool| tool.policy.on_interruption = Interruption::AtMostOnce),
            with(&|tool| tool.policy.approval = true),
            with(&|tool| tool.policy.execution_timeout = None),
            with(&|tool| tool.policy.heartbeat_timeout = Duration::from_secs(5)),
            with(&|tool| tool.policy.concurrency = 1),
            {
                let mut server = github();
                server.concurrency = 2;
                ActionSet::new([server], [mcp_tool("github.a")])
                    .unwrap()
                    .digest()
            },
        ];
        let unique: BTreeSet<Digest> = digests.iter().copied().collect();
        assert_eq!(unique.len(), digests.len());
    }

    #[test]
    fn defaults_depend_on_the_route() {
        let mcp = ToolPolicy::default_for(&ToolRoute::Mcp {
            server: "github".into(),
        });
        assert_eq!(mcp.execution_timeout, Some(Duration::from_secs(300)));
        assert_eq!(mcp.heartbeat_timeout, Duration::from_secs(60));
        assert_eq!(mcp.concurrency, 16);
        assert_eq!(mcp.on_interruption, Interruption::Retry);
        assert!(!mcp.approval);
        let host = ToolPolicy::default_for(&ToolRoute::Host { group: None });
        assert_eq!(host.execution_timeout, None);
        assert_eq!(github().concurrency, 8);
    }

    #[test]
    fn host_tools_get_their_group_from_their_name() {
        let action_set =
            ActionSet::from_host_tools(["harness.ask_user", "notify", "notify"]).unwrap();
        assert_eq!(action_set.tools().len(), 2);
        let grouped = action_set.tool("harness.ask_user").unwrap();
        assert_eq!(
            grouped.route,
            ToolRoute::Host {
                group: Some("harness".into())
            }
        );
        assert_eq!(grouped.local_name(), "ask_user");
        assert_eq!(action_set.tool("notify").unwrap().namespace(), None);
        assert!(!ToolRoute::Host { group: None }.is_dispatched());
        assert!(ToolRoute::Builtin(Builtin::Webhook).is_dispatched());
    }

    #[test]
    fn builtins_are_named_as_the_builtin_and_http_needs_hosts() {
        let http = |hosts: &[&str]| {
            Tool::new(
                "http",
                ToolRoute::Builtin(Builtin::Http {
                    allowed_hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
                }),
                json!({}),
            )
        };
        let action_set =
            ActionSet::new([], [http(&["B.example", "a.example", "b.example"])]).unwrap();
        assert_eq!(
            action_set.tool("http").unwrap().route,
            ToolRoute::Builtin(Builtin::Http {
                allowed_hosts: vec!["a.example".into(), "b.example".into()]
            })
        );
        assert!(matches!(
            ActionSet::new([], [http(&[])]),
            Err(ActionSetError::InvalidTool { .. })
        ));
        let misnamed = Tool::new("hook", ToolRoute::Builtin(Builtin::Webhook), json!({}));
        assert!(matches!(
            ActionSet::new([], [misnamed]),
            Err(ActionSetError::MisplacedTool { .. })
        ));
        let impostor = Tool::new("webhook", ToolRoute::Host { group: None }, json!({}));
        assert!(matches!(
            ActionSet::new([], [impostor]),
            Err(ActionSetError::MisplacedTool { .. })
        ));
    }

    #[test]
    fn invariants_are_checked() {
        let err =
            |servers: Vec<McpServer>, tools: Vec<Tool>| ActionSet::new(servers, tools).unwrap_err();
        assert_eq!(
            err(vec![github(), github()], vec![]),
            ActionSetError::DuplicateServer("github".into())
        );
        assert_eq!(
            err(vec![], vec![mcp_tool("github.a")]),
            ActionSetError::UnknownServer {
                tool: "github.a".into(),
                server: "github".into()
            }
        );
        assert_eq!(
            err(
                vec![github()],
                vec![mcp_tool("github.a"), mcp_tool("github.a")]
            ),
            ActionSetError::DuplicateTool("github.a".into())
        );
        assert!(matches!(
            err(vec![github()], vec![mcp_tool("slack.post")]),
            ActionSetError::MisplacedTool { .. }
        ));
        assert!(matches!(
            err(vec![github()], vec![mcp_tool("github.")]),
            ActionSetError::MisplacedTool { .. }
        ));
        let mut bad_server = github();
        bad_server.name = "git-hub".into();
        assert_eq!(
            err(vec![bad_server], vec![]),
            ActionSetError::InvalidServerName("git-hub".into())
        );
        let mut idle = github();
        idle.concurrency = 0;
        assert!(matches!(
            err(vec![idle], vec![]),
            ActionSetError::InvalidServer { .. }
        ));
        let dotted = Tool::new("a.b", ToolRoute::Host { group: None }, json!({}));
        assert!(matches!(
            err(vec![], vec![dotted]),
            ActionSetError::MisplacedTool { .. }
        ));
        let mut blocked = mcp_tool("github.a");
        blocked.policy.concurrency = 0;
        assert!(matches!(
            err(vec![github()], vec![blocked]),
            ActionSetError::InvalidTool { .. }
        ));
        let mut fractional = mcp_tool("github.a");
        fractional.policy.heartbeat_timeout = Duration::from_micros(1500);
        assert!(matches!(
            err(vec![github()], vec![fractional]),
            ActionSetError::InvalidTool { .. }
        ));
        assert_eq!(
            err(
                vec![],
                vec![Tool::new("", ToolRoute::Host { group: None }, json!({}))]
            ),
            ActionSetError::InvalidToolName(String::new())
        );
    }

    fn server(name: &str) -> McpServer {
        McpServer::new(
            name,
            McpTransport::Http {
                url: format!("https://mcp.example.com/{name}"),
            },
        )
    }

    /// An action set where `name` becomes an identifier of `rewind:tools`: as a server, as a host-tool
    /// group, and as a host tool without a group.
    fn as_identifier(name: &str) -> [Result<ActionSet, ActionSetError>; 3] {
        let tool = Tool::new(
            format!("{name}.find"),
            ToolRoute::Mcp {
                server: name.to_owned(),
            },
            json!({}),
        );
        let grouped = Tool::new(
            format!("{name}.ask"),
            ToolRoute::Host {
                group: Some(name.to_owned()),
            },
            json!({}),
        );
        let bare = Tool::new(name, ToolRoute::Host { group: None }, json!({}));
        [
            ActionSet::new([server(name)], [tool]),
            ActionSet::new([], [grouped]),
            ActionSet::new([], [bare]),
        ]
    }

    #[test]
    fn reserved_words_are_refused_where_a_name_becomes_an_identifier() {
        for word in RESERVED_WORDS {
            let [server, group, bare] = as_identifier(word);
            assert_eq!(
                server.unwrap_err(),
                ActionSetError::InvalidServerName((*word).to_owned()),
                "{word}"
            );
            let group = group.unwrap_err();
            assert!(
                matches!(&group, ActionSetError::InvalidTool { message, .. } if message.contains("reserved word")),
                "{word}: {group}"
            );
            let bare = bare.unwrap_err();
            assert!(
                matches!(&bare, ActionSetError::InvalidTool { message, .. } if message.contains("reserved word")),
                "{word}: {bare}"
            );
        }
        // The keywords of the language, the ones of strict mode, and the names strict mode does
        // not let a declaration take.
        for word in [
            "await",
            "break",
            "class",
            "default",
            "delete",
            "enum",
            "export",
            "import",
            "new",
            "null",
            "this",
            "true",
            "typeof",
            "void",
            "yield",
            "let",
            "static",
            "implements",
            "interface",
            "package",
            "private",
            "protected",
            "public",
            "eval",
            "arguments",
        ] {
            assert!(RESERVED_WORDS.contains(&word), "{word}");
        }
        assert_eq!(
            ActionSetError::InvalidServerName("delete".into()).to_string(),
            "invalid server name \"delete\": expected an identifier (letters, digits, _ and $) \
             that is not a reserved word of JavaScript"
        );
    }

    #[test]
    fn contextual_keywords_and_other_names_are_identifiers() {
        for name in [
            "async",
            "of",
            "get",
            "set",
            "from",
            "as",
            "type",
            "namespace",
            "module",
            "declare",
            "string",
            "number",
            "any",
            "never",
            "unknown",
            "undefined",
            "globalThis",
            "NaN",
            "constructor",
            "prototype",
            "Delete",
            "DEFAULT",
            "delete_",
            "_class",
            "$new",
            "github",
        ] {
            for action_set in as_identifier(name) {
                assert!(action_set.is_ok(), "{name}: {action_set:?}");
            }
        }
        // A host tool whose name is no identifier at all stays out of `rewind:tools`, as before.
        let dashed = Tool::new("ask-user", ToolRoute::Host { group: None }, json!({}));
        assert!(ActionSet::new([], [dashed]).is_ok());
    }

    fn pinned(name: &str) -> PinnedWorkflow {
        crate::Workflow::new(
            name,
            None,
            Digest::of(b"script"),
            Digest::of(b"action_set"),
            crate::WorkflowSchemas {
                input: json!({ "type": "object" }),
                output: json!({}),
                signals: BTreeMap::from([("approval".to_owned(), json!({}))]),
            },
        )
        .unwrap()
        .pinned()
    }

    #[test]
    fn pinned_workflows_give_their_builtins_outside_the_document() {
        let base = ActionSet::new([github()], [mcp_tool("github.a")]).unwrap();
        let action_set = base
            .clone()
            .with_workflows([pinned("triage"), pinned("digest")])
            .unwrap();
        let names: Vec<&str> = action_set
            .workflows()
            .iter()
            .map(|w| w.name.as_str())
            .collect();
        assert_eq!(names, ["digest", "triage"]);
        assert_eq!(action_set.tools(), base.tools());
        let start = action_set.tool("workflows.triage.start").unwrap();
        assert_eq!(
            start.route,
            ToolRoute::Builtin(Builtin::Workflow {
                workflow: "triage".into(),
                op: WorkflowOp::Start,
            })
        );
        assert_eq!(start.policy, ToolPolicy::default_for(&start.route));
        assert!(action_set.contains("workflows.digest.signal"));
        assert!(!action_set.contains("workflows.triage"));
        assert_eq!(action_set.workflow("triage"), Some(&pinned("triage")));
        assert_ne!(action_set.digest(), base.digest());
        // Without workflows, the document has no `workflows`: older digests hold.
        assert_eq!(
            base.clone().with_workflows([]).unwrap().digest(),
            base.digest()
        );
        assert!(action_set.to_string().ends_with("(1 tools, 2 workflows)"));
    }

    #[test]
    fn workflows_are_checked_and_their_namespace_is_reserved() {
        assert_eq!(
            ActionSet::empty().with_workflows([pinned("triage"), pinned("triage")]),
            Err(ActionSetError::DuplicateWorkflow("triage".into()))
        );
        let mut invalid = pinned("triage");
        invalid.name = "default".into();
        assert!(matches!(
            ActionSet::empty().with_workflows([invalid]),
            Err(ActionSetError::InvalidWorkflow(_))
        ));
        assert!(matches!(
            ActionSet::new([server("workflows")], []),
            Err(ActionSetError::InvalidServer { .. })
        ));
        let grouped = Tool::new(
            "workflows.ask",
            ToolRoute::Host {
                group: Some("workflows".into()),
            },
            json!({}),
        );
        assert!(matches!(
            ActionSet::new([], [grouped]),
            Err(ActionSetError::InvalidTool { .. })
        ));
        let declared = Tool::new(
            "workflows.triage.start",
            ToolRoute::Builtin(Builtin::Workflow {
                workflow: "triage".into(),
                op: WorkflowOp::Start,
            }),
            json!({}),
        );
        assert!(matches!(
            ActionSet::new([], [declared]),
            Err(ActionSetError::InvalidTool { .. })
        ));
    }

    fn dax() -> Api {
        Api::new("dax", "https://api.example.com/v1")
    }

    fn create_measure() -> ApiOperation {
        ApiOperation {
            method: HttpMethod::Post,
            path: "/measures/{project}".into(),
            parameters: vec![
                ApiParameter {
                    name: "project".into(),
                    location: ParameterLocation::Path,
                },
                ApiParameter {
                    name: "dryRun".into(),
                    location: ParameterLocation::Query,
                },
            ],
            body: true,
            idempotency_key: false,
        }
    }

    fn api_tool() -> Tool {
        Tool::api(
            "dax.createMeasure",
            "dax",
            create_measure(),
            json!({ "type": "object" }),
        )
    }

    #[test]
    fn api_tools_have_an_operation_and_a_policy_from_their_method() {
        let action_set = ActionSet::from_parts([github()], [dax()], [api_tool()]).unwrap();
        let tool = action_set.tool("dax.createMeasure").unwrap();
        assert_eq!(tool.namespace(), Some("dax"));
        assert_eq!(tool.local_name(), "createMeasure");
        assert_eq!(tool.route.server(), Some("dax"));
        assert!(tool.route.is_dispatched());
        assert_eq!(tool.policy.on_interruption, Interruption::AtMostOnce);
        assert_eq!(
            tool.policy.execution_timeout,
            Some(ToolPolicy::DEFAULT_EXECUTION_TIMEOUT)
        );
        let names: Vec<&str> = tool
            .operation
            .as_ref()
            .unwrap()
            .parameters
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["dryRun", "project"], "parameters are sorted");
        let keyed = ApiOperation {
            idempotency_key: true,
            ..create_measure()
        };
        assert_eq!(
            ToolPolicy::default_for_operation(&keyed).on_interruption,
            Interruption::Retry
        );
        let get = ApiOperation {
            method: HttpMethod::Get,
            ..create_measure()
        };
        assert_eq!(
            ToolPolicy::default_for_operation(&get).on_interruption,
            Interruption::Retry
        );
        assert_eq!(action_set.api("dax"), Some(&dax()));
        assert_eq!(action_set.auth("dax"), Some(&ToolAuth::None));
        assert_eq!(action_set.auth("github"), Some(&ToolAuth::None));
        assert_eq!(action_set.auth("slack"), None);
    }

    #[test]
    fn api_tools_are_checked() {
        let err = |apis: Vec<Api>, tools: Vec<Tool>| {
            ActionSet::from_parts([github()], apis, tools).unwrap_err()
        };
        assert_eq!(
            err(vec![], vec![api_tool()]),
            ActionSetError::UnknownApi {
                tool: "dax.createMeasure".into(),
                api: "dax".into()
            }
        );
        assert_eq!(
            err(vec![dax(), dax()], vec![]),
            ActionSetError::DuplicateApi("dax".into())
        );
        assert_eq!(
            err(vec![Api::new("github", "https://api.github.com")], vec![]),
            ActionSetError::DuplicateApi("github".into()),
            "an API and a server share the namespaces of rewind:tools"
        );
        assert_eq!(
            err(vec![Api::new("delete", "https://x.example")], vec![]),
            ActionSetError::InvalidApiName("delete".into())
        );
        for base in [
            "ftp://x.example",
            "https://",
            "https://x.example/?a=1",
            "x.example",
        ] {
            assert!(
                matches!(
                    err(vec![Api::new("dax", base)], vec![]),
                    ActionSetError::InvalidApi { .. }
                ),
                "{base}"
            );
        }
        let mut no_operation = api_tool();
        no_operation.operation = None;
        assert!(matches!(
            err(vec![dax()], vec![no_operation]),
            ActionSetError::InvalidTool { .. }
        ));
        let mut stray = mcp_tool("github.a");
        stray.operation = Some(create_measure());
        assert!(matches!(
            err(vec![], vec![stray]),
            ActionSetError::InvalidTool { .. }
        ));
        let broken = |change: &dyn Fn(&mut ApiOperation)| {
            let mut tool = api_tool();
            change(tool.operation.as_mut().unwrap());
            err(vec![dax()], vec![tool])
        };
        for error in [
            broken(&|op| op.path = "measures".into()),
            broken(&|op| op.path = "/measures/{project}/{id}".into()),
            broken(&|op| op.path = "/measures".into()),
            broken(&|op| op.parameters[1].name = "body".into()),
            broken(&|op| op.parameters[1].name = "project".into()),
        ] {
            assert!(
                matches!(error, ActionSetError::InvalidTool { .. }),
                "{error}"
            );
        }
    }

    #[test]
    fn auth_fits_the_transport() {
        let stdio = |auth: ToolAuth| {
            let mut server = McpServer::new(
                "local",
                McpTransport::Stdio {
                    command: "mcp-local".into(),
                    args: vec![],
                    env: BTreeMap::from([("MODE".to_owned(), "fast".to_owned())]),
                },
            );
            server.auth = auth;
            ActionSet::new([server], [])
        };
        let http = |auth: ToolAuth| {
            let mut server = github();
            server.auth = auth;
            ActionSet::new([server], [])
        };
        let env = |variable: &str| {
            ToolAuth::Secret(SecretPlacement::Env {
                variable: variable.into(),
            })
        };
        let header = |name: &str, format: Option<&str>| {
            ToolAuth::Secret(SecretPlacement::Header {
                name: name.into(),
                format: format.map(str::to_owned),
                encoding: SecretEncoding::Plain,
            })
        };
        assert!(stdio(env("TOKEN")).is_ok());
        assert!(stdio(ToolAuth::None).is_ok());
        assert!(http(ToolAuth::Rewind).is_ok());
        assert!(http(ToolAuth::Secret(SecretPlacement::bearer())).is_ok());
        assert!(http(header("X-Api-Key", None)).is_ok());
        for refused in [
            stdio(ToolAuth::Rewind),
            stdio(header("X-Api-Key", None)),
            stdio(env("MODE")),
            stdio(env("1TOKEN")),
            http(env("TOKEN")),
            http(header("X Api", None)),
            http(header("Idempotency-Key", None)),
            http(header("traceparent", None)),
            http(header("Authorization", Some("Bearer"))),
        ] {
            assert!(
                matches!(refused, Err(ActionSetError::InvalidServer { .. })),
                "{refused:?}"
            );
        }
        let mut api = dax();
        api.auth = env("TOKEN");
        assert!(matches!(
            ActionSet::from_parts([], [api], []),
            Err(ActionSetError::InvalidApi { .. })
        ));
    }

    #[test]
    fn the_new_fields_are_in_the_document_only_when_used() {
        let base = ActionSet::new([github()], [mcp_tool("github.a")]).unwrap();
        assert_eq!(
            ActionSet::from_parts([github()], [], [mcp_tool("github.a")])
                .unwrap()
                .digest(),
            base.digest(),
            "no APIs, no auth: the document of before"
        );
        let mut signed = github();
        signed.auth = ToolAuth::Rewind;
        let mut keyed = github();
        keyed.auth = ToolAuth::Secret(SecretPlacement::bearer());
        let mut basic = github();
        basic.auth = ToolAuth::Secret(SecretPlacement::basic());
        let mut signed_api = dax();
        signed_api.auth = ToolAuth::Rewind;
        let digests = [
            base.digest(),
            ActionSet::new([signed], [mcp_tool("github.a")])
                .unwrap()
                .digest(),
            ActionSet::new([keyed], [mcp_tool("github.a")])
                .unwrap()
                .digest(),
            ActionSet::new([basic], [mcp_tool("github.a")])
                .unwrap()
                .digest(),
            ActionSet::from_parts([github()], [dax()], [mcp_tool("github.a")])
                .unwrap()
                .digest(),
            ActionSet::from_parts([github()], [signed_api], [mcp_tool("github.a")])
                .unwrap()
                .digest(),
            ActionSet::from_parts([github()], [dax()], [mcp_tool("github.a"), api_tool()])
                .unwrap()
                .digest(),
        ];
        let unique: BTreeSet<Digest> = digests.iter().copied().collect();
        assert_eq!(unique.len(), digests.len());

        let action_set = ActionSet::from_parts([github()], [dax()], [api_tool()]).unwrap();
        assert_eq!(
            canonical_json(&action_set.document()),
            concat!(
                r#"{"apis":[{"base_url":"https://api.example.com/v1","concurrency":8,"name":"dax"}],"#,
                r#""servers":[{"concurrency":8,"name":"github","transport":{"type":"http","#,
                r#""url":"https://mcp.example.com/github"}}],"tools":[{"name":"dax.createMeasure","#,
                r#""operation":{"body":true,"idempotency_key":false,"method":"post","parameters":"#,
                r#"[{"in":"query","name":"dryRun"},{"in":"path","name":"project"}],"#,
                r#""path":"/measures/{project}"},"policy":{"approval":false,"concurrency":16,"#,
                r#""execution_timeout_ms":300000,"heartbeat_timeout_ms":60000,"#,
                r#""on_interruption":"at_most_once"},"route":{"api":"dax","type":"api"},"#,
                r#""schemas":{"input":{"type":"object"}}}],"v":1}"#
            )
        );
        let mut basic = github();
        basic.auth = ToolAuth::Secret(SecretPlacement::basic());
        let document = ActionSet::new([basic], []).unwrap().document();
        assert_eq!(
            canonical_json(&document["servers"][0]["auth"]),
            r#"{"encoding":"base64","format":"Basic {secret}","header":"Authorization","type":"secret"}"#
        );
    }

    #[test]
    fn the_servers_taking_secrets_are_listed() {
        let mut keyed = github();
        keyed.auth = ToolAuth::Secret(SecretPlacement::bearer());
        let mut signed = server("crm");
        signed.auth = ToolAuth::Rewind;
        let mut api = Api::new("zendesk", "https://zendesk.example.com");
        api.auth = ToolAuth::Secret(SecretPlacement::header("X-Api-Key"));
        let action_set = ActionSet::from_parts([keyed, signed], [dax(), api], []).unwrap();
        assert_eq!(
            action_set.taking_secrets().collect::<Vec<_>>(),
            ["github", "zendesk"]
        );
    }
}
