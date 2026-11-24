//! The declarations of workflows (`archi/04-tools.md`, "Action sets and code generation" and
//! "Type-checking").
//!
//! For the workflows an action set pins, `rewind-actions.d.ts` declares `rewind:workflows`:
//!
//! ```ts
//! declare module "rewind:workflows" {
//!   import type { Workflow } from "rewind";
//!
//!   /** Triage the stale issues of a repository. (version 9f2c4e1a07b3) */
//!   export const triage: Workflow<
//!     { repo: string },
//!     { closed: number },
//!     { approval: { close: boolean } }
//!   >;
//! }
//! ```
//!
//! and the prompt gives each one a line:
//!
//! ```text
//! import { triage } from "rewind:workflows";
//! triage.start({ repo: string }) → run · run.signal("approval", { close: boolean }) · run.result() → { closed: number } · Triage…
//! ```
//!
//! For a workflow's own code, checked when it is registered, [`workflow_declarations`] gives
//! its signals (which type `signal()` and `webhook()`), and `entry.ts`, which assigns the code's
//! default export to a function from its input type to its output type.

use rewind_core::{PinnedWorkflow, WorkflowSchemas};

use crate::schema::{self, Declaration, Role, Scope};
use crate::ty::{Compact, Doc, Object, Property, Ty, jsdoc, property_name, render, string_literal};

/// The namespace of a workflow's named types in its own declarations: a global of the check.
const TYPES: &str = "RewindWorkflow";

/// Characters of a type in a prompt line before it is shortened.
const BUDGET: usize = 160;
/// Characters of a workflow's description in its line.
const DESCRIPTION: usize = 200;

/// A workflow's schemas as types: they share its named types.
struct Interface {
    scope: Scope,
    input: Ty,
    output: Ty,
    /// By signal name, sorted.
    signals: Vec<(String, Ty)>,
}

impl Interface {
    fn of(name: &str, schemas: &WorkflowSchemas) -> Self {
        let mut scope = Scope::default();
        let input = schema::convert(&mut scope, &schemas.input, name, Role::Args);
        let output = schema::convert(&mut scope, &schemas.output, name, Role::Result);
        let signals = schemas
            .signals
            .iter()
            .map(|(signal, schema)| {
                (
                    signal.clone(),
                    schema::convert(&mut scope, schema, signal, Role::Args),
                )
            })
            .collect();
        Self {
            scope,
            input,
            output,
            signals,
        }
    }

    /// The signals as one object type: `{ approval: { close: boolean } }`.
    fn signals_type(&self) -> Ty {
        Ty::Object(Object {
            properties: self
                .signals
                .iter()
                .map(|(name, ty)| Property {
                    name: name.clone(),
                    ty: ty.clone(),
                    optional: false,
                    doc: Doc::default(),
                })
                .collect(),
            index: None,
        })
    }

    /// The body of a named type.
    fn body(&self, name: &str) -> Option<&Ty> {
        self.scope.get(name).map(|declaration| &declaration.ty)
    }
}

/// The module `rewind:workflows` for these workflows: one export each. Empty for an action set
/// that pins none, so that the module exists and exports nothing.
pub(crate) fn module(workflows: &[PinnedWorkflow]) -> String {
    let mut out = String::from("declare module \"rewind:workflows\" {\n");
    if !workflows.is_empty() {
        out.push_str("  import type { Workflow } from \"rewind\";\n");
    }
    for workflow in workflows {
        out.push('\n');
        write_workflow(&mut out, workflow);
    }
    out.push_str("}\n");
    out
}

/// One workflow's declaration, as a module of its own: what `search_actions` returns.
pub(crate) fn declaration(workflow: &PinnedWorkflow) -> String {
    let mut out = String::from(
        "declare module \"rewind:workflows\" {\n  import type { Workflow } from \"rewind\";\n\n",
    );
    write_workflow(&mut out, workflow);
    out.push_str("}\n");
    out
}

fn write_workflow(out: &mut String, workflow: &PinnedWorkflow) {
    let interface = Interface::of(&workflow.name, &workflow.schemas);
    let qualifier = (!interface.scope.declarations.is_empty()).then_some(workflow.name.as_str());
    let doc = Doc {
        description: Some(match workflow.description.as_deref().map(str::trim) {
            Some(description) if !description.is_empty() => {
                format!("{description}\n\nVersion {}.", workflow.version.short())
            }
            _ => format!(
                "The workflow {}, version {}.",
                workflow.name,
                workflow.version.short()
            ),
        }),
        tags: Vec::new(),
    };
    jsdoc(out, &doc, 1);
    out.push_str(&format!(
        "  export const {}: Workflow<\n    {},\n    {},\n    {}\n  >;\n",
        workflow.name,
        render(&interface.input, qualifier, 2),
        render(&interface.output, qualifier, 2),
        render(&interface.signals_type(), qualifier, 2),
    ));
    write_types(out, &workflow.name, &interface.scope.declarations, 1);
}

/// `export namespace <name> { … }` with the named types, at `indent`.
fn write_types(out: &mut String, namespace: &str, declarations: &[Declaration], indent: usize) {
    if declarations.is_empty() {
        return;
    }
    let pad = "  ".repeat(indent);
    out.push_str(&format!("{pad}export namespace {namespace} {{\n"));
    for (index, declaration) in declarations.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        jsdoc(out, &declaration.doc, indent + 1);
        match &declaration.ty {
            Ty::Object(object) => out.push_str(&format!(
                "{pad}  export interface {} {}\n",
                declaration.name,
                crate::ty::block(object, None, indent + 1)
            )),
            ty => out.push_str(&format!(
                "{pad}  export type {} = {};\n",
                declaration.name,
                render(ty, None, indent + 1)
            )),
        }
    }
    out.push_str(&format!("{pad}}}\n"));
}

/// The line that imports every workflow, if there is one.
pub(crate) fn imports(workflows: &[PinnedWorkflow]) -> Option<String> {
    if workflows.is_empty() {
        return None;
    }
    let names: Vec<&str> = workflows
        .iter()
        .map(|workflow| workflow.name.as_str())
        .collect();
    Some(format!(
        "import {{ {} }} from \"rewind:workflows\";",
        names.join(", ")
    ))
}

/// A workflow's line of the prompt: how to start it, signal its runs and read their result.
pub(crate) fn prompt(workflow: &PinnedWorkflow) -> String {
    let interface = Interface::of(&workflow.name, &workflow.schemas);
    let named = |name: &str| interface.body(name);
    let compact = Compact { named: &named };
    let shortest = |ty: &Ty| {
        let mut text = String::new();
        for depth in (0..=2).rev() {
            text = match ty {
                Ty::Object(object) => compact.arguments(object, depth, true),
                ty => compact.render(ty, depth),
            };
            if text.chars().count() <= BUDGET {
                break;
            }
        }
        text
    };
    let mut parts = vec![format!(
        "{}.start({}) → run",
        workflow.name,
        shortest(&interface.input)
    )];
    for (signal, ty) in &interface.signals {
        parts.push(format!(
            "run.signal({}, {})",
            string_literal(signal),
            shortest(ty)
        ));
    }
    parts.push(format!("run.result() → {}", shortest(&interface.output)));
    if let Some(description) = summary(workflow.description.as_deref()) {
        parts.push(description);
    }
    parts.join(" · ")
}

/// The first paragraph of a description, on one line, shortened.
fn summary(description: Option<&str>) -> Option<String> {
    let paragraph = description?.trim().split("\n\n").next().unwrap_or_default();
    let line = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    if line.chars().count() <= DESCRIPTION {
        return Some(line);
    }
    let mut cut: String = line.chars().take(DESCRIPTION).collect();
    if let Some(space) = cut.rfind(' ') {
        cut.truncate(space);
    }
    cut.push('…');
    Some(cut)
}

/// What a workflow's own code is checked against when it is registered, besides its
/// action set's declarations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkflowDeclarations {
    /// `rewind-workflow.d.ts`: the `Signals` of the `rewind` module, and the named types of the
    /// workflow's schemas in the global namespace `RewindWorkflow`.
    pub signals: String,
    /// `entry.ts`: assigns the code's default export to a function from the input type to
    /// the output type.
    pub entry: String,
}

/// The declarations a workflow's code is checked against: its signals, and its entry.
pub fn workflow_declarations(name: &str, schemas: &WorkflowSchemas) -> WorkflowDeclarations {
    let interface = Interface::of(name, schemas);
    let qualifier = (!interface.scope.declarations.is_empty()).then_some(TYPES);
    let mut signals = format!(
        "// The signals of the workflow {name}, generated by rewind-codegen: do not edit.\n\n"
    );
    if !interface.scope.declarations.is_empty() {
        let mut types = String::new();
        write_types(&mut types, TYPES, &interface.scope.declarations, 0);
        signals.push_str(&types.replacen("export namespace", "declare namespace", 1));
        signals.push('\n');
    }
    signals.push_str("declare module \"rewind\" {\n  export interface Signals {\n");
    for (signal, ty) in &interface.signals {
        signals.push_str(&format!(
            "    {}: {};\n",
            property_name(signal),
            render(ty, qualifier, 2)
        ));
    }
    signals.push_str("  }\n}\n");
    let entry = format!(
        "// Checks that the default export of the workflow {name} takes its input and returns its\n\
         // output, as its schemas say. Generated by rewind-codegen: do not edit.\n\
         import main from \"./workflow\";\n\n\
         type Input = {};\n\
         type Output = {};\n\n\
         const entry: (input: Input) => Output | Promise<Output> = main;\n\
         export {{ entry }};\n",
        render(&interface.input, qualifier, 0),
        render(&interface.output, qualifier, 0),
    );
    WorkflowDeclarations { signals, entry }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rewind_core::{Digest, Workflow};
    use serde_json::json;

    use super::*;

    fn triage() -> PinnedWorkflow {
        Workflow::new(
            "triage",
            Some("Triage the stale issues of a repository, after approval.".into()),
            Digest::of(b"script"),
            Digest::of(b"action_set"),
            WorkflowSchemas {
                input: json!({
                    "type": "object",
                    "properties": { "repo": { "type": "string" } },
                    "required": ["repo"],
                }),
                output: json!({
                    "type": "object",
                    "properties": { "closed": { "type": "integer" } },
                    "required": ["closed"],
                }),
                signals: BTreeMap::from([
                    (
                        "approval".to_owned(),
                        json!({
                            "type": "object",
                            "properties": { "close": { "type": "boolean" } },
                            "required": ["close"],
                        }),
                    ),
                    ("pr merged".to_owned(), json!(true)),
                ]),
            },
        )
        .unwrap()
        .pinned()
    }

    #[test]
    fn a_pinned_workflow_is_a_typed_export() {
        let workflow = triage();
        let module = module(std::slice::from_ref(&workflow));
        let version = workflow.version.short();
        assert_eq!(
            module,
            format!(
                "declare module \"rewind:workflows\" {{\n  import type {{ Workflow }} from \"rewind\";\n\n  \
                 /**\n   * Triage the stale issues of a repository, after approval.\n   *\n   * Version {version}.\n   */\n  \
                 export const triage: Workflow<\n    {{ repo: string }},\n    {{ closed: number }},\n    \
                 {{ approval: {{ close: boolean }}; \"pr merged\": unknown }}\n  >;\n}}\n"
            )
        );
        assert_eq!(
            super::module(&[]),
            "declare module \"rewind:workflows\" {\n}\n"
        );
        assert_eq!(
            prompt(&workflow),
            "triage.start({ repo: string }) → run · run.signal(\"approval\", { close: boolean }) · \
             run.signal(\"pr merged\", unknown) · run.result() → { closed: number } · Triage the \
             stale issues of a repository, after approval."
        );
        assert_eq!(
            imports(&[workflow]).as_deref(),
            Some("import { triage } from \"rewind:workflows\";")
        );
    }

    #[test]
    fn a_workflow_s_own_declarations_type_its_signals_and_its_entry() {
        let workflow = triage();
        let declarations = workflow_declarations("triage", &workflow.schemas);
        assert!(
            declarations.signals.ends_with(
                "declare module \"rewind\" {\n  export interface Signals {\n    approval: { close: boolean };\n    \"pr merged\": unknown;\n  }\n}\n"
            ),
            "{}",
            declarations.signals
        );
        assert!(
            declarations
                .entry
                .contains("type Input = { repo: string };"),
            "{}",
            declarations.entry
        );
        assert!(
            declarations
                .entry
                .contains("const entry: (input: Input) => Output | Promise<Output> = main;"),
            "{}",
            declarations.entry
        );
    }

    #[test]
    fn named_types_live_in_the_workflow_s_namespace() {
        let mut workflow = triage();
        workflow.schemas.output = json!({
            "$defs": { "Issue": { "type": "object", "properties": { "number": { "type": "integer" } } } },
            "type": "array",
            "items": { "$ref": "#/$defs/Issue" },
        });
        let module = module(std::slice::from_ref(&workflow));
        assert!(module.contains("triage.Issue[]"), "{module}");
        assert!(
            module.contains("  export namespace triage {\n    export interface Issue {"),
            "{module}"
        );
        let declarations = workflow_declarations("triage", &workflow.schemas);
        assert!(
            declarations.entry.contains("RewindWorkflow.Issue[]"),
            "{}",
            declarations.entry
        );
        assert!(
            declarations
                .signals
                .contains("declare namespace RewindWorkflow {"),
            "{}",
            declarations.signals
        );
    }
}
