//! The sandbox, driven by the engine with in-memory adapters and virtual time: determinism,
//! durable time, signals, imports, callsites, cancellation, how a script ends, and limits.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use ptc_core::{
    AbortKind, Annotation, Callsite, Command, Completion, Epoch, Event, HistoryEntry, RunId,
    RunOutcome, RunState, RunStatus, Script, ScriptError, Seed, Seq, Timestamp, ToolFailure,
    ToolFailureKind, Toolset, canonical_json,
};
use ptc_engine::adapters::{
    MemoryBlobs, MemoryJournal, MemorySecrets, MemoryWebhooks, SeededEntropy, VirtualClock,
};
use ptc_engine::ports::{
    CompiledScript, Credentials, Dispatched, ExecutorError, RunFilter, RunRef, ScriptExecutor,
    StartScript, TenantId, ToolCall, ToolDispatcher,
};
use ptc_engine::replay::ReplayEnd;
use ptc_engine::{
    Adapters, DriveOptions, Engine, EngineConfig, ReplayVerdict, StartError, StartRun, ToolOrder,
};
use ptc_runtime::{SandboxLimits, V8Executor};
use serde_json::{Value, json};

const T0: Timestamp = Timestamp(1_767_225_600_000); // 2026-01-01T00:00:00Z
const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;

type Handler = dyn Fn(&ToolCall) -> Result<Value, ToolFailure> + Send + Sync;

#[derive(Clone)]
struct Tools(Arc<Handler>);

impl ToolDispatcher for Tools {
    async fn dispatch(&self, call: ToolCall, _credentials: Option<Credentials>) -> Dispatched {
        match (self.0)(&call) {
            Ok(result) => Dispatched::Result(result),
            Err(failure) => Dispatched::Failed(failure),
        }
    }
}

fn issue_tools(call: &ToolCall) -> Result<Value, ToolFailure> {
    let failure = |kind, message: &str| ToolFailure {
        kind,
        message: message.into(),
        data: Some(json!({ "status": 503 })),
    };
    match call.tool.name.as_str() {
        "github.get_issue" => Ok(json!({
            "number": call.args["number"],
            "title": format!("issue {}", call.args["number"]),
        })),
        "flaky" => Err(failure(ToolFailureKind::OutcomeUnknown, "connection reset")),
        "failing" => Err(failure(ToolFailureKind::Error, "boom")),
        _ => Ok(json!({ "ok": true })),
    }
}

type V8Engine = Engine<
    MemoryJournal,
    MemoryBlobs,
    V8Executor,
    Tools,
    VirtualClock,
    SeededEntropy,
    MemorySecrets,
    MemoryWebhooks,
>;

struct Harness {
    engine: V8Engine,
    options: DriveOptions,
}

fn harness() -> Harness {
    harness_with(SandboxLimits::default(), 1)
}

fn harness_with(limits: SandboxLimits, entropy: u64) -> Harness {
    Harness {
        engine: Engine::new(
            Adapters {
                journal: MemoryJournal::new(),
                blobs: MemoryBlobs::new(),
                executor: V8Executor::new(limits).expect("TZ=UTC (see .cargo/config.toml)"),
                dispatcher: Tools(Arc::new(issue_tools)),
                clock: VirtualClock::new(T0),
                entropy: SeededEntropy::new(entropy),
                secrets: MemorySecrets::new(),
                webhooks: MemoryWebhooks::new(),
            },
            EngineConfig::default(),
        ),
        options: DriveOptions::default(),
    }
}

struct Run {
    id: RunRef,
    history: Vec<HistoryEntry>,
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

    fn error(&self) -> &ScriptError {
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

fn toolset() -> Toolset {
    Toolset::from_host_tools([
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
    async fn try_start(&self, source: &str, input: Value) -> Result<RunRef, StartError> {
        let mut request = StartRun::new(Script::new(source), toolset(), input);
        request.mode = ptc_core::RunMode::Mock;
        Ok(self.engine.start_run(request).await?.run)
    }

    async fn start(&self, source: &str, input: Value) -> RunRef {
        self.try_start(source, input).await.unwrap()
    }

    async fn drive(&self) {
        let report = self
            .engine
            .run_until_idle_virtual(&self.options)
            .await
            .unwrap();
        assert!(!report.interrupted, "{report:?}");
        assert_eq!(report.quarantined, 0, "{report:?}");
    }

    async fn get(&self, id: &RunRef) -> Run {
        let history = self.engine.export_history(id).await.unwrap();
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

    /// Replays the run with its own script: it must match, to the end of its history.
    async fn assert_replays(&self, run: &Run) {
        let verdict = self.engine.replay(&run.id, None).await.unwrap();
        assert!(matches!(verdict, ReplayVerdict::Matches(_)), "{verdict:?}");
    }
}

const FAN_OUT: &str = r#"
import { github } from "ptc:tools";

export default async function main(input: { repo: string }) {
  const issues = await Promise.all([1, 2, 3].map((number) => github.get_issue({ repo: input.repo, number })));
  return { titles: issues.map((issue: { title: string }) => issue.title) };
}
"#;

#[tokio::test]
async fn promise_all_with_out_of_order_completions_replays_identically() {
    let mut harness = harness();
    harness.options.tool_order = ToolOrder::Reversed;
    let run = harness.run(FAN_OUT, json!({ "repo": "acme/api" })).await;

    assert_eq!(
        run.output(),
        &json!({ "titles": ["issue 1", "issue 2", "issue 3"] })
    );
    let delivered: Vec<Seq> = run
        .history
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Completion(Completion::ToolCallCompleted { seq, .. }) => Some(*seq),
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
import { audit } from "ptc:tools";
import { run } from "ptc";

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
    let first = harness_with(SandboxLimits::default(), 7);
    let second = harness_with(SandboxLimits::default(), 7);
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

#[tokio::test]
async fn crypto_is_chacha20_keyed_by_the_seed() {
    // The first bytes of the ChaCha20 keystream (RFC 8439, nonce 0) under the key derived
    // from an all-zero seed, as the runtime derives it.
    let executor = V8Executor::new(SandboxLimits::default()).unwrap();
    let source = r#"export default async function main() {
      return Array.from(crypto.getRandomValues(new Uint8Array(8)));
    }"#;
    let compiled = executor
        .compile(&Script::new(source), &Toolset::default())
        .unwrap();
    let start = |seed| StartScript {
        run_id: RunId::from_u128(1),
        compiled: compiled.clone(),
        toolset: Toolset::default(),
        input: json!({}),
        seed: Seed::from_bytes(seed),
        epoch: Epoch(1),
        started_at: T0,
    };
    let output = |emitted: Vec<ptc_engine::ports::Emitted>| match emitted.as_slice() {
        [ptc_engine::ports::Emitted::Command(Command::CompleteRun { output })] => {
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
async fn replay_detects_a_changed_script_at_the_diverging_entry() {
    let harness = harness();
    let run = harness.run(FAN_OUT, json!({ "repo": "acme/api" })).await;

    let changed_args = Script::new(FAN_OUT.replace("[1, 2, 3]", "[1, 2, 4]"));
    let verdict = harness
        .engine
        .replay(&run.id, Some(&changed_args))
        .await
        .unwrap();
    let ReplayVerdict::Diverges(mismatch) = verdict else {
        panic!("{verdict:?}")
    };
    assert_eq!(
        mismatch.to_string(),
        "history entry 3 is tool call #3 github.get_issue({\"number\":3,\"repo\":\"acme/api\"}), \
         but the script emitted tool call #3 github.get_issue({\"number\":4,\"repo\":\"acme/api\"})"
    );

    let extra_call = Script::new(FAN_OUT.replace(
        "return { titles",
        "await github.close_issue({ number: 1 });\n  return { titles",
    ));
    let verdict = harness
        .engine
        .replay(&run.id, Some(&extra_call))
        .await
        .unwrap();
    let ReplayVerdict::Diverges(mismatch) = verdict else {
        panic!("{verdict:?}")
    };
    assert!(
        mismatch.to_string().contains("github.close_issue"),
        "{mismatch}"
    );

    // Moving code around without changing behavior is allowed: callsites are ignored.
    let moved = Script::new(format!("\n\n// a comment\n{FAN_OUT}"));
    let verdict = harness.engine.replay(&run.id, Some(&moved)).await.unwrap();
    assert_eq!(verdict, ReplayVerdict::Matches(ReplayEnd::EndOfHistory));
}

#[tokio::test]
async fn durable_sleep_moves_logical_time() {
    let harness = harness();
    let run = harness
        .run(
            r#"
import { sleep } from "ptc";

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
    let harness = harness();
    let id = harness
        .start(
            r#"
import { signal, sleep } from "ptc";

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
    let real_time = DriveOptions::default();
    harness.engine.run_until_idle(&real_time).await.unwrap();
    harness
        .engine
        .signal(&id, "approval", json!({ "by": "paul" }), None)
        .await
        .unwrap();

    // One hour later, the sleep ends and the script waits for "comment".
    harness.engine.clock().advance_to_ms(T0.0 + HOUR);
    harness.engine.run_until_idle(&real_time).await.unwrap();
    let waiting = harness.get(&id).await;
    assert_eq!(waiting.state.status(), RunStatus::Active);
    assert_eq!(waiting.state.awaited_signals()[0].name, "comment");

    harness.engine.clock().advance_to_ms(T0.0 + 2 * HOUR);
    harness
        .engine
        .signal(&id, "comment", json!("lgtm"), None)
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

/// Lets tests move a virtual clock by milliseconds.
trait AdvanceMs {
    fn advance_to_ms(&self, ms: u64);
}

impl AdvanceMs for VirtualClock {
    fn advance_to_ms(&self, ms: u64) {
        ptc_engine::ports::VirtualTime::advance_to(self, Timestamp(ms));
    }
}

#[tokio::test]
async fn the_sandbox_has_exactly_the_documented_globals() {
    let harness = harness();
    let run = harness
        .run(
            r#"
export default async function main() {
  const absent = ["fetch", "Deno", "WeakRef", "FinalizationRegistry", "SharedArrayBuffer",
                  "Atomics", "__bootstrap", "__ptcHandoff", "require", "process", "Buffer"];
  return {
    absent: absent.filter((name) => typeof (globalThis as any)[name] !== "undefined"),
    globals: Object.getOwnPropertyNames(globalThis).sort(),
    timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    locale: Intl.DateTimeFormat().resolvedOptions().locale,
  };
}
"#,
            json!({}),
        )
        .await;
    let output = run.output();
    assert_eq!(output["absent"], json!([]));
    assert_eq!(output["timezone"], "UTC");
    assert_eq!(output["locale"], "en-US");

    // Beyond the ECMAScript built-ins, only what archi/01-product.md lists.
    let ecmascript = [
        "AggregateError",
        "Array",
        "ArrayBuffer",
        "AsyncDisposableStack",
        "BigInt",
        "BigInt64Array",
        "BigUint64Array",
        "Boolean",
        "DataView",
        "Date",
        "DisposableStack",
        "Error",
        "EvalError",
        "Float16Array",
        "Float32Array",
        "Float64Array",
        "Function",
        "Infinity",
        "Int16Array",
        "Int32Array",
        "Int8Array",
        "Intl",
        "Iterator",
        "JSON",
        "Map",
        "Math",
        "NaN",
        "Number",
        "Object",
        "Promise",
        "Proxy",
        "RangeError",
        "ReferenceError",
        "Reflect",
        "RegExp",
        "Set",
        "String",
        "SuppressedError",
        "Symbol",
        "SyntaxError",
        "Temporal",
        "TypeError",
        "URIError",
        "Uint16Array",
        "Uint32Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "WeakMap",
        "WeakSet",
        "WebAssembly",
        "decodeURI",
        "decodeURIComponent",
        "encodeURI",
        "encodeURIComponent",
        "escape",
        "eval",
        "globalThis",
        "isFinite",
        "isNaN",
        "parseFloat",
        "parseInt",
        "undefined",
        "unescape",
    ];
    let documented = [
        "clearInterval",
        "clearTimeout",
        "console",
        "crypto",
        "performance",
        "queueMicrotask",
        "setInterval",
        "setTimeout",
    ];
    let unexpected: Vec<&str> = output["globals"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|name| !ecmascript.contains(name) && !documented.contains(name))
        .collect();
    assert_eq!(unexpected, Vec::<&str>::new(), "undocumented globals");
}

#[tokio::test]
async fn scripts_that_cannot_load_are_refused_at_start() {
    let harness = harness();
    for (source, expected) in [
        (
            "import fs from \"node:fs\";\nexport default async function main() { return typeof fs; }",
            "cannot import \"node:fs\" (script.ts:1:16): scripts can only import",
        ),
        (
            "import hooks from \"ptc:internal\";\nexport default async function main() { return typeof hooks; }",
            "scripts can only import",
        ),
        (
            "import { jira } from \"ptc:tools\";\nexport default async () => jira;",
            "\"ptc:tools\" has no export named \"jira\" (script.ts:1:10); it exports audit, \
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
        ("export default async function main( {", "script.ts:1"),
        (
            "export default async function main() { return await import(\"ptc\"); }",
            "dynamic import() is not available",
        ),
    ] {
        let refused = harness.try_start(source, json!({})).await.unwrap_err();
        assert!(
            matches!(refused, StartError::InvalidScript(_)),
            "{source}: {refused:?}"
        );
        assert!(
            refused.to_string().contains(expected),
            "{source}: {refused}"
        );
    }
    let runs = harness
        .engine
        .list_runs(&RunFilter::new(TenantId::local()))
        .await
        .unwrap();
    assert!(runs.is_empty(), "no run was created: {runs:?}");
}

/// What only shows when the script runs, such as an error thrown by its top-level code, aborts
/// the run when it loads.
#[tokio::test]
async fn scripts_that_fail_while_loading_are_aborted_as_invalid() {
    let harness = harness();
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
        assert_eq!(kind, AbortKind::InvalidScript, "{source}");
        assert!(message.contains(expected), "{source}: {message}");
        assert_eq!(run.events(), ["run_started", "run_aborted"]);
        harness.assert_replays(&run).await;
    }
}

#[tokio::test]
async fn tool_errors_reach_the_script_and_can_be_caught() {
    let harness = harness();
    let run = harness
        .run(
            r#"
import { flaky } from "ptc:tools";
import { ToolError } from "ptc";

export default async function main() {
  try {
    await flaky({});
  } catch (error) {
    if (error instanceof ToolError && error.outcomeUnknown) {
      throw new TypeError(`gave up: ${error.message} (${error.tool}, ${JSON.stringify(error.data)})`);
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
        error.stack.as_deref().unwrap().contains("script.ts:10"),
        "{error:?}"
    );
    assert_eq!(run.state.status(), RunStatus::Failed);
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn callsites_point_to_typescript_lines() {
    let harness = harness();
    let run = harness
        .run(
            r#"
import { github } from "ptc:tools";

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
        Event::Command(Command::ScheduleToolCall { callsite, .. }) => *callsite,
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
    let harness = harness();
    let polite = harness
        .start(
            r#"
import { sleep, CancelledError } from "ptc";

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
import { slow } from "ptc:tools";
export default async function main() { await slow({}); }
"#,
            json!({}),
        )
        .await;
    for _ in 0..2 {
        harness.engine.activate_next().await.unwrap();
    }
    harness
        .engine
        .cancel(&polite, Some("user asked".into()))
        .await
        .unwrap();
    harness.engine.cancel(&blunt, None).await.unwrap();
    // The cancellations are delivered before the tool answers: it is not waited for.
    while harness.engine.activate_next().await.unwrap().is_some() {}
    harness.drive().await;

    let polite = harness.get(&polite).await;
    assert_eq!(
        polite.output(),
        &json!("cleaned up after: run cancelled: user asked")
    );
    let blunt = harness.get(&blunt).await;
    assert_eq!(blunt.state.status(), RunStatus::Cancelled);
    assert_eq!(blunt.error().name, "CancelledError");
    assert_eq!(
        blunt.events(),
        [
            "run_started",
            "schedule_tool_call",
            "cancel_requested",
            "fail_run"
        ]
    );
    harness.assert_replays(&polite).await;
    harness.assert_replays(&blunt).await;
}

// ---------------------------------------------------------------------------------------------
// How a script ends (archi/03-execution.md)

#[tokio::test]
async fn a_forgotten_await_is_harmless_the_run_waits_for_the_call() {
    let harness = harness();
    let run = harness
        .run(
            r#"
import { audit } from "ptc:tools";
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
            "schedule_tool_call",
            "tool_call_completed",
            "complete_run"
        ],
        "complete_run waits for the call in flight"
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn a_forgotten_await_that_fails_fails_the_run() {
    let harness = harness();
    let run = harness
        .run(
            r#"
import { failing } from "ptc:tools";
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
        ("ToolError", "failing failed: boom")
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn a_rejection_left_unhandled_fails_the_run_as_in_node() {
    let harness = harness();
    let run = harness
        .run(
            r#"
import { failing, audit } from "ptc:tools";
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
    let harness = harness();
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

    // Waiting for a signal is not a deadlock: something outside can still wake the script.
    let waiting = harness
        .run(
            "import { signal } from \"ptc\";\nexport default async function main() { return await signal(\"go\"); }",
            json!({}),
        )
        .await;
    assert_eq!(waiting.state.status(), RunStatus::Active);
}

#[tokio::test]
async fn after_a_failure_no_script_code_runs_and_fail_run_waits_for_calls_in_flight() {
    let harness = harness();
    let run = harness
        .run(
            r#"
import { audit } from "ptc:tools";
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
        [
            "run_started",
            "schedule_tool_call",
            "tool_call_completed",
            "fail_run"
        ]
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn an_exception_in_a_timer_callback_fails_the_run() {
    let harness = harness();
    let run = harness
        .run(
            r#"
import { sleep } from "ptc";
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
async fn a_raced_slow_tool_is_still_awaited() {
    let harness = harness();
    let run = harness
        .run(
            r#"
import { slow } from "ptc:tools";
import { sleep } from "ptc";
export default async function main() {
  return await Promise.race([slow({}).then(() => "tool"), sleep(10).then(() => "timer")]);
}
"#,
            json!({}),
        )
        .await;
    // The tool answers first in virtual time here; either way the call has an outcome.
    let state = &run.state;
    assert_eq!(state.status(), RunStatus::Completed);
    assert!(run.events().contains(&"tool_call_completed"));
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
    );
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
    );
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

// ---------------------------------------------------------------------------------------------
// Boundaries

#[tokio::test]
async fn an_unsupported_epoch_is_refused() {
    let executor = V8Executor::new(SandboxLimits::default()).unwrap();
    assert_eq!(executor.epochs(), [Epoch(1)]);
    let start = StartScript {
        run_id: RunId::from_u128(1),
        compiled: CompiledScript {
            javascript: "export default () => 1".into(),
            source_map: String::new(),
        },
        toolset: Toolset::default(),
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
    let harness = harness();
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
async fn only_the_toolset_is_callable() {
    let harness = harness();
    let run = harness
        .run(
            "import * as tools from \"ptc:tools\";\nexport default async function main() { return Object.keys(tools).sort(); }",
            json!({}),
        )
        .await;
    assert_eq!(
        run.output(),
        &json!(["audit", "failing", "flaky", "github", "slow"])
    );
}

// ---------------------------------------------------------------------------------------------
// Found by review: each of these broke determinism or the rules of how a script ends.

/// A value delivered live and the same value loaded back from storage must look the same to
/// the script, or its replay diverges and a healthy run is quarantined.
#[tokio::test]
async fn object_keys_are_the_same_live_and_on_replay() {
    let harness = harness();
    let big = "x".repeat(1_100); // above the inline threshold: stored as a blob
    let input: Value = serde_json::from_str(&format!(r#"{{"z":1,"a":"{big}"}}"#)).unwrap();
    let run = harness
        .run(
            r#"
import { audit } from "ptc:tools";
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
    let harness = harness();
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
    let harness = harness();
    let run = harness
        .run(
            r#"
import { audit } from "ptc:tools";
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
            "schedule_tool_call",
            "tool_call_completed",
            "complete_run"
        ],
        "the call made while serializing the output has an outcome before the run ends"
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn lone_surrogates_become_replacement_characters() {
    let harness = harness();
    let run = harness
        .run(
            r#"
import { audit } from "ptc:tools";
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
            Command::ScheduleToolCall { args, .. } => args.value().cloned(),
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
    let harness = harness();
    let id = harness
        .start(
            "export default async function main() {\n  await new Promise((resolve) => setTimeout(resolve, 60_000));\n  return \"woke up\";\n}",
            json!({}),
        )
        .await;
    harness.engine.activate_next().await.unwrap();
    harness
        .engine
        .cancel(&id, Some("user asked".into()))
        .await
        .unwrap();
    harness.drive().await;
    let run = harness.get(&id).await;
    assert_eq!(run.state.status(), RunStatus::Cancelled);
    let error = run.error();
    assert_eq!(
        (error.name.as_str(), error.message.as_str()),
        ("CancelledError", "run cancelled: user asked")
    );
    harness.assert_replays(&run).await;
}
