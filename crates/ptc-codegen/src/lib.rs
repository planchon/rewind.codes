//! Code generation of ai-ptc (`archi/04-tools.md`, "Toolsets and code generation").
//!
//! From a pinned toolset, [`generate`] writes what the type-checker and the model read:
//!
//! | Output | Read by | Example |
//! | --- | --- | --- |
//! | `ptc-tools.d.ts`: the `ptc:tools` module, types for args (from `inputSchema`) and results (from `outputSchema`, else the MCP content type), tool descriptions as JSDoc | tsgo | `list_issues(args: { repo: string; state?: "open" \| "closed" }): Promise<github.Issue[]>` |
//! | The compact prompt rendering, one line per tool | The model | `github.list_issues({ repo: string, state?: "open" \| "closed" }) → Issue[] · List the issues` |
//!
//! The runtime module `ptc:tools` itself is part of the runtime's glue, kept per determinism
//! epoch. Codegen is deterministic: the same toolset and observed shapes give the same bytes.
//!
//! The JSON Schema → TypeScript converter is ours: no maintained crate exists.
//! When a tool declares no `outputSchema`, its result type comes from the shapes observed in
//! recorded results, when there are some, marked `@observed`: for a host tool, the result's
//! shape; for an MCP tool, which returns its content, the shape of its `structuredContent`.
//!
//! Built-ins are not part of `ptc:tools`: scripts reach `http` through `fetch` (declared by the
//! sandbox lib when the toolset enables it) and `webhook` through the `ptc` module. The prompt
//! still lists them, with the hosts `fetch` may reach.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use ptc_core::{Shape, Toolset};

mod dts;
mod model;
mod observed;
mod prompt;
mod schema;
mod ty;

/// What codegen generated for a toolset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Declarations {
    /// `ptc-tools.d.ts`.
    pub typescript: String,
    /// The compact prompt rendering: an import line, one line per tool, then the line of
    /// `ToolContent` when a tool returns MCP content.
    pub prompt: String,
    /// Each tool on its own, sorted by name: what `search_tools` returns.
    pub tools: Vec<ToolDeclaration>,
}

/// One tool's declarations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolDeclaration {
    /// As scripts call it: `github.list_issues`, `ask_user`, `http`.
    pub name: String,
    /// The tool's line of the prompt rendering.
    pub prompt: String,
    /// The tool's declaration in `ptc-tools.d.ts`, with the types it uses, as a module of its
    /// own. For a built-in, its declaration in the sandbox lib.
    pub typescript: String,
}

/// Generates the declarations of a toolset. `observed` holds the shapes observed in the
/// recorded results of its tools, by tool name (`ReadModel::observed_shapes`); they type the
/// results of the tools that declare no output schema.
///
/// A bare host tool whose name is not an identifier cannot be imported from `ptc:tools`: it is
/// left out, as the runtime leaves it out of the module.
pub fn generate(toolset: &Toolset, observed: &BTreeMap<String, Shape>) -> Declarations {
    let exports = model::exports(toolset, observed);
    let typescript = dts::module(&toolset.digest().short(), &exports);

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
    for tool in toolset.tools() {
        if let Some(builtin) = prompt::builtin_of(tool) {
            tools.push(ToolDeclaration {
                name: tool.name.clone(),
                prompt: prompt::builtin(builtin),
                typescript: builtin_declaration(builtin).to_owned(),
            });
        }
    }
    tools.sort_by(|a, b| a.name.cmp(&b.name));

    let mut lines: Vec<String> = prompt::imports(&exports).into_iter().collect();
    lines.extend(tools.iter().map(|tool| tool.prompt.clone()));
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
fn builtin_declaration(builtin: &ptc_core::Builtin) -> &'static str {
    match builtin {
        ptc_core::Builtin::Http { .. } => {
            "declare function fetch(input: string | URL, init?: RequestInit): Promise<Response>;\n"
        }
        ptc_core::Builtin::Webhook => {
            "declare module \"ptc\" {\n  export function webhook(name: string): Promise<{ url: string }>;\n}\n"
        }
    }
}
