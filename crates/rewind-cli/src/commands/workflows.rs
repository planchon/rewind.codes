//! `rewind workflows record <name>`: tests recorded from a workflow's past runs
//! (`archi/05-testing-and-observability.md`, "Tests recorded from runs").

use rewind_api::workflows::RecordTests;

use super::Server;
use crate::args::WorkflowsCommand;
use crate::error::CliError;
use crate::exit::Exit;
use crate::output::Console;

pub(super) async fn workflows(
    server: &Server<'_>,
    command: WorkflowsCommand,
    console: &mut Console,
) -> Result<Exit, CliError> {
    match command {
        WorkflowsCommand::Record {
            name,
            query,
            runs,
            limit,
        } => {
            let client = server.unbound_client().await?;
            let recorded = client
                .record_tests(&name, &RecordTests { query, runs, limit })
                .await?;
            if console.json() {
                console.print_json(&recorded)?;
                return Ok(Exit::Success);
            }
            if recorded.tests.is_empty() {
                console.line(format!("no run of {name} to record"));
            }
            for test in &recorded.tests {
                console.line(format!("{}  {}", test.digest, test.name));
                console.line(format!("  {}", test.path));
            }
            for skipped in &recorded.skipped {
                console.note(format!("skipped run {}: {}", skipped.run, skipped.reason));
            }
            Ok(Exit::Success)
        }
    }
}
