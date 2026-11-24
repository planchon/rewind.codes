//! The system prompt of an episode, in each prompt rendering of the action set.
//!
//! The renderings are what the evals compare (`archi/08-decisions.md`: "the evals decide
//! between renderings"):
//!
//! | Rendering | The actions of `rewind:actions` in the prompt |
//! | --- | --- |
//! | `compact` | The compact rendering of `rewind-codegen`: one line per action, with argument and result types |
//! | `dts` | The whole `rewind-actions.d.ts`, as the type-checker reads it |
//! | `search` | Action names only: the model reads declarations with `search_actions` |

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How the action set's declarations are written in the system prompt.
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
run_script, check_script, test_script, get_run, list_runs, signal_run, cancel_run, retry_run, \
search_actions, save_workflow, get_workflow and record_tests.

- A script is one ES module whose default export is `async function main(input)`. What `main` \
returns is the run's output, as JSON.
- It imports its actions from \"rewind:actions\", and `sleep`, `signal`, `webhook`, `run`, \
`ActionError` and `CancelledError` from \"rewind\". Nothing else can be imported: there is no npm, \
no `fetch`, no file system and no `process`.
- A call that fails throws an `ActionError`. Runs are durable: `await sleep(\"2d\")` and \
`await signal(\"name\")` can last days, and a script sees time move only as its run does.
- run_script type-checks the script before running it: fix what the diagnostics say and run it \
again. When a run fails halfway, retry_run starts it again with a corrected script, without \
repeating the calls it already made.
- A workflow is a script saved with a name and schemas (save_workflow). Each version carries \
its tests, which every later version must pass: a version is saved only if they all do.
- When the task is done, answer with a short summary of what you did, and the answer if the \
task asks a question.";

/// The `rewind` module's declarations, without the file's own comment.
fn rewind_module() -> &'static str {
    let lib = rewind_runtime::sandbox_lib::REWIND_D_TS;
    lib.find("declare module")
        .map_or(lib, |start| &lib[start..])
}

/// The system prompt for an action set's declarations, in a rendering.
pub fn system(declarations: &rewind_codegen::Declarations, rendering: Rendering) -> String {
    let actions = match rendering {
        Rendering::Compact => format!(
            "The actions of \"rewind:actions\", one per line:\n\n{}",
            declarations.prompt.trim_end()
        ),
        Rendering::Dts => format!(
            "The declarations of \"rewind:actions\" (rewind-actions.d.ts):\n\n```ts\n{}\n```",
            declarations.typescript.trim_end()
        ),
        Rendering::Search => {
            let names: Vec<&str> = declarations
                .actions
                .iter()
                .map(|action| action.name.as_str())
                .collect();
            format!(
                "The actions of \"rewind:actions\", by name: {}.\nRead their declarations (arguments, results, \
                 descriptions) with search_actions before using them.",
                names.join(", ")
            )
        }
    };
    format!(
        "{INTRODUCTION}\n\nThe `rewind` module:\n\n```ts\n{}\n```\n\n{actions}\n",
        rewind_module().trim_end()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declarations() -> rewind_codegen::Declarations {
        rewind_codegen::Declarations {
            typescript: "declare module \"rewind:actions\" {}\n".into(),
            prompt: "import { crm } from \"rewind:actions\";\ncrm.get_customer({ id: string }) → ActionContent · Get a customer\n".into(),
            actions: vec![rewind_codegen::ActionDeclaration {
                name: "crm.get_customer".into(),
                prompt: String::new(),
                typescript: String::new(),
            }],
        }
    }

    #[test]
    fn each_rendering_writes_the_actions_its_way() {
        let declarations = declarations();
        let compact = system(&declarations, Rendering::Compact);
        assert!(compact.contains("crm.get_customer({ id: string })"));
        assert!(compact.contains("declare module \"rewind\""));
        assert!(!compact.contains("archi/"));
        assert!(
            system(&declarations, Rendering::Dts).contains("declare module \"rewind:actions\" {}")
        );
        let search = system(&declarations, Rendering::Search);
        assert!(search.contains("by name: crm.get_customer."));
        assert!(!search.contains("crm.get_customer({"));
        assert_eq!("dts".parse::<Rendering>(), Ok(Rendering::Dts));
        assert!("names".parse::<Rendering>().is_err());
    }
}
