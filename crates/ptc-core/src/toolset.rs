//! The pinned toolset (`archi/04-tools.md`, "Toolsets and code generation").
//!
//! Registering a toolset connects to its MCP servers, lists their tools and **pins** them: the
//! servers, and for each tool its name, route, description, schemas and policy. The pinned
//! toolset is immutable and content-addressed: its digest is the SHA-256 of its canonical JSON,
//! the pinned toolset document of the stored format v1 (`archi/02-journal.md`), and every run
//! records the digest it was started with.
//!
//! ```text
//! Toolset
//! ├── servers: McpServer { name, transport: http(url) | stdio(command, args, env), concurrency }
//! └── tools:   Tool { name, route, description?, input_schema, output_schema?, policy }
//!                     route  = mcp(server) | host(group?) | builtin(http(allowed_hosts) | webhook)
//!                     policy = { on_interruption: retry | at_most_once, approval,
//!                                execution_timeout?, heartbeat_timeout, concurrency }
//! ```
//!
//! A tool is named as its server defines it, behind its namespace: `github.list_issues` for the
//! tool `list_issues` of the MCP server `github`, `harness.ask_user` or `ask_user` for a host
//! tool, `http` and `webhook` for the built-ins. The namespace is what `ptc:tools` exports.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::canonical::canonical_json;
use crate::digest::Digest;

/// The tools a run may call, pinned when the toolset was registered.
///
/// Built with [`Toolset::new`], which checks every invariant: unique names, MCP tools of known
/// servers and inside their namespace, valid policies. Servers and tools are kept sorted by
/// name, so the digest does not depend on the order they were given in.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Toolset {
    servers: Vec<McpServer>,
    tools: Vec<Tool>,
}

/// An MCP server of a toolset, as it was pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServer {
    /// The namespace of its tools in `ptc:tools`: an identifier, such as `github`.
    pub name: String,
    pub transport: McpTransport,
    /// Calls in flight to this server, on each worker. The claim of tool tasks enforces it.
    pub concurrency: u32,
}

impl McpServer {
    /// Calls in flight to one MCP server on each worker, unless the toolset says otherwise.
    pub const DEFAULT_CONCURRENCY: u32 = 8;

    /// A server with the default concurrency.
    pub fn new(name: impl Into<String>, transport: McpTransport) -> Self {
        Self {
            name: name.into(),
            transport,
            concurrency: Self::DEFAULT_CONCURRENCY,
        }
    }
}

/// How the dispatcher reaches an MCP server. Credentials are not part of it: they are resolved
/// at call time from the `SecretStore` and injected into the transport
/// (`archi/04-tools.md`, "Credentials").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransport {
    /// Streamable HTTP.
    Http { url: String },
    /// A process started by the dispatcher, speaking MCP on its standard input and output.
    /// `env` holds configuration, never secrets.
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
            description: None,
            input_schema,
            output_schema: None,
            policy,
        }
    }

    /// The namespace `ptc:tools` exports the tool under: its MCP server or its host-tool group.
    /// `None` for a bare host tool and for the built-ins.
    pub fn namespace(&self) -> Option<&str> {
        match &self.route {
            ToolRoute::Mcp { server } => Some(server),
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
    /// A tool of one of the toolset's MCP servers, called by the dispatcher of ai-ptc.
    Mcp { server: String },
    /// A built-in, called by the dispatcher of ai-ptc.
    Builtin(Builtin),
    /// A function of the harness, executed by its SDK worker, which claims the task through
    /// `GET /v1/tasks`. A grouped host tool is named `{group}.{tool}`.
    Host { group: Option<String> },
}

impl ToolRoute {
    /// Whether ai-ptc calls the tool itself: MCP tools and built-ins. Host tools are claimed by
    /// the harness's worker instead.
    pub fn is_dispatched(&self) -> bool {
        !matches!(self, ToolRoute::Host { .. })
    }
}

/// The built-in tools. A toolset enables one by listing it, named as the built-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Builtin {
    /// `http`: the only way a script reaches the network, through `fetch`. Requests leave from
    /// the dispatcher, only toward these hosts (`archi/04-tools.md`, "The http tool and
    /// egress"). Hosts are lowercase, sorted and unique, and there is at least one.
    Http { allowed_hosts: Vec<String> },
    /// `webhook`: `webhook(name)` returns a URL bound to the run and the signal name.
    Webhook,
}

impl Builtin {
    /// The name of the tool, as scripts call it.
    pub fn name(&self) -> &'static str {
        match self {
            Builtin::Http { .. } => "http",
            Builtin::Webhook => "webhook",
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

/// The policy of one tool, pinned in the toolset and set per tool by the harness when it
/// registers the toolset (`archi/04-tools.md`, "Policies").
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
    /// Calls of one tool in flight in one run, unless the toolset says otherwise.
    pub const DEFAULT_CONCURRENCY: u32 = 16;
    /// The execution timeout of the tools ai-ptc calls itself (MCP tools and built-ins).
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
                ToolRoute::Mcp { .. } | ToolRoute::Builtin(_) => {
                    Some(Self::DEFAULT_EXECUTION_TIMEOUT)
                }
            },
            heartbeat_timeout: Self::DEFAULT_HEARTBEAT_TIMEOUT,
            concurrency: Self::DEFAULT_CONCURRENCY,
        }
    }
}

/// A toolset that breaks an invariant. The messages are written for the harness developer who
/// registers the toolset.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolsetError {
    #[error(
        "invalid server name {0:?}: expected an identifier (letters, digits, _ and $) that is \
         not a reserved word of JavaScript"
    )]
    InvalidServerName(String),
    #[error("server {0:?} is declared twice")]
    DuplicateServer(String),
    #[error("server {server:?}: {message}")]
    InvalidServer { server: String, message: String },
    #[error("invalid tool name {0:?}")]
    InvalidToolName(String),
    #[error("tool {0:?} is declared twice")]
    DuplicateTool(String),
    #[error("tool {tool:?} belongs to server {server:?}, which the toolset does not declare")]
    UnknownServer { tool: String, server: String },
    #[error("tool {tool:?} must be named {expected}")]
    MisplacedTool { tool: String, expected: String },
    #[error("tool {tool:?}: {message}")]
    InvalidTool { tool: String, message: String },
}

impl Toolset {
    /// A toolset of these servers and tools, sorted by name. The allowed hosts of `http` are
    /// lowercased, sorted and deduplicated.
    pub fn new(
        servers: impl IntoIterator<Item = McpServer>,
        tools: impl IntoIterator<Item = Tool>,
    ) -> Result<Self, ToolsetError> {
        let mut servers: Vec<McpServer> = servers.into_iter().collect();
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        for pair in servers.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(ToolsetError::DuplicateServer(pair[0].name.clone()));
            }
        }
        for server in &servers {
            check_server(server)?;
        }

        let mut tools: Vec<Tool> = tools.into_iter().collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        for pair in tools.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(ToolsetError::DuplicateTool(pair[0].name.clone()));
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
            check_tool(tool, &servers)?;
        }
        Ok(Self { servers, tools })
    }

    /// A toolset with no tool.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Host tools with these names, the default policy, and an input schema that accepts any
    /// value: what mock mode and tests use when the toolset does not matter. A name
    /// `group.tool` puts the tool in the host-tool group `group`. Duplicates are ignored.
    pub fn from_host_tools<S: Into<String>>(
        names: impl IntoIterator<Item = S>,
    ) -> Result<Self, ToolsetError> {
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

    /// The tools, sorted by name.
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    pub fn tool(&self, name: &str) -> Option<&Tool> {
        self.tools
            .binary_search_by(|tool| tool.name.as_str().cmp(name))
            .ok()
            .map(|index| &self.tools[index])
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

    /// SHA-256 of the toolset's canonical JSON (RFC 8785), which is the pinned toolset document
    /// of the stored format v1, `"v": 1` included. The document `ptc-format` writes for a
    /// toolset hashes to this digest.
    pub fn digest(&self) -> Digest {
        Digest::of(canonical_json(&self.document()).as_bytes())
    }

    /// The pinned toolset document v1, as a JSON value.
    fn document(&self) -> Value {
        let servers: Vec<Value> = self.servers.iter().map(server_json).collect();
        let tools: Vec<Value> = self.tools.iter().map(tool_json).collect();
        json!({ "v": 1, "servers": servers, "tools": tools })
    }
}

impl fmt::Display for Toolset {
    /// `toolset 9f2c4e1a07b3 (3 tools)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "toolset {} ({} tools)",
            self.digest().short(),
            self.tools.len()
        )
    }
}

// -------------------------------------------------------------------------------------------
// Invariants

fn check_server(server: &McpServer) -> Result<(), ToolsetError> {
    let invalid = |message: &str| ToolsetError::InvalidServer {
        server: server.name.clone(),
        message: message.to_owned(),
    };
    if !is_identifier(&server.name) || is_reserved(&server.name) {
        return Err(ToolsetError::InvalidServerName(server.name.clone()));
    }
    if server.concurrency == 0 {
        return Err(invalid("its concurrency must be at least 1"));
    }
    match &server.transport {
        McpTransport::Http { url } if url.is_empty() => Err(invalid("its URL is empty")),
        McpTransport::Stdio { command, .. } if command.is_empty() => {
            Err(invalid("its command is empty"))
        }
        _ => Ok(()),
    }
}

fn check_tool(tool: &Tool, servers: &[McpServer]) -> Result<(), ToolsetError> {
    let name = tool.name.as_str();
    let invalid = |message: String| ToolsetError::InvalidTool {
        tool: name.to_owned(),
        message,
    };
    let misplaced = |expected: String| ToolsetError::MisplacedTool {
        tool: name.to_owned(),
        expected,
    };
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(ToolsetError::InvalidToolName(name.to_owned()));
    }
    match &tool.route {
        ToolRoute::Mcp { server } => {
            if servers
                .binary_search_by(|known| known.name.as_str().cmp(server))
                .is_err()
            {
                return Err(ToolsetError::UnknownServer {
                    tool: name.to_owned(),
                    server: server.clone(),
                });
            }
            in_namespace(name, server).ok_or_else(|| misplaced(format!("{server}.<tool>")))?;
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
    if matches!(tool.route, ToolRoute::Mcp { .. } | ToolRoute::Host { .. })
        && matches!(name, "http" | "webhook")
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

/// The tool's local name if `name` is `{namespace}.{local}` with a non-empty local name.
fn in_namespace<'a>(name: &'a str, namespace: &str) -> Option<&'a str> {
    name.strip_prefix(namespace)?
        .strip_prefix('.')
        .filter(|local| !local.is_empty())
}

/// An ECMAScript identifier name in ASCII. What `ptc:tools` exports a namespace or a host tool
/// as must also not be a [reserved word](RESERVED_WORDS).
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// The names no declaration of `ptc:tools` can take: an ES module is strict mode code, and its
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

fn is_reserved(name: &str) -> bool {
    RESERVED_WORDS.contains(&name)
}

// -------------------------------------------------------------------------------------------
// The document v1, as the digest sees it. `ptc-format` writes the same JSON, which its golden
// tests check.

fn server_json(server: &McpServer) -> Value {
    let transport = match &server.transport {
        McpTransport::Http { url } => json!({ "type": "http", "url": url }),
        McpTransport::Stdio { command, args, env } => {
            json!({ "type": "stdio", "command": command, "args": args, "env": env })
        }
    };
    json!({ "name": server.name, "transport": transport, "concurrency": server.concurrency })
}

fn tool_json(tool: &Tool) -> Value {
    let route = match &tool.route {
        ToolRoute::Mcp { server } => json!({ "type": "mcp", "server": server }),
        ToolRoute::Host { group: None } => json!({ "type": "host" }),
        ToolRoute::Host { group: Some(group) } => json!({ "type": "host", "group": group }),
        ToolRoute::Builtin(Builtin::Http { allowed_hosts }) => {
            json!({ "type": "builtin", "builtin": "http", "allowed_hosts": allowed_hosts })
        }
        ToolRoute::Builtin(Builtin::Webhook) => json!({ "type": "builtin", "builtin": "webhook" }),
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
        let toolset = Toolset::new(
            [github()],
            [
                mcp_tool("github.list_issues"),
                mcp_tool("github.close_issue"),
            ],
        )
        .unwrap();
        let names: Vec<&str> = toolset.tools().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["github.close_issue", "github.list_issues"]);
        let tool = toolset.tool("github.list_issues").unwrap();
        assert_eq!(tool.namespace(), Some("github"));
        assert_eq!(tool.local_name(), "list_issues");
        assert!(toolset.contains("github.close_issue"));
        assert!(!toolset.contains("github"));
        assert_eq!(toolset.server("github"), Some(&github()));
    }

    #[test]
    fn the_digest_ignores_order_and_is_the_digest_of_the_document() {
        let a = Toolset::new([github()], [mcp_tool("github.a"), mcp_tool("github.b")]).unwrap();
        let b = Toolset::new([github()], [mcp_tool("github.b"), mcp_tool("github.a")]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.digest(), b.digest());
        assert_eq!(
            Toolset::empty().digest(),
            Digest::of(br#"{"servers":[],"tools":[],"v":1}"#)
        );
        let host = Toolset::from_host_tools(["ask_user"]).unwrap();
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
        let base = Toolset::new([github()], [mcp_tool("github.a")]).unwrap();
        let with = |change: &dyn Fn(&mut Tool)| {
            let mut tool = mcp_tool("github.a");
            change(&mut tool);
            Toolset::new([github()], [tool]).unwrap().digest()
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
                Toolset::new([server], [mcp_tool("github.a")])
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
        let toolset = Toolset::from_host_tools(["harness.ask_user", "notify", "notify"]).unwrap();
        assert_eq!(toolset.tools().len(), 2);
        let grouped = toolset.tool("harness.ask_user").unwrap();
        assert_eq!(
            grouped.route,
            ToolRoute::Host {
                group: Some("harness".into())
            }
        );
        assert_eq!(grouped.local_name(), "ask_user");
        assert_eq!(toolset.tool("notify").unwrap().namespace(), None);
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
        let toolset = Toolset::new([], [http(&["B.example", "a.example", "b.example"])]).unwrap();
        assert_eq!(
            toolset.tool("http").unwrap().route,
            ToolRoute::Builtin(Builtin::Http {
                allowed_hosts: vec!["a.example".into(), "b.example".into()]
            })
        );
        assert!(matches!(
            Toolset::new([], [http(&[])]),
            Err(ToolsetError::InvalidTool { .. })
        ));
        let misnamed = Tool::new("hook", ToolRoute::Builtin(Builtin::Webhook), json!({}));
        assert!(matches!(
            Toolset::new([], [misnamed]),
            Err(ToolsetError::MisplacedTool { .. })
        ));
        let impostor = Tool::new("webhook", ToolRoute::Host { group: None }, json!({}));
        assert!(matches!(
            Toolset::new([], [impostor]),
            Err(ToolsetError::MisplacedTool { .. })
        ));
    }

    #[test]
    fn invariants_are_checked() {
        let err =
            |servers: Vec<McpServer>, tools: Vec<Tool>| Toolset::new(servers, tools).unwrap_err();
        assert_eq!(
            err(vec![github(), github()], vec![]),
            ToolsetError::DuplicateServer("github".into())
        );
        assert_eq!(
            err(vec![], vec![mcp_tool("github.a")]),
            ToolsetError::UnknownServer {
                tool: "github.a".into(),
                server: "github".into()
            }
        );
        assert_eq!(
            err(
                vec![github()],
                vec![mcp_tool("github.a"), mcp_tool("github.a")]
            ),
            ToolsetError::DuplicateTool("github.a".into())
        );
        assert!(matches!(
            err(vec![github()], vec![mcp_tool("slack.post")]),
            ToolsetError::MisplacedTool { .. }
        ));
        assert!(matches!(
            err(vec![github()], vec![mcp_tool("github.")]),
            ToolsetError::MisplacedTool { .. }
        ));
        let mut bad_server = github();
        bad_server.name = "git-hub".into();
        assert_eq!(
            err(vec![bad_server], vec![]),
            ToolsetError::InvalidServerName("git-hub".into())
        );
        let mut idle = github();
        idle.concurrency = 0;
        assert!(matches!(
            err(vec![idle], vec![]),
            ToolsetError::InvalidServer { .. }
        ));
        let dotted = Tool::new("a.b", ToolRoute::Host { group: None }, json!({}));
        assert!(matches!(
            err(vec![], vec![dotted]),
            ToolsetError::MisplacedTool { .. }
        ));
        let mut blocked = mcp_tool("github.a");
        blocked.policy.concurrency = 0;
        assert!(matches!(
            err(vec![github()], vec![blocked]),
            ToolsetError::InvalidTool { .. }
        ));
        let mut fractional = mcp_tool("github.a");
        fractional.policy.heartbeat_timeout = Duration::from_micros(1500);
        assert!(matches!(
            err(vec![github()], vec![fractional]),
            ToolsetError::InvalidTool { .. }
        ));
        assert_eq!(
            err(
                vec![],
                vec![Tool::new("", ToolRoute::Host { group: None }, json!({}))]
            ),
            ToolsetError::InvalidToolName(String::new())
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

    /// A toolset where `name` becomes an identifier of `ptc:tools`: as a server, as a host-tool
    /// group, and as a host tool without a group.
    fn as_identifier(name: &str) -> [Result<Toolset, ToolsetError>; 3] {
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
            Toolset::new([server(name)], [tool]),
            Toolset::new([], [grouped]),
            Toolset::new([], [bare]),
        ]
    }

    #[test]
    fn reserved_words_are_refused_where_a_name_becomes_an_identifier() {
        for word in RESERVED_WORDS {
            let [server, group, bare] = as_identifier(word);
            assert_eq!(
                server.unwrap_err(),
                ToolsetError::InvalidServerName((*word).to_owned()),
                "{word}"
            );
            let group = group.unwrap_err();
            assert!(
                matches!(&group, ToolsetError::InvalidTool { message, .. } if message.contains("reserved word")),
                "{word}: {group}"
            );
            let bare = bare.unwrap_err();
            assert!(
                matches!(&bare, ToolsetError::InvalidTool { message, .. } if message.contains("reserved word")),
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
            ToolsetError::InvalidServerName("delete".into()).to_string(),
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
            for toolset in as_identifier(name) {
                assert!(toolset.is_ok(), "{name}: {toolset:?}");
            }
        }
        // A host tool whose name is no identifier at all stays out of `ptc:tools`, as before.
        let dashed = Tool::new("ask-user", ToolRoute::Host { group: None }, json!({}));
        assert!(Toolset::new([], [dashed]).is_ok());
    }
}
