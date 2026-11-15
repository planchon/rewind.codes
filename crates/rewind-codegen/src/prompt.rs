//! The compact prompt rendering: the same declarations, one line per tool, for the model.
//!
//! ```text
//! import { ask_user, github } from "rewind:tools";
//! github.list_issues({ repo: string, state?: "open" | "closed" }) → { number: number, title: string }[] · List the issues of a repository.
//! ask_user({ question: string }) → unknown · Ask the end user a question.
//! fetch(url, { method?, headers?, body? }) → Response · The http tool, for these hosts only: api.example.com.
//! ToolContent<T> = { content: ContentBlock[], structuredContent?: T, text(): string, json(): T } · What an MCP tool without an output schema returns.
//! ```
//!
//! Named types are written out where they are used, as the prompt has no other place for them.
//! Arguments and results longer than [`BUDGET`] characters are shortened: deeper objects
//! become `{…}`, then named types keep their names, then arguments keep only their names.

use rewind_core::{Builtin, Tool, ToolRoute};

use crate::model::{Export, ExportKind, Signature};
use crate::ty::{Compact, Ty, is_identifier, string_literal};

/// Characters of arguments, or of a result, before they are shortened.
const BUDGET: usize = 160;
/// Characters of a tool's description in its line.
const DESCRIPTION: usize = 200;

pub(crate) const CONTENT_LINE: &str = "ToolContent<T> = { content: ContentBlock[], structuredContent?: T, text(): string, json(): T } · What an MCP tool without an output schema returns: its content blocks (text, image, audio, resource), with helpers.";

/// The line that imports every export.
pub(crate) fn imports(exports: &[Export<'_>]) -> Option<String> {
    if exports.is_empty() {
        return None;
    }
    let names: Vec<&str> = exports.iter().map(|export| export.name).collect();
    Some(format!(
        "import {{ {} }} from \"rewind:tools\";",
        names.join(", ")
    ))
}

/// A tool's line.
pub(crate) fn tool(export: &Export<'_>, signature: &Signature<'_>) -> String {
    let named = |name: &str| export.body(name);
    let compact = Compact { named: &named };
    let callee = match export.kind {
        ExportKind::Function => export.name.to_owned(),
        ExportKind::Namespace => {
            let local = signature.tool.local_name();
            if is_identifier(local) {
                format!("{}.{local}", export.name)
            } else {
                format!("{}[{}]", export.name, string_literal(local))
            }
        }
    };
    let args = arguments(&compact, export, &signature.args);
    let result = shortest(|depth| compact.render(&signature.result, depth), 3);
    let observed = if signature.observed {
        " (observed)"
    } else {
        ""
    };
    let mut line = format!("{callee}({args}) → {result}{observed}");
    if let Some(description) = summary(signature.tool) {
        line.push_str(" · ");
        line.push_str(&description);
    }
    line
}

/// The line of a built-in: `fetch` for `http`, `webhook` from `rewind`.
pub(crate) fn builtin(builtin: &Builtin) -> String {
    match builtin {
        Builtin::Http { allowed_hosts } => format!(
            "fetch(url, {{ method?, headers?, body? }}) → Response · The http tool, a global: \
             HTTP requests to these hosts only: {}.",
            allowed_hosts.join(", ")
        ),
        Builtin::Webhook => "webhook(name) → { url: string } · From \"rewind\": a URL bound to \
                             this run; what is posted to it becomes the signal `name`."
            .to_owned(),
        // Never listed: a workflow's built-ins are reached through `rewind:workflows`.
        Builtin::Workflow { workflow, .. } => format!("{workflow}.start(input) → run"),
    }
}

fn arguments(compact: &Compact<'_>, export: &Export<'_>, args: &Ty) -> String {
    let object = match args {
        Ty::Object(object) => Some(object),
        Ty::Named(name) => match export.body(name) {
            Some(Ty::Object(object)) => Some(object),
            _ => None,
        },
        _ => None,
    };
    match object {
        Some(object) if object.properties.is_empty() => match object.index.as_deref() {
            None => String::new(),
            Some(Ty::Unknown) => "args?".to_owned(),
            Some(index) => format!("args: {{ [key: string]: {} }}", compact.render(index, 1)),
        },
        Some(object) => {
            for depth in (0..=2).rev() {
                let text = compact.arguments(object, depth, true);
                if text.chars().count() <= BUDGET {
                    return text;
                }
            }
            compact.arguments(object, 0, false)
        }
        None if *args == Ty::Unknown => "args?".to_owned(),
        None => format!("args: {}", shortest(|depth| compact.render(args, depth), 2)),
    }
}

/// The first rendering, from `depth` down, that fits the budget; the last one otherwise.
fn shortest(render: impl Fn(usize) -> String, depth: usize) -> String {
    let mut text = String::new();
    for depth in (0..=depth).rev() {
        text = render(depth);
        if text.chars().count() <= BUDGET {
            break;
        }
    }
    text
}

/// The first paragraph of the tool's description, on one line, shortened.
fn summary(tool: &Tool) -> Option<String> {
    let description = tool.description.as_deref()?;
    let paragraph = description.trim().split("\n\n").next().unwrap_or_default();
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

/// Whether the route is one the prompt lists as a built-in.
pub(crate) fn builtin_of(tool: &Tool) -> Option<&Builtin> {
    match &tool.route {
        ToolRoute::Builtin(builtin) => Some(builtin),
        _ => None,
    }
}
