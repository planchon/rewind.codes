//! The conformance suite of `ReadModel`: run summaries, searches, scripts, tool statistics and
//! observed shapes, projected idempotently and in any order; tenants; retention.

use std::collections::{BTreeMap, BTreeSet};

use ptc_core::{
    Command, Completion, Digest, Epoch, Event, Field, HistoryEntry, Lifecycle, Payload, RunId,
    RunMode, RunStarted, RunStatus, ScriptError, Seed, Seq, Shape, Timestamp, ToolFailure,
    ToolFailureKind,
};
use serde_json::json;

use super::{T0, tenant};
use crate::ports::{
    Latency, ProjectedRun, ReadModel, RunRef, RunSearch, RunSummary, ScriptFilter, TenantId,
    TenantScope, ToolStats, ToolStatsFilter,
};

/// Runs every check, each on new stores.
pub async fn run_all<S: ReadModel>(new_store: impl Fn() -> S) {
    eprintln!("read model conformance: runs_are_summarized_and_searched");
    runs_are_summarized_and_searched(&new_store()).await;
    eprintln!("read model conformance: scripts_and_tools_are_aggregated");
    scripts_and_tools_are_aggregated(&new_store()).await;
    eprintln!("read model conformance: scripts_are_filtered_and_count_labels");
    scripts_are_filtered_and_count_labels(&new_store()).await;
    eprintln!("read model conformance: attempts_are_recorded_per_call");
    attempts_are_recorded_per_call(&new_store()).await;
    eprintln!("read model conformance: shapes_are_observed_per_tenant_and_tool");
    shapes_are_observed_per_tenant_and_tool(&new_store()).await;
    eprintln!("read model conformance: projections_ignore_order_and_repetition");
    projections_ignore_order_and_repetition(&new_store(), &new_store()).await;
    eprintln!("read model conformance: runs_are_purged_and_deleted");
    runs_are_purged_and_deleted(&new_store()).await;
}

// -------------------------------------------------------------------------------------------
// The projected runs

fn acme() -> TenantId {
    tenant("acme")
}

fn id(n: u128) -> RunId {
    RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000 | n)
}

fn script(n: u8) -> Digest {
    Digest::of(&[b's', n])
}

fn toolset(n: u8) -> Digest {
    Digest::of(&[b't', n])
}

fn at(ms: u64) -> Timestamp {
    Timestamp(T0.0 + ms)
}

/// Builds a history entry by entry, indexes in order.
struct History(Vec<HistoryEntry>);

impl History {
    fn started(ms: u64) -> Self {
        let started = RunStarted {
            script: script(0),
            toolset: toolset(0),
            input: json!({}).into(),
            seed: Seed::from_bytes([0; 32]),
            epoch: Epoch(1),
            expires_at: at(1_000_000),
            mode: RunMode::Live,
        };
        Self(vec![HistoryEntry {
            index: 0,
            recorded_at: at(ms),
            event: Event::Lifecycle(Lifecycle::RunStarted(started)),
        }])
    }

    fn push(mut self, ms: u64, event: Event) -> Self {
        let index = self.0.len() as u64;
        self.0.push(HistoryEntry {
            index,
            recorded_at: at(ms),
            event,
        });
        self
    }

    fn call(self, ms: u64, seq: u32, tool: &str) -> Self {
        self.push(
            ms,
            Event::Command(Command::ScheduleToolCall {
                seq: Seq(seq),
                tool: tool.into(),
                args: json!({ "seq": seq }).into(),
                callsite: None,
            }),
        )
    }

    fn result(self, ms: u64, seq: u32, result: Payload) -> Self {
        self.push(
            ms,
            Event::Completion(Completion::ToolCallCompleted {
                seq: Seq(seq),
                result,
            }),
        )
    }

    fn failure(self, ms: u64, seq: u32, kind: ToolFailureKind) -> Self {
        let error = ToolFailure {
            kind,
            message: "m".into(),
            data: None,
        };
        self.push(
            ms,
            Event::Completion(Completion::ToolCallFailed {
                seq: Seq(seq),
                error,
            }),
        )
    }

    fn complete(self, ms: u64) -> Self {
        let output = json!(null).into();
        self.push(ms, Event::Command(Command::CompleteRun { output }))
    }

    fn fail(self, ms: u64, name: &str) -> Self {
        let error = ScriptError {
            name: name.into(),
            message: "m".into(),
            stack: None,
        };
        self.push(ms, Event::Command(Command::FailRun { error }))
    }
}

struct Fixture {
    run: ProjectedRun,
    history: Vec<HistoryEntry>,
}

fn projected(tenant: TenantId, n: u128, script_n: u8, toolset_n: u8, agent: &str) -> ProjectedRun {
    ProjectedRun {
        run: RunRef::new(tenant, id(n)),
        script: script(script_n),
        toolset: toolset(toolset_n),
        labels: BTreeMap::from([("agent".to_owned(), agent.to_owned())]),
    }
}

/// Five runs: four of acme (completed, failed, cancelled, active) and one of another tenant.
fn fixtures() -> Vec<Fixture> {
    let r1 = History::started(0)
        .call(0, 1, "github.list_issues")
        .result(100, 1, json!({ "id": 1, "title": "x" }).into())
        .call(100, 2, "slack.post")
        .failure(300, 2, ToolFailureKind::Unavailable)
        .complete(300);
    let r2 = History::started(1_000)
        .call(1_000, 1, "github.list_issues")
        .result(1_400, 1, json!({ "id": 2, "closed": true }).into())
        .call(1_400, 2, "github.close_issue")
        .failure(1_500, 2, ToolFailureKind::Error)
        .fail(1_500, "TypeError");
    let r3 = History::started(2_000)
        .call(2_000, 1, "github.list_issues")
        .result(2_200, 1, Payload::Digest(Digest::of(b"large result")))
        .fail(2_500, ScriptError::CANCELLED);
    let r4 = History::started(3_000).call(3_000, 1, "slack.post");
    let r5 = History::started(10)
        .call(10, 1, "github.list_issues")
        .result(20, 1, json!({ "secret_of_other": "x" }).into())
        .complete(50);
    vec![
        Fixture {
            run: projected(acme(), 1, 1, 1, "triage"),
            history: r1.0,
        },
        Fixture {
            run: projected(acme(), 2, 1, 1, "deploy"),
            history: r2.0,
        },
        Fixture {
            run: projected(acme(), 3, 2, 2, "triage"),
            history: r3.0,
        },
        Fixture {
            run: projected(acme(), 4, 2, 1, "deploy"),
            history: r4.0,
        },
        Fixture {
            run: projected(tenant("other"), 5, 1, 1, "triage"),
            history: r5.0,
        },
    ]
}

/// Applies every fixture, each split in two batches.
async fn project<S: ReadModel>(store: &S) {
    for fixture in fixtures() {
        let (first, second) = fixture.history.split_at(fixture.history.len() / 2);
        store.apply(&fixture.run, first).await.expect("apply");
        store.apply(&fixture.run, second).await.expect("apply");
    }
}

async fn search<S: ReadModel>(store: &S, search: RunSearch) -> Vec<RunId> {
    store
        .search_runs(&search)
        .await
        .expect("search_runs")
        .into_iter()
        .map(|summary| summary.run.id)
        .collect()
}

// -------------------------------------------------------------------------------------------
// Checks

pub async fn runs_are_summarized_and_searched<S: ReadModel>(store: &S) {
    project(store).await;
    let all = store
        .search_runs(&RunSearch::new(acme(), 10))
        .await
        .expect("search_runs");
    assert_eq!(
        all.iter().map(|summary| summary.run.id).collect::<Vec<_>>(),
        [id(4), id(3), id(2), id(1)],
        "newest run id first, one tenant"
    );
    assert_eq!(
        all[3],
        RunSummary {
            run: RunRef::new(acme(), id(1)),
            script: script(1),
            toolset: toolset(1),
            status: RunStatus::Completed,
            labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
            started_at: Some(at(0)),
            ended_at: Some(at(300)),
            last_activity_at: at(300),
            tool_calls: 2,
            errors: 1,
            history_len: 6,
        }
    );
    let statuses: Vec<RunStatus> = all.iter().map(|summary| summary.status).collect();
    assert_eq!(
        statuses,
        [
            RunStatus::Active,
            RunStatus::Cancelled,
            RunStatus::Failed,
            RunStatus::Completed
        ]
    );
    assert_eq!((all[0].ended_at, all[0].history_len), (None, 2));

    let with = |change: &dyn Fn(&mut RunSearch)| {
        let mut query = RunSearch::new(acme(), 10);
        change(&mut query);
        query
    };
    let cases: Vec<(RunSearch, Vec<RunId>)> = vec![
        (with(&|q| q.status = Some(RunStatus::Failed)), vec![id(2)]),
        (
            with(&|q| q.labels = BTreeMap::from([("agent".into(), "triage".into())])),
            vec![id(3), id(1)],
        ),
        (with(&|q| q.script = Some(script(2))), vec![id(4), id(3)]),
        (with(&|q| q.toolset = Some(toolset(2))), vec![id(3)]),
        (
            with(&|q| q.tool = Some("github.close_issue".into())),
            vec![id(2)],
        ),
        (
            with(&|q| q.tool = Some("github.list_issues".into())),
            vec![id(3), id(2), id(1)],
        ),
        (
            with(&|q| q.error_kind = Some(ToolFailureKind::Unavailable)),
            vec![id(1)],
        ),
        (
            with(&|q| q.started_after = Some(at(1_000))),
            vec![id(4), id(3), id(2)],
        ),
        (with(&|q| q.started_before = Some(at(1_000))), vec![id(1)]),
        (with(&|q| q.before = Some(id(3))), vec![id(2), id(1)]),
        (with(&|q| q.limit = 1), vec![id(4)]),
        (
            with(&|q| {
                q.tool = Some("slack.post".into());
                q.status = Some(RunStatus::Active);
            }),
            vec![id(4)],
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(search(store, query.clone()).await, expected, "{query:?}");
    }
    assert_eq!(
        search(store, RunSearch::new(tenant("other"), 10)).await,
        [id(5)]
    );
    assert!(
        search(store, RunSearch::new(tenant("nobody"), 10))
            .await
            .is_empty()
    );
}

pub async fn scripts_and_tools_are_aggregated<S: ReadModel>(store: &S) {
    project(store).await;
    let scripts = store
        .scripts(&ScriptFilter::new(acme()))
        .await
        .expect("scripts");
    let mut expected_order = [script(1), script(2)];
    expected_order.sort();
    assert_eq!(
        scripts.iter().map(|s| s.script).collect::<Vec<_>>(),
        expected_order,
        "ordered by digest"
    );
    let first = scripts
        .iter()
        .find(|s| s.script == script(1))
        .expect("script 1");
    assert_eq!(first.runs, 2);
    assert_eq!(
        first.statuses,
        BTreeMap::from([(RunStatus::Completed, 1), (RunStatus::Failed, 1)])
    );
    assert_eq!(
        first.tools,
        BTreeSet::from([
            "github.close_issue".to_owned(),
            "github.list_issues".to_owned(),
            "slack.post".to_owned()
        ])
    );
    assert_eq!(
        (first.first_started_at, first.last_started_at),
        (Some(at(0)), Some(at(1_000)))
    );
    let second = scripts
        .iter()
        .find(|s| s.script == script(2))
        .expect("script 2");
    assert_eq!(
        second.statuses,
        BTreeMap::from([(RunStatus::Active, 1), (RunStatus::Cancelled, 1)])
    );
    let recent = store
        .scripts(&ScriptFilter {
            since: Some(at(1_000)),
            ..ScriptFilter::new(acme())
        })
        .await
        .expect("scripts");
    let first = recent
        .iter()
        .find(|s| s.script == script(1))
        .expect("script 1");
    assert_eq!(first.runs, 1, "only the runs started since");

    let stats = store
        .tool_stats(&ToolStatsFilter {
            tenant: acme(),
            toolset: None,
            since: None,
            until: None,
        })
        .await
        .expect("tool_stats");
    assert_eq!(
        stats,
        [
            ToolStats {
                tool: "github.close_issue".into(),
                calls: 1,
                completed: 0,
                failed: 1,
                errors_by_kind: BTreeMap::from([(ToolFailureKind::Error, 1)]),
                retries: 0,
                latency: Some(Latency {
                    count: 1,
                    mean_ms: 100,
                    p50_ms: 100,
                    p95_ms: 100,
                    p99_ms: 100,
                    max_ms: 100
                }),
            },
            ToolStats {
                tool: "github.list_issues".into(),
                calls: 3,
                completed: 3,
                failed: 0,
                errors_by_kind: BTreeMap::new(),
                retries: 0,
                latency: Some(Latency {
                    count: 3,
                    mean_ms: 233,
                    p50_ms: 200,
                    p95_ms: 400,
                    p99_ms: 400,
                    max_ms: 400
                }),
            },
            ToolStats {
                tool: "slack.post".into(),
                calls: 2,
                completed: 0,
                failed: 1,
                errors_by_kind: BTreeMap::from([(ToolFailureKind::Unavailable, 1)]),
                retries: 0,
                latency: Some(Latency {
                    count: 1,
                    mean_ms: 200,
                    p50_ms: 200,
                    p95_ms: 200,
                    p99_ms: 200,
                    max_ms: 200
                }),
            },
        ]
    );
    let of_toolset = store
        .tool_stats(&ToolStatsFilter {
            tenant: acme(),
            toolset: Some(toolset(2)),
            since: None,
            until: None,
        })
        .await
        .expect("tool_stats");
    assert_eq!(
        of_toolset
            .iter()
            .map(|s| (s.tool.as_str(), s.calls))
            .collect::<Vec<_>>(),
        [("github.list_issues", 1)]
    );
    let windowed = store
        .tool_stats(&ToolStatsFilter {
            tenant: acme(),
            toolset: None,
            since: Some(at(1_000)),
            until: Some(at(3_000)),
        })
        .await
        .expect("tool_stats");
    assert_eq!(
        windowed
            .iter()
            .map(|s| (s.tool.as_str(), s.calls))
            .collect::<Vec<_>>(),
        [("github.close_issue", 1), ("github.list_issues", 2)],
        "calls scheduled in [since, until)"
    );
}

pub async fn shapes_are_observed_per_tenant_and_tool<S: ReadModel>(store: &S) {
    project(store).await;
    let tools = [
        "github.list_issues".to_owned(),
        "slack.post".to_owned(),
        "unknown".to_owned(),
    ];
    let shapes = store
        .observed_shapes(&acme(), &tools)
        .await
        .expect("observed_shapes");
    let field = |shape: Shape, optional: bool| Field { shape, optional };
    assert_eq!(
        shapes,
        BTreeMap::from([(
            "github.list_issues".to_owned(),
            Shape::Object(BTreeMap::from([
                ("closed".to_owned(), field(Shape::Boolean, true)),
                ("id".to_owned(), field(Shape::Number, false)),
                ("title".to_owned(), field(Shape::String, true)),
            ]))
        )]),
        "results held only by digest, and other tenants' results, contribute nothing"
    );
    let other = store
        .observed_shapes(&tenant("other"), &tools[..1])
        .await
        .expect("observed_shapes");
    assert_eq!(
        other["github.list_issues"],
        Shape::of(&json!({ "secret_of_other": "x" }))
    );
}

/// The same entries applied in another order, some of them twice, give the same projections.
pub async fn projections_ignore_order_and_repetition<S: ReadModel>(ordered: &S, shuffled: &S) {
    project(ordered).await;
    let mut batches: Vec<(ProjectedRun, Vec<HistoryEntry>)> = Vec::new();
    for fixture in fixtures() {
        for chunk in fixture.history.chunks(2) {
            batches.push((fixture.run.clone(), chunk.to_vec()));
        }
    }
    batches.reverse();
    let repeated: Vec<(ProjectedRun, Vec<HistoryEntry>)> =
        batches.iter().step_by(2).cloned().collect();
    for (run, entries) in batches.iter().chain(repeated.iter()) {
        shuffled.apply(run, entries).await.expect("apply");
    }

    for tenant in [acme(), tenant("other")] {
        let query = RunSearch::new(tenant.clone(), 100);
        assert_eq!(
            ordered.search_runs(&query).await.expect("search_runs"),
            shuffled.search_runs(&query).await.expect("search_runs")
        );
        let filter = ScriptFilter::new(tenant.clone());
        assert_eq!(
            ordered.scripts(&filter).await.expect("scripts"),
            shuffled.scripts(&filter).await.expect("scripts")
        );
        let filter = ToolStatsFilter {
            tenant: tenant.clone(),
            toolset: None,
            since: None,
            until: None,
        };
        assert_eq!(
            ordered.tool_stats(&filter).await.expect("tool_stats"),
            shuffled.tool_stats(&filter).await.expect("tool_stats")
        );
        let tools = ["github.list_issues".to_owned()];
        assert_eq!(
            ordered
                .observed_shapes(&tenant, &tools)
                .await
                .expect("observed_shapes"),
            shuffled
                .observed_shapes(&tenant, &tools)
                .await
                .expect("observed_shapes")
        );
    }
}

pub async fn scripts_are_filtered_and_count_labels<S: ReadModel>(store: &S) {
    project(store).await;
    let counts = |scripts: &[crate::ports::ScriptSummary]| {
        scripts
            .iter()
            .map(|s| (s.script, s.runs))
            .collect::<BTreeMap<_, _>>()
    };
    let all = store
        .scripts(&ScriptFilter::new(acme()))
        .await
        .expect("scripts");
    let first = all
        .iter()
        .find(|s| s.script == script(1))
        .expect("script 1");
    assert_eq!(
        first.labels,
        BTreeMap::from([
            ("agent:deploy".to_owned(), 1),
            ("agent:triage".to_owned(), 1)
        ]),
        "each label of its runs, counted"
    );

    let triage = BTreeMap::from([("agent".to_owned(), "triage".to_owned())]);
    let cases: Vec<(ScriptFilter, BTreeMap<Digest, u64>)> = vec![
        (
            ScriptFilter {
                digest: Some(script(2)),
                ..ScriptFilter::new(acme())
            },
            BTreeMap::from([(script(2), 2)]),
        ),
        (
            ScriptFilter {
                labels: triage.clone(),
                ..ScriptFilter::new(acme())
            },
            BTreeMap::from([(script(1), 1), (script(2), 1)]),
        ),
        (
            ScriptFilter {
                tool: Some("github.close_issue".into()),
                ..ScriptFilter::new(acme())
            },
            BTreeMap::from([(script(1), 2)]),
        ),
        (
            ScriptFilter {
                tool: Some("github.close_issue".into()),
                labels: triage,
                ..ScriptFilter::new(acme())
            },
            BTreeMap::new(),
        ),
    ];
    for (filter, expected) in cases {
        let scripts = store.scripts(&filter).await.expect("scripts");
        assert_eq!(counts(&scripts), expected, "{filter:?}");
    }
    let labelled = store
        .scripts(&ScriptFilter {
            labels: BTreeMap::from([("agent".to_owned(), "deploy".to_owned())]),
            ..ScriptFilter::new(acme())
        })
        .await
        .expect("scripts");
    let first = labelled
        .iter()
        .find(|s| s.script == script(1))
        .expect("script 1");
    assert_eq!(
        first.labels,
        BTreeMap::from([("agent:deploy".to_owned(), 1)]),
        "only the counted runs' labels"
    );
}

pub async fn attempts_are_recorded_per_call<S: ReadModel>(store: &S) {
    let run1 = RunRef::new(acme(), id(1));
    let run2 = RunRef::new(acme(), id(2));
    // Recorded before the run is projected: kept for when it is.
    store
        .record_attempt(&run1, Seq(2), 2)
        .await
        .expect("record_attempt");
    project(store).await;
    store
        .record_attempt(&run1, Seq(2), 1)
        .await
        .expect("record_attempt");
    store
        .record_attempt(&run2, Seq(1), 3)
        .await
        .expect("record_attempt");
    store
        .record_attempt(&run2, Seq(1), 3)
        .await
        .expect("record_attempt");
    store
        .record_attempt(&RunRef::new(tenant("other"), id(1)), Seq(1), 4)
        .await
        .expect("record_attempt of another tenant's run");

    assert_eq!(
        store.attempts(&run1).await.expect("attempts"),
        BTreeMap::from([(Seq(2), 2)]),
        "the highest attempt is kept"
    );
    assert_eq!(
        store.attempts(&run2).await.expect("attempts"),
        BTreeMap::from([(Seq(1), 3)])
    );
    assert!(
        store
            .attempts(&RunRef::new(tenant("other"), id(1)))
            .await
            .expect("attempts")
            .is_empty(),
        "another tenant sees nothing"
    );

    let stats = store
        .tool_stats(&ToolStatsFilter {
            tenant: acme(),
            toolset: None,
            since: None,
            until: None,
        })
        .await
        .expect("tool_stats");
    let retries: BTreeMap<String, u64> = stats
        .into_iter()
        .map(|stats| (stats.tool, stats.retries))
        .collect();
    assert_eq!(
        retries,
        BTreeMap::from([
            ("github.close_issue".to_owned(), 0),
            ("github.list_issues".to_owned(), 2),
            ("slack.post".to_owned(), 1),
        ]),
        "attempts beyond the first"
    );

    assert!(store.delete_run(&run1).await.expect("delete_run"));
    assert!(
        store.attempts(&run1).await.expect("attempts").is_empty(),
        "deleted with the run"
    );
}

pub async fn runs_are_purged_and_deleted<S: ReadModel>(store: &S) {
    project(store).await;
    let acme_only = TenantScope::Only(BTreeSet::from([acme()]));
    assert_eq!(
        store.purge(at(300), &acme_only).await.expect("purge"),
        0,
        "ended before, strictly"
    );
    assert_eq!(store.purge(at(1_000), &acme_only).await.expect("purge"), 1);
    assert_eq!(
        search(store, RunSearch::new(acme(), 10)).await,
        [id(4), id(3), id(2)]
    );
    assert_eq!(
        search(store, RunSearch::new(tenant("other"), 10)).await,
        [id(5)],
        "out of scope"
    );
    let except_acme = TenantScope::Except(BTreeSet::from([acme()]));
    assert_eq!(
        store
            .purge(at(1_000_000), &except_acme)
            .await
            .expect("purge"),
        1
    );
    assert!(
        search(store, RunSearch::new(tenant("other"), 10))
            .await
            .is_empty()
    );
    assert_eq!(
        store
            .purge(at(1_000_000), &TenantScope::All)
            .await
            .expect("purge"),
        2,
        "the active run stays"
    );
    assert_eq!(search(store, RunSearch::new(acme(), 10)).await, [id(4)]);

    let intruder = RunRef::new(tenant("other"), id(4));
    assert!(!store.delete_run(&intruder).await.expect("delete_run"));
    let run = RunRef::new(acme(), id(4));
    assert!(store.delete_run(&run).await.expect("delete_run"));
    assert!(!store.delete_run(&run).await.expect("delete_run"));
    assert!(search(store, RunSearch::new(acme(), 10)).await.is_empty());
    let shapes = store
        .observed_shapes(&acme(), &["slack.post".to_owned()])
        .await
        .expect("observed_shapes");
    assert_eq!(shapes, BTreeMap::<String, Shape>::new());
}
