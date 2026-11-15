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
    /// The determinism epochs this server's workers support. New runs start under the last.
    pub epochs: Vec<u32>,
    pub roles: Vec<ServerRole>,
    /// `rewind server --embedded`: one tenant, `local`, on a unix socket without a key.
    pub embedded: bool,
    /// Time is virtual: timers fire as soon as nothing else can advance.
    pub virtual_time: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServerRole {
    /// HTTP API, authentication, SSE. No V8.
    Api,
    /// V8, dispatcher, scheduler, projector.
    Worker,
}
