//! The server itself (`archi/07-code-architecture.md`, "Embedded server and CLI"). API keys are
//! in [`crate::access`], who calls in [`crate::organization`].

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `GET /v1/server`: what this server is. Every response also carries the `Rewind-Version`
/// header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// The version of the `rewind` binary. A CLI of another version restarts an embedded server.
    pub version: String,
    /// The determinism epochs this server's executors support. New runs start under the last.
    pub epochs: Vec<u32>,
    /// The node that answered.
    pub node: String,
    /// The partitions of the deployment: 64 by default, 1 for the embedded server.
    pub partitions: u16,
    /// The nodes of the deployment alive now, executors-only nodes included.
    pub nodes: u32,
    /// `rewind server --embedded`: one tenant, `local`, on a unix socket without a key.
    pub embedded: bool,
    /// Time is virtual: timers fire as soon as nothing else can advance.
    pub virtual_time: bool,
    /// The URL MCP clients add as a connector, `{public-url}/mcp`; null without a public URL.
    #[serde(default)]
    pub mcp_url: Option<String>,
    /// People connect MCP clients with OAuth (`/oauth`): the server has a public URL and a
    /// sign-in. Without it, `/mcp` takes API keys and end users' tokens only.
    #[serde(default)]
    pub oauth: bool,
}
