//! Statistics, facets and failure groups, counted in Rust over the runs a query matches: the
//! tenant's at most, which the embedded server's single machine holds. They follow the
//! in-memory reference (`rewind_engine::adapters::MemoryReadModel`) rule for rule.

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{RunStatus, Timestamp, ToolFailureKind};
use rewind_engine::ports::{
    FacetValue, Facets, FailureGroup, FailureGroups, FailureWindow, LabelFacet, Latency, RunStats,
    RunSummary, StatsWindow, Wait, WaitKind, WaitingFor,
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

/// A run a query matched: its summary and what its calls did.
#[derive(Debug)]
pub(super) struct Found {
    pub(super) summary: RunSummary,
    /// The tools it scheduled calls to.
    pub(super) tools: BTreeSet<String>,
    /// The kinds its failed calls failed with.
    pub(super) failed: BTreeSet<ToolFailureKind>,
}

impl Found {
    /// What the run waits for: its waits' kinds, while it is active.
    fn waiting(&self) -> BTreeSet<WaitingFor> {
        if self.summary.status != RunStatus::Active {
            return BTreeSet::new();
        }
        self.summary.waits.iter().map(Wait::waiting_for).collect()
    }

    /// The signals the run waits for.
    fn signals(&self) -> BTreeSet<&str> {
        self.summary
            .waits
            .iter()
            .filter(|wait| wait.kind == WaitKind::Signal)
            .filter_map(|wait| wait.name.as_deref())
            .collect()
    }

    /// From start to end, once ended.
    fn duration(&self) -> Option<u64> {
        let (started, ended) = (self.summary.started_at?, self.summary.ended_at?);
        Some(ended.0.saturating_sub(started.0))
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

/// The most frequent values first, then by value (byte order); at most `top`.
fn ranked(counts: BTreeMap<String, u64>, top: usize) -> Vec<FacetValue> {
    let mut values: Vec<FacetValue> = counts
        .into_iter()
        .map(|(value, runs)| FacetValue { value, runs })
        .collect();
    values.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.value.cmp(&b.value)));
    values.truncate(top);
    values
}

/// The nearest-rank percentile `p` of sorted values: the one at `ceil(p × n / 100)`.
pub(super) fn percentile(sorted: &[u64], p: u64) -> u64 {
    let n = sorted.len() as u64;
    let rank = (p * n).div_ceil(100).max(1);
    sorted[(rank - 1) as usize]
}

/// The count, mean, percentiles and maximum of `values`; `None` without any.
pub(super) fn latency(mut values: Vec<u64>) -> Option<Latency> {
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

/// Counts, series and durations of the runs started in the window, with their facets; the
/// runs waiting and quarantined now, whenever they started.
pub(super) fn run_stats(found: &[Found], window: &StatsWindow) -> RunStats {
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
    for run in found {
        for kind in run.waiting() {
            *stats.waiting.entry(kind).or_default() += 1;
        }
        if run.summary.quarantine.is_some() {
            stats.quarantined += 1;
        }
        let started_at = run.summary.started_at;
        if !in_window(started_at, window.since, window.until) {
            continue;
        }
        stats.runs += 1;
        *stats.statuses.entry(run.summary.status).or_default() += 1;
        if let Some(index) = buckets
            .zip(started_at)
            .and_then(|(buckets, at)| buckets.index(at))
        {
            *stats.series[index].entry(run.summary.status).or_default() += 1;
        }
        durations.extend(run.duration());
        windowed.push(run);
    }
    stats.durations = latency(durations);
    stats.facets = facets(&windowed, window.top);
    stats
}

fn facets(found: &[&Found], top: usize) -> Facets {
    let mut statuses: BTreeMap<String, u64> = BTreeMap::new();
    let mut labels: BTreeMap<&str, (u64, BTreeMap<String, u64>)> = BTreeMap::new();
    let mut tools: BTreeMap<String, u64> = BTreeMap::new();
    let mut errors: BTreeMap<String, u64> = BTreeMap::new();
    let mut scripts: BTreeMap<String, u64> = BTreeMap::new();
    let mut waiting: BTreeMap<String, u64> = BTreeMap::new();
    let mut signals: BTreeMap<String, u64> = BTreeMap::new();
    for run in found {
        let summary = &run.summary;
        *statuses
            .entry(summary.status.as_str().to_owned())
            .or_default() += 1;
        for (key, value) in &summary.labels {
            let (runs, values) = labels.entry(key).or_default();
            *runs += 1;
            *values.entry(value.clone()).or_default() += 1;
        }
        for tool in &run.tools {
            *tools.entry(tool.clone()).or_default() += 1;
        }
        // A call's kind and the ending's name count once each per run.
        let mut run_errors: BTreeSet<String> = run
            .failed
            .iter()
            .map(|kind| kind.as_str().to_owned())
            .collect();
        run_errors.extend(summary.ending.as_ref().map(|ending| ending.error.clone()));
        for error in run_errors {
            *errors.entry(error).or_default() += 1;
        }
        *scripts.entry(summary.script.to_hex()).or_default() += 1;
        for kind in run.waiting() {
            *waiting.entry(kind.as_str().to_owned()).or_default() += 1;
        }
        for signal in run.signals() {
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

/// The runs with an ending that ended in the window, by fingerprint: the most runs first, then
/// the latest ending, then by fingerprint.
pub(super) fn failure_groups(found: &[Found], window: &FailureWindow) -> FailureGroups {
    let buckets = Buckets::new(window.since, window.until, window.bucket_ms);
    let mut groups: BTreeMap<&str, Vec<&Found>> = BTreeMap::new();
    let mut runs = 0;
    for run in found {
        let Some(ending) = &run.summary.ending else {
            continue;
        };
        let ended_at = run.summary.ended_at;
        if ended_at.is_none() || !in_window(ended_at, window.since, window.until) {
            continue;
        }
        runs += 1;
        groups.entry(&ending.fingerprint).or_default().push(run);
    }
    let mut top: Vec<FailureGroup> = groups
        .values()
        .filter_map(|runs| failure_group(runs, buckets))
        .collect();
    top.sort_by(|a, b| {
        b.runs
            .cmp(&a.runs)
            .then_with(|| b.last_ended_at.cmp(&a.last_ended_at))
            .then_with(|| a.fingerprint.cmp(&b.fingerprint))
    });
    let groups = top.len() as u64;
    top.truncate(window.limit);
    FailureGroups { runs, groups, top }
}

/// One failure group, from its runs (each with an ending and an end).
fn failure_group(runs: &[&Found], buckets: Option<Buckets>) -> Option<FailureGroup> {
    let ended = |run: &Found| run.summary.ended_at.unwrap_or(Timestamp(0));
    let latest = runs
        .iter()
        .max_by_key(|run| (ended(run), run.summary.run.id))?;
    let ending = latest.summary.ending.as_ref()?;
    let mut series = vec![0; buckets.map_or(0, |buckets| buckets.count)];
    let mut lines: BTreeMap<u32, u64> = BTreeMap::new();
    let mut labels: BTreeMap<String, u64> = BTreeMap::new();
    let mut scripts = BTreeSet::new();
    for run in runs {
        if let Some(index) = buckets.and_then(|buckets| buckets.index(ended(run))) {
            series[index] += 1;
        }
        if let Some(line) = run.summary.ending.as_ref().and_then(|ending| ending.line) {
            *lines.entry(line).or_default() += 1;
        }
        for (key, value) in &run.summary.labels {
            *labels.entry(format!("{key}:{value}")).or_default() += 1;
        }
        scripts.insert(run.summary.script);
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
        runs: runs.len() as u64,
        series,
        scripts: scripts.len() as u64,
        first_ended_at: runs.iter().map(|run| ended(run)).min()?,
        last_ended_at: ended(latest),
        line,
        labels: ranked(labels, GROUP_LABELS),
        latest: latest.summary.run.id,
        latest_status: latest.summary.status,
        latest_message: ending.message.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::{Buckets, in_window, percentile};
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
        let buckets = buckets.expect("buckets");
        assert_eq!(buckets.count, 3, "ceil(2500 / 1000)");
        assert_eq!(buckets.index(Timestamp(999)), None);
        assert_eq!(buckets.index(Timestamp(1_000)), Some(0));
        assert_eq!(buckets.index(Timestamp(3_999)), Some(2));
        assert_eq!(buckets.index(Timestamp(4_000)), None);
        assert!(Buckets::new(None, Some(Timestamp(1)), Some(1)).is_none());
        assert!(Buckets::new(Some(Timestamp(0)), Some(Timestamp(1)), Some(0)).is_none());
    }

    #[test]
    fn a_window_is_half_open_and_a_missing_time_is_outside_any_bound() {
        let (since, until) = (Some(Timestamp(10)), Some(Timestamp(20)));
        assert!(in_window(Some(Timestamp(10)), since, until));
        assert!(!in_window(Some(Timestamp(20)), since, until));
        assert!(!in_window(None, since, None));
        assert!(in_window(None, None, None));
    }
}
