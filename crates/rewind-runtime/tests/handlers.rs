//! TypeScript handlers (`archi/05-testing-and-observability.md`, "Test runs"): an
//! `ActionDispatcher` for test runs, running the handlers in an isolate of their own.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use rewind_core::{
    Action, ActionFailure, ActionFailureKind, ActionRoute, ActionSet, Code, RunId, RunMode,
    RunOutcome, RunState, Seq, Timestamp,
};
use rewind_engine::ports::{
    ActionDispatcher, Call, CallAuth, Clock, Dispatched, NamespaceName, RunRef, TaskToken, TenantId,
};
use rewind_engine::{ReplayVerdict, StartRun};
use rewind_runtime::{HandlerError, SandboxLimits, TsHandlers, V8Executor};
use serde_json::{Value, json};

mod support;

use support::{T0, World};

const MOCKS: &str = r#"
type Call = { action: string; runId: string; seq: number; delivery: number; idempotencyKey: string };

let closed = 0;

export default {
  "github.get_issue": ({ number }: { number: number }) => ({ number, title: `issue ${number}` }),
  github: {
    close_issue: async ({ number }: { number: number }, call: Call) => {
      closed += 1;
      return { number, closed, key: call.idempotencyKey, seq: call.seq, delivery: call.delivery };
    },
    // A handler written before calls had actions and deliveries reads `tool` and `attempt`.
    reopen_issue: (_: unknown, call: { tool: string; attempt: number }) => ({
      tool: call.tool,
      attempt: call.attempt,
    }),
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

async fn handlers(source: &str) -> Result<TsHandlers, HandlerError> {
    TsHandlers::new(&Code::new(source), clock(), SandboxLimits::default()).await
}

fn call(action: &str, args: Value) -> Call {
    let run = RunId::from_u128(7);
    Call {
        run: RunRef::new(TenantId::local(), run),
        namespace: NamespaceName::new(NamespaceName::LOCAL).unwrap(),
        labels: Default::default(),
        seq: Seq(3),
        action: Action::new(action, ActionRoute::Harness { group: None }, json!({})),
        server: None,
        api: None,
        args,
        idempotency_key: format!("{run}/3"),
        delivery: 2,
        schema_change_approved: false,
        token: TaskToken("token".into()),
        trace_context: None,
    }
}

async fn dispatch(handlers: &TsHandlers, action: &str, args: Value) -> Dispatched {
    handlers.dispatch(call(action, args), CallAuth::None).await
}

fn failure(dispatched: Dispatched) -> ActionFailure {
    match dispatched {
        Dispatched::Failed(failure) => failure,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[tokio::test]
async fn handlers_answer_by_action_name_or_group() {
    let handlers = handlers(MOCKS).await.unwrap();
    let actions: Vec<&str> = handlers.actions().iter().map(String::as_str).collect();
    assert_eq!(
        actions,
        [
            "clock",
            "fails",
            "github.close_issue",
            "github.get_issue",
            "github.reopen_issue",
            "logs",
            "rejects",
            "spins",
            "waits"
        ]
    );
    assert!(handlers.handles("github.get_issue"));
    assert!(!handlers.handles("github"));

    assert_eq!(
        dispatch(&handlers, "github.get_issue", json!({ "number": 4 })).await,
        Dispatched::Result(json!({ "number": 4, "title": "issue 4" }))
    );
    // Module state persists across calls; the handler sees the call.
    for closed in 1..=2 {
        assert_eq!(
            dispatch(&handlers, "github.close_issue", json!({ "number": 4 })).await,
            Dispatched::Result(json!({
                "number": 4,
                "closed": closed,
                "key": format!("{}/3", RunId::from_u128(7)),
                "seq": 3,
                "delivery": 2,
            }))
        );
    }
    assert_eq!(
        dispatch(&handlers, "github.reopen_issue", json!({})).await,
        Dispatched::Result(json!({ "tool": "github.reopen_issue", "attempt": 2 }))
    );
    assert_eq!(
        dispatch(&handlers, "logs", json!({ "a": 1 })).await,
        Dispatched::Result(Value::Null)
    );
}

#[tokio::test]
async fn failures_carry_their_kind_message_and_data() {
    let handlers = handlers(MOCKS).await.unwrap();
    assert_eq!(
        failure(dispatch(&handlers, "fails", json!({})).await),
        ActionFailure {
            kind: ActionFailureKind::Unavailable,
            message: "rate limited".into(),
            data: Some(json!({ "retryAfter": 3 })),
        }
    );
    assert_eq!(
        failure(dispatch(&handlers, "rejects", json!({})).await),
        ActionFailure {
            kind: ActionFailureKind::Error,
            message: "plain failure".into(),
            data: None,
        }
    );
    let missing = failure(dispatch(&handlers, "slack.post", json!({})).await);
    assert_eq!(missing.kind, ActionFailureKind::Unavailable);
    assert_eq!(missing.message, "no handler for slack.post");
    let waits = failure(dispatch(&handlers, "waits", json!({})).await);
    assert_eq!(
        waits.message,
        "the handler of waits did not settle: handlers cannot wait for timers, \
         signals or actions"
    );
}

/// Handlers see the dispatcher's clock and seeded randomness: two dispatchers answer alike.
#[tokio::test]
async fn handlers_are_deterministic() {
    let a = dispatch(&handlers(MOCKS).await.unwrap(), "clock", json!({})).await;
    let b = dispatch(&handlers(MOCKS).await.unwrap(), "clock", json!({})).await;
    assert_eq!(a, b);
    let Dispatched::Result(result) = a else {
        panic!("{a:?}");
    };
    assert_eq!(result["now"], json!(T0.as_millis()));
}

#[tokio::test]
async fn a_handler_that_spins_is_stopped_and_the_handlers_stop() {
    let handlers = TsHandlers::new(
        &Code::new(MOCKS),
        clock(),
        SandboxLimits {
            cpu_per_trigger: Duration::from_millis(100),
            ..SandboxLimits::default()
        },
    )
    .await
    .unwrap();
    let spun = failure(dispatch(&handlers, "spins", json!({})).await);
    assert_eq!(
        spun.message,
        "the handler of spins used more than 100ms of CPU time reacting to one trigger"
    );
    let after = failure(dispatch(&handlers, "github.get_issue", json!({ "number": 1 })).await);
    assert!(after.message.contains("have stopped"), "{after:?}");
}

/// What a handler logs is kept outside V8's heap until the call ends: a flood is stopped, like
/// one of the code's.
#[tokio::test]
async fn a_handler_that_floods_its_logs_is_stopped() {
    let source = "export default { noisy: () => { const big = \"x\".repeat(1 << 20); \
                  for (;;) console.log(big); } };";
    let handlers = TsHandlers::new(
        &Code::new(source),
        clock(),
        SandboxLimits {
            emitted_bytes_per_trigger: 4 * 1024 * 1024,
            ..SandboxLimits::default()
        },
    )
    .await
    .unwrap();
    let flooded = failure(dispatch(&handlers, "noisy", json!({})).await);
    assert_eq!(
        flooded.message,
        "the handler of noisy emitted more than 4 MiB of commands and logs reacting to one \
         trigger"
    );
}

#[tokio::test]
async fn invalid_modules_are_refused() {
    for (source, expected) in [
        ("export default {", "handlers.ts"),
        (
            "import { sleep } from \"rewind\";\nexport default { a: sleep };",
            "handlers cannot import modules",
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
            "the handler a is not a function",
        ),
        ("throw new Error(\"boom\");\nexport default {};", "boom"),
    ] {
        match handlers(source).await {
            Err(HandlerError::Invalid(message)) => {
                assert!(message.contains(expected), "{source}: {message}");
            }
            other => panic!("{source}: expected an invalid module, got {other:?}"),
        }
    }
}

/// Test runs: the run plane in the driver, with memory adapters, virtual time, and the handlers
/// as dispatcher.
#[tokio::test]
async fn a_test_run_is_answered_by_the_handlers_and_replays() {
    let world = World::new(
        V8Executor::new(SandboxLimits::default()).unwrap(),
        handlers(MOCKS).await.unwrap(),
        1,
        |_| {},
    )
    .await;
    let action_set =
        ActionSet::from_harness_actions(["github.get_issue", "github.close_issue", "fails"])
            .unwrap();
    let mut request = StartRun::new(
        Code::new(
            r#"
import { github, fails } from "rewind:actions";

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
        action_set,
        json!({}),
    );
    request.mode = RunMode::Test;
    let run = world.start(request).await.unwrap();
    world.idle().await;
    let history = world.history(&run).await;
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
        world.replay(&run, None).await,
        ReplayVerdict::Matches(_)
    ));
}
