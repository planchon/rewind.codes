//! The sandbox, driven by the engine's run plane in its driver, with memory adapters and virtual
//! time: determinism, durable time, signals, imports, callsites, cancellation, how a code ends,
//! and limits.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use rewind_core::{
    AbortKind, Action, ActionFailure, ActionFailureKind, ActionRoute, ActionSet, Annotation,
    Builtin, Callsite, Code, Command, Completion, Epoch, ErrorInfo, Event, HistoryEvent, McpServer,
    McpTransport, RunId, RunOutcome, RunState, RunStatus, Seed, Seq, Timestamp, canonical_json,
};
use rewind_engine::ports::{
    ActionDispatcher, Call, CallAuth, CodeExecutor, CompiledCode, Dispatched, ExecutorError,
    RunRef, StartCode,
};
use rewind_engine::replay::ReplayEnd;
use rewind_engine::{ReplayVerdict, StartError, StartRun};
use rewind_runtime::{SandboxLimits, V8Executor};
use serde_json::{Value, json};

mod support;

use support::{T0, World};

const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;

type Handler = dyn Fn(&Call) -> Dispatched + Send + Sync;

#[derive(Clone)]
struct Actions(Arc<Handler>);

impl ActionDispatcher for Actions {
    async fn dispatch(&self, call: Call, _auth: CallAuth) -> Dispatched {
        (self.0)(&call)
    }
}

/// Actions that answer every call at once, as `actions` says.
fn answered_by(
    actions: impl Fn(&Call) -> Result<Value, ActionFailure> + Send + Sync + 'static,
) -> Actions {
    Actions(Arc::new(move |call| match actions(call) {
        Ok(result) => Dispatched::Result(result),
        Err(failure) => Dispatched::Failed(failure),
    }))
}

fn issue_actions(call: &Call) -> Result<Value, ActionFailure> {
    let failure = |kind, message: &str| ActionFailure {
        kind,
        message: message.into(),
        data: Some(json!({ "status": 503 })),
    };
    match call.action.name.as_str() {
        "github.get_issue" => Ok(json!({
            "number": call.args["number"],
            "title": format!("issue {}", call.args["number"]),
        })),
        "flaky" => Err(failure(
            ActionFailureKind::OutcomeUnknown,
            "connection reset",
        )),
        "signalUrl" => Ok(json!({
            "url": format!("https://rewind.example.com/hooks/tok_{}", call.args["name"].as_str().unwrap_or_default()),
        })),
        // Echoes the request it was sent, as a JSON body.
        "http" => Ok(json!({
            "status": if call.args["url"].as_str().unwrap_or_default().ends_with("/missing") { 404 } else { 200 },
            "statusText": "OK",
            "url": call.args["url"],
            "headers": [["Content-Type", "application/json"], ["x-b", "1"], ["x-a", "0"], ["x-b", "2"]],
            "redirected": false,
            "body": call.args.to_string(),
        })),
        "failing" => Err(failure(ActionFailureKind::Error, "boom")),
        // MCP content, as `rewind-mcp` returns it for an action pinned without an output schema.
        "crm.lookup" => Ok(json!({
            "content": [
                { "type": "text", "text": "{\"name\":\"Ada\"}" },
                { "type": "image", "data": "aGk=", "mimeType": "image/png" },
            ],
        })),
        "crm.profile" => Ok(json!({
            "content": [{ "type": "text", "text": "Ada" }, { "type": "text", "text": "VIP" }],
            "structuredContent": { "name": "Ada", "vip": true },
        })),
        // The structured content of an action pinned with an output schema.
        "crm.score" => Ok(json!({ "score": 42 })),
        _ => Ok(json!({ "ok": true })),
    }
}

struct Harness {
    world: World<Actions>,
}

async fn harness() -> Harness {
    harness_with(SandboxLimits::default(), 1).await
}

async fn harness_with(limits: SandboxLimits, entropy: u64) -> Harness {
    Harness::new(limits, entropy, answered_by(issue_actions), |_| {}).await
}

struct Run {
    id: RunRef,
    history: Vec<HistoryEvent>,
    state: RunState,
}

impl Run {
    fn output(&self) -> &Value {
        match self.state.outcome() {
            Some(RunOutcome::Completed { output }) => output.value().unwrap(),
            other => panic!(
                "expected a completed run, got {other:?}\n{:#?}",
                self.history
            ),
        }
    }

    fn error(&self) -> &ErrorInfo {
        match self.state.outcome() {
            Some(RunOutcome::Failed { error }) => error,
            other => panic!("expected a failed run, got {other:?}\n{:#?}", self.history),
        }
    }

    fn abort(&self) -> (AbortKind, &str) {
        match self.state.outcome() {
            Some(RunOutcome::Aborted { reason }) => (reason.kind, &reason.message),
            other => panic!(
                "expected an aborted run, got {other:?}\n{:#?}",
                self.history
            ),
        }
    }

    fn events(&self) -> Vec<&'static str> {
        self.history
            .iter()
            .map(|entry| entry.event.name())
            .collect()
    }

    fn commands(&self) -> Vec<&Command> {
        self.history
            .iter()
            .filter_map(|entry| match &entry.event {
                Event::Command(command) => Some(command),
                _ => None,
            })
            .collect()
    }
}

fn action_set() -> ActionSet {
    ActionSet::from_harness_actions([
        "github.get_issue",
        "github.close_issue",
        "audit.record",
        "flaky",
        "failing",
        "slow",
    ])
    .unwrap()
}

impl Harness {
    /// A node running the V8 executor under `limits` and `actions`, its entropy seeded with
    /// `entropy`, configured by `configure`.
    async fn new(
        limits: SandboxLimits,
        entropy: u64,
        actions: Actions,
        configure: impl FnOnce(&mut rewind_engine::plane::NodeConfig),
    ) -> Self {
        let executor = V8Executor::new(limits).expect("TZ=UTC (see .cargo/config.toml)");
        Self {
            world: World::new(executor, actions, entropy, configure).await,
        }
    }

    async fn try_start(&self, source: &str, input: Value) -> Result<RunRef, StartError> {
        self.try_start_with(source, input, action_set()).await
    }

    async fn try_start_with(
        &self,
        source: &str,
        input: Value,
        action_set: ActionSet,
    ) -> Result<RunRef, StartError> {
        let mut request = StartRun::new(Code::new(source), action_set, input);
        request.mode = rewind_core::RunMode::Test;
        self.world.start(request).await
    }

    async fn start(&self, source: &str, input: Value) -> RunRef {
        self.try_start(source, input).await.unwrap()
    }

    async fn start_with(&self, source: &str, input: Value, action_set: ActionSet) -> RunRef {
        self.try_start_with(source, input, action_set)
            .await
            .unwrap()
    }

    /// Drives the node in virtual time until only the outside world can wake a run.
    async fn drive(&self) {
        self.world.idle().await;
    }

    async fn get(&self, id: &RunRef) -> Run {
        let history = self.world.history(id).await;
        let state = RunState::from_history(&history).unwrap();
        Run {
            id: id.clone(),
            history,
            state,
        }
    }

    async fn run(&self, source: &str, input: Value) -> Run {
        let id = self.start(source, input).await;
        self.drive().await;
        self.get(&id).await
    }

    /// Replays the run with its own code: it must match, to the end of its history.
    async fn assert_replays(&self, run: &Run) {
        let verdict = self.world.replay(&run.id, None).await;
        assert!(matches!(verdict, ReplayVerdict::Matches(_)), "{verdict:?}");
    }
}

const FAN_OUT: &str = r#"
import { github } from "rewind:actions";

export default async function main(input: { repo: string }) {
  const issues = await Promise.all([1, 2, 3].map((number) => github.get_issue({ repo: input.repo, number })));
  return { titles: issues.map((issue: { title: string }) => issue.title) };
}
"#;

#[tokio::test]
async fn promise_all_with_out_of_order_completions_replays_identically() {
    let harness = Harness::new(
        SandboxLimits::default(),
        1,
        answered_by(issue_actions),
        |config| config.reversed_calls = true,
    )
    .await;
    let run = harness.run(FAN_OUT, json!({ "repo": "acme/api" })).await;

    assert_eq!(
        run.output(),
        &json!({ "titles": ["issue 1", "issue 2", "issue 3"] })
    );
    let delivered: Vec<Seq> = run
        .history
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Completion(Completion::CallCompleted { seq, .. }) => Some(*seq),
            _ => None,
        })
        .collect();
    assert_eq!(
        delivered,
        [Seq(3), Seq(2), Seq(1)],
        "completions arrived out of order"
    );
    harness.assert_replays(&run).await;
}

const NON_DETERMINISTIC_SOURCES: &str = r#"
import { audit } from "rewind:actions";
import { run } from "rewind";

export default async function main() {
  const sample = {
    now: Date.now(),
    date: new Date().toISOString(),
    called: Date(),
    random: Math.random(),
    uuid: crypto.randomUUID(),
    bytes: Array.from(crypto.getRandomValues(new Uint8Array(8))),
    perf: performance.now(),
    formatted: new Intl.DateTimeFormat("en-US", { dateStyle: "long", timeZone: "UTC" }).format(),
    locale: (1234567.891).toLocaleString(),
    runId: run.id,
    startedAt: run.startedAt.toISOString(),
  };
  await audit.record(sample);
  return sample;
}
"#;

#[tokio::test]
async fn time_and_randomness_are_deterministic_and_keyed_by_the_seed() {
    let first = harness_with(SandboxLimits::default(), 7).await;
    let second = harness_with(SandboxLimits::default(), 7).await;
    let a = first.run(NON_DETERMINISTIC_SOURCES, json!({})).await;
    let b = second.run(NON_DETERMINISTIC_SOURCES, json!({})).await;
    assert_eq!(
        a.history, b.history,
        "same seed and same completions, same history"
    );

    let output = a.output();
    assert_eq!(output["now"], json!(T0.0));
    assert_eq!(output["date"], json!("2026-01-01T00:00:00.000Z"));
    assert_eq!(
        output["called"],
        json!("Thu Jan 01 2026 00:00:00 GMT+0000 (Coordinated Universal Time)")
    );
    assert_eq!(output["formatted"], json!("January 1, 2026"));
    assert_eq!(output["locale"], json!("1,234,567.891"));
    assert_eq!(output["perf"], json!(0));
    assert_eq!(output["runId"], json!(a.id.id.to_string()));
    assert_eq!(output["startedAt"], json!("2026-01-01T00:00:00.000Z"));

    let other = harness_with(SandboxLimits::default(), 8)
        .await
        .run(NON_DETERMINISTIC_SOURCES, json!({}))
        .await;
    for key in ["random", "uuid", "bytes"] {
        assert_ne!(
            other.output()[key],
            output[key],
            "{key} depends on the seed"
        );
    }
    first.assert_replays(&a).await;
}

/// The depth at which recursion overflows the stack depends on the size of the frames, which
/// changes when V8 installs optimized code. Compiled concurrently, it was installed at a time
/// that varied from one isolate to the next: the same code caught its first overflow at
/// different depths, and its replay diverged.
#[tokio::test]
async fn the_depth_of_a_stack_overflow_does_not_vary() {
    const RECURSES: &str = r#"
import { audit } from "rewind:actions";

function depth(n: number): number {
  try {
    return depth(n + 1);
  } catch {
    return n;
  }
}

export default async function main() {
  const depths: number[] = [];
  for (let i = 0; i < 3000; i++) depths.push(depth(0));
  const sample = { first: depths[0], last: depths[depths.length - 1], distinct: new Set(depths).size };
  await audit.record(sample);
  return sample;
}
"#;
    let harness = harness().await;
    let mut outputs = Vec::new();
    for _ in 0..4 {
        let run = harness.run(RECURSES, json!({})).await;
        harness.assert_replays(&run).await;
        outputs.push(run.output().clone());
    }
    assert!(
        outputs.windows(2).all(|pair| pair[0] == pair[1]),
        "{outputs:?}"
    );
}

#[tokio::test]
async fn crypto_is_chacha20_keyed_by_the_seed() {
    // The first bytes of the ChaCha20 keystream (RFC 8439, nonce 0) under the key derived
    // from an all-zero seed, as the runtime derives it.
    let executor = V8Executor::new(SandboxLimits::default()).unwrap();
    let source = r#"export default async function main() {
      return Array.from(crypto.getRandomValues(new Uint8Array(8)));
    }"#;
    let compiled = executor
        .compile(&Code::new(source), &ActionSet::default())
        .unwrap();
    let start = |seed| StartCode {
        run_id: RunId::from_u128(1),
        compiled: compiled.clone(),
        action_set: ActionSet::default(),
        input: json!({}),
        seed: Seed::from_bytes(seed),
        epoch: Epoch(1),
        started_at: T0,
    };
    let output = |emitted: Vec<rewind_engine::ports::Emitted>| match emitted.as_slice() {
        [rewind_engine::ports::Emitted::Command(Command::CompleteRun { output })] => {
            output.value().unwrap().clone()
        }
        other => panic!("{other:?}"),
    };
    let (_, zero) = executor.start(start([0; 32])).await.unwrap();
    let (_, again) = executor.start(start([0; 32])).await.unwrap();
    let (_, one) = executor.start(start([1; 32])).await.unwrap();
    let zero = output(zero);
    assert_eq!(zero, output(again));
    assert_ne!(zero, output(one));
    assert_eq!(zero.as_array().unwrap().len(), 8);
}

#[tokio::test]
async fn replay_detects_a_changed_code_at_the_diverging_event() {
    let harness = harness().await;
    let run = harness.run(FAN_OUT, json!({ "repo": "acme/api" })).await;

    let changed_args = Code::new(FAN_OUT.replace("[1, 2, 3]", "[1, 2, 4]"));
    let verdict = harness.world.replay(&run.id, Some(&changed_args)).await;
    let ReplayVerdict::Diverges(mismatch) = verdict else {
        panic!("{verdict:?}")
    };
    assert_eq!(
        mismatch.to_string(),
        "event 3 is call #3 github.get_issue({\"number\":3,\"repo\":\"acme/api\"}), \
         but the code emitted call #3 github.get_issue({\"number\":4,\"repo\":\"acme/api\"})"
    );

    let extra_call = Code::new(FAN_OUT.replace(
        "return { titles",
        "await github.close_issue({ number: 1 });\n  return { titles",
    ));
    let verdict = harness.world.replay(&run.id, Some(&extra_call)).await;
    let ReplayVerdict::Diverges(mismatch) = verdict else {
        panic!("{verdict:?}")
    };
    assert!(
        mismatch.to_string().contains("github.close_issue"),
        "{mismatch}"
    );

    // Moving code around without changing behavior is allowed: callsites are ignored.
    let moved = Code::new(format!("\n\n// a comment\n{FAN_OUT}"));
    let verdict = harness.world.replay(&run.id, Some(&moved)).await;
    assert_eq!(verdict, ReplayVerdict::Matches(ReplayEnd::EndOfHistory));
}

#[tokio::test]
async fn durable_sleep_moves_logical_time() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { sleep } from "rewind";

export default async function main() {
  const before = Date.now();
  await sleep("10d");
  await new Promise((resolve) => setTimeout(resolve, 1500));
  return { elapsed: Date.now() - before, iso: new Date().toISOString() };
}
"#,
            json!({}),
        )
        .await;

    assert_eq!(
        run.output(),
        &json!({ "elapsed": 10 * DAY + 1500, "iso": "2026-01-11T00:00:01.500Z" })
    );
    let timers: Vec<u64> = run
        .commands()
        .into_iter()
        .filter_map(|command| match command {
            Command::StartTimer { duration_ms, .. } => Some(*duration_ms),
            _ => None,
        })
        .collect();
    assert_eq!(timers, [10 * DAY, 1500]);
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn signals_are_buffered_cancel_their_timeout_and_time_out() {
    let harness = harness().await;
    let id = harness
        .start(
            r#"
import { signal, sleep } from "rewind";

export default async function main() {
  await sleep("1h");                                           // "approval" arrives meanwhile
  const approval = await signal("approval");                   // buffered: returns at once
  const comment = await signal("comment", { timeout: "1d" });  // arrives: timeout cancelled
  const never = await signal("never", { timeout: "2h" });      // times out
  return { approval, comment, never: never ?? null, elapsed: performance.now() };
}
"#,
            json!({}),
        )
        .await;
    harness.world.settle().await;
    harness
        .world
        .signal(&id, "approval", json!({ "by": "paul" }))
        .await
        .unwrap();

    // One hour later, the sleep ends and the code waits for "comment".
    harness.world.advance_to(Timestamp(T0.0 + HOUR));
    harness.world.settle().await;
    let waiting = harness.get(&id).await;
    assert_eq!(
        RunStatus::at_rest(waiting.state.phase()),
        RunStatus::Waiting
    );
    assert_eq!(waiting.state.awaited_signals()[0].name, "comment");

    harness.world.advance_to(Timestamp(T0.0 + 2 * HOUR));
    harness
        .world
        .signal(&id, "comment", json!("lgtm"))
        .await
        .unwrap();
    harness.drive().await; // virtual time: "never" times out two hours later

    let run = harness.get(&id).await;
    assert_eq!(
        run.output(),
        &json!({ "approval": { "by": "paul" }, "comment": "lgtm", "never": null, "elapsed": 4 * HOUR })
    );
    assert!(
        run.commands()
            .contains(&&Command::CancelTimer { seq: Seq(2) }),
        "the comment's timeout timer is cancelled"
    );
    let awaited: Vec<(&str, Option<Seq>)> = run
        .history
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Annotation(Annotation::SignalAwaited { name, timeout, .. }) => {
                Some((name.as_str(), *timeout))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        awaited,
        [("comment", Some(Seq(2))), ("never", Some(Seq(3)))]
    );
    harness.assert_replays(&run).await;
}

/// What type-checks is what exists (`archi/04-tools.md`, "Type-checking"): the isolate's global
/// names are exactly those `lib/globals.json` declares, with `fetch` only for an action set that
/// enables the `http` action.
#[tokio::test]
async fn the_sandbox_has_exactly_the_declared_globals() {
    const SCRIPT: &str = r#"
export default async function main() {
  const absent = ["Deno", "WeakRef", "FinalizationRegistry", "SharedArrayBuffer", "Atomics",
                  "WebAssembly", "__bootstrap", "__rewindHooks", "__rewindHandoff", "require", "process",
                  "Buffer"];
  return {
    absent: absent.filter((name) => typeof (globalThis as any)[name] !== "undefined"),
    globals: Object.getOwnPropertyNames(globalThis).sort(),
    blobUrls: typeof (URL as any).createObjectURL,
    timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    locale: Intl.DateTimeFormat().resolvedOptions().locale,
  };
}
"#;
    let declared = rewind_runtime::sandbox_lib::declared_globals();
    for http_action in [false, true] {
        let harness = harness().await;
        let action_set = if http_action {
            builtins_action_set()
        } else {
            action_set()
        };
        let id = harness.start_with(SCRIPT, json!({}), action_set).await;
        harness.drive().await;
        let run = harness.get(&id).await;
        let output = run.output();
        assert_eq!(output["absent"], json!([]));
        assert_eq!(output["blobUrls"], "undefined");
        assert_eq!(output["timezone"], "UTC");
        assert_eq!(output["locale"], "en-US");
        let globals: std::collections::BTreeSet<&str> = output["globals"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            globals,
            declared.names(http_action),
            "http action: {http_action}"
        );
    }
}

/// An action set with the built-ins `http` and `webhook`, and one harness action. `webhook` is
/// pinned with the schema of `rewind_runtime::builtins`; `http` with a schema that accepts
/// anything, as its schema lives in `rewind-mcp` (the corpus of `rewind-e2e` pins that one).
fn builtins_action_set() -> ActionSet {
    ActionSet::new(
        [],
        [
            Action::new(
                "http",
                ActionRoute::Builtin(Builtin::Http {
                    allowed_hosts: vec!["api.example.com".into()],
                }),
                json!({}),
            ),
            Action::new(
                "signalUrl",
                ActionRoute::Builtin(Builtin::SignalUrl),
                rewind_runtime::builtins::signal_url_input_schema(),
            ),
            Action::new(
                "github.get_issue",
                ActionRoute::Harness {
                    group: Some("github".into()),
                },
                json!({
                    "type": "object",
                    "properties": { "number": { "type": "integer", "minimum": 1 } },
                    "required": ["number"],
                    "additionalProperties": false,
                }),
            ),
        ],
    )
    .unwrap()
}

#[tokio::test]
async fn scripts_that_cannot_load_are_refused_at_start() {
    let harness = harness().await;
    for (source, expected) in [
        (
            "import fs from \"node:fs\";\nexport default async function main() { return typeof fs; }",
            "cannot import \"node:fs\" (workflow.ts:1:16): workflows can only import",
        ),
        (
            "import hooks from \"rewind:internal\";\nexport default async function main() { return typeof hooks; }",
            "workflows can only import",
        ),
        (
            "import { jira } from \"rewind:actions\";\nexport default async () => jira;",
            "\"rewind:actions\" has no export named \"jira\" (workflow.ts:1:10); it exports audit, \
             failing, flaky, github, slow",
        ),
        (
            "export async function main() { return 1; }",
            "no default export",
        ),
        (
            "export default 42;",
            "must `export default` its main function",
        ),
        ("export default async function main( {", "workflow.ts:1"),
        (
            "export default async function main() { return await import(\"rewind\"); }",
            "dynamic import() is not available",
        ),
    ] {
        let refused = harness.try_start(source, json!({})).await.unwrap_err();
        assert!(
            matches!(refused, StartError::InvalidCode(_)),
            "{source}: {refused:?}"
        );
        assert!(
            refused.to_string().contains(expected),
            "{source}: {refused}"
        );
    }
    let runs = harness.world.runs(Vec::new()).await;
    assert!(runs.is_empty(), "no run was created: {runs:?}");
}

/// What only shows when the code runs, such as an error thrown by its top-level code, aborts
/// the run when it loads.
#[tokio::test]
async fn scripts_that_fail_while_loading_are_aborted_as_invalid() {
    let harness = harness().await;
    for (source, expected) in [
        (
            "throw new Error(\"top-level failure\");\nexport default async () => 1;",
            "top-level failure",
        ),
        (
            "const main = 42;\nexport default main;",
            "must `export default` its main function",
        ),
    ] {
        let run = harness.run(source, json!({})).await;
        let (kind, message) = run.abort();
        assert_eq!(kind, AbortKind::InvalidCode, "{source}");
        assert!(message.contains(expected), "{source}: {message}");
        assert_eq!(run.events(), ["run_started", "run_aborted"]);
        harness.assert_replays(&run).await;
    }
}

#[tokio::test]
async fn action_errors_reach_the_script_and_can_be_caught() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { flaky } from "rewind:actions";
import { ActionError } from "rewind";

export default async function main() {
  try {
    await flaky({});
  } catch (error) {
    if (error instanceof ActionError && error.outcomeUnknown) {
      throw new TypeError(`gave up: ${error.message} (${error.action}, ${JSON.stringify(error.data)})`);
    }
  }
}
"#,
            json!({}),
        )
        .await;
    let error = run.error();
    assert_eq!(error.name, "TypeError");
    assert_eq!(
        error.message,
        "gave up: flaky failed: connection reset (flaky, {\"status\":503})"
    );
    assert!(
        error.stack.as_deref().unwrap().contains("workflow.ts:10"),
        "{error:?}"
    );
    assert_eq!(RunStatus::at_rest(run.state.phase()), RunStatus::Failed);
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn callsites_point_to_typescript_lines() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { github } from "rewind:actions";

interface Issue { number: number; title: string }
type Repo = `${string}/${string}`;

export default async function main(input: { repo: Repo }): Promise<string> {
  const issue: Issue = await github.get_issue({ repo: input.repo, number: 7 });
  console.log("fetched", issue.number);
  return issue.title;
}
"#,
            json!({ "repo": "acme/api" }),
        )
        .await;

    let call = run.history.iter().find_map(|entry| match &entry.event {
        Event::Command(Command::ScheduleCall { callsite, .. }) => *callsite,
        _ => None,
    });
    assert_eq!(call.map(|c| c.line), Some(8));
    let log = run.history.iter().find_map(|entry| match &entry.event {
        Event::Annotation(Annotation::Log {
            message, callsite, ..
        }) => Some((message.clone(), *callsite)),
        _ => None,
    });
    assert_eq!(
        log,
        Some((
            "fetched 7".into(),
            Some(Callsite {
                line: 9,
                column: 11
            })
        ))
    );
}

#[tokio::test]
async fn cancellation_rejects_pending_awaits_and_the_script_can_clean_up() {
    // `slow` never answers by itself: its call is in flight when the cancellations arrive.
    let actions = Actions(Arc::new(|call: &Call| match call.action.name.as_str() {
        "slow" => Dispatched::Deferred,
        _ => Dispatched::Result(json!({ "ok": true })),
    }));
    let harness = Harness::new(SandboxLimits::default(), 1, actions, |_| {}).await;
    let polite = harness
        .start(
            r#"
import { sleep, CancelledError } from "rewind";

export default async function main() {
  try {
    await sleep("30d");
    return "slept";
  } catch (error) {
    return error instanceof CancelledError ? `cleaned up after: ${error.message}` : "unexpected";
  }
}
"#,
            json!({}),
        )
        .await;
    let blunt = harness
        .start(
            r#"
import { slow } from "rewind:actions";
export default async function main() { await slow({}); }
"#,
            json!({}),
        )
        .await;
    harness.world.settle().await;
    harness
        .world
        .cancel(&polite, Some("user asked"))
        .await
        .unwrap();
    harness.world.cancel(&blunt, None).await.unwrap();
    // The cancellations are delivered before the action answers: it is not waited for.
    harness.drive().await;

    let polite = harness.get(&polite).await;
    assert_eq!(
        polite.output(),
        &json!("cleaned up after: run cancelled: user asked")
    );
    let blunt = harness.get(&blunt).await;
    assert_eq!(
        RunStatus::at_rest(blunt.state.phase()),
        RunStatus::Cancelled
    );
    assert_eq!(blunt.error().name, "CancelledError");
    assert_eq!(
        blunt.events(),
        [
            "run_started",
            "schedule_call",
            "cancel_requested",
            "fail_run"
        ]
    );
    harness.assert_replays(&polite).await;
    harness.assert_replays(&blunt).await;
}

// ---------------------------------------------------------------------------------------------
// How a code ends (archi/03-execution.md)

#[tokio::test]
async fn a_forgotten_await_is_harmless_the_run_waits_for_the_call() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { audit } from "rewind:actions";
export default async function main() {
  audit.record({ event: "done" });   // no await
  return "returned first";
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(run.output(), &json!("returned first"));
    assert_eq!(
        run.events(),
        [
            "run_started",
            "schedule_call",
            "call_completed",
            "complete_run"
        ],
        "complete_run waits for the call in flight"
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn a_forgotten_await_that_fails_fails_the_run() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { failing } from "rewind:actions";
export default async function main() {
  failing({});
  return "returned";
}
"#,
            json!({}),
        )
        .await;
    let error = run.error();
    assert_eq!(
        (error.name.as_str(), error.message.as_str()),
        ("ActionError", "failing failed: boom")
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn a_rejection_left_unhandled_fails_the_run_as_in_node() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { failing, audit } from "rewind:actions";
export default async function main() {
  const p = failing({});
  await audit.record({});   // p is rejected before it is awaited
  await p.catch(() => "too late");
  return "unreachable";
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(run.error().message, "failing failed: boom");
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn nothing_left_to_wake_the_script_is_a_deadlock() {
    let harness = harness().await;
    let run = harness
        .run(
            "export default async function main() { await new Promise(() => {}); }",
            json!({}),
        )
        .await;
    let error = run.error();
    assert_eq!(error.name, "DeadlockError");
    assert_eq!(run.events(), ["run_started", "fail_run"]);
    harness.assert_replays(&run).await;

    // Waiting for a signal is not a deadlock: something outside can still wake the code.
    let waiting = harness
        .run(
            "import { signal } from \"rewind\";\nexport default async function main() { return await signal(\"go\"); }",
            json!({}),
        )
        .await;
    assert_eq!(
        RunStatus::at_rest(waiting.state.phase()),
        RunStatus::Waiting
    );
}

#[tokio::test]
async fn after_a_failure_no_script_code_runs_and_fail_run_waits_for_calls_in_flight() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { audit } from "rewind:actions";
export default async function main() {
  audit.record({ n: 1 }).then(() => console.log("must not run"));
  throw new RangeError("bad input");
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(run.error().name, "RangeError");
    assert_eq!(
        run.events(),
        ["run_started", "schedule_call", "call_completed", "fail_run"]
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn an_exception_in_a_timer_callback_fails_the_run() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { sleep } from "rewind";
export default async function main() {
  setTimeout(() => { throw new Error("in a callback"); }, 10);
  await sleep("1h");
  return "unreachable";
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(run.error().message, "in a callback");
}

#[tokio::test]
async fn a_raced_slow_action_is_still_awaited() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { slow } from "rewind:actions";
import { sleep } from "rewind";
export default async function main() {
  return await Promise.race([slow({}).then(() => "action"), sleep(10).then(() => "timer")]);
}
"#,
            json!({}),
        )
        .await;
    // The action answers first in virtual time here; either way the call has an outcome.
    let state = &run.state;
    assert_eq!(RunStatus::at_rest(state.phase()), RunStatus::Completed);
    assert!(run.events().contains(&"call_completed"));
}

// ---------------------------------------------------------------------------------------------
// Limits

#[tokio::test]
async fn an_endless_loop_exceeds_the_cpu_limit() {
    let harness = harness_with(
        SandboxLimits {
            cpu_per_trigger: Duration::from_millis(200),
            ..SandboxLimits::default()
        },
        1,
    )
    .await;
    let run = harness
        .run(
            "export default async function main() { for (;;) {} }",
            json!({}),
        )
        .await;
    let (kind, message) = run.abort();
    assert_eq!(kind, AbortKind::LimitExceeded);
    assert!(message.contains("200ms of CPU time"), "{message}");
}

#[tokio::test]
async fn a_memory_bomb_exceeds_the_heap_limit() {
    let harness = harness_with(
        SandboxLimits {
            heap_bytes: 64 * 1024 * 1024,
            ..SandboxLimits::default()
        },
        1,
    )
    .await;
    let run = harness
        .run(
            "export default async function main() { const a = []; for (;;) a.push(new Array(1e5).fill(1)); }",
            json!({}),
        )
        .await;
    let (kind, message) = run.abort();
    assert_eq!(kind, AbortKind::LimitExceeded);
    assert!(message.contains("64 MiB of heap"), "{message}");
}

/// An array buffer past the instance's budget is V8's RangeError, never an abort of the process:
/// `new ArrayBuffer(2 ** 36)` used to abort it, resizable or not.
#[tokio::test]
async fn the_process_survives_an_array_buffer_of_2_pow_36() {
    let harness = harness().await;
    for allocation in [
        "new ArrayBuffer(2 ** 36)",
        "new ArrayBuffer(2 ** 36, { maxByteLength: 2 ** 36 })",
        "new ArrayBuffer(0, { maxByteLength: 2 ** 36 })",
        "new Float64Array(2 ** 33)",
    ] {
        let run = harness
            .run(
                &format!(
                    "export default async function main() {{ return {allocation}.byteLength; }}"
                ),
                json!({}),
            )
            .await;
        let error = run.error();
        assert_eq!(error.name, "RangeError", "{allocation}");
        assert_eq!(
            error.message, "Array buffer allocation failed",
            "{allocation}"
        );
    }
    let run = harness
        .run(
            "export default async function main() { return new ArrayBuffer(8).byteLength; }",
            json!({}),
        )
        .await;
    assert_eq!(run.output(), &json!(8));
}

/// The buffers an instance holds share one budget. A buffer the code dropped gives its bytes
/// back before an allocation fails, so where it fails depends only on what the code holds:
/// the same on replay.
#[tokio::test]
async fn array_buffers_share_one_budget_and_fail_where_replay_does() {
    let harness = harness_with(
        SandboxLimits {
            array_buffer_bytes: 16 * 1024 * 1024,
            ..SandboxLimits::default()
        },
        1,
    )
    .await;
    let run = harness
        .run(
            r#"
import { sleep } from "rewind";

function hoard(): string {
  const kept: Uint8Array[] = [];
  try {
    for (;;) kept.push(new Uint8Array(1 << 20));
  } catch (error) {
    return `${kept.length} ${error}`;
  }
}

export default async function main() {
  const first = hoard();
  await sleep(1000);
  const again = [hoard(), hoard(), hoard()];
  return { first, again };
}
"#,
            json!({}),
        )
        .await;
    let output = run.output();
    let first = output["first"].as_str().unwrap();
    assert!(
        first.ends_with(" RangeError: Array buffer allocation failed"),
        "{first}"
    );
    let count: usize = first.split(' ').next().unwrap().parse().unwrap();
    assert!((14..=16).contains(&count), "{first}");
    assert_eq!(output["again"], json!([first, first, first]));
    harness.assert_replays(&run).await;
}

/// A resizable buffer takes its maximum length from the budget when it is created, and keeps it
/// through a transfer; a clone takes its own; `transferToFixedLength` gives it back.
#[tokio::test]
async fn resizable_array_buffers_take_their_maximum_from_the_budget() {
    let harness = harness_with(
        SandboxLimits {
            array_buffer_bytes: 16 * 1024 * 1024,
            ..SandboxLimits::default()
        },
        1,
    )
    .await;
    let run = harness
        .run(
            r#"
const MAX = 10 << 20;
function attempt(make: () => unknown): string {
  try {
    make();
    return "ok";
  } catch (error) {
    return String(error);
  }
}

export default async function main() {
  let a = new ArrayBuffer(16, { maxByteLength: MAX });
  a.resize(MAX);
  const second = attempt(() => new ArrayBuffer(0, { maxByteLength: MAX }));
  a = a.transfer();
  const afterTransfer = attempt(() => new ArrayBuffer(0, { maxByteLength: MAX }));
  const clone = attempt(() => structuredClone(a));
  const fixed = a.transferToFixedLength(16);
  const afterFixed = attempt(() => new ArrayBuffer(0, { maxByteLength: MAX }));
  return {
    second,
    afterTransfer,
    clone,
    afterFixed,
    resizable: a.resizable,
    detached: a.detached,
    fixed: [fixed.byteLength, fixed.resizable],
    wrapper: [
      ArrayBuffer.name,
      ArrayBuffer.length,
      fixed.constructor === ArrayBuffer,
      new Uint8Array(1).buffer instanceof ArrayBuffer,
      ArrayBuffer.isView(new Uint8Array(1)),
      ArrayBuffer.prototype.transfer.length,
    ],
  };
}
"#,
            json!({}),
        )
        .await;
    let refused = "RangeError: Array buffer allocation failed";
    assert_eq!(
        run.output(),
        &json!({
            "second": refused,
            "afterTransfer": refused,
            "clone": refused,
            "afterFixed": "ok",
            "resizable": true,
            "detached": true,
            "fixed": [16, false],
            "wrapper": ["ArrayBuffer", 1, true, true, true, 0],
        })
    );
    harness.assert_replays(&run).await;
}

/// Built-ins that V8 runs to their end without an interrupt check end the call as the heap limit
/// does, before they start, when they must create more elements than the heap holds:
/// `new Array(2 ** 27).fill(1.5)` used to run 18 s and take 1.9 GiB under a 64 MiB heap, and the
/// last split made V8 abort the process.
#[tokio::test]
async fn built_ins_that_would_outgrow_the_heap_end_the_call_at_once() {
    let harness = harness_with(
        SandboxLimits {
            heap_bytes: 64 * 1024 * 1024,
            ..SandboxLimits::default()
        },
        1,
    )
    .await;
    for call in [
        "new Array(2 ** 27).fill(1.5)",
        "Array.prototype.fill.call({ length: 2 ** 32 + 5 }, 0)",
        "Array.from({ length: 2 ** 27 })",
        "new Array(2 ** 27).toReversed()",
        "new Array(2 ** 27).toSorted()",
        "new Array(2 ** 27).toSpliced(0, 0, 1)",
        "new Array(2 ** 27).with(-1, 0)",
        "\"x,\".repeat(2 ** 27).split(\",\")",
        "\"x\".repeat(2 ** 28).split(\"\")",
    ] {
        let run = harness
            .run(
                &format!("export default async function main() {{ return {call}.length; }}"),
                json!({}),
            )
            .await;
        let (kind, message) = run.abort();
        assert_eq!(kind, AbortKind::LimitExceeded, "{call}");
        assert_eq!(message, "the code used more than 64 MiB of heap", "{call}");
        assert_eq!(run.events(), ["run_started", "run_aborted"], "{call}");
    }

    // Below the bound, or with what V8 must check itself, they behave as V8's.
    let run = harness
        .run(
            r#"
function attempt(make: () => unknown): string {
  try {
    return String(make());
  } catch (error) {
    return String(error);
  }
}
export default async function main() {
  const holey = new Array(2 ** 27);
  return [
    holey.fill(0, 0, 3).slice(0, 4),
    holey.toSpliced(0, 2 ** 27 - 2).length,
    attempt(() => holey.with(2 ** 28, 0)),
    attempt(() => new Array(2 ** 27).toSorted(1 as never)),
    Array.from({ length: 3 }, (_, i) => i * 2),
    [3, 1, 2].toSorted(),
    "a,b,,c".split(",", 3),
    "abc".split(""),
    [1, 2].with(-1, 9),
    [Array.prototype.fill.name, Array.prototype.fill.length, "".split.length, Array.from.length],
  ];
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(
        run.output(),
        &json!([
            [0, 0, 0, null],
            2,
            "RangeError: Invalid index : 268435456",
            "TypeError: The comparison function must be either a function or undefined: 1",
            [0, 2, 4],
            [1, 2, 3],
            ["a", "b", ""],
            ["a", "b", "c"],
            [1, 9],
            ["fill", 1, 2, 1],
        ])
    );
}

/// The heap limit is raised by V8's largest object when it is reached, so that V8 can finish the
/// allocation in progress: raised less, V8 aborted the process on one 256 MiB string.
#[tokio::test]
async fn one_allocation_past_the_heap_limit_ends_the_call_not_the_process() {
    let harness = harness_with(
        SandboxLimits {
            heap_bytes: 64 * 1024 * 1024,
            ..SandboxLimits::default()
        },
        1,
    )
    .await;
    let run = harness
        .run(
            "export default async function main() { return \"ab\".repeat(2 ** 27).toUpperCase().length; }",
            json!({}),
        )
        .await;
    let (kind, message) = run.abort();
    assert_eq!(kind, AbortKind::LimitExceeded);
    assert_eq!(message, "the code used more than 64 MiB of heap");
}

/// What a code emits is kept outside V8's heap until the call ends: a flood of logs, of action
/// arguments or of signal names is stopped once one trigger's response holds more than its
/// bound, and nothing of it is recorded.
#[tokio::test]
async fn what_one_trigger_emits_is_bounded_in_bytes() {
    let harness = harness_with(
        SandboxLimits {
            emitted_bytes_per_trigger: 4 * 1024 * 1024,
            ..SandboxLimits::default()
        },
        1,
    )
    .await;
    for flood in [
        "for (let i = 0; i < 100; i++) console.log(big);",
        "for (let i = 0; i < 100; i++) void audit.record({ big });",
        "for (let i = 0; i < 100; i++) void signal(big);",
        "throw new Error(big + big + big + big + big);",
        "return [big, big, big, big, big];",
    ] {
        let run = harness
            .run(
                &format!(
                    "import {{ signal }} from \"rewind\";\nimport {{ audit }} from \"rewind:actions\";\n\
                     export default async function main() {{\n  \
                       const big = \"x\".repeat(1 << 20);\n  {flood}\n}}"
                ),
                json!({}),
            )
            .await;
        let (kind, message) = run.abort();
        assert_eq!(kind, AbortKind::LimitExceeded, "{flood}");
        assert_eq!(
            message,
            "the code emitted more than 4 MiB of commands and logs reacting to one trigger",
            "{flood}"
        );
        assert_eq!(run.events(), ["run_started", "run_aborted"], "{flood}");
    }
}

#[tokio::test]
async fn what_one_trigger_emits_is_bounded_in_number() {
    let harness = harness_with(
        SandboxLimits {
            emitted_per_trigger: 100,
            ..SandboxLimits::default()
        },
        1,
    )
    .await;
    let run = harness
        .run(
            "export default async function main() { for (;;) console.log(1); }",
            json!({}),
        )
        .await;
    let (kind, message) = run.abort();
    assert_eq!(kind, AbortKind::LimitExceeded);
    assert_eq!(
        message,
        "the code emitted more than 100 commands and logs reacting to one trigger"
    );
}

/// A string longer than what is left is never copied out of V8: a rope of 2^27 characters
/// would take 384 MiB as UTF-8.
#[tokio::test]
async fn a_long_string_is_refused_before_it_leaves_v8() {
    let harness = harness().await;
    let run = harness
        .run(
            "export default async function main() { let s = \"\u{20ac}\"; for (let i = 0; i < 27; i++) s += s; console.log(s); }",
            json!({}),
        )
        .await;
    let (kind, message) = run.abort();
    assert_eq!(kind, AbortKind::LimitExceeded);
    assert!(
        message.contains("more than 64 MiB of commands and logs"),
        "{message}"
    );
}

/// The bound is per trigger: a run may emit more than it over several, and what its top-level
/// code emits counts with `main(input)`'s response.
#[tokio::test]
async fn the_bound_on_what_is_emitted_is_per_trigger() {
    let harness = harness_with(
        SandboxLimits {
            emitted_bytes_per_trigger: 4 * 1024 * 1024,
            ..SandboxLimits::default()
        },
        1,
    )
    .await;
    let run = harness
        .run(
            r#"
import { sleep } from "rewind";
const big = "x".repeat(1 << 20);
for (let i = 0; i < 2; i++) console.log(big);
export default async function main() {
  console.log(big);
  await sleep(1000);
  for (let i = 0; i < 3; i++) console.log(big);
  return "done";
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(run.output(), &json!("done"));

    let run = harness
        .run(
            r#"
const big = "x".repeat(1 << 20);
for (let i = 0; i < 3; i++) console.log(big);
export default async function main() {
  console.log(big);
  console.log(big);
}
"#,
            json!({}),
        )
        .await;
    let (kind, message) = run.abort();
    assert_eq!(kind, AbortKind::LimitExceeded);
    assert!(message.contains("more than 4 MiB"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// Boundaries

#[tokio::test]
async fn an_unsupported_epoch_is_refused() {
    let executor = V8Executor::new(SandboxLimits::default()).unwrap();
    assert_eq!(executor.epochs(), [Epoch(1)]);
    let start = StartCode {
        run_id: RunId::from_u128(1),
        compiled: CompiledCode {
            javascript: "export default () => 1".into(),
            source_map: String::new(),
        },
        action_set: ActionSet::default(),
        input: json!({}),
        seed: Seed::from_bytes([0; 32]),
        epoch: Epoch(99),
        started_at: T0,
    };
    assert_eq!(
        executor.start(start).await.err(),
        Some(ExecutorError::UnsupportedEpoch(Epoch(99)))
    );
}

/// RFC 8785 writes numbers as ECMAScript does: the core's canonical JSON must agree with V8 on
/// any double.
#[tokio::test]
async fn canonical_numbers_agree_with_v8() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
export default async function main() {
  const bits = new Float64Array(1);
  const bytes = new Uint8Array(bits.buffer);
  const numbers: number[] = [];
  while (numbers.length < 5000) {
    crypto.getRandomValues(bytes);
    if (Number.isFinite(bits[0])) numbers.push(bits[0]);
  }
  for (const x of [0, -0, 1e21, 1e-7, 123e-20, 5e-324, 2 ** 53 + 2, 0.1 + 0.2, 1424953923781206.2]) {
    numbers.push(x);
  }
  return { numbers, texts: numbers.map((x) => JSON.stringify(x)) };
}
"#,
            json!({}),
        )
        .await;
    let output = run.output();
    let numbers = output["numbers"].as_array().unwrap();
    let texts = output["texts"].as_array().unwrap();
    assert_eq!(numbers.len(), texts.len());
    for (number, text) in numbers.iter().zip(texts) {
        assert_eq!(&canonical_json(number), text.as_str().unwrap(), "{number}");
    }
}

#[tokio::test]
async fn only_the_action_set_is_callable() {
    let harness = harness().await;
    let run = harness
        .run(
            "import * as actions from \"rewind:actions\";\nexport default async function main() { return Object.keys(actions).sort(); }",
            json!({}),
        )
        .await;
    assert_eq!(
        run.output(),
        &json!(["audit", "failing", "flaky", "github", "slow"])
    );
}

// ---------------------------------------------------------------------------------------------
// Found by review: each of these broke determinism or the rules of how a code ends.

/// A value delivered live and the same value loaded back from storage must look the same to
/// the code, or its replay diverges and a healthy run is quarantined.
#[tokio::test]
async fn object_keys_are_the_same_live_and_on_replay() {
    let harness = harness().await;
    let big = "x".repeat(1_100); // above the inline threshold: stored as a blob
    let input: Value = serde_json::from_str(&format!(r#"{{"z":1,"a":"{big}"}}"#)).unwrap();
    let run = harness
        .run(
            r#"
import { audit } from "rewind:actions";
export default async function main(input: Record<string, unknown>) {
  await audit.record({ first: Object.keys(input)[0] });   // a second activation replays this
  return Object.keys(input);
}
"#,
            input,
        )
        .await;
    assert_eq!(run.output(), &json!(["a", "z"]));
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn the_real_clock_is_out_of_reach() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
export default async function main() {
  const parent = Object.getPrototypeOf(Date);
  let constructed = "refused";
  try { constructed = String(Reflect.construct(parent, [])); } catch {}
  return {
    parentIsNotDate: parent !== Date && typeof parent.now !== "function",
    constructed,
    sameNow: Date.now() === new Date().getTime(),
    parse: Date.parse("2026-01-02T00:00:00Z"),
    utc: Date.UTC(2026, 0, 3),
    instance: new Date() instanceof Date,
    subclass: new (class extends Date {})().getTime(),
    constructors: Date.prototype.constructor === Date && new Date().constructor === Date,
    called: Date() === String(new Date()),
    intl: new Intl.DateTimeFormat().format() === new Intl.DateTimeFormat().format(Date.now()),
    performance: performance.now() === Date.now() - performance.timeOrigin,
  };
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(
        run.output(),
        &json!({
            "parentIsNotDate": true,
            "constructed": "refused",
            "sameNow": true,
            "parse": 1_767_312_000_000_u64,
            "utc": 1_767_398_400_000_u64,
            "instance": true,
            "subclass": T0.0,
            "constructors": true,
            "called": true,
            "intl": true,
            "performance": true,
        })
    );
}

#[tokio::test]
async fn serializing_the_output_cannot_schedule_a_call_after_the_end() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { audit } from "rewind:actions";
export default async function main() {
  return { toJSON() { audit.record({ late: true }); return "out"; } };
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(run.output(), &json!("out"));
    assert_eq!(
        run.events(),
        [
            "run_started",
            "schedule_call",
            "call_completed",
            "complete_run"
        ],
        "the call made while serializing the output has an outcome before the run ends"
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn lone_surrogates_become_replacement_characters() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { audit } from "rewind:actions";
export default async function main() {
  const cut = "😀 hi".slice(0, 1);   // half of a surrogate pair
  await audit.record({ text: cut, [cut]: 1, escaped: "\\ud800" });
  return { text: cut, length: cut.length };
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(run.output(), &json!({ "text": "\u{fffd}", "length": 1 }));
    let args = run
        .commands()
        .into_iter()
        .find_map(|command| match command {
            Command::ScheduleCall { args, .. } => args.value().cloned(),
            _ => None,
        });
    assert_eq!(
        args,
        Some(json!({ "text": "\u{fffd}", "\u{fffd}": 1, "escaped": "\\ud800" })),
        "a literal backslash followed by u-d-8-0-0 is not a surrogate"
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn cancelling_a_wait_built_on_set_timeout_ends_the_run_cancelled() {
    let harness = harness().await;
    let id = harness
        .start(
            "export default async function main() {\n  await new Promise((resolve) => setTimeout(resolve, 60_000));\n  return \"woke up\";\n}",
            json!({}),
        )
        .await;
    harness.world.settle().await;
    harness.world.cancel(&id, Some("user asked")).await.unwrap();
    harness.drive().await;
    let run = harness.get(&id).await;
    assert_eq!(RunStatus::at_rest(run.state.phase()), RunStatus::Cancelled);
    let error = run.error();
    assert_eq!(
        (error.name.as_str(), error.message.as_str()),
        ("CancelledError", "run cancelled: user asked")
    );
    harness.assert_replays(&run).await;
}

/// Each piece of TypeScript's lib beyond ES2024 that `lib/tsconfig.json` lists is shipped by
/// V8: what type-checks exists.
#[tokio::test]
async fn the_ecmascript_lib_beyond_es2024_exists() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
export default async function main() {
  const g: any = globalThis;
  const L = g.Intl.Locale.prototype;
  const libs: Record<string, unknown[]> = {
    "es2025.collection": [g.Set.prototype.union, g.Set.prototype.isDisjointFrom],
    "es2025.float16": [g.Float16Array, g.Math.f16round, g.DataView.prototype.getFloat16],
    "es2025.intl": [g.Intl.DurationFormat],
    "es2025.iterator": [g.Iterator.from, g.Iterator.prototype.map, g.Iterator.prototype.toArray],
    "es2025.promise": [g.Promise.try],
    "es2025.regexp": [g.RegExp.escape],
    "esnext.array": [g.Array.fromAsync],
    "esnext.collection": [g.Map.prototype.getOrInsert, g.Map.prototype.getOrInsertComputed,
                          g.WeakMap.prototype.getOrInsert, g.WeakMap.prototype.getOrInsertComputed],
    "esnext.date": [g.Date.prototype.toTemporalInstant],
    "esnext.disposable": [g.DisposableStack, g.AsyncDisposableStack, g.SuppressedError],
    "esnext.error": [g.Error.isError],
    "esnext.intl": [L.getCalendars, L.getCollations, L.getHourCycles, L.getNumberingSystems,
                    L.getTextInfo, L.getTimeZones, L.getWeekInfo],
    "esnext.temporal": [g.Temporal.Instant, g.Temporal.Now.instant],
    "esnext.typedarrays": [g.Uint8Array.prototype.toHex, g.Uint8Array.prototype.setFromHex,
                           g.Uint8Array.fromHex, g.Uint8Array.prototype.toBase64,
                           g.Uint8Array.prototype.setFromBase64, g.Uint8Array.fromBase64],
  };
  return Object.fromEntries(
    Object.entries(libs).map(([lib, members]) => [lib, members.every((m) => typeof m === "function")]),
  );
}
"#,
            json!({}),
        )
        .await;
    let config: Value = serde_json::from_str(rewind_runtime::sandbox_lib::TSCONFIG_JSON).unwrap();
    let listed: Vec<&str> = config["compilerOptions"]["lib"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|lib| lib.starts_with("es2025") || lib.starts_with("esnext"))
        .collect();
    let checked = run.output().as_object().unwrap();
    for lib in listed {
        assert_eq!(checked.get(lib), Some(&json!(true)), "{lib}");
    }
}

// ---------------------------------------------------------------------------------------------
// Web-standard globals, args validation, webhook and fetch

#[tokio::test]
async fn web_standard_globals_are_pure_and_replay() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
export default async function main() {
  const url = new URL("../b?x=1#h", "https://Example.com/a/c");
  url.searchParams.append("y", "z w");
  const params = new URLSearchParams({ b: "2", a: "1" });
  params.sort();
  const bytes = new TextEncoder().encode("héllo 👋");
  const copy = structuredClone({ when: new Date(0), list: [1, { a: 2 }], map: new Map([["k", 1]]) });
  let bad = "";
  try { new URL("not a url"); } catch (e) { bad = (e as Error).name; }
  let notLatin1 = "";
  try { btoa("👋"); } catch (e) { notLatin1 = (e as Error).name; }
  return {
    href: url.href,
    host: url.host,
    params: params.toString(),
    canParse: [URL.canParse("https://a.example"), URL.canParse("nope")],
    bytes: Array.from(bytes),
    text: new TextDecoder().decode(bytes),
    latin1: new TextDecoder("latin1").decode(new Uint8Array([0xe9])),
    base64: btoa("hello"),
    decoded: atob("aGVsbG8="),
    copy: [copy.when instanceof Date, copy.when.getTime(), copy.list, copy.map.get("k")],
    errors: [bad, notLatin1],
  };
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(
        run.output(),
        &json!({
            "href": "https://example.com/b?x=1&y=z+w#h",
            "host": "example.com",
            "params": "a=1&b=2",
            "canParse": [true, false],
            "bytes": [104, 195, 169, 108, 108, 111, 32, 240, 159, 145, 139],
            "text": "héllo 👋",
            "latin1": "é",
            "base64": "aGVsbG8=",
            "decoded": "hello",
            "copy": [true, 0, [1, { "a": 2 }], 1],
            "errors": ["TypeError", "InvalidCharacterError"],
        })
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn invalid_args_throw_a_type_error_at_the_call_site_and_record_nothing() {
    let harness = harness().await;
    let code = r#"
import { github } from "rewind:actions";

export default async function main() {
  let caught = "";
  try {
    await github.get_issue({ number: 0, extra: true } as any);
  } catch (e) {
    caught = `${(e as Error).name}: ${(e as Error).message}`;
  }
  const issue = await github.get_issue({ number: 4 });
  await github.get_issue({ number: "4" } as any);
}
"#;
    let id = harness
        .start_with(code, json!({}), builtins_action_set())
        .await;
    harness.drive().await;
    let run = harness.get(&id).await;
    let error = run.error();
    assert_eq!(error.name, "TypeError");
    assert_eq!(
        error.message,
        "invalid arguments for github.get_issue: args.number: \"4\" is not of type \"integer\""
    );
    // The stack starts at the code's call, without frames of the glue.
    let stack = error.stack.as_deref().unwrap();
    assert!(
        stack.contains("at main (file:///workflow.ts:12:16)"),
        "{stack}"
    );
    assert!(!stack.contains("rewind:"), "{stack}");
    // Only the valid call was recorded.
    let calls: Vec<&Command> = run
        .commands()
        .into_iter()
        .filter(|command| matches!(command, Command::ScheduleCall { .. }))
        .collect();
    assert_eq!(calls.len(), 1, "{:#?}", run.history);
    harness.assert_replays(&run).await;

    let caught = harness
        .start_with(
            r#"
import { github } from "rewind:actions";
export default async function main() {
  const p = github.get_issue({ number: 0, extra: true } as any);
  return await p.then(() => "resolved", (e: Error) => `${e.name}: ${e.message}`);
}
"#,
            json!({}),
            builtins_action_set(),
        )
        .await;
    harness.drive().await;
    assert_eq!(
        harness.get(&caught).await.output(),
        &json!(
            "TypeError: invalid arguments for github.get_issue: args.number: 0 is less than the \
             minimum of 1; args: Additional properties are not allowed ('extra' was unexpected)"
        )
    );
}

#[tokio::test]
async fn signal_url_is_a_recorded_call_of_the_built_in() {
    let harness = harness().await;
    let id = harness
        .start_with(
            r#"
import { signalUrl, signal } from "rewind";

export default async function main() {
  const hook = await signalUrl("pr_merged");
  const event = await signal<{ merged: boolean }>("pr_merged");
  return { url: hook.url, merged: event.merged };
}
"#,
            json!({}),
            builtins_action_set(),
        )
        .await;
    harness.drive().await;
    harness
        .world
        .signal(&id, "pr_merged", json!({ "merged": true }))
        .await
        .unwrap();
    harness.drive().await;
    let run = harness.get(&id).await;
    assert_eq!(
        run.output(),
        &json!({ "url": "https://rewind.example.com/hooks/tok_pr_merged", "merged": true })
    );
    let Command::ScheduleCall {
        action,
        args,
        callsite,
        ..
    } = run.commands()[0]
    else {
        panic!("{:#?}", run.history);
    };
    assert_eq!(action, "signalUrl");
    assert_eq!(args.value(), Some(&json!({ "name": "pr_merged" })));
    assert_eq!(callsite.map(|c| c.line), Some(5));
    harness.assert_replays(&run).await;

    // Without the built-in, signalUrl() rejects at the call site, and nothing is recorded.
    let run = harness
        .run(
            r#"
import { signalUrl } from "rewind";
export default async function main() {
  return await signalUrl("x").then(() => "resolved", (e: Error) => `${e.name}: ${e.message}`);
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(
        run.output(),
        &json!(
            "TypeError: signalUrl() needs the built-in signalUrl action, which this run's action set \
             does not have"
        )
    );
    assert_eq!(run.events(), ["run_started", "complete_run"]);
}

/// Code written before signal URLs were named calls `webhook(name)` (`archi/08-decisions.md`,
/// R1): the same function as `signalUrl`, the same recorded call, and it replays.
#[tokio::test]
async fn webhook_is_signal_url_for_code_already_written() {
    let harness = harness().await;
    let id = harness
        .start_with(
            r#"
import { webhook, signalUrl, signal } from "rewind";

export default async function main() {
  const hook = await webhook("pr_merged");
  await signal("pr_merged");
  return { url: hook.url, same: webhook === signalUrl };
}
"#,
            json!({}),
            builtins_action_set(),
        )
        .await;
    harness.drive().await;
    harness
        .world
        .signal(&id, "pr_merged", json!({}))
        .await
        .unwrap();
    harness.drive().await;
    let run = harness.get(&id).await;
    assert_eq!(
        run.output(),
        &json!({ "url": "https://rewind.example.com/hooks/tok_pr_merged", "same": true })
    );
    let Command::ScheduleCall { action, .. } = run.commands()[0] else {
        panic!("{:#?}", run.history);
    };
    assert_eq!(action, "signalUrl");
    harness.assert_replays(&run).await;
}

/// The actions of an MCP server `crm`: two pinned without an output schema, one with.
fn crm_action_set() -> ActionSet {
    let server = McpServer::new(
        "crm",
        McpTransport::Http {
            url: "https://mcp.example.com/crm".into(),
        },
    );
    let route = || ActionRoute::Mcp {
        server: "crm".into(),
    };
    let mut score = Action::new("crm.score", route(), json!({}));
    score.output_schema = Some(json!({ "type": "object" }));
    ActionSet::new(
        [server],
        [
            Action::new("crm.lookup", route(), json!({})),
            Action::new("crm.profile", route(), json!({})),
            score,
        ],
    )
    .unwrap()
}

#[tokio::test]
async fn mcp_content_gets_the_text_and_json_helpers() {
    let harness = harness().await;
    let id = harness
        .start_with(
            r#"
import { crm } from "rewind:actions";

export default async function main() {
  const found = await crm.lookup({ id: "c-1" });
  const profile = await crm.profile();
  const score = await crm.score({ id: "c-1" });
  const keys: string[] = [];
  for (const key in found) keys.push(key);
  return {
    found: [found.json(), found.text(), keys, JSON.stringify(found) === JSON.stringify({ ...found })],
    profile: [profile.json(), profile.text()],
    score: [score, typeof (score as { text?: unknown }).text],
    frozen: Object.isFrozen(Object.getPrototypeOf(found)),
  };
}
"#,
            json!({}),
            crm_action_set(),
        )
        .await;
    harness.drive().await;
    let run = harness.get(&id).await;
    assert_eq!(
        run.output(),
        &json!({
            "found": [
                { "name": "Ada" },
                "{\"name\":\"Ada\"}",
                ["content"],
                true,
            ],
            "profile": [{ "name": "Ada", "vip": true }, "Ada\nVIP"],
            "score": [{ "score": 42 }, "undefined"],
            "frozen": true,
        })
    );
    // A call without arguments sends `{}`.
    let args: Vec<Value> = run
        .commands()
        .into_iter()
        .filter_map(|command| match command {
            Command::ScheduleCall { args, .. } => args.value().cloned(),
            _ => None,
        })
        .collect();
    assert_eq!(
        args,
        [json!({ "id": "c-1" }), json!({}), json!({ "id": "c-1" })]
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn fetch_is_a_recorded_call_of_the_http_action() {
    let harness = harness().await;
    let id = harness
        .start_with(
            r#"
export default async function main() {
  const response = await fetch(new URL("https://API.example.com/v1/items?q=1"), {
    method: "post",
    headers: [["X-Trace", " abc "], ["Accept", "application/json"]],
    body: JSON.stringify({ name: "x" }),
  });
  const sent = await response.json();
  let again = "";
  try { await response.text(); } catch (e) { again = (e as Error).message; }
  const missing = await fetch("https://api.example.com/missing");
  const bytes = await missing.bytes();
  const form = await fetch("https://api.example.com/form", {
    method: "PUT",
    body: new URLSearchParams({ a: "1" }),
  });
  const binary = await fetch("https://api.example.com/upload", {
    method: "POST",
    body: new Uint8Array([0, 255]),
  });
  let refused = "";
  try { await fetch("ftp://api.example.com/x"); } catch (e) { refused = `${(e as Error).name}: ${(e as Error).message}`; }
  let getBody = "";
  try { await fetch("https://api.example.com/", { body: "x" }); } catch (e) { getBody = (e as Error).message; }
  return {
    status: response.status,
    ok: response.ok,
    url: response.url,
    redirected: response.redirected,
    used: response.bodyUsed,
    contentType: response.headers.get("content-type"),
    xb: response.headers.get("X-B"),
    headers: [...response.headers],
    sent,
    again,
    missing: [missing.status, missing.ok, bytes.length > 0],
    form: await form.json().then((sent) => [sent.body, sent.bodyEncoding]),
    binary: await binary.json().then((sent) => [sent.body, sent.bodyEncoding]),
    refused,
    getBody,
  };
}
"#,
            json!({}),
            builtins_action_set(),
        )
        .await;
    harness.drive().await;
    let run = harness.get(&id).await;
    let output = run.output();
    assert_eq!(output["status"], 200);
    assert_eq!(output["ok"], true);
    assert_eq!(output["url"], "https://api.example.com/v1/items?q=1");
    assert_eq!(output["redirected"], false);
    assert_eq!(output["used"], true);
    assert_eq!(output["contentType"], "application/json");
    assert_eq!(output["xb"], "1, 2");
    assert_eq!(
        output["headers"],
        json!([
            ["content-type", "application/json"],
            ["x-a", "0"],
            ["x-b", "1, 2"]
        ])
    );
    assert_eq!(
        output["sent"],
        json!({
            "url": "https://api.example.com/v1/items?q=1",
            "method": "POST",
            "headers": [
                ["x-trace", "abc"],
                ["accept", "application/json"],
                ["content-type", "text/plain;charset=UTF-8"],
            ],
            "body": "{\"name\":\"x\"}",
        })
    );
    assert_eq!(
        output["again"],
        "the body of this response was already read"
    );
    assert_eq!(output["missing"], json!([404, false, true]));
    assert_eq!(output["form"], json!(["a=1", null]));
    assert_eq!(output["binary"], json!(["AP8=", "base64"]));
    assert_eq!(
        output["refused"],
        "TypeError: fetch only sends http and https requests, not ftp:"
    );
    assert_eq!(output["getBody"], "a GET request cannot have a body");
    let actions: Vec<&str> = run
        .commands()
        .into_iter()
        .filter_map(|command| match command {
            Command::ScheduleCall { action, .. } => Some(action.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(actions, ["http", "http", "http", "http"]);
    harness.assert_replays(&run).await;
}
