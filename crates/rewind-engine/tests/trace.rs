//! The LLM trace of a run, and one entry in full (`rewind trace <run> [--entry n]`).

#![allow(clippy::unwrap_used)]

use crate::support;

use rewind_engine::ports::Delivery;
use serde_json::json;
use support::plane::World;
use support::*;

/// Lists issues, logs, waits for `approval` with a timeout of 3 days, then fails.
fn triage() -> FakeExecutor {
    FakeExecutor::new().with(
        "triage",
        |run| {
            run.call("a", json!({ "repo": "acme/api", "state": "open" }));
        },
        |run, delivery| match delivery {
            Delivery::ToolCallCompleted { .. } => {
                run.log("3 stale issues");
                run.timer(3 * DAY);
                run.await_signal("approval");
            }
            Delivery::SignalReceived { .. } => run.fail("TypeError", "decision.close is undefined"),
            _ => {}
        },
    )
}

#[tokio::test]
async fn the_trace_folds_each_call_with_its_outcome_and_truncates_payloads() {
    let mut world = World::new(
        triage(),
        RecordingDispatcher::new(|_| {
            Ok(json!([{ "number": 1 }, { "number": 2 }, { "number": 3 }]))
        }),
    )
    .await;
    let run = world
        .start(request("triage", json!({ "repo": "acme/api" })))
        .await
        .unwrap()
        .run;
    world.settle().await;
    world.advance(HOUR);
    let trace = world.trace(&run).await;
    let lines: Vec<&str> = trace.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "run {}  suspended · waiting for signal \"approval\"",
            run.id
        )
    );
    assert!(
        lines[1].ends_with("· started 2026-01-01T00:00:00Z"),
        "{trace}"
    );
    assert_eq!(lines[2], "");
    assert!(
        lines[5].contains("#1 a({\"repo\":\"acme/api\",\"state\":\"open\"})")
            && lines[5].contains("L1")
            && lines[5].ends_with("→ 3 items · 40 B · 0.0s · entry 2"),
        "{trace}"
    );
    assert!(
        lines[7].contains("timer #2 3d") && lines[7].ends_with("→ pending"),
        "{trace}"
    );

    world
        .signal(&run, "approval", json!({ "close": "x".repeat(300) }), None)
        .await
        .unwrap();
    world.settle().await;
    let trace = world.trace(&run).await;
    let lines: Vec<&str> = trace.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "run {}  failed · TypeError: decision.close is undefined",
            run.id
        )
    );
    assert!(lines[7].ends_with("→ dropped with the run"), "{trace}");
    assert!(
        lines[8].ends_with("→ received after 1h00m · entry 6"),
        "{trace}"
    );
    // A payload is truncated in the trace.
    assert!(
        lines[9].contains("signal \"approval\" ← {\"close\":\"xxx"),
        "{trace}"
    );
    assert!(lines[9].ends_with("…"), "{trace}");
    assert!(lines[10].contains("failed · TypeError: decision.close is undefined"));

    // One entry in full.
    let mut gateway = world.driver.gateway(&world.at.clone());
    let entry = world
        .edge
        .trace_entry(&mut gateway, &run, 6)
        .await
        .unwrap()
        .unwrap();
    assert!(entry.starts_with("entry 6 · signal_received · +1h00m (2026-01-01T01:00:00Z)"));
    assert!(entry.contains(&"x".repeat(300)), "{entry}");
    assert_eq!(
        world
            .edge
            .trace_entry(&mut gateway, &run, 99)
            .await
            .unwrap(),
        None
    );
}
