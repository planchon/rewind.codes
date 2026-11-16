//! The worker role (`archi/06-storage-and-scale.md`, "Leases, fencing, workers"): activations
//! with the sticky cache and the lease heartbeat, the dispatch of tool calls with their claims'
//! heartbeats, the scheduler (timers, deadlines, deferred tasks), the projector, the retention
//! jobs with the erasure of deleted namespaces and the purge of ended tokens, the writing of the CPU time counted per
//! namespace and the sending of quota alerts ("Quotas"), and, once at start, the retry of the
//! quarantined runs of this build's epochs.
//!
//! In real time each runs in its own loop, woken by changes and by a short poll. In virtual time
//! one driver does everything that is ready, then moves the clock to the next timer, tool call
//! retry or deadline, and saves it in the store.

use std::sync::Arc;
use std::time::Duration;

use rewind_core::Timestamp;
use rewind_engine::ActivationOutcome;
use rewind_engine::ports::{AccessStore, FeedConsumer, JournalStore, StoreError, VirtualTime};
use rewind_engine::quotas::QuotaAlerts;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::app::App;
use crate::deliveries::{LEASE_MARGIN, Outbox, Waiting};
use crate::wake::{POLL, wait};

/// How often the claims of the calls in flight are renewed.
const CLAIM_HEARTBEAT: Duration = Duration::from_secs(1);
/// How often the retention jobs run.
const HOUSEKEEPING: Duration = Duration::from_secs(300);
/// How long an end user's opaque token is kept once expired or revoked: a margin for the
/// clocks of the servers, and a day to read its row after an incident.
const TOKENS_KEPT: Duration = Duration::from_secs(24 * 3_600);
/// How many tokens one statement of the purge deletes.
const TOKEN_PURGE_BATCH: usize = 1_000;
/// How long a loop waits after a failure of the store before it tries again.
const BACKOFF: Duration = Duration::from_secs(1);

/// Starts the worker's loops.
pub(crate) fn spawn(app: &Arc<App>, activations: usize, dispatches: usize) -> Vec<JoinHandle<()>> {
    let mut tasks = vec![
        tokio::spawn(retry_quarantined(app.clone())),
        tokio::spawn(projector(app.clone())),
        tokio::spawn(webhooks(app.clone())),
        tokio::spawn(housekeeping(app.clone())),
        tokio::spawn(usage(app.clone())),
        tokio::spawn(quota_alerts(app.clone())),
    ];
    if app.engine.clock().is_virtual() {
        tasks.push(tokio::spawn(virtual_driver(app.clone())));
        return tasks;
    }
    for _ in 0..activations {
        tasks.push(tokio::spawn(activate(app.clone())));
    }
    let calls = InFlight::new(dispatches);
    tasks.push(tokio::spawn(dispatch(app.clone(), calls.clone())));
    tasks.push(tokio::spawn(heartbeats(app.clone(), calls)));
    tasks.push(tokio::spawn(scheduler(app.clone())));
    tasks
}

/// The slots of the tool calls dispatched at once: a call holds one until it is answered and
/// its outcome committed.
#[derive(Debug, Clone)]
struct InFlight {
    slots: Arc<Semaphore>,
    size: u32,
}

impl InFlight {
    fn new(dispatches: usize) -> Self {
        let size = u32::try_from(dispatches)
            .unwrap_or(u32::MAX)
            .min(u32::try_from(Semaphore::MAX_PERMITS).unwrap_or(u32::MAX));
        Self {
            slots: Arc::new(Semaphore::new(size as usize)),
            size,
        }
    }

    /// Whether no call is in flight.
    fn idle(&self) -> bool {
        self.slots.available_permits() == self.size as usize
    }

    /// Waits until every call in flight has ended.
    async fn drained(&self) {
        let _ = self.slots.acquire_many(self.size).await;
    }
}

/// Activates runnable runs, one at a time per loop.
async fn activate(app: Arc<App>) {
    let mut changes = app.wake.subscribe();
    while !app.stopping() {
        match activate_one(&app).await {
            Ok(true) => {}
            Ok(false) => wait(&mut changes, POLL, None).await,
            Err(()) => tokio::time::sleep(BACKOFF).await,
        }
    }
}

/// One activation. Returns whether a run was activated.
async fn activate_one(app: &App) -> Result<bool, ()> {
    let report = match app.engine.activate_next().await {
        Ok(Some(report)) => report,
        Ok(None) => return Ok(false),
        Err(error) => {
            tracing::error!(%error, "no run could be leased");
            return Err(());
        }
    };
    app.activity.touch();
    match &report.outcome {
        ActivationOutcome::Quarantined { kind, reason } => {
            tracing::warn!(run = %report.run, ?kind, %reason, "run quarantined");
            app.record_quarantine(&report.run).await;
            // Queued: the engine's sender delivers in the background.
            if let Err(error) = app.projector().notify_quarantined(&report.run).await {
                tracing::warn!(run = %report.run, %error, "run.quarantined was not sent");
            }
        }
        ActivationOutcome::Failed(error) => {
            tracing::warn!(run = %report.run, %error, "an activation failed; it is retried once its lease expires");
        }
        ActivationOutcome::Committed { .. } | ActivationOutcome::NothingToDo => {}
    }
    app.wake.bump();
    Ok(true)
}

/// Claims the tool calls Rewind dispatches itself and runs them, as many at once as `calls`
/// has slots. Once the server stops, it claims nothing more and returns when the calls in flight
/// have ended: their outcomes are committed before the server stops, so a stop neither fails
/// nor repeats them.
async fn dispatch(app: Arc<App>, calls: InFlight) {
    let slots = calls.slots.clone();
    let mut changes = app.wake.subscribe();
    while !app.stopping() {
        let free = slots.available_permits();
        if free == 0 {
            let _ = tokio::time::timeout(POLL, slots.clone().acquire_owned()).await;
            continue;
        }
        let claimed = match app.engine.claim_tool_tasks(free).await {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::error!(%error, "no tool task could be claimed");
                tokio::time::sleep(BACKOFF).await;
                continue;
            }
        };
        if claimed.is_empty() {
            wait(&mut changes, POLL, None).await;
            continue;
        }
        for task in claimed {
            let Ok(slot) = slots.clone().acquire_owned().await else {
                return;
            };
            let app = app.clone();
            tokio::spawn(async move {
                let busy = app.activity.busy();
                app.record_task(&task.task).await;
                let name = task.task.task.clone();
                match app.engine.run_tool_task(task).await {
                    // Parked or deferred: it waits for the harness.
                    Ok(Some(waiting)) => app.record_progress(&name.run, waiting).await,
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(task = %name, %error, "a tool call could not be prepared");
                    }
                }
                app.wake.bump();
                drop(busy);
                drop(slot);
            });
        }
    }
    calls.drained().await;
}

/// Renews the claims of the calls in flight, until the server stops and the last of them has
/// ended.
async fn heartbeats(app: Arc<App>, calls: InFlight) {
    let mut ticks = tokio::time::interval(CLAIM_HEARTBEAT);
    while !(app.stopping() && calls.idle()) {
        ticks.tick().await;
        if let Err(error) = app.engine.heartbeat_tool_claims().await {
            tracing::warn!(%error, "the claims of the calls in flight were not renewed");
        }
    }
}

/// Fires due timers, cancels the runs past their deadline, ends late deferred tasks. Due timers
/// fire at once when the server starts.
async fn scheduler(app: Arc<App>) {
    let mut changes = app.wake.subscribe();
    while !app.stopping() {
        match schedule(&app).await {
            Ok(0) => wait(&mut changes, POLL, None).await,
            Ok(_) => {}
            Err(()) => tokio::time::sleep(BACKOFF).await,
        }
    }
}

/// One round of the scheduler. Returns how many things it moved.
async fn schedule(app: &App) -> Result<usize, ()> {
    let mut moved = 0;
    match app.engine.expire_due_runs().await {
        Ok(count) => moved += count,
        Err(error) => {
            tracing::error!(%error, "runs past their deadline were not expired");
            return Err(());
        }
    }
    match app.engine.fire_due_timers().await {
        Ok(count) => moved += count,
        Err(error) => {
            tracing::error!(%error, "due timers were not fired");
            return Err(());
        }
    }
    match app.engine.expire_deferred_tool_tasks().await {
        Ok(count) => moved += count,
        Err(error) => {
            tracing::error!(%error, "late deferred tasks were not ended");
            return Err(());
        }
    }
    match app.engine.fail_interrupted_host_tasks().await {
        Ok(count) => moved += count,
        Err(error) => {
            tracing::error!(%error, "interrupted host tasks were not ended");
            return Err(());
        }
    }
    if moved > 0 {
        app.activity.touch();
        app.wake.bump();
    }
    Ok(moved)
}

/// Applies the feed to the read model and the run telemetry. Lifecycle webhooks have a loop
/// of their own: a receiver that is slow or down must not hold up the projections every
/// tenant reads.
async fn projector(app: Arc<App>) {
    let mut changes = app.wake.subscribe();
    while !app.stopping() {
        let mut projected = 0;
        let mut failed = None;
        for consumer in [FeedConsumer::ReadModel, FeedConsumer::Otel] {
            match app.projector().project_consumer(consumer, app.now()).await {
                Ok((applied, _)) => projected += applied,
                Err(error) => failed = Some(error),
            }
        }
        if let Some(error) = failed {
            tracing::error!(%error, "the projector failed");
            tokio::time::sleep(BACKOFF).await;
        } else if projected == 0 {
            wait(&mut changes, POLL * 4, None).await;
        }
    }
}

/// Sends the lifecycle webhooks of the feed, concurrently and a few at a time per subscription
/// (`deliveries`). When the server stops, the deliveries in progress are left unacknowledged,
/// and sent again (with the same `webhook-id`) once their claims expire.
async fn webhooks(app: Arc<App>) {
    let outbox = Outbox {
        journal: &app.store,
        clock: app.engine.clock(),
        deliveries: app.deliveries.clone(),
        owner: app.settings.projector.owner.clone(),
        batch: app.settings.projector.batch,
    };
    outbox.run(app.wake.subscribe(), app.stop.subscribe()).await;
}

/// Archives, deletes and cleans up, as each tenant's retention says.
async fn housekeeping(app: Arc<App>) {
    let mut stop = app.stop.subscribe();
    while !app.stopping() {
        match app
            .housekeeper()
            .run(app.now(), rewind_format::encode_journal)
            .await
        {
            Ok(done) => {
                if done.archived
                    + done.deleted
                    + done.payloads_deleted
                    + done.purged_from_read_model
                    + done.erased_runs
                    + done.cancelled_runs
                    + done.erased_namespaces
                    > 0
                {
                    tracing::info!(?done, "retention jobs");
                }
                if done.cancelled_runs > 0 {
                    app.wake.bump();
                }
            }
            Err(error) => tracing::error!(%error, "the retention jobs failed"),
        }
        match purge_tokens(&app).await {
            Ok(0) => {}
            Ok(purged) => tracing::info!(purged, "ended tokens deleted"),
            Err(error) => tracing::error!(%error, "the ended tokens were not deleted"),
        }
        let _ = tokio::time::timeout(HOUSEKEEPING, stop.changed()).await;
    }
}

/// Deletes the end users' opaque tokens expired or revoked more than [`TOKENS_KEPT`] ago, batch
/// after batch (`archi/06-storage-and-scale.md`, "Retention and deletion").
async fn purge_tokens(app: &App) -> Result<u64, StoreError> {
    let kept = u64::try_from(TOKENS_KEPT.as_millis()).unwrap_or(u64::MAX);
    let before = Timestamp::from_millis(app.now().as_millis().saturating_sub(kept));
    let mut purged = 0;
    loop {
        let batch = app.store.purge_tokens(before, TOKEN_PURGE_BATCH).await?;
        purged += batch;
        if batch < TOKEN_PURGE_BATCH as u64 || app.stopping() {
            return Ok(purged);
        }
    }
}

/// Writes the CPU time this worker counted per namespace to the store, every lease renewal
/// period (about 10 s), and once more when the server stops: a worker that dies loses at most
/// that much counting (`archi/06-storage-and-scale.md`, "Quotas").
async fn usage(app: Arc<App>) {
    let mut stop = app.stop.subscribe();
    let mut ticks = tokio::time::interval(app.engine.config().lease_renewal);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let stopping = tokio::select! {
            _ = ticks.tick() => false,
            _ = stop.wait_for(|stop| *stop) => true,
        };
        match app.engine.flush_usage().await {
            Ok(alerts) if !alerts.is_empty() => app.wake.bump(),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "the CPU time counted was not written: it is written with the next");
            }
        }
        if stopping {
            return;
        }
    }
}

/// Sends the quota alerts the store recorded to the tenants' subscriptions (`quota.alert`), and
/// acknowledges them once their deliveries ended. An alert whose deliveries are cut by a stop is
/// sent again once its claim expires, with the same id.
async fn quota_alerts(app: Arc<App>) {
    let mut changes = app.wake.subscribe();
    let mut stop = app.stop.subscribe();
    let webhooks = Waiting(app.deliveries.clone());
    let sender = QuotaAlerts {
        journal: &app.store,
        webhooks: &webhooks,
        owner: &app.settings.projector.owner,
        lease_duration: app.deliveries.window().saturating_add(LEASE_MARGIN),
        batch: app.settings.projector.batch,
    };
    while !app.stopping() {
        let sent = tokio::select! {
            sent = sender.send(app.now()) => sent,
            _ = stop.wait_for(|stop| *stop) => return,
        };
        match sent {
            Ok(0) => wait(&mut changes, POLL * 4, None).await,
            Ok(_) => {}
            Err(error) => {
                tracing::error!(%error, "the quota alerts cannot be sent");
                tokio::time::sleep(BACKOFF).await;
            }
        }
    }
}

/// Retries, once, the quarantined runs of this build's epochs that no worker of this build has
/// retried yet (`archi/03-execution.md`, "Non-determinism and quarantine").
async fn retry_quarantined(app: Arc<App>) {
    match app.engine.retry_quarantined().await {
        Ok(reports) if !reports.is_empty() => {
            let released = reports
                .iter()
                .filter(|report| matches!(report.outcome, ActivationOutcome::Committed { .. }))
                .count();
            tracing::info!(
                retried = reports.len(),
                released,
                "quarantined runs retried by this build"
            );
            app.wake.bump();
        }
        Ok(_) => {}
        Err(error) => tracing::error!(%error, "the quarantined runs were not retried"),
    }
}

/// Virtual time: does everything ready, then jumps to the next timer, retry or deadline.
async fn virtual_driver(app: Arc<App>) {
    let mut changes = app.wake.subscribe();
    while !app.stopping() {
        match drive(&app).await {
            Ok(true) => {}
            Ok(false) => match next_wake_up(&app).await {
                Ok(Some(next)) if next > app.now() => {
                    app.engine.clock().advance_to(next);
                    if let Err(error) = app.store.set_virtual_time(Some(next)).await {
                        tracing::warn!(%error, "the virtual time was not saved");
                    }
                    app.activity.touch();
                }
                Ok(_) => wait(&mut changes, POLL, None).await,
                Err(()) => tokio::time::sleep(BACKOFF).await,
            },
            Err(()) => tokio::time::sleep(BACKOFF).await,
        }
    }
}

/// One pass of the virtual driver. Returns whether anything moved.
async fn drive(app: &App) -> Result<bool, ()> {
    let mut moved = schedule(app).await? > 0;
    let claimed = app.engine.claim_tool_tasks(100).await.map_err(|error| {
        tracing::error!(%error, "no tool task could be claimed");
    })?;
    for task in claimed {
        moved = true;
        app.record_task(&task.task).await;
        let run = task.task.task.run.clone();
        match app.engine.run_tool_task(task).await {
            Ok(Some(waiting)) => app.record_progress(&run, waiting).await,
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "a tool call could not be prepared"),
        }
    }
    while activate_one(app).await? {
        moved = true;
    }
    if moved {
        app.wake.bump();
    }
    Ok(moved)
}

/// The next timer or tool call retry, or the next deadline if it comes first. A deadline alone
/// wakes nothing: a run that waits for a signal waits for the outside world.
async fn next_wake_up(app: &App) -> Result<Option<Timestamp>, ()> {
    let failed =
        |error: &dyn std::fmt::Display| tracing::error!(%error, "the next wake-up is unknown");
    let timer = app
        .store
        .next_timer()
        .await
        .map_err(|error| failed(&error))?;
    let retry = app
        .engine
        .next_tool_wake_up()
        .await
        .map_err(|error| failed(&error))?;
    let Some(next) = timer.into_iter().chain(retry).min() else {
        return Ok(None);
    };
    let deadline = app
        .store
        .next_deadline()
        .await
        .map_err(|error| failed(&error))?;
    Ok(Some(deadline.map_or(next, |deadline| deadline.min(next))))
}
