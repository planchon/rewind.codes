//! The contract between codegen, the checker and the runtime (`archi/04-tools.md`, "Toolsets
//! and code generation" and "Type-checking"): the `ptc-tools.d.ts` that `ptc-codegen` writes
//! for a pinned toolset makes `ptc-check` accept what the runtime runs and refuse what it
//! refuses.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use ptc_check::TsgoChecker;
use ptc_core::{Builtin, McpServer, McpTransport, Script, Tool, ToolRoute, Toolset};
use ptc_engine::ports::{Diagnostic, DiagnosticSource, ScriptExecutor};
use ptc_runtime::{SandboxLimits, V8Executor};
use serde_json::json;

/// One checker for the file, extracted where `ptc-check`'s own tests extract it.
static CHECKER: LazyLock<TsgoChecker> = LazyLock::new(|| {
    TsgoChecker::new(Path::new(env!("CARGO_TARGET_TMPDIR")).join("ptc-check-cache"))
});

static EXECUTOR: LazyLock<V8Executor> = LazyLock::new(|| {
    V8Executor::new(SandboxLimits::default()).expect("TZ=UTC (see .cargo/config.toml)")
});

/// A toolset with an MCP server (tools with and without an output schema, with and without
/// required arguments), a host tool, and the built-ins as they are pinned.
fn toolset(http_tool: bool) -> Toolset {
    let server = McpServer::new(
        "crm",
        McpTransport::Http {
            url: "https://mcp.example.com/crm".into(),
        },
    );
    let crm = || ToolRoute::Mcp {
        server: "crm".into(),
    };
    let by_id = json!({
        "type": "object",
        "properties": { "id": { "type": "string" } },
        "required": ["id"],
        "additionalProperties": false,
    });
    let mut lookup = Tool::new("crm.lookup", crm(), by_id.clone());
    lookup.description = Some("Find a customer by id.".into());
    let export = Tool::new(
        "crm.export",
        crm(),
        json!({
            "type": "object",
            "properties": { "since": { "type": "string", "format": "date" } },
            "additionalProperties": false,
        }),
    );
    let mut score = Tool::new("crm.score", crm(), by_id);
    score.output_schema = Some(json!({
        "type": "object",
        "properties": { "score": { "type": "number" } },
        "required": ["score"],
    }));
    let ask_user = Tool::new(
        "ask_user",
        ToolRoute::Host { group: None },
        json!({
            "type": "object",
            "properties": { "question": { "type": "string" } },
            "required": ["question"],
        }),
    );
    let webhook = Tool::new(
        "webhook",
        ToolRoute::Builtin(Builtin::Webhook),
        ptc_runtime::builtins::webhook_input_schema(),
    );
    let mut tools = vec![lookup, export, score, ask_user, webhook];
    if http_tool {
        tools.push(ptc_mcp::http_tool(vec!["api.example.com".into()]));
    }
    Toolset::new([server], tools).unwrap()
}

/// What the checker says of a script, against the declarations codegen writes for the toolset.
async fn check(source: &str, toolset: &Toolset) -> Vec<Diagnostic> {
    let declarations = ptc_codegen::generate(toolset, &BTreeMap::new());
    CHECKER
        .check_source(source, &declarations.typescript)
        .await
        .unwrap_or_else(|error| panic!("{error}\n{}", declarations.typescript))
}

/// What the runtime says of a script when a run starts.
fn compile(source: &str, toolset: &Toolset) -> Result<(), String> {
    EXECUTOR
        .compile(&Script::new(source), toolset)
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

const GOOD: &str = r#"import { crm, ask_user } from "ptc:tools";
import type { ToolContent } from "ptc:tools";
import { webhook } from "ptc";

export default async function main() {
  const customer: ToolContent = await crm.lookup({ id: "c-1" });
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
    let toolset = toolset(true);
    assert_eq!(summary(&check(GOOD, &toolset).await), []);
    assert_eq!(compile(GOOD, &toolset), Ok(()));
}

#[tokio::test]
async fn args_of_the_wrong_type_are_refused() {
    let script = GOOD.replace(r#"crm.lookup({ id: "c-1" })"#, "crm.lookup({ id: 42 })");
    assert_eq!(
        summary(&check(&script, &toolset(true)).await),
        [(
            "TS2322".into(),
            "Type 'number' is not assignable to type 'string'.".into()
        )]
    );
}

#[tokio::test]
async fn fetch_needs_the_http_tool() {
    let diagnostics = check(GOOD, &toolset(false)).await;
    assert_eq!(
        summary(&diagnostics),
        [(
            "absent-global".into(),
            "`fetch` does not exist in ai-ptc scripts.".into()
        )]
    );
    assert_eq!(diagnostics[0].line, 12);
}

#[tokio::test]
async fn built_ins_are_not_in_ptc_tools_for_either() {
    let script = "import { http, webhook } from \"ptc:tools\";\n\nexport default async function main() {\n  return [http, webhook];\n}\n";
    let toolset = toolset(true);
    assert_eq!(
        summary(&check(script, &toolset).await),
        [
            (
                "TS2305".into(),
                "Module '\"ptc:tools\"' has no exported member 'http'.".into()
            ),
            (
                "TS2305".into(),
                "Module '\"ptc:tools\"' has no exported member 'webhook'.".into()
            ),
        ]
    );
    let refused = compile(script, &toolset).unwrap_err();
    assert!(
        refused.starts_with("\"ptc:tools\" has no export named \"http\""),
        "{refused}"
    );
}

#[tokio::test]
async fn types_are_imported_as_types_for_both() {
    let script = GOOD.replace(
        "import { crm, ask_user } from \"ptc:tools\";\nimport type { ToolContent } from \"ptc:tools\";",
        "import { crm, ask_user, ToolContent } from \"ptc:tools\";",
    );
    let toolset = toolset(true);
    let diagnostics = summary(&check(&script, &toolset).await);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].0, "TS1484");
    assert!(
        diagnostics[0]
            .1
            .starts_with("'ToolContent' is a type and must be imported using a type-only import"),
        "{diagnostics:?}"
    );
    let refused = compile(&script, &toolset).unwrap_err();
    assert!(
        refused.starts_with("\"ptc:tools\" has no export named \"ToolContent\""),
        "{refused}"
    );
}

/// Every golden `ptc-tools.d.ts` of `ptc-codegen` is valid under the checker's options: a
/// check with them reports nothing, and does not fail on the declarations.
#[tokio::test]
async fn the_golden_declarations_of_codegen_check() {
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ptc-codegen/tests/golden");
    let mut checked = 0;
    for entry in std::fs::read_dir(&golden).unwrap() {
        let path = entry.unwrap().path().join("ptc-tools.d.ts");
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
    assert!(checked >= 4, "the golden cases of ptc-codegen are missing");
}
