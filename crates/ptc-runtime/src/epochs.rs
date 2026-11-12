//! The JavaScript embedded in the binary, one version per supported determinism epoch
//! (`archi/03-execution.md`, "Determinism epoch").
//!
//! Changing anything here that a script can observe (a `seq`, a digest, a value, a global)
//! requires a new epoch: a new `js/epochN/` directory and a new entry in [`EPOCHS`], while the
//! previous epochs stay as they are for the runs that started under them. The epoch corpus
//! (`ptc-e2e`) fails the build when a supported epoch stops replaying.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use ptc_core::{Epoch, Tool, ToolRoute, Toolset};

/// The glue of one epoch.
#[derive(Debug)]
pub(crate) struct Glue {
    pub(crate) epoch: Epoch,
    /// Runs once per isolate, before any module: deterministic globals, removed capabilities,
    /// and the hooks Rust drives the script with.
    pub(crate) bootstrap: &'static str,
    /// `ptc:entry`, the module the runtime loads.
    pub(crate) entry: &'static str,
    /// `ptc:internal`, the hand-over from the bootstrap to the `ptc` modules.
    pub(crate) internal: &'static str,
    /// `ptc`, the standard library of scripts.
    pub(crate) std: &'static str,
    /// The names `ptc` exports, checked when a script is compiled.
    pub(crate) std_exports: &'static [&'static str],
    /// Generates `ptc:tools` from the run's toolset.
    pub(crate) tools: fn(&Toolset) -> String,
    /// The names `ptc:tools` exports for a toolset, checked when a script is compiled.
    pub(crate) tool_exports: fn(&Toolset) -> BTreeSet<String>,
}

const EPOCH_1: Glue = Glue {
    epoch: Epoch(1),
    bootstrap: include_str!("js/epoch1/bootstrap.js"),
    entry: include_str!("js/epoch1/entry.js"),
    internal: include_str!("js/epoch1/internal.js"),
    std: include_str!("js/epoch1/std.js"),
    std_exports: &[
        "CancelledError",
        "ToolError",
        "run",
        "signal",
        "sleep",
        "webhook",
    ],
    tools: tools_module_v1,
    tool_exports: tool_exports_v1,
};

/// Every supported epoch, oldest first. New runs start under the last one.
pub(crate) const EPOCHS: &[Glue] = &[EPOCH_1];

pub(crate) fn glue(epoch: Epoch) -> Option<&'static Glue> {
    EPOCHS.iter().find(|glue| glue.epoch == epoch)
}

/// The epoch new runs start under.
pub(crate) fn latest() -> &'static Glue {
    const { &EPOCHS[EPOCHS.len() - 1] }
}

/// The tools of a toolset as `ptc:tools` exposes them in epoch 1, from their routes: one
/// namespace per MCP server or host-tool group, and the host tools without a group by name.
/// The built-ins are not exported: scripts reach them through `fetch` and `webhook`.
struct ToolExportsV1<'a> {
    namespaces: BTreeMap<&'a str, Vec<&'a Tool>>,
    bare: Vec<&'a Tool>,
}

impl<'a> ToolExportsV1<'a> {
    fn of(toolset: &'a Toolset) -> Self {
        let mut namespaces: BTreeMap<&str, Vec<&Tool>> = BTreeMap::new();
        let mut bare = Vec::new();
        for tool in toolset.tools() {
            if matches!(tool.route, ToolRoute::Builtin(_)) {
                continue;
            }
            match tool.namespace() {
                Some(namespace) => namespaces.entry(namespace).or_default().push(tool),
                None if is_identifier(&tool.name) => bare.push(tool),
                // Unreachable from a script: there is no name to import it by.
                None => {}
            }
        }
        Self { namespaces, bare }
    }
}

fn tool_exports_v1(toolset: &Toolset) -> BTreeSet<String> {
    let exports = ToolExportsV1::of(toolset);
    let namespaces = exports.namespaces.keys().copied();
    let bare = exports.bare.iter().map(|tool| tool.name.as_str());
    namespaces.chain(bare).map(str::to_owned).collect()
}

/// The function of `ptc:internal` that calls a tool: an MCP tool pinned without an output
/// schema returns its content, which gets the `text()` and `json()` helpers of `ToolContent`.
fn caller(tool: &Tool) -> &'static str {
    match tool.route {
        ToolRoute::Mcp { .. } if tool.output_schema.is_none() => "content",
        _ => "call",
    }
}

/// `ptc:tools` for epoch 1: one frozen namespace object per MCP server or host-tool group, and
/// one function per host tool without a group. Tool names are kept as the server defines them;
/// a name that is not an identifier is still reachable with brackets (`github["close-issue"]`).
fn tools_module_v1(toolset: &Toolset) -> String {
    let ToolExportsV1 { namespaces, bare } = ToolExportsV1::of(toolset);
    let mut module = String::from(
        "import hooks from \"ptc:internal\";\n\
         const call = hooks.callTool;\n\
         const content = hooks.callToolContent;\n",
    );
    for (namespace, tools) in &namespaces {
        let _ = writeln!(module, "export const {namespace} = Object.freeze({{");
        for tool in tools {
            let name = &tool.name[namespace.len() + 1..];
            let _ = writeln!(
                module,
                "  {}: (args) => {}({}, args),",
                js_string(name),
                caller(tool),
                js_string(&tool.name)
            );
        }
        module.push_str("});\n");
    }
    for tool in bare {
        let _ = writeln!(
            module,
            "export const {} = (args) => {}({}, args);",
            tool.name,
            caller(tool),
            js_string(&tool.name)
        );
    }
    module
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn js_string(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A pinned toolset only has namespaces that are identifiers (`ptc_core::Toolset::new`).
    #[test]
    fn generates_namespaces_and_bare_tools() {
        let module = tools_module_v1(
            &Toolset::from_host_tools(["github.list_issues", "github.close-issue", "ask_user"])
                .unwrap(),
        );
        assert_eq!(
            module,
            "import hooks from \"ptc:internal\";\n\
             const call = hooks.callTool;\n\
             const content = hooks.callToolContent;\n\
             export const github = Object.freeze({\n  \
               \"close-issue\": (args) => call(\"github.close-issue\", args),\n  \
               \"list_issues\": (args) => call(\"github.list_issues\", args),\n\
             });\n\
             export const ask_user = (args) => call(\"ask_user\", args);\n"
        );
        let exports: Vec<String> = tool_exports_v1(
            &Toolset::from_host_tools(["github.list_issues", "github.close-issue", "ask_user"])
                .unwrap(),
        )
        .into_iter()
        .collect();
        assert_eq!(exports, ["ask_user", "github"]);
    }

    /// The names the compiler accepts are exactly what `std.js` exports.
    #[test]
    fn std_exports_are_those_of_the_module() {
        for glue in EPOCHS {
            let line = glue
                .std
                .lines()
                .find(|line| line.starts_with("export const {"))
                .expect("std exports its names in one destructuring");
            let inner = &line["export const {".len()..line.find('}').expect("closing brace")];
            let mut names: Vec<&str> = inner.split(',').map(str::trim).collect();
            names.sort_unstable();
            assert_eq!(names, glue.std_exports, "{}", glue.epoch);
        }
    }

    #[test]
    fn built_ins_are_not_exported() {
        use ptc_core::{Builtin, McpServer, McpTransport};
        let github = McpServer::new(
            "github",
            McpTransport::Http {
                url: "https://mcp.example.com/github".into(),
            },
        );
        let mut get_issue = Tool::new(
            "github.get_issue",
            ToolRoute::Mcp {
                server: "github".into(),
            },
            serde_json::json!({}),
        );
        get_issue.output_schema = Some(serde_json::json!({ "type": "object" }));
        let toolset = Toolset::new(
            [github],
            [
                get_issue,
                Tool::new(
                    "github.list_issues",
                    ToolRoute::Mcp {
                        server: "github".into(),
                    },
                    serde_json::json!({}),
                ),
                Tool::new(
                    "http",
                    ToolRoute::Builtin(Builtin::Http {
                        allowed_hosts: vec!["api.example.com".into()],
                    }),
                    serde_json::json!({}),
                ),
                Tool::new(
                    "webhook",
                    ToolRoute::Builtin(Builtin::Webhook),
                    serde_json::json!({}),
                ),
            ],
        )
        .unwrap();
        let exports: Vec<String> = tool_exports_v1(&toolset).into_iter().collect();
        assert_eq!(exports, ["github"]);
        let module = tools_module_v1(&toolset);
        assert!(module.contains("export const github"), "{module}");
        // An MCP tool without an output schema returns its content, with the helpers; with
        // one, its structured content.
        assert!(
            module.contains("\"get_issue\": (args) => call(\"github.get_issue\", args),"),
            "{module}"
        );
        assert!(
            module.contains("\"list_issues\": (args) => content(\"github.list_issues\", args),"),
            "{module}"
        );
        assert!(
            !module.contains("\"http\"") && !module.contains("webhook"),
            "{module}"
        );
    }

    #[test]
    fn epochs_are_ordered_and_unique() {
        assert!(EPOCHS.windows(2).all(|pair| pair[0].epoch < pair[1].epoch));
        assert!(glue(Epoch(1)).is_some());
        assert!(glue(Epoch(0)).is_none());
    }
}
