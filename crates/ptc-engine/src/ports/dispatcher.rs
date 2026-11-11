use std::future::Future;

use ptc_core::{JsonValue, RunId, Seq, ToolFailure};

/// One attempt at a tool call, as handed to a [`ToolDispatcher`].
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub run_id: RunId,
    pub seq: Seq,
    pub tool: String,
    pub args: JsonValue,
    /// `"{run_id}/{seq}"`: the same for every attempt of the call, so that the tool can
    /// deduplicate (`archi/04-tools.md`, "Side effects and dispatch").
    pub idempotency_key: String,
    /// 1 for the first attempt.
    pub attempt: u32,
}

/// Executes tool calls outside the sandbox: MCP servers, host tools, built-ins, or static
/// responses in tests and mock mode.
///
/// The outcome is what the script sees: a result, or a failure thrown as a `ToolError`.
pub trait ToolDispatcher: Send + Sync {
    fn dispatch(
        &self,
        call: ToolCall,
    ) -> impl Future<Output = Result<JsonValue, ToolFailure>> + Send;
}
