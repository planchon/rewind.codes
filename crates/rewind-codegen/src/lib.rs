//! Code generation of Rewind (`archi/04-tools.md`, "Action sets and code generation").
//!
//! From a pinned action set, [`generate`] writes what the type-checker and the model read:
//!
//! | Output | Read by | Example |
//! | --- | --- | --- |
//! | `rewind-tools.d.ts`: the `rewind:tools` module, types for args (from `inputSchema`) and results (from `outputSchema`, else the MCP content type), tool descriptions as JSDoc | tsgo | `list_issues(args: { repo: string; state?: "open" \| "closed" }): Promise<github.Issue[]>` |
//! | The compact prompt rendering, one line per tool | The model | `github.list_issues({ repo: string, state?: "open" \| "closed" }) → Issue[] · List the issues` |
//!
//! The runtime module `rewind:tools` itself is part of the runtime's glue, kept per determinism
//! epoch. Codegen is deterministic: the same action set and observed shapes give the same bytes.
//!
//! The JSON Schema → TypeScript converter is ours: no maintained crate exists.
//! When a tool declares no `outputSchema`, its result type comes from the shapes observed in
//! recorded results, when there are some, marked `@observed`: for a host tool, the result's
//! shape; for an MCP tool, which returns its content, the shape of its `structuredContent`.
//!
//! `rewind-tools.d.ts` always declares the module `rewind:tools` (`declare module "rewind:tools" { … }`),
//! empty for an action set without tools. An action set that pins workflows also gets the module
//! `rewind:workflows`, one typed export per workflow, and a prompt line per workflow; the
//! declarations a workflow's own script is checked against come from
//! [`workflow_declarations`]. Built-ins are not part of it: scripts reach `http` through
//! `fetch` and `webhook` through the `rewind` module. For an action set that enables `http`, the file
//! starts with [`HTTP_TOOL_REFERENCE`], from which the type-checker knows to declare `fetch`.
//! The prompt lists the built-ins too, with the hosts `fetch` may reach.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use rewind_core::{ActionSet, Builtin, Shape, ToolRoute};

mod dts;
mod model;
mod observed;
mod prompt;
mod schema;
mod ty;
mod workflows;

pub use workflows::{WorkflowDeclarations, workflow_declarations};

/// The first line of the `rewind-tools.d.ts` of an action set that enables the `http` tool. It is how
/// the type-checker learns that the script has `fetch`: it then adds `fetch.d.ts` to the check.
/// `rewind-check` exports the same line (`HTTP_TOOL_REFERENCE`); `archi/04-tools.md` records it.
pub const HTTP_TOOL_REFERENCE: &str = "/// <reference path=\"fetch.d.ts\" />";

/// What codegen generated for an action set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Declarations {
    /// `rewind-tools.d.ts`.
    pub typescript: String,
    /// The compact prompt rendering: an import line, one line per tool, then the line of
    /// `ToolContent` when a tool returns MCP content.
    pub prompt: String,
    /// Each tool on its own, sorted by name: what `search_tools` returns.
    pub tools: Vec<ToolDeclaration>,
}

/// One tool's declarations, or one pinned workflow's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDeclaration {
    /// As scripts call it: `github.list_issues`, `ask_user`, `http`; `workflows.triage` for the
    /// workflow `triage` of `rewind:workflows`.
    pub name: String,
    /// The tool's line of the prompt rendering.
    pub prompt: String,
    /// The tool's declaration in `rewind-tools.d.ts`, with the types it uses, as a module of its
    /// own. For a built-in, its declaration in the sandbox lib.
    pub typescript: String,
}

/// Generates the declarations of an action set. `observed` holds the shapes observed in the
/// recorded results of its tools, by tool name (`ReadModel::observed_shapes`); they type the
/// results of the tools that declare no output schema.
///
/// A bare host tool whose name is not an identifier cannot be imported from `rewind:tools`: it is
/// left out, as the runtime leaves it out of the module.
pub fn generate(action_set: &ActionSet, observed: &BTreeMap<String, Shape>) -> Declarations {
    let exports = model::exports(action_set, observed);
    let http_tool = action_set
        .tools()
        .iter()
        .any(|tool| matches!(tool.route, ToolRoute::Builtin(Builtin::Http { .. })));
    let mut typescript = dts::module(&action_set.digest().short(), &exports, http_tool);
    if !action_set.workflows().is_empty() {
        typescript.push('\n');
        typescript.push_str(&workflows::module(action_set.workflows()));
    }

    let mut tools = Vec::new();
    for export in &exports {
        for signature in &export.tools {
            tools.push(ToolDeclaration {
                name: signature.tool.name.clone(),
                prompt: prompt::tool(export, signature),
                typescript: dts::tool(export, signature),
            });
        }
    }
    for tool in action_set.tools() {
        if let Some(builtin) = prompt::builtin_of(tool) {
            tools.push(ToolDeclaration {
                name: tool.name.clone(),
                prompt: prompt::builtin(builtin),
                typescript: builtin_declaration(builtin).to_owned(),
            });
        }
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let pinned: Vec<ToolDeclaration> = action_set
        .workflows()
        .iter()
        .map(|workflow| ToolDeclaration {
            name: format!("{}.{}", rewind_core::WORKFLOWS_NAMESPACE, workflow.name),
            prompt: workflows::prompt(workflow),
            typescript: workflows::declaration(workflow),
        })
        .collect();

    let mut lines: Vec<String> = prompt::imports(&exports).into_iter().collect();
    lines.extend(workflows::imports(action_set.workflows()));
    lines.extend(tools.iter().map(|tool| tool.prompt.clone()));
    lines.extend(pinned.iter().map(|workflow| workflow.prompt.clone()));
    tools.extend(pinned);
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    if typescript.contains("export interface ToolContent<") {
        lines.push(prompt::CONTENT_LINE.to_owned());
    }
    let mut prompt = lines.join("\n");
    if !prompt.is_empty() {
        prompt.push('\n');
    }

    Declarations {
        typescript,
        prompt,
        tools,
    }
}

/// How the sandbox lib declares a built-in.
fn builtin_declaration(builtin: &rewind_core::Builtin) -> &'static str {
    match builtin {
        rewind_core::Builtin::Http { .. } => {
            "declare function fetch(input: string | URL, init?: RequestInit): Promise<Response>;\n"
        }
        rewind_core::Builtin::Webhook => {
            "declare module \"rewind\" {\n  export function webhook(name: string): Promise<{ url: string }>;\n}\n"
        }
        // Not listed: a workflow's built-ins are reached through `rewind:workflows`.
        rewind_core::Builtin::Workflow { .. } => "",
    }
}
