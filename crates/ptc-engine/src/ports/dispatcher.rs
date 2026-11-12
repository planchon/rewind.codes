use std::future::Future;

use ptc_core::{JsonValue, McpServer, Seq, Tool, ToolFailure};

use crate::ports::journal::{ParkReason, TaskToken, TraceContext};
use crate::ports::secrets::Credentials;
use crate::ports::tenancy::RunRef;

/// One attempt at a tool call, as handed to a [`ToolDispatcher`].
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub run: RunRef,
    pub seq: Seq,
    /// The tool as pinned in the run's toolset: route, schemas and policy. The dispatcher checks
    /// the call against the server's current schema, and enforces the execution timeout.
    pub tool: Tool,
    /// For an MCP tool, its server as pinned: how to reach it.
    pub server: Option<McpServer>,
    pub args: JsonValue,
    /// `"{run_id}/{seq}"`: the same for every attempt of the call, so that the tool can
    /// deduplicate (`archi/04-tools.md`, "Routes"), passed in MCP `_meta` as
    /// `io.ai-ptc/idempotency-key`. A fork's calls carry other keys
    /// (`archi/03-execution.md`, "Forks").
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
    /// The task waits for the harness: the schema changed at dispatch, or the server refused
    /// the credentials.
    Park(ParkReason),
}

/// Executes tool calls outside the sandbox: MCP servers and built-ins, or mocks in mock mode
/// (`archi/07-code-architecture.md`, "The other ports"). Host tools have no dispatcher: their
/// tasks are claimed by the harness's worker.
///
/// The dispatcher never writes to the journal. Credentials, resolved by the engine (the run's,
/// else the toolset's), are injected into the transport and go nowhere else.
pub trait ToolDispatcher: Send + Sync {
    fn dispatch(
        &self,
        call: ToolCall,
        credentials: Option<Credentials>,
    ) -> impl Future<Output = Dispatched> + Send;
}
