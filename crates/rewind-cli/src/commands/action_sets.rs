//! `rewind action-sets add|declarations|search`: action sets registered on the server
//! (`archi/04-tools.md`, "Action sets and code generation").

use rewind_api::action_sets::{CreateActionSet, DeclarationFormat, Route, SearchQuery};

use super::Server;
use crate::args::{ActionSetsCommand, DeclarationsFormat};
use crate::error::CliError;
use crate::exit::Exit;
use crate::input;
use crate::output::Console;
use crate::render;

pub(super) async fn action_sets(
    server: &Server<'_>,
    command: ActionSetsCommand,
    console: &mut Console,
) -> Result<Exit, CliError> {
    match command {
        ActionSetsCommand::Add { file } => {
            let create: CreateActionSet = input::read_json(&file)?;
            let client = server.client().await?;
            let action_set = client.create_action_set(&create).await?;
            if console.json() {
                console.print_json(&action_set)?;
            } else {
                console.line(format!("action set {}", action_set.id));
                let mut rows = vec![[
                    "ACTION".to_owned(),
                    "ROUTE".to_owned(),
                    "DESCRIPTION".to_owned(),
                ]];
                for action in &action_set.actions {
                    let route = match action.route {
                        Route::Mcp => format!("mcp {}", action.server.as_deref().unwrap_or("")),
                        Route::Api => format!("api {}", action.api.as_deref().unwrap_or("")),
                        Route::Harness => "harness".to_owned(),
                        Route::Builtin => "built-in".to_owned(),
                    };
                    let description = action.description.lines().next().unwrap_or("").to_owned();
                    rows.push([action.name.clone(), route, description]);
                }
                console.text(&render::table(&rows));
            }
        }
        ActionSetsCommand::Declarations { id, format } => {
            let format = match format {
                DeclarationsFormat::Dts => DeclarationFormat::Dts,
                DeclarationsFormat::Prompt => DeclarationFormat::Prompt,
            };
            let client = server.client().await?;
            let declarations = client.action_set_declarations(&id, format).await?;
            if console.json() {
                console.print_json(&declarations)?;
            } else {
                console.text(&declarations.text);
            }
        }
        ActionSetsCommand::Search { id, query, limit } => {
            let client = server.client().await?;
            let results = client
                .search_actions(&id, &SearchQuery { query, limit })
                .await?;
            if console.json() {
                console.print_json(&results)?;
            } else if results.actions.is_empty() {
                console.line("no action matches");
            } else {
                for action in &results.actions {
                    console.line(&action.prompt);
                }
            }
        }
    }
    Ok(Exit::Success)
}
