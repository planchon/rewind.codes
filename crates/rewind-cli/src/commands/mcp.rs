//! `rewind mcp`: the agent tools over MCP on stdin and stdout, against the embedded server
//! (`archi/07-code-architecture.md`, "Embedded server and CLI"), to debug them with an MCP client
//! on this machine (the MCP Inspector, Claude Code). Never a remote server
//! (`archi/08-decisions.md`, "Taken for Claude as a harness").
//!
//! The socket's one caller is the operator of the tenant `local`: it is the session's one
//! organization, with every permission. Each call reaches the server through `rewind-api`'s
//! client on the socket, with its namespace as `Rewind-Namespace`.

use rewind_agent_tools::Organization;
use rewind_api::Client;
use rewind_api::access::Permission;
use rewind_api::namespaces::NamespaceName;
use rewind_mcp_server::{Connector, ConnectorError};

use super::Server;
use crate::error::CliError;
use crate::exit::Exit;
use crate::interrupt;
use crate::output::Console;

/// The tenant of the embedded server.
const TENANT: &str = "local";

/// The operator of the embedded server, on its socket.
struct Embedded {
    client: Client,
}

impl Connector for Embedded {
    type Api = Client;

    async fn organizations(&self) -> Result<Vec<Organization>, ConnectorError> {
        Ok(vec![Organization {
            id: TENANT.to_owned(),
            name: None,
            permissions: Permission::ALL.to_vec(),
        }])
    }

    /// The socket's caller has one tenant: the organization needs no header.
    fn api(
        &self,
        _organization: &Organization,
        namespace: Option<&NamespaceName>,
    ) -> Result<Client, ConnectorError> {
        Ok(match namespace {
            Some(namespace) => self.client.with_namespace(namespace.clone()),
            None => self.client.clone(),
        })
    }
}

pub(super) async fn mcp(server: &Server<'_>, console: &mut Console) -> Result<Exit, CliError> {
    if server.global.url.is_some() {
        return Err(CliError::Usage(
            "rewind mcp serves the embedded server of this directory only, to debug the agent \
             tools: leave out --url (REWIND_URL). Claude reaches a deployed Rewind at its /mcp \
             URL, with OAuth"
                .into(),
        ));
    }
    let client = server.unbound_client().await?;
    let (input, output) = console.take_mcp_streams();
    console.note(format!(
        "serving the agent tools over MCP on stdin and stdout, against the embedded server of {}",
        server.global.dir.display()
    ));
    console.flush();
    tokio::select! {
        served = rewind_mcp_server::serve(Embedded { client }, input, output) => {
            served.map_err(|error| CliError::Internal(error.to_string()))?;
        }
        () = interrupt::requested_now() => {}
    }
    Ok(Exit::Success)
}
