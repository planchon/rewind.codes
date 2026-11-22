//! The change hub: the runs whose summary changes, read from the read model and sent to the
//! change streams (`GET /v1/changes`, `http::changes`).
//!
//! **Temporary by design** (`archi/08-decisions.md`, "Taken for the live dashboard"): the
//! architecture is moving to events, which will replace how the server learns of a change; the
//! stream's contract stays. Until then, ONE loop per process, started when the first stream opens
//! and stopped when the last one closes, reads the summaries stamped since its last pass
//! (`ReadModel::changed_runs`) for the tenants with an open stream: every second, and sooner when
//! the process's wake counter moves or this process records a change of the read model (the
//! projector's batches, task states, quarantines). It does not scale past a few api processes:
//! each reads every change of the tenants it serves.
//!
//! - **Overlap.** A pass reads from the latest stamp it read less [`OVERLAP_MS`], for the commits
//!   that land late, and drops the `(run, changed_at)` pairs it already sent; the store stamps
//!   each write later than the previous one, so a run that changes again is sent again. The
//!   cursor moves with the stamps read, the store's clock, never with this process's, except at
//!   the first pass, which reads from now less the overlap. A stamp read [`SETTLED`] ago (twice
//!   the overlap) is a floor no pass reads before again: a quiet organization's passes read
//!   only its latest change, not the whole overlap.
//! - **Once per pass.** Each summary read becomes the API's once, as `GET /v1/runs` lists it
//!   (merged with the run's own record: `http::runs::listed`, a few runs at once), in JSON; the
//!   pass is sent to every stream as one batch, and each stream keeps the runs its caller may
//!   see. A run that cannot be read is left out, and logged.
//! - **Limit.** A pass reads its limit (500 runs) of changes it has not sent, beyond those it
//!   sent and reads again (at most [`REREAD`] limits of them). One that reads that many sends
//!   `resync`, instead of their runs, to the streams of their tenants; the next pass follows
//!   at once and goes on after them. A pass that reads only runs already sent, more than it
//!   reads again, goes on after the last stamp it read. Nothing is sent twice, and a pass that
//!   reads nothing new sends nothing. A stream that falls behind the batches gets `resync`.
//! - **Failures.** After [`FAILURES`] passes in a row that fail (the store cannot be read), the
//!   streams get `resync` and close: the clients read at their former rates and reconnect.
//!
//! What the loop misses (a commit later than the overlap, a deletion) the dashboard's refresh,
//! every 60 s, fixes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use rewind_api::events::ChangeEvent;
use rewind_core::RunId;
use rewind_engine::adapters::SystemClock;
use rewind_engine::ports::{ChangedRun, Clock, NamespaceName, ReadModel, TenantId};
use tokio::sync::{Notify, broadcast};

use crate::app::App;
use crate::error::ApiError;
use crate::http::runs::listed;

/// The most runs one pass reads by default.
pub(crate) const PER_PASS: usize = 500;

/// How far back a pass reads before the latest stamp it read, in milliseconds: commits that
/// land late.
pub(crate) const OVERLAP_MS: u64 = 2_000;

/// How long after it was read a stamp becomes the floor no pass reads before: twice the
/// overlap, so that a commit stamped before it, landing at most the overlap late, was read by a
/// pass in between (passes run at least every second).
const SETTLED: Duration = Duration::from_millis(2 * OVERLAP_MS);

/// The runs already sent a pass reads again, beyond its limit, at most: this many limits.
const REREAD: usize = 4;

/// The runs a pass reads from the journal store at once, to merge their records.
const AT_ONCE: usize = 16;

/// Passes failed in a row after which the streams get `resync` and close.
const FAILURES: u32 = 5;

/// How long the loop waits between passes when nothing wakes it.
const EVERY: Duration = Duration::from_secs(1);

/// The shortest time between two passes: a burst of wake-ups is read once.
const GAP: Duration = Duration::from_millis(100);

/// How long a commit that moved the wake counter is given to reach the read model (the
/// projector applies it) before the pass reads.
const SETTLE: Duration = Duration::from_millis(100);

/// The batches a stream may fall behind by before it gets `resync`.
const BACKLOG: usize = 64;

/// A run that changed, as a stream needs it.
#[derive(Debug)]
pub(crate) struct Changed {
    pub(crate) tenant: TenantId,
    pub(crate) namespace: NamespaceName,
    pub(crate) labels: BTreeMap<String, String>,
    /// The event's name: `run`.
    pub(crate) event: &'static str,
    /// The event's data: the API's summary, in JSON.
    pub(crate) data: String,
}

/// What one pass sends every stream.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    pub(crate) runs: Vec<Changed>,
    /// The tenants whose streams get `resync`.
    pub(crate) resync: BTreeSet<TenantId>,
    /// The loop cannot read the changes: every stream gets `resync` and closes.
    pub(crate) close: bool,
}

impl Batch {
    fn is_empty(&self) -> bool {
        self.runs.is_empty() && self.resync.is_empty() && !self.close
    }
}

/// The hub, owned by `App`.
#[derive(Debug)]
pub(crate) struct Changes {
    batches: broadcast::Sender<Arc<Batch>>,
    open: Mutex<Open>,
    /// Moved when this process changed the read model: the loop reads sooner.
    poked: Notify,
    per_pass: usize,
}

/// The streams open, and whether the loop runs.
#[derive(Debug, Default)]
struct Open {
    /// Streams per tenant.
    streams: BTreeMap<TenantId, usize>,
    running: bool,
}

impl Changes {
    /// A hub whose passes read at most `per_pass` runs (at least 1).
    pub(crate) fn new(per_pass: usize) -> Self {
        Self {
            batches: broadcast::Sender::new(BACKLOG),
            open: Mutex::new(Open::default()),
            poked: Notify::new(),
            per_pass: per_pass.max(1),
        }
    }

    /// This process changed the read model: the loop, if it runs, reads now.
    pub(crate) fn poke(&self) {
        self.poked.notify_one();
    }

    /// Opens a stream of `tenant`'s changes: its tenant is read from now on, and the loop runs
    /// while the stream is open.
    pub(crate) fn subscribe(app: &Arc<App>, tenant: &TenantId) -> Subscription {
        let hub = &app.changes;
        // Subscribed before the loop starts: no batch of the stream's tenant is missed.
        let batches = hub.batches.subscribe();
        let start = hub.open.lock().is_ok_and(|mut open| {
            *open.streams.entry(tenant.clone()).or_default() += 1;
            !std::mem::replace(&mut open.running, true)
        });
        if start {
            tokio::spawn(run(app.clone()));
        }
        Subscription {
            batches,
            app: app.clone(),
            tenant: tenant.clone(),
        }
    }

    /// The tenants to read, or `None` when no stream is open any more: the loop stops then.
    fn tenants(&self) -> Option<Vec<TenantId>> {
        let mut open = self.open.lock().ok()?;
        if open.streams.is_empty() {
            open.running = false;
            return None;
        }
        Some(open.streams.keys().cloned().collect())
    }

    fn leave(&self, tenant: &TenantId) {
        if let Ok(mut open) = self.open.lock()
            && let Some(streams) = open.streams.get_mut(tenant)
        {
            *streams = streams.saturating_sub(1);
            if *streams == 0 {
                open.streams.remove(tenant);
            }
        }
    }

    /// Marks the loop as stopped, when it stops for another reason than the last stream.
    fn stopped(&self) {
        if let Ok(mut open) = self.open.lock() {
            open.running = false;
        }
    }
}

/// A stream's hold on the hub: the batches, and its tenant among those read until it drops.
#[derive(Debug)]
pub(crate) struct Subscription {
    pub(crate) batches: broadcast::Receiver<Arc<Batch>>,
    app: Arc<App>,
    tenant: TenantId,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.app.changes.leave(&self.tenant);
    }
}

/// The loop: a pass, then a wait, while a stream is open and the server runs.
async fn run(app: Arc<App>) {
    let hub = &app.changes;
    let mut cursor = Cursor::new(SystemClock.now().as_millis());
    let mut wake = app.wake.subscribe();
    let mut stop = app.stop.subscribe();
    let mut failures = 0;
    loop {
        if app.stopping() {
            hub.stopped();
            return;
        }
        let Some(tenants) = hub.tenants() else {
            return;
        };
        let started = tokio::time::Instant::now();
        let mut full = false;
        let batch = match cursor.pass(&app, &tenants, hub.per_pass).await {
            Ok(pass) => {
                full = pass.full;
                failures = if pass.failed { failures + 1 } else { 0 };
                pass.batch
            }
            Err(error) => {
                tracing::warn!(detail = %error.0.detail, "the change stream's pass failed");
                failures += 1;
                Batch::default()
            }
        };
        let batch = if failures >= FAILURES {
            tracing::warn!(
                failures,
                "the change streams close: their passes keep failing"
            );
            failures = 0;
            Batch {
                close: true,
                ..batch
            }
        } else {
            batch
        };
        if !batch.is_empty() {
            // No receiver: every stream closed during the pass.
            let _ = hub.batches.send(Arc::new(batch));
        }
        let next = started + GAP;
        // A pass that read as many runs as it could is followed at once.
        if full {
            tokio::time::sleep_until(next).await;
            continue;
        }
        let woken = tokio::select! {
            () = tokio::time::sleep_until(started + EVERY) => false,
            _ = wake.changed() => true,
            () = hub.poked.notified() => false,
            _ = stop.changed() => false,
        };
        tokio::time::sleep_until(if woken {
            next.max(tokio::time::Instant::now() + SETTLE)
        } else {
            next
        })
        .await;
    }
}

/// What a pass sends, and how it went.
#[derive(Debug)]
struct Pass {
    batch: Batch,
    /// It read as many runs as it could: more may follow.
    full: bool,
    /// Every run it had to send failed to be read.
    failed: bool,
}

/// Where the loop stands.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    /// The latest stamp read; at first, now.
    latest: u64,
    /// No pass reads before this stamp: a stamp read [`SETTLED`] ago, or the end of a pass that
    /// read only runs already sent.
    floor: u64,
    /// The latest stamp each recent pass read, with when: the floor once [`SETTLED`].
    recent: VecDeque<(Instant, u64)>,
    /// The `(changed_at, run)` pairs sent, those a pass may read again.
    sent: BTreeSet<(u64, RunId)>,
}

/// What a pass read, and where the loop stands after it.
#[derive(Debug)]
struct Read {
    /// The changes not sent yet.
    fresh: Vec<ChangedRun>,
    /// The pass read as many runs as it could: more may follow. With changes not sent yet,
    /// they read their limit (or more than the pass reads again).
    full: bool,
    next: Cursor,
}

impl Read {
    /// The changes are sent as `resync` to their tenants, not as runs.
    fn resyncs(&self) -> bool {
        self.full && !self.fresh.is_empty()
    }
}

impl Cursor {
    fn new(now: u64) -> Self {
        Self {
            latest: now,
            floor: 0,
            recent: VecDeque::new(),
            sent: BTreeSet::new(),
        }
    }

    /// Where the next pass reads from. It never moves back.
    fn since(&self) -> u64 {
        self.latest.saturating_sub(OVERLAP_MS).max(self.floor)
    }

    /// How many runs the next pass reads at most: its limit, beyond the runs it sent and may
    /// read again.
    fn limit(&self, per_pass: usize) -> usize {
        per_pass + self.sent.len().min(per_pass.saturating_mul(REREAD))
    }

    /// The stamps read [`SETTLED`] before `now` become the floor.
    fn settle(&mut self, now: Instant) {
        while let Some(&(at, stamp)) = self.recent.front() {
            if now.saturating_duration_since(at) < SETTLED {
                break;
            }
            self.floor = self.floor.max(stamp);
            self.recent.pop_front();
        }
        self.forget();
    }

    /// What no pass reads again is forgotten: since never moves back.
    fn forget(&mut self) {
        let since = self.since();
        self.sent.retain(|(at, _)| *at >= since);
    }

    /// What a pass that read `rows` from [`Cursor::since`], at most [`Cursor::limit`], at
    /// `now`, sends, and the cursor after it.
    fn read(&self, rows: Vec<ChangedRun>, per_pass: usize, now: Instant) -> Read {
        let since = self.since();
        let read_all = rows.len() >= self.limit(per_pass);
        let last = rows.last().map(|row| row.changed_at);
        let newest = rows.iter().map(|row| row.changed_at).max();
        let fresh: Vec<ChangedRun> = rows
            .into_iter()
            .filter(|row| !self.sent.contains(&(row.changed_at, row.summary.run.id)))
            .collect();
        let mut next = self.clone();
        if let Some(newest) = newest {
            next.latest = next.latest.max(newest);
            next.recent.push_back((now, newest));
        }
        if read_all
            && fresh.is_empty()
            && let Some(last) = last
        {
            // Only runs already sent, more than a pass reads again: on after them, a
            // millisecond further when they all share the one it read from.
            next.floor = next.floor.max(if last > since { last } else { last + 1 });
        }
        next.sent
            .extend(fresh.iter().map(|row| (row.changed_at, row.summary.run.id)));
        next.forget();
        Read {
            full: read_all,
            fresh,
            next,
        }
    }

    /// Reads the changes since the last pass and turns them into a batch. The cursor moves only
    /// once the changes are read: a pass that cannot read them is read again. A run that cannot
    /// be merged with its record is left out, and logged.
    async fn pass(
        &mut self,
        app: &App,
        tenants: &[TenantId],
        per_pass: usize,
    ) -> Result<Pass, ApiError> {
        let now = Instant::now();
        self.settle(now);
        let rows = app
            .store
            .changed_runs(tenants, self.since(), self.limit(per_pass))
            .await?;
        let read = self.read(rows, per_pass, now);
        let resyncs = read.resyncs();
        let Read { fresh, full, next } = read;
        *self = next;
        let mut batch = Batch::default();
        if full {
            if resyncs {
                batch.resync = fresh
                    .iter()
                    .map(|row| row.summary.run.tenant.clone())
                    .collect();
            }
            return Ok(Pass {
                batch,
                full,
                failed: false,
            });
        }
        let making: Vec<_> = fresh.iter().map(|row| changed(app, row)).collect();
        let made: Vec<Result<Changed, ApiError>> = futures_util::stream::iter(making)
            .buffered(AT_ONCE)
            .collect()
            .await;
        for (row, made) in fresh.iter().zip(made) {
            match made {
                Ok(changed) => batch.runs.push(changed),
                Err(error) => tracing::warn!(
                    run = %row.summary.run,
                    detail = %error.0.detail,
                    "a changed run is left out of the change streams"
                ),
            }
        }
        Ok(Pass {
            failed: !fresh.is_empty() && batch.runs.is_empty(),
            batch,
            full: false,
        })
    }
}

/// A run that changed, as `GET /v1/runs` lists it, in JSON.
async fn changed(app: &App, row: &ChangedRun) -> Result<Changed, ApiError> {
    let event = ChangeEvent::Run(Box::new(listed(app, &row.summary).await?));
    let data = serde_json::to_string(&event).map_err(ApiError::internal)?;
    Ok(Changed {
        tenant: row.summary.run.tenant.clone(),
        namespace: row.summary.namespace.clone(),
        labels: row.summary.labels.clone(),
        event: event.sse_event(),
        data,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rewind_core::{Digest, RunStatus, Timestamp};
    use rewind_engine::ports::{RunRef, RunSummary};

    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn change(n: u128, changed_at: u64) -> ChangedRun {
        ChangedRun {
            summary: RunSummary {
                run: RunRef::new(TenantId::local(), RunId::from_u128(n)),
                namespace: NamespaceName::migrated(),
                script: Digest::of(b"s"),
                headline: String::new(),
                toolset: Digest::of(b"t"),
                status: RunStatus::Active,
                mode: None,
                labels: BTreeMap::new(),
                parent: None,
                workflow: None,
                started_by: None,
                started_at: None,
                ended_at: None,
                last_activity_at: Timestamp(0),
                tool_calls: 0,
                errors: 0,
                history_len: 0,
                waits: Vec::new(),
                wakes_at: None,
                ending: None,
                quarantine: None,
            },
            changed_at,
        }
    }

    /// The read model's stamps: each run's latest, as `changed_runs` reads them.
    #[derive(Default)]
    struct Stamps(BTreeMap<u128, u64>);

    impl Stamps {
        fn write(&mut self, run: u128, at: u64) {
            self.0.insert(run, at);
        }

        fn changed_runs(&self, since: u64, limit: usize) -> Vec<ChangedRun> {
            let mut rows: Vec<(u64, u128)> = self
                .0
                .iter()
                .filter(|(_, at)| **at >= since)
                .map(|(run, at)| (*at, *run))
                .collect();
            rows.sort_unstable();
            rows.into_iter()
                .take(limit)
                .map(|(at, run)| change(run, at))
                .collect()
        }
    }

    /// What the passes of a loop sent.
    #[derive(Default)]
    struct Sent {
        /// `run` events, by `(run, changed_at)`: how many times each.
        runs: BTreeMap<(u128, u64), usize>,
        /// Passes that sent `resync`, and the runs they covered.
        resyncs: usize,
        resynced: BTreeSet<(u128, u64)>,
        /// How many rows each pass read.
        reads: Vec<usize>,
    }

    /// One pass at `now`, as `Cursor::pass` makes it.
    fn pass(cursor: &mut Cursor, stamps: &Stamps, per_pass: usize, now: Instant, sent: &mut Sent) {
        cursor.settle(now);
        let rows = stamps.changed_runs(cursor.since(), cursor.limit(per_pass));
        sent.reads.push(rows.len());
        let read = cursor.read(rows, per_pass, now);
        let pairs = read
            .fresh
            .iter()
            .map(|row| (row.summary.run.id.as_uuid().as_u128(), row.changed_at));
        if read.resyncs() {
            sent.resyncs += 1;
            sent.resynced.extend(pairs);
        } else if !read.full {
            for pair in pairs {
                *sent.runs.entry(pair).or_default() += 1;
            }
        }
        *cursor = read.next;
    }

    fn assert_once(sent: &Sent) {
        let twice: Vec<_> = sent.runs.iter().filter(|(_, n)| **n > 1).collect();
        assert!(twice.is_empty(), "sent twice: {twice:?}");
        for pair in sent.runs.keys() {
            assert!(!sent.resynced.contains(pair), "sent and resynced: {pair:?}");
        }
    }

    #[test]
    fn a_pass_reads_back_the_overlap_and_drops_what_it_sent() {
        let start = Instant::now();
        let mut cursor = Cursor::new(NOW);
        assert_eq!(
            cursor.since(),
            NOW - OVERLAP_MS,
            "the first pass reads from now less 2 s"
        );
        let mut stamps = Stamps::default();
        let mut sent = Sent::default();
        stamps.write(1, NOW - 500);
        stamps.write(2, NOW + 10);
        pass(&mut cursor, &stamps, 10, start, &mut sent);
        assert_eq!(
            cursor.since(),
            NOW + 10 - OVERLAP_MS,
            "from the stamps read"
        );

        // Read again within the overlap: #2 changed again, #3 committed late.
        stamps.write(3, NOW);
        stamps.write(2, NOW + 20);
        pass(&mut cursor, &stamps, 10, start + EVERY, &mut sent);
        assert_eq!(
            sent.runs.keys().copied().collect::<Vec<_>>(),
            [(1, NOW - 500), (2, NOW + 10), (2, NOW + 20), (3, NOW)]
        );
        assert_once(&sent);
        assert_eq!(sent.resyncs, 0);

        // Quiet: once the latest stamp settled, a pass reads only it.
        for n in 2..10 {
            pass(&mut cursor, &stamps, 10, start + EVERY * n, &mut sent);
        }
        assert_eq!(cursor.since(), NOW + 20, "the settled stamp is the floor");
        assert_eq!(sent.reads.last(), Some(&1));
        assert!(
            cursor.sent.iter().all(|(at, _)| *at >= cursor.since()),
            "what cannot be read again is forgotten"
        );
        assert_once(&sent);
    }

    /// 600 changes within a second, a limit of 500, then nothing: one `resync`, the other 100
    /// sent once, and no pass after them sends anything.
    #[test]
    fn a_burst_beyond_the_limit_resyncs_once_and_never_again() {
        let start = Instant::now();
        let mut cursor = Cursor::new(NOW);
        let mut stamps = Stamps::default();
        for n in 0..600 {
            stamps.write(n, NOW + n as u64);
        }
        let mut sent = Sent::default();
        let mut at = start;
        for _ in 0..60 {
            pass(&mut cursor, &stamps, 500, at, &mut sent);
            at += EVERY;
        }
        assert_eq!(sent.resyncs, 1, "one resync, at the burst");
        assert_eq!(sent.resynced.len(), 500);
        assert_eq!(sent.runs.len(), 100);
        assert_once(&sent);
        assert_eq!(
            sent.reads.last(),
            Some(&1),
            "a quiet pass reads the latest only"
        );
    }

    /// 300 changes a second for a minute, some of them to runs that changed before, with a
    /// limit of 500 and a pass a second: every change sent once, never `resync`.
    #[test]
    fn a_sustained_rate_below_the_limit_is_sent_once_without_resync() {
        let start = Instant::now();
        let mut cursor = Cursor::new(NOW);
        let mut stamps = Stamps::default();
        let mut sent = Sent::default();
        let mut written = BTreeSet::new();
        let mut next_run = 0;
        for second in 0..60u64 {
            for n in 0..300u64 {
                let at = NOW + second * 1_000 + n * 3;
                // Every tenth write changes a run written before.
                let run = if n % 10 == 0 && next_run > 100 {
                    next_run - 100
                } else {
                    next_run += 1;
                    next_run
                };
                stamps.write(run, at);
                written.insert((run, at));
            }
            pass(
                &mut cursor,
                &stamps,
                500,
                start + EVERY * u32::try_from(second).unwrap(),
                &mut sent,
            );
        }
        for n in 60..70 {
            pass(&mut cursor, &stamps, 500, start + EVERY * n, &mut sent);
        }
        assert_eq!(sent.resyncs, 0);
        assert_once(&sent);
        // A change overwritten within the same second is never read: the latest of each run is.
        let latest: BTreeSet<(u128, u64)> = stamps.0.iter().map(|(run, at)| (*run, *at)).collect();
        for pair in &latest {
            assert!(sent.runs.contains_key(pair), "never sent: {pair:?}");
        }
        assert!(sent.runs.keys().all(|pair| written.contains(pair)));
    }

    /// More runs than a pass reads again, all in one millisecond: the loop goes on after them
    /// and does not read them again.
    #[test]
    fn runs_sharing_one_millisecond_never_hold_the_loop() {
        let start = Instant::now();
        let mut cursor = Cursor::new(NOW);
        let mut stamps = Stamps::default();
        for n in 0..30 {
            stamps.write(n, NOW);
        }
        let mut sent = Sent::default();
        for n in 0..20 {
            pass(&mut cursor, &stamps, 2, start + GAP * n, &mut sent);
        }
        assert!(cursor.since() > NOW, "on after the millisecond");
        assert_eq!(sent.reads.last(), Some(&0));
        assert_once(&sent);
        stamps.write(99, NOW + 5);
        pass(&mut cursor, &stamps, 2, start + GAP * 20, &mut sent);
        assert!(
            sent.runs.contains_key(&(99, NOW + 5)),
            "a later change is sent"
        );
    }
}
