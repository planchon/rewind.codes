//! The system prompt of an episode, in each prompt rendering of the toolset.
//!
//! The renderings are what the evals compare (`archi/08-decisions.md`: "the evals decide
//! between renderings"):
//!
//! | Rendering | The tools of `rewind:tools` in the prompt |
//! | --- | --- |
//! | `compact` | The compact rendering of `rewind-codegen`: one line per tool, with argument and result types |
//! | `dts` | The whole `rewind-tools.d.ts`, as the type-checker reads it |
//! | `search` | Tool names only: the model reads declarations with `search_tools` |

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How the toolset's declarations are written in the system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rendering {
    Compact,
    Dts,
    Search,
}

impl Rendering {
    pub const ALL: [Rendering; 3] = [Rendering::Compact, Rendering::Dts, Rendering::Search];

    pub fn as_str(self) -> &'static str {
        match self {
            Rendering::Compact => "compact",
            Rendering::Dts => "dts",
            Rendering::Search => "search",
        }
    }
}

impl fmt::Display for Rendering {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Rendering {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        Rendering::ALL
            .into_iter()
            .find(|rendering| rendering.as_str() == text)
            .ok_or_else(|| format!("unknown rendering {text:?}: expected compact, dts or search"))
    }
}

const INTRODUCTION: &str = "\
You solve tasks by writing TypeScript scripts that Rewind runs for you, through the tools \
run_script, check_script, test_script, get_run, list_runs, signal_run, cancel_run, retry_run \
and search_tools.

- A script is one ES module whose default export is `async function main(input)`. What `main` \
returns is the run's output, as JSON.
- It imports its tools from \"rewind:tools\", and `sleep`, `signal`, `webhook`, `run`, \
`ToolError` and `CancelledError` from \"rewind\". Nothing else can be imported: there is no npm, \
no `fetch`, no file system and no `process`.
- A tool call that fails throws a `ToolError`. Runs are durable: `await sleep(\"2d\")` and \
`await signal(\"name\")` can last days, and a script sees time move only as its run does.
- run_script type-checks the script before running it: fix what the diagnostics say and run it \
again. When a run fails halfway, retry_run starts it again with a corrected script, without \
repeating the calls it already made.
- When the task is done, answer with a short summary of what you did, and the answer if the \
task asks a question.";

/// The `rewind` module's declarations, without the file's own comment.
fn rewind_module() -> &'static str {
    let lib = rewind_runtime::sandbox_lib::REWIND_D_TS;
    lib.find("declare module")
        .map_or(lib, |start| &lib[start..])
}

/// The system prompt for a toolset's declarations, in a rendering.
pub fn system(declarations: &rewind_codegen::Declarations, rendering: Rendering) -> String {
    let tools = match rendering {
        Rendering::Compact => format!(
            "The tools of \"rewind:tools\", one per line:\n\n{}",
            declarations.prompt.trim_end()
        ),
        Rendering::Dts => format!(
            "The declarations of \"rewind:tools\" (rewind-tools.d.ts):\n\n```ts\n{}\n```",
            declarations.typescript.trim_end()
        ),
        Rendering::Search => {
            let names: Vec<&str> = declarations
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect();
            format!(
                "The tools of \"rewind:tools\", by name: {}.\nRead their declarations (arguments, results, \
                 descriptions) with search_tools before using them.",
                names.join(", ")
            )
        }
    };
    format!(
        "{INTRODUCTION}\n\nThe `rewind` module:\n\n```ts\n{}\n```\n\n{tools}\n",
        rewind_module().trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declarations() -> rewind_codegen::Declarations {
        rewind_codegen::Declarations {
            typescript: "declare module \"rewind:tools\" {}\n".into(),
            prompt: "import { crm } from \"rewind:tools\";\ncrm.get_customer({ id: string }) → ToolContent · Get a customer\n".into(),
            tools: vec![rewind_codegen::ToolDeclaration {
                name: "crm.get_customer".into(),
                prompt: String::new(),
                typescript: String::new(),
            }],
        }
    }

    #[test]
    fn each_rendering_writes_the_tools_its_way() {
        let declarations = declarations();
        let compact = system(&declarations, Rendering::Compact);
        assert!(compact.contains("crm.get_customer({ id: string })"));
        assert!(compact.contains("declare module \"rewind\""));
        assert!(!compact.contains("archi/"));
        assert!(
            system(&declarations, Rendering::Dts).contains("declare module \"rewind:tools\" {}")
        );
        let search = system(&declarations, Rendering::Search);
        assert!(search.contains("by name: crm.get_customer."));
        assert!(!search.contains("crm.get_customer({"));
        assert_eq!("dts".parse::<Rendering>(), Ok(Rendering::Dts));
        assert!("names".parse::<Rendering>().is_err());
    }
}
