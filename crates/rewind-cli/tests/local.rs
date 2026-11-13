//! The commands that run in this process, without a server: `rewind check`, `rewind test` and
//! `rewind replay` of an exported journal; and the e2e corpus driven through them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rewind_api::scripts::{CheckResult, TestResult};
use rewind_cli::{Console, Exit};
use serde_json::{Value, json};

struct Run {
    exit: Exit,
    out: String,
    err: String,
}

/// `rewind <args…>` in this process. No `--dir`: a local command must not need a server, and
/// the state directory it would use does not exist.
async fn rewind(args: &[&str]) -> Run {
    let mut argv = vec!["--dir", "/nonexistent/.rewind"];
    argv.extend(args);
    let (mut console, captured) = Console::captured();
    let exit = rewind_cli::run(argv, &mut console).await;
    Run {
        exit,
        out: captured.stdout(),
        err: captured.stderr(),
    }
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../rewind-e2e/corpus/epoch-1")
}

// -------------------------------------------------------------------------------------------
// rewind check

#[tokio::test]
async fn check_reports_type_errors_rules_and_start_checks() {
    let dir = tempfile::tempdir().unwrap();
    let good = write(
        dir.path(),
        "good.ts",
        "import { sleep } from \"rewind\";\nexport default async function main() {\n  await sleep(\"1s\");\n  return 1;\n}\n",
    );
    let run = rewind(&["check", good.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::Success, "{}{}", run.out, run.err);
    assert!(run.out.ends_with("good.ts: no problems\n"), "{}", run.out);

    let bad = write(
        dir.path(),
        "bad.ts",
        "import { sleep } from \"rewind\";\nexport default async function main() {\n  const name: string = 42;\n  sleep(\"1s\");\n  return name;\n}\n",
    );
    let run = rewind(&["check", bad.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::ScriptRefused, "{}", run.out);
    assert!(run.out.contains("bad.ts:3:9 error TS2322"), "{}", run.out);
    assert!(
        run.out.contains("error rewind/floating-promise"),
        "{}",
        run.out
    );
    assert!(
        run.out.contains("bad.ts: 2 errors, 0 warnings"),
        "{}",
        run.out
    );

    let run = rewind(&["--json", "check", bad.to_str().unwrap()]).await;
    let result: CheckResult = serde_json::from_str(&run.out).unwrap();
    assert!(!result.ok);
    let codes: Vec<&str> = result.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert!(
        codes.contains(&"TS2322") && codes.contains(&"rewind/floating-promise"),
        "{codes:?}"
    );

    // The runtime's checks at start join tsgo's: a script without a default export.
    let headless = write(dir.path(), "headless.ts", "export const x = 1;\n");
    let run = rewind(&["check", headless.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::ScriptRefused, "{}", run.out);
    assert!(
        run.out.contains("error rewind/invalid-script"),
        "{}",
        run.out
    );
}

#[tokio::test]
async fn check_reads_a_toolset_file() {
    let dir = tempfile::tempdir().unwrap();
    let toolset = write(
        dir.path(),
        "toolset.json",
        &json!({
            "host": [{
                "name": "ask_user",
                "description": "Ask the end user a question.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "question": { "type": "string" } },
                    "required": ["question"]
                },
                "outputSchema": { "type": "string" }
            }]
        })
        .to_string(),
    );
    let script = write(
        dir.path(),
        "ask.ts",
        "import { ask_user } from \"rewind:tools\";\nexport default async function main() {\n  const answer = await ask_user({ question: 42 });\n  return answer.toUpperCase();\n}\n",
    );
    let run = rewind(&[
        "check",
        script.to_str().unwrap(),
        "--toolset",
        toolset.to_str().unwrap(),
    ])
    .await;
    assert_eq!(run.exit, Exit::ScriptRefused, "{}{}", run.out, run.err);
    assert!(run.out.contains("ask.ts:3:"), "{}", run.out);
    assert!(run.out.contains("TS2322"), "{}", run.out);

    let fixed = write(
        dir.path(),
        "fixed.ts",
        "import { ask_user } from \"rewind:tools\";\nexport default async function main() {\n  const answer = await ask_user({ question: \"why?\" });\n  return answer.toUpperCase();\n}\n",
    );
    let run = rewind(&[
        "check",
        fixed.to_str().unwrap(),
        "--toolset",
        toolset.to_str().unwrap(),
    ])
    .await;
    assert_eq!(run.exit, Exit::Success, "{}", run.out);
    // Without the toolset, `rewind:tools` exports nothing.
    let run = rewind(&["check", fixed.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::ScriptRefused, "{}", run.out);
}

// -------------------------------------------------------------------------------------------
// rewind test

const TRIAGE: &str = r#"import { github, notify } from "rewind:tools";
import { signal, sleep } from "rewind";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo });
  await sleep("2d");
  const decision = await signal<{ close: boolean }>("approval");
  let closed = 0;
  if (decision.close) {
    for (const issue of issues) {
      await github.close_issue({ number: issue.number });
      closed++;
    }
  }
  await notify({ text: `closed ${closed}` });
  return { closed, repo: input.repo };
}
"#;

#[tokio::test]
async fn test_runs_a_scenario_with_static_responses_and_mocks() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "triage.ts", TRIAGE);
    // rewind.tools.json next to the scenario; the scenario's `tools` override it tool by tool.
    write(
        dir.path(),
        "rewind.tools.json",
        r#"{"github.list_issues": [{"number": 1}], "notify": {"$error": "slack is down"}}"#,
    );
    write(
        dir.path(),
        "triage.mocks.ts",
        "export default { github: { close_issue: ({ number }) => ({ number, closed: true }) } };\n",
    );
    let scenario = write(
        dir.path(),
        "scenario.json",
        &json!({
            "script": "triage.ts",
            "input": { "repo": "acme/api" },
            "mocks": "triage.mocks.ts",
            "tools": {
                "github.list_issues": [{ "number": 4 }, { "number": 7 }, { "number": 9 }],
                "notify": { "ok": true }
            },
            "signals": [{ "name": "approval", "payload": { "close": true } }],
            "expect": {
                "status": "completed",
                "output": { "closed": 3 },
                "calls": { "github.list_issues": 1, "github.close_issue": 3, "notify": 1 }
            }
        })
        .to_string(),
    );
    let run = rewind(&["test", scenario.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::Success, "{}{}", run.out, run.err);
    assert!(run.out.starts_with("PASS "), "{}", run.out);
    assert!(
        run.out.contains("github.close_issue"),
        "the trace is printed: {}",
        run.out
    );

    let run = rewind(&["--json", "test", scenario.to_str().unwrap()]).await;
    let result: TestResult = serde_json::from_str(&run.out).unwrap();
    assert!(result.passed);
    assert_eq!(
        result.output,
        Some(json!({"closed": 3, "repo": "acme/api"}))
    );

    // rewind.tools.json alone: its notify fails, and the expectations say what does not hold.
    let failing = write(
        dir.path(),
        "failing.json",
        &json!({
            "script": "triage.ts",
            "input": { "repo": "acme/api" },
            "signals": [{ "name": "approval", "when": "3d", "payload": { "close": false } }],
            "expect": { "status": "completed", "calls": { "github.list_issues": 2 } }
        })
        .to_string(),
    );
    let run = rewind(&["test", failing.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::Failed, "{}{}", run.out, run.err);
    assert!(run.out.starts_with("FAIL "), "{}", run.out);
    assert!(
        run.out.contains(
            "the run is failed (ToolError: notify failed: slack is down), expected completed"
        ),
        "{}",
        run.out
    );
    assert!(
        run.out
            .contains("github.list_issues was called 1 times, expected 2"),
        "{}",
        run.out
    );

    write(
        dir.path(),
        "broken.ts",
        "export default async function main( {\n",
    );
    let refused = write(
        dir.path(),
        "refused.json",
        r#"{"script": "broken.ts", "expect": {"status": "completed"}}"#,
    );
    let run = rewind(&["test", refused.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::ScriptRefused, "{}{}", run.out, run.err);
    assert!(run.err.contains("the script is invalid"), "{}", run.err);
}

/// `rewind test` replays a recorded run as a scenario: the run's cassette answers its calls.
#[tokio::test]
async fn test_replays_a_recorded_cassette() {
    let dir = tempfile::tempdir().unwrap();
    let case = corpus().join("blue/waiting_for_a_signal");
    let journal = rewind_format::decode_journal(
        &std::fs::read_to_string(case.join("journal.v1.json")).unwrap(),
    )
    .unwrap();
    let cassette =
        rewind_core::Cassette::from_history(&journal.entries, rewind_core::Matching::Sequential)
            .unwrap();
    write(
        dir.path(),
        "recorded.json",
        &rewind_format::encode_cassette(&cassette),
    );
    std::fs::copy(case.join("script.ts"), dir.path().join("script.ts")).unwrap();
    let scenario = write(
        dir.path(),
        "scenario.json",
        &json!({
            "script": "script.ts",
            "cassette": "recorded.json",
            // The recorded run stopped at the signal: what comes after has a static response.
            "tools": { "github.close_issue": { "closed": true } },
            "signals": [{ "name": "approval", "payload": { "close": true } }],
            "expect": {
                "status": "completed",
                "output": { "close": true },
                "calls": { "github.get_issue": 1, "github.close_issue": 1 }
            }
        })
        .to_string(),
    );
    let run = rewind(&["--json", "test", scenario.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::Success, "{}{}", run.out, run.err);
    let result: TestResult = serde_json::from_str(&run.out).unwrap();
    assert!(result.passed, "{:?}", result.failures);
    // The recorded answer: the cassette echoes the args the run sent.
    assert!(result.trace.contains("get_issue"), "{}", result.trace);
}

// -------------------------------------------------------------------------------------------
// rewind replay of an exported journal

#[tokio::test]
async fn replay_reports_compatibility_with_a_corrected_script() {
    let dir = tempfile::tempdir().unwrap();
    let case = corpus().join("blue/waiting_for_a_signal");
    let journal = case.join("journal.v1.json");

    let run = rewind(&["replay", journal.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::Success, "{}{}", run.out, run.err);
    assert!(
        run.out.contains("the script replays the 4 entries"),
        "{}",
        run.out
    );

    // The same calls, moved to other lines: still compatible, the call reused.
    let moved = write(
        dir.path(),
        "moved.ts",
        "import { github } from \"rewind:tools\";\nimport { signal } from \"rewind\";\n\n\nexport default async function main() {\n  const issue = await github.get_issue({ number: 4 });\n  const decision = await signal<{ close: boolean }>(\"approval\");\n  if (decision.close) await github.close_issue({ number: issue.number });\n  return decision;\n}\n",
    );
    let run = rewind(&[
        "replay",
        journal.to_str().unwrap(),
        "--script",
        moved.to_str().unwrap(),
    ])
    .await;
    assert_eq!(run.exit, Exit::Success, "{}{}", run.out, run.err);
    assert!(run.out.contains("the script is compatible"), "{}", run.out);
    assert!(
        run.out.contains("a fork reuses: #1 github.get_issue"),
        "{}",
        run.out
    );

    // Another argument: where it diverges.
    let changed = write(
        dir.path(),
        "changed.ts",
        "import { github } from \"rewind:tools\";\nexport default async function main() {\n  return await github.get_issue({ number: 5 });\n}\n",
    );
    let run = rewind(&[
        "--json",
        "replay",
        journal.to_str().unwrap(),
        "--script",
        changed.to_str().unwrap(),
    ])
    .await;
    assert_eq!(run.exit, Exit::Failed, "{}{}", run.out, run.err);
    let report: Value = serde_json::from_str(&run.out).unwrap();
    assert_eq!(report["matches"], false);
    assert!(
        report["verdict"].as_str().unwrap().contains("diverges"),
        "{report}"
    );

    let run = rewind(&["replay", dir.path().join("missing.json").to_str().unwrap()]).await;
    assert_eq!(
        run.exit,
        Exit::Usage,
        "a missing file starts no server: {}",
        run.err
    );
}

// -------------------------------------------------------------------------------------------
// The e2e corpus, driven through the CLI

/// Every committed journal of the corpus replays through `rewind replay <file>`, and each red
/// case that alters its replay fails exactly as specified.
#[tokio::test]
async fn the_corpus_replays_through_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let mut replayed = 0;
    for (name, case) in cases() {
        let journal = case.join("journal.v1.json");
        if !journal.exists() {
            continue;
        }
        let scenario: Value =
            serde_json::from_str(&std::fs::read_to_string(case.join("scenario.json")).unwrap())
                .unwrap();
        let run = rewind(&["--json", "replay", journal.to_str().unwrap()]).await;
        assert_eq!(run.exit, Exit::Success, "{name}: {}{}", run.out, run.err);
        replayed += 1;

        let Some(check) = scenario.get("replay") else {
            continue;
        };
        let expected = check
            .get("diverges")
            .or_else(|| check.get("error"))
            .and_then(Value::as_str)
            .unwrap();
        let run = match (check.get("script"), check.get("drop_entry")) {
            (Some(script), None) => {
                let script = case.join(script.as_str().unwrap());
                rewind(&[
                    "replay",
                    journal.to_str().unwrap(),
                    "--script",
                    script.to_str().unwrap(),
                ])
                .await
            }
            (None, Some(index)) => {
                let mut tampered =
                    rewind_format::decode_journal(&std::fs::read_to_string(&journal).unwrap())
                        .unwrap();
                tampered
                    .entries
                    .remove(usize::try_from(index.as_u64().unwrap()).unwrap());
                let path = dir.path().join(format!("{}.json", name.replace('/', "_")));
                std::fs::write(&path, rewind_format::encode_journal(&tampered)).unwrap();
                rewind(&["replay", path.to_str().unwrap()]).await
            }
            other => panic!("{name}: unexpected replay check {other:?}"),
        };
        assert_eq!(run.exit, Exit::Failed, "{name}: {}{}", run.out, run.err);
        assert!(
            run.out.contains(expected),
            "{name}: {} does not say {expected:?}",
            run.out
        );
    }
    // Every case has its journal, but those whose start is refused.
    assert!(replayed >= 54, "only {replayed} journals replayed");
}

/// The corpus cases whose tools and steps a scenario of `rewind test` can express run through
/// `rewind test`, and meet their expectations: status, output, error, calls, abort, refused start.
/// The others need what only the corpus harness drives (approvals, virtual time moved by hand,
/// a host worker, limits, forks, MCP servers, `http` hosts, idempotency keys, transient
/// failures): they are listed, so that the selection cannot shrink silently.
#[tokio::test]
async fn the_corpus_runs_through_rewind_test() {
    let dir = tempfile::tempdir().unwrap();
    let mut ran = Vec::new();
    let mut skipped = Vec::new();
    for (name, case) in cases() {
        let scenario: Value =
            serde_json::from_str(&std::fs::read_to_string(case.join("scenario.json")).unwrap())
                .unwrap();
        // Its toolset lacks the `webhook` built-in, which every toolset of a server, and of
        // `rewind test`, pins.
        let without_webhook = name == "red/webhook_without_the_built_in";
        let Some(translated) = translate(&scenario).filter(|_| !without_webhook) else {
            skipped.push(name);
            continue;
        };
        let work = dir.path().join(name.replace('/', "_"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(case.join("script.ts"), work.join("script.ts")).unwrap();
        if let Some(mocks) = &translated.mocks {
            write(&work, "mocks.ts", mocks);
        }
        let path = write(&work, "scenario.json", &translated.scenario.to_string());
        let run = rewind(&["--json", "test", path.to_str().unwrap()]).await;
        let expect = &scenario["expect"];
        if let Some(message) = expect.get("start_error").and_then(Value::as_str) {
            assert_ne!(run.exit, Exit::Success, "{name}: the start must be refused");
            let problem: rewind_api::Problem = serde_json::from_str(run.err.trim())
                .unwrap_or_else(|error| panic!("{name}: {error}: {}", run.err));
            assert!(
                problem.detail.contains(message),
                "{name}: {} does not say {message:?}",
                problem.detail
            );
            ran.push(name);
            continue;
        }
        let result: TestResult = serde_json::from_str(&run.out)
            .unwrap_or_else(|error| panic!("{name}: {error}: {}{}", run.out, run.err));
        assert!(
            result.passed,
            "{name}: {:?}\n{}",
            result.failures, result.trace
        );
        // A scenario whose expectations hold passes, however its run ended.
        assert_eq!(
            run.exit,
            Exit::Success,
            "{name}: the run is {}",
            result.status
        );
        if let Some(abort) = expect.get("abort") {
            let found = result.abort.as_ref().expect("an abort");
            let kind = serde_json::to_value(found.kind).unwrap();
            assert_eq!(kind, abort["kind"], "{name}");
            assert!(
                found.message.contains(abort["message"].as_str().unwrap()),
                "{name}: {}",
                found.message
            );
        }
        ran.push(name);
    }
    eprintln!("ran through rewind test: {ran:?}\nskipped: {skipped:?}");
    assert_eq!(
        skipped,
        [
            "blue/approval_granted",
            "blue/args_checked_against_the_schema",
            "blue/at_most_once_outcome_unknown",
            "blue/cancellation_cleanup",
            "blue/cpu_budget_per_trigger",
            "blue/credentials_renewed",
            "blue/fan_out_out_of_order",
            "blue/fetch_with_the_http_tool",
            "blue/fork_calls_again_what_its_parent_left",
            "blue/fork_live_from_an_entry",
            "blue/fork_reuses_recorded_calls",
            "blue/idempotent_signal",
            "blue/interrupted_call_retried",
            "blue/mcp_content_helpers",
            "blue/signals_buffered_then_timed_out",
            "blue/tool_called_without_arguments",
            "blue/transient_failure_retried",
            "blue/triage_workflow",
            "red/approval_rejected",
            "red/built_in_imported_from_rewind_tools",
            "red/cancellation_grace_exceeded",
            "red/cancelled",
            "red/cancelled_inside_set_timeout",
            "red/cpu_limit",
            "red/cpu_limit_in_a_delivery",
            "red/deadline_passed",
            "red/fetch_refused",
            "red/fork_outcome_unknown",
            "red/heap_limit",
            "red/invalid_args",
            "red/mcp_content_that_is_not_json",
            "red/too_many_commands",
            "red/webhook_without_the_built_in",
        ],
        "the corpus cases left to the corpus harness"
    );
    assert!(ran.len() >= 25, "only {} cases ran: {ran:?}", ran.len());
}

/// The cases of the corpus, `epoch-1/<color>/<name>`, sorted.
fn cases() -> Vec<(String, PathBuf)> {
    let mut cases = Vec::new();
    for color in ["blue", "red"] {
        for entry in std::fs::read_dir(corpus().join(color)).unwrap() {
            let path = entry.unwrap().path();
            if path.join("scenario.json").exists() {
                let name = format!("{color}/{}", path.file_name().unwrap().to_str().unwrap());
                cases.push((name, path));
            }
        }
    }
    cases.sort();
    cases
}

struct Translated {
    scenario: Value,
    mocks: Option<String>,
}

/// A corpus scenario as a scenario of `rewind test`, when it can be one: plain values and
/// `$error` become static responses; `$echo`, `$sequence` and failures with a kind or data
/// become TypeScript mock handlers; signals are sent once the run is blocked, without their
/// idempotency keys unless a key is sent twice.
fn translate(scenario: &Value) -> Option<Translated> {
    let fields = scenario.as_object()?;
    let allowed = [
        "description",
        "input",
        "tools",
        "steps",
        "ttl",
        "expect",
        "replay",
    ];
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return None;
    }
    let mut signals = Vec::new();
    let mut keys = Vec::new();
    for step in scenario["steps"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let signal = step.get("signal")?;
        // An idempotency key changes nothing unless it is sent again.
        if let Some(key) = signal.get("key") {
            if keys.contains(&key) {
                return None;
            }
            keys.push(key);
        }
        signals.push(json!({
            "name": signal["name"],
            "payload": signal.get("payload").cloned().unwrap_or(Value::Null),
        }));
    }
    let mut statics = BTreeMap::new();
    let mut handlers = Vec::new();
    for (tool, response) in scenario["tools"].as_object().cloned().unwrap_or_default() {
        if tool == "http" {
            return None;
        }
        match handler(&response)? {
            Handler::Static(value) => {
                statics.insert(tool, value);
            }
            Handler::Script(body) => handlers.push(format!("  {}: {body},", json!(tool))),
        }
    }
    let mocks = (!handlers.is_empty()).then(|| {
        format!(
            "const counters: Record<string, number> = {{}};\nfunction next(tool: string): number {{\n  counters[tool] = (counters[tool] ?? 0) + 1;\n  return counters[tool] - 1;\n}}\nfunction fail(failure: {{ message: string; kind?: string; data?: unknown }}): never {{\n  throw failure;\n}}\nexport default {{\n{}\n}};\n",
            handlers.join("\n")
        )
    });
    let expect = &scenario["expect"];
    let mut expected = serde_json::Map::new();
    for key in ["status", "output", "error", "calls"] {
        if let Some(value) = expect.get(key) {
            expected.insert(key.to_owned(), value.clone());
        }
    }
    let mut translated = json!({
        "script": "script.ts",
        "input": scenario.get("input").cloned().unwrap_or(json!({})),
        "tools": statics,
        "signals": signals,
        "expect": expected,
    });
    if let Some(ttl) = scenario.get("ttl") {
        translated["ttl"] = ttl.clone();
    }
    if mocks.is_some() {
        translated["mocks"] = json!("mocks.ts");
    }
    Some(Translated {
        scenario: translated,
        mocks,
    })
}

enum Handler {
    Static(Value),
    Script(String),
}

/// How a mock answers one corpus response; `None` for the responses only the corpus's own
/// tools give (transient failures, deferrals, crashes, refused credentials).
fn handler(response: &Value) -> Option<Handler> {
    let Some(fields) = response.as_object() else {
        return Some(Handler::Static(response.clone()));
    };
    let special: Vec<&String> = fields.keys().filter(|key| key.starts_with('$')).collect();
    if special.is_empty() {
        return Some(Handler::Static(response.clone()));
    }
    if fields.len() == 1 && fields.get("$error").is_some_and(Value::is_string) {
        return Some(Handler::Static(response.clone()));
    }
    if fields.len() == 1 && fields.get("$echo") == Some(&Value::Bool(true)) {
        return Some(Handler::Script("(args) => args".into()));
    }
    if let Some(error) = fields.get("$error") {
        if fields
            .keys()
            .any(|key| !["$error", "$kind", "$data"].contains(&key.as_str()))
        {
            return None;
        }
        let failure = json!({
            "message": error,
            "kind": fields.get("$kind").cloned().unwrap_or(json!("error")),
            "data": fields.get("$data").cloned().unwrap_or(Value::Null),
        });
        return Some(Handler::Script(format!("() => fail({failure})")));
    }
    if let Some(Value::Array(sequence)) = fields.get("$sequence") {
        if fields.len() != 1 {
            return None;
        }
        let mut answers = Vec::new();
        for answer in sequence {
            answers.push(match handler(answer)? {
                Handler::Static(value) if value.get("$error").is_some() => {
                    format!("() => fail({})", json!({ "message": value["$error"] }))
                }
                Handler::Static(value) => format!("() => ({value})"),
                Handler::Script(body) => body,
            });
        }
        // Past the end, the call fails, as with the corpus's tools.
        return Some(Handler::Script(format!(
            "(args, call) => {{ const answers = [{}]; const n = next(call.tool); if (n >= answers.length) fail({{ message: `${{call.tool}} has no response left`, kind: \"unavailable\" }}); return answers[n](args); }}",
            answers.join(", ")
        )));
    }
    None
}
