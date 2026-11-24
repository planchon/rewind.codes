//! The commands that run in this process, without a server: `rewind check`, `rewind test` and
//! `rewind replay` of a run export; and the e2e corpus driven through them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rewind_api::scripts::CheckResult;
use rewind_api::testing::TestRun;
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
    assert_eq!(run.exit, Exit::CodeRefused, "{}", run.out);
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

    // The runtime's checks at start join tsgo's: a code without a default export.
    let headless = write(dir.path(), "headless.ts", "export const x = 1;\n");
    let run = rewind(&["check", headless.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::CodeRefused, "{}", run.out);
    assert!(run.out.contains("error rewind/invalid-code"), "{}", run.out);
}

#[tokio::test]
async fn check_reads_an_action_set_file() {
    let dir = tempfile::tempdir().unwrap();
    let action_set = write(
        dir.path(),
        "action_set.json",
        &json!({
            "harness": [{
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
    let code = write(
        dir.path(),
        "ask.ts",
        "import { ask_user } from \"rewind:actions\";\nexport default async function main() {\n  const answer = await ask_user({ question: 42 });\n  return answer.toUpperCase();\n}\n",
    );
    let run = rewind(&[
        "check",
        code.to_str().unwrap(),
        "--action-set",
        action_set.to_str().unwrap(),
    ])
    .await;
    assert_eq!(run.exit, Exit::CodeRefused, "{}{}", run.out, run.err);
    assert!(run.out.contains("ask.ts:3:"), "{}", run.out);
    assert!(run.out.contains("TS2322"), "{}", run.out);

    let fixed = write(
        dir.path(),
        "fixed.ts",
        "import { ask_user } from \"rewind:actions\";\nexport default async function main() {\n  const answer = await ask_user({ question: \"why?\" });\n  return answer.toUpperCase();\n}\n",
    );
    let run = rewind(&[
        "check",
        fixed.to_str().unwrap(),
        "--action-set",
        action_set.to_str().unwrap(),
    ])
    .await;
    assert_eq!(run.exit, Exit::Success, "{}", run.out);
    // Without the action set, `rewind:actions` exports nothing.
    let run = rewind(&["check", fixed.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::CodeRefused, "{}", run.out);
}

// -------------------------------------------------------------------------------------------
// rewind test

const TRIAGE: &str = r#"import { github, notify } from "rewind:actions";
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
async fn test_runs_tests_with_static_responses_and_handlers() {
    let dir = tempfile::tempdir().unwrap();
    let code = write(dir.path(), "triage.ts", TRIAGE);
    // rewind.actions.json next to the tests: tried after each test's own world.
    write(
        dir.path(),
        "rewind.actions.json",
        r#"{"github.list_issues": [{"number": 1}], "notify": {"$error": "slack is down"}}"#,
    );
    write(
        dir.path(),
        "triage.handlers.ts",
        "export default { github: { close_issue: ({ number }) => ({ number, closed: true }) } };\n",
    );
    // triage.tests.json next to the code, found by default.
    write(
        dir.path(),
        "triage.tests.json",
        &json!([{
            "name": "closes the three once approved",
            "input": { "repo": "acme/api" },
            "world": {
                "actions": {
                    "github.list_issues": { "returns": [{ "number": 4 }, { "number": 7 }, { "number": 9 }] },
                    "notify": { "returns": { "ok": true } }
                },
                "handlers": "triage.handlers.ts"
            },
            "signals": [{ "name": "approval", "payload": { "close": true } }],
            "expect": {
                "status": "completed",
                "output": { "closed": 3 },
                "calls": [
                    { "action": "github.list_issues", "times": 1 },
                    { "action": "github.close_issue", "times": 3 },
                    { "action": "notify", "times": 1 }
                ]
            }
        }])
        .to_string(),
    );
    let run = rewind(&["test", code.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::Success, "{}{}", run.out, run.err);
    assert!(
        run.out.starts_with("PASS closes the three once approved"),
        "{}",
        run.out
    );
    assert!(run.out.contains("1 of 1 tests passed"), "{}", run.out);

    let run = rewind(&["--json", "test", code.to_str().unwrap()]).await;
    let result: TestRun = serde_json::from_str(&run.out).unwrap();
    assert!(result.passed);
    assert_eq!(
        result.tests[0].output,
        Some(json!({"closed": 3, "repo": "acme/api"}))
    );

    // rewind.actions.json alone: its notify fails, and the expectations say what does not hold.
    let failing = write(
        dir.path(),
        "failing.tests.json",
        &json!([{
            "name": "rejects in three days",
            "input": { "repo": "acme/api" },
            "signals": [{ "name": "approval", "when": "3d", "payload": { "close": false } }],
            "expect": {
                "status": "completed",
                "calls": [{ "action": "github.list_issues", "times": 2 }]
            }
        }])
        .to_string(),
    );
    let run = rewind(&[
        "test",
        code.to_str().unwrap(),
        "--tests",
        failing.to_str().unwrap(),
    ])
    .await;
    assert_eq!(run.exit, Exit::Failed, "{}{}", run.out, run.err);
    assert!(
        run.out.starts_with("FAIL rejects in three days"),
        "{}",
        run.out
    );
    assert!(
        run.out.contains(
            "the run is failed (ActionError: notify failed: slack is down), expected completed"
        ),
        "{}",
        run.out
    );
    assert!(
        run.out
            .contains("github.list_issues was called once, expected 2 times"),
        "{}",
        run.out
    );
    // The transcript of a test that did not pass is printed.
    assert!(run.out.contains("github.list_issues"), "{}", run.out);

    let broken = write(
        dir.path(),
        "broken.ts",
        "export default async function main( {\n",
    );
    write(
        dir.path(),
        "broken.tests.json",
        r#"[{"name": "starts", "expect": {"status": "completed"}}]"#,
    );
    let run = rewind(&["test", broken.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::CodeRefused, "{}{}", run.out, run.err);
    assert!(run.err.contains("the code is invalid"), "{}", run.err);

    // No tests: a usage error that names the file it looked for.
    let alone = write(dir.path(), "alone.ts", TRIAGE);
    let run = rewind(&["test", alone.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::Usage, "{}{}", run.out, run.err);
    assert!(run.err.contains("alone.tests.json"), "{}", run.err);
}

/// `rewind test` replays a recorded run: the run's cassette answers its calls.
#[tokio::test]
async fn test_replays_a_recorded_cassette() {
    let dir = tempfile::tempdir().unwrap();
    let case = corpus().join("blue/waiting_for_a_signal");
    let export = rewind_format::decode_run_export(
        &std::fs::read_to_string(case.join("history.v1.json")).unwrap(),
    )
    .unwrap();
    let cassette =
        rewind_core::Cassette::from_history(&export.events, rewind_core::Matching::Sequential)
            .unwrap();
    write(
        dir.path(),
        "recorded.json",
        &rewind_format::encode_cassette(&cassette),
    );
    std::fs::copy(case.join("workflow.ts"), dir.path().join("workflow.ts")).unwrap();
    write(
        dir.path(),
        "workflow.tests.json",
        &json!([{
            "name": "closes once approved",
            "world": {
                "cassette": "recorded.json",
                // The recorded run stopped at the signal: what comes after has a rule.
                "actions": { "github.close_issue": { "returns": { "closed": true } } }
            },
            "signals": [{ "name": "approval", "payload": { "close": true } }],
            "expect": {
                "status": "completed",
                "output": { "close": true },
                "calls": [
                    { "action": "github.get_issue", "times": 1 },
                    { "action": "github.close_issue", "times": 1 }
                ]
            }
        }])
        .to_string(),
    );
    let code = dir.path().join("workflow.ts");
    let run = rewind(&["--json", "test", code.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::Success, "{}{}", run.out, run.err);
    let result: TestRun = serde_json::from_str(&run.out).unwrap();
    assert!(result.passed, "{:?}", result.tests[0].failures);
    // The recorded answer: the cassette echoes the args the run sent.
    assert!(
        result.tests[0].transcript.contains("get_issue"),
        "{}",
        result.tests[0].transcript
    );
}

// -------------------------------------------------------------------------------------------
// rewind replay of a run export

#[tokio::test]
async fn replay_reports_compatibility_with_a_corrected_script() {
    let dir = tempfile::tempdir().unwrap();
    let case = corpus().join("blue/waiting_for_a_signal");
    let export = case.join("history.v1.json");

    let run = rewind(&["replay", export.to_str().unwrap()]).await;
    assert_eq!(run.exit, Exit::Success, "{}{}", run.out, run.err);
    assert!(
        run.out.contains("the code replays the 4 events"),
        "{}",
        run.out
    );

    // The same calls, moved to other lines: still compatible, the call reused.
    let moved = write(
        dir.path(),
        "moved.ts",
        "import { github } from \"rewind:actions\";\nimport { signal } from \"rewind\";\n\n\nexport default async function main() {\n  const issue = await github.get_issue({ number: 4 });\n  const decision = await signal<{ close: boolean }>(\"approval\");\n  if (decision.close) await github.close_issue({ number: issue.number });\n  return decision;\n}\n",
    );
    let run = rewind(&[
        "replay",
        export.to_str().unwrap(),
        "--code",
        moved.to_str().unwrap(),
    ])
    .await;
    assert_eq!(run.exit, Exit::Success, "{}{}", run.out, run.err);
    assert!(run.out.contains("the code is compatible"), "{}", run.out);
    assert!(
        run.out.contains("a fork reuses: #1 github.get_issue"),
        "{}",
        run.out
    );

    // Another argument: where it diverges.
    let changed = write(
        dir.path(),
        "changed.ts",
        "import { github } from \"rewind:actions\";\nexport default async function main() {\n  return await github.get_issue({ number: 5 });\n}\n",
    );
    let run = rewind(&[
        "--json",
        "replay",
        export.to_str().unwrap(),
        "--code",
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

/// Every committed history of the corpus replays through `rewind replay <file>`, and each red
/// case that alters its replay fails exactly as specified.
#[tokio::test]
async fn the_corpus_replays_through_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let mut replayed = 0;
    for (name, case) in cases() {
        let export = case.join("history.v1.json");
        if !export.exists() {
            continue;
        }
        let scenario: Value =
            serde_json::from_str(&std::fs::read_to_string(case.join("scenario.json")).unwrap())
                .unwrap();
        let run = rewind(&["--json", "replay", export.to_str().unwrap()]).await;
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
        let run = match (check.get("code"), check.get("drop_event")) {
            (Some(code), None) => {
                let code = case.join(code.as_str().unwrap());
                rewind(&[
                    "replay",
                    export.to_str().unwrap(),
                    "--code",
                    code.to_str().unwrap(),
                ])
                .await
            }
            (None, Some(index)) => {
                let mut tampered =
                    rewind_format::decode_run_export(&std::fs::read_to_string(&export).unwrap())
                        .unwrap();
                tampered
                    .events
                    .remove(usize::try_from(index.as_u64().unwrap()).unwrap());
                let path = dir.path().join(format!("{}.json", name.replace('/', "_")));
                std::fs::write(&path, rewind_format::encode_run_export(&tampered)).unwrap();
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
    // Every case has its history, but those whose start is refused.
    assert!(replayed >= 54, "only {replayed} histories replayed");
}

/// The corpus cases whose actions and steps a scenario of `rewind test` can express run through
/// `rewind test`, and meet their expectations: status, output, error, calls, abort, refused start.
/// The others need what only the corpus harness drives (approvals, virtual time moved by hand,
/// a harness worker, limits, forks, MCP servers, `http` hosts, idempotency keys, transient
/// failures, workflows registered before the run): they are listed, so that the selection
/// cannot shrink silently.
#[tokio::test]
async fn the_corpus_runs_through_rewind_test() {
    let dir = tempfile::tempdir().unwrap();
    let mut ran = Vec::new();
    let mut skipped = Vec::new();
    for (name, case) in cases() {
        let scenario: Value =
            serde_json::from_str(&std::fs::read_to_string(case.join("scenario.json")).unwrap())
                .unwrap();
        // Its action set lacks the `webhook` built-in, which every action set of a server, and of
        // `rewind test`, pins.
        let without_webhook = name == "red/webhook_without_the_built_in";
        let Some(translated) = translate(&scenario).filter(|_| !without_webhook) else {
            skipped.push(name);
            continue;
        };
        let work = dir.path().join(name.replace('/', "_"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(case.join("workflow.ts"), work.join("workflow.ts")).unwrap();
        if let Some(handlers) = &translated.handlers {
            write(&work, "handlers.ts", handlers);
        }
        write(
            &work,
            "workflow.tests.json",
            &Value::Array(vec![translated.test.clone()]).to_string(),
        );
        let code = work.join("workflow.ts");
        let run = rewind(&["--json", "test", code.to_str().unwrap()]).await;
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
        let run_result: TestRun = serde_json::from_str(&run.out)
            .unwrap_or_else(|error| panic!("{name}: {error}: {}{}", run.out, run.err));
        let result = &run_result.tests[0];
        assert!(
            result.passed,
            "{name}: {:?}\n{}",
            result.failures, result.transcript
        );
        // A test whose expectations hold passes, however its run ended.
        assert_eq!(
            run.exit,
            Exit::Success,
            "{name}: the run is {:?}",
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
            "blue/action_called_without_arguments",
            "blue/approval_granted",
            "blue/args_checked_against_the_schema",
            "blue/cancellation_cleanup",
            "blue/cpu_budget_per_trigger",
            "blue/credentials_renewed",
            "blue/fan_out_out_of_order",
            "blue/fetch_with_the_http_action",
            "blue/fork_calls_again_what_its_source_left",
            "blue/fork_inherits_its_source_s_secrets",
            "blue/fork_live_from_an_event",
            "blue/fork_reuses_recorded_calls",
            "blue/idempotent_signal",
            "blue/interrupted_call_delivered_again",
            "blue/mcp_content_helpers",
            "blue/not_idempotent_outcome_unknown",
            "blue/secret_deleted_mid_run_resumes_when_put_again",
            "blue/signals_buffered_then_timed_out",
            "blue/transient_failure_delivered_again",
            "blue/triage_workflow",
            "blue/workflow_attached_run_ends_first",
            "blue/workflow_detached_run_lives_on",
            "blue/workflow_start_answered_in_a_test_run",
            "blue/workflow_version_reads_its_secrets",
            "blue/workflows_started_signalled_and_awaited",
            "red/approval_rejected",
            "red/built_in_imported_from_rewind_actions",
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
            "red/namespace_secret_never_falls_back",
            "red/secret_for_a_server_taking_none",
            "red/secret_missing_at_start",
            "red/secret_not_yours_to_attach",
            "red/too_many_commands",
            "red/ttl_too_long",
            "red/webhook_without_the_built_in",
            "red/workflow_input_refused",
            "red/workflow_not_pinned",
            "red/workflow_parent_failure_cancels_its_runs",
            "red/workflow_run_failed_uncaught",
            "red/workflow_secret_missing_in_the_namespace",
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
    test: Value,
    handlers: Option<String>,
}

/// A corpus scenario as a test of `rewind test`, when it can be one: plain values and `$error`
/// become rules; `$echo`, `$sequence` and failures with a kind or data become TypeScript
/// handlers; signals are sent once the run is blocked, without their idempotency keys unless a
/// key is sent twice. A test has no TTL: a scenario that sets one stays with the corpus harness.
fn translate(scenario: &Value) -> Option<Translated> {
    let fields = scenario.as_object()?;
    let allowed = [
        "description",
        "input",
        "actions",
        "steps",
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
    let mut rules = BTreeMap::new();
    let mut handlers = Vec::new();
    for (action, response) in scenario["actions"].as_object().cloned().unwrap_or_default() {
        if action == "http" {
            return None;
        }
        match handler(&response)? {
            Handler::Static(value) => {
                let rule = match value.get("$error").and_then(Value::as_str) {
                    Some(message) => json!({ "error": { "message": message } }),
                    None => json!({ "returns": value }),
                };
                rules.insert(action, rule);
            }
            Handler::Code(body) => handlers.push(format!("  {}: {body},", json!(action))),
        }
    }
    let handlers = (!handlers.is_empty()).then(|| {
        format!(
            "const counters: Record<string, number> = {{}};\nfunction next(action: string): number {{\n  counters[action] = (counters[action] ?? 0) + 1;\n  return counters[action] - 1;\n}}\nfunction fail(failure: {{ message: string; kind?: string; data?: unknown }}): never {{\n  throw failure;\n}}\nexport default {{\n{}\n}};\n",
            handlers.join("\n")
        )
    });
    let expect = &scenario["expect"];
    let mut expected = serde_json::Map::new();
    for key in ["status", "output", "error"] {
        if let Some(value) = expect.get(key) {
            expected.insert(key.to_owned(), value.clone());
        }
    }
    if let Some(calls) = expect.get("calls").and_then(Value::as_object) {
        let calls: Vec<Value> = calls
            .iter()
            .map(|(action, times)| json!({ "action": action, "times": times }))
            .collect();
        expected.insert("calls".to_owned(), Value::Array(calls));
    }
    let mut world = json!({ "actions": rules });
    if handlers.is_some() {
        world["handlers"] = json!("handlers.ts");
    }
    let test = json!({
        "name": "corpus",
        "input": scenario.get("input").cloned().unwrap_or(json!({})),
        "world": world,
        "signals": signals,
        "expect": expected,
    });
    Some(Translated { test, handlers })
}

enum Handler {
    Static(Value),
    Code(String),
}

/// How a handler answers one corpus response; `None` for the responses only the corpus's own
/// actions give (transient failures, deferrals, crashes, refused credentials).
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
        return Some(Handler::Code("(args) => args".into()));
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
        return Some(Handler::Code(format!("() => fail({failure})")));
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
                Handler::Code(body) => body,
            });
        }
        // Past the end, the call fails, as with the corpus's actions.
        return Some(Handler::Code(format!(
            "(args, call) => {{ const answers = [{}]; const n = next(call.action); if (n >= answers.length) fail({{ message: `${{call.action}} has no response left`, kind: \"unavailable\" }}); return answers[n](args); }}",
            answers.join(", ")
        )));
    }
    None
}
