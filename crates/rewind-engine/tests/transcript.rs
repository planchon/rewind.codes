//! The transcript of a run, and one event in full (`rewind transcript <run> [--event n]`).

#![allow(clippy::unwrap_used)]

mod support;

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
            Delivery::CallCompleted { .. } => {
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
async fn the_transcript_folds_each_call_with_its_outcome_and_truncates_payloads() {
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
    let transcript = world.transcript(&run).await;
    let lines: Vec<&str> = transcript.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "run {}  suspended · waiting for signal \"approval\"",
            run.id
        )
    );
    assert!(
        lines[1].ends_with("· started 2026-01-01T00:00:00Z"),
        "{transcript}"
    );
    assert_eq!(lines[2], "");
    assert!(
        lines[5].contains("#1 a({\"repo\":\"acme/api\",\"state\":\"open\"})")
            && lines[5].contains("L1")
            && lines[5].ends_with("→ 3 items · 40 B · 0.0s · event 2"),
        "{transcript}"
    );
    assert!(
        lines[7].contains("timer #2 3d") && lines[7].ends_with("→ pending"),
        "{transcript}"
    );

    world
        .signal(&run, "approval", json!({ "close": "x".repeat(300) }), None)
        .await
        .unwrap();
    world.settle().await;
    let transcript = world.transcript(&run).await;
    let lines: Vec<&str> = transcript.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "run {}  failed · TypeError: decision.close is undefined",
            run.id
        )
    );
    assert!(lines[7].ends_with("→ dropped with the run"), "{transcript}");
    assert!(
        lines[8].ends_with("→ received after 1h00m · event 6"),
        "{transcript}"
    );
    // A payload is truncated in the transcript.
    assert!(
        lines[9].contains("signal \"approval\" ← {\"close\":\"xxx"),
        "{transcript}"
    );
    assert!(lines[9].ends_with("…"), "{transcript}");
    assert!(lines[10].contains("failed · TypeError: decision.close is undefined"));

    // One event in full.
    let mut gateway = world.driver.gateway(&world.at.clone());
    let entry = world
        .edge
        .transcript_event(&mut gateway, &run, 6)
        .await
        .unwrap()
        .unwrap();
    assert!(entry.starts_with("event 6 · signal_received · +1h00m (2026-01-01T01:00:00Z)"));
    assert!(entry.contains(&"x".repeat(300)), "{entry}");
    assert_eq!(
        world
            .edge
            .transcript_event(&mut gateway, &run, 99)
            .await
            .unwrap(),
        None
    );
}
