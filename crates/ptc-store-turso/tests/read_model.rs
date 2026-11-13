//! What the conformance suite of `ReadModel` does not pin, and SQL could get wrong: prefixes
//! compared case-sensitively (Turso's `LIKE` folds ASCII case), text lowercased with Unicode's
//! rules (SQLite's `lower` only knows ASCII), text never matched across two fields whatever
//! characters it holds, NUL included, and task states recorded before a run is deleted.

use std::collections::BTreeMap;

use ptc_core::{
    Command, Digest, Epoch, Event, HistoryEntry, Lifecycle, RunId, RunMode, RunStarted, RunStatus,
    ScriptError, Seed, Seq, Timestamp,
};
use ptc_engine::ports::{
    Ending, Predicate, ProjectedRun, ReadModel, RunOrder, RunQuery, RunRef, RunSnapshot, TaskPhase,
    TaskProgress, TenantId, TextMatch, Wait, WaitKind,
};
use ptc_store_turso::{MasterKey, TursoStore};
use serde_json::json;
use tempfile::TempDir;

const T0: Timestamp = Timestamp(1_767_225_600_000);

fn tenant() -> TenantId {
    TenantId::new("acme").expect("tenant id")
}

fn id(n: u128) -> RunId {
    RunId::from_u128(0x0193_0000_0000_7000_8000_abcd_0000_0000 | n)
}

async fn open(dir: &TempDir) -> TursoStore {
    TursoStore::open(
        dir.path().join("journal.db"),
        MasterKey::from_bytes([3; 32]),
    )
    .await
    .expect("open the store")
}

fn entry(index: u64, event: Event) -> HistoryEntry {
    HistoryEntry {
        index,
        recorded_at: Timestamp(T0.0 + index),
        event,
    }
}

/// A run started, which called `tool`, and failed with `ending` if given; otherwise active,
/// waiting for the signal `signal`.
struct Run<'a> {
    n: u128,
    headline: &'a str,
    labels: &'a [(&'a str, &'a str)],
    tool: &'a str,
    signal: &'a str,
    ending: Option<(&'a str, &'a str)>,
}

async fn project(store: &TursoStore, run: Run<'_>) {
    let started = RunStarted {
        script: Digest::of(b"script"),
        toolset: Digest::of(b"toolset"),
        input: json!({}).into(),
        seed: Seed::from_bytes([0; 32]),
        epoch: Epoch(1),
        expires_at: Timestamp(T0.0 + 86_400_000),
        mode: RunMode::Live,
    };
    let mut entries = vec![
        entry(0, Event::Lifecycle(Lifecycle::RunStarted(started))),
        entry(
            1,
            Event::Command(Command::ScheduleToolCall {
                seq: Seq(1),
                tool: run.tool.into(),
                args: json!({}).into(),
                callsite: None,
            }),
        ),
    ];
    let mut waits = Vec::new();
    let mut ending = None;
    match run.ending {
        Some((error, message)) => {
            let failure = ScriptError {
                name: error.into(),
                message: message.into(),
                stack: None,
            };
            entries.push(entry(
                2,
                Event::Command(Command::FailRun { error: failure }),
            ));
            ending = Some(Ending {
                error: error.into(),
                message: message.into(),
                line: None,
                signature: format!("{error}: {message}"),
                fingerprint: "0123456789abcdef".into(),
            });
        }
        None => waits.push(Wait {
            kind: WaitKind::Signal,
            seq: None,
            name: Some(run.signal.into()),
            since: Timestamp(T0.0 + 1),
            until: None,
            callsite: None,
            task: None,
        }),
    }
    let projected = ProjectedRun {
        run: RunRef::new(tenant(), id(run.n)),
        script: Digest::of(b"script"),
        headline: run.headline.into(),
        toolset: Digest::of(b"toolset"),
        labels: run
            .labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        parent: None,
    };
    let snapshot = RunSnapshot {
        history_len: entries.len() as u64,
        status: if ending.is_some() {
            RunStatus::Failed
        } else {
            RunStatus::Active
        },
        mode: Some(RunMode::Live),
        waits,
        ending,
    };
    store
        .apply(&projected, &entries, &snapshot)
        .await
        .expect("apply");
}

/// The runs a query matches, and those its negation matches, by number.
async fn found(store: &TursoStore, predicate: Predicate) -> (Vec<u128>, Vec<u128>) {
    let mut sides = Vec::new();
    for query in [
        RunQuery::new(tenant()).with(predicate.clone()),
        RunQuery::new(tenant()).without(predicate.clone()),
    ] {
        let runs = store
            .search_runs(&query, RunOrder::Started, None, 100)
            .await
            .expect("search_runs");
        let mut numbers: Vec<u128> = runs
            .iter()
            .map(|summary| summary.run.id.as_uuid().as_u128() & 0xffff)
            .collect();
        numbers.sort_unstable();
        sides.push(numbers);
    }
    let negated = sides.pop().expect("negation");
    (sides.pop().expect("predicate"), negated)
}

fn prefix(value: &str) -> TextMatch {
    TextMatch::Prefix(value.into())
}

fn text(value: &str) -> Predicate {
    Predicate::Text(value.into())
}

#[tokio::test]
async fn prefixes_are_case_sensitive() {
    let dir = TempDir::new().expect("temporary directory");
    let store = open(&dir).await;
    project(
        &store,
        Run {
            n: 1,
            headline: "",
            labels: &[("agent", "Deploy")],
            tool: "GitHub.close_issue",
            signal: "Review",
            ending: None,
        },
    )
    .await;
    project(
        &store,
        Run {
            n: 2,
            headline: "",
            labels: &[("agent", "deploy")],
            tool: "github.close_issue",
            signal: "review",
            ending: None,
        },
    )
    .await;
    let label = |values| Predicate::Label {
        key: "agent".into(),
        values,
    };
    let cases = [
        (label(vec![prefix("De")]), vec![1]),
        (label(vec![prefix("de")]), vec![2]),
        (label(vec![TextMatch::Exact("DEPLOY".into())]), vec![]),
        (Predicate::Tool(vec![prefix("GitHub.")]), vec![1]),
        (Predicate::Tool(vec![prefix("github.")]), vec![2]),
        (Predicate::Signal(vec![prefix("Rev")]), vec![1]),
        (Predicate::Signal(vec![prefix("rev")]), vec![2]),
        (
            Predicate::Id(vec![id(1).to_string().to_uppercase()]),
            vec![],
        ),
        (
            Predicate::Script(vec![Digest::of(b"script").to_hex()[..8].to_uppercase()]),
            vec![],
        ),
    ];
    for (predicate, expected) in cases {
        let all = [1, 2];
        let other: Vec<u128> = all.into_iter().filter(|n| !expected.contains(n)).collect();
        assert_eq!(
            found(&store, predicate.clone()).await,
            (expected, other),
            "{predicate:?}"
        );
    }
}

#[tokio::test]
async fn text_is_lowercased_with_unicode_rules() {
    let dir = TempDir::new().expect("temporary directory");
    let store = open(&dir).await;
    project(
        &store,
        Run {
            n: 1,
            headline: "ÉCOLE Été",
            labels: &[("city", "ZÜRICH")],
            tool: "t",
            signal: "s",
            ending: Some(("ΣφάλμαError", "ÇA N'A PAS MARCHÉ")),
        },
    )
    .await;
    project(
        &store,
        Run {
            n: 2,
            headline: "école",
            labels: &[],
            tool: "t",
            signal: "s",
            ending: None,
        },
    )
    .await;
    for (needle, expected) in [
        ("école été", vec![1]),
        ("école", vec![1, 2]),
        ("zürich", vec![1]),
        ("σφάλμαerror", vec![1]),
        ("ça n'a pas marché", vec![1]),
        ("École", vec![]),
    ] {
        assert_eq!(found(&store, text(needle)).await.0, expected, "{needle:?}");
    }
}

#[tokio::test]
async fn text_never_matches_across_fields() {
    let dir = TempDir::new().expect("temporary directory");
    let store = open(&dir).await;
    project(
        &store,
        Run {
            n: 1,
            headline: "alpha",
            labels: &[("a", "beta"), ("b", "gamma\0delta")],
            tool: "t",
            signal: "s",
            ending: Some(("Epsilon", "zeta")),
        },
    )
    .await;
    for separator in ["", " ", "\n", "\t", "\u{1f}", "\u{1e}", "\0", "|", ",", ":"] {
        for (left, right) in [
            ("alpha", "beta"),
            ("beta", "gamma"),
            ("delta", "epsilon"),
            ("epsilon", "zeta"),
            ("zeta", "alpha"),
        ] {
            let needle = format!("{left}{separator}{right}");
            assert_eq!(
                found(&store, text(&needle)).await,
                (vec![], vec![1]),
                "{needle:?} spans two fields"
            );
        }
    }
    for needle in [
        "gamma\0delta",
        "a\0d",
        "\0",
        "alpha",
        "ta",
        "epsilon",
        "zeta",
        "",
    ] {
        assert_eq!(found(&store, text(needle)).await.0, [1], "{needle:?}");
    }
    assert_eq!(found(&store, text("gamma\0x")).await.0, Vec::<u128>::new());
    let label = Predicate::Label {
        key: "b".into(),
        values: vec![prefix("gamma\0d")],
    };
    assert_eq!(found(&store, label).await.0, [1], "a NUL is a character");
    let label = Predicate::Label {
        key: "b".into(),
        values: vec![prefix("gamma\0x")],
    };
    assert_eq!(found(&store, label).await.0, Vec::<u128>::new());
}

#[tokio::test]
async fn a_deleted_run_loses_what_was_recorded_before_it_was_projected() {
    let dir = TempDir::new().expect("temporary directory");
    let store = open(&dir).await;
    let run = RunRef::new(tenant(), id(1));
    let task = TaskProgress {
        seq: Seq(1),
        attempt: 2,
        phase: TaskPhase::Claimed,
    };
    store
        .record_tasks(&run, &[task])
        .await
        .expect("record_tasks");
    assert!(
        !store.delete_run(&run).await.expect("delete_run"),
        "not projected"
    );
    project(
        &store,
        Run {
            n: 1,
            headline: "",
            labels: &[],
            tool: "t",
            signal: "s",
            ending: None,
        },
    )
    .await;
    assert_eq!(
        store.attempts(&run).await.expect("attempts"),
        BTreeMap::new(),
        "the task states recorded before were deleted with the run"
    );
}
