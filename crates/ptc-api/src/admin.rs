//! Administration: API keys (`archi/06-storage-and-scale.md`, "Tenancy and authentication")
//! and the server itself (`archi/07-code-architecture.md`, "Embedded server and CLI").

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::common::{Labels, Timestamp};

/// What a key may call. The key's tenant scopes everything it sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Everything, and the administration of API keys for every tenant. The operator's role.
    Admin,
    /// Everything of its tenant but the administration: toolsets, scripts, runs, tasks,
    /// subscriptions, reading. The role of a harness and of its agent tools.
    Harness,
    /// Only the host-tool task endpoints: the role of a harness's SDK worker.
    Worker,
    /// Only reading: the `GET` endpoints of its tenant. The role of a dashboard.
    Reader,
}

/// `POST /v1/api-keys`: a key `ptc_sk_…` for a tenant. Keys are stored hashed: the key itself
/// is only in this answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateApiKey {
    /// Created with its first key.
    pub tenant: String,
    pub role: Role,
    /// The key only sees runs carrying all these labels: an agent's tools see only their own
    /// conversation's runs.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub allowed_labels: Labels,
    /// What the key is for, for the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiKey {
    /// For `DELETE /v1/api-keys/{id}`. Not a secret.
    pub id: String,
    pub tenant: String,
    pub role: Role,
    pub allowed_labels: Labels,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub created_at: Timestamp,
    /// The key, `ptc_sk_…`, to send as `Authorization: Bearer ptc_sk_…`. Only in the answer
    /// to `POST /v1/api-keys`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// `GET /v1/server`: what this server is. Every response also carries the `Ptc-Version`
/// header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// The version of the `ptc` binary. A CLI of another version restarts an embedded server.
    pub version: String,
    /// The determinism epochs this server's workers support. New runs start under the last.
    pub epochs: Vec<u32>,
    pub roles: Vec<ServerRole>,
    /// `ptc server --embedded`: one tenant, `local`, on a unix socket without a key.
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
