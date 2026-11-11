use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use ptc_core::{
    Command, Completion, Event, HistoryEntry, Lifecycle, Payload, RunId, RunStatus, ScriptError,
    Seq, Shape, Timestamp, ToolFailureKind,
};

use crate::ports::{
    Latency, ProjectedRun, ReadModel, ReadModelError, RunRef, RunSearch, RunSummary, ScriptFilter,
    ScriptSummary, TenantId, TenantScope, ToolStats, ToolStatsFilter,
};

/// `ReadModel` in memory: the projected entries of every run, keyed by `(run, index)`, from
/// which every query is computed. It passes the conformance suite.
#[derive(Debug, Default)]
pub struct MemoryReadModel {
    runs: Mutex<BTreeMap<RunId, Projected>>,
}

#[derive(Debug)]
struct Projected {
    run: ProjectedRun,
    entries: BTreeMap<u64, HistoryEntry>,
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

impl MemoryReadModel {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<RunId, Projected>>, ReadModelError> {
        self.runs
            .lock()
            .map_err(|_| ReadModelError::Backend("the memory read model's lock is poisoned".into()))
    }
}

impl Projected {
    fn summary(&self) -> RunSummary {
        let mut status = RunStatus::Active;
        let mut started_at = None;
        let mut ended_at = None;
        let mut tool_calls = 0;
        let mut errors = 0;
        for entry in self.entries.values() {
            match &entry.event {
                Event::Lifecycle(Lifecycle::RunStarted(_)) => started_at = Some(entry.recorded_at),
                Event::Lifecycle(Lifecycle::RunAborted(_)) => {
                    status = RunStatus::Aborted;
                    ended_at = Some(entry.recorded_at);
                }
                Event::Command(Command::CompleteRun { .. }) => {
                    status = RunStatus::Completed;
                    ended_at = Some(entry.recorded_at);
                }
                Event::Command(Command::FailRun { error }) => {
                    status = if error.name == ScriptError::CANCELLED {
                        RunStatus::Cancelled
                    } else {
                        RunStatus::Failed
                    };
                    ended_at = Some(entry.recorded_at);
                }
                Event::Command(Command::ScheduleToolCall { .. }) => tool_calls += 1,
                Event::Completion(Completion::ToolCallFailed { .. }) => errors += 1,
                _ => {}
            }
        }
        RunSummary {
            run: self.run.run.clone(),
            script: self.run.script,
            toolset: self.run.toolset,
            status,
            labels: self.run.labels.clone(),
            started_at,
            ended_at,
            last_activity_at: self
                .entries
                .values()
                .map(|entry| entry.recorded_at)
                .max()
                .unwrap_or(Timestamp(0)),
            tool_calls,
            errors,
            history_len: self.entries.keys().next_back().map_or(0, |last| last + 1),
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

/// The nearest-rank percentile `p` of sorted values.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    let n = sorted.len() as u64;
    let rank = (p * n).div_ceil(100).max(1);
    sorted[(rank - 1) as usize]
}

impl ReadModel for MemoryReadModel {
    async fn apply(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
    ) -> Result<(), ReadModelError> {
        let mut runs = self.lock()?;
        let projected = runs.entry(run.run.id).or_insert_with(|| Projected {
            run: run.clone(),
            entries: BTreeMap::new(),
        });
        projected.run = run.clone();
        for entry in entries {
            projected.entries.insert(entry.index, entry.clone());
        }
        Ok(())
    }

    async fn search_runs(&self, search: &RunSearch) -> Result<Vec<RunSummary>, ReadModelError> {
        let runs = self.lock()?;
        let mut found: Vec<RunSummary> =
            runs.values()
                .rev()
                .filter(|projected| projected.run.run.tenant == search.tenant)
                .filter(|projected| {
                    search
                        .before
                        .is_none_or(|before| projected.run.run.id < before)
                })
                .filter(|projected| {
                    search
                        .script
                        .is_none_or(|script| projected.run.script == script)
                })
                .filter(|projected| {
                    search
                        .toolset
                        .is_none_or(|toolset| projected.run.toolset == toolset)
                })
                .filter(|projected| {
                    search
                        .labels
                        .iter()
                        .all(|(key, value)| projected.run.labels.get(key) == Some(value))
                })
                .filter(|projected| {
                    let calls = projected.calls();
                    search.tool.as_ref().is_none_or(|tool| {
                        calls.values().any(|call| call.tool.as_ref() == Some(tool))
                    }) && search
                        .error_kind
                        .is_none_or(|kind| calls.values().any(|call| call.failed == Some(kind)))
                })
                .map(Projected::summary)
                .filter(|summary| search.status.is_none_or(|status| summary.status == status))
                .filter(|summary| {
                    search
                        .started_after
                        .is_none_or(|after| summary.started_at.is_some_and(|at| at >= after))
                        && search
                            .started_before
                            .is_none_or(|before| summary.started_at.is_some_and(|at| at < before))
                })
                .collect();
        found.truncate(search.limit);
        Ok(found)
    }

    async fn scripts(&self, filter: &ScriptFilter) -> Result<Vec<ScriptSummary>, ReadModelError> {
        let runs = self.lock()?;
        let mut scripts: BTreeMap<ptc_core::Digest, ScriptSummary> = BTreeMap::new();
        for projected in runs.values() {
            if projected.run.run.tenant != filter.tenant {
                continue;
            }
            let summary = projected.summary();
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
                    first_started_at: None,
                    last_started_at: None,
                });
            script.runs += 1;
            *script.statuses.entry(summary.status).or_default() += 1;
            script
                .tools
                .extend(projected.calls().into_values().filter_map(|call| call.tool));
            if let Some(at) = summary.started_at {
                script.first_started_at = Some(script.first_started_at.map_or(at, |t| t.min(at)));
                script.last_started_at = Some(script.last_started_at.map_or(at, |t| t.max(at)));
            }
        }
        Ok(scripts.into_values().collect())
    }

    async fn tool_stats(&self, filter: &ToolStatsFilter) -> Result<Vec<ToolStats>, ReadModelError> {
        let runs = self.lock()?;
        let mut stats: BTreeMap<String, (ToolStats, Vec<u64>)> = BTreeMap::new();
        for projected in runs.values() {
            if projected.run.run.tenant != filter.tenant
                || filter
                    .toolset
                    .is_some_and(|toolset| projected.run.toolset != toolset)
            {
                continue;
            }
            for call in projected.calls().into_values() {
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
                        latency: None,
                    };
                    (stats, Vec::new())
                });
                entry.calls += 1;
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
            .map(|(mut entry, mut latencies)| {
                latencies.sort_unstable();
                if !latencies.is_empty() {
                    let count = latencies.len() as u64;
                    entry.latency = Some(Latency {
                        count,
                        mean_ms: latencies.iter().sum::<u64>() / count,
                        p50_ms: percentile(&latencies, 50),
                        p95_ms: percentile(&latencies, 95),
                        max_ms: latencies[latencies.len() - 1],
                    });
                }
                entry
            })
            .collect())
    }

    async fn observed_shapes(
        &self,
        tenant: &TenantId,
        tools: &[String],
    ) -> Result<BTreeMap<String, Shape>, ReadModelError> {
        let runs = self.lock()?;
        let mut shapes: BTreeMap<String, Shape> = BTreeMap::new();
        for projected in runs.values().filter(|p| p.run.run.tenant == *tenant) {
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

    async fn purge(
        &self,
        ended_before: Timestamp,
        tenants: &TenantScope,
    ) -> Result<usize, ReadModelError> {
        let mut runs = self.lock()?;
        let before = runs.len();
        runs.retain(|_, projected| {
            let ended_at = projected
                .entries
                .values()
                .find(|entry| status_of(entry).is_some())
                .map(|entry| entry.recorded_at);
            !(tenants.contains(&projected.run.run.tenant)
                && ended_at.is_some_and(|at| at < ended_before))
        });
        Ok(before - runs.len())
    }

    async fn delete_run(&self, run: &RunRef) -> Result<bool, ReadModelError> {
        let mut runs = self.lock()?;
        if runs
            .get(&run.id)
            .is_some_and(|projected| projected.run.run.tenant == run.tenant)
        {
            runs.remove(&run.id);
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentiles_are_nearest_rank() {
        let values = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&values, 50), 50);
        assert_eq!(percentile(&values, 95), 100);
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[1, 2, 3], 50), 2);
    }
}
