//! The commands. Each returns its exit code, or the error that stopped it.

mod check;
mod replay;
mod runs;
mod test;
mod toolsets;

use rewind_api::Client;

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
    }
}

/// The server a command talks to, connected only when the command needs it: local commands
/// never start the embedded server.
pub(crate) struct Server<'a> {
    global: &'a Global,
}

impl Server<'_> {
    /// The remote server of `--url`, or the embedded server of `--dir`, started if need be.
    pub(crate) async fn client(&self) -> Result<Client, CliError> {
        let namespace = |name: &str| {
            rewind_api::namespaces::NamespaceName::new(name)
                .map_err(|error| CliError::Usage(format!("--namespace: {error}")))
        };
        match &self.global.url {
            Some(url) => {
                let client = Client::remote(url, self.global.api_key.as_deref())?;
                Ok(match &self.global.namespace {
                    Some(name) => client.with_namespace(namespace(name)?),
                    None => client,
                })
            }
            None => {
                let launcher = Launcher::new(&self.global.dir).map_err(server_error)?;
                let client = launcher.connect().await.map_err(server_error)?;
                let name = self.global.namespace.as_deref().unwrap_or("local");
                Ok(client.with_namespace(namespace(name)?))
            }
        }
    }
}

fn server_error(error: LaunchError) -> CliError {
    match error {
        LaunchError::Client(error) => CliError::Api(error),
        other => CliError::Server(other.to_string()),
    }
}
