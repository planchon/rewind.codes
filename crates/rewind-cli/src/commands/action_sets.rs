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
                    "TOOL".to_owned(),
                    "ROUTE".to_owned(),
                    "DESCRIPTION".to_owned(),
                ]];
                for tool in &action_set.tools {
                    let route = match tool.route {
                        Route::Mcp => format!("mcp {}", tool.server.as_deref().unwrap_or("")),
                        Route::Api => format!("api {}", tool.api.as_deref().unwrap_or("")),
                        Route::Host => "host".to_owned(),
                        Route::Builtin => "built-in".to_owned(),
                    };
                    let description = tool.description.lines().next().unwrap_or("").to_owned();
                    rows.push([tool.name.clone(), route, description]);
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
                .search_tools(&id, &SearchQuery { query, limit })
                .await?;
            if console.json() {
                console.print_json(&results)?;
            } else if results.tools.is_empty() {
                console.line("no tool matches");
            } else {
                for tool in &results.tools {
                    console.line(&tool.prompt);
                }
            }
        }
    }
    Ok(Exit::Success)
}
