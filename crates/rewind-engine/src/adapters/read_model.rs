use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use rewind_core::{
    Command, Completion, Digest, Event, HistoryEntry, Lifecycle, Payload, RunId, RunStatus,
    ScriptError, Seq, Shape, Timestamp, ToolFailureKind,
};

use crate::adapters::SystemClock;
use crate::ports::{
    ChangedRun, Clock, FacetValue, Facets, FailureGroup, FailureGroups, FailureWindow,
    IndexedSummary, LabelFacet, Latency, ProjectedRun, QuarantineNote, ReadModel, ReadModelError,
    RunOrder, RunQuery, RunRef, RunSnapshot, RunStats, RunSummary, ScriptFilter, ScriptSummary,
    SearchCursor, StatsWindow, TaskProgress, TenantId, TenantScope, ToolStats, ToolStatsFilter,
    Wait, WaitKind, WaitingFor,
};

/// Every status, in the order of `RunStatus`: what `run_stats` counts, zeros included.
const STATUSES: [RunStatus; 6] = [
    RunStatus::Pending,
    RunStatus::Active,
    RunStatus::Completed,
    RunStatus::Failed,
    RunStatus::Cancelled,
    RunStatus::Aborted,
];

/// Labels a failure group lists at most.
const GROUP_LABELS: usize = 5;

/// `ReadModel` in memory: the projected entries of every run, keyed by `(run, index)`, with the
/// snapshot of the longest history; the task states and quarantine recorded per run; when each
/// run's summary last changed, by the system clock. Every query is computed from them. It passes the conformance suite: it is the reference implementation
/// of the contract.
#[derive(Debug, Default)]
pub struct MemoryReadModel {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    runs: BTreeMap<RunId, Projected>,
    /// Per call, the record of the highest rank, by run: recorded before or after the run's
    /// entries are projected.
    tasks: BTreeMap<RunId, BTreeMap<Seq, TaskProgress>>,
    /// The latest record of each run's quarantine, with its time.
    quarantines: BTreeMap<RunId, (Timestamp, Option<QuarantineNote>)>,
    /// When each run's summary last changed: milliseconds of the system clock.
    changed: BTreeMap<RunId, u64>,
}

#[derive(Debug)]
struct Projected {
    run: ProjectedRun,
    entries: BTreeMap<u64, HistoryEntry>,
    /// The snapshot of the longest history, its waits without task states.
    snapshot: RunSnapshot,
}

/// One tool call, from its `schedule_tool_call` and its completion.
#[derive(Debug, Default)]
struct Call {
    tool: Option<String>,
    scheduled_at: Option<Timestamp>,
    completed_at: Option<Timestamp>,
    failed: Option<ToolFailureKind>,
    completed: bool,
    result: Option<Shape>,
}

/// A run as the queries see it: its summary and what its calls did, as the run index of a
/// partition holds it.
type Row = IndexedSummary;

impl MemoryReadModel {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, ReadModelError> {
        self.state
            .lock()
            .map_err(|_| ReadModelError::Backend("the memory read model's lock is poisoned".into()))
    }
}

fn status_of(entry: &HistoryEntry) -> Option<RunStatus> {
    match &entry.event {
        Event::Lifecycle(Lifecycle::RunAborted(_)) => Some(RunStatus::Aborted),
        Event::Command(Command::CompleteRun { .. }) => Some(RunStatus::Completed),
        Event::Command(Command::FailRun { error }) if error.name == ScriptError::CANCELLED => {
            Some(RunStatus::Cancelled)
        }
        Event::Command(Command::FailRun { .. }) => Some(RunStatus::Failed),
        _ => None,
    }
}

impl Projected {
    /// The terminal entry's status and `recorded_at`, once projected.
    fn ended(&self) -> Option<(RunStatus, Timestamp)> {
        self.entries
            .values()
            .find_map(|entry| status_of(entry).map(|status| (status, entry.recorded_at)))
    }

    fn summary(
        &self,
        tasks: Option<&BTreeMap<Seq, TaskProgress>>,
        quarantine: Option<&QuarantineNote>,
    ) -> RunSummary {
        let ended = self.ended();
        let status = ended.map_or(RunStatus::Active, |(status, _)| status);
        let mut started_at = None;
        let mut tool_calls = 0;
        let mut errors = 0;
        for entry in self.entries.values() {
            match &entry.event {
                Event::Lifecycle(Lifecycle::RunStarted(_)) => started_at = Some(entry.recorded_at),
                Event::Command(Command::ScheduleToolCall { .. }) => tool_calls += 1,
                Event::Completion(Completion::ToolCallFailed { .. }) => errors += 1,
                _ => {}
            }
        }
        let waits: Vec<Wait> = if status == RunStatus::Active {
            self.snapshot
                .waits
                .iter()
                .map(|wait| Wait {
                    task: match (wait.kind, wait.seq) {
                        (WaitKind::Call | WaitKind::Host, Some(seq)) => tasks
                            .and_then(|tasks| tasks.get(&seq))
                            .map(|task| task.phase),
                        _ => None,
                    },
                    ..wait.clone()
                })
                .collect()
        } else {
            Vec::new()
        };
        RunSummary {
            run: self.run.run.clone(),
            namespace: self.run.namespace.clone(),
            origin: self.run.origin.clone(),
            suffix_len: self.run.suffix_len,
            script: self.run.script,
            headline: self.run.headline.clone(),
            toolset: self.run.toolset,
            status,
            mode: self.snapshot.mode,
            labels: self.run.labels.clone(),
            parent: self.run.parent,
            workflow: self.run.workflow.clone(),
            started_by: self.run.started_by,
            started_at,
            ended_at: ended.map(|(_, at)| at),
            last_activity_at: self
                .entries
                .values()
                .map(|entry| entry.recorded_at)
                .max()
                .unwrap_or(Timestamp(0)),
            tool_calls,
            errors,
            history_len: self.entries.keys().next_back().map_or(0, |last| last + 1),
            wakes_at: waits.iter().filter_map(|wait| wait.until).min(),
            waits,
            ending: self.snapshot.ending.clone(),
            quarantine: quarantine.cloned(),
        }
    }

    fn calls(&self) -> BTreeMap<Seq, Call> {
        let mut calls: BTreeMap<Seq, Call> = BTreeMap::new();
        for entry in self.entries.values() {
            match &entry.event {
                Event::Command(Command::ScheduleToolCall { seq, tool, .. }) => {
                    let call = calls.entry(*seq).or_default();
                    call.tool = Some(tool.clone());
                    call.scheduled_at = Some(entry.recorded_at);
                }
                Event::Completion(Completion::ToolCallCompleted { seq, result }) => {
                    let call = calls.entry(*seq).or_default();
                    call.completed = true;
                    call.completed_at = Some(entry.recorded_at);
                    if let Payload::Value(value) = result {
                        call.result = Some(Shape::of(value));
                    }
                }
                Event::Completion(Completion::ToolCallFailed { seq, error }) => {
                    let call = calls.entry(*seq).or_default();
                    call.failed = Some(error.kind);
                    call.completed_at = Some(entry.recorded_at);
                }
                _ => {}
            }
        }
        calls
    }
}

impl State {
    /// Whether the run is projected under another tenant: what is recorded for it is ignored.
    fn foreign(&self, run: &RunRef) -> bool {
        self.runs
            .get(&run.id)
            .is_some_and(|projected| projected.run.run.tenant != run.tenant)
    }

    fn row(&self, projected: &Projected) -> Row {
        let id = projected.run.run.id;
        let summary = projected.summary(
            self.tasks.get(&id),
            self.quarantines
                .get(&id)
                .and_then(|(_, note)| note.as_ref()),
        );
        let mut tools = BTreeSet::new();
        let mut failed = BTreeSet::new();
        for call in projected.calls().into_values() {
            tools.extend(call.tool);
            failed.extend(call.failed);
        }
        Row {
            summary,
            tools,
            call_errors: failed,
        }
    }

    /// The tenant's runs that match every term of the query.
    fn matching(&self, query: &RunQuery) -> Vec<Row> {
        self.runs
            .values()
            .filter(|projected| projected.run.run.tenant == query.tenant)
            .map(|projected| self.row(projected))
            .filter(|row| row.matches(query))
            .collect()
    }

    fn remove(&mut self, run: RunId) {
        self.runs.remove(&run);
        self.tasks.remove(&run);
        self.quarantines.remove(&run);
        self.changed.remove(&run);
    }

    /// Stamps the run's summary as changed now, later than its previous stamp.
    fn stamp(&mut self, run: RunId) {
        let now = SystemClock.now().as_millis();
        let stamp = self
            .changed
            .get(&run)
            .map_or(now, |previous| now.max(previous.saturating_add(1)));
        self.changed.insert(run, stamp);
    }
}

/// A run's place in an order of `search_runs`: ascending is the page's order.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Position {
    Started(Reverse<RunId>),
    Oldest(RunId),
    Activity(Reverse<Option<u64>>, Reverse<RunId>),
    /// The runs without a wake last, then by run id.
    Wakes(bool, Option<u64>, RunId),
    /// The runs that have not ended last, then newest first.
    Duration(bool, Reverse<Option<u64>>, Reverse<RunId>),
    Calls(Reverse<Option<u64>>, Reverse<RunId>),
}

impl Position {
    fn of(order: RunOrder, cursor: &SearchCursor) -> Self {
        let (key, run) = (cursor.key, cursor.run);
        match order {
            RunOrder::Started => Position::Started(Reverse(run)),
            RunOrder::Oldest => Position::Oldest(run),
            RunOrder::Activity => Position::Activity(Reverse(key), Reverse(run)),
            RunOrder::Wakes => Position::Wakes(key.is_none(), key, run),
            RunOrder::Duration => Position::Duration(key.is_none(), Reverse(key), Reverse(run)),
            RunOrder::Calls => Position::Calls(Reverse(key), Reverse(run)),
        }
    }
}

/// The buckets of a series: from `since`, each `width` wide, up to `until`.
#[derive(Debug, Clone, Copy)]
struct Buckets {
    since: Timestamp,
    width: u64,
    count: usize,
}

impl Buckets {
    fn new(since: Option<Timestamp>, until: Option<Timestamp>, width: Option<u64>) -> Option<Self> {
        let (since, until, width) = (since?, until?, width.filter(|width| *width > 0)?);
        let count = until.0.saturating_sub(since.0).div_ceil(width);
        Some(Self {
            since,
            width,
            count: usize::try_from(count).unwrap_or(usize::MAX),
        })
    }

    fn index(&self, at: Timestamp) -> Option<usize> {
        let offset = at.0.checked_sub(self.since.0)?;
        usize::try_from(offset / self.width)
            .ok()
            .filter(|index| *index < self.count)
    }
}

/// Whether a time is in `[since, until)`; a missing time is in it only without bounds.
fn in_window(at: Option<Timestamp>, since: Option<Timestamp>, until: Option<Timestamp>) -> bool {
    since.is_none_or(|since| at.is_some_and(|at| at >= since))
        && until.is_none_or(|until| at.is_some_and(|at| at < until))
}

fn statuses() -> BTreeMap<RunStatus, u64> {
    STATUSES.iter().map(|status| (*status, 0)).collect()
}

/// The most frequent values first, then by value; at most `top`.
fn ranked(counts: BTreeMap<String, u64>, top: usize) -> Vec<FacetValue> {
    let mut values: Vec<FacetValue> = counts
        .into_iter()
        .map(|(value, runs)| FacetValue { value, runs })
        .collect();
    values.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.value.cmp(&b.value)));
    values.truncate(top);
    values
}

/// The nearest-rank percentile `p` of sorted values.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    let n = sorted.len() as u64;
    let rank = (p * n).div_ceil(100).max(1);
    sorted[(rank - 1) as usize]
}

fn latency(mut values: Vec<u64>) -> Option<Latency> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let count = values.len() as u64;
    Some(Latency {
        count,
        mean_ms: values.iter().sum::<u64>() / count,
        p50_ms: percentile(&values, 50),
        p95_ms: percentile(&values, 95),
        p99_ms: percentile(&values, 99),
        max_ms: values[values.len() - 1],
    })
}

fn facets(rows: &[&Row], top: usize) -> Facets {
    let mut namespaces: BTreeMap<String, u64> = BTreeMap::new();
    let mut statuses: BTreeMap<String, u64> = BTreeMap::new();
    let mut labels: BTreeMap<&str, (u64, BTreeMap<String, u64>)> = BTreeMap::new();
    let mut tools: BTreeMap<String, u64> = BTreeMap::new();
    let mut errors: BTreeMap<String, u64> = BTreeMap::new();
    let mut scripts: BTreeMap<String, u64> = BTreeMap::new();
    let mut waiting: BTreeMap<String, u64> = BTreeMap::new();
    let mut signals: BTreeMap<String, u64> = BTreeMap::new();
    for row in rows {
        let summary = &row.summary;
        *namespaces
            .entry(summary.namespace.as_str().to_owned())
            .or_default() += 1;
        *statuses
            .entry(summary.status.as_str().to_owned())
            .or_default() += 1;
        for (key, value) in &summary.labels {
            let (runs, values) = labels.entry(key).or_default();
            *runs += 1;
            *values.entry(value.clone()).or_default() += 1;
        }
        for tool in &row.tools {
            *tools.entry(tool.clone()).or_default() += 1;
        }
        let mut run_errors: BTreeSet<String> = row
            .call_errors
            .iter()
            .map(|kind| kind.as_str().to_owned())
            .collect();
        run_errors.extend(summary.ending.as_ref().map(|ending| ending.error.clone()));
        for error in run_errors {
            *errors.entry(error).or_default() += 1;
        }
        *scripts.entry(summary.script.to_hex()).or_default() += 1;
        for kind in row.waiting() {
            *waiting.entry(kind.as_str().to_owned()).or_default() += 1;
        }
        for signal in row.signals() {
            *signals.entry(signal.to_owned()).or_default() += 1;
        }
    }
    let mut keys: Vec<(&str, u64, BTreeMap<String, u64>)> = labels
        .into_iter()
        .map(|(key, (runs, values))| (key, runs, values))
        .collect();
    keys.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    keys.truncate(top);
    Facets {
        namespaces: ranked(namespaces, top),
        statuses: ranked(statuses, top),
        labels: keys
            .into_iter()
            .map(|(key, runs, values)| LabelFacet {
                key: key.to_owned(),
                runs,
                values: ranked(values, top),
            })
            .collect(),
        tools: ranked(tools, top),
        errors: ranked(errors, top),
        scripts: ranked(scripts, top),
        waiting: ranked(waiting, top),
        signals: ranked(signals, top),
    }
}

/// One failure group, from its runs (each with an ending and an end).
fn failure_group(rows: &[&Row], buckets: Option<Buckets>) -> Option<FailureGroup> {
    let ended = |row: &Row| row.summary.ended_at.unwrap_or(Timestamp(0));
    let latest = rows
        .iter()
        .max_by_key(|row| (ended(row), row.summary.run.id))?;
    let ending = latest.summary.ending.as_ref()?;
    let mut series = vec![0; buckets.map_or(0, |buckets| buckets.count)];
    let mut lines: BTreeMap<u32, u64> = BTreeMap::new();
    let mut labels: BTreeMap<String, u64> = BTreeMap::new();
    let mut scripts = BTreeSet::new();
    for row in rows {
        if let Some(index) = buckets.and_then(|buckets| buckets.index(ended(row))) {
            series[index] += 1;
        }
        if let Some(line) = row.summary.ending.as_ref().and_then(|ending| ending.line) {
            *lines.entry(line).or_default() += 1;
        }
        for (key, value) in &row.summary.labels {
            *labels.entry(format!("{key}:{value}")).or_default() += 1;
        }
        scripts.insert(row.summary.script);
    }
    // Ascending lines: the first of the most frequent is the lowest.
    let line = lines
        .iter()
        .fold(None, |best: Option<(u32, u64)>, (line, runs)| match best {
            Some((_, most)) if most >= *runs => best,
            _ => Some((*line, *runs)),
        })
        .map(|(line, _)| line);
    Some(FailureGroup {
        fingerprint: ending.fingerprint.clone(),
        signature: ending.signature.clone(),
        error: ending.error.clone(),
        runs: rows.len() as u64,
        series,
        scripts: scripts.len() as u64,
        first_ended_at: rows.iter().map(|row| ended(row)).min()?,
        last_ended_at: ended(latest),
        line,
        labels: ranked(labels, GROUP_LABELS),
        latest: latest.summary.run.id,
        latest_handle: latest.summary.handle(),
        latest_status: latest.summary.status,
        latest_message: ending.message.clone(),
    })
}

impl ReadModel for MemoryReadModel {
    async fn apply(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
        snapshot: &RunSnapshot,
    ) -> Result<(), ReadModelError> {
        let mut snapshot = snapshot.clone();
        for wait in &mut snapshot.waits {
            wait.task = None;
        }
        let mut state = self.lock()?;
        let projected = state.runs.entry(run.run.id).or_insert_with(|| Projected {
            run: run.clone(),
            entries: BTreeMap::new(),
            snapshot: snapshot.clone(),
        });
        projected.run = run.clone();
        if snapshot.history_len > projected.snapshot.history_len {
            projected.snapshot = snapshot;
        }
        for entry in entries {
            projected.entries.insert(entry.index, entry.clone());
        }
        state.stamp(run.run.id);
        Ok(())
    }

    async fn search_runs(
        &self,
        query: &RunQuery,
        order: RunOrder,
        after: Option<&SearchCursor>,
        limit: usize,
    ) -> Result<Vec<RunSummary>, ReadModelError> {
        let state = self.lock()?;
        let after = after.map(|cursor| Position::of(order, cursor));
        let mut found: Vec<(Position, RunSummary)> = state
            .matching(query)
            .into_iter()
            .map(|row| (Position::of(order, &row.summary.cursor(order)), row.summary))
            .filter(|(position, _)| after.as_ref().is_none_or(|after| position > after))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(found
            .into_iter()
            .take(limit)
            .map(|(_, summary)| summary)
            .collect())
    }

    async fn run_stats(
        &self,
        query: &RunQuery,
        window: &StatsWindow,
    ) -> Result<RunStats, ReadModelError> {
        let state = self.lock()?;
        let rows = state.matching(query);
        let buckets = Buckets::new(window.since, window.until, window.bucket_ms);
        let mut stats = RunStats {
            runs: 0,
            statuses: statuses(),
            series: vec![statuses(); buckets.map_or(0, |buckets| buckets.count)],
            durations: None,
            waiting: WaitingFor::ALL.iter().map(|kind| (*kind, 0)).collect(),
            quarantined: 0,
            facets: Facets::default(),
        };
        let mut durations = Vec::new();
        let mut windowed = Vec::new();
        for row in &rows {
            for kind in row.waiting() {
                *stats.waiting.entry(kind).or_default() += 1;
            }
            if row.summary.quarantine.is_some() {
                stats.quarantined += 1;
            }
            if !in_window(row.summary.started_at, window.since, window.until) {
                continue;
            }
            stats.runs += 1;
            *stats.statuses.entry(row.summary.status).or_default() += 1;
            if let Some(index) = buckets
                .zip(row.summary.started_at)
                .and_then(|(buckets, at)| buckets.index(at))
            {
                *stats.series[index].entry(row.summary.status).or_default() += 1;
            }
            durations.extend(row.summary.duration());
            windowed.push(row);
        }
        stats.durations = latency(durations);
        stats.facets = facets(&windowed, window.top);
        Ok(stats)
    }

    async fn failure_groups(
        &self,
        query: &RunQuery,
        window: &FailureWindow,
    ) -> Result<FailureGroups, ReadModelError> {
        let state = self.lock()?;
        let rows = state.matching(query);
        let buckets = Buckets::new(window.since, window.until, window.bucket_ms);
        let mut groups: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
        let mut runs = 0;
        for row in &rows {
            let Some(ending) = &row.summary.ending else {
                continue;
            };
            if row.summary.ended_at.is_none()
                || !in_window(row.summary.ended_at, window.since, window.until)
            {
                continue;
            }
            runs += 1;
            groups.entry(&ending.fingerprint).or_default().push(row);
        }
        let mut top: Vec<FailureGroup> = groups
            .values()
            .filter_map(|rows| failure_group(rows, buckets))
            .collect();
        top.sort_by(|a, b| {
            b.runs
                .cmp(&a.runs)
                .then_with(|| b.last_ended_at.cmp(&a.last_ended_at))
                .then_with(|| a.fingerprint.cmp(&b.fingerprint))
        });
        let groups = top.len() as u64;
        top.truncate(window.limit);
        Ok(FailureGroups { runs, groups, top })
    }

    async fn scripts(&self, filter: &ScriptFilter) -> Result<Vec<ScriptSummary>, ReadModelError> {
        let state = self.lock()?;
        let mut scripts: BTreeMap<Digest, ScriptSummary> = BTreeMap::new();
        for projected in state.runs.values() {
            if projected.run.run.tenant != filter.tenant
                || filter
                    .namespace
                    .as_ref()
                    .is_some_and(|name| projected.run.namespace != *name)
                || filter
                    .digest
                    .is_some_and(|digest| projected.run.script != digest)
                || !filter
                    .labels
                    .iter()
                    .all(|(key, value)| projected.run.labels.get(key) == Some(value))
            {
                continue;
            }
            let Row { summary, tools, .. } = state.row(projected);
            if let Some(since) = filter.since
                && summary.started_at.is_none_or(|at| at < since)
            {
                continue;
            }
            let script = scripts
                .entry(projected.run.script)
                .or_insert_with(|| ScriptSummary {
                    script: projected.run.script,
                    runs: 0,
                    statuses: BTreeMap::new(),
                    tools: BTreeSet::new(),
                    labels: BTreeMap::new(),
                    first_started_at: None,
                    last_started_at: None,
                });
            script.runs += 1;
            *script.statuses.entry(summary.status).or_default() += 1;
            script.tools.extend(tools);
            for (key, value) in &projected.run.labels {
                *script.labels.entry(format!("{key}:{value}")).or_default() += 1;
            }
            if let Some(at) = summary.started_at {
                script.first_started_at = Some(script.first_started_at.map_or(at, |t| t.min(at)));
                script.last_started_at = Some(script.last_started_at.map_or(at, |t| t.max(at)));
            }
        }
        Ok(scripts
            .into_values()
            .filter(|script| {
                filter
                    .tool
                    .as_ref()
                    .is_none_or(|tool| script.tools.contains(tool))
            })
            .collect())
    }

    async fn tool_stats(&self, filter: &ToolStatsFilter) -> Result<Vec<ToolStats>, ReadModelError> {
        let state = self.lock()?;
        let mut stats: BTreeMap<String, (ToolStats, Vec<u64>)> = BTreeMap::new();
        for projected in state.runs.values() {
            if projected.run.run.tenant != filter.tenant
                || filter
                    .namespace
                    .as_ref()
                    .is_some_and(|name| projected.run.namespace != *name)
                || filter
                    .toolset
                    .is_some_and(|toolset| projected.run.toolset != toolset)
            {
                continue;
            }
            let tasks = state.tasks.get(&projected.run.run.id);
            for (seq, call) in projected.calls() {
                let (Some(tool), Some(scheduled_at)) = (call.tool, call.scheduled_at) else {
                    continue;
                };
                if filter.since.is_some_and(|since| scheduled_at < since)
                    || filter.until.is_some_and(|until| scheduled_at >= until)
                {
                    continue;
                }
                let (entry, latencies) = stats.entry(tool.clone()).or_insert_with(|| {
                    let stats = ToolStats {
                        tool,
                        calls: 0,
                        completed: 0,
                        failed: 0,
                        errors_by_kind: BTreeMap::new(),
                        retries: 0,
                        latency: None,
                    };
                    (stats, Vec::new())
                });
                entry.calls += 1;
                let attempt = tasks
                    .and_then(|tasks| tasks.get(&seq))
                    .map(|task| task.attempt);
                entry.retries += u64::from(attempt.unwrap_or(0).saturating_sub(1));
                if call.completed {
                    entry.completed += 1;
                }
                if let Some(kind) = call.failed {
                    entry.failed += 1;
                    *entry.errors_by_kind.entry(kind).or_default() += 1;
                }
                if let Some(completed_at) = call.completed_at {
                    latencies.push(completed_at.0.saturating_sub(scheduled_at.0));
                }
            }
        }
        Ok(stats
            .into_values()
            .map(|(mut entry, latencies)| {
                entry.latency = latency(latencies);
                entry
            })
            .collect())
    }

    async fn observed_shapes(
        &self,
        tenant: &TenantId,
        tools: &[String],
    ) -> Result<BTreeMap<String, Shape>, ReadModelError> {
        let state = self.lock()?;
        let mut shapes: BTreeMap<String, Shape> = BTreeMap::new();
        for projected in state.runs.values().filter(|p| p.run.run.tenant == *tenant) {
            for call in projected.calls().into_values() {
                let (Some(tool), Some(shape)) = (call.tool, call.result) else {
                    continue;
                };
                if !tools.contains(&tool) {
                    continue;
                }
                let merged = shapes.remove(&tool).unwrap_or(Shape::Unknown).merge(shape);
                shapes.insert(tool, merged);
            }
        }
        Ok(shapes)
    }

    async fn record_tasks(
        &self,
        run: &RunRef,
        tasks: &[TaskProgress],
    ) -> Result<(), ReadModelError> {
        let mut state = self.lock()?;
        if state.foreign(run) {
            return Ok(());
        }
        let recorded = state.tasks.entry(run.id).or_default();
        let mut changed = false;
        for task in tasks {
            match recorded.get_mut(&task.seq) {
                Some(kept) if task.rank() <= kept.rank() => {}
                Some(kept) => {
                    *kept = *task;
                    changed = true;
                }
                None => {
                    recorded.insert(task.seq, *task);
                    changed = true;
                }
            }
        }
        if changed {
            state.stamp(run.id);
        }
        Ok(())
    }

    async fn attempts(&self, run: &RunRef) -> Result<BTreeMap<Seq, u32>, ReadModelError> {
        let state = self.lock()?;
        let projected = state
            .runs
            .get(&run.id)
            .is_some_and(|projected| projected.run.run.tenant == run.tenant);
        if !projected {
            return Ok(BTreeMap::new());
        }
        Ok(state
            .tasks
            .get(&run.id)
            .map(|tasks| {
                tasks
                    .iter()
                    .map(|(seq, task)| (*seq, task.attempt))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn record_quarantine(
        &self,
        run: &RunRef,
        quarantine: Option<&QuarantineNote>,
        at: Timestamp,
    ) -> Result<(), ReadModelError> {
        let mut state = self.lock()?;
        if state.foreign(run) {
            return Ok(());
        }
        let newer = state
            .quarantines
            .get(&run.id)
            .is_none_or(|(kept_at, kept)| {
                at > *kept_at || (at == *kept_at && kept.is_none() && quarantine.is_some())
            });
        if newer {
            state.quarantines.insert(run.id, (at, quarantine.cloned()));
            state.stamp(run.id);
        }
        Ok(())
    }

    async fn changed_runs(
        &self,
        tenants: &[TenantId],
        since: u64,
        limit: usize,
    ) -> Result<Vec<ChangedRun>, ReadModelError> {
        let state = self.lock()?;
        let mut changed: Vec<(u64, RunId)> = state
            .runs
            .values()
            .filter(|projected| tenants.contains(&projected.run.run.tenant))
            .filter_map(|projected| {
                let id = projected.run.run.id;
                let at = state.changed.get(&id).copied()?;
                (at >= since).then_some((at, id))
            })
            .collect();
        changed.sort_unstable();
        changed.truncate(limit);
        Ok(changed
            .into_iter()
            .filter_map(|(changed_at, id)| {
                let projected = state.runs.get(&id)?;
                Some(ChangedRun {
                    summary: state.row(projected).summary,
                    changed_at,
                })
            })
            .collect())
    }

    async fn purge(
        &self,
        ended_before: Timestamp,
        tenants: &TenantScope,
    ) -> Result<usize, ReadModelError> {
        let mut state = self.lock()?;
        let purged: Vec<RunId> = state
            .runs
            .iter()
            .filter(|(_, projected)| {
                tenants.contains(&projected.run.run.tenant)
                    && projected.ended().is_some_and(|(_, at)| at < ended_before)
            })
            .map(|(id, _)| *id)
            .collect();
        for run in &purged {
            state.remove(*run);
        }
        Ok(purged.len())
    }

    async fn delete_run(&self, run: &RunRef) -> Result<bool, ReadModelError> {
        let mut state = self.lock()?;
        if state.foreign(run) {
            return Ok(false);
        }
        let projected = state.runs.contains_key(&run.id);
        state.remove(run.id);
        Ok(projected)
    }
}

#[cfg(test)]
mod tests {
    use super::{Buckets, percentile};
    use rewind_core::Timestamp;

    #[test]
    fn percentiles_are_nearest_rank() {
        let values = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&values, 50), 50);
        assert_eq!(percentile(&values, 95), 100);
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[1, 2, 3], 50), 2);
    }

    #[test]
    fn buckets_start_at_since_and_cover_until() {
        let buckets = Buckets::new(Some(Timestamp(1_000)), Some(Timestamp(3_500)), Some(1_000));
        let buckets = buckets.unwrap();
        assert_eq!(buckets.count, 3);
        assert_eq!(buckets.index(Timestamp(999)), None);
        assert_eq!(buckets.index(Timestamp(1_000)), Some(0));
        assert_eq!(buckets.index(Timestamp(3_999)), Some(2));
        assert_eq!(buckets.index(Timestamp(4_000)), None);
        assert!(Buckets::new(None, Some(Timestamp(1)), Some(1)).is_none());
        assert!(Buckets::new(Some(Timestamp(0)), Some(Timestamp(1)), Some(0)).is_none());
    }
}
