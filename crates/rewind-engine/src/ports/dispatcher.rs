use std::collections::BTreeMap;
use std::future::Future;

use rewind_core::{Api, JsonValue, McpServer, RunId, SecretPlacement, Seq, Tool, ToolFailure};

use crate::ports::journal::{ParkReason, TaskToken, TraceContext};
use crate::ports::namespaces::NamespaceName;
use crate::ports::secrets::{Secret, SigningKey};
use crate::ports::tenancy::{RunRef, TenantId};

/// One attempt at a tool call, as handed to a [`ToolDispatcher`].
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub run: RunRef,
    /// The run's namespace: the end user it acts for.
    pub namespace: NamespaceName,
    /// The run's labels.
    pub labels: BTreeMap<String, String>,
    pub seq: Seq,
    /// The tool as pinned in the run's toolset: route, operation, schemas and policy. The
    /// dispatcher checks an MCP call against the server's current schema, and enforces the
    /// execution timeout.
    pub tool: Tool,
    /// For an MCP tool, its server as pinned: how to reach it and how its calls authenticate.
    pub server: Option<McpServer>,
    /// For an API tool, its API as pinned: where its calls go and how they authenticate.
    pub api: Option<Api>,
    pub args: JsonValue,
    /// `"{run_id}/{seq}"`: the same for every attempt of the call, so that the tool can
    /// deduplicate (`archi/04-tools.md`, "Routes"), passed in MCP `_meta` as
    /// `codes.rewind/idempotency-key`, and to an API in the `Idempotency-Key` header when its
    /// operation declares one. A fork's calls carry other keys (`archi/03-execution.md`,
    /// "Forks").
    pub idempotency_key: String,
    /// 1 for the first attempt.
    pub attempt: u32,
    /// The harness approved the call after a schema change (`tool.schema_changed`): send it
    /// against the tool's current schema, without checking it against the pinned one again.
    pub schema_change_approved: bool,
    /// The token of the claim. A handler that answers later keeps it and answers through it
    /// ([`Dispatched::Deferred`]).
    pub token: TaskToken,
    /// The W3C trace context to forward, with the tool call span as parent, so the server's own
    /// spans join the run's trace.
    pub trace_context: Option<TraceContext>,
}

/// How the dispatcher proves who calls, as the engine resolved it from the `auth` of the call's
/// server or API (`archi/04-tools.md`, "Authenticating calls"). It goes to the transport and
/// nowhere else: never in a log, a span or an outcome.
#[derive(Debug, Clone, PartialEq)]
pub enum CallAuth {
    /// The server's `auth` is none, or the call has no server (built-ins, mocks).
    None,
    /// The secret the run attached for the server, read for this attempt, and where it goes.
    Secret {
        value: Secret,
        placement: SecretPlacement,
    },
    /// The tenant's signing key, with which the dispatcher signs each HTTP request it builds for
    /// the call, and the claims the engine knows.
    Signed { key: SigningKey, claims: CallClaims },
}

/// The claims of a signed call that the engine knows (`archi/04-tools.md`, "Signed calls"). The
/// dispatcher adds those of each request: `aud` (the API's `baseUrl` or the MCP server's URL),
/// `htm`, `htu`, `body`, `iat`, `exp` (60 s later) and `jti`, `{run}/{seq}/{attempt}` for the
/// request of the call itself. An MCP session's other requests (`initialize`…) carry only
/// `iss`, `aud`, `jti`, `htm`, `htu`, `body`, `iat` and `exp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallClaims {
    /// `iss`: `{public-url}/organizations/{tenant}`; the keys are at `{iss}/jwks.json`.
    pub issuer: String,
    /// `sub`: the run's namespace, the end user.
    pub subject: NamespaceName,
    /// `labels`: the run's labels.
    pub labels: BTreeMap<String, String>,
    /// `run`.
    pub run: RunId,
    /// `tool`: the tool's name, as scripts call it.
    pub tool: String,
    /// `idempotency_key`: `{run_id}/{seq}`, or a fork's inherited key.
    pub idempotency_key: String,
    pub seq: Seq,
    /// With the run and the `seq`, gives the call's `jti`.
    pub attempt: u32,
}

impl CallClaims {
    /// The issuer of a tenant's signed calls, `{public-url}/organizations/{tenant}`: its public
    /// keys are at `{issuer}/jwks.json`, and a server set up with one issuer accepts no other
    /// tenant's JWT.
    pub fn issuer(public_url: &str, tenant: &TenantId) -> String {
        format!(
            "{}/organizations/{tenant}",
            public_url.trim_end_matches('/')
        )
    }

    /// `jti` of the request of the call itself: `{run_id}/{seq}/{attempt}`, one per attempt.
    pub fn jti(&self) -> String {
        format!("{}/{}/{}", self.run, self.seq.0, self.attempt)
    }
}

/// The key of a transient failure's `data` that holds, in seconds, how long the server asked to
/// wait before the call is sent again (an API's `Retry-After` that the dispatcher could not wait
/// out within the execution timeout): the engine retries no sooner (`archi/04-tools.md`, "API
/// tools": "never before its `Retry-After`").
pub const RETRY_AFTER_DATA: &str = "retryAfter";

/// What became of one attempt (`archi/04-tools.md`, "Tool task lifecycle").
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatched {
    /// The tool answered: the script receives this result.
    Result(JsonValue),
    /// The tool failed for good: the script receives this failure as a `ToolError`.
    Failed(ToolFailure),
    /// A failure worth retrying (unreachable, overloaded, timed out): under `retry`, the engine
    /// puts the task back to `ready` with backoff; under `at_most_once`, it fails the call with
    /// this failure.
    Transient(ToolFailure),
    /// The handler took the claim's token and will answer through it, maybe days later: the
    /// task is `deferred`.
    Deferred,
    /// The task waits for the harness: the schema changed at dispatch
    /// ([`SchemaChanged`](crate::ports::ParkKind::SchemaChanged)), or the server answered 401
    /// to the secret or the signature
    /// ([`CredentialsRefused`](crate::ports::ParkKind::CredentialsRefused)).
    Park(ParkReason),
}

/// Executes tool calls outside the sandbox: MCP servers, APIs and built-ins, or mocks in mock
/// mode (`archi/07-code-architecture.md`, "The other ports"). Host tools have no dispatcher:
/// their tasks are claimed by the harness's worker.
///
/// `dispatch(call, auth)`: `auth` is what the engine resolved from the server's `auth`: nothing,
/// the attached secret's value, or the tenant's signing key. The engine parks a call itself
/// (`secret_missing`) when the secret the run attached no longer exists, before anything is
/// sent. The dispatcher never writes to the journal, and `auth` goes into the transport and
/// nowhere else.
pub trait ToolDispatcher: Send + Sync {
    fn dispatch(&self, call: ToolCall, auth: CallAuth) -> impl Future<Output = Dispatched> + Send;
}
