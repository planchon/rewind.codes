//! The contract between codegen, the checker and the runtime (`archi/04-tools.md`, "Toolsets
//! and code generation" and "Type-checking"): the `rewind-tools.d.ts` that `rewind-codegen` writes
//! for a pinned toolset makes `rewind-check` accept what the runtime runs and refuse what it
//! refuses.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use rewind_check::TsgoChecker;
use rewind_core::{Builtin, McpServer, McpTransport, Script, Tool, ToolRoute, Toolset};
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
        rewind_runtime::builtins::webhook_input_schema(),
    );
    let mut tools = vec![lookup, export, score, ask_user, webhook];
    if http_tool {
        tools.push(rewind_mcp::http_tool(vec!["api.example.com".into()]));
    }
    Toolset::new([server], tools).unwrap()
}

/// What the checker says of a script, against the declarations codegen writes for the toolset.
async fn check(source: &str, toolset: &Toolset) -> Vec<Diagnostic> {
    let declarations = rewind_codegen::generate(toolset, &BTreeMap::new());
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

const GOOD: &str = r#"import { crm, ask_user } from "rewind:tools";
import type { ToolContent } from "rewind:tools";
import { webhook } from "rewind";

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
            "`fetch` does not exist in Rewind scripts.".into()
        )]
    );
    assert_eq!(diagnostics[0].line, 12);
}

#[tokio::test]
async fn built_ins_are_not_in_rewind_tools_for_either() {
    let script = "import { http, webhook } from \"rewind:tools\";\n\nexport default async function main() {\n  return [http, webhook];\n}\n";
    let toolset = toolset(true);
    assert_eq!(
        summary(&check(script, &toolset).await),
        [
            (
                "TS2305".into(),
                "Module '\"rewind:tools\"' has no exported member 'http'.".into()
            ),
            (
                "TS2305".into(),
                "Module '\"rewind:tools\"' has no exported member 'webhook'.".into()
            ),
        ]
    );
    let refused = compile(script, &toolset).unwrap_err();
    assert!(
        refused.starts_with("\"rewind:tools\" has no export named \"http\""),
        "{refused}"
    );
}

#[tokio::test]
async fn types_are_imported_as_types_for_both() {
    let script = GOOD.replace(
        "import { crm, ask_user } from \"rewind:tools\";\nimport type { ToolContent } from \"rewind:tools\";",
        "import { crm, ask_user, ToolContent } from \"rewind:tools\";",
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
        refused.starts_with("\"rewind:tools\" has no export named \"ToolContent\""),
        "{refused}"
    );
}

/// Every golden `rewind-tools.d.ts` of `rewind-codegen` is valid under the checker's options: a
/// check with them reports nothing, and does not fail on the declarations.
#[tokio::test]
async fn the_golden_declarations_of_codegen_check() {
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("../rewind-codegen/tests/golden");
    let mut checked = 0;
    for entry in std::fs::read_dir(&golden).unwrap() {
        let path = entry.unwrap().path().join("rewind-tools.d.ts");
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

/// A toolset in which `name` is a server, a host-tool group or a host tool without a group,
/// each tool taking `{ q: string }` and returning `{ id: string }`; and a script that calls it.
fn named(name: &str) -> Vec<(Toolset, String)> {
    let input = json!({
        "type": "object",
        "properties": { "q": { "type": "string" } },
        "required": ["q"],
    });
    let tool = |tool: String, route: ToolRoute| {
        let mut tool = Tool::new(tool, route, input.clone());
        tool.output_schema = Some(json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
        }));
        tool
    };
    let server = McpServer::new(
        name,
        McpTransport::Http {
            url: "https://mcp.example.com/mcp".into(),
        },
    );
    let call = |imported: &str, called: &str| {
        format!(
            "import {{ {imported} as tools }} from \"rewind:tools\";\n\n\
             export default async function main() {{\n  \
             const found: {{ id: string }} = await {called}({{ q: \"x\" }});\n  \
             return found.id;\n}}\n"
        )
    };
    let mcp = ToolRoute::Mcp {
        server: name.to_owned(),
    };
    let group = ToolRoute::Host {
        group: Some(name.to_owned()),
    };
    vec![
        (
            Toolset::new([server], [tool(format!("{name}.find"), mcp)]).unwrap(),
            call(name, "tools.find"),
        ),
        (
            Toolset::new([], [tool(format!("{name}.ask"), group)]).unwrap(),
            call(name, "tools.ask"),
        ),
        (
            Toolset::new([], [tool(name.to_owned(), ToolRoute::Host { group: None })]).unwrap(),
            call(name, "tools"),
        ),
    ]
}

/// Names that are keywords somewhere, globals, or the declarations' own types, but not
/// reserved words: a toolset takes them, the checker accepts a script that imports them, and
/// the runtime starts it. The reserved words are refused by the toolset (`rewind-core`).
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
        "ToolContent",
        "Promise",
    ] {
        for (toolset, script) in named(name) {
            let diagnostics = summary(&check(&script, &toolset).await);
            assert_eq!(diagnostics, [], "{name}:\n{script}");
            assert_eq!(compile(&script, &toolset), Ok(()), "{name}:\n{script}");
        }
    }
}
