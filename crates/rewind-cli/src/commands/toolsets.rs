//! `rewind toolsets add|declarations|search`: toolsets registered on the server
//! (`archi/04-tools.md`, "Toolsets and code generation").

use rewind_api::toolsets::{CreateToolset, DeclarationFormat, Route, SearchQuery};

use super::Server;
use crate::args::{DeclarationsFormat, ToolsetsCommand};
use crate::error::CliError;
use crate::exit::Exit;
use crate::input;
use crate::output::Console;
use crate::render;

pub(super) async fn toolsets(
    server: &Server<'_>,
    command: ToolsetsCommand,
    console: &mut Console,
) -> Result<Exit, CliError> {
    match command {
        ToolsetsCommand::Add { file } => {
            let create: CreateToolset = input::read_json(&file)?;
            let client = server.client().await?;
            let toolset = client.create_toolset(&create).await?;
            if console.json() {
                console.print_json(&toolset)?;
            } else {
                console.line(format!("toolset {}", toolset.id));
                let mut rows = vec![[
                    "TOOL".to_owned(),
                    "ROUTE".to_owned(),
                    "DESCRIPTION".to_owned(),
                ]];
                for tool in &toolset.tools {
                    let route = match tool.route {
                        Route::Mcp => format!("mcp {}", tool.server.as_deref().unwrap_or("")),
                        Route::Host => "host".to_owned(),
                        Route::Builtin => "built-in".to_owned(),
                    };
                    let description = tool.description.lines().next().unwrap_or("").to_owned();
                    rows.push([tool.name.clone(), route, description]);
                }
                console.text(&render::table(&rows));
            }
        }
        ToolsetsCommand::Declarations { id, format } => {
            let format = match format {
                DeclarationsFormat::Dts => DeclarationFormat::Dts,
                DeclarationsFormat::Prompt => DeclarationFormat::Prompt,
            };
            let client = server.client().await?;
            let declarations = client.toolset_declarations(&id, format).await?;
            if console.json() {
                console.print_json(&declarations)?;
            } else {
                console.text(&declarations.text);
            }
        }
        ToolsetsCommand::Search { id, query, limit } => {
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
