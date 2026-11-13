//! TypeScript mock handlers (`archi/05-testing-and-observability.md`, "Testing and mocks"): a
//! `ToolDispatcher` for mock mode, running the handlers in an isolate of their own.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use rewind_core::{
    RunId, RunMode, RunOutcome, RunState, Script, Seq, Timestamp, Tool, ToolFailure,
    ToolFailureKind, ToolRoute, Toolset,
};
use rewind_engine::adapters::{
    MemoryBlobs, MemoryJournal, MemorySecrets, MemoryWebhooks, SeededEntropy, VirtualClock,
};
use rewind_engine::ports::{
    Clock, Dispatched, RunRef, TaskToken, TenantId, ToolCall, ToolDispatcher,
};
use rewind_engine::{Adapters, DriveOptions, Engine, EngineConfig, ReplayVerdict, StartRun};
use rewind_runtime::{MockError, SandboxLimits, TsMocks, V8Executor};
use serde_json::{Value, json};

const T0: Timestamp = Timestamp(1_767_225_600_000); // 2026-01-01T00:00:00Z

const MOCKS: &str = r#"
type Call = { tool: string; runId: string; seq: number; attempt: number; idempotencyKey: string };

let closed = 0;

export default {
  "github.get_issue": ({ number }: { number: number }) => ({ number, title: `issue ${number}` }),
  github: {
    close_issue: async ({ number }: { number: number }, call: Call) => {
      closed += 1;
      return { number, closed, key: call.idempotencyKey, seq: call.seq, attempt: call.attempt };
    },
  },
  clock: () => ({ now: Date.now(), random: Math.random(), uuid: crypto.randomUUID() }),
  fails: () => {
    throw Object.assign(new Error("rate limited"), { kind: "unavailable", data: { retryAfter: 3 } });
  },
  rejects: async () => {
    throw "plain failure";
  },
  waits: () => new Promise((resolve) => setTimeout(resolve, 10)),
  spins: () => {
    for (;;) {}
  },
  logs: (args: unknown) => {
    console.log("called with", args);
    return null;
  },
};
"#;

#[derive(Debug)]
struct FixedClock(Timestamp);

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(FixedClock(T0))
}

async fn mocks(source: &str) -> Result<TsMocks, MockError> {
    TsMocks::new(&Script::new(source), clock(), SandboxLimits::default()).await
}

fn call(tool: &str, args: Value) -> ToolCall {
    let run = RunId::from_u128(7);
    ToolCall {
        run: RunRef::new(TenantId::local(), run),
        seq: Seq(3),
        tool: Tool::new(tool, ToolRoute::Host { group: None }, json!({})),
        server: None,
        args,
        idempotency_key: format!("{run}/3"),
        attempt: 2,
        schema_change_approved: false,
        token: TaskToken("token".into()),
        trace_context: None,
    }
}

async fn dispatch(mocks: &TsMocks, tool: &str, args: Value) -> Dispatched {
    mocks.dispatch(call(tool, args), None).await
}

fn failure(dispatched: Dispatched) -> ToolFailure {
    match dispatched {
        Dispatched::Failed(failure) => failure,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn handlers_answer_by_tool_name_or_namespace() {
    let mocks = mocks(MOCKS).await.unwrap();
    let tools: Vec<&str> = mocks.tools().iter().map(String::as_str).collect();
    assert_eq!(
        tools,
        [
            "clock",
            "fails",
            "github.close_issue",
            "github.get_issue",
            "logs",
            "rejects",
            "spins",
            "waits"
        ]
    );
    assert!(mocks.handles("github.get_issue"));
    assert!(!mocks.handles("github"));

    assert_eq!(
        dispatch(&mocks, "github.get_issue", json!({ "number": 4 })).await,
        Dispatched::Result(json!({ "number": 4, "title": "issue 4" }))
    );
    // Module state persists across calls; the handler sees the call.
    for closed in 1..=2 {
        assert_eq!(
            dispatch(&mocks, "github.close_issue", json!({ "number": 4 })).await,
            Dispatched::Result(json!({
                "number": 4,
                "closed": closed,
                "key": format!("{}/3", RunId::from_u128(7)),
                "seq": 3,
                "attempt": 2,
            }))
        );
    }
    assert_eq!(
        dispatch(&mocks, "logs", json!({ "a": 1 })).await,
        Dispatched::Result(Value::Null)
    );
}

#[tokio::test]
async fn failures_carry_their_kind_message_and_data() {
    let mocks = mocks(MOCKS).await.unwrap();
    assert_eq!(
        failure(dispatch(&mocks, "fails", json!({})).await),
        ToolFailure {
            kind: ToolFailureKind::Unavailable,
            message: "rate limited".into(),
            data: Some(json!({ "retryAfter": 3 })),
        }
    );
    assert_eq!(
        failure(dispatch(&mocks, "rejects", json!({})).await),
        ToolFailure {
            kind: ToolFailureKind::Error,
            message: "plain failure".into(),
            data: None,
        }
    );
    let missing = failure(dispatch(&mocks, "slack.post", json!({})).await);
    assert_eq!(missing.kind, ToolFailureKind::Unavailable);
    assert_eq!(missing.message, "no mock handler for slack.post");
    let waits = failure(dispatch(&mocks, "waits", json!({})).await);
    assert_eq!(
        waits.message,
        "the mock handler of waits did not settle: mock handlers cannot wait for timers, \
         signals or tools"
    );
}

/// Mocks see the dispatcher's clock and seeded randomness: two dispatchers answer alike.
#[tokio::test]
async fn handlers_are_deterministic() {
    let a = dispatch(&mocks(MOCKS).await.unwrap(), "clock", json!({})).await;
    let b = dispatch(&mocks(MOCKS).await.unwrap(), "clock", json!({})).await;
    assert_eq!(a, b);
    let Dispatched::Result(result) = a else {
        panic!("{a:?}");
    };
    assert_eq!(result["now"], json!(T0.as_millis()));
}

#[tokio::test]
async fn a_handler_that_spins_is_stopped_and_the_mocks_stop() {
    let mocks = TsMocks::new(
        &Script::new(MOCKS),
        clock(),
        SandboxLimits {
            cpu_per_trigger: Duration::from_millis(100),
            ..SandboxLimits::default()
        },
    )
    .await
    .unwrap();
    let spun = failure(dispatch(&mocks, "spins", json!({})).await);
    assert_eq!(
        spun.message,
        "the mock handler of spins used more than 100ms of CPU time reacting to one trigger"
    );
    let after = failure(dispatch(&mocks, "github.get_issue", json!({ "number": 1 })).await);
    assert!(after.message.contains("have stopped"), "{after:?}");
}

/// What a handler logs is kept outside V8's heap until the call ends: a flood is stopped, like
/// one of the script's.
#[tokio::test]
async fn a_handler_that_floods_its_logs_is_stopped() {
    let source = "export default { noisy: () => { const big = \"x\".repeat(1 << 20); \
                  for (;;) console.log(big); } };";
    let mocks = TsMocks::new(
        &Script::new(source),
        clock(),
        SandboxLimits {
            emitted_bytes_per_trigger: 4 * 1024 * 1024,
            ..SandboxLimits::default()
        },
    )
    .await
    .unwrap();
    let flooded = failure(dispatch(&mocks, "noisy", json!({})).await);
    assert_eq!(
        flooded.message,
        "the mock handler of noisy emitted more than 4 MiB of commands and logs reacting to one \
         trigger"
    );
}

#[tokio::test]
async fn invalid_modules_are_refused() {
    for (source, expected) in [
        ("export default {", "mocks.ts"),
        (
            "import { sleep } from \"rewind\";\nexport default { a: sleep };",
            "mock handlers cannot import modules",
        ),
        (
            "export default 42;",
            "must `export default` an object of handlers",
        ),
        (
            "export const a = () => 1;",
            "must `export default` an object of handlers",
        ),
        (
            "export default { a: 1 };",
            "the mock handler a is not a function",
        ),
        ("throw new Error(\"boom\");\nexport default {};", "boom"),
    ] {
        match mocks(source).await {
            Err(MockError::Invalid(message)) => {
                assert!(message.contains(expected), "{source}: {message}");
            }
            other => panic!("{source}: expected an invalid module, got {other:?}"),
        }
    }
}

/// Mock mode: the engine with in-memory adapters, virtual time, and the mocks as dispatcher.
#[tokio::test]
async fn a_run_in_mock_mode_is_answered_by_the_handlers_and_replays() {
    let engine = Engine::new(
        Adapters {
            journal: MemoryJournal::new(),
            blobs: MemoryBlobs::new(),
            executor: V8Executor::new(SandboxLimits::default()).unwrap(),
            dispatcher: mocks(MOCKS).await.unwrap(),
            clock: VirtualClock::new(T0),
            entropy: SeededEntropy::new(1),
            secrets: MemorySecrets::new(),
            webhooks: MemoryWebhooks::new(),
        },
        EngineConfig::default(),
    );
    let toolset =
        Toolset::from_host_tools(["github.get_issue", "github.close_issue", "fails"]).unwrap();
    let mut request = StartRun::new(
        Script::new(
            r#"
import { github, fails } from "rewind:tools";

export default async function main() {
  const issue = await github.get_issue({ number: 4 });
  const closed = await github.close_issue({ number: issue.number });
  let failure = "";
  try {
    await fails({});
  } catch (e) {
    failure = (e as Error).message;
  }
  return { title: issue.title, closed: closed.closed, failure };
}
"#,
        ),
        toolset,
        json!({}),
    );
    request.mode = RunMode::Mock;
    let run = engine.start_run(request).await.unwrap().run;
    engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    let history = engine.export_history(&run).await.unwrap();
    let state = RunState::from_history(&history).unwrap();
    match state.outcome() {
        Some(RunOutcome::Completed { output }) => assert_eq!(
            output.value(),
            Some(
                &json!({ "title": "issue 4", "closed": 1, "failure": "fails failed: rate limited" })
            )
        ),
        other => panic!("{other:?}\n{history:#?}"),
    }
    assert!(matches!(
        engine.replay(&run, None).await.unwrap(),
        ReplayVerdict::Matches(_)
    ));
}
