//! The agent tools of Rewind, in Rust (`archi/01-product.md`, "Agent tools";
//! `archi/07-code-architecture.md`, "Crates").
//!
//! The tools a model uses to write, check, test, run, follow and repair its scripts, as the
//! TypeScript SDK defines them (`sdk/typescript/src/agent-tools/`), which stays the reference:
//!
//! - [`definitions`]: the tools for a model. The SDK's set, word for word; the MCP set, with
//!   `save_workflow` out, `list_orgs`, `list_namespaces` and `list_action_sets` in, `org`,
//!   `namespace` and `action_set` as arguments, and each tool's title, annotations and permission.
//! - [`AgentTools`]: the handlers of the SDK's set over a port [`Api`], one method per operation
//!   of `/v1` they call, with `rewind-api`'s DTOs; [`McpTools`] those of the MCP set over
//!   [`McpApi`], which adds the operations that find what a caller reaches.
//! - [`render`]: the bounded plain-text results.
//!
//! `rewind_api::Client` implements both ports: the CLI's `rewind mcp` and `rewind-server` use it.
//! The evals implement [`Api`] with the engine in process.
//!
//! A mistake the model can fix (a bad argument, a refused script, a run that has ended: a
//! problem of `/v1` with a 4xx status but 401) is a result with `is_error`; a fault of the
//! platform (unreachable, a 401, a 5xx) or of the harness is an [`AgentError`].

#![forbid(unsafe_code)]

mod api;
mod client;
pub mod definitions;
mod input;
mod keys;
mod mcp;
pub mod render;
mod tools;

pub use api::{Api, ApiError, McpApi};
pub use keys::{idempotency_key, run_key};
pub use mcp::{MCP_DEFAULT_WAIT, MCP_MAX_WAIT, McpTools, Organization};
pub use tools::{
    AgentError, AgentOptions, AgentToolResult, AgentTools, HandleOptions, ScriptVerdict,
};
