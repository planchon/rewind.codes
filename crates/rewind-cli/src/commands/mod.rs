//! The commands. Each returns its exit code, or the error that stopped it.

mod check;
mod namespaces;
mod replay;
mod runs;
mod test;
mod toolsets;

use rewind_api::Client;
use rewind_api::headers::API_KEY_PREFIX;

use crate::args::{Command, Global};
use crate::embedded::{LaunchError, Launcher};
use crate::error::CliError;
use crate::exit::Exit;
use crate::output::Console;

pub(crate) async fn execute(
    command: Command,
    global: &Global,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let server = Server { global };
    match command {
        Command::Run(args) => runs::run(&server, args, console).await,
        Command::Check(args) => check::check(&server, args, console).await,
        Command::Test(args) => test::test(args, console).await,
        Command::Signal(args) => runs::signal(&server, args, console).await,
        Command::Cancel(args) => runs::cancel(&server, args, console).await,
        Command::Status(args) => runs::status(&server, args, console).await,
        Command::Replay(args) => replay::replay(&server, args, console).await,
        Command::Fork(args) => runs::fork(&server, args, console).await,
        Command::Trace(args) => runs::trace(&server, args, console).await,
        Command::Runs(args) => runs::list(&server, args, console).await,
        Command::Release(args) => runs::release(&server, args, console).await,
        Command::Export(args) => runs::export(&server, args, console).await,
        Command::Toolsets(command) => toolsets::toolsets(&server, command, console).await,
        Command::Namespaces(command) => namespaces::namespaces(&server, command, console).await,
    }
}

/// The server a command talks to, connected only when the command needs it: local commands
/// never start the embedded server.
pub(crate) struct Server<'a> {
    global: &'a Global,
}

impl Server<'_> {
    /// The remote server of `--url`, or the embedded server of `--dir`, started if need be,
    /// acting for the namespace of `--namespace`; against the embedded server, `local` when none
    /// is given.
    pub(crate) async fn client(&self) -> Result<Client, CliError> {
        self.connect(true).await
    }

    /// The client of a command that starts a run: against a remote server, an API key names the
    /// namespace with `--namespace` (a usage error without it); an end user's token names its
    /// own.
    pub(crate) async fn starting_client(&self) -> Result<Client, CliError> {
        let token = self
            .global
            .api_key
            .as_deref()
            .is_some_and(|key| !key.starts_with(API_KEY_PREFIX));
        if self.global.url.is_some() && self.global.namespace.is_none() && !token {
            return Err(CliError::Usage(
                "--namespace (REWIND_NAMESPACE) is required to start a run on a remote server: \
                 it names the end user the run acts for"
                    .into(),
            ));
        }
        self.client().await
    }

    /// The client of a command across namespaces: bound to one only when `--namespace` names it.
    pub(crate) async fn unbound_client(&self) -> Result<Client, CliError> {
        self.connect(false).await
    }

    async fn connect(&self, local_by_default: bool) -> Result<Client, CliError> {
        let namespace = |name: &str| {
            rewind_api::namespaces::NamespaceName::new(name)
                .map_err(|error| CliError::Usage(format!("--namespace: {error}")))
        };
        let client = match &self.global.url {
            Some(url) => Client::remote(url, self.global.api_key.as_deref())?,
            None => {
                let launcher = Launcher::new(&self.global.dir).map_err(server_error)?;
                launcher.connect().await.map_err(server_error)?
            }
        };
        let name = match (&self.global.namespace, &self.global.url) {
            (Some(name), _) => Some(name.as_str()),
            (None, None) if local_by_default => Some(LOCAL),
            (None, _) => None,
        };
        Ok(match name {
            Some(name) => client.with_namespace(namespace(name)?),
            None => client,
        })
    }
}

/// The namespace of the CLI against its own embedded server when `--namespace` names none.
const LOCAL: &str = "local";

fn server_error(error: LaunchError) -> CliError {
    match error {
        LaunchError::Client(error) => CliError::Api(error),
        other => CliError::Server(other.to_string()),
    }
}
