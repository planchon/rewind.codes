//! The conformance suite of `ReadModel` (`archi/05-testing-and-observability.md`, "Read model"
//! and "Query language"): run summaries with their snapshots, the typed query and each
//! predicate's negation, orders and cursors, statistics, failure groups, task states and
//! quarantine, scripts, tool statistics and observed shapes; projected idempotently and in any
//! order; tenants; retention.
//!
//! Most checks project a corpus of runs whose snapshots are written by hand: an adapter stores
//! what it is given, the projector folds histories. Runs are named by the last digits of their
//! id (`#4`), and failures name the query and the runs found.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{self, Excluded, Included, Unbounded};

use rewind_core::{
    AbortKind, AbortReason, Annotation, Callsite, Command, Completion, Digest, Epoch, Event, Field,
    HistoryEntry, Lifecycle, LogLevel, Payload, RunId, RunMode, RunStarted, RunStatus, ScriptError,
    Seed, Seq, Shape, Timestamp, ToolFailure, ToolFailureKind,
};
use serde_json::json;

use super::{T0, check, tenant};
use crate::ports::{
    Ending, FacetValue, Facets, FailureGroup, FailureGroups, FailureWindow, LabelFacet, Latency,
    Predicate, ProjectedRun, QuarantineKind, QuarantineNote, Range, ReadModel, RunOrder, RunQuery,
    RunRef, RunSnapshot, RunStats, RunSummary, ScriptFilter, ScriptSummary, SearchCursor,
    StatsWindow, TaskPhase, TaskProgress, TenantId, TenantScope, TextMatch, ToolStats,
    ToolStatsFilter, Wait, WaitKind, WaitingFor,
};

/// Runs every check, each on new stores.
pub async fn run_all<S: ReadModel>(new_store: impl Fn() -> S) {
    check!(
        "read model",
        new_store(),
        snapshots_keep_the_longest_history
    );
    check!("read model", new_store(), runs_are_summarized);
    check!("read model", new_store(), predicates_and_their_negations);
    check!(
        "read model",
        new_store(),
        predicates_hold_for_runs_without_values
    );
    check!("read model", new_store(), text_and_prefixes_match_literally);
    check!("read model", new_store(), tenants_are_kept_apart);
    check!(
        "read model",
        new_store(),
        orders_and_cursors_page_through_runs
    );
    check!("read model", new_store(), stats_count_the_runs_of_a_query);
    check!(
        "read model",
        new_store(),
        failures_are_grouped_by_fingerprint
    );
    check!("read model", new_store(), task_states_keep_the_furthest);
    check!(
        "read model",
        new_store(),
        quarantine_keeps_the_latest_record
    );
    check!("read model", new_store(), scripts_and_tools_are_aggregated);
    check!(
        "read model",
        new_store(),
        scripts_are_filtered_and_count_labels
    );
    check!("read model", new_store(), attempts_give_retries);
    check!(
        "read model",
        new_store(),
        shapes_are_observed_per_tenant_and_tool
    );
    eprintln!("read model conformance: projections_ignore_order_and_repetition");
    projections_ignore_order_and_repetition(&new_store(), &new_store()).await;
    check!("read model", new_store(), runs_are_purged_and_deleted);
}

// -------------------------------------------------------------------------------------------
// Building runs

fn acme() -> TenantId {
    tenant("acme")
}

fn other() -> TenantId {
    tenant("other")
}

fn id(n: u128) -> RunId {
    RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000 | n)
}

/// The number a run is named by in failure messages.
fn number(id: RunId) -> u128 {
    id.as_uuid().as_u128() & 0xffff
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

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// Builds a history entry by entry, indexes in order.
struct History(Vec<HistoryEntry>);

impl History {
    fn started(ms: u64) -> Self {
        Self::started_in(ms, RunMode::Live)
    }

    fn started_in(ms: u64, mode: RunMode) -> Self {
        let started = RunStarted {
            script: script(0),
            toolset: toolset(0),
            input: json!({}).into(),
            seed: Seed::from_bytes([0; 32]),
            epoch: Epoch(1),
            expires_at: at(1_000_000),
            mode,
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

    fn timer(self, ms: u64, seq: u32, fire_ms: u64) -> Self {
        self.push(
            ms,
            Event::Command(Command::StartTimer {
                seq: Seq(seq),
                duration_ms: fire_ms.saturating_sub(ms),
                fire_at: at(fire_ms),
                callsite: None,
            }),
        )
    }

    fn awaited(self, ms: u64, name: &str, timeout: Option<u32>) -> Self {
        self.push(
            ms,
            Event::Annotation(Annotation::SignalAwaited {
                name: name.into(),
                timeout: timeout.map(Seq),
                callsite: None,
            }),
        )
    }

    fn received(self, ms: u64, name: &str) -> Self {
        self.push(
            ms,
            Event::Completion(Completion::SignalReceived {
                name: name.into(),
                payload: json!(null).into(),
            }),
        )
    }

    fn log(self, ms: u64, message: &str) -> Self {
        self.push(
            ms,
            Event::Annotation(Annotation::Log {
                level: LogLevel::Info,
                message: message.into(),
                callsite: None,
            }),
        )
    }

    fn complete(self, ms: u64) -> Self {
        let output = json!(null).into();
        self.push(ms, Event::Command(Command::CompleteRun { output }))
    }

    fn fail(self, ms: u64, name: &str) -> Self {
        self.fail_with(ms, name, "m")
    }

    fn fail_with(self, ms: u64, name: &str, message: &str) -> Self {
        let error = ScriptError {
            name: name.into(),
            message: message.into(),
            stack: None,
        };
        self.push(ms, Event::Command(Command::FailRun { error }))
    }

    fn abort(self, ms: u64, kind: AbortKind, message: &str) -> Self {
        let reason = AbortReason {
            kind,
            message: message.into(),
        };
        self.push(ms, Event::Lifecycle(Lifecycle::RunAborted(reason)))
    }
}

/// The status the entries give: `active` until a terminal entry.
fn status_of(history: &[HistoryEntry]) -> RunStatus {
    history
        .iter()
        .find_map(|entry| match &entry.event {
            Event::Lifecycle(Lifecycle::RunAborted(_)) => Some(RunStatus::Aborted),
            Event::Command(Command::CompleteRun { .. }) => Some(RunStatus::Completed),
            Event::Command(Command::FailRun { error }) if error.name == ScriptError::CANCELLED => {
                Some(RunStatus::Cancelled)
            }
            Event::Command(Command::FailRun { .. }) => Some(RunStatus::Failed),
            _ => None,
        })
        .unwrap_or(RunStatus::Active)
}

fn wait(kind: WaitKind, seq: Option<u32>, name: Option<&str>, since: u64) -> Wait {
    Wait {
        kind,
        seq: seq.map(Seq),
        name: name.map(str::to_owned),
        since: at(since),
        until: None,
        callsite: None,
        task: None,
    }
}

fn until(wait: Wait, ms: u64) -> Wait {
    Wait {
        until: Some(at(ms)),
        ..wait
    }
}

fn located(wait: Wait, line: u32, column: u32) -> Wait {
    Wait {
        callsite: Some(Callsite { line, column }),
        ..wait
    }
}

fn tasked(wait: Wait, phase: TaskPhase) -> Wait {
    Wait {
        task: Some(phase),
        ..wait
    }
}

fn ending(error: &str, message: &str, line: Option<u32>, signature: &str, fp: &str) -> Ending {
    Ending {
        error: error.into(),
        message: message.into(),
        line,
        signature: signature.into(),
        fingerprint: fp.into(),
    }
}

fn task(seq: u32, attempt: u32, phase: TaskPhase) -> TaskProgress {
    TaskProgress {
        seq: Seq(seq),
        attempt,
        phase,
    }
}

/// The waits of the snapshots before the last: a later snapshot replaces them.
fn stale_waits() -> Vec<Wait> {
    vec![
        until(wait(WaitKind::Timer, Some(999), None, 0), 1),
        wait(WaitKind::Signal, None, Some("stale"), 0),
    ]
}

/// A run, its history and the snapshot of its whole history.
#[derive(Clone)]
struct Fixture {
    run: ProjectedRun,
    history: Vec<HistoryEntry>,
    snapshot: RunSnapshot,
}

impl Fixture {
    fn new(run: ProjectedRun, history: History, mode: RunMode) -> Self {
        let history = history.0;
        let snapshot = RunSnapshot {
            history_len: history.len() as u64,
            status: status_of(&history),
            mode: Some(mode),
            waits: Vec::new(),
            ending: None,
        };
        Self {
            run,
            history,
            snapshot,
        }
    }

    fn waits(mut self, waits: Vec<Wait>) -> Self {
        self.snapshot.waits = waits;
        self
    }

    fn ending(mut self, ending: Ending) -> Self {
        self.snapshot.ending = Some(ending);
        self
    }

    /// The snapshot of the first `len` entries: the last one for the whole history; before,
    /// an active run with waits that must not outlive it.
    fn snapshot_at(&self, len: usize) -> RunSnapshot {
        if len >= self.history.len() {
            return self.snapshot.clone();
        }
        RunSnapshot {
            history_len: len as u64,
            status: RunStatus::Active,
            mode: self.snapshot.mode,
            waits: stale_waits(),
            ending: None,
        }
    }

    /// The batches of `size` entries, each with the snapshot up to its last entry.
    fn batches(&self, size: usize) -> Vec<(ProjectedRun, Vec<HistoryEntry>, RunSnapshot)> {
        let mut batches = Vec::new();
        let mut end = 0;
        for chunk in self.history.chunks(size) {
            end += chunk.len();
            batches.push((self.run.clone(), chunk.to_vec(), self.snapshot_at(end)));
        }
        batches
    }

    /// Applies the history in two batches, in order.
    async fn apply<S: ReadModel>(&self, store: &S) {
        for (run, entries, snapshot) in self.batches(self.history.len().div_ceil(2).max(1)) {
            store.apply(&run, &entries, &snapshot).await.expect("apply");
        }
    }
}

fn run(
    tenant: TenantId,
    n: u128,
    script_n: u8,
    toolset_n: u8,
    headline: &str,
    pairs: &[(&str, &str)],
    parent: Option<RunId>,
) -> ProjectedRun {
    ProjectedRun {
        run: RunRef::new(tenant, id(n)),
        script: script(script_n),
        headline: headline.into(),
        toolset: toolset(toolset_n),
        labels: labels(pairs),
        parent,
    }
}

async fn apply_all<S: ReadModel>(store: &S, fixtures: &[Fixture]) {
    for fixture in fixtures {
        fixture.apply(store).await;
    }
}

// -------------------------------------------------------------------------------------------
// The corpus: nine runs of acme (#1 to #9), two of another tenant (#20, #21)

const FP_TYPE: &str = "d874f3620ac13ff9";
const FP_RATE: &str = "ccc64b2adcd7c61d";
const FP_CANCEL: &str = "c0ffee00c0ffee00";
const FP_LIMIT: &str = "11e1e1e1e1e1e1e1";
const SIG_TYPE: &str = "TypeError: Cannot read properties of undefined (reading 'id')";
const SIG_RATE: &str = "ToolError: github.close_issue failed: rate limit exceeded on <n> requests";
const SIG_CANCEL: &str = "CancelledError: the run was cancelled";
const SIG_LIMIT: &str = "limit_exceeded: the script used <n> ms of CPU time";

const ACME: [u128; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 9];

fn type_error() -> Ending {
    ending(
        "TypeError",
        "Cannot read properties of undefined (reading 'id')",
        Some(7),
        SIG_TYPE,
        FP_TYPE,
    )
}

fn rate_limit(requests: u32) -> Ending {
    ending(
        "ToolError",
        &format!("github.close_issue failed: rate limit exceeded on {requests} requests"),
        Some(3),
        SIG_RATE,
        FP_RATE,
    )
}

fn quarantine_note() -> QuarantineNote {
    QuarantineNote {
        kind: QuarantineKind::NonDeterminism,
        reason: "history entry 3 does not match".into(),
        since: at(4_300),
    }
}

/// #4's waits: a timer, a call parked, a host call waiting for its approval, a signal with a
/// timeout. As the snapshot gives them, before task states.
fn waits_of_4() -> Vec<Wait> {
    vec![
        located(
            until(wait(WaitKind::Timer, Some(3), None, 3_000), 8_000),
            4,
            3,
        ),
        located(
            wait(WaitKind::Call, Some(2), Some("github.get_issue"), 3_000),
            3,
            1,
        ),
        located(wait(WaitKind::Host, Some(1), Some("ask_user"), 3_000), 2, 1),
        located(
            until(
                wait(WaitKind::Signal, Some(4), Some("approval"), 3_100),
                9_000,
            ),
            12,
            5,
        ),
    ]
}

/// #5's waits: a call, a host call deferred, a host call in flight, a signal without timeout.
/// The snapshot's task state is ignored: only records count.
fn waits_of_5() -> Vec<Wait> {
    vec![
        tasked(
            wait(WaitKind::Call, Some(2), Some("github.get_issue"), 4_000),
            TaskPhase::Parked,
        ),
        wait(WaitKind::Host, Some(1), Some("ask_user"), 4_000),
        wait(WaitKind::Host, Some(3), Some("ask_user"), 4_000),
        wait(WaitKind::Signal, None, Some("review"), 4_200),
    ]
}

fn corpus() -> Vec<Fixture> {
    let live = RunMode::Live;
    vec![
        Fixture::new(
            run(
                acme(),
                1,
                1,
                1,
                "// Close the stale issues",
                &[("agent", "triage"), ("customer", "acme-corp")],
                None,
            ),
            History::started(0)
                .call(0, 1, "github.list_issues")
                .result(100, 1, json!({ "id": 1, "title": "x" }).into())
                .call(100, 2, "slack.post")
                .failure(300, 2, ToolFailureKind::Unavailable)
                .complete(300),
            live,
        ),
        Fixture::new(
            run(
                acme(),
                2,
                1,
                1,
                "// Deploy the release",
                &[("agent", "deploy")],
                None,
            ),
            History::started(1_000)
                .call(1_000, 1, "github.list_issues")
                .result(1_400, 1, json!({ "id": 2, "closed": true }).into())
                .call(1_400, 2, "github.close_issue")
                .failure(1_500, 2, ToolFailureKind::Error)
                .fail_with(
                    1_500,
                    "TypeError",
                    "Cannot read properties of undefined (reading 'id')",
                ),
            live,
        )
        .ending(type_error()),
        Fixture::new(
            run(
                acme(),
                3,
                2,
                2,
                "// Refund the order",
                &[("agent", "triage")],
                None,
            ),
            History::started(2_000)
                .call(2_000, 1, "github.list_issues")
                .result(2_200, 1, Payload::Digest(Digest::of(b"large result")))
                .fail_with(2_500, ScriptError::CANCELLED, "the run was cancelled"),
            live,
        )
        .ending(ending(
            ScriptError::CANCELLED,
            "the run was cancelled",
            None,
            SIG_CANCEL,
            FP_CANCEL,
        )),
        Fixture::new(
            run(
                acme(),
                4,
                2,
                1,
                "// Ask before closing",
                &[("agent", "deploy"), ("conversation", "c-42")],
                Some(id(1)),
            ),
            History::started_in(3_000, RunMode::Mock)
                .call(3_000, 1, "ask_user")
                .call(3_000, 2, "github.get_issue")
                .timer(3_000, 3, 8_000)
                .timer(3_100, 4, 9_000)
                .awaited(3_100, "approval", Some(4)),
            RunMode::Mock,
        )
        .waits(waits_of_4()),
        Fixture::new(
            run(
                acme(),
                5,
                3,
                1,
                "// Review the pull request",
                &[("agent", "review"), ("customer", "globex")],
                Some(id(2)),
            ),
            History::started(4_000)
                .call(4_000, 1, "ask_user")
                .call(4_000, 2, "github.get_issue")
                .call(4_000, 3, "ask_user")
                .awaited(4_200, "review", None),
            live,
        )
        .waits(waits_of_5()),
        Fixture::new(
            run(acme(), 6, 3, 2, "while (true) {}", &[], None),
            History::started(5_000).abort(
                5_100,
                AbortKind::LimitExceeded,
                "the script used 5000 ms of CPU time",
            ),
            live,
        )
        .ending(ending(
            "limit_exceeded",
            "the script used 5000 ms of CPU time",
            None,
            SIG_LIMIT,
            FP_LIMIT,
        )),
        Fixture::new(
            run(acme(), 7, 1, 1, "", &[], None),
            History::started(6_000).log(6_050, "warming up"),
            live,
        ),
        Fixture::new(
            run(
                acme(),
                8,
                4,
                1,
                "// Close the duplicates",
                &[("agent", "triage"), ("customer", "acme-corp")],
                None,
            ),
            History::started(7_000)
                .call(7_000, 1, "github.close_issue")
                .failure(7_100, 1, ToolFailureKind::Error)
                .fail_with(
                    7_200,
                    "ToolError",
                    "github.close_issue failed: rate limit exceeded on 42 requests",
                ),
            live,
        )
        .ending(rate_limit(42)),
        Fixture::new(
            run(
                acme(),
                9,
                5,
                1,
                "// Close the duplicates, again",
                &[("agent", "triage")],
                None,
            ),
            History::started(8_000)
                .call(8_000, 1, "github.close_issue")
                .failure(8_050, 1, ToolFailureKind::Timeout)
                .fail_with(
                    8_100,
                    "ToolError",
                    "github.close_issue failed: rate limit exceeded on 7 requests",
                ),
            live,
        )
        .ending(rate_limit(7)),
        // Another tenant's runs, alike in every way a query reads.
        Fixture::new(
            run(
                other(),
                20,
                1,
                1,
                "// Close the stale issues",
                &[("agent", "triage"), ("customer", "acme-corp")],
                None,
            ),
            History::started(500)
                .call(500, 1, "github.close_issue")
                .failure(600, 1, ToolFailureKind::Unavailable)
                .fail_with(
                    700,
                    "TypeError",
                    "Cannot read properties of undefined (reading 'id')",
                ),
            live,
        )
        .ending(type_error()),
        Fixture::new(
            run(
                other(),
                21,
                2,
                1,
                "// Ask before closing",
                &[("agent", "deploy")],
                Some(id(1)),
            ),
            History::started_in(3_500, RunMode::Mock)
                .call(3_500, 1, "ask_user")
                .awaited(3_600, "approval", None),
            RunMode::Mock,
        )
        .waits(vec![
            wait(WaitKind::Host, Some(1), Some("ask_user"), 3_500),
            wait(WaitKind::Signal, None, Some("approval"), 3_600),
        ]),
    ]
}

/// The task states of the corpus, as whoever changed them recorded them, in order.
fn corpus_tasks() -> Vec<(RunRef, Vec<TaskProgress>)> {
    let acme_run = |n| RunRef::new(acme(), id(n));
    vec![
        (acme_run(4), vec![task(1, 0, TaskPhase::Approval)]),
        (
            acme_run(4),
            vec![
                task(2, 0, TaskPhase::Ready),
                task(1, 0, TaskPhase::Approval),
            ],
        ),
        (acme_run(4), vec![task(2, 1, TaskPhase::Claimed)]),
        (acme_run(4), vec![task(2, 1, TaskPhase::Parked)]),
        (
            acme_run(5),
            vec![task(1, 0, TaskPhase::Ready), task(3, 0, TaskPhase::Ready)],
        ),
        (acme_run(5), vec![task(1, 1, TaskPhase::Claimed)]),
        (acme_run(5), vec![task(3, 1, TaskPhase::Claimed)]),
        (acme_run(5), vec![task(1, 1, TaskPhase::Deferred)]),
        (
            RunRef::new(other(), id(21)),
            vec![task(1, 0, TaskPhase::Approval)],
        ),
    ]
}

/// The quarantines of the corpus: #5 and #21 are quarantined; #5 was released once before.
fn corpus_quarantines() -> Vec<(RunRef, Option<QuarantineNote>, Timestamp)> {
    vec![
        (RunRef::new(acme(), id(5)), None, at(4_250)),
        (
            RunRef::new(acme(), id(5)),
            Some(quarantine_note()),
            at(4_300),
        ),
        (
            RunRef::new(other(), id(21)),
            Some(QuarantineNote {
                kind: QuarantineKind::Failures,
                reason: "5 activations failed".into(),
                since: at(3_700),
            }),
            at(3_700),
        ),
    ]
}

async fn record_corpus_state<S: ReadModel>(store: &S, reversed: bool) {
    let mut tasks = corpus_tasks();
    let mut quarantines = corpus_quarantines();
    if reversed {
        tasks.reverse();
        quarantines.reverse();
    }
    for (run, tasks) in tasks {
        store
            .record_tasks(&run, &tasks)
            .await
            .expect("record_tasks");
    }
    for (run, note, when) in quarantines {
        store
            .record_quarantine(&run, note.as_ref(), when)
            .await
            .expect("record_quarantine");
    }
}

/// Projects the corpus, each run in two batches, then records its service state.
async fn project_corpus<S: ReadModel>(store: &S) {
    apply_all(store, &corpus()).await;
    record_corpus_state(store, false).await;
}

// -------------------------------------------------------------------------------------------
// Queries

async fn page<S: ReadModel>(
    store: &S,
    query: &RunQuery,
    order: RunOrder,
    after: Option<&SearchCursor>,
    limit: usize,
) -> Vec<RunSummary> {
    store
        .search_runs(query, order, after, limit)
        .await
        .expect("search_runs")
}

/// The runs a query matches, by number, newest run id first.
async fn ids<S: ReadModel>(store: &S, query: &RunQuery) -> Vec<u128> {
    page(store, query, RunOrder::Started, None, 1_000)
        .await
        .iter()
        .map(|summary| number(summary.run.id))
        .collect()
}

async fn summary<S: ReadModel>(store: &S, tenant: TenantId, n: u128) -> RunSummary {
    let query = RunQuery::new(tenant).with(Predicate::Id(vec![id(n).to_string()]));
    let mut found = page(store, &query, RunOrder::Started, None, 10).await;
    assert_eq!(found.len(), 1, "run #{n} is projected");
    found.remove(0)
}

/// Checks each predicate, and its negation: exactly the other runs of the tenant.
async fn check_terms<S: ReadModel>(
    store: &S,
    tenant: &TenantId,
    all: &[u128],
    cases: Vec<(Predicate, Vec<u128>)>,
) {
    let all: BTreeSet<u128> = all.iter().copied().collect();
    for (predicate, expected) in cases {
        let expected: BTreeSet<u128> = expected.into_iter().collect();
        let query = RunQuery::new(tenant.clone()).with(predicate.clone());
        let found: BTreeSet<u128> = ids(store, &query).await.into_iter().collect();
        assert_eq!(
            found, expected,
            "the runs of {tenant} matching {predicate:?}"
        );
        let query = RunQuery::new(tenant.clone()).without(predicate.clone());
        let found: BTreeSet<u128> = ids(store, &query).await.into_iter().collect();
        assert_eq!(
            found,
            &all - &expected,
            "the runs of {tenant} not matching {predicate:?}: every other run"
        );
    }
}

fn exact(value: &str) -> TextMatch {
    TextMatch::Exact(value.into())
}

fn prefix(value: &str) -> TextMatch {
    TextMatch::Prefix(value.into())
}

fn label(key: &str, values: Vec<TextMatch>) -> Predicate {
    Predicate::Label {
        key: key.into(),
        values,
    }
}

fn errors(names: &[&str], kinds: &[ToolFailureKind]) -> Predicate {
    Predicate::Error {
        names: names.iter().map(|name| (*name).to_owned()).collect(),
        kinds: kinds.iter().copied().collect(),
    }
}

fn waiting(kinds: &[WaitingFor]) -> Predicate {
    Predicate::Waiting(kinds.iter().copied().collect())
}

fn statuses(statuses: &[RunStatus]) -> Predicate {
    Predicate::Status(statuses.iter().copied().collect())
}

fn text(value: &str) -> Predicate {
    Predicate::Text(value.into())
}

fn span(start: Bound<u64>, end: Bound<u64>) -> Range<u64> {
    Range { start, end }
}

fn period(start: Bound<u64>, end: Bound<u64>) -> Range<Timestamp> {
    Range {
        start: start.map(at),
        end: end.map(at),
    }
}

fn hex(digest: Digest, digits: usize) -> String {
    digest.to_hex()[..digits].to_owned()
}

// -------------------------------------------------------------------------------------------
// Checks: snapshots and summaries

/// Of a run's snapshots, the one of the longest history is kept, whatever the order and the
/// repetitions; its waits replace the previous ones; the task state a snapshot gives is ignored.
pub async fn snapshots_keep_the_longest_history<S: ReadModel>(store: &S) {
    let projected = run(acme(), 1, 1, 1, "// Wait for go", &[], None);
    let history = History::started_in(0, RunMode::Mock)
        .call(0, 1, "ask_user")
        .result(100, 1, json!({ "ok": true }).into())
        .timer(100, 2, 5_000)
        .awaited(200, "go", None)
        .log(300, "waiting")
        .received(400, "go")
        .complete(500)
        .0;
    let snapshot = |len: usize, waits: Vec<Wait>| RunSnapshot {
        history_len: len as u64,
        status: status_of(&history[..len]),
        mode: Some(RunMode::Mock),
        waits,
        ending: None,
    };
    let early = snapshot(
        2,
        vec![tasked(
            wait(WaitKind::Host, Some(1), Some("ask_user"), 0),
            TaskPhase::Approval,
        )],
    );
    let waiting_waits = vec![
        until(wait(WaitKind::Timer, Some(2), None, 100), 5_000),
        wait(WaitKind::Signal, None, Some("go"), 200),
    ];
    let paused = snapshot(6, waiting_waits.clone());
    let ended = snapshot(8, Vec::new());
    let apply = |entries: &[HistoryEntry], snapshot: &RunSnapshot| {
        let (projected, entries, snapshot) =
            (projected.clone(), entries.to_vec(), snapshot.clone());
        async move {
            store
                .apply(&projected, &entries, &snapshot)
                .await
                .expect("apply");
        }
    };

    apply(&history[..2], &early).await;
    let first = summary(store, acme(), 1).await;
    assert_eq!(
        first.waits,
        [wait(WaitKind::Host, Some(1), Some("ask_user"), 0)],
        "the snapshot's waits, without the task state it gives: only records count"
    );
    assert_eq!((first.history_len, first.wakes_at), (2, None));

    apply(&history[2..6], &paused).await;
    apply(&history[..2], &early).await;
    apply(&history[2..6], &paused).await;
    let middle = summary(store, acme(), 1).await;
    assert_eq!(
        middle.waits, waiting_waits,
        "the longest snapshot's waits, whatever arrived after it"
    );
    assert_eq!(middle.wakes_at, Some(at(5_000)));
    assert_eq!(middle.status, RunStatus::Active);
    assert_eq!(middle.mode, Some(RunMode::Mock));
    assert_eq!(middle.history_len, 6);
    assert_eq!(middle.last_activity_at, at(300));

    apply(&history[6..], &ended).await;
    apply(&history[2..6], &paused).await;
    apply(&history[..2], &early).await;
    let last = summary(store, acme(), 1).await;
    assert_eq!(
        (last.status, last.ended_at, last.history_len),
        (RunStatus::Completed, Some(at(500)), 8)
    );
    assert!(
        last.waits.is_empty() && last.wakes_at.is_none(),
        "an ended run waits for nothing: {:?}",
        last.waits
    );
    let waiting_runs = RunQuery::new(acme()).with(waiting(&[]));
    assert!(ids(store, &waiting_runs).await.is_empty());
    let signals = RunQuery::new(acme()).with(Predicate::Signal(vec![TextMatch::Any]));
    assert!(ids(store, &signals).await.is_empty());

    // Out of order from the start, in another run.
    let reversed = ProjectedRun {
        run: RunRef::new(acme(), id(2)),
        ..projected.clone()
    };
    for (entries, snapshot) in [
        (&history[6..], &ended),
        (&history[..2], &early),
        (&history[2..6], &paused),
        (&history[6..], &ended),
    ] {
        store
            .apply(&reversed, entries, snapshot)
            .await
            .expect("apply");
    }
    let mut again = summary(store, acme(), 2).await;
    again.run = last.run.clone();
    assert_eq!(again, last, "the same summary, whatever the order");
}

/// Every field of a summary: the run's metadata, what its entries count, its snapshot, its
/// tasks' states and its quarantine.
pub async fn runs_are_summarized<S: ReadModel>(store: &S) {
    project_corpus(store).await;
    assert_eq!(
        summary(store, acme(), 4).await,
        RunSummary {
            run: RunRef::new(acme(), id(4)),
            script: script(2),
            headline: "// Ask before closing".into(),
            toolset: toolset(1),
            status: RunStatus::Active,
            mode: Some(RunMode::Mock),
            labels: labels(&[("agent", "deploy"), ("conversation", "c-42")]),
            parent: Some(id(1)),
            started_at: Some(at(3_000)),
            ended_at: None,
            last_activity_at: at(3_100),
            tool_calls: 2,
            errors: 0,
            history_len: 6,
            waits: vec![
                waits_of_4()[0].clone(),
                tasked(waits_of_4()[1].clone(), TaskPhase::Parked),
                tasked(waits_of_4()[2].clone(), TaskPhase::Approval),
                waits_of_4()[3].clone(),
            ],
            wakes_at: Some(at(8_000)),
            ending: None,
            quarantine: None,
        },
        "#4: its waits in the snapshot's order, each call's with its task's furthest state"
    );
    let five = summary(store, acme(), 5).await;
    assert_eq!(
        five.waits,
        [
            wait(WaitKind::Call, Some(2), Some("github.get_issue"), 4_000),
            tasked(
                wait(WaitKind::Host, Some(1), Some("ask_user"), 4_000),
                TaskPhase::Deferred
            ),
            tasked(
                wait(WaitKind::Host, Some(3), Some("ask_user"), 4_000),
                TaskPhase::Claimed
            ),
            wait(WaitKind::Signal, None, Some("review"), 4_200),
        ],
        "#5: a call without a record has no task state"
    );
    assert_eq!(five.wakes_at, None, "no wait of #5 ends by itself");
    assert_eq!(five.quarantine, Some(quarantine_note()));
    assert_eq!(five.parent, Some(id(2)));
    assert_eq!(
        summary(store, acme(), 2).await,
        RunSummary {
            run: RunRef::new(acme(), id(2)),
            script: script(1),
            headline: "// Deploy the release".into(),
            toolset: toolset(1),
            status: RunStatus::Failed,
            mode: Some(RunMode::Live),
            labels: labels(&[("agent", "deploy")]),
            parent: None,
            started_at: Some(at(1_000)),
            ended_at: Some(at(1_500)),
            last_activity_at: at(1_500),
            tool_calls: 2,
            errors: 1,
            history_len: 6,
            waits: Vec::new(),
            wakes_at: None,
            ending: Some(type_error()),
            quarantine: None,
        },
        "#2: failed, with its ending"
    );
    let seven = summary(store, acme(), 7).await;
    assert_eq!(
        (
            seven.status,
            seven.headline.as_str(),
            seven.labels.len(),
            seven.waits.len()
        ),
        (RunStatus::Active, "", 0, 0),
        "#7: active, running, without headline or labels"
    );
    assert_eq!((seven.history_len, seven.last_activity_at), (2, at(6_050)));
    let statuses: Vec<(u128, RunStatus)> =
        page(store, &RunQuery::new(acme()), RunOrder::Started, None, 100)
            .await
            .iter()
            .map(|summary| (number(summary.run.id), summary.status))
            .collect();
    assert_eq!(
        statuses,
        [
            (9, RunStatus::Failed),
            (8, RunStatus::Failed),
            (7, RunStatus::Active),
            (6, RunStatus::Aborted),
            (5, RunStatus::Active),
            (4, RunStatus::Active),
            (3, RunStatus::Cancelled),
            (2, RunStatus::Failed),
            (1, RunStatus::Completed),
        ]
    );
}

// -------------------------------------------------------------------------------------------
// Checks: the query

/// Every predicate, alone and negated, on the corpus.
pub async fn predicates_and_their_negations<S: ReadModel>(store: &S) {
    project_corpus(store).await;
    let scripts: Vec<String> = (1..=5).map(|n| hex(script(n), 8)).collect();
    let distinct: BTreeSet<&String> = scripts.iter().collect();
    assert_eq!(
        distinct.len(),
        5,
        "the suite's scripts have distinct prefixes"
    );
    assert_ne!(hex(toolset(1), 8), hex(toolset(2), 8));
    let cases = vec![
        // Ids.
        (Predicate::Id(vec![id(4).to_string()]), vec![4]),
        (
            Predicate::Id(vec!["01930000-0000-7000-8000-00000000000".into()]),
            ACME.to_vec(),
        ),
        (
            Predicate::Id(vec![id(4).to_string(), id(7).to_string()]),
            vec![4, 7],
        ),
        (Predicate::Id(vec![id(20).to_string()]), vec![]),
        // Statuses.
        (
            statuses(&[RunStatus::Failed, RunStatus::Aborted]),
            vec![2, 6, 8, 9],
        ),
        (statuses(&[RunStatus::Active]), vec![4, 5, 7]),
        (
            statuses(&[RunStatus::Completed, RunStatus::Cancelled]),
            vec![1, 3],
        ),
        (statuses(&[RunStatus::Pending]), vec![]),
        (statuses(&[]), vec![]),
        // Labels.
        (label("agent", vec![exact("triage")]), vec![1, 3, 8, 9]),
        (label("agent", vec![exact("Triage")]), vec![]),
        (label("agent", vec![prefix("de")]), vec![2, 4]),
        (
            label("agent", vec![exact("review"), exact("deploy")]),
            vec![2, 4, 5],
        ),
        (label("agent", vec![prefix("")]), vec![1, 2, 3, 4, 5, 8, 9]),
        (label("customer", vec![TextMatch::Any]), vec![1, 5, 8]),
        (label("customer", vec![prefix("acme")]), vec![1, 8]),
        (label("conversation", vec![TextMatch::Any]), vec![4]),
        (label("missing", vec![TextMatch::Any]), vec![]),
        (label("agent", vec![]), vec![]),
        // Scripts and toolsets, by prefix.
        (Predicate::Script(vec![scripts[0].clone()]), vec![1, 2, 7]),
        (
            Predicate::Script(vec![script(2).to_hex(), hex(script(5), 12)]),
            vec![3, 4, 9],
        ),
        (Predicate::Script(vec!["not a digest".into()]), vec![]),
        (Predicate::Toolset(vec![hex(toolset(2), 8)]), vec![3, 6]),
        (
            Predicate::Toolset(vec![toolset(1).to_hex()]),
            vec![1, 2, 4, 5, 7, 8, 9],
        ),
        // Tools.
        (
            Predicate::Tool(vec![exact("github.close_issue")]),
            vec![2, 8, 9],
        ),
        (
            Predicate::Tool(vec![prefix("github.")]),
            vec![1, 2, 3, 4, 5, 8, 9],
        ),
        (Predicate::Tool(vec![prefix("slack")]), vec![1]),
        (Predicate::Tool(vec![exact("ask_user")]), vec![4, 5]),
        (
            Predicate::Tool(vec![exact("slack.post"), exact("github.get_issue")]),
            vec![1, 4, 5],
        ),
        (
            Predicate::Tool(vec![TextMatch::Any]),
            vec![1, 2, 3, 4, 5, 8, 9],
        ),
        (Predicate::Tool(vec![exact("github")]), vec![]),
        // Errors: an ending's name, or a failed call's kind.
        (errors(&["TypeError"], &[]), vec![2]),
        (errors(&["limit_exceeded"], &[]), vec![6]),
        (errors(&[ScriptError::CANCELLED], &[]), vec![3]),
        (errors(&[], &[ToolFailureKind::Unavailable]), vec![1]),
        (errors(&[], &[ToolFailureKind::Timeout]), vec![9]),
        (errors(&[], &[ToolFailureKind::Error]), vec![2, 8]),
        (
            errors(&["ToolError"], &[ToolFailureKind::Unavailable]),
            vec![1, 8, 9],
        ),
        (errors(&["unavailable", "error"], &[]), vec![]),
        (errors(&[], &[]), vec![]),
        // Waits: approval, parked and deferred come from the task states.
        (waiting(&[]), vec![4, 5]),
        (waiting(&[WaitingFor::Signal]), vec![4, 5]),
        (waiting(&[WaitingFor::Timer]), vec![4]),
        (waiting(&[WaitingFor::Call]), vec![5]),
        (waiting(&[WaitingFor::Host]), vec![5]),
        (waiting(&[WaitingFor::Approval]), vec![4]),
        (waiting(&[WaitingFor::Parked]), vec![4]),
        (waiting(&[WaitingFor::Deferred]), vec![5]),
        (
            waiting(&[WaitingFor::Approval, WaitingFor::Deferred]),
            vec![4, 5],
        ),
        // Signals.
        (Predicate::Signal(vec![exact("approval")]), vec![4]),
        (Predicate::Signal(vec![prefix("re")]), vec![5]),
        (Predicate::Signal(vec![TextMatch::Any]), vec![4, 5]),
        (Predicate::Signal(vec![exact("stale")]), vec![]),
        // Times.
        (
            Predicate::Started(period(Included(1_000), Excluded(3_000))),
            vec![2, 3],
        ),
        (
            Predicate::Started(period(Excluded(1_000), Included(3_000))),
            vec![3, 4],
        ),
        (Predicate::Started(period(Unbounded, Excluded(0))), vec![]),
        (Predicate::Started(period(Unbounded, Included(0))), vec![1]),
        (
            Predicate::Started(period(Included(8_000), Unbounded)),
            vec![9],
        ),
        (
            Predicate::Started(period(Unbounded, Unbounded)),
            ACME.to_vec(),
        ),
        (
            Predicate::Ended(period(Included(1_500), Included(5_100))),
            vec![2, 3, 6],
        ),
        (
            Predicate::Ended(period(Excluded(1_500), Excluded(5_100))),
            vec![3],
        ),
        (
            Predicate::Ended(period(Unbounded, Unbounded)),
            vec![1, 2, 3, 6, 8, 9],
        ),
        (
            Predicate::Activity(period(Excluded(3_100), Included(6_050))),
            vec![5, 6, 7],
        ),
        (
            Predicate::Activity(period(Included(3_100), Excluded(4_200))),
            vec![4],
        ),
        // Durations and counts.
        (
            Predicate::Duration(span(Included(200), Excluded(500))),
            vec![1, 8],
        ),
        (
            Predicate::Duration(span(Included(500), Included(500))),
            vec![2, 3],
        ),
        (
            Predicate::Duration(span(Excluded(100), Unbounded)),
            vec![1, 2, 3, 8],
        ),
        (
            Predicate::Duration(span(Unbounded, Unbounded)),
            vec![1, 2, 3, 6, 8, 9],
        ),
        (
            Predicate::Calls(span(Included(2), Unbounded)),
            vec![1, 2, 4, 5],
        ),
        (Predicate::Calls(span(Unbounded, Excluded(1))), vec![6, 7]),
        (
            Predicate::Calls(span(Excluded(1), Excluded(3))),
            vec![1, 2, 4],
        ),
        (
            Predicate::Errors(span(Excluded(0), Unbounded)),
            vec![1, 2, 8, 9],
        ),
        (
            Predicate::Errors(span(Included(0), Included(0))),
            vec![3, 4, 5, 6, 7],
        ),
        // Forks, failure groups, quarantine, mode.
        (Predicate::Parent(id(1)), vec![4]),
        (Predicate::Parent(id(2)), vec![5]),
        (Predicate::Parent(id(9)), vec![]),
        (Predicate::Fingerprint(vec![FP_RATE.into()]), vec![8, 9]),
        (
            Predicate::Fingerprint(vec![FP_TYPE.into(), FP_LIMIT.into()]),
            vec![2, 6],
        ),
        (Predicate::Fingerprint(vec![FP_CANCEL.into()]), vec![3]),
        (Predicate::Quarantined, vec![5]),
        (Predicate::Mode(vec![RunMode::Mock]), vec![4]),
        (
            Predicate::Mode(vec![RunMode::Live]),
            vec![1, 2, 3, 5, 6, 7, 8, 9],
        ),
        (
            Predicate::Mode(vec![RunMode::Live, RunMode::Mock]),
            ACME.to_vec(),
        ),
        (Predicate::Mode(vec![]), vec![]),
        // Text: the ending's error and message, the headline, label values, the run's id.
        (text("close the"), vec![1, 8, 9]),
        (text("reading 'id'"), vec![2]),
        (text("cannot read"), vec![2]),
        (text("rate limit"), vec![8, 9]),
        (text("typeerror"), vec![2]),
        (text("cancellederror"), vec![3]),
        (text("limit_exceeded"), vec![6]),
        (text("ms of cpu"), vec![6]),
        (text("acme-corp"), vec![1, 8]),
        (text("globex"), vec![5]),
        (text("c-42"), vec![4]),
        (text("-000000000004"), vec![4]),
        (text("while (true)"), vec![6]),
        (text("slack.post"), vec![]),
        (text("conversation"), vec![]),
    ];
    check_terms(store, &acme(), &ACME, cases).await;

    let combined: Vec<(RunQuery, Vec<u128>)> = vec![
        (RunQuery::new(acme()), vec![9, 8, 7, 6, 5, 4, 3, 2, 1]),
        (
            RunQuery::new(acme())
                .with(label("agent", vec![exact("triage")]))
                .without(statuses(&[RunStatus::Completed])),
            vec![9, 8, 3],
        ),
        (
            RunQuery::new(acme())
                .with(Predicate::Tool(vec![prefix("github.")]))
                .with(waiting(&[])),
            vec![5, 4],
        ),
        (
            RunQuery::new(acme())
                .without(waiting(&[]))
                .without(statuses(&[RunStatus::Failed])),
            vec![7, 6, 3, 1],
        ),
        (
            RunQuery::new(acme())
                .with(Predicate::Quarantined)
                .without(Predicate::Quarantined),
            vec![],
        ),
    ];
    for (query, expected) in combined {
        assert_eq!(
            ids(store, &query).await,
            expected,
            "the runs matching {query:?}"
        );
    }
}

/// A run without the value a predicate reads does not match it, so it matches its negation:
/// a run whose `run_started` is not projected yet, without mode, labels, waits or ending.
pub async fn predicates_hold_for_runs_without_values<S: ReadModel>(store: &S) {
    let history = History::started(0).call(10, 1, "github.list_issues").0;
    let bare = run(acme(), 1, 1, 1, "", &[], None);
    let snapshot = RunSnapshot {
        history_len: 2,
        status: RunStatus::Active,
        mode: None,
        waits: Vec::new(),
        ending: None,
    };
    store
        .apply(&bare, &history[1..], &snapshot)
        .await
        .expect("apply");
    Fixture::new(
        run(acme(), 2, 1, 1, "// Done", &[("agent", "x")], None),
        History::started(0).complete(100),
        RunMode::Live,
    )
    .apply(store)
    .await;

    let one = summary(store, acme(), 1).await;
    assert_eq!(
        (one.started_at, one.ended_at, one.mode, one.status),
        (None, None, None, RunStatus::Active),
        "#1: neither started nor ended, without mode"
    );
    assert_eq!(
        (one.tool_calls, one.history_len, one.last_activity_at),
        (1, 2, at(10))
    );
    let cases = vec![
        (Predicate::Started(period(Unbounded, Unbounded)), vec![2]),
        (Predicate::Ended(period(Unbounded, Unbounded)), vec![2]),
        (Predicate::Duration(span(Unbounded, Unbounded)), vec![2]),
        (Predicate::Mode(vec![RunMode::Live, RunMode::Mock]), vec![2]),
        (label("agent", vec![TextMatch::Any]), vec![2]),
        (Predicate::Signal(vec![TextMatch::Any]), vec![]),
        (waiting(&[]), vec![]),
        (Predicate::Fingerprint(vec![FP_TYPE.into()]), vec![]),
        (Predicate::Parent(id(2)), vec![]),
        (
            errors(
                &["TypeError"],
                &[ToolFailureKind::Error, ToolFailureKind::Timeout],
            ),
            vec![],
        ),
        (Predicate::Quarantined, vec![]),
        (text("done"), vec![2]),
        (Predicate::Calls(span(Included(1), Unbounded)), vec![1]),
        (statuses(&[RunStatus::Active]), vec![1]),
    ];
    check_terms(store, &acme(), &[1, 2], cases).await;
}

/// Free text and prefixes match what they say: `%`, `_` and `\` are not patterns, text is not
/// matched across two fields, and case does not matter in text.
pub async fn text_and_prefixes_match_literally<S: ReadModel>(store: &S) {
    let live = RunMode::Live;
    let fixtures = vec![
        Fixture::new(
            run(
                acme(),
                1,
                1,
                1,
                "Refund 100% of it",
                &[("note", "a_b")],
                None,
            ),
            History::started(0).call(0, 1, "git_hub.list").complete(10),
            live,
        ),
        Fixture::new(
            run(
                acme(),
                2,
                1,
                1,
                "Refund 1000 of it",
                &[("note", "aXb")],
                None,
            ),
            History::started(100)
                .call(100, 1, "gitXhub.list")
                .complete(110),
            live,
        ),
        Fixture::new(
            run(acme(), 3, 1, 1, "Cancel the order", &[], None),
            History::started(200).fail_with(210, "TimeoutError", "Timed out"),
            live,
        )
        .ending(ending(
            "TimeoutError",
            "Timed out",
            None,
            "TimeoutError: Timed out",
            "3333333333333333",
        )),
        Fixture::new(
            run(
                acme(),
                4,
                1,
                1,
                r"Copy C:\temp\x",
                &[("note", r"a\b")],
                None,
            ),
            History::started(300).awaited(310, "reXview", None),
            live,
        )
        .waits(vec![wait(WaitKind::Signal, None, Some("reXview"), 310)]),
        Fixture::new(
            run(
                acme(),
                5,
                1,
                1,
                "plain",
                &[("note", "triage"), ("other", "deploy")],
                None,
            ),
            History::started(400).awaited(410, "re_view", None),
            live,
        )
        .waits(vec![wait(WaitKind::Signal, None, Some("re_view"), 410)]),
    ];
    apply_all(store, &fixtures).await;
    let cases = vec![
        (text("100%"), vec![1]),
        (text("%"), vec![1]),
        (text("0%"), vec![1]),
        (text("a_b"), vec![1]),
        (text("_"), vec![1]),
        (text(r"\"), vec![4]),
        (text(r"c:\temp"), vec![4]),
        (text(r"\t"), vec![4]),
        (text(r"a\b"), vec![4]),
        (text("cancel the"), vec![3]),
        (text("timed out"), vec![3]),
        (text("timeouterror"), vec![3]),
        (text("order timed"), vec![]),
        (text("out cancel"), vec![]),
        (text("triage deploy"), vec![]),
        (text("deploy triage"), vec![]),
        (text("triagedeploy"), vec![]),
        (text("triage"), vec![5]),
        (label("note", vec![prefix("a_")]), vec![1]),
        (label("note", vec![prefix(r"a\")]), vec![4]),
        (label("note", vec![prefix("%")]), vec![]),
        (label("note", vec![exact("a_b")]), vec![1]),
        (label("note", vec![exact("a%")]), vec![]),
        (Predicate::Tool(vec![prefix("git_")]), vec![1]),
        (Predicate::Tool(vec![prefix("git%")]), vec![]),
        (Predicate::Signal(vec![prefix("re_")]), vec![5]),
        (Predicate::Signal(vec![prefix("re%")]), vec![]),
        (Predicate::Signal(vec![exact("re_view")]), vec![5]),
    ];
    check_terms(store, &acme(), &[1, 2, 3, 4, 5], cases).await;
}

/// Every query is limited to its tenant: runs, statistics, failure groups, attempts.
pub async fn tenants_are_kept_apart<S: ReadModel>(store: &S) {
    project_corpus(store).await;
    let cases = vec![
        (label("agent", vec![exact("triage")]), vec![20]),
        (text("reading 'id'"), vec![20]),
        (Predicate::Fingerprint(vec![FP_TYPE.into()]), vec![20]),
        (waiting(&[]), vec![21]),
        (waiting(&[WaitingFor::Approval]), vec![21]),
        (Predicate::Quarantined, vec![21]),
        (Predicate::Parent(id(1)), vec![21]),
        (Predicate::Mode(vec![RunMode::Mock]), vec![21]),
        (Predicate::Tool(vec![exact("github.close_issue")]), vec![20]),
        (Predicate::Id(vec![id(4).to_string()]), vec![]),
    ];
    check_terms(store, &other(), &[20, 21], cases).await;
    let nobody = tenant("nobody");
    assert!(ids(store, &RunQuery::new(nobody.clone())).await.is_empty());
    assert!(
        ids(
            store,
            &RunQuery::new(nobody.clone()).without(Predicate::Quarantined)
        )
        .await
        .is_empty(),
        "a negation is still limited to the tenant"
    );

    let window = StatsWindow {
        since: None,
        until: None,
        bucket_ms: None,
        top: 10,
    };
    let stats = store
        .run_stats(&RunQuery::new(other()), &window)
        .await
        .expect("run_stats");
    assert_eq!(stats.runs, 2);
    assert_eq!(stats.quarantined, 1);
    assert_eq!(stats.waiting[&WaitingFor::Approval], 1);
    assert_eq!(stats.waiting[&WaitingFor::Parked], 0);
    let acme_stats = store
        .run_stats(&RunQuery::new(acme()), &window)
        .await
        .expect("run_stats");
    assert_eq!((acme_stats.runs, acme_stats.quarantined), (9, 1));
    let empty = store
        .run_stats(&RunQuery::new(nobody.clone()), &window)
        .await
        .expect("run_stats");
    assert_eq!((empty.runs, empty.durations), (0, None));

    let groups = store
        .failure_groups(
            &RunQuery::new(acme()),
            &FailureWindow {
                since: None,
                until: None,
                bucket_ms: None,
                limit: 10,
            },
        )
        .await
        .expect("failure_groups");
    let counts: Vec<(&str, u64)> = groups
        .top
        .iter()
        .map(|group| (group.fingerprint.as_str(), group.runs))
        .collect();
    assert_eq!(
        counts,
        [(FP_RATE, 2), (FP_LIMIT, 1), (FP_CANCEL, 1), (FP_TYPE, 1)],
        "acme's groups only: #20 is another tenant's"
    );
    assert!(
        store
            .attempts(&RunRef::new(other(), id(4)))
            .await
            .expect("attempts")
            .is_empty(),
        "another tenant sees no attempts"
    );
}

// -------------------------------------------------------------------------------------------
// Checks: orders and cursors

/// Six active runs with ties on activity and on wakes.
fn ordered_runs() -> Vec<Fixture> {
    let fixture = |n: u128, started: u64, activity: u64, waits: Vec<Wait>| {
        Fixture::new(
            run(acme(), n, 1, 1, "", &[], None),
            History::started(started).log(activity, "tick"),
            RunMode::Live,
        )
        .waits(waits)
    };
    vec![
        fixture(
            1,
            100,
            500,
            vec![
                until(wait(WaitKind::Signal, Some(9), Some("a"), 100), 900),
                wait(WaitKind::Call, Some(1), Some("x"), 100),
            ],
        ),
        fixture(
            2,
            200,
            700,
            vec![wait(WaitKind::Signal, None, Some("b"), 200)],
        ),
        fixture(
            3,
            300,
            500,
            vec![until(wait(WaitKind::Timer, Some(1), None, 300), 900)],
        ),
        fixture(
            4,
            50,
            900,
            vec![
                until(wait(WaitKind::Timer, Some(1), None, 50), 400),
                until(wait(WaitKind::Timer, Some(2), None, 60), 100),
            ],
        ),
        fixture(5, 500, 700, Vec::new()),
        fixture(
            6,
            10,
            100,
            vec![until(wait(WaitKind::Timer, Some(1), None, 10), 900)],
        ),
    ]
}

/// The three orders, their ties, the runs without a wake, and pages through cursors.
pub async fn orders_and_cursors_page_through_runs<S: ReadModel>(store: &S) {
    apply_all(store, &ordered_runs()).await;
    let all = RunQuery::new(acme());
    let numbers = |summaries: &[RunSummary]| -> Vec<u128> {
        summaries
            .iter()
            .map(|summary| number(summary.run.id))
            .collect()
    };
    let orders = [
        (RunOrder::Started, vec![6, 5, 4, 3, 2, 1]),
        (RunOrder::Activity, vec![4, 5, 2, 3, 1, 6]),
        (RunOrder::Wakes, vec![4, 1, 3, 6, 2, 5]),
    ];
    for (order, expected) in &orders {
        let found = page(store, &all, *order, None, 100).await;
        assert_eq!(
            &numbers(&found),
            expected,
            "the runs in the order {order:?}"
        );
        for limit in [1, 2, 4, 6, 10] {
            let mut paged = Vec::new();
            let mut after: Option<SearchCursor> = None;
            loop {
                let found = page(store, &all, *order, after.as_ref(), limit).await;
                assert!(found.len() <= limit, "a page of {limit} at most");
                let Some(last) = found.last() else {
                    break;
                };
                after = Some(last.cursor(*order));
                paged.extend(numbers(&found));
                if found.len() < limit {
                    break;
                }
                assert!(paged.len() <= expected.len(), "pages of {order:?} end");
            }
            assert_eq!(
                &paged, expected,
                "pages of {limit} in the order {order:?}, each after the last run of the previous"
            );
        }
        assert!(page(store, &all, *order, None, 0).await.is_empty());
    }
    let four = summary(store, acme(), 4).await;
    assert_eq!(
        four.wakes_at,
        Some(at(100)),
        "the earliest end of #4's waits"
    );
    assert_eq!(summary(store, acme(), 1).await.wakes_at, Some(at(900)));
    assert_eq!(summary(store, acme(), 2).await.wakes_at, None);

    let cursor = |key: Option<u64>, n: u128| SearchCursor {
        key: key.map(at),
        run: id(n),
    };
    let cases = [
        (RunOrder::Wakes, cursor(Some(900), 1), vec![3, 6, 2, 5]),
        (RunOrder::Wakes, cursor(None, 2), vec![5]),
        (RunOrder::Wakes, cursor(Some(500), 99), vec![1, 3, 6, 2, 5]),
        (RunOrder::Wakes, cursor(None, 0), vec![2, 5]),
        (RunOrder::Activity, cursor(Some(700), 5), vec![2, 3, 1, 6]),
        (RunOrder::Activity, cursor(Some(600), 0), vec![3, 1, 6]),
        (RunOrder::Activity, cursor(Some(100), 6), vec![]),
        (RunOrder::Started, cursor(None, 4), vec![3, 2, 1]),
        (RunOrder::Started, cursor(None, 100), vec![6, 5, 4, 3, 2, 1]),
    ];
    for (order, after, expected) in cases {
        let found = page(store, &all, order, Some(&after), 100).await;
        assert_eq!(
            numbers(&found),
            expected,
            "the runs after {after:?} in the order {order:?}: a cursor is a position, not a run"
        );
    }
    let waiting_runs = RunQuery::new(acme()).with(waiting(&[]));
    assert_eq!(
        numbers(&page(store, &waiting_runs, RunOrder::Wakes, None, 100).await),
        [4, 1, 3, 6, 2],
        "the waiting runs, earliest wake first"
    );
    let after_three = page(
        store,
        &waiting_runs,
        RunOrder::Wakes,
        Some(&cursor(Some(900), 3)),
        1,
    )
    .await;
    assert_eq!(numbers(&after_three), [6]);
}

// -------------------------------------------------------------------------------------------
// Checks: statistics

fn every_status(counts: &[(RunStatus, u64)]) -> BTreeMap<RunStatus, u64> {
    let mut all: BTreeMap<RunStatus, u64> = [
        RunStatus::Pending,
        RunStatus::Active,
        RunStatus::Completed,
        RunStatus::Failed,
        RunStatus::Cancelled,
        RunStatus::Aborted,
    ]
    .into_iter()
    .map(|status| (status, 0))
    .collect();
    all.extend(counts.iter().copied());
    all
}

fn every_wait(counts: &[(WaitingFor, u64)]) -> BTreeMap<WaitingFor, u64> {
    let mut all: BTreeMap<WaitingFor, u64> =
        WaitingFor::ALL.iter().map(|kind| (*kind, 0)).collect();
    all.extend(counts.iter().copied());
    all
}

fn values(pairs: &[(&str, u64)]) -> Vec<FacetValue> {
    pairs
        .iter()
        .map(|(value, runs)| FacetValue {
            value: (*value).to_owned(),
            runs: *runs,
        })
        .collect()
}

fn latency(count: u64, mean: u64, p50: u64, p95: u64, p99: u64, max: u64) -> Latency {
    Latency {
        count,
        mean_ms: mean,
        p50_ms: p50,
        p95_ms: p95,
        p99_ms: p99,
        max_ms: max,
    }
}

/// Scripts with the same number of runs, by digest.
fn scripts_by_hex(counts: &[(u8, u64)]) -> Vec<FacetValue> {
    let mut sorted: Vec<(String, u64)> = counts
        .iter()
        .map(|(n, runs)| (script(*n).to_hex(), *runs))
        .collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted
        .into_iter()
        .map(|(value, runs)| FacetValue { value, runs })
        .collect()
}

/// Counts, series, durations, facets: over the runs that started in the window; waiting runs
/// and quarantined runs: whenever they started.
pub async fn stats_count_the_runs_of_a_query<S: ReadModel>(store: &S) {
    project_corpus(store).await;
    let all = RunQuery::new(acme());
    let stats = |query: RunQuery, window: StatsWindow| async move {
        store.run_stats(&query, &window).await.expect("run_stats")
    };
    let waiting_now = every_wait(&[
        (WaitingFor::Signal, 2),
        (WaitingFor::Timer, 1),
        (WaitingFor::Call, 1),
        (WaitingFor::Host, 1),
        (WaitingFor::Approval, 1),
        (WaitingFor::Parked, 1),
        (WaitingFor::Deferred, 1),
    ]);

    let everything = stats(
        all.clone(),
        StatsWindow {
            since: None,
            until: None,
            bucket_ms: None,
            top: 3,
        },
    )
    .await;
    assert_eq!(
        everything,
        RunStats {
            runs: 9,
            statuses: every_status(&[
                (RunStatus::Active, 3),
                (RunStatus::Completed, 1),
                (RunStatus::Failed, 3),
                (RunStatus::Cancelled, 1),
                (RunStatus::Aborted, 1),
            ]),
            series: Vec::new(),
            durations: Some(latency(6, 283, 200, 500, 500, 500)),
            waiting: waiting_now.clone(),
            quarantined: 1,
            facets: Facets {
                statuses: values(&[("active", 3), ("failed", 3), ("aborted", 1)]),
                labels: vec![
                    LabelFacet {
                        key: "agent".into(),
                        runs: 7,
                        values: values(&[("triage", 4), ("deploy", 2), ("review", 1)]),
                    },
                    LabelFacet {
                        key: "customer".into(),
                        runs: 3,
                        values: values(&[("acme-corp", 2), ("globex", 1)]),
                    },
                    LabelFacet {
                        key: "conversation".into(),
                        runs: 1,
                        values: values(&[("c-42", 1)]),
                    },
                ],
                tools: values(&[
                    ("github.close_issue", 3),
                    ("github.list_issues", 3),
                    ("ask_user", 2)
                ]),
                errors: values(&[("ToolError", 2), ("error", 2), ("CancelledError", 1)]),
                scripts: {
                    let mut scripts = scripts_by_hex(&[(1, 3)]);
                    scripts.extend(scripts_by_hex(&[(2, 2), (3, 2)]).into_iter().take(2));
                    scripts
                },
                waiting: values(&[("signal", 2), ("approval", 1), ("call", 1)]),
                signals: values(&[("approval", 1), ("review", 1)]),
            },
        },
        "every run of acme, the 3 most frequent values of each facet (ties by value, byte order)"
    );

    let windowed = stats(
        all.clone(),
        StatsWindow {
            since: Some(at(1_000)),
            until: Some(at(5_500)),
            bucket_ms: Some(2_000),
            top: 10,
        },
    )
    .await;
    assert_eq!(
        windowed,
        RunStats {
            runs: 5,
            statuses: every_status(&[
                (RunStatus::Active, 2),
                (RunStatus::Failed, 1),
                (RunStatus::Cancelled, 1),
                (RunStatus::Aborted, 1),
            ]),
            series: vec![
                every_status(&[(RunStatus::Failed, 1), (RunStatus::Cancelled, 1)]),
                every_status(&[(RunStatus::Active, 2)]),
                every_status(&[(RunStatus::Aborted, 1)]),
            ],
            durations: Some(latency(3, 366, 500, 500, 500, 500)),
            waiting: waiting_now.clone(),
            quarantined: 1,
            facets: Facets {
                statuses: values(&[
                    ("active", 2),
                    ("aborted", 1),
                    ("cancelled", 1),
                    ("failed", 1)
                ]),
                labels: vec![
                    LabelFacet {
                        key: "agent".into(),
                        runs: 4,
                        values: values(&[("deploy", 2), ("review", 1), ("triage", 1)]),
                    },
                    LabelFacet {
                        key: "conversation".into(),
                        runs: 1,
                        values: values(&[("c-42", 1)]),
                    },
                    LabelFacet {
                        key: "customer".into(),
                        runs: 1,
                        values: values(&[("globex", 1)]),
                    },
                ],
                tools: values(&[
                    ("ask_user", 2),
                    ("github.get_issue", 2),
                    ("github.list_issues", 2),
                    ("github.close_issue", 1),
                ]),
                errors: values(&[
                    ("CancelledError", 1),
                    ("TypeError", 1),
                    ("error", 1),
                    ("limit_exceeded", 1),
                ]),
                scripts: {
                    let mut scripts = scripts_by_hex(&[(2, 2), (3, 2)]);
                    scripts.extend(scripts_by_hex(&[(1, 1)]));
                    scripts
                },
                waiting: values(&[
                    ("signal", 2),
                    ("approval", 1),
                    ("call", 1),
                    ("deferred", 1),
                    ("host", 1),
                    ("parked", 1),
                    ("timer", 1),
                ]),
                signals: values(&[("approval", 1), ("review", 1)]),
            },
        },
        "the runs started in [1000, 5500), in buckets of 2000 from 1000: the last one cut at 5500"
    );

    let late = stats(
        all.clone(),
        StatsWindow {
            since: Some(at(5_000)),
            until: Some(at(9_000)),
            bucket_ms: None,
            top: 10,
        },
    )
    .await;
    assert_eq!(late.runs, 4);
    assert_eq!(
        late.statuses,
        every_status(&[
            (RunStatus::Active, 1),
            (RunStatus::Failed, 2),
            (RunStatus::Aborted, 1)
        ])
    );
    assert_eq!(late.durations, Some(latency(3, 133, 100, 200, 200, 200)));
    assert_eq!(
        (&late.waiting, late.quarantined),
        (&waiting_now, 1),
        "waiting and quarantined runs count whenever they started"
    );
    assert!(
        late.facets.waiting.is_empty() && late.facets.signals.is_empty(),
        "the facets are over the runs started in the window, and #7 waits for nothing: {:?}",
        late.facets
    );

    let empty = stats(
        all.clone(),
        StatsWindow {
            since: Some(at(20_000)),
            until: Some(at(22_000)),
            bucket_ms: Some(1_000),
            top: 10,
        },
    )
    .await;
    assert_eq!(
        empty,
        RunStats {
            runs: 0,
            statuses: every_status(&[]),
            series: vec![every_status(&[]), every_status(&[])],
            durations: None,
            waiting: waiting_now.clone(),
            quarantined: 1,
            facets: Facets::default(),
        },
        "a window without runs: zeros everywhere, but the runs waiting now"
    );

    let triage = stats(
        all.clone().with(label("agent", vec![exact("triage")])),
        StatsWindow {
            since: None,
            until: None,
            bucket_ms: None,
            top: 0,
        },
    )
    .await;
    assert_eq!(
        triage,
        RunStats {
            runs: 4,
            statuses: every_status(&[
                (RunStatus::Completed, 1),
                (RunStatus::Cancelled, 1),
                (RunStatus::Failed, 2),
            ]),
            series: Vec::new(),
            durations: Some(latency(4, 275, 200, 500, 500, 500)),
            waiting: every_wait(&[]),
            quarantined: 0,
            facets: Facets::default(),
        },
        "the runs a query matches; no facet value at top 0"
    );
}

// -------------------------------------------------------------------------------------------
// Checks: failure groups

const GROUP_A: &str = "aaaaaaaaaaaaaaaa";
const GROUP_B: &str = "bbbbbbbbbbbbbbbb";
const GROUP_C: &str = "cccccccccccccccc";
const GROUP_D: &str = "dddddddddddddddd";
const GROUP_E: &str = "eeeeeeeeeeeeeeee";

/// Eleven runs in five failure groups, and two runs without an ending.
fn failed_runs() -> Vec<Fixture> {
    let failed = |n: u128,
                  ended: u64,
                  script_n: u8,
                  pairs: &[(&str, &str)],
                  error: &str,
                  message: &str,
                  line: Option<u32>,
                  fingerprint: &str| {
        let history = if error == "limit_exceeded" {
            History::started(ended - 50).abort(ended, AbortKind::LimitExceeded, message)
        } else {
            History::started(ended - 50).fail_with(ended, error, message)
        };
        let signature = format!("{error}: {}", message.split(' ').next().unwrap_or(""));
        Fixture::new(
            run(acme(), n, script_n, 1, "", pairs, None),
            history,
            RunMode::Live,
        )
        .ending(ending(error, message, line, &signature, fingerprint))
    };
    vec![
        failed(
            1,
            1_000,
            1,
            &[("agent", "x"), ("env", "prod")],
            "TypeError",
            "x is 1",
            Some(5),
            GROUP_A,
        ),
        failed(
            2,
            2_000,
            1,
            &[("agent", "x")],
            "TypeError",
            "x is 2",
            Some(3),
            GROUP_A,
        ),
        failed(
            3,
            3_000,
            2,
            &[("agent", "y"), ("env", "prod"), ("team", "a")],
            "TypeError",
            "x is 3",
            Some(5),
            GROUP_A,
        ),
        failed(
            4,
            4_000,
            3,
            &[("agent", "x"), ("team", "b"), ("tier", "1"), ("zone", "eu")],
            "TypeError",
            "x is 4",
            Some(3),
            GROUP_A,
        ),
        failed(
            5,
            4_000,
            3,
            &[("region", "us")],
            "TypeError",
            "x is 5",
            Some(9),
            GROUP_A,
        ),
        failed(6, 1_500, 1, &[], "ToolError", "y failed", None, GROUP_B),
        failed(
            7,
            2_500,
            1,
            &[("agent", "x")],
            "ToolError",
            "y failed",
            Some(4),
            GROUP_B,
        ),
        failed(
            8,
            3_500,
            4,
            &[],
            "limit_exceeded",
            "too long",
            None,
            GROUP_C,
        ),
        failed(
            9,
            3_600,
            4,
            &[],
            "limit_exceeded",
            "too long",
            None,
            GROUP_C,
        ),
        failed(
            10,
            500,
            5,
            &[],
            ScriptError::CANCELLED,
            "stop",
            None,
            GROUP_D,
        ),
        failed(11, 500, 5, &[], "RangeError", "out", None, GROUP_E),
        Fixture::new(
            run(acme(), 12, 1, 1, "", &[("agent", "x")], None),
            History::started(1_000).call(1_000, 1, "slow"),
            RunMode::Live,
        )
        .waits(vec![wait(WaitKind::Call, Some(1), Some("slow"), 1_000)]),
        Fixture::new(
            run(acme(), 13, 1, 1, "", &[("agent", "x")], None),
            History::started(1_000).complete(1_100),
            RunMode::Live,
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn group(
    fingerprint: &str,
    error: &str,
    signature: &str,
    runs: u64,
    series: Vec<u64>,
    scripts: u64,
    (first, last): (u64, u64),
    line: Option<u32>,
    pairs: &[(&str, u64)],
    (latest, latest_status, latest_message): (u128, RunStatus, &str),
) -> FailureGroup {
    FailureGroup {
        fingerprint: fingerprint.into(),
        signature: signature.into(),
        error: error.into(),
        runs,
        series,
        scripts,
        first_ended_at: at(first),
        last_ended_at: at(last),
        line,
        labels: values(pairs),
        latest: id(latest),
        latest_status,
        latest_message: latest_message.into(),
    }
}

/// Groups by fingerprint, the most runs first, then the latest ending, then by fingerprint;
/// in each group its series, scripts, first and last ending, most common line, labels and
/// latest run.
pub async fn failures_are_grouped_by_fingerprint<S: ReadModel>(store: &S) {
    apply_all(store, &failed_runs()).await;
    let groups = |query: RunQuery, window: FailureWindow| async move {
        store
            .failure_groups(&query, &window)
            .await
            .expect("failure_groups")
    };
    let window =
        |since: Option<u64>, until: Option<u64>, bucket_ms: Option<u64>, limit| FailureWindow {
            since: since.map(at),
            until: until.map(at),
            bucket_ms,
            limit,
        };
    let all = RunQuery::new(acme());
    let a = |series, runs, scripts, ended, line, pairs: &[(&str, u64)], latest: (u128, &str)| {
        group(
            GROUP_A,
            "TypeError",
            "TypeError: x",
            runs,
            series,
            scripts,
            ended,
            line,
            pairs,
            (latest.0, RunStatus::Failed, latest.1),
        )
    };
    let b = |series| {
        group(
            GROUP_B,
            "ToolError",
            "ToolError: y",
            2,
            series,
            1,
            (1_500, 2_500),
            Some(4),
            &[("agent:x", 1)],
            (7, RunStatus::Failed, "y failed"),
        )
    };
    let c = |series| {
        group(
            GROUP_C,
            "limit_exceeded",
            "limit_exceeded: too",
            2,
            series,
            1,
            (3_500, 3_600),
            None,
            &[],
            (9, RunStatus::Aborted, "too long"),
        )
    };

    assert_eq!(
        groups(all.clone(), window(None, None, None, 10)).await,
        FailureGroups {
            runs: 11,
            groups: 5,
            top: vec![
                a(
                    Vec::new(),
                    5,
                    3,
                    (1_000, 4_000),
                    Some(3),
                    &[
                        ("agent:x", 3),
                        ("env:prod", 2),
                        ("agent:y", 1),
                        ("region:us", 1),
                        ("team:a", 1)
                    ],
                    (5, "x is 5"),
                ),
                c(Vec::new()),
                b(Vec::new()),
                group(
                    GROUP_D,
                    ScriptError::CANCELLED,
                    "CancelledError: stop",
                    1,
                    Vec::new(),
                    1,
                    (500, 500),
                    None,
                    &[],
                    (10, RunStatus::Cancelled, "stop"),
                ),
                group(
                    GROUP_E,
                    "RangeError",
                    "RangeError: out",
                    1,
                    Vec::new(),
                    1,
                    (500, 500),
                    None,
                    &[],
                    (11, RunStatus::Failed, "out"),
                ),
            ],
        },
        "every group: lines 3 and 5 tie in A (the lowest wins), #4 and #5 ended together in A \
         (the highest id is the latest), D and E tie (by fingerprint)"
    );

    assert_eq!(
        groups(
            all.clone(),
            window(Some(1_000), Some(4_000), Some(1_000), 10)
        )
        .await,
        FailureGroups {
            runs: 7,
            groups: 3,
            top: vec![
                a(
                    vec![1, 1, 1],
                    3,
                    2,
                    (1_000, 3_000),
                    Some(5),
                    &[
                        ("agent:x", 2),
                        ("env:prod", 2),
                        ("agent:y", 1),
                        ("team:a", 1)
                    ],
                    (3, "x is 3"),
                ),
                c(vec![0, 0, 2]),
                b(vec![1, 1, 0]),
            ],
        },
        "the runs ended in [1000, 4000), in buckets of 1000"
    );

    let limited = groups(all.clone(), window(None, None, None, 2)).await;
    assert_eq!(
        (limited.runs, limited.groups),
        (11, 5),
        "counted beyond the limit"
    );
    assert_eq!(
        limited
            .top
            .iter()
            .map(|group| group.fingerprint.as_str())
            .collect::<Vec<_>>(),
        [GROUP_A, GROUP_C]
    );
    let none = groups(all.clone(), window(None, None, None, 0)).await;
    assert_eq!((none.runs, none.groups, none.top.len()), (11, 5, 0));

    assert_eq!(
        groups(
            all.clone().with(label("agent", vec![exact("x")])),
            window(None, None, None, 10)
        )
        .await,
        FailureGroups {
            runs: 4,
            groups: 2,
            top: vec![
                a(
                    Vec::new(),
                    3,
                    2,
                    (1_000, 4_000),
                    Some(3),
                    &[
                        ("agent:x", 3),
                        ("env:prod", 1),
                        ("team:b", 1),
                        ("tier:1", 1),
                        ("zone:eu", 1)
                    ],
                    (4, "x is 4"),
                ),
                group(
                    GROUP_B,
                    "ToolError",
                    "ToolError: y",
                    1,
                    Vec::new(),
                    1,
                    (2_500, 2_500),
                    Some(4),
                    &[("agent:x", 1)],
                    (7, RunStatus::Failed, "y failed"),
                ),
            ],
        },
        "the runs the query matches: #12 and #13 have no ending"
    );
    let others = groups(
        all.clone()
            .without(Predicate::Fingerprint(vec![GROUP_A.into()])),
        window(None, None, None, 10),
    )
    .await;
    assert_eq!((others.runs, others.groups), (6, 4));
}

// -------------------------------------------------------------------------------------------
// Checks: service state

fn task_run() -> Fixture {
    Fixture::new(
        run(acme(), 1, 1, 1, "", &[], None),
        History::started(0)
            .call(0, 1, "ask_user")
            .call(0, 2, "github.get_issue")
            .call(0, 3, "http")
            .call(0, 4, "ask_user"),
        RunMode::Live,
    )
    .waits(vec![
        wait(WaitKind::Call, Some(2), Some("github.get_issue"), 0),
        tasked(
            wait(WaitKind::Call, Some(3), Some("http"), 0),
            TaskPhase::Deferred,
        ),
        wait(WaitKind::Host, Some(1), Some("ask_user"), 0),
        wait(WaitKind::Host, Some(4), Some("ask_user"), 0),
    ])
}

/// The records of the task states, in the order their changes happened.
fn task_records() -> Vec<Vec<TaskProgress>> {
    vec![
        vec![task(1, 0, TaskPhase::Ready), task(2, 0, TaskPhase::Ready)],
        vec![task(4, 0, TaskPhase::Approval)],
        vec![
            task(1, 1, TaskPhase::Claimed),
            task(2, 1, TaskPhase::Claimed),
        ],
        vec![task(1, 1, TaskPhase::Parked)],
        vec![task(2, 1, TaskPhase::Parked)],
        vec![task(1, 1, TaskPhase::Ready), task(2, 1, TaskPhase::Ready)],
        vec![task(1, 2, TaskPhase::Claimed)],
        vec![task(1, 2, TaskPhase::Deferred)],
        vec![task(5, 3, TaskPhase::Claimed), task(6, 0, TaskPhase::Ready)],
    ]
}

/// Task states, recorded in any order, before or after the run is projected: each call keeps
/// the state of the highest rank and its highest attempt; the waits show them.
pub async fn task_states_keep_the_furthest<S: ReadModel>(store: &S) {
    let fixture = task_run();
    let run1 = fixture.run.run.clone();
    let records = task_records();
    let (before, after) = records.split_at(2);
    // Out of order: the last change first, then the others backwards.
    let mut late = after.to_vec();
    late.reverse();
    for tasks in before.iter().rev() {
        store
            .record_tasks(&run1, tasks)
            .await
            .expect("record_tasks");
    }
    fixture.apply(store).await;
    let phases = |summary: &RunSummary| -> Vec<(u32, Option<TaskPhase>)> {
        summary
            .waits
            .iter()
            .map(|wait| (wait.seq.map_or(0, |seq| seq.0), wait.task))
            .collect()
    };
    assert_eq!(
        phases(&summary(store, acme(), 1).await),
        [
            (2, Some(TaskPhase::Ready)),
            (3, None),
            (1, Some(TaskPhase::Ready)),
            (4, Some(TaskPhase::Approval))
        ],
        "recorded before the run was projected; the snapshot's task state ignored"
    );
    for tasks in &late {
        store
            .record_tasks(&run1, tasks)
            .await
            .expect("record_tasks");
    }
    store
        .record_tasks(&run1, &[task(1, 0, TaskPhase::Approval)])
        .await
        .expect("record_tasks");
    store
        .record_tasks(
            &RunRef::new(other(), id(1)),
            &[task(1, 9, TaskPhase::Ready), task(7, 1, TaskPhase::Claimed)],
        )
        .await
        .expect("record_tasks of another tenant's run");
    let one = summary(store, acme(), 1).await;
    assert_eq!(
        phases(&one),
        [
            (2, Some(TaskPhase::Ready)),
            (3, None),
            (1, Some(TaskPhase::Deferred)),
            (4, Some(TaskPhase::Approval))
        ],
        "each call's state of the highest rank: (attempt, approval or claim < parked or \
         deferred < ready)"
    );
    let waiting_for: BTreeSet<WaitingFor> = one.waits.iter().map(Wait::waiting_for).collect();
    assert_eq!(
        waiting_for,
        BTreeSet::from([WaitingFor::Call, WaitingFor::Deferred, WaitingFor::Approval])
    );
    let expected_attempts = BTreeMap::from([
        (Seq(1), 2),
        (Seq(2), 1),
        (Seq(4), 0),
        (Seq(5), 3),
        (Seq(6), 0),
    ]);
    assert_eq!(
        store.attempts(&run1).await.expect("attempts"),
        expected_attempts,
        "the highest attempt of every call recorded, those without a wait included"
    );
    assert!(
        store
            .attempts(&RunRef::new(other(), id(1)))
            .await
            .expect("attempts")
            .is_empty(),
        "another tenant sees nothing"
    );
    for (kinds, expected) in [
        (vec![WaitingFor::Deferred], vec![1]),
        (vec![WaitingFor::Approval], vec![1]),
        (vec![WaitingFor::Parked], vec![]),
        (vec![WaitingFor::Host], vec![]),
    ] {
        let query = RunQuery::new(acme()).with(waiting(&kinds));
        assert_eq!(ids(store, &query).await, expected, "{query:?}");
    }

    // A later snapshot keeps the task states: they are recorded per call, not per wait.
    let mut longer = fixture.clone();
    longer.history = History(fixture.history.clone()).log(50, "still waiting").0;
    longer.snapshot.history_len += 1;
    longer.apply(store).await;
    assert_eq!(phases(&summary(store, acme(), 1).await), phases(&one));
}

fn note(kind: QuarantineKind, reason: &str, since: u64) -> QuarantineNote {
    QuarantineNote {
        kind,
        reason: reason.into(),
        since: at(since),
    }
}

/// Quarantine: the record of the latest `at`, a quarantine over a release at the same `at`;
/// in any order, before or after the run is projected.
pub async fn quarantine_keeps_the_latest_record<S: ReadModel>(store: &S) {
    let first = note(QuarantineKind::NonDeterminism, "entry 3 differs", 1_000);
    let second = note(QuarantineKind::Failures, "5 activations failed", 1_100);
    let run1 = RunRef::new(acme(), id(1));
    let quarantined = || {
        let query = RunQuery::new(acme()).with(Predicate::Quarantined);
        async move { !ids(store, &query).await.is_empty() }
    };
    store
        .record_quarantine(&run1, Some(&first), at(1_000))
        .await
        .expect("record_quarantine");
    let fixture = Fixture::new(
        run(acme(), 1, 1, 1, "", &[], None),
        History::started(0).log(10, "x"),
        RunMode::Live,
    );
    fixture.apply(store).await;
    assert_eq!(
        summary(store, acme(), 1).await.quarantine,
        Some(first.clone()),
        "recorded before the run was projected"
    );
    let steps: [(Option<&QuarantineNote>, u64, Option<&QuarantineNote>, &str); 5] = [
        (None, 900, Some(&first), "an older release changes nothing"),
        (
            None,
            1_000,
            Some(&first),
            "a quarantine wins over a release at the same time",
        ),
        (None, 1_100, None, "a later release"),
        (
            Some(&second),
            1_050,
            None,
            "an older quarantine changes nothing",
        ),
        (
            Some(&second),
            1_100,
            Some(&second),
            "a quarantine wins over a release at the same time",
        ),
    ];
    for (record, when, expected, why) in steps {
        store
            .record_quarantine(&run1, record, at(when))
            .await
            .expect("record_quarantine");
        assert_eq!(
            summary(store, acme(), 1).await.quarantine.as_ref(),
            expected,
            "{why}"
        );
        assert_eq!(quarantined().await, expected.is_some(), "{why}");
    }
    store
        .record_quarantine(&RunRef::new(other(), id(1)), None, at(9_000))
        .await
        .expect("record_quarantine of another tenant's run");
    assert_eq!(
        summary(store, acme(), 1).await.quarantine,
        Some(second.clone()),
        "another tenant's record is ignored"
    );

    // The same records backwards, on another run: the same quarantine.
    let run2 = RunRef::new(acme(), id(2));
    let records = [
        (Some(&first), 1_000),
        (None, 900),
        (None, 1_000),
        (None, 1_100),
        (Some(&second), 1_050),
        (Some(&second), 1_100),
    ];
    for (record, when) in records.iter().rev() {
        store
            .record_quarantine(&run2, *record, at(*when))
            .await
            .expect("record_quarantine");
    }
    Fixture {
        run: ProjectedRun {
            run: run2.clone(),
            ..fixture.run.clone()
        },
        ..fixture.clone()
    }
    .apply(store)
    .await;
    assert_eq!(summary(store, acme(), 2).await.quarantine, Some(second));
    let stats = store
        .run_stats(
            &RunQuery::new(acme()),
            &StatsWindow {
                since: None,
                until: None,
                bucket_ms: None,
                top: 0,
            },
        )
        .await
        .expect("run_stats");
    assert_eq!(stats.quarantined, 2);
}

// -------------------------------------------------------------------------------------------
// Checks: scripts, tools and shapes, on five plain runs

fn projected(tenant: TenantId, n: u128, script_n: u8, toolset_n: u8, agent: &str) -> ProjectedRun {
    run(
        tenant,
        n,
        script_n,
        toolset_n,
        "",
        &[("agent", agent)],
        None,
    )
}

/// Five runs: four of acme (completed, failed, cancelled, active) and one of another tenant.
fn plain_runs() -> Vec<Fixture> {
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
    let live = RunMode::Live;
    vec![
        Fixture::new(projected(acme(), 1, 1, 1, "triage"), r1, live),
        Fixture::new(projected(acme(), 2, 1, 1, "deploy"), r2, live),
        Fixture::new(projected(acme(), 3, 2, 2, "triage"), r3, live),
        Fixture::new(projected(acme(), 4, 2, 1, "deploy"), r4, live),
        Fixture::new(projected(other(), 5, 1, 1, "triage"), r5, live),
    ]
}

pub async fn scripts_and_tools_are_aggregated<S: ReadModel>(store: &S) {
    apply_all(store, &plain_runs()).await;
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
                latency: Some(latency(1, 100, 100, 100, 100, 100)),
            },
            ToolStats {
                tool: "github.list_issues".into(),
                calls: 3,
                completed: 3,
                failed: 0,
                errors_by_kind: BTreeMap::new(),
                retries: 0,
                latency: Some(latency(3, 233, 200, 400, 400, 400)),
            },
            ToolStats {
                tool: "slack.post".into(),
                calls: 2,
                completed: 0,
                failed: 1,
                errors_by_kind: BTreeMap::from([(ToolFailureKind::Unavailable, 1)]),
                retries: 0,
                latency: Some(latency(1, 200, 200, 200, 200, 200)),
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
    apply_all(store, &plain_runs()).await;
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
        .observed_shapes(&other(), &tools[..1])
        .await
        .expect("observed_shapes");
    assert_eq!(
        other["github.list_issues"],
        Shape::of(&json!({ "secret_of_other": "x" }))
    );
}

pub async fn scripts_are_filtered_and_count_labels<S: ReadModel>(store: &S) {
    apply_all(store, &plain_runs()).await;
    let counts = |scripts: &[ScriptSummary]| {
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

    let triage = labels(&[("agent", "triage")]);
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
            labels: labels(&[("agent", "deploy")]),
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

/// The attempts recorded with the task states give `tool_stats` its retries.
pub async fn attempts_give_retries<S: ReadModel>(store: &S) {
    let run1 = RunRef::new(acme(), id(1));
    let run2 = RunRef::new(acme(), id(2));
    let claimed = |seq, attempt| [task(seq, attempt, TaskPhase::Claimed)];
    // Recorded before the run is projected: kept for when it is.
    store
        .record_tasks(&run1, &claimed(2, 2))
        .await
        .expect("record_tasks");
    apply_all(store, &plain_runs()).await;
    store
        .record_tasks(&run1, &claimed(2, 1))
        .await
        .expect("record_tasks");
    store
        .record_tasks(&run2, &claimed(1, 3))
        .await
        .expect("record_tasks");
    store
        .record_tasks(&run2, &claimed(1, 3))
        .await
        .expect("record_tasks");
    store
        .record_tasks(&RunRef::new(other(), id(1)), &claimed(1, 4))
        .await
        .expect("record_tasks of another tenant's run");

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
            .attempts(&RunRef::new(other(), id(1)))
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
}

// -------------------------------------------------------------------------------------------
// Checks: order and repetition, retention

/// The same entries and records in another order, some of them twice, give the same
/// projections.
pub async fn projections_ignore_order_and_repetition<S: ReadModel>(ordered: &S, shuffled: &S) {
    project_corpus(ordered).await;
    let mut batches: Vec<(ProjectedRun, Vec<HistoryEntry>, RunSnapshot)> = corpus()
        .iter()
        .flat_map(|fixture| fixture.batches(2))
        .collect();
    batches.reverse();
    let repeated: Vec<_> = batches.iter().step_by(2).cloned().collect();
    record_corpus_state(shuffled, true).await;
    for (run, entries, snapshot) in batches.iter().chain(repeated.iter()) {
        shuffled.apply(run, entries, snapshot).await.expect("apply");
    }
    record_corpus_state(shuffled, true).await;

    for tenant in [acme(), other()] {
        let query = RunQuery::new(tenant.clone());
        for order in [RunOrder::Started, RunOrder::Activity, RunOrder::Wakes] {
            assert_eq!(
                page(ordered, &query, order, None, 100).await,
                page(shuffled, &query, order, None, 100).await,
                "the runs of {tenant} in the order {order:?}"
            );
        }
        let window = StatsWindow {
            since: Some(at(0)),
            until: Some(at(10_000)),
            bucket_ms: Some(2_500),
            top: 10,
        };
        assert_eq!(
            ordered.run_stats(&query, &window).await.expect("run_stats"),
            shuffled
                .run_stats(&query, &window)
                .await
                .expect("run_stats")
        );
        let window = FailureWindow {
            since: Some(at(0)),
            until: Some(at(10_000)),
            bucket_ms: Some(2_500),
            limit: 10,
        };
        assert_eq!(
            ordered
                .failure_groups(&query, &window)
                .await
                .expect("failure_groups"),
            shuffled
                .failure_groups(&query, &window)
                .await
                .expect("failure_groups")
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
    for fixture in corpus() {
        assert_eq!(
            ordered.attempts(&fixture.run.run).await.expect("attempts"),
            shuffled.attempts(&fixture.run.run).await.expect("attempts"),
            "the attempts of #{}",
            number(fixture.run.run.id)
        );
    }
}

/// Purging and deleting remove everything of a run: summary, labels, waits, calls, task
/// states, quarantine. Projected again, the run has none of its records.
pub async fn runs_are_purged_and_deleted<S: ReadModel>(store: &S) {
    let fixtures = plain_runs();
    apply_all(store, &fixtures).await;
    let note = note(QuarantineKind::Failures, "5 activations failed", 200);
    for n in [1, 4] {
        let run = RunRef::new(acme(), id(n));
        store
            .record_tasks(&run, &[task(1, 2, TaskPhase::Claimed)])
            .await
            .expect("record_tasks");
        store
            .record_quarantine(&run, Some(&note), at(200))
            .await
            .expect("record_quarantine");
    }
    let acme_only = TenantScope::Only(BTreeSet::from([acme()]));
    let all = RunQuery::new(acme());
    assert_eq!(
        store.purge(at(300), &acme_only).await.expect("purge"),
        0,
        "ended before, strictly"
    );
    assert_eq!(store.purge(at(1_000), &acme_only).await.expect("purge"), 1);
    assert_eq!(ids(store, &all).await, [4, 3, 2]);
    assert_eq!(
        ids(store, &RunQuery::new(other())).await,
        [5],
        "out of scope"
    );
    // Projected again, the purged run comes back without its records.
    fixtures[0].apply(store).await;
    let one = summary(store, acme(), 1).await;
    assert_eq!(one.quarantine, None, "the quarantine was purged");
    assert!(
        store.attempts(&one.run).await.expect("attempts").is_empty(),
        "the task states were purged"
    );
    assert_eq!(store.purge(at(1_000), &acme_only).await.expect("purge"), 1);

    let except_acme = TenantScope::Except(BTreeSet::from([acme()]));
    assert_eq!(
        store
            .purge(at(1_000_000), &except_acme)
            .await
            .expect("purge"),
        1
    );
    assert!(ids(store, &RunQuery::new(other())).await.is_empty());
    assert_eq!(
        store
            .purge(at(1_000_000), &TenantScope::All)
            .await
            .expect("purge"),
        2,
        "the active run stays"
    );
    assert_eq!(ids(store, &all).await, [4]);

    let intruder = RunRef::new(other(), id(4));
    assert!(!store.delete_run(&intruder).await.expect("delete_run"));
    let four = RunRef::new(acme(), id(4));
    assert_eq!(
        summary(store, acme(), 4).await.quarantine,
        Some(note.clone()),
        "another tenant cannot delete a run"
    );
    assert!(store.delete_run(&four).await.expect("delete_run"));
    assert!(!store.delete_run(&four).await.expect("delete_run"));
    assert!(ids(store, &all).await.is_empty());
    let shapes = store
        .observed_shapes(&acme(), &["slack.post".to_owned()])
        .await
        .expect("observed_shapes");
    assert_eq!(shapes, BTreeMap::<String, Shape>::new());
    fixtures[3].apply(store).await;
    let four_again = summary(store, acme(), 4).await;
    assert_eq!(
        (four_again.quarantine, four_again.tool_calls),
        (None, 1),
        "deleted with its quarantine"
    );
    assert!(
        store.attempts(&four).await.expect("attempts").is_empty(),
        "deleted with its task states"
    );
}
