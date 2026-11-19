//! Rewind's MCP server (`archi/07-code-architecture.md`, "MCP and OAuth"): the agent tools of
//! the MCP set (`rewind-agent-tools`) as an `rmcp` [`ServerHandler`](rmcp::ServerHandler), for
//! Claude and any MCP client ([`RewindMcp`]).
//!
//! - `initialize` answers the name `Rewind`, this crate's version and [`INSTRUCTIONS`]: from an
//!   organization to a run.
//! - `tools/list` lists the tools the caller may use in at least one of its organizations, each
//!   with its title, its input schema and its annotations; `list_orgs` always.
//! - `tools/call` resolves the call's `org` among the caller's organizations (an id, or a name no
//!   other of them has; left out when the caller reaches one), checks the tool's permission
//!   there, then runs the agent tool with an API of `/v1` bound to that organization and to the
//!   call's `namespace`. Every refusal the model can fix is a result with `isError`; a fault of
//!   the platform is a JSON-RPC internal error.
//!
//! # The boundary with its hosts
//!
//! The server knows neither tokens nor stores. Its host gives it, for each caller, a
//! [`Connector`]: the caller's organizations, each with the permissions it may use there, and an
//! [`McpApi`](rewind_agent_tools::McpApi) for an organization and an optional namespace, which
//! calls `/v1` as that caller.
//!
//! - **Over HTTP** ([`http_service`]), `rewind-server` serves `POST /mcp`: Streamable HTTP
//!   without session, each request a whole exchange answered in JSON, `GET` and `DELETE` 405.
//!   The host authenticates the request first (and answers 401 with `WWW-Authenticate` itself),
//!   puts what it found in the request's extensions, and gives the service a function that builds
//!   the connector from the request's [`http::request::Parts`]. `None` there is a fault of the
//!   host: the call fails with an internal error.
//! - **Over stdio** ([`serve_stdio`]), `rewind mcp` serves one connector for the whole session:
//!   the operator of the embedded server.

#![forbid(unsafe_code)]

mod connector;
mod handler;
mod organizations;
mod transport;

pub use connector::{Connector, ConnectorError};
pub use handler::{INSTRUCTIONS, RewindMcp};
pub use rewind_agent_tools::Organization;
pub use transport::{HttpService, ServeError, http_service, serve, serve_stdio};
