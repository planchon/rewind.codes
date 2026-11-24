//! The contract between codegen, the checker and the runtime (`archi/04-tools.md`, "Action sets
//! and code generation" and "Type-checking"): the `rewind-actions.d.ts` that `rewind-codegen` writes
//! for a pinned action set makes `rewind-check` accept what the runtime runs and refuse what it
//! refuses.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use rewind_check::TsgoChecker;
use rewind_core::{Action, ActionRoute, ActionSet, Builtin, McpServer, McpTransport, Script};
use rewind_engine::ports::{Diagnostic, DiagnosticSource, ScriptExecutor};
use rewind_runtime::{SandboxLimits, V8Executor};
use serde_json::json;

/// One checker for the file, extracted where `rewind-check`'s own tests extract it.
static CHECKER: LazyLock<TsgoChecker> = LazyLock::new(|| {
    TsgoChecker::new(Path::new(env!("CARGO_TARGET_TMPDIR")).join("rewind-check-cache"))
});

static EXECUTOR: LazyLock<V8Executor> = LazyLock::new(|| {
    V8Executor::new(SandboxLimits::default()).expect("TZ=UTC (see .cargo/config.toml)")
});

/// An action set with an MCP server (actions with and without an output schema, with and without
/// required arguments), a host tool, and the built-ins as they are pinned.
fn action_set(http_action: bool) -> ActionSet {
    let server = McpServer::new(
        "crm",
        McpTransport::Http {
            url: "https://mcp.example.com/crm".into(),
        },
    );
    let crm = || ActionRoute::Mcp {
        server: "crm".into(),
    };
    let by_id = json!({
        "type": "object",
        "properties": { "id": { "type": "string" } },
        "required": ["id"],
        "additionalProperties": false,
    });
    let mut lookup = Action::new("crm.lookup", crm(), by_id.clone());
    lookup.description = Some("Find a customer by id.".into());
    let export = Action::new(
        "crm.export",
        crm(),
        json!({
            "type": "object",
            "properties": { "since": { "type": "string", "format": "date" } },
            "additionalProperties": false,
        }),
    );
    let mut score = Action::new("crm.score", crm(), by_id);
    score.output_schema = Some(json!({
        "type": "object",
        "properties": { "score": { "type": "number" } },
        "required": ["score"],
    }));
    let ask_user = Action::new(
        "ask_user",
        ActionRoute::Host { group: None },
        json!({
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"],
        }),
    );
    let webhook = Action::new(
        "webhook",
        ActionRoute::Builtin(Builtin::Webhook),
        rewind_runtime::builtins::webhook_input_schema(),
    );
    let mut actions = vec![lookup, export, score, ask_user, webhook];
    if http_action {
        actions.push(rewind_mcp::http_action(vec!["api.example.com".into()]));
    }
    ActionSet::new([server], actions).unwrap()
}

/// What the checker says of a script, against the declarations codegen writes for the action set.
async fn check(source: &str, action_set: &ActionSet) -> Vec<Diagnostic> {
    let declarations = rewind_codegen::generate(action_set, &BTreeMap::new());
    CHECKER
        .check_source(source, &declarations.typescript)
        .await
        .unwrap_or_else(|error| panic!("{error}\n{}", declarations.typescript))
}

/// What the runtime says of a script when a run starts.
fn compile(source: &str, action_set: &ActionSet) -> Result<(), String> {
    EXECUTOR
        .compile(&Script::new(source), action_set)
        .map(|_| ())
        .map_err(|invalid| invalid.message)
}

/// `(source, message)` of each diagnostic.
fn summary(diagnostics: &[Diagnostic]) -> Vec<(String, String)> {
    diagnostics
        .iter()
        .map(|diagnostic| {
            let source = match &diagnostic.source {
                DiagnosticSource::TypeScript { code } => format!("TS{code}"),
                DiagnosticSource::Rule(rule) => rule.clone(),
            };
            (source, diagnostic.message.clone())
        })
        .collect()
}

const GOOD: &str = r#"import { crm, ask_user } from "rewind:actions";
import type { ActionContent } from "rewind:actions";
import { webhook } from "rewind";

export default async function main() {
  const customer: ActionContent = await crm.lookup({ id: "c-1" });
  const name = customer.json<{ name: string }>().name;
  const csv: string = (await crm.export()).text();
  const { score } = await crm.score({ id: "c-1" });
  const answer = await ask_user({ question: `Is ${score} right for ${name}?` });
  const hook = await webhook("done");
  const response = await fetch("https://api.example.com/v1/items", { method: "POST", body: csv });
  return { answer, hook: hook.url, status: response.status, items: await response.json() };
}
"#;

#[tokio::test]
async fn a_good_script_checks_and_starts() {
    let action_set = action_set(true);
    assert_eq!(summary(&check(GOOD, &action_set).await), []);
    assert_eq!(compile(GOOD, &action_set), Ok(()));
}

/// Code written before actions were named (`archi/08-decisions.md`, R1): it imports
/// `rewind:tools`, catches a `ToolError` and reads its `tool`.
const OLD_NAMES: &str = r#"import { crm, ask_user } from "rewind:tools";
import type { ToolContent } from "rewind:tools";
import { ToolError, sleep } from "rewind";

export default async function main() {
  try {
    const customer: ToolContent = await crm.lookup({ id: "c-1" });
    await sleep("1s");
    return customer.json<{ name: string }>().name;
  } catch (error) {
    if (error instanceof ToolError) return `${error.tool} failed: ${error.message}`;
    return await ask_user({ question: "what now?" });
  }
}
"#;

#[tokio::test]
async fn code_written_with_the_old_names_checks_and_starts() {
    let action_set = action_set(false);
    let script = OLD_NAMES;
    assert_eq!(summary(&check(script, &action_set).await), []);
    assert_eq!(compile(script, &action_set), Ok(()));
    // The old names reach the same exports: a wrong argument is refused through either.
    let wrong = script.replace(r#"crm.lookup({ id: "c-1" })"#, "crm.lookup({ id: 42 })");
    assert_eq!(
        summary(&check(&wrong, &action_set).await),
        [(
            "TS2322".to_owned(),
            "Type 'number' is not assignable to type 'string'.".to_owned()
        )]
    );
    let missing = script.replace("{ crm, ask_user }", "{ crm, jira }");
    assert!(compile(&missing, &action_set).is_err());
    assert!(!check(&missing, &action_set).await.is_empty());
}

#[tokio::test]
async fn the_old_names_are_declared_nowhere() {
    let declarations = rewind_codegen::generate(&action_set(true), &BTreeMap::new());
    for text in [&declarations.typescript, &declarations.prompt] {
        assert!(!text.contains("rewind:tools"), "{text}");
        assert!(!text.contains("ToolError"), "{text}");
        assert!(!text.contains("ToolContent"), "{text}");
    }
}

#[tokio::test]
async fn args_of_the_wrong_type_are_refused() {
    let script = GOOD.replace(r#"crm.lookup({ id: "c-1" })"#, "crm.lookup({ id: 42 })");
    assert_eq!(
        summary(&check(&script, &action_set(true)).await),
        [(
            "TS2322".into(),
            "Type 'number' is not assignable to type 'string'.".into()
        )]
    );
}

#[tokio::test]
async fn fetch_needs_the_http_action() {
    let diagnostics = check(GOOD, &action_set(false)).await;
    assert_eq!(
        summary(&diagnostics),
        [(
            "absent-global".into(),
            "`fetch` does not exist in Rewind scripts.".into()
        )]
    );
    assert_eq!(diagnostics[0].line, 12);
}

#[tokio::test]
async fn built_ins_are_not_in_rewind_actions_for_either() {
    let script = "import { http, webhook } from \"rewind:actions\";\n\nexport default async function main() {\n  return [http, webhook];\n}\n";
    let action_set = action_set(true);
    assert_eq!(
        summary(&check(script, &action_set).await),
        [
            (
                "TS2305".into(),
                "Module '\"rewind:actions\"' has no exported member 'http'.".into()
            ),
            (
                "TS2305".into(),
                "Module '\"rewind:actions\"' has no exported member 'webhook'.".into()
            ),
        ]
    );
    let refused = compile(script, &action_set).unwrap_err();
    assert!(
        refused.starts_with("\"rewind:actions\" has no export named \"http\""),
        "{refused}"
    );
}

#[tokio::test]
async fn types_are_imported_as_types_for_both() {
    let script = GOOD.replace(
        "import { crm, ask_user } from \"rewind:actions\";\nimport type { ActionContent } from \"rewind:actions\";",
        "import { crm, ask_user, ActionContent } from \"rewind:actions\";",
    );
    let action_set = action_set(true);
    let diagnostics = summary(&check(&script, &action_set).await);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].0, "TS1484");
    assert!(
        diagnostics[0]
            .1
            .starts_with("'ActionContent' is a type and must be imported using a type-only import"),
        "{diagnostics:?}"
    );
    let refused = compile(&script, &action_set).unwrap_err();
    assert!(
        refused.starts_with("\"rewind:actions\" has no export named \"ActionContent\""),
        "{refused}"
    );
}

/// Every golden `rewind-actions.d.ts` of `rewind-codegen` is valid under the checker's options: a
/// check with them reports nothing, and does not fail on the declarations.
#[tokio::test]
async fn the_golden_declarations_of_codegen_check() {
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("../rewind-codegen/tests/golden");
    let mut checked = 0;
    for entry in std::fs::read_dir(&golden).unwrap() {
        let path = entry.unwrap().path().join("rewind-actions.d.ts");
        let Ok(declarations) = std::fs::read_to_string(&path) else {
            continue;
        };
        let script = "export default async function main() {\n  return 1 + \"x\" * 2;\n}\n";
        let diagnostics = CHECKER
            .check_source(script, &declarations)
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        // The script's own mistake only: the declarations add none.
        assert_eq!(
            summary(&diagnostics),
            [(
                "TS2362".into(),
                "The left-hand side of an arithmetic operation must be of type 'any', 'number', 'bigint' or an enum type.".into()
            )],
            "{}",
            path.display()
        );
        checked += 1;
    }
    assert!(
        checked >= 4,
        "the golden cases of rewind-codegen are missing"
    );
}

/// An action set in which `name` is a server, a host-tool group or a host tool without a group,
/// each action taking `{ q: string }` and returning `{ id: string }`; and a script that calls it.
fn named(name: &str) -> Vec<(ActionSet, String)> {
    let input = json!({
        "type": "object",
        "properties": { "q": { "type": "string" } },
        "required": ["q"],
    });
    let action = |action: String, route: ActionRoute| {
        let mut action = Action::new(action, route, input.clone());
        action.output_schema = Some(json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
        }));
        action
    };
    let server = McpServer::new(
        name,
        McpTransport::Http {
            url: "https://mcp.example.com/mcp".into(),
        },
    );
    let call = |imported: &str, called: &str| {
        format!(
            "import {{ {imported} as actions }} from \"rewind:actions\";\n\n\
             export default async function main() {{\n  \
             const found: {{ id: string }} = await {called}({{ q: \"x\" }});\n  \
             return found.id;\n}}\n"
        )
    };
    let mcp = ActionRoute::Mcp {
        server: name.to_owned(),
    };
    let group = ActionRoute::Host {
        group: Some(name.to_owned()),
    };
    vec![
        (
            ActionSet::new([server], [action(format!("{name}.find"), mcp)]).unwrap(),
            call(name, "actions.find"),
        ),
        (
            ActionSet::new([], [action(format!("{name}.ask"), group)]).unwrap(),
            call(name, "actions.ask"),
        ),
        (
            ActionSet::new(
                [],
                [action(name.to_owned(), ActionRoute::Host { group: None })],
            )
            .unwrap(),
            call(name, "actions"),
        ),
    ]
}

/// Names that are keywords somewhere, globals, or the declarations' own types, but not
/// reserved words: an action set takes them, the checker accepts a script that imports them, and
/// the runtime starts it. The reserved words are refused by the action set (`rewind-core`).
#[tokio::test]
async fn names_that_are_not_reserved_words_check_and_start() {
    for name in [
        "async",
        "of",
        "type",
        "namespace",
        "global",
        "declare",
        "string",
        "unknown",
        "undefined",
        "globalThis",
        "NaN",
        "require",
        "constructor",
        "ActionContent",
        "Promise",
    ] {
        for (action_set, script) in named(name) {
            let diagnostics = summary(&check(&script, &action_set).await);
            assert_eq!(diagnostics, [], "{name}:\n{script}");
            assert_eq!(compile(&script, &action_set), Ok(()), "{name}:\n{script}");
        }
    }
}

// -------------------------------------------------------------------------------------------
// Workflows: `rewind:workflows` as codegen declares it, and a workflow's own declarations

fn triage() -> rewind_core::Workflow {
    rewind_core::Workflow::new(
        "triage",
        Some("Triage the stale issues of a repository.".into()),
        Script::new(TRIAGE).digest(),
        action_set(false).digest(),
        rewind_core::WorkflowSchemas {
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
            signals: BTreeMap::from([(
                "approval".to_owned(),
                json!({
                    "type": "object",
                    "properties": { "close": { "type": "boolean" } },
                    "required": ["close"],
                }),
            )]),
        },
    )
    .unwrap()
}

const TRIAGE: &str = r#"import { crm } from "rewind:actions";
import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const { score } = await crm.score({ id: input.repo });
  const approval = await signal("approval");
  return { closed: approval.close ? Math.round(score) : 0 };
}
"#;

const PARENT: &str = r#"import { triage } from "rewind:workflows";
import { WorkflowError } from "rewind";

export default async function main(input: { repos: string[] }) {
  const runs = await Promise.all(input.repos.map((repo) => triage.start({ repo }, { detached: false })));
  for (const run of runs) await run.signal("approval", { close: true });
  try {
    const results = await Promise.all(runs.map((run) => run.result({ timeout: "1d" })));
    return results.map((result) => result?.closed ?? 0);
  } catch (error) {
    if (error instanceof WorkflowError) return [error.status];
    throw error;
  }
}
"#;

#[tokio::test]
async fn a_script_that_starts_a_pinned_workflow_checks_and_starts() {
    let action_set = action_set(false)
        .with_workflows([triage().pinned()])
        .unwrap();
    assert_eq!(summary(&check(PARENT, &action_set).await), []);
    assert_eq!(compile(PARENT, &action_set), Ok(()));

    let wrong = PARENT.replace("triage.start({ repo }", "triage.start({ repo: 1 }");
    assert_eq!(
        summary(&check(&wrong, &action_set).await),
        [(
            "TS2322".into(),
            "Type 'number' is not assignable to type 'string'.".into()
        )]
    );
}

#[tokio::test]
async fn a_workflow_the_action_set_does_not_pin_is_refused_by_both() {
    let action_set = action_set(false);
    let diagnostics = summary(&check(PARENT, &action_set).await);
    assert_eq!(
        diagnostics[0],
        (
            "TS2305".into(),
            "Module '\"rewind:workflows\"' has no exported member 'triage'.".into()
        )
    );
    let refused = compile(PARENT, &action_set).unwrap_err();
    assert!(
        refused.starts_with("\"rewind:workflows\" has no export named \"triage\""),
        "{refused}"
    );
}

#[tokio::test]
async fn a_workflow_s_script_is_checked_against_its_own_schemas() {
    let workflow = triage();
    let own = rewind_codegen::workflow_declarations("triage", workflow.schemas());
    let declarations = rewind_codegen::generate(&action_set(false), &BTreeMap::new());
    let check_workflow = |source: String| {
        let own = own.clone();
        let declarations = declarations.typescript.clone();
        async move {
            CHECKER
                .check_workflow_source(&source, &declarations, &own.signals, &own.entry)
                .await
                .unwrap()
        }
    };
    assert_eq!(summary(&check_workflow(TRIAGE.into()).await), []);
    let undeclared = summary(&check_workflow(TRIAGE.replace("\"approval\"", "\"aproval\"")).await);
    assert!(
        undeclared
            .iter()
            .any(|(_, message)| message.contains("\"aproval\"")),
        "{undeclared:?}"
    );
    let mismatched = check_workflow(TRIAGE.replace("return { closed:", "return { count:")).await;
    assert!(
        mismatched.iter().any(|diagnostic| diagnostic.line == 4
            && diagnostic
                .message
                .starts_with("the default export does not match the workflow's schemas")),
        "{:?}",
        summary(&mismatched)
    );
}
