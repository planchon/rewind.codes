//! Checks with the real tsgo: good and bad scripts, against hand-written declarations of the
//! kind `rewind-codegen` generates.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use rewind_check::{CheckError, HTTP_TOOL_REFERENCE, TSGO_VERSION, TsgoChecker};
use rewind_core::Script;
use rewind_engine::ports::{
    CheckerError, Diagnostic, DiagnosticSource, Severity, ToolDeclarations, TypeChecker,
};

/// The `rewind-tools.d.ts` of a toolset with two MCP servers and a host tool.
const TOOLS: &str = r##"declare module "rewind:tools" {
  export namespace github {
    export interface Issue {
      number: number;
      title: string;
      updated_at: string;
    }
    /** List the issues of a repository. */
    export function list_issues(args: {
      repo: string;
      state?: "open" | "closed";
      labels?: string;
    }): Promise<Issue[]>;
    /** Close an issue. */
    export function close_issue(args: { repo: string; number: number }): Promise<void>;
  }
  export namespace slack {
    /** Post a message to a channel. */
    export function post_message(args: { channel: string; text: string }): Promise<{ ts: string }>;
  }
  /** Ask the end user a question and return their answer. */
  export function ask_user(args: { question: string }): Promise<string>;
}
"##;

/// The script of `archi/01-product.md`, "What the model writes".
const GOOD: &str = r##"import { github, slack } from "rewind:tools";
import { sleep, signal } from "rewind";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo, state: "open" });
  const stale = issues.filter((i) => Date.now() - Date.parse(i.updated_at) > 30 * 86_400_000);

  await slack.post_message({ channel: "#triage", text: `${stale.length} stale issues in ${input.repo}` });

  const decision = await signal<{ close: boolean }>("approval", { timeout: "3d" });
  if (!decision?.close) return { closed: 0 };

  await Promise.all(stale.map((i) => github.close_issue({ repo: input.repo, number: i.number })));

  await sleep("10d");
  const reopened = await github.list_issues({ repo: input.repo, state: "open", labels: "reopened" });
  return { closed: stale.length, reopened: reopened.length };
}
"##;

/// One checker for the whole file: tsgo is extracted once, in the target directory.
static CHECKER: LazyLock<TsgoChecker> = LazyLock::new(|| TsgoChecker::new(cache_dir()));

fn cache_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("rewind-check-cache")
}

async fn check(source: &str) -> Vec<Diagnostic> {
    check_with(source, TOOLS).await
}

async fn check_with(source: &str, declarations: &str) -> Vec<Diagnostic> {
    CHECKER
        .check_source(source, declarations)
        .await
        .unwrap_or_else(|error| panic!("{error}\n{source}"))
}

/// `(line, column, source, message)` of each diagnostic.
fn summary(diagnostics: &[Diagnostic]) -> Vec<(u32, u32, String, String)> {
    diagnostics
        .iter()
        .map(|diagnostic| {
            let source = match &diagnostic.source {
                DiagnosticSource::TypeScript { code } => format!("TS{code}"),
                DiagnosticSource::Rule(rule) => rule.clone(),
            };
            (
                diagnostic.line,
                diagnostic.column,
                source,
                diagnostic.message.clone(),
            )
        })
        .collect()
}

fn sources(diagnostics: &[Diagnostic]) -> Vec<String> {
    summary(diagnostics)
        .into_iter()
        .map(|(_, _, source, _)| source)
        .collect()
}

fn main_with(body: &str) -> String {
    format!(
        "import {{ github, slack, ask_user }} from \"rewind:tools\";\nimport {{ sleep, signal, webhook, run, ToolError, CancelledError }} from \"rewind\";\n\nexport default async function main(input: {{ repo: string }}) {{\n{body}\n}}\n"
    )
}

#[tokio::test]
async fn a_good_script_has_no_diagnostics() {
    assert_eq!(check(GOOD).await, []);
}

#[tokio::test]
async fn the_sandbox_and_rewind_type_check() {
    let script = main_with(
        r##"  const id: string = run.id;
  const started: Date = run.startedAt;
  const hook = await webhook("merged");
  const url = new URL(hook.url);
  url.searchParams.set("run", id);
  const bytes = new TextEncoder().encode(btoa("x"));
  const copy = structuredClone({ bytes: Array.from(bytes) });
  console.log(started.toISOString(), copy, crypto.randomUUID(), performance.now());
  const instant = Temporal.Now.instant();
  const evens = Iterator.from([1, 2, 3, 4]).filter((n) => n % 2 === 0).toArray();
  const union = new Set([1]).union(new Set([2]));
  await new Promise((resolve) => setTimeout(resolve, 100));
  try {
    await github.close_issue({ repo: input.repo, number: 1 });
  } catch (error) {
    if (error instanceof ToolError && error.outcomeUnknown) return { tool: error.tool };
    if (error instanceof CancelledError) throw error;
  }
  return { instant: instant.toString(), evens, union: [...union] };"##,
    );
    assert_eq!(check(&script).await, []);
}

#[tokio::test]
async fn a_type_error_is_located_in_the_script_with_a_frame() {
    let script =
        main_with("  const issues = await github.list_issues({ repo: 42 });\n  return issues;");
    let diagnostics = check(&script).await;
    assert_eq!(
        summary(&diagnostics),
        [(
            5,
            45,
            "TS2322".into(),
            "Type 'number' is not assignable to type 'string'.".into()
        )]
    );
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic.severity, Severity::Error);
    assert_eq!(diagnostic.hint, None);
    assert_eq!(
        diagnostic.to_string(),
        "script.ts:5:45 error TS2322: Type 'number' is not assignable to type 'string'.\n4 | export default async function main(input: { repo: string }) {\n5 |   const issues = await github.list_issues({ repo: 42 });\n  |                                             ^^^^\n6 |   return issues;"
    );
}

#[tokio::test]
async fn chained_messages_keep_their_lines() {
    let script = main_with("  const f: (a: string) => void = (a: number) => {};\n  return f;");
    let diagnostics = check(&script).await;
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].message,
        "Type '(a: number) => void' is not assignable to type '(a: string) => void'.\n  Types of parameters 'a' and 'a' are incompatible.\n    Type 'string' is not assignable to type 'number'."
    );
}

#[tokio::test]
async fn misuses_of_the_tools_and_of_rewind_are_found() {
    let script = main_with(
        r##"  const issues = await github.list_issues({ repo: input.repo, state: "all" });
  await github.close_issue({ repo: input.repo });
  await slack.post_message({ channel: "#x", text: "y", extra: true });
  const decision = await signal<{ close: boolean }>("approval", { timeout: "3d" });
  const close: boolean = decision.close;
  throw new ToolError("no");"##,
    );
    let codes = sources(&check(&script).await);
    assert_eq!(codes, ["TS2322", "TS2741", "TS2353", "TS18048", "TS2673"]);
}

#[tokio::test]
async fn absent_globals_get_a_hint_instead_of_tsgo_advice() {
    let script = main_with(
        r##"  const token = process.env.TOKEN;
  const fs = require("fs");
  const bytes: Buffer = Buffer.from("x");
  const page = await fetch("https://example.com");
  return { token, fs, bytes, page };"##,
    );
    let diagnostics = check(&script).await;
    assert_eq!(
        summary(&diagnostics),
        [
            (
                5,
                17,
                "absent-global".into(),
                "`process` does not exist in Rewind scripts.".into()
            ),
            (
                6,
                14,
                "absent-global".into(),
                "`require` does not exist in Rewind scripts.".into()
            ),
            (
                7,
                16,
                "absent-global".into(),
                "`Buffer` does not exist in Rewind scripts.".into()
            ),
            (
                7,
                25,
                "absent-global".into(),
                "`Buffer` does not exist in Rewind scripts.".into()
            ),
            (
                8,
                22,
                "absent-global".into(),
                "`fetch` does not exist in Rewind scripts.".into()
            ),
        ]
    );
    let process = &diagnostics[0];
    assert!(
        process.hint.as_deref().unwrap().contains("the run's input"),
        "{process}"
    );
    assert!(!process.to_string().contains("npm"), "{process}");
    assert!(process.frame.contains("^^^^^^^"), "{process}");
}

#[tokio::test]
async fn fetch_exists_only_with_the_http_tool() {
    let script = main_with(
        r##"  const response = await fetch("https://api.example.com/x", { method: "POST", body: "{}" });
  const text: string = await response.text();
  fetch("https://api.example.com/y");
  return { ok: response.ok, status: response.status, text };"##,
    );
    let http = format!("{HTTP_TOOL_REFERENCE}\n{TOOLS}");
    let diagnostics = check_with(&script, &http).await;
    assert_eq!(
        summary(&diagnostics),
        [(
            7,
            3,
            "floating-promise".into(),
            "`fetch(…)` calls a tool, and its promise is neither awaited, returned nor assigned: the script goes on without waiting for the call, and loses its result and its errors.".into()
        )]
    );
    let without = check(&script).await;
    assert_eq!(
        sources(&without),
        ["absent-global", "absent-global"],
        "{without:?}"
    );
}

#[tokio::test]
async fn the_rules_join_tsgo_diagnostics() {
    let script = main_with(
        r##"  const issues = await github.list_issues({ repo: input.repo });
  issues.forEach(async (issue) => {
    await github.close_issue({ repo: input.repo, number: issue.number });
  });
  sleep("1h");
  slack.post_message({ channel: "#x", text: 42 });
  void slack.post_message({ channel: "#x", text: "y" });
  return issues.length;"##,
    );
    let diagnostics = check(&script).await;
    assert_eq!(
        summary(&diagnostics)
            .into_iter()
            .map(|(line, column, source, _)| (line, column, source))
            .collect::<Vec<_>>(),
        [
            (6, 10, "ignored-async-callback".into()),
            (9, 3, "floating-promise".into()),
            (10, 3, "floating-promise".into()),
            (10, 39, "TS2322".into()),
        ]
    );
    let floating = &diagnostics[1];
    assert_eq!(
        floating.to_string(),
        "script.ts:9:3 error floating-promise: `sleep(…)` returns a promise that is neither awaited, returned nor assigned: the script goes on at once instead of waiting.\n  hint: Write `await` before the call. If not waiting for it is intended, write `void` before it.\n 8 |   });\n 9 |   sleep(\"1h\");\n   |   ^^^^^\n10 |   slack.post_message({ channel: \"#x\", text: 42 });"
    );
}

#[tokio::test]
async fn nothing_outside_the_check_is_read() {
    let secrets = tempfile::tempdir().unwrap();
    let secret = secrets.path().join("secret.ts");
    std::fs::write(
        &secret,
        "export const secret = \"hunter2\" as const;\nexport type Secret = \"hunter2\";\n",
    )
    .unwrap();
    let path = secret.display().to_string();
    let script = format!(
        "/// <reference path=\"{path}\" />\nimport {{ secret }} from \"{}\";\nimport type {{ Secret }} from \"../../../../../../../../../../..{}\";\nexport default async function main() {{\n  const n: number = secret;\n  const s: Secret = 1;\n  return [n, s];\n}}\n",
        path.trim_end_matches(".ts"),
        path.trim_end_matches(".ts"),
    );
    let diagnostics = check(&script).await;
    for diagnostic in &diagnostics {
        assert!(
            !diagnostic.message.contains("hunter2"),
            "the check read a file of the host: {diagnostic}"
        );
    }
    assert_eq!(
        sources(&diagnostics),
        ["TS2307", "TS2307"],
        "{diagnostics:?}"
    );
}

#[tokio::test]
async fn a_script_cannot_add_the_dom_or_node() {
    let script = "/// <reference lib=\"dom\" />\n/// <reference types=\"node\" />\nexport default async function main() {\n  return [document.title, new WeakRef({}), new SharedArrayBuffer(1), Atomics, WebAssembly];\n}\n";
    let diagnostics = check(script).await;
    assert_eq!(
        summary(&diagnostics),
        [
            (
                4,
                11,
                "TS2584".into(),
                "Cannot find name 'document'. Do you need to change your target library? Try changing the 'lib' compiler option to include 'dom'.".into()
            ),
            (4, 31, "TS2304".into(), "Cannot find name 'WeakRef'.".into()),
            (
                4,
                48,
                "TS2583".into(),
                "Cannot find name 'SharedArrayBuffer'. Do you need to change your target library? Try changing the 'lib' compiler option to 'es2017' or later.".into()
            ),
            (
                4,
                70,
                "TS2583".into(),
                "Cannot find name 'Atomics'. Do you need to change your target library? Try changing the 'lib' compiler option to 'es2017' or later.".into()
            ),
            (
                4,
                79,
                "TS2304".into(),
                "Cannot find name 'WebAssembly'.".into()
            ),
        ]
    );
}

/// TypeScript reads a directive's name in any case, and skips U+FEFF and U+200B as whitespace
/// before it: none of these may add the DOM either.
#[tokio::test]
async fn no_spelling_of_a_directive_adds_the_dom() {
    for header in [
        "/// <REFERENCE lib=\"dom\" />",
        "/// <Reference LIB=\"dom\" />",
        "// a\n\u{feff}/// <reference lib=\"dom\" />",
        "// a\n\u{200b}/// <reference lib=\"dom\" />",
    ] {
        let script = format!(
            "{header}\nexport default async function main() {{\n  return document.title;\n}}\n"
        );
        let line = 2 + u32::try_from(header.lines().count()).unwrap();
        let diagnostics = check(&script).await;
        assert_eq!(
            summary(&diagnostics),
            [(
                line,
                10,
                "TS2584".into(),
                "Cannot find name 'document'. Do you need to change your target library? Try changing the 'lib' compiler option to include 'dom'.".into()
            )],
            "{header:?}"
        );
    }
}

#[tokio::test]
async fn without_tools_rewind_tools_exports_nothing() {
    let script = "import { github } from \"rewind:tools\";\nexport default async function main() {\n  return github;\n}\n";
    let diagnostics = check_with(script, "").await;
    assert_eq!(
        summary(&diagnostics),
        [(
            1,
            10,
            "TS2305".into(),
            "Module '\"rewind:tools\"' has no exported member 'github'.".into()
        )]
    );
    assert_eq!(
        check_with(
            "import * as tools from \"rewind:tools\";\nexport default async () => tools;\n",
            ""
        )
        .await,
        []
    );
}

#[tokio::test]
async fn declarations_that_do_not_check_are_a_fault_of_the_platform() {
    let broken = "declare module \"rewind:tools\" {\n  export function list(args: Missing): Promise<void>;\n}\n";
    let error = CHECKER.check_source(GOOD, broken).await.unwrap_err();
    assert!(
        matches!(&error, CheckError::Declarations(message) if message.contains("rewind-tools.d.ts(2,30): TS2304: Cannot find name 'Missing'.")),
        "{error}"
    );
    let port: CheckerError = error.into();
    assert!(matches!(port, CheckerError::Internal(_)));
}

#[tokio::test]
async fn a_conflict_the_script_declares_is_the_scripts() {
    let script = "declare global {\n  interface Console {\n    log: number;\n  }\n  var crypto: number;\n}\nexport default async function main() {\n  return 1;\n}\n";
    let diagnostics = check(script).await;
    assert_eq!(
        summary(&diagnostics),
        [
            (3, 5, "TS2300".into(), "Duplicate identifier 'log'.".into()),
            (
                5,
                7,
                "TS2403".into(),
                "Subsequent variable declarations must have the same type.  Variable 'crypto' must be of type 'Crypto', but here has type 'number'.".into()
            ),
        ]
    );
}

#[tokio::test]
async fn syntax_errors_are_reported() {
    let diagnostics = check("export default async function main() {\n  return [1, 2;\n}\n").await;
    assert_eq!(
        summary(&diagnostics),
        [
            (2, 15, "TS1005".into(), "',' expected.".into()),
            (
                3,
                1,
                "TS1137".into(),
                "Expression or comma expected.".into()
            )
        ]
    );
    assert_eq!(
        diagnostics[0].frame,
        "1 | export default async function main() {\n2 |   return [1, 2;\n  |               ^\n3 | }"
    );
}

#[tokio::test]
async fn columns_count_characters() {
    let script = main_with("  const label = \"😀😀 é\"; const n: number = label;\n  return n;");
    let diagnostics = check(&script).await;
    assert_eq!(
        summary(&diagnostics),
        [(
            5,
            31,
            "TS2322".into(),
            "Type 'string' is not assignable to type 'number'.".into()
        )]
    );
    let marked: Vec<&str> = diagnostics[0].frame.lines().collect();
    let column = marked[2].find('^').unwrap() - "  | ".len();
    assert_eq!(
        marked[1]["5 | ".len()..].chars().nth(column),
        Some('n'),
        "{}",
        diagnostics[0].frame
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checks_are_deterministic_and_run_in_parallel() {
    let script = main_with(
        "  const issues = await github.list_issues({ repo: 1, state: \"x\" });\n  sleep(1);\n  return process;",
    );
    let first = check(&script).await;
    assert_eq!(first.len(), 4, "{first:?}");
    let checks: Vec<_> = (0..8)
        .map(|_| {
            let script = script.clone();
            tokio::spawn(async move { CHECKER.check_source(&script, TOOLS).await.unwrap() })
        })
        .collect();
    for check in checks {
        assert_eq!(check.await.unwrap(), first);
    }
}

#[tokio::test]
async fn the_port_checks_a_script_against_a_toolsets_declarations() {
    let declarations = ToolDeclarations {
        typescript: TOOLS.to_owned(),
        prompt: String::new(),
    };
    let checker: &TsgoChecker = &CHECKER;
    let diagnostics = TypeChecker::check(checker, &Script::new(GOOD), &declarations)
        .await
        .unwrap();
    assert_eq!(diagnostics, []);
    let diagnostics = TypeChecker::check(
        checker,
        &Script::new("export default async () => { const x: string = 1; return x; };\n"),
        &declarations,
    )
    .await
    .unwrap();
    assert_eq!(sources(&diagnostics), ["TS2322"]);
}

#[tokio::test]
async fn a_check_past_its_timeout_fails() {
    let checker = TsgoChecker::new(cache_dir()).with_timeout(Duration::ZERO);
    let error = checker.check_source(GOOD, TOOLS).await.unwrap_err();
    assert!(matches!(error, CheckError::Timeout(_)), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tsgo_is_extracted_once_into_the_cache_directory() {
    let cache = tempfile::tempdir().unwrap();
    let checkers: Vec<_> = (0..4)
        .map(|_| TsgoChecker::new(cache.path()))
        .map(|checker| tokio::spawn(async move { checker.install().await.map(Path::to_owned) }))
        .collect();
    let mut executables = Vec::new();
    for checker in checkers {
        executables.push(checker.await.unwrap().unwrap());
    }
    executables.dedup();
    assert_eq!(executables.len(), 1);
    let executable = &executables[0];
    assert!(executable.is_file());
    assert!(
        executable
            .display()
            .to_string()
            .contains(&format!("typescript-{TSGO_VERSION}-")),
        "{}",
        executable.display()
    );
    // Nothing is left of the staging directories.
    let entries: Vec<_> = std::fs::read_dir(cache.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");

    // An extraction that does not match the tarball is replaced.
    let installed = executable.parent().unwrap().parent().unwrap();
    std::fs::write(installed.join(".integrity"), "sha512-other").unwrap();
    std::fs::remove_file(executable).unwrap();
    let checker = TsgoChecker::new(cache.path());
    assert_eq!(checker.install().await.unwrap(), executable);
    assert_eq!(checker.check_source(GOOD, TOOLS).await.unwrap(), []);
}

/// Check latency, measured on the good script: `cargo test -p rewind-check --release --test
/// check -- --ignored --nocapture latency`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a measure, not a test"]
async fn latency() {
    let cache = tempfile::tempdir().unwrap();
    let checker = TsgoChecker::new(cache.path());
    let started = Instant::now();
    checker.install().await.unwrap();
    println!("extraction: {:?}", started.elapsed());
    let started = Instant::now();
    assert_eq!(checker.check_source(GOOD, TOOLS).await.unwrap(), []);
    println!("first check: {:?}", started.elapsed());

    let mut times = Vec::new();
    for _ in 0..50 {
        let started = Instant::now();
        assert_eq!(checker.check_source(GOOD, TOOLS).await.unwrap(), []);
        times.push(started.elapsed());
    }
    times.sort();
    println!(
        "sequential checks: p50 {:?}, p90 {:?}, max {:?}",
        times[25], times[45], times[49]
    );

    let checker = std::sync::Arc::new(checker);
    let started = Instant::now();
    let checks: Vec<_> = (0..64)
        .map(|_| {
            let checker = checker.clone();
            tokio::spawn(async move { checker.check_source(GOOD, TOOLS).await.unwrap() })
        })
        .collect();
    for check in checks {
        check.await.unwrap();
    }
    println!(
        "64 concurrent checks: {:?} in all, {:?} per check",
        started.elapsed(),
        started.elapsed() / 64
    );
}

/// A script with errors everywhere gets the first of them by position, not all: their number,
/// and tsgo's output, are bounded whatever the size of the script.
#[tokio::test]
async fn the_diagnostics_of_a_script_are_bounded() {
    let body = "x;\nfetch;\n".repeat(100_000);
    let script = format!("export default async function main() {{\n{body}}}\n");
    let diagnostics = check(&script).await;
    assert_eq!(diagnostics.len(), TsgoChecker::MAX_DIAGNOSTICS);
    let lines: Vec<u32> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.line)
        .collect();
    assert!(lines.is_sorted(), "{lines:?}");
    assert_eq!((lines[0], lines[1]), (2, 3));
    assert_eq!(
        sources(&diagnostics[..2]),
        ["TS2304", "absent-global"],
        "{:?}",
        &diagnostics[..2]
    );
}
