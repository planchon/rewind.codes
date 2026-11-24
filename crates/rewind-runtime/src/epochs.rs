//! The JavaScript embedded in the binary, one version per supported determinism epoch
//! (`archi/03-execution.md`, "Determinism epoch").
//!
//! Changing anything here that a code can observe (a `seq`, a digest, a value, a global)
//! requires a new epoch: a new `js/epochN/` directory and a new event in [`EPOCHS`], while the
//! previous epochs stay as they are for the runs that started under them. The epoch corpus
//! (`rewind-e2e`) fails the build when a supported epoch stops replaying.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use rewind_core::{Action, ActionRoute, ActionSet, Epoch};

/// The glue of one epoch.
#[derive(Debug)]
pub(crate) struct Glue {
    pub(crate) epoch: Epoch,
    /// Runs once per isolate, before any module: deterministic globals, removed capabilities,
    /// and the hooks Rust drives the code with.
    pub(crate) bootstrap: &'static str,
    /// `rewind:event`, the module the runtime loads.
    pub(crate) entry: &'static str,
    /// `rewind:internal`, the hand-over from the bootstrap to the `rewind` modules.
    pub(crate) internal: &'static str,
    /// `rewind`, the standard library of workflows.
    pub(crate) std: &'static str,
    /// The names `rewind` exports, checked when a code is compiled.
    pub(crate) std_exports: &'static [&'static str],
    /// The old names `rewind` still exports for code already written, accepted when a code is
    /// compiled but never declared nor listed (`archi/08-decisions.md`, R1).
    pub(crate) std_aliases: &'static [&'static str],
    /// Generates `rewind:actions` from the run's action set.
    pub(crate) actions: fn(&ActionSet) -> String,
    /// The names `rewind:actions` exports for an action set, checked when a code is compiled.
    pub(crate) action_exports: fn(&ActionSet) -> BTreeSet<String>,
    /// Generates `rewind:workflows` from the run's action set.
    pub(crate) workflows: fn(&ActionSet) -> String,
    /// The names `rewind:workflows` exports for an action set, checked when a code is compiled.
    pub(crate) workflow_exports: fn(&ActionSet) -> BTreeSet<String>,
}

const EPOCH_1: Glue = Glue {
    epoch: Epoch(1),
    bootstrap: include_str!("js/epoch1/bootstrap.js"),
    entry: include_str!("js/epoch1/entry.js"),
    internal: include_str!("js/epoch1/internal.js"),
    std: include_str!("js/epoch1/std.js"),
    std_exports: &[
        "ActionError",
        "CancelledError",
        "RunError",
        "run",
        "signal",
        "sleep",
        "webhook",
    ],
    std_aliases: &["ToolError", "WorkflowError"],
    actions: actions_module_v1,
    action_exports: action_exports_v1,
    workflows: workflows_module_v1,
    workflow_exports: workflow_exports_v1,
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

/// The actions of an action set as `rewind:actions` exposes them in epoch 1, from their routes: one
/// object per group (an MCP server, an API or a group of harness actions), and the harness actions
/// without a group by name. The built-ins are not exported: workflows reach them through `fetch` and
/// `webhook`.
struct ActionExportsV1<'a> {
    groups: BTreeMap<&'a str, Vec<&'a Action>>,
    bare: Vec<&'a Action>,
}

impl<'a> ActionExportsV1<'a> {
    fn of(action_set: &'a ActionSet) -> Self {
        let mut groups: BTreeMap<&str, Vec<&Action>> = BTreeMap::new();
        let mut bare = Vec::new();
        for action in action_set.actions() {
            if matches!(action.route, ActionRoute::Builtin(_)) {
                continue;
            }
            match action.group() {
                Some(group) => groups.entry(group).or_default().push(action),
                None if is_identifier(&action.name) => bare.push(action),
                // Unreachable from a code: there is no name to import it by.
                None => {}
            }
        }
        Self { groups, bare }
    }
}

fn action_exports_v1(action_set: &ActionSet) -> BTreeSet<String> {
    let exports = ActionExportsV1::of(action_set);
    let groups = exports.groups.keys().copied();
    let bare = exports.bare.iter().map(|action| action.name.as_str());
    groups.chain(bare).map(str::to_owned).collect()
}

/// The function of `rewind:internal` that calls an action: an MCP action pinned without an output
/// schema returns its content, which gets the `text()` and `json()` helpers of `ActionContent`.
fn caller(action: &Action) -> &'static str {
    match action.route {
        ActionRoute::Mcp { .. } if action.output_schema.is_none() => "content",
        _ => "call",
    }
}

/// `rewind:actions` for epoch 1: one frozen object per group (an MCP server, an API or a group of
/// harness actions), and one function per harness action without a group. Action names are kept as
/// the server defines them; a name that is not an identifier is still reachable with brackets
/// (`github["close-issue"]`).
fn actions_module_v1(action_set: &ActionSet) -> String {
    let ActionExportsV1 { groups, bare } = ActionExportsV1::of(action_set);
    let mut module = String::from(
        "import hooks from \"rewind:internal\";\n\
         const call = hooks.callAction;\n\
         const content = hooks.callActionContent;\n",
    );
    for (group, actions) in &groups {
        let _ = writeln!(module, "export const {group} = Object.freeze({{");
        for action in actions {
            let name = &action.name[group.len() + 1..];
            let _ = writeln!(
                module,
                "  {}: (args) => {}({}, args),",
                js_string(name),
                caller(action),
                js_string(&action.name)
            );
        }
        module.push_str("});\n");
    }
    for action in bare {
        let _ = writeln!(
            module,
            "export const {} = (args) => {}({}, args);",
            action.name,
            caller(action),
            js_string(&action.name)
        );
    }
    module
}

/// The names `rewind:workflows` exports in epoch 1: one per pinned workflow.
fn workflow_exports_v1(action_set: &ActionSet) -> BTreeSet<String> {
    action_set
        .workflows()
        .iter()
        .map(|workflow| workflow.name.clone())
        .collect()
}

/// `rewind:workflows` for epoch 1: one frozen handle per pinned workflow, built by the glue
/// (`start`, `get`). A pinned workflow's name is an identifier (`rewind_core::ActionSet`).
fn workflows_module_v1(action_set: &ActionSet) -> String {
    let mut module = String::from(
        "import hooks from \"rewind:internal\";\n\
         const workflow = hooks.workflow;\n",
    );
    for workflow in action_set.workflows() {
        let _ = writeln!(
            module,
            "export const {} = workflow({});",
            workflow.name,
            js_string(&workflow.name)
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

    // A pinned action set only has groups that are identifiers (`rewind_core::ActionSet::new`).
    #[test]
    fn generates_groups_and_bare_actions() {
        let module = actions_module_v1(
            &ActionSet::from_harness_actions([
                "github.list_issues",
                "github.close-issue",
                "ask_user",
            ])
            .unwrap(),
        );
        assert_eq!(
            module,
            "import hooks from \"rewind:internal\";\n\
             const call = hooks.callAction;\n\
             const content = hooks.callActionContent;\n\
             export const github = Object.freeze({\n  \
               \"close-issue\": (args) => call(\"github.close-issue\", args),\n  \
               \"list_issues\": (args) => call(\"github.list_issues\", args),\n\
             });\n\
             export const ask_user = (args) => call(\"ask_user\", args);\n"
        );
        let exports: Vec<String> = action_exports_v1(
            &ActionSet::from_harness_actions([
                "github.list_issues",
                "github.close-issue",
                "ask_user",
            ])
            .unwrap(),
        )
        .into_iter()
        .collect();
        assert_eq!(exports, ["ask_user", "github"]);
    }

    /// The names the compiler accepts, its aliases included, are exactly what `std.js` exports.
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
            let mut accepted: Vec<&str> = glue.std_exports.to_vec();
            accepted.extend(glue.std_aliases);
            accepted.sort_unstable();
            assert_eq!(names, accepted, "{}", glue.epoch);
        }
    }

    #[test]
    fn built_ins_are_not_exported() {
        use rewind_core::{Builtin, McpServer, McpTransport};
        let github = McpServer::new(
            "github",
            McpTransport::Http {
                url: "https://mcp.example.com/github".into(),
            },
        );
        let mut get_issue = Action::new(
            "github.get_issue",
            ActionRoute::Mcp {
                server: "github".into(),
            },
            serde_json::json!({}),
        );
        get_issue.output_schema = Some(serde_json::json!({ "type": "object" }));
        let action_set = ActionSet::new(
            [github],
            [
                get_issue,
                Action::new(
                    "github.list_issues",
                    ActionRoute::Mcp {
                        server: "github".into(),
                    },
                    serde_json::json!({}),
                ),
                Action::new(
                    "http",
                    ActionRoute::Builtin(Builtin::Http {
                        allowed_hosts: vec!["api.example.com".into()],
                    }),
                    serde_json::json!({}),
                ),
                Action::new(
                    "webhook",
                    ActionRoute::Builtin(Builtin::Webhook),
                    serde_json::json!({}),
                ),
            ],
        )
        .unwrap();
        let exports: Vec<String> = action_exports_v1(&action_set).into_iter().collect();
        assert_eq!(exports, ["github"]);
        let module = actions_module_v1(&action_set);
        assert!(module.contains("export const github"), "{module}");
        // An MCP action without an output schema returns its content, with the helpers; with
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
