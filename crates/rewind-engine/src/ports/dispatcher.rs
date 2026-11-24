use std::collections::BTreeMap;
use std::future::Future;

use rewind_core::{Action, ActionFailure, Api, JsonValue, McpServer, RunId, SecretPlacement, Seq};

use crate::ports::journal::{ParkReason, TaskToken, TraceContext};
use crate::ports::namespaces::NamespaceName;
use crate::ports::secrets::{Secret, SigningKey};
use crate::ports::tenancy::{RunRef, TenantId};

/// One delivery of a call, as handed to an [`ActionDispatcher`].
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub run: RunRef,
    /// The run's namespace: the end user it acts for.
    pub namespace: NamespaceName,
    /// The run's labels.
    pub labels: BTreeMap<String, String>,
    pub seq: Seq,
    /// The action as pinned in the run's action set: route, operation, schemas and policy. The
    /// dispatcher checks an MCP call against the server's current schema, and enforces the
    /// execution timeout.
    pub action: Action,
    /// For an MCP action, its server as pinned: how to reach it and how its calls authenticate.
    pub server: Option<McpServer>,
    /// For an API action, its API as pinned: where its calls go and how they authenticate.
    pub api: Option<Api>,
    pub args: JsonValue,
    /// `"{run_id}/{seq}"`: the same for every delivery of the call, so that the action can
    /// deduplicate (`archi/04-tools.md`, "Routes"), passed in MCP `_meta` as
    /// `codes.rewind/idempotency-key`, and to an API in the `Idempotency-Key` header when its
    /// operation declares one. A fork's calls carry other keys (`archi/03-execution.md`,
    /// "Forks").
    pub idempotency_key: String,
    /// 1 for the first delivery.
    pub delivery: u32,
    /// The harness approved the call after a schema change (`action.schema_changed`): send it
    /// against the action's current schema, without checking it against the pinned one again.
    pub schema_change_approved: bool,
    /// The token of this delivery. A handler that answers later keeps it and answers through it
    /// ([`Dispatched::Deferred`]).
    pub token: TaskToken,
    /// The W3C trace context to forward, with the call span as parent, so the server's own
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
    /// The secret the run attached for the server, read for this delivery, and where it goes.
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
/// `htm`, `htu`, `body`, `iat`, `exp` (60 s later) and `jti`, `{run}/{seq}/{delivery}` for the
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
    /// `action`: the action's name, as scripts call it.
    pub action: String,
    /// `idempotency_key`: `{run_id}/{seq}`, or a fork's inherited key.
    pub idempotency_key: String,
    pub seq: Seq,
    /// With the run and the `seq`, gives the call's `jti`.
    pub delivery: u32,
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

    /// `jti` of the request of the call itself: `{run_id}/{seq}/{delivery}`, one per delivery.
    pub fn jti(&self) -> String {
        format!("{}/{}/{}", self.run, self.seq.0, self.delivery)
    }
}

/// The key of a transient failure's `data` that holds, in seconds, how long the server asked to
/// wait before the call is sent again (an API's `Retry-After` that the dispatcher could not wait
/// out within the execution timeout): the engine retries no sooner (`archi/04-tools.md`, "API
/// actions": "never before its `Retry-After`").
pub const RETRY_AFTER_DATA: &str = "retryAfter";

/// What became of one delivery (`archi/04-tools.md`, "Call lifecycle").
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatched {
    /// The action answered: the script receives this result.
    Result(JsonValue),
    /// The action failed for good: the script receives this failure as an `ActionError`.
    Failed(ActionFailure),
    /// A failure worth delivering again (unreachable, overloaded, timed out): when the action is
    /// idempotent, the engine puts the task back to `ready` with backoff; otherwise it fails the
    /// call with this failure.
    Transient(ActionFailure),
    /// The handler took the call's token and answers through it later, maybe days later: the
    /// task is `deferred`.
    Deferred,
    /// The task waits for the harness: the schema changed at dispatch
    /// ([`SchemaChanged`](crate::ports::ParkKind::SchemaChanged)), or the server answered 401
    /// to the secret or the signature
    /// ([`CredentialsRefused`](crate::ports::ParkKind::CredentialsRefused)).
    Park(ParkReason),
}

/// Executes calls outside the sandbox: MCP servers, APIs and built-ins, or mocks in mock
/// mode (`archi/07-code-architecture.md`, "The other ports"). Harness actions have no dispatcher:
/// their tasks are handed to the SDK workers' streams.
///
/// `dispatch(call, auth)`: `auth` is what the engine resolved from the server's `auth`: nothing,
/// the attached secret's value, or the tenant's signing key. The engine parks a call itself
/// (`secret_missing`) when the secret the run attached no longer exists, before anything is
/// sent. The dispatcher never writes to the journal, and `auth` goes into the transport and
/// nowhere else.
pub trait ActionDispatcher: Send + Sync {
    fn dispatch(&self, call: Call, auth: CallAuth) -> impl Future<Output = Dispatched> + Send;
}
