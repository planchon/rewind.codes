//! The toolset as `ptc:tools` exports it: one namespace per MCP server or host-tool group, one
//! function per bare host tool, and the type of each tool's args and result.

use std::collections::BTreeMap;

use ptc_core::{Shape, Tool, ToolRoute, Toolset};

use crate::observed;
use crate::schema::{self, Role, Scope};
use crate::ty::{Doc, Ty, is_identifier};

/// One export of `ptc:tools`.
#[derive(Debug)]
pub(crate) struct Export<'a> {
    /// The name scripts import: the namespace, or the bare tool's name.
    pub(crate) name: &'a str,
    pub(crate) kind: ExportKind,
    /// The named types of its tools.
    pub(crate) scope: Scope,
    /// Its tools, sorted by name. A bare tool's export has exactly one.
    pub(crate) tools: Vec<Signature<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportKind {
    /// A namespace object: `github.list_issues(…)`.
    Namespace,
    /// A function: `ask_user(…)`.
    Function,
}

/// A tool's signature.
#[derive(Debug)]
pub(crate) struct Signature<'a> {
    pub(crate) tool: &'a Tool,
    pub(crate) args: Ty,
    /// The script may leave the args out: no property is required. The call sends `{}`.
    pub(crate) args_optional: bool,
    pub(crate) result: Ty,
    /// The result type was inferred from recorded results, not declared.
    pub(crate) observed: bool,
    pub(crate) doc: Doc,
}

/// The exports of a toolset, sorted by name, built-ins left out: scripts reach them through
/// `fetch` and `webhook` from `ptc`. A bare host tool whose name is not an identifier cannot be
/// imported, and is left out too.
pub(crate) fn exports<'a>(
    toolset: &'a Toolset,
    observed: &BTreeMap<String, Shape>,
) -> Vec<Export<'a>> {
    let mut exports: BTreeMap<&str, Export<'a>> = BTreeMap::new();
    for tool in toolset.tools() {
        let (name, kind) = match (&tool.route, tool.namespace()) {
            (ToolRoute::Builtin(_), _) => continue,
            (_, Some(namespace)) => (namespace, ExportKind::Namespace),
            (_, None) if is_identifier(&tool.name) => (tool.name.as_str(), ExportKind::Function),
            (_, None) => continue,
        };
        let export = exports.entry(name).or_insert_with(|| Export {
            name,
            kind,
            scope: Scope::default(),
            tools: Vec::new(),
        });
        let signature = signature(&mut export.scope, tool, observed.get(&tool.name));
        export.tools.push(signature);
    }
    exports.into_values().collect()
}

fn signature<'a>(scope: &mut Scope, tool: &'a Tool, observed: Option<&Shape>) -> Signature<'a> {
    let local = tool.local_name();
    let args = schema::convert(scope, &tool.input_schema, local, Role::Args);
    let args_optional = args_optional(&args, scope);
    let (result, observed) = match &tool.output_schema {
        Some(output) => (schema::convert(scope, output, local, Role::Result), false),
        None => match (&tool.route, observed) {
            (ToolRoute::Mcp { .. }, Some(shape)) => match observed::structured_content(shape) {
                Some(structured) => (Ty::Content(Box::new(observed::type_of(structured))), true),
                None => (Ty::Content(Box::new(Ty::Unknown)), false),
            },
            (ToolRoute::Mcp { .. }, None) => (Ty::Content(Box::new(Ty::Unknown)), false),
            (_, Some(shape)) if *shape != Shape::Unknown => (observed::type_of(shape), true),
            (_, _) => (Ty::Unknown, false),
        },
    };
    let mut doc = Doc {
        description: tool
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        tags: Vec::new(),
    };
    if observed {
        doc.tags.push(
            "@observed The result type was inferred from recorded results, not declared by the \
             tool: it may be incomplete."
                .to_owned(),
        );
    }
    Signature {
        tool,
        args,
        args_optional,
        result,
        observed,
        doc,
    }
}

/// Whether args of this type may be left out: anything goes, or no property is required.
fn args_optional(ty: &Ty, scope: &Scope) -> bool {
    match ty {
        Ty::Unknown => true,
        Ty::Object(object) => object.properties.iter().all(|property| property.optional),
        Ty::Named(name) => match scope.get(name).map(|declaration| &declaration.ty) {
            Some(Ty::Object(object)) => object.properties.iter().all(|property| property.optional),
            _ => false,
        },
        _ => false,
    }
}

impl Export<'_> {
    /// The named types a signature uses, transitively, in the order of the scope.
    pub(crate) fn types_used_by(&self, signature: &Signature<'_>) -> Vec<String> {
        let mut used = Vec::new();
        signature.args.named(&mut used);
        signature.result.named(&mut used);
        let mut next = 0;
        while next < used.len() {
            if let Some(declaration) = self.scope.get(&used[next]) {
                declaration.ty.named(&mut used);
            }
            next += 1;
        }
        self.scope
            .declarations
            .iter()
            .filter(|declaration| used.contains(&declaration.name))
            .map(|declaration| declaration.name.clone())
            .collect()
    }

    /// The named type's body, for the prompt's rendering.
    pub(crate) fn body(&self, name: &str) -> Option<&Ty> {
        self.scope.get(name).map(|declaration| &declaration.ty)
    }

    /// Who the namespace's tools belong to, for its comment.
    pub(crate) fn owner(&self) -> String {
        let mcp = self
            .tools
            .iter()
            .all(|signature| matches!(signature.tool.route, ToolRoute::Mcp { .. }));
        let host = self
            .tools
            .iter()
            .all(|signature| matches!(signature.tool.route, ToolRoute::Host { .. }));
        if mcp {
            format!("Tools of the MCP server `{}`.", self.name)
        } else if host {
            format!("Tools of the harness, group `{}`.", self.name)
        } else {
            format!("Tools of `{}`.", self.name)
        }
    }
}
