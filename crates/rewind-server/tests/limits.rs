//! The limits of `archi/03-execution.md` ("Resource limits", "Run TTL") through the HTTP API,
//! with the server's defaults: 5 s of CPU time per call into the script, 128 MiB of heap,
//! 10,000 commands per activation, 50,000 history entries, 8 MiB per payload, and the TTL
//! bounded by the deployment's maximum.
//!
//! A limit exceeded by the script ends the run with `run_aborted { limit_exceeded }`: the trigger
//! is committed with it, and what the script emitted in response is dropped. A payload the API
//! receives above the limit is refused with 413 before anything is stored.
//!
//! Proved elsewhere: the quota of active runs (429) in `api.rs`
//! (`a_tenant_above_its_quota_is_refused_with_429`), and the same limits, lowered, at the
//! engine's level in the e2e corpus (`red/cpu_limit`, `red/heap_limit`, `red/too_many_commands`
//! …). The 256 KiB bound of `run_script` is the SDK's (`sdk/typescript/test/live.test.ts`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::Duration;

use rewind_api::Client;
use rewind_api::access::BuiltInRole;
use rewind_api::common::{AbortKind, RunStatus};
use rewind_api::runs::{JournalExport, Run, StartRun};
use rewind_api::tasks::{CompleteTask, TaskStreamQuery};
use rewind_api::toolsets::{CreateToolset, HostTool};
use rewind_api::{Problem, ProblemType};
use serde_json::{Value, json};
use support::{TestServer, wait};

/// The payload limit of the engine's defaults, in bytes of canonical JSON.
const MAX_PAYLOAD: usize = 8 * 1024 * 1024;

fn start(script: &str) -> StartRun {
    let mut start = StartRun::new(script);
    start.check = Some(false);
    start
}

/// Waits up to `limit` for the run to end.
async fn ended(client: &Client, run: &str, limit: Duration) -> Run {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let state = client.get_run(run, wait("30s")).await.unwrap();
        if !matches!(state.status, RunStatus::Pending | RunStatus::Active) {
            return state;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "run {run} did not end within {limit:?}: {state:?}"
        );
    }
}

/// Asserts that the run was aborted for a limit, with a message containing `message`.
fn assert_limit(run: &Run, message: &str) {
    assert_eq!(run.status, RunStatus::Aborted, "{run:?}");
    let abort = run.abort.as_ref().expect("an abort reason");
    assert_eq!(abort.kind, AbortKind::LimitExceeded, "{abort:?}");
    assert!(
        abort.message.contains(message),
        "{:?} does not contain {message:?}",
        abort.message
    );
}

/// The `type` of each entry of the run's journal.
async fn events(client: &Client, run: &str) -> Vec<String> {
    let journal = client
        .export_journal(run, JournalExport::Journal)
        .await
        .unwrap();
    journal["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["event"]["type"].as_str().unwrap().to_owned())
        .collect()
}

fn problem(error: rewind_api::Error) -> Problem {
    match error {
        rewind_api::Error::Problem(problem) => *problem,
        other => panic!("expected a problem, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cpu_time_is_bounded_per_call_into_the_script() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    // In `main(input)`, and in the delivery of a timer: each call has its own 5 s.
    let in_main = harness
        .start_run(
            &start("export default async function main() { for (;;) {} }"),
            None,
        )
        .await
        .unwrap();
    let in_delivery = harness
        .start_run(
            &start(
                r#"import { sleep } from "rewind";
export default async function main() {
  await sleep(1);
  for (;;) {}
}"#,
            ),
            None,
        )
        .await
        .unwrap();
    for run in [&in_main, &in_delivery] {
        let run = ended(&harness, &run.id, Duration::from_secs(120)).await;
        assert_limit(&run, "more than 5000ms of CPU time reacting to one trigger");
    }
    assert_eq!(
        events(&harness, &in_main.id).await,
        ["run_started", "run_aborted"]
    );
    // The timer that woke the loop is committed with the abort.
    assert_eq!(
        events(&harness, &in_delivery.id).await,
        ["run_started", "start_timer", "timer_fired", "run_aborted"]
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_heap_is_bounded_per_instance() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let run = harness
        .start_run(
            &start(
                "export default async function main() {
  const hoard: number[][] = [];
  for (;;) hoard.push(new Array(100_000).fill(1));
}",
            ),
            None,
        )
        .await
        .unwrap();
    let run = ended(&harness, &run.id, Duration::from_secs(120)).await;
    assert_limit(&run, "more than 128 MiB of heap");
    assert_eq!(
        events(&harness, &run.id).await,
        ["run_started", "run_aborted"]
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commands_are_bounded_per_activation() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let run = harness
        .start_run(
            &start(
                r#"import { sleep } from "rewind";
export default async function main() {
  await Promise.all(Array.from({ length: 10_001 }, () => sleep(1)));
}"#,
            ),
            None,
        )
        .await
        .unwrap();
    let run = ended(&harness, &run.id, Duration::from_secs(120)).await;
    assert_limit(&run, "more than 10000 commands in one activation");
    // Nothing the script emitted in response is kept, and no timer was started.
    assert_eq!(
        events(&harness, &run.id).await,
        ["run_started", "run_aborted"]
    );
    assert!(run.waiting_for.is_empty());
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_history_is_capped_at_50000_entries() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    // Batches of 9,000 timers (below the commands' bound): 18,000 entries each.
    let run = harness
        .start_run(
            &start(
                r#"import { sleep } from "rewind";
export default async function main() {
  for (let batch = 0; batch < 10; batch++) {
    await Promise.all(Array.from({ length: 9_000 }, () => sleep(1)));
  }
}"#,
            ),
            None,
        )
        .await
        .unwrap();
    let run = ended(&harness, &run.id, Duration::from_secs(600)).await;
    assert_limit(&run, "the history would exceed 50000 entries");
    // The cap bounds what the run records before it ends: the trigger that would cross it is
    // committed with `run_aborted`, two entries past the cap at most.
    assert!(
        run.history_len <= 50_002 && run.history_len > 45_000,
        "{} entries",
        run.history_len
    );
    let events = events(&harness, &run.id).await;
    assert_eq!(events.len() as u64, run.history_len);
    assert_eq!(
        &events[events.len() - 2..],
        ["timer_fired", "run_aborted"],
        "the trigger is committed with the abort"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payloads_are_bounded_to_8_mib() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let worker = server.key(BuiltInRole::Worker, &[]).await;
    let too_large = Value::String("x".repeat(MAX_PAYLOAD));

    // The API refuses a payload it is given: input, signal, a host tool's result.
    let mut refused = start("export default async function main() {}");
    refused.input = json!({ "blob": too_large });
    let problem_of = problem(harness.start_run(&refused, None).await.unwrap_err());
    assert_eq!(problem_of.kind(), Some(ProblemType::PayloadTooLarge));
    assert_eq!(problem_of.status, 413);

    let toolset = harness
        .create_toolset(&CreateToolset {
            host: vec![HostTool {
                name: "store.put".into(),
                description: "Stores a value.".into(),
                input_schema: json!({ "type": "object" }),
                output_schema: None,
                policy: None,
            }],
            ..CreateToolset::default()
        })
        .await
        .unwrap();
    let mut waiting = start(
        r#"import { signal } from "rewind";
import { store } from "rewind:tools";
export default async function main() {
  const got = await store.put({ small: true });
  const signalled = await signal("go");
  return { got, signalled };
}"#,
    );
    waiting.toolset = Some(toolset.id.clone());
    let waiting = harness.start_run(&waiting, None).await.unwrap();
    let task = support::TaskWorker::open(
        &worker,
        &TaskStreamQuery {
            toolset: Some(toolset.id.clone()),
            ..TaskStreamQuery::default()
        },
    )
    .await
    .task()
    .await;
    let result = problem(
        worker
            .complete_task(
                &task.token,
                &CompleteTask {
                    result: too_large.clone(),
                },
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(result.kind(), Some(ProblemType::PayloadTooLarge));
    worker
        .complete_task(&task.token, &CompleteTask { result: json!(1) })
        .await
        .unwrap();
    let signal = problem(
        harness
            .signal_run(&waiting.id, "go", &too_large, None)
            .await
            .unwrap_err(),
    );
    assert_eq!(signal.kind(), Some(ProblemType::PayloadTooLarge));
    harness
        .signal_run(&waiting.id, "go", &json!("small"), None)
        .await
        .unwrap();
    let completed = ended(&harness, &waiting.id, Duration::from_secs(60)).await;
    assert_eq!(completed.status, RunStatus::Completed, "{completed:?}");
    assert_eq!(
        completed.output.unwrap().value,
        Some(json!({ "got": 1, "signalled": "small" }))
    );

    // What the script emits above the limit ends the run: a tool call's arguments, its output.
    let mut args = start(
        r#"import { store } from "rewind:tools";
export default async function main() {
  await store.put({ blob: "x".repeat(8 * 1024 * 1024) });
}"#,
    );
    args.toolset = Some(toolset.id.clone());
    let args = harness.start_run(&args, None).await.unwrap();
    let output = harness
        .start_run(
            &start(
                r#"export default async function main() { return "x".repeat(8 * 1024 * 1024); }"#,
            ),
            None,
        )
        .await
        .unwrap();
    let args = ended(&harness, &args.id, Duration::from_secs(60)).await;
    assert_limit(&args, "the payload of schedule_tool_call is");
    assert_eq!(
        events(&harness, &args.id).await,
        ["run_started", "run_aborted"]
    );
    let output = ended(&harness, &output.id, Duration::from_secs(60)).await;
    assert_limit(&output, "the payload of complete_run is");
    assert_limit(&output, "above the limit of 8388608 bytes");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ttls_are_bounded_by_the_deployment() {
    let server = TestServer::start(&["--default-ttl", "1h", "--max-ttl", "1d"]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let script = r#"import { signal } from "rewind";
export default async function main() { await signal("never"); }"#;

    let run = harness.start_run(&start(script), None).await.unwrap();
    let ttl = run.expires_at.duration_since(run.created_at).unsigned_abs();
    assert_eq!(
        ttl,
        Duration::from_secs(3_600),
        "the deployment's default TTL"
    );

    let mut longer = start(script);
    longer.ttl = Some("2d".parse().unwrap());
    let refused = problem(harness.start_run(&longer, None).await.unwrap_err());
    assert_eq!(
        refused.kind(),
        Some(ProblemType::InvalidRequest),
        "{refused:?}"
    );
    assert!(refused.detail.contains("TTL"), "{}", refused.detail);

    let mut shorter = start(script);
    shorter.ttl = Some("12h".parse().unwrap());
    let run = harness.start_run(&shorter, None).await.unwrap();
    assert_eq!(
        run.expires_at.duration_since(run.created_at).unsigned_abs(),
        Duration::from_secs(12 * 3_600)
    );
    server.stop().await;
}
