//! How the server is reached: Streamable HTTP without session, for `rewind-server`'s
//! `POST /mcp`; stdio, for `rewind mcp`.

use std::sync::Arc;

use rmcp::ServiceExt as _;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::connector::Connector;
use crate::handler::RewindMcp;

/// The tower service of `POST /mcp`.
pub type HttpService<C> = StreamableHttpService<RewindMcp<C>, NeverSessionManager>;

/// The tower service of `POST /mcp`: Streamable HTTP without session, each request a whole
/// exchange answered in JSON (`application/json`, no `Mcp-Session-Id`), `GET` and `DELETE`
/// answered 405. `connect` builds the caller of each request from its parts, where the host put
/// what it authenticated. `allowed_hosts` are the `Host` values the service answers (the public
/// URL's host and port); empty, any.
pub fn http_service<C, F>(connect: F, allowed_hosts: Vec<String>) -> HttpService<C>
where
    C: Connector,
    F: Fn(&http::request::Parts) -> Option<C> + Send + Sync + 'static,
{
    let handler = RewindMcp::per_request(connect);
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_sse_retry(None);
    let config = if allowed_hosts.is_empty() {
        config.disable_allowed_hosts()
    } else {
        config.with_allowed_hosts(allowed_hosts)
    };
    StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    )
}

/// Why a session over a stream ended in error.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The client's `initialize` did not complete.
    #[error("the MCP session could not start: {0}")]
    Initialize(String),
    /// The session broke.
    #[error("the MCP session ended in error: {0}")]
    Session(String),
}

/// Serves one caller over a pair of streams until the client closes them.
pub async fn serve<C, R, W>(connector: C, input: R, output: W) -> Result<(), ServeError>
where
    C: Connector,
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let running = RewindMcp::new(connector)
        .serve((input, output))
        .await
        .map_err(|error| ServeError::Initialize(error.to_string()))?;
    running
        .waiting()
        .await
        .map(drop)
        .map_err(|error| ServeError::Session(error.to_string()))
}

/// Serves one caller on the process's stdin and stdout until the client closes them.
pub async fn serve_stdio<C: Connector>(connector: C) -> Result<(), ServeError> {
    let (input, output) = rmcp::transport::io::stdio();
    serve(connector, input, output).await
}
