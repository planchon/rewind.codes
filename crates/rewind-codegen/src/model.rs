//! The action set as `rewind:actions` exports it: one namespace per group (an MCP server, an API or
//! a group of harness actions), one function per bare harness action, and the type of each action's
//! args and result.

use std::collections::BTreeMap;

use rewind_core::{Action, ActionRoute, ActionSet, Shape};

use crate::observed;
use crate::schema::{self, Declaration, Role, Scope};
use crate::ty::{Doc, Names, Ty, is_identifier};

/// One export of `rewind:actions`.
#[derive(Debug)]
pub(crate) struct Export<'a> {
    /// The name workflows import: the namespace, or the bare action's name.
    pub(crate) name: &'a str,
    pub(crate) kind: ExportKind,
    /// The named types of its actions.
    pub(crate) scope: Scope,
    /// Its actions, sorted by name. A bare action's export has exactly one.
    pub(crate) actions: Vec<Signature<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportKind {
    /// A namespace object: `github.list_issues(…)`.
    Namespace,
    /// A function: `ask_user(…)`.
    Function,
}

/// An action's signature.
#[derive(Debug)]
pub(crate) struct Signature<'a> {
    pub(crate) action: &'a Action,
    pub(crate) args: Ty,
    /// The code may leave the args out: no property is required. The call sends `{}`.
    pub(crate) args_optional: bool,
    pub(crate) result: Ty,
    /// The result type was inferred from recorded results, not declared.
    pub(crate) observed: bool,
    pub(crate) doc: Doc,
}

/// The exports of an action set, sorted by name, built-ins left out: workflows reach them through
/// `fetch` and `signalUrl` from `rewind`. A bare harness action whose name is not an identifier
/// cannot be imported, and is left out too.
pub(crate) fn exports<'a>(
    action_set: &'a ActionSet,
    observed: &BTreeMap<String, Shape>,
) -> Vec<Export<'a>> {
    let mut exports: BTreeMap<&str, Export<'a>> = BTreeMap::new();
    for action in action_set.actions() {
        let (name, kind) = match (&action.route, action.group()) {
            (ActionRoute::Builtin(_), _) => continue,
            (_, Some(namespace)) => (namespace, ExportKind::Namespace),
            (_, None) if is_identifier(&action.name) => {
                (action.name.as_str(), ExportKind::Function)
            }
            (_, None) => continue,
        };
        let export = exports.entry(name).or_insert_with(|| Export {
            name,
            kind,
            scope: Scope::default(),
            actions: Vec::new(),
        });
        let signature = signature(&mut export.scope, action, observed.get(&action.name));
        export.actions.push(signature);
    }
    exports.into_values().collect()
}

fn signature<'a>(scope: &mut Scope, action: &'a Action, observed: Option<&Shape>) -> Signature<'a> {
    let local = action.local_name();
    let args = schema::convert(scope, &action.input_schema, local, Role::Args);
    let args_optional = args_optional(&args, scope);
    let (result, observed) = match &action.output_schema {
        Some(output) => (schema::convert(scope, output, local, Role::Result), false),
        None => match (&action.route, observed) {
            (ActionRoute::Mcp { .. }, Some(shape)) => match observed::structured_content(shape) {
                Some(structured) => (Ty::Content(Box::new(observed::type_of(structured))), true),
                None => (Ty::Content(Box::new(Ty::Unknown)), false),
            },
            (ActionRoute::Mcp { .. }, None) => (Ty::Content(Box::new(Ty::Unknown)), false),
            (_, Some(shape)) if *shape != Shape::Unknown => (observed::type_of(shape), true),
            (_, _) => (Ty::Unknown, false),
        },
    };
    let mut doc = Doc {
        description: action
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
             action: it may be incomplete."
                .to_owned(),
        );
    }
    Signature {
        action,
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
    pub(crate) fn types_used_by(&self, signature: &Signature<'_>) -> Vec<&Declaration> {
        let mut used = Names::default();
        signature.args.named(&mut used);
        signature.result.named(&mut used);
        let mut next = 0;
        while next < used.order.len() {
            if let Some(declaration) = self.scope.get(&used.order[next]) {
                declaration.ty.named(&mut used);
            }
            next += 1;
        }
        self.scope
            .declarations
            .iter()
            .filter(|declaration| used.contains(&declaration.name))
            .collect()
    }

    /// The named type's body, for the prompt's rendering.
    pub(crate) fn body(&self, name: &str) -> Option<&Ty> {
        self.scope.get(name).map(|declaration| &declaration.ty)
    }

    /// Who the namespace's actions belong to, for its comment.
    pub(crate) fn owner(&self) -> String {
        let mcp = self
            .actions
            .iter()
            .all(|signature| matches!(signature.action.route, ActionRoute::Mcp { .. }));
        let host = self
            .actions
            .iter()
            .all(|signature| matches!(signature.action.route, ActionRoute::Harness { .. }));
        if mcp {
            format!("Actions of the MCP server `{}`.", self.name)
        } else if host {
            format!("Actions of the harness, group `{}`.", self.name)
        } else {
            format!("Actions of `{}`.", self.name)
        }
    }
}
