//! Code generation of Rewind (`archi/04-tools.md`, "Action sets and code generation").
//!
//! From a pinned action set, [`generate`] writes what the type-checker and the model read:
//!
//! | Output | Read by | Example |
//! | --- | --- | --- |
//! | `rewind-actions.d.ts`: the `rewind:actions` module, types for args (from `inputSchema`) and results (from `outputSchema`, else the MCP content type), action descriptions as JSDoc | tsgo | `list_issues(args: { repo: string; state?: "open" \| "closed" }): Promise<github.Issue[]>` |
//! | The compact prompt rendering, one line per action | The model | `github.list_issues({ repo: string, state?: "open" \| "closed" }) → Issue[] · List the issues` |
//!
//! The runtime module `rewind:actions` itself is part of the runtime's glue, kept per determinism
//! epoch. Codegen is deterministic: the same action set and observed shapes give the same bytes.
//!
//! The JSON Schema → TypeScript converter is ours: no maintained crate exists.
//! When an action declares no `outputSchema`, its result type comes from the shapes observed in
//! recorded results, when there are some, marked `@observed`: for a harness action, the result's
//! shape; for an MCP action, which returns its content, the shape of its `structuredContent`.
//!
//! `rewind-actions.d.ts` always declares the module `rewind:actions` (`declare module "rewind:actions" { … }`),
//! empty for an action set without actions. An action set that pins workflows also gets the module
//! `rewind:workflows`, one typed export per workflow, and a prompt line per workflow; the
//! declarations a workflow's own script is checked against come from
//! [`workflow_declarations`]. Built-ins are not part of it: scripts reach `http` through
//! `fetch` and `webhook` through the `rewind` module. For an action set that enables `http`, the file
//! starts with [`HTTP_ACTION_REFERENCE`], from which the type-checker knows to declare `fetch`.
//! The prompt lists the built-ins too, with the hosts `fetch` may reach.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use rewind_core::{ActionRoute, ActionSet, Builtin, Shape};

mod dts;
mod model;
mod observed;
mod prompt;
mod schema;
mod ty;
mod workflows;

pub use workflows::{WorkflowDeclarations, workflow_declarations};

/// The first line of the `rewind-actions.d.ts` of an action set that enables the `http` action. It is how
/// the type-checker learns that the script has `fetch`: it then adds `fetch.d.ts` to the check.
/// `rewind-check` exports the same line (`HTTP_ACTION_REFERENCE`); `archi/04-tools.md` records it.
pub const HTTP_ACTION_REFERENCE: &str = "/// <reference path=\"fetch.d.ts\" />";

/// What codegen generated for an action set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Declarations {
    /// `rewind-actions.d.ts`.
    pub typescript: String,
    /// The compact prompt rendering: an import line, one line per action, then the line of
    /// `ActionContent` when an action returns MCP content.
    pub prompt: String,
    /// Each action on its own, sorted by name: what `search_actions` returns.
    pub actions: Vec<ActionDeclaration>,
}

/// One action's declarations, or one pinned workflow's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionDeclaration {
    /// As scripts call it: `github.list_issues`, `ask_user`, `http`; `workflows.triage` for the
    /// workflow `triage` of `rewind:workflows`.
    pub name: String,
    /// The action's line of the prompt rendering.
    pub prompt: String,
    /// The action's declaration in `rewind-actions.d.ts`, with the types it uses, as a module of its
    /// own. For a built-in, its declaration in the sandbox lib.
    pub typescript: String,
}

/// Generates the declarations of an action set. `observed` holds the shapes observed in the
/// recorded results of its actions, by action name (`ReadModel::observed_shapes`); they type the
/// results of the actions that declare no output schema.
///
/// A bare harness action whose name is not an identifier cannot be imported from `rewind:actions`: it is
/// left out, as the runtime leaves it out of the module.
pub fn generate(action_set: &ActionSet, observed: &BTreeMap<String, Shape>) -> Declarations {
    let exports = model::exports(action_set, observed);
    let http_action = action_set
        .actions()
        .iter()
        .any(|action| matches!(action.route, ActionRoute::Builtin(Builtin::Http { .. })));
    let mut typescript = dts::module(&action_set.digest().short(), &exports, http_action);
    if !action_set.workflows().is_empty() {
        typescript.push('\n');
        typescript.push_str(&workflows::module(action_set.workflows()));
    }

    let mut actions = Vec::new();
    for export in &exports {
        for signature in &export.actions {
            actions.push(ActionDeclaration {
                name: signature.action.name.clone(),
                prompt: prompt::action(export, signature),
                typescript: dts::action(export, signature),
            });
        }
    }
    for action in action_set.actions() {
        if let Some(builtin) = prompt::builtin_of(action) {
            actions.push(ActionDeclaration {
                name: action.name.clone(),
                prompt: prompt::builtin(builtin),
                typescript: builtin_declaration(builtin).to_owned(),
            });
        }
    }
    actions.sort_by(|a, b| a.name.cmp(&b.name));
    let pinned: Vec<ActionDeclaration> = action_set
        .workflows()
        .iter()
        .map(|workflow| ActionDeclaration {
            name: format!("{}.{}", rewind_core::WORKFLOWS_GROUP, workflow.name),
            prompt: workflows::prompt(workflow),
            typescript: workflows::declaration(workflow),
        })
        .collect();

    let mut lines: Vec<String> = prompt::imports(&exports).into_iter().collect();
    lines.extend(workflows::imports(action_set.workflows()));
    lines.extend(actions.iter().map(|action| action.prompt.clone()));
    lines.extend(pinned.iter().map(|workflow| workflow.prompt.clone()));
    actions.extend(pinned);
    actions.sort_by(|a, b| a.name.cmp(&b.name));
    if typescript.contains("export interface ActionContent<") {
        lines.push(prompt::CONTENT_LINE.to_owned());
    }
    let mut prompt = lines.join("\n");
    if !prompt.is_empty() {
        prompt.push('\n');
    }

    Declarations {
        typescript,
        prompt,
        actions,
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
