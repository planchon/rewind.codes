//! The counters and gauges of a partition (`archi/05-testing-and-observability.md`, "Read
//! model", "Statistics"; `archi/06-storage-and-scale.md`, "Quotas").
//!
//! - **Counters** are exact, per tenant, namespace and UTC hour, written in the batch of what
//!   they count: runs created, started and ended, durations, CPU time, failure groups, calls per
//!   action, runs per code. A window of hours sums them; a day and a month are windows.
//! - **Gauges** count what holds now: the active runs, the runs waiting by what they wait for,
//!   the quarantined runs, the runs kept per code. They follow from the records kept, so
//!   erasing a run takes what it held out of them.
//! - **Shapes** of the results observed per action merge in any order.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rewind_core::{ActionFailureKind, Digest, RunId, RunStatus, Shape, Timestamp};

use super::run::RunHead;
use super::search::IndexedSummary;
use crate::ports::namespaces::{Day, Month, NamespaceName};
use crate::ports::read_model::{Latency, Wait, WaitingFor};
use crate::ports::tenancy::TenantId;

const HOUR_MS: u64 = 3_600_000;

/// The runs a read covers: a tenant's, across its namespaces, or one namespace's.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    Tenant(TenantId),
    Namespace {
        tenant: TenantId,
        namespace: NamespaceName,
    },
}

impl Scope {
    pub fn tenant(&self) -> &TenantId {
        match self {
            Scope::Tenant(tenant) | Scope::Namespace { tenant, .. } => tenant,
        }
    }

    pub fn namespace(&self) -> Option<&NamespaceName> {
        match self {
            Scope::Tenant(_) => None,
            Scope::Namespace { namespace, .. } => Some(namespace),
        }
    }

    /// Whether what belongs to `namespace` of `tenant` is in the scope.
    pub fn contains(&self, tenant: &TenantId, namespace: &NamespaceName) -> bool {
        self.tenant() == tenant && self.namespace().is_none_or(|only| only == namespace)
    }
}

/// A whole UTC hour: hours since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hour(pub u32);

impl Hour {
    /// The hour `at` falls in.
    pub fn of(at: Timestamp) -> Self {
        Self(u32::try_from(at.as_millis() / HOUR_MS).unwrap_or(u32::MAX))
    }

    /// Its first instant.
    pub fn start(self) -> Timestamp {
        Timestamp::from_millis(u64::from(self.0) * HOUR_MS)
    }

    /// The first instant of the next hour.
    pub fn end(self) -> Timestamp {
        self.next().start()
    }

    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// Whole UTC hours, from `from` included to `to` excluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Window {
    pub from: Hour,
    pub to: Hour,
}

impl Window {
    /// The hours from `from` included to `to` excluded.
    pub fn hours(from: Hour, to: Hour) -> Self {
        Self { from, to }
    }

    /// The whole hours that cover the time from `since` to `until`: both bounds rounded out to
    /// the hour, so a window always counts whole hours.
    pub fn covering(since: Timestamp, until: Timestamp) -> Self {
        let from = Hour::of(since);
        let last = Hour::of(until);
        let to = if last.start() == until {
            last
        } else {
            last.next()
        };
        Self {
            from,
            to: to.max(from),
        }
    }

    /// The hours of a UTC day.
    pub fn of_day(day: Day) -> Self {
        Self::covering(day.start(), Day(day.0.saturating_add(1)).start())
    }

    /// The hours of a calendar month, in UTC.
    pub fn of_month(month: Month) -> Self {
        Self::covering(month.start(), month.end())
    }

    pub fn contains(&self, hour: Hour) -> bool {
        self.from <= hour && hour < self.to
    }

    pub fn is_empty(&self) -> bool {
        self.to <= self.from
    }

    /// Its first instant.
    pub fn start(&self) -> Timestamp {
        self.from.start()
    }

    /// The first instant after it.
    pub fn end(&self) -> Timestamp {
        self.to.start()
    }
}

/// A mergeable sketch of durations, in milliseconds: counts in buckets of about 6 % of their
/// value (the exact value below 16 ms), with the count, the sum and the maximum. Sketches add up
/// in any order, so the hours of a window sum into one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sketch {
    pub count: u64,
    pub sum_ms: u64,
    pub max_ms: u64,
    /// By bucket: [`Sketch::bucket`] of the values counted.
    pub buckets: BTreeMap<u16, u64>,
}

impl Sketch {
    /// A sketch of these values.
    pub fn of(values: impl IntoIterator<Item = u64>) -> Self {
        let mut sketch = Self::default();
        for value in values {
            sketch.record(value);
        }
        sketch
    }

    /// The bucket of a value: the value itself below 16; above, 16 buckets per power of two.
    pub fn bucket(value_ms: u64) -> u16 {
        if value_ms < 16 {
            return u16::try_from(value_ms).unwrap_or(0);
        }
        let exponent = 63 - value_ms.leading_zeros();
        let mantissa = (value_ms >> (exponent - 4)) & 0xF;
        u16::try_from(16 * u64::from(exponent - 3) + mantissa).unwrap_or(u16::MAX)
    }

    /// The smallest value of a bucket.
    pub fn bucket_floor(bucket: u16) -> u64 {
        if bucket < 16 {
            return u64::from(bucket);
        }
        let exponent = u32::from(bucket / 16) + 3;
        let mantissa = u64::from(bucket % 16);
        (16 + mantissa) << (exponent - 4)
    }

    pub fn record(&mut self, value_ms: u64) {
        self.count += 1;
        self.sum_ms = self.sum_ms.saturating_add(value_ms);
        self.max_ms = self.max_ms.max(value_ms);
        *self.buckets.entry(Self::bucket(value_ms)).or_default() += 1;
    }

    pub fn add(&mut self, other: &Sketch) {
        self.count += other.count;
        self.sum_ms = self.sum_ms.saturating_add(other.sum_ms);
        self.max_ms = self.max_ms.max(other.max_ms);
        for (bucket, count) in &other.buckets {
            *self.buckets.entry(*bucket).or_default() += count;
        }
    }

    /// The `p`-th percentile, nearest rank: the floor of the bucket of the value at position
    /// `ceil(p × count / 100)`, at most the maximum. `None` for an empty sketch.
    pub fn percentile(&self, p: u64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let rank = (p.min(100) * self.count).div_ceil(100).max(1);
        let mut seen = 0;
        for (bucket, count) in &self.buckets {
            seen += count;
            if seen >= rank {
                return Some(Self::bucket_floor(*bucket).min(self.max_ms));
            }
        }
        Some(self.max_ms)
    }

    /// The latency the statistics answer; `None` for an empty sketch.
    pub fn latency(&self) -> Option<Latency> {
        Some(Latency {
            count: self.count,
            mean_ms: self.sum_ms.checked_div(self.count)?,
            p50_ms: self.percentile(50)?,
            p95_ms: self.percentile(95)?,
            p99_ms: self.percentile(99)?,
            max_ms: self.max_ms,
        })
    }
}

/// Runs started and ended, by status.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunCounts {
    /// Runs whose `run_started` was committed.
    pub started: u64,
    pub ended: BTreeMap<RunStatus, u64>,
}

impl RunCounts {
    pub fn add(&mut self, other: &RunCounts) {
        self.started += other.started;
        add_counts(&mut self.ended, &other.ended);
    }
}

/// The latest runs of a failure group a counter keeps: [`LATEST_RUNS`] at most.
pub const LATEST_RUNS: usize = 5;

/// A run that ended badly, as its failure group lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatestRun {
    pub run: RunId,
    pub ended_at: Timestamp,
    pub status: RunStatus,
    pub message: String,
}

/// The runs of a failure group (one fingerprint).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FailureCounts {
    /// The error's name and its message with the parts that vary replaced.
    pub signature: String,
    /// The error's name, or the abort's kind.
    pub error: String,
    pub runs: u64,
    /// The workflows its runs ran.
    pub scripts: BTreeMap<Digest, u64>,
    /// The lines of `workflow.ts` its runs ended at.
    pub lines: BTreeMap<u32, u64>,
    /// The latest runs, the latest end first (the highest run id on a tie).
    pub latest: Vec<LatestRun>,
}

impl FailureCounts {
    pub fn add(&mut self, other: &FailureCounts) {
        if self.signature.is_empty() {
            self.signature.clone_from(&other.signature);
        }
        if self.error.is_empty() {
            self.error.clone_from(&other.error);
        }
        self.runs += other.runs;
        add_counts(&mut self.scripts, &other.scripts);
        add_counts(&mut self.lines, &other.lines);
        self.latest.extend(other.latest.iter().cloned());
        self.latest
            .sort_by_key(|latest| std::cmp::Reverse((latest.ended_at, latest.run)));
        self.latest.dedup_by_key(|latest| latest.run);
        self.latest.truncate(LATEST_RUNS);
    }
}

/// The calls of one action.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionCounts {
    /// Calls scheduled.
    pub calls: u64,
    pub completed: u64,
    pub failed: u64,
    pub errors: BTreeMap<ActionFailureKind, u64>,
    /// Deliveries beyond the first.
    pub redeliveries: u64,
    /// From the call to its outcome, in logical time.
    pub latency: Sketch,
}

impl ActionCounts {
    pub fn add(&mut self, other: &ActionCounts) {
        self.calls += other.calls;
        self.completed += other.completed;
        self.failed += other.failed;
        add_counts(&mut self.errors, &other.errors);
        self.redeliveries += other.redeliveries;
        self.latency.add(&other.latency);
    }
}

/// The runs of one code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptCounts {
    pub runs: RunCounts,
    /// The actions its runs called.
    pub actions: BTreeSet<String>,
    pub first_started_at: Option<Timestamp>,
    pub last_started_at: Option<Timestamp>,
}

impl ScriptCounts {
    pub fn add(&mut self, other: &ScriptCounts) {
        self.runs.add(&other.runs);
        self.actions.extend(other.actions.iter().cloned());
        self.first_started_at = earliest(self.first_started_at, other.first_started_at);
        self.last_started_at = self.last_started_at.max(other.last_started_at);
    }
}

fn earliest(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// The counters of one namespace and hour, or their sum over a scope and a window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Runs created, forks included: what `runsPerMonth` counts.
    pub created: u64,
    pub runs: RunCounts,
    /// From start to end, of the runs that ended.
    pub durations: Sketch,
    /// CPU time of the isolates running the workflows, replays excluded: what `cpuPerMonth`
    /// counts.
    pub cpu: Duration,
    /// The runs of each workflow, by name.
    pub workflows: BTreeMap<String, RunCounts>,
    /// The runs of each namespace. A delta leaves it empty: the store fills it with the delta's
    /// namespace ([`Counters::add_for`]).
    pub namespaces: BTreeMap<NamespaceName, RunCounts>,
    /// Failure groups, by fingerprint.
    pub failures: BTreeMap<String, FailureCounts>,
    /// Calls, by action.
    pub actions: BTreeMap<String, ActionCounts>,
    /// Runs, by code.
    pub scripts: BTreeMap<Digest, ScriptCounts>,
}

impl Counters {
    /// Adds `other`, its `namespaces` included.
    pub fn add(&mut self, other: &Counters) {
        self.add_without_namespaces(other);
        for (namespace, runs) in &other.namespaces {
            self.namespaces
                .entry(namespace.clone())
                .or_default()
                .add(runs);
        }
    }

    /// Adds the counts of `namespace`, and its runs under it in `namespaces`: how a store sums
    /// the hours of a scope.
    pub fn add_for(&mut self, namespace: &NamespaceName, counts: &Counters) {
        self.add_without_namespaces(counts);
        self.namespaces
            .entry(namespace.clone())
            .or_default()
            .add(&counts.runs);
    }

    /// What the counts of an erased namespace leave in its tenant's: what it consumed (runs
    /// created, started and ended, their durations, CPU time, the runs of each workflow), not
    /// its failures, actions or workflows.
    pub fn consumed(&self) -> Counters {
        Counters {
            created: self.created,
            runs: self.runs.clone(),
            durations: self.durations.clone(),
            cpu: self.cpu,
            workflows: self.workflows.clone(),
            ..Counters::default()
        }
    }

    fn add_without_namespaces(&mut self, other: &Counters) {
        self.created += other.created;
        self.runs.add(&other.runs);
        self.durations.add(&other.durations);
        self.cpu = self.cpu.saturating_add(other.cpu);
        for (workflow, runs) in &other.workflows {
            self.workflows
                .entry(workflow.clone())
                .or_default()
                .add(runs);
        }
        for (fingerprint, failures) in &other.failures {
            self.failures
                .entry(fingerprint.clone())
                .or_default()
                .add(failures);
        }
        for (action, calls) in &other.actions {
            self.actions.entry(action.clone()).or_default().add(calls);
        }
        for (code, runs) in &other.scripts {
            self.scripts.entry(*code).or_default().add(runs);
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Counters::default()
    }
}

fn add_counts<K: Ord + Clone>(into: &mut BTreeMap<K, u64>, from: &BTreeMap<K, u64>) {
    for (key, count) in from {
        *into.entry(key.clone()).or_default() += count;
    }
}

/// What a batch adds to the counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CounterDelta {
    /// Counts added to a namespace's hour.
    Hour {
        tenant: TenantId,
        namespace: NamespaceName,
        hour: Hour,
        counts: Counters,
    },
    /// The shape of an action's results, merged into the namespace's ([`Shape::merge`]).
    Shape {
        tenant: TenantId,
        namespace: NamespaceName,
        action: String,
        shape: Shape,
    },
}

/// What holds now among the runs of a scope. Kinds and workflows with nothing are left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gauges {
    /// Runs that have not ended (pending or active): what `activeRuns` counts.
    pub active_runs: u64,
    /// Active runs by what they wait for; a run waiting for several kinds counts once in each.
    pub waiting: BTreeMap<WaitingFor, u64>,
    /// Runs quarantined that have not ended.
    pub quarantined: u64,
    /// Runs kept, by code, ended or not: the number of workflows is what `scripts` counts.
    pub scripts: BTreeMap<Digest, u64>,
}

impl Gauges {
    /// Counts one run kept: its head, and its summary once it has one.
    pub fn count(&mut self, head: &RunHead, summary: Option<&IndexedSummary>) {
        *self.scripts.entry(head.code).or_default() += 1;
        if head.is_ended() {
            return;
        }
        self.active_runs += 1;
        if head.quarantine.is_some() {
            self.quarantined += 1;
        }
        let waiting: BTreeSet<WaitingFor> = summary
            .filter(|indexed| indexed.summary.status == RunStatus::Active)
            .map(|indexed| {
                indexed
                    .summary
                    .waits
                    .iter()
                    .map(Wait::waiting_for)
                    .collect()
            })
            .unwrap_or_default();
        for kind in waiting {
            *self.waiting.entry(kind).or_default() += 1;
        }
    }

    pub fn add(&mut self, other: &Gauges) {
        self.active_runs += other.active_runs;
        add_counts(&mut self.waiting, &other.waiting);
        self.quarantined += other.quarantined;
        add_counts(&mut self.scripts, &other.scripts);
    }

    /// The runs waiting for `kind`.
    pub fn waiting_for(&self, kind: WaitingFor) -> u64 {
        self.waiting.get(&kind).copied().unwrap_or(0)
    }

    /// The distinct workflows with a run kept.
    pub fn distinct_scripts(&self) -> u64 {
        u64::try_from(self.scripts.len()).unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_rounded_out_to_whole_hours() {
        let hour = Hour(500_000);
        let mid = Timestamp::from_millis(hour.start().as_millis() + 1_800_000);
        assert_eq!(Hour::of(mid), hour);
        assert_eq!(
            Window::covering(mid, Timestamp::from_millis(mid.as_millis() + 1)),
            Window::hours(hour, hour.next())
        );
        assert_eq!(
            Window::covering(hour.start(), hour.next().start()),
            Window::hours(hour, hour.next()),
            "bounds on the hour are kept"
        );
        assert_eq!(
            Window::covering(mid, hour.next().next().start()),
            Window::hours(hour, hour.next().next())
        );
        assert!(Window::covering(hour.start(), hour.start()).is_empty());
        let window = Window::of_day(Day(20_000));
        assert_eq!(window.to.0 - window.from.0, 24);
        assert_eq!(window.start(), Day(20_000).start());
        let january = Window::of_month(Month::new(2026, 1).unwrap());
        assert_eq!(january.to.0 - january.from.0, 31 * 24);
        assert!(january.contains(january.from) && !january.contains(january.to));
    }

    #[test]
    fn sketches_bucket_within_a_sixteenth_and_merge_in_any_order() {
        for value in [0, 1, 15, 16, 17, 31, 32, 100, 1_000, 65_537, u64::MAX / 3] {
            let floor = Sketch::bucket_floor(Sketch::bucket(value));
            assert!(floor <= value, "{value}");
            assert!(value - floor <= value / 16, "{value}: {floor}");
        }
        for value in 0..16 {
            assert_eq!(Sketch::bucket_floor(Sketch::bucket(value)), value);
        }
        let (a, b) = (Sketch::of([1, 2, 3]), Sketch::of([100, 200]));
        let mut ab = a.clone();
        ab.add(&b);
        let mut ba = b.clone();
        ba.add(&a);
        assert_eq!(ab, ba);
        assert_eq!(ab, Sketch::of([1, 2, 3, 100, 200]));
        let latency = ab.latency().unwrap();
        assert_eq!(
            (latency.count, latency.mean_ms, latency.max_ms),
            (5, 61, 200)
        );
        assert_eq!(latency.p50_ms, 3);
        assert!(latency.p99_ms <= 200 && latency.p99_ms >= 188);
        assert_eq!(Sketch::default().latency(), None);
    }

    #[test]
    fn counters_add_up_and_keep_the_latest_failed_runs() {
        let alice = NamespaceName::new("alice").unwrap();
        let failure = |run: u128, at: u64| FailureCounts {
            signature: "TypeError: <n>".into(),
            error: "TypeError".into(),
            runs: 1,
            scripts: BTreeMap::from([(Digest::of(b"s"), 1)]),
            lines: BTreeMap::from([(3, 1)]),
            latest: vec![LatestRun {
                run: RunId::from_u128(run),
                ended_at: Timestamp::from_millis(at),
                status: RunStatus::Failed,
                message: "x".into(),
            }],
        };
        let mut total = Counters::default();
        for n in 0..7 {
            let counts = Counters {
                created: 1,
                failures: BTreeMap::from([("f".to_owned(), failure(n, u64::try_from(n).unwrap()))]),
                ..Counters::default()
            };
            total.add_for(&alice, &counts);
        }
        assert_eq!(total.created, 7);
        let group = &total.failures["f"];
        assert_eq!(group.runs, 7);
        assert_eq!(group.lines[&3], 7);
        assert_eq!(group.latest.len(), LATEST_RUNS);
        assert_eq!(group.latest[0].run, RunId::from_u128(6), "the latest first");
        assert_eq!(total.namespaces.len(), 1);
    }
}
