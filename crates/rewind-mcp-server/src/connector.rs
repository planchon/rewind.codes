//! What the MCP server needs from its host, for one caller.

use std::future::Future;

use rewind_agent_tools::{McpApi, Organization};
use rewind_api::namespaces::NamespaceName;

/// The host could not say who calls, or build their API: a fault of the platform.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct ConnectorError(Box<dyn std::error::Error + Send + Sync>);

impl ConnectorError {
    pub fn new(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(error.into())
    }
}

/// One caller of the MCP server, as its host knows them: `rewind-server` for an HTTP request (a
/// connection's token, an API key, an end user's token), `rewind-cli` for `rewind mcp` (the
/// operator of the embedded server).
pub trait Connector: Send + Sync + 'static {
    /// The API of `/v1` as this caller.
    type Api: McpApi + 'static;

    /// The organizations the caller reaches, each with the permissions it may use there; read
    /// again at each request, as `/v1` reads the grants.
    fn organizations(
        &self,
    ) -> impl Future<Output = Result<Vec<Organization>, ConnectorError>> + Send;

    /// The API of `/v1` as this caller in `organization` (`Rewind-Organization`), acting in
    /// `namespace` when given (`Rewind-Namespace`).
    fn api(
        &self,
        organization: &Organization,
        namespace: Option<&NamespaceName>,
    ) -> Result<Self::Api, ConnectorError>;
}
