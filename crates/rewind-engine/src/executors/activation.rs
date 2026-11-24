//! One activation of the run plane (`archi/07-code-architecture.md`, "Inside a node";
//! `archi/03-execution.md`, "Activations", "Sticky execution", "Forks").
//!
//! ```text
//! request ← head, inbox, history unless the cache holds the run at its length, payloads, script
//! instance ← cache[run], or executor.start + ReplayCursor over history
//! for item in inbox:
//!     recorded_at ← max(clock.now(), last recorded_at)
//!     if the attempt ended: drop the item
//!     emitted ← instance.deliver(item, recorded_at)    # payloads loaded from where they live
//!     cpu += instance.cpu_time()                       # calls that execute, never a replay's
//!     entries += [item] + emitted
//! move payloads > 1 KiB out of the entries: to the run's payload records, or a blob, put first
//! derive tasks, timers, head, summary, counters and projections from the new entries
//! → Committed, for the owner's next batch; the instance stays in the cache
//! ```
//!
//! The same work runs for a local owner and for a remote one: the request carries what the
//! executor needs, and the result what the owner commits. A replay mismatch commits nothing
//! and quarantines the run; any other failure commits nothing and is recorded by the owner.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use rewind_core::{
    AbortKind, AbortReason, ActionFailure, ActionFailureKind, Completion, Digest, Event,
    HistoryEntry, JsonValue, Lifecycle, RunEnd, RunOutcome, RunState, Timestamp,
};

use crate::batch::EngineError;
use crate::batch::{Batch, CountCpu, Grace, abort_reason};
use crate::cache::Cached;
use crate::executors::derive;
use crate::fork::{FollowWith, ForkSource, follow_parent};
use crate::payloads::{self, PayloadSource};
use crate::plane::{InstanceOf, NodeConfig, Plane};
use crate::ports::link::{ActivationRequest, ActivationResult, Committed, Purpose};
use crate::ports::partition::Activation;
use crate::ports::{
    BlobError, BlobOwner, BlobStore, Clock, ExecutorError, NamespaceName, PayloadRecord,
    QUARANTINE_AFTER_FAILURES, QuarantineKind, RunHead, RunRef, ScriptExecutor, ScriptInstance,
    StartScript, TenantId, Trigger,
};
use crate::replay::{self, Rebuilt, ReplayEnd, ReplayError, ReplayInput};
use crate::text;
use tracing::Instrument as _;

/// An instance kept by the executors of a node, with the run's state and history.
pub(crate) type Warm<P> = Cached<InstanceOf<P>>;

/// Runs an activation. Never fails: a failure is the result the owner records.
pub(crate) async fn activate<P: Plane>(
    plane: &P,
    config: &NodeConfig,
    request: ActivationRequest,
    warm: Option<Warm<P>>,
) -> (ActivationResult, Option<Warm<P>>) {
    let run = request.head.run.clone();
    let (trace, _) = crate::telemetry::trace_of(request.head.run.id, &request.head.metadata);
    let first = request.head.history_len;
    let triggers = request.inbox.len();
    let span = tracing::info_span!(
        "activation",
        rewind.run_id = %run.id,
        trace_id = %trace,
        span_id = %crate::telemetry::span_id(run.id, crate::telemetry::SpanKey::Activation(first)),
        parent_span_id = %crate::telemetry::span_id(run.id, crate::telemetry::SpanKey::Run),
        histogram.rewind.activation.duration = tracing::field::Empty,
        rewind.outcome = tracing::field::Empty,
    );
    let done = match work(plane, config, request, warm)
        .instrument(span.clone())
        .await
    {
        Ok(done) => done,
        Err(error) => {
            tracing::error!(%run, %error, "activation failed");
            (
                ActivationResult::Failed {
                    error: text::storable(error.to_string()),
                },
                None,
            )
        }
    };
    let outcome = match &done.0 {
        ActivationResult::Committed(_) => "committed",
        ActivationResult::Quarantined { .. } => "quarantined",
        ActivationResult::Released => "released",
        ActivationResult::Failed { .. } => "failed",
        ActivationResult::Miss => "miss",
    };
    span.record(crate::metrics::OUTCOME, outcome);
    if !matches!(done.0, ActivationResult::Miss) {
        crate::metrics::inbox(triggers);
    }
    done
}

/// The CPU time of the calls into the script that executed.
#[derive(Debug, Default)]
struct Tally(Mutex<Duration>);

impl CountCpu for Tally {
    fn count_cpu(&self, _tenant: &TenantId, _namespace: &NamespaceName, cpu: Duration) {
        let mut total = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *total = total.saturating_add(cpu);
    }
}

impl Tally {
    fn total(&self) -> Duration {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where an activation loads its payloads from: the records it was sent, then the blobs of the
/// run (and of a fork's parent).
pub(crate) struct Carried<'a, B> {
    pub(crate) records: BTreeMap<Digest, PayloadRecord>,
    pub(crate) blobs: &'a B,
    pub(crate) owners: Vec<BlobOwner>,
}

impl<B: BlobStore> PayloadSource for Carried<'_, B> {
    async fn fetch(
        &self,
        _tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<JsonValue>, BlobError> {
        if let Some(value) = self
            .records
            .get(&digest)
            .and_then(|record| record.value.clone())
        {
            return Ok(Some(value));
        }
        for owner in &self.owners {
            if let Some(bytes) = self.blobs.get(owner, digest).await? {
                let value = serde_json::from_slice(&bytes).map_err(|err| BlobError::Corrupt {
                    digest,
                    message: err.to_string(),
                })?;
                return Ok(Some(value));
            }
        }
        Ok(None)
    }
}

/// Why a run is quarantined instead of activated: [`QUARANTINE_AFTER_FAILURES`] activations
/// started on owners that died during them, counted at each recovery. `None` otherwise.
fn unfinished(head: &RunHead) -> Option<String> {
    if head.attempts < QUARANTINE_AFTER_FAILURES {
        return None;
    }
    let why = "the node running it stopped during it, as when the script crashes the node's \
               process or the node is killed for the memory it uses";
    Some(if head.failures == 0 {
        format!(
            "the activation of this run never finished {} times in a row: {why}",
            head.attempts
        )
    } else {
        format!(
            "{} activations of this run in a row committed nothing: {} never finished ({why}), \
             and {} failed",
            head.attempts + head.failures,
            head.attempts,
            head.failures
        )
    })
}

async fn work<P: Plane>(
    plane: &P,
    config: &NodeConfig,
    request: ActivationRequest,
    warm: Option<Warm<P>>,
) -> Result<(ActivationResult, Option<Warm<P>>), EngineError> {
    let ActivationRequest {
        partition: _,
        head,
        summary: previous,
        inbox,
        history,
        payloads,
        parent,
        script,
        action_set,
        purpose,
        tasks,
    } = request;
    let run = head.run.clone();
    let run_id = run.id;
    let corrupt = |error| EngineError::CorruptHistory { run_id, error };
    if purpose == Purpose::Runnable
        && let Some(reason) = unfinished(&head)
    {
        tracing::error!(%run, %reason, "the run's activations never finish: run quarantined");
        return Ok((
            ActivationResult::Quarantined {
                kind: QuarantineKind::Failures,
                reason: text::storable(reason),
            },
            None,
        ));
    }

    let mut records: BTreeMap<Digest, PayloadRecord> = payloads
        .into_iter()
        .map(|record| (record.digest, record))
        .collect();
    let mut owners = vec![BlobOwner::Run(run.clone())];
    // A fork copies the blobs of its parent it references.
    let mut copied = BTreeMap::new();
    if let Some(parent) = &parent {
        let from = BlobOwner::Run(RunRef::new(run.tenant.clone(), parent.run));
        for record in &parent.payloads {
            if record.value.is_none() {
                copied.insert(record.digest, from.clone());
            }
            records
                .entry(record.digest)
                .or_insert_with(|| record.clone());
        }
        if let Some(sealed) = &parent.sealed {
            for entry in &sealed.entries {
                if let Some(rewind_core::Payload::Digest(digest)) =
                    payloads::payload_of(&entry.event)
                {
                    copied.entry(*digest).or_insert_with(|| from.clone());
                }
            }
        }
        owners.push(from);
    }
    let source = Carried {
        records,
        blobs: plane.blobs(),
        owners,
    };

    if purpose == Purpose::GiveUp {
        let Some(history) = history else {
            return Ok((ActivationResult::Miss, None));
        };
        return give_up(
            plane,
            config,
            &source,
            head,
            inbox,
            history,
            &action_set,
            &script,
            &tasks,
        )
        .await
        .map(|result| (result, None));
    }

    let hit = warm.as_ref().is_some_and(|warm| {
        purpose == Purpose::Runnable && warm.state.history_len() == head.history_len
    });
    let counted = purpose == Purpose::Runnable && head.history_len > 0;
    let (mut instance, state, mut history) = match warm {
        Some(warm) if hit => {
            tracing::debug!("instance cache hit");
            if counted {
                crate::metrics::instance_cache_lookup(true);
            }
            (Some(warm.instance), warm.state, warm.history)
        }
        _ => {
            let Some(history) = history else {
                return Ok((ActivationResult::Miss, None));
            };
            if counted {
                crate::metrics::instance_cache_lookup(false);
            }
            let state = RunState::from_history(&history).map_err(corrupt)?;
            let mut instance = None;
            if !history.is_empty() && !state.is_terminal() {
                let input = ReplayInput {
                    run: &run,
                    compiled: Ok(&script.compiled),
                    action_set: &action_set.action_set,
                    history: &history,
                };
                match replay::replay(plane.executor(), &source, input).await {
                    Ok(Rebuilt {
                        instance: Some(rebuilt),
                        end: ReplayEnd::EndOfHistory,
                    }) => instance = Some(rebuilt),
                    Ok(_) => {
                        return Err(EngineError::CorruptInbox {
                            run_id,
                            message: "the replay of a run that has not ended stopped early".into(),
                        });
                    }
                    Err(ReplayError::NonDeterminism(mismatch)) => {
                        tracing::error!(%run, %mismatch, "replay mismatch: run quarantined");
                        return Ok((
                            ActivationResult::Quarantined {
                                kind: QuarantineKind::NonDeterminism,
                                reason: text::storable(mismatch.to_string()),
                            },
                            None,
                        ));
                    }
                    Err(ReplayError::Executor(
                        err
                        @ (ExecutorError::InvalidScript(_) | ExecutorError::UnsupportedEpoch(_)),
                    )) => {
                        return Ok((
                            ActivationResult::Quarantined {
                                kind: QuarantineKind::NonDeterminism,
                                reason: text::storable(format!(
                                    "the replay could not go on: {err}"
                                )),
                            },
                            None,
                        ));
                    }
                    Err(ReplayError::Executor(error)) => {
                        return Err(EngineError::ReplayInterrupted { run_id, error });
                    }
                    Err(ReplayError::History(error)) => return Err(corrupt(error)),
                    Err(ReplayError::Blob(err)) => return Err(err.into()),
                }
            }
            (instance, state, history)
        }
    };

    let released = purpose == Purpose::QuarantineRetry;
    if released {
        tracing::warn!(%run, "a quarantined run replays again: released");
        if inbox.is_empty() {
            let warm = instance.map(|instance| Cached {
                instance,
                state,
                history,
            });
            return Ok((ActivationResult::Released, warm));
        }
    }

    let tally = Tally::default();
    let last_recorded_at = state.updated_at().unwrap_or(Timestamp(0));
    let mut batch = Batch::new(
        &run,
        Some(head.namespace.clone()),
        state,
        last_recorded_at,
        &config.limits,
    );
    let clock = plane.clock();
    let mut first_of_fork = false;
    for item in inbox {
        batch.consumed.push(item.id);
        if batch.state.is_terminal() {
            // A completion for a run that has ended: dropped.
            continue;
        }
        match item.trigger {
            Trigger::RunStarted(started) => {
                if batch.state.history_len() != 0 {
                    return Err(EngineError::CorruptInbox {
                        run_id,
                        message: "run_started for a run that has already started".into(),
                    });
                }
                if let Some(link) = head.metadata.parent {
                    let Some(parent) = &parent else {
                        return Err(EngineError::CorruptInbox {
                            run_id,
                            message: "the history of the fork's parent was not sent".into(),
                        });
                    };
                    let parent_history = parent
                        .sealed
                        .as_ref()
                        .map_or(&parent.entries, |sealed| &sealed.entries);
                    let with = FollowWith {
                        executor: plane.executor(),
                        payloads: &source,
                        cpu: &tally,
                    };
                    let source_of_fork = ForkSource {
                        run: &run,
                        parent: link,
                        started,
                        compiled: &script.compiled,
                        action_set: &action_set.action_set,
                    };
                    instance = start_fork(&with, clock, &mut batch, source_of_fork, parent_history)
                        .await?;
                    first_of_fork = true;
                    continue;
                }
                let recorded_at = batch.recorded_at(clock.now());
                let input = payloads::load(&source, &run.tenant, &started.input).await?;
                let trigger = Event::Lifecycle(Lifecycle::RunStarted(started.clone()));
                let start = StartScript {
                    run_id,
                    compiled: script.compiled.clone(),
                    action_set: action_set.action_set.clone(),
                    input,
                    seed: started.seed,
                    epoch: started.epoch,
                    started_at: recorded_at,
                };
                match plane.executor().start(start).await {
                    Ok((started, emitted)) => {
                        batch.count_cpu(&tally, started.cpu_time());
                        instance = Some(started);
                        batch.respond(recorded_at, trigger, emitted, Grace::No)
                    }
                    Err(err) => batch.abort(recorded_at, trigger, abort_reason(err)),
                }
                .map_err(corrupt)?;
            }
            Trigger::Completion(completion) => {
                let Some(running) = instance.as_mut() else {
                    return Err(EngineError::CorruptInbox {
                        run_id,
                        message: format!("{} arrived before run_started", completion.name()),
                    });
                };
                let mut now = clock.now();
                // A timer fires at its `fire_at` at the earliest, whatever the clock of the node
                // delivering it: after `sleep(d)`, `Date.now()` is at least `d` later.
                if let Completion::TimerFired { seq } = &completion
                    && let Some(fire_at) = batch.state.pending_timers().get(seq)
                {
                    now = now.max(*fire_at);
                }
                let recorded_at = batch.recorded_at(now);
                deliver(
                    &source,
                    &tally,
                    &mut batch,
                    running,
                    completion,
                    recorded_at,
                )
                .await?;
            }
        }
    }

    let from = history.len();
    let crate::batch::Finished {
        entries,
        consumed,
        before,
        after: state,
    } = batch.finish();
    history.extend(entries);
    let end = end_of(&source, &run.tenant, &state).await?;
    // The summary the owner holds describes the history before the new entries, unless the
    // activation copied a fork's parent: then the summary is derived from every entry.
    let previous =
        previous.filter(|previous| !first_of_fork && previous.summary.history_len == from as u64);
    let committed = commit(
        plane,
        config,
        &source,
        CommitOf {
            head: &head,
            history: &mut history,
            from,
            before: &before,
            state: &state,
            previous: previous.as_ref(),
            consumed,
            released,
            copied: &copied,
            action_set: &action_set.action_set,
            headline: &crate::projector::headline(script.script.source()),
            tasks: &tasks,
            cpu: tally.total(),
            end,
            first_of_fork,
        },
    )
    .await?;
    let warm = match instance {
        Some(instance) if !state.is_terminal() => Some(Cached {
            instance,
            state,
            history,
        }),
        _ => None,
    };
    tracing::debug!(
        entries = committed.activation.entries.len(),
        status = %committed.activation.head.status,
        "activation done"
    );
    Ok((ActivationResult::Committed(Box::new(committed)), warm))
}

/// What an activation commits, before it is derived.
struct CommitOf<'a> {
    head: &'a RunHead,
    history: &'a mut Vec<HistoryEntry>,
    /// The new entries are `history[from..]`: they take the run from `before` to `state`.
    from: usize,
    before: &'a RunState,
    state: &'a RunState,
    /// The summary of `history[..from]`, when there is one to count the new entries from.
    previous: Option<&'a crate::ports::IndexedSummary>,
    consumed: Vec<u64>,
    released: bool,
    copied: &'a BTreeMap<Digest, BlobOwner>,
    action_set: &'a rewind_core::ActionSet,
    headline: &'a str,
    tasks: &'a [crate::ports::TaskRecord],
    cpu: Duration,
    end: Option<JsonValue>,
    first_of_fork: bool,
}

/// Moves the new entries' payloads out, and derives what the activation writes beside them.
async fn commit<P: Plane, B: BlobStore>(
    plane: &P,
    config: &NodeConfig,
    source: &Carried<'_, B>,
    of: CommitOf<'_>,
) -> Result<Committed, EngineError> {
    let CommitOf {
        head,
        history,
        from,
        before,
        state,
        previous,
        consumed,
        released,
        copied,
        action_set,
        headline,
        tasks,
        cpu,
        end,
        first_of_fork,
    } = of;
    let run = &head.run;
    let now = plane.clock().now();
    let shapes = derive::shapes_of(
        head,
        history,
        from,
        &derive::Openings::new(history, from, before),
    );
    let records = payloads::offload_into_run(
        plane.blobs(),
        run,
        &mut history[from..],
        config.payloads,
        copied,
    )
    .await?;
    let (new_tasks, timers) = derive::effects_of(head, history, from, state, action_set, now)
        .map_err(|action| EngineError::UnknownAction {
            run_id: run.id,
            action,
        })?;
    let mut by_seq: BTreeMap<_, _> = tasks
        .iter()
        .map(|task| (task.task.task.seq, task.clone()))
        .collect();
    for task in &new_tasks {
        by_seq.insert(task.task.task.seq, task.clone());
    }
    let mut summary_head = head.clone();
    if released {
        summary_head.quarantine = None;
    }
    let summary = derive::summary_of(
        &summary_head,
        headline,
        history,
        state,
        action_set,
        &by_seq,
        previous,
    );
    let next = derive::head_after(head, history, &summary, released);
    let summary = derive::with_quarantine(summary, next.quarantine.as_ref());
    let openings = derive::Openings::new(history, from, before);
    let mut counters = derive::counters_of(&next, history, from, &openings, &summary, cpu, now);
    counters.extend(shapes);
    let mut all_records = source.records.clone();
    for record in &records {
        all_records.insert(record.digest, record.clone());
    }
    let new = derive::NewEntries {
        history,
        from,
        before,
        after: state,
    };
    let projections =
        derive::projections_of(&next, new, action_set, &all_records, &config.telemetry);
    let measurements = derive::measurements_of(&next, new, &all_records, &config.telemetry);
    let children = if state.is_terminal() || first_of_fork {
        derive::children_of(history)
    } else {
        Vec::new()
    };
    let mode = state.mode();
    Ok(Committed {
        activation: Activation {
            run: run.clone(),
            attempt: head.attempt,
            entries: history[from..].to_vec(),
            consumed,
            payloads: records,
            tasks: new_tasks,
            timers,
            head: next,
            summary,
            counters,
            outbox: Vec::new(),
        },
        projections,
        end,
        children,
        copied: first_of_fork,
        cpu,
        mode,
        measurements,
    })
}

/// Delivers one completion, recorded at `recorded_at`, and appends it with the script's
/// response, or with `run_aborted` if the script cannot go on.
async fn deliver<I: ScriptInstance, S: PayloadSource, C: CountCpu>(
    source: &S,
    cpu: &C,
    batch: &mut Batch<'_>,
    instance: &mut I,
    completion: Completion,
    recorded_at: Timestamp,
) -> Result<(), EngineError> {
    let run_id = batch.run_id;
    let delivery = payloads::delivery(source, &batch.tenant, &completion).await?;
    let grace = match completion {
        Completion::CancelRequested { .. } => Grace::Ends,
        _ => Grace::No,
    };
    let trigger = Event::Completion(completion);
    let delivered = instance.deliver(delivery, recorded_at).await;
    batch.count_cpu(cpu, instance.cpu_time());
    match delivered {
        Ok(emitted) => batch.respond(recorded_at, trigger, emitted, grace),
        Err(err) => batch.abort(recorded_at, trigger, abort_reason(err)),
    }
    .map_err(|error| EngineError::CorruptHistory { run_id, error })
}

/// The first activation of a fork: follows the parent's history, then delivers the outcomes of
/// the inherited calls (`archi/03-execution.md`, "Forks").
async fn start_fork<X, S, C, K>(
    with: &FollowWith<'_, X, S, C>,
    clock: &K,
    batch: &mut Batch<'_>,
    source: ForkSource<'_>,
    parent: &[HistoryEntry],
) -> Result<Option<X::Instance>, EngineError>
where
    X: ScriptExecutor,
    S: PayloadSource,
    C: CountCpu,
    K: Clock,
{
    let parent_id = source.parent.run;
    let followed = follow_parent(with, batch, source, parent).await?;
    tracing::info!(parent = %parent_id, point = %followed.point, "the fork goes live");
    let Some(mut instance) = followed.instance else {
        return Ok(None);
    };
    let unknown = followed.unknown.into_iter().map(|seq| {
        let error = ActionFailure {
            kind: ActionFailureKind::OutcomeUnknown,
            message: format!(
                "the parent run {parent_id} left this call without an outcome, and the action is \
                 not idempotent: it may or may not have taken effect"
            ),
            data: None,
        };
        Completion::CallFailed { seq, error }
    });
    for completion in followed.reused.into_iter().chain(unknown) {
        if batch.state.is_terminal() {
            break;
        }
        let recorded_at = batch.recorded_at(clock.now());
        deliver(
            with.payloads,
            with.cpu,
            batch,
            &mut instance,
            completion,
            recorded_at,
        )
        .await?;
    }
    Ok(Some(instance))
}

/// The end of a run that just ended, for its watchers: from its outcome, the output loaded when
/// it is held by digest.
async fn end_of<S: PayloadSource>(
    source: &S,
    tenant: &TenantId,
    state: &RunState,
) -> Result<Option<JsonValue>, EngineError> {
    let Some(outcome) = state.outcome() else {
        return Ok(None);
    };
    if let Some(end) = RunEnd::from_outcome(outcome) {
        return Ok(Some(end.to_json()));
    }
    let RunOutcome::Completed { output } = outcome else {
        return Ok(None);
    };
    let output = payloads::load(source, tenant, output).await?;
    Ok(Some(RunEnd::Completed { output }.to_json()))
}

/// Ends a quarantined run: `run_aborted { non_determinism }` after a replay mismatch,
/// `run_aborted { internal }` after repeated failures. A run quarantined before its first
/// commit (a fork whose parent is gone) commits `run_started` with it.
#[allow(clippy::too_many_arguments)]
async fn give_up<P: Plane, B: BlobStore>(
    plane: &P,
    config: &NodeConfig,
    source: &Carried<'_, B>,
    head: RunHead,
    inbox: Vec<crate::ports::InboxItem>,
    mut history: Vec<HistoryEntry>,
    action_set: &crate::ports::StoredActionSet,
    script: &crate::ports::StoredScript,
    tasks: &[crate::ports::TaskRecord],
) -> Result<ActivationResult, EngineError> {
    let run_id = head.run.id;
    let Some(quarantine) = head.quarantine.clone() else {
        return Ok(ActivationResult::Released);
    };
    let mut state = RunState::from_history(&history)
        .map_err(|error| EngineError::CorruptHistory { run_id, error })?;
    if state.is_terminal() {
        return Ok(ActivationResult::Released);
    }
    let now = plane.clock().now();
    let recorded_at = state.updated_at().map_or(now, |last| last.max(now));
    let mut events = Vec::with_capacity(2);
    if history.is_empty() {
        let started = inbox.iter().find_map(|item| match &item.trigger {
            Trigger::RunStarted(started) => Some(started.clone()),
            Trigger::Completion(_) => None,
        });
        let started = started.ok_or_else(|| EngineError::CorruptInbox {
            run_id,
            message: "a run that has not started has no run_started".into(),
        })?;
        events.push(Event::Lifecycle(Lifecycle::RunStarted(started)));
    }
    let abort = AbortReason {
        kind: match quarantine.kind {
            QuarantineKind::NonDeterminism => AbortKind::NonDeterminism,
            QuarantineKind::Failures => AbortKind::Internal,
        },
        message: quarantine.reason,
    };
    events.push(Event::Lifecycle(Lifecycle::RunAborted(abort)));
    let from = history.len();
    let before = state.clone();
    for event in events {
        let entry = HistoryEntry {
            index: state.history_len(),
            recorded_at,
            event,
        };
        state
            .apply(&entry)
            .map_err(|error| EngineError::CorruptHistory { run_id, error })?;
        history.push(entry);
    }
    let end = end_of(source, &head.run.tenant, &state).await?;
    tracing::warn!(run = %head.run, "quarantined run given up");
    let committed = commit(
        plane,
        config,
        source,
        CommitOf {
            head: &head,
            history: &mut history,
            from,
            before: &before,
            state: &state,
            previous: None,
            consumed: inbox.iter().map(|item| item.id).collect(),
            released: false,
            copied: &BTreeMap::new(),
            action_set: &action_set.action_set,
            headline: &crate::projector::headline(script.script.source()),
            tasks,
            cpu: Duration::ZERO,
            end,
            first_of_fork: false,
        },
    )
    .await?;
    Ok(ActivationResult::Committed(Box::new(committed)))
}

/// The digests of the payloads entries hold by digest: what an activation must be sent.
pub(crate) fn digests_of<'a>(
    entries: impl IntoIterator<Item = &'a HistoryEntry>,
) -> BTreeSet<Digest> {
    entries
        .into_iter()
        .filter_map(|entry| match payloads::payload_of(&entry.event) {
            Some(rewind_core::Payload::Digest(digest)) => Some(*digest),
            _ => None,
        })
        .collect()
}
