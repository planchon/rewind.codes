//! Forks (`archi/03-execution.md`, "Forks"; `archi/05-testing-and-observability.md`, "Replay,
//! debugging and forks").
//!
//! A fork is a new run started from an existing one with a corrected script, so that an agent
//! can fix step 7 without redoing the side effects of steps 1 to 6. It has its own id, the
//! parent's seed, input (unless overridden) and toolset, and a new TTL.
//!
//! Starting a fork only creates the run, with its parent and `at` as run metadata. Its first
//! activation replays the new script against the parent's history, in the same transaction as
//! everything else it commits:
//!
//! - While the script's commands match the parent's, the parent's triggers are copied into the
//!   fork's history, with their recorded results and `recorded_at`: no tool is called again.
//! - At the first command that differs, at the entry `at`, at the parent's cancellation or
//!   abort, or at the end of the parent's history, the fork goes live.
//! - The calls the fork inherited (their commands matched before it went live) and that are
//!   still pending: the parent's outcome, if it recorded one, is delivered; otherwise the call is
//!   dispatched again with the parent's idempotency key under `retry`, and fails with
//!   `outcome_unknown` under `at_most_once`.
//!
//! The fork's history is self-contained: it replays without the parent.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use rewind_core::{
    Attachments, Command, Completion, Event, HistoryEntry, Interruption, JournalDocument,
    JsonValue, Lifecycle, Next, NonDeterminism, Payload, ReplayCursor, Replayed, RunId, RunStarted,
    RunStatus, Script, Seq, Toolset,
};

use crate::attachments::AttachRights;
use crate::batch::EngineError;
use crate::batch::{Batch, CountCpu, Grace, abort_reason};
use crate::payloads;
use crate::plane::Limits;
use crate::ports::{
    CompiledScript, Emitted, ForkParent, RunRef, ScriptExecutor, ScriptInstance, StartScript,
    StoreError, TraceContext,
};
use crate::replay::{Heartbeat, NoHeartbeat};
use crate::start::StartError;

/// A request to fork a run (`POST /v1/runs/{id}/fork`, `rewind fork`, `retry_run`).
#[derive(Debug, Clone)]
pub struct ForkRun {
    pub parent: RunRef,
    /// The corrected script.
    pub script: Script,
    /// Go live from this entry of the parent's history, even if the commands still match.
    pub at: Option<u64>,
    /// The fork's input; the parent's when `None`.
    pub input: Option<JsonValue>,
    /// The fork's TTL, from now; [`EngineConfig::default_ttl`](crate::EngineConfig) when `None`.
    pub ttl: Option<Duration>,
    /// The fork's labels; the parent's when `None`.
    pub labels: Option<BTreeMap<String, String>>,
    pub trace_context: Option<TraceContext>,
    /// A retried request with the same key returns the same fork.
    pub idempotency_key: Option<String>,
    /// The fork's attachments; the parent's when `None`. Either way they are checked against
    /// `attach`: the script is new code (`archi/04-tools.md`, "Secrets").
    pub secrets: Option<Attachments>,
    /// What the caller may attach.
    pub attach: AttachRights,
}

impl ForkRun {
    pub fn new(parent: RunRef, script: Script) -> Self {
        Self {
            parent,
            script,
            at: None,
            input: None,
            ttl: None,
            labels: None,
            trace_context: None,
            idempotency_key: None,
            secrets: None,
            attach: AttachRights::NamespaceOnly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ForkError {
    #[error("run {0} not found")]
    ParentNotFound(RunId),
    #[error("run {0} has not started yet: there is nothing to fork")]
    ParentNotStarted(RunId),
    #[error("the journal of run {0} is archived: it cannot be forked")]
    ParentArchived(RunId),
    /// The fork cannot start, for the reasons a run cannot: an invalid script, TTL or input, the
    /// tenant's quota.
    #[error(transparent)]
    Start(StartError),
    #[error(transparent)]
    Engine(#[from] EngineError),
}

impl From<StoreError> for ForkError {
    fn from(err: StoreError) -> Self {
        ForkError::Engine(err.into())
    }
}

/// Where a fork stops following its parent and goes live.
#[derive(Debug, Clone, PartialEq)]
pub enum ForkPoint {
    /// The script emitted a command the parent did not: the fork goes live there.
    Diverged(NonDeterminism),
    /// The entry `at` of the request: the commands still matched.
    At(u64),
    /// The parent was cancelled at this entry: its history ends there for the fork.
    ParentCancelled(u64),
    /// The parent was aborted at this entry.
    ParentAborted(u64),
    /// The script followed the whole history of the parent, its end included when the parent
    /// ended by itself.
    EndOfParent,
    /// The fork's script could not go on while it followed its parent: it could not load, or
    /// hit a limit. The fork is aborted.
    Stopped,
}

impl fmt::Display for ForkPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForkPoint::Diverged(mismatch) => write!(f, "it diverges: {mismatch}"),
            ForkPoint::At(index) => write!(f, "it is forced live at entry {index}"),
            ForkPoint::ParentCancelled(index) => {
                write!(f, "the parent was cancelled at entry {index}")
            }
            ForkPoint::ParentAborted(index) => write!(f, "the parent was aborted at entry {index}"),
            ForkPoint::EndOfParent => f.write_str("it follows the parent's whole history"),
            ForkPoint::Stopped => f.write_str("the script could not go on"),
        }
    }
}

/// Whether a corrected script is compatible with a run's history, and what a fork of the run
/// with it would reuse (`rewind replay <run> --script fixed.ts`).
#[derive(Debug, Clone, PartialEq)]
pub struct Compatibility {
    pub point: ForkPoint,
    /// The calls whose recorded outcome a fork reuses: they are not called again.
    pub reused: Vec<Seq>,
    /// Calls the run left without an outcome, under `retry`: a fork calls them again with the
    /// run's idempotency key.
    pub redispatched: Vec<Seq>,
    /// Calls the run left without an outcome, under `at_most_once`: they fail in the fork with
    /// `outcome_unknown`.
    pub unknown: Vec<Seq>,
    /// The fork's status once it has followed the run: `completed` or `failed` when it ended as
    /// the run did, `active` when it goes live.
    pub status: RunStatus,
}

impl Compatibility {
    /// Whether the script reproduces the whole history: replay stops at a cancellation or an
    /// abort of the run.
    pub fn is_compatible(&self) -> bool {
        matches!(
            self.point,
            ForkPoint::EndOfParent | ForkPoint::ParentCancelled(_) | ForkPoint::ParentAborted(_)
        )
    }
}

/// A fork's first activation, as the activation hands it over.
pub(crate) struct ForkSource<'a> {
    pub(crate) run: &'a RunRef,
    pub(crate) parent: ForkParent,
    pub(crate) started: RunStarted,
    pub(crate) compiled: &'a CompiledScript,
    pub(crate) toolset: &'a Toolset,
}

/// What following a parent's history gave.
pub(crate) struct Followed<I> {
    pub(crate) instance: Option<I>,
    pub(crate) point: ForkPoint,
    pub(crate) reused: Vec<Completion>,
    pub(crate) redispatched: Vec<Seq>,
    pub(crate) unknown: Vec<Seq>,
}

/// What following a parent's history runs on: the executor, where the payloads are loaded
/// from, and what counts the CPU time of the calls into the script.
pub(crate) struct FollowWith<'a, X, S, C> {
    pub(crate) executor: &'a X,
    pub(crate) payloads: &'a S,
    pub(crate) cpu: &'a C,
}

/// Whether `script` is compatible with a run's journal, and if not, where it diverges; and what
/// a fork of the run with it would reuse (`rewind replay <run> --script fixed.ts`,
/// `archi/05-testing-and-observability.md`, "Replay, debugging and forks"). The script is
/// followed against the journal on `executor` as a fork's first activation follows its parent,
/// as the run itself (it reads the run's id); every payload of the journal is held as a value.
/// Nothing is written and no tool is called.
pub async fn compatibility<X: ScriptExecutor>(
    executor: &X,
    journal: &JournalDocument,
    script: &Script,
    at: Option<u64>,
    limits: &Limits,
) -> Result<Compatibility, ForkError> {
    let Some(HistoryEntry {
        event: Event::Lifecycle(Lifecycle::RunStarted(started)),
        ..
    }) = journal.entries.first()
    else {
        return Err(ForkError::ParentNotStarted(journal.run));
    };
    let compiled = executor
        .compile(script, &journal.toolset)
        .map_err(|invalid| ForkError::Start(StartError::InvalidScript(invalid)))?;
    let started = RunStarted {
        script: script.digest(),
        ..started.clone()
    };
    let run = RunRef::new(crate::ports::TenantId::local(), journal.run);
    let mut batch = Batch::for_check(&run, limits);
    let with = FollowWith {
        executor,
        payloads: &crate::replay::InJournal,
        cpu: &Uncounted,
    };
    let followed = follow_parent(
        &with,
        &mut batch,
        &mut NoHeartbeat,
        ForkSource {
            run: &run,
            parent: ForkParent {
                run: journal.run,
                at,
            },
            started,
            compiled: &compiled,
            toolset: &journal.toolset,
        },
        &journal.entries,
    )
    .await?;
    let mut reused: Vec<Seq> = followed.reused.iter().filter_map(completion_seq).collect();
    reused.sort();
    Ok(Compatibility {
        point: followed.point,
        reused,
        redispatched: followed.redispatched,
        unknown: followed.unknown,
        status: batch.state.status(),
    })
}

/// The CPU time of a check, which counts toward no namespace.
struct Uncounted;

impl CountCpu for Uncounted {
    fn count_cpu(
        &self,
        _tenant: &crate::ports::TenantId,
        _namespace: &crate::ports::NamespaceName,
        _cpu: Duration,
    ) {
    }
}

/// Runs the fork's script against the parent's history, copying the parent's triggers into
/// `batch` while the script's commands match the parent's.
pub(crate) async fn follow_parent<X, S, C, H>(
    with: &FollowWith<'_, X, S, C>,
    batch: &mut Batch<'_>,
    heartbeat: &mut H,
    source: ForkSource<'_>,
    history: &[HistoryEntry],
) -> Result<Followed<X::Instance>, EngineError>
where
    X: ScriptExecutor,
    S: payloads::PayloadSource,
    C: CountCpu,
    H: Heartbeat,
{
    {
        let parent_id = source.parent.run;
        let tenant = &source.run.tenant;
        // A parent's cancellation is where its history ends for the fork.
        let followed_len = history
            .iter()
            .position(|entry| {
                matches!(
                    entry.event,
                    Event::Completion(Completion::CancelRequested { .. })
                )
            })
            .unwrap_or(history.len());
        let parent = &history[..followed_len];
        let mut cursor =
            ReplayCursor::new(parent).map_err(|error| EngineError::CorruptHistory {
                run_id: parent_id,
                error,
            })?;
        let at = source.parent.at.unwrap_or(u64::MAX);
        let corrupt = |error| EngineError::CorruptHistory {
            run_id: source.run.id,
            error,
        };

        batch.copying = true;
        // The fork starts when its parent started: its logical time is the parent's while it
        // follows it.
        let recorded_at = batch.recorded_at(cursor.started_at());
        let input = payloads::load(with.payloads, tenant, &source.started.input).await?;
        let start = StartScript {
            run_id: source.run.id,
            compiled: source.compiled.clone(),
            toolset: source.toolset.clone(),
            input,
            seed: source.started.seed,
            epoch: source.started.epoch,
            started_at: recorded_at,
        };
        let trigger = Event::Lifecycle(Lifecycle::RunStarted(source.started));
        let mut inherited = BTreeSet::new();
        heartbeat.beat().await?;
        let (mut instance, mut point) = match with.executor.start(start).await {
            Ok((instance, emitted)) => {
                // The fork's script runs for the first time: its calls execute, though they
                // follow the parent's history.
                batch.count_cpu(with.cpu, instance.cpu_time());
                let followed = follow(&mut cursor, &emitted, at, &mut inherited);
                batch
                    .respond(recorded_at, trigger, emitted, Grace::No)
                    .map_err(corrupt)?;
                (instance, followed.err())
            }
            Err(err) => {
                batch
                    .abort(recorded_at, trigger, abort_reason(err))
                    .map_err(corrupt)?;
                batch.copying = false;
                return Ok(Followed {
                    instance: None,
                    point: ForkPoint::Stopped,
                    reused: Vec::new(),
                    redispatched: Vec::new(),
                    unknown: Vec::new(),
                });
            }
        };

        while point.is_none() {
            // The script ended as the parent did, its last command matching the parent's; or
            // the host aborted it.
            if batch.state.is_terminal() {
                let aborted = batch.entries().last().is_some_and(|entry| {
                    matches!(entry.event, Event::Lifecycle(Lifecycle::RunAborted(_)))
                });
                point = Some(if aborted {
                    ForkPoint::Stopped
                } else {
                    ForkPoint::EndOfParent
                });
                break;
            }
            let next = match cursor.on_idle() {
                Ok(next) => next,
                Err(mismatch) => {
                    point = Some(ForkPoint::Diverged(mismatch));
                    break;
                }
            };
            let (index, completion) = match next {
                Next::End if followed_len < history.len() => {
                    point = Some(ForkPoint::ParentCancelled(followed_len as u64));
                    break;
                }
                Next::End => {
                    point = Some(ForkPoint::EndOfParent);
                    break;
                }
                Next::Aborted { index, .. } => {
                    point = Some(ForkPoint::ParentAborted(index));
                    break;
                }
                Next::Deliver { index, .. } if index >= at => {
                    point = Some(ForkPoint::At(index));
                    break;
                }
                Next::Deliver { index, completion } => (index, completion),
            };
            // The parent's trigger is copied with its value, so that the fork references its
            // payloads itself, and with its `recorded_at`.
            let completion = with_values(with.payloads, tenant, completion).await?;
            let recorded_at = batch.recorded_at(cursor.logical_now());
            let delivery = payloads::delivery(with.payloads, tenant, &completion).await?;
            heartbeat.beat().await?;
            let trigger = Event::Completion(completion);
            let delivered = instance.deliver(delivery, recorded_at).await;
            batch.count_cpu(with.cpu, instance.cpu_time());
            match delivered {
                Ok(emitted) => {
                    let followed = follow(&mut cursor, &emitted, at, &mut inherited);
                    batch
                        .respond(recorded_at, trigger, emitted, Grace::No)
                        .map_err(corrupt)?;
                    point = followed.err();
                }
                Err(err) => {
                    batch
                        .abort(recorded_at, trigger, abort_reason(err))
                        .map_err(corrupt)?;
                    point = Some(ForkPoint::Stopped);
                }
            }
            tracing::trace!(index, "parent entry followed");
        }
        batch.copying = false;

        // The inherited calls still pending: the parent's outcome is reused if it recorded one.
        let mut reused = Vec::new();
        let mut redispatched = Vec::new();
        let mut unknown = Vec::new();
        if !batch.state.is_terminal() {
            let pending = batch.state.pending_tool_calls().clone();
            let mut outcomes: BTreeMap<Seq, Completion> = BTreeMap::new();
            let mut order = Vec::new();
            for entry in parent {
                if let Event::Completion(completion) = &entry.event
                    && let Some(seq) = completion_seq(completion)
                    && inherited.contains(&seq)
                    && pending.contains_key(&seq)
                    && !outcomes.contains_key(&seq)
                {
                    outcomes.insert(seq, completion.clone());
                    order.push(seq);
                }
            }
            for seq in order {
                if let Some(completion) = outcomes.remove(&seq) {
                    reused.push(with_values(with.payloads, tenant, &completion).await?);
                }
            }
            for (seq, tool) in &pending {
                if !inherited.contains(seq)
                    || reused.iter().any(|c| completion_seq(c) == Some(*seq))
                {
                    continue;
                }
                let policy = source
                    .toolset
                    .tool(tool)
                    .map(|tool| tool.policy.on_interruption);
                match policy {
                    Some(Interruption::AtMostOnce) => unknown.push(*seq),
                    _ => redispatched.push(*seq),
                }
            }
        }
        Ok(Followed {
            instance: Some(instance),
            point: point.unwrap_or(ForkPoint::Stopped),
            reused,
            redispatched,
            unknown,
        })
    }
}

/// A completion with its payload loaded: a fork's history holds its own values.
pub(crate) async fn with_values<S: payloads::PayloadSource>(
    payloads: &S,
    tenant: &crate::ports::TenantId,
    completion: &Completion,
) -> Result<Completion, EngineError> {
    Ok(match completion {
        Completion::ToolCallCompleted { seq, result } => Completion::ToolCallCompleted {
            seq: *seq,
            result: Payload::Value(payloads::load(payloads, tenant, result).await?),
        },
        Completion::SignalReceived { name, payload } => Completion::SignalReceived {
            name: name.clone(),
            payload: Payload::Value(payloads::load(payloads, tenant, payload).await?),
        },
        other => other.clone(),
    })
}

/// Whether a history scheduled the call `seq` to `tool` with `args`, and recorded no outcome for
/// it before its cancellation.
pub(crate) fn left_without_outcome(
    history: &[HistoryEntry],
    seq: Seq,
    tool: &str,
    args: &Payload,
) -> bool {
    let mut scheduled = false;
    for entry in history {
        match &entry.event {
            Event::Completion(Completion::CancelRequested { .. }) => break,
            Event::Command(Command::ScheduleToolCall {
                seq: recorded,
                tool: recorded_tool,
                args: recorded_args,
                ..
            }) if *recorded == seq => {
                scheduled = recorded_tool == tool && recorded_args.same_as(args);
            }
            Event::Completion(completion) if completion_seq(completion) == Some(seq) => {
                return false;
            }
            _ => {}
        }
    }
    scheduled
}

pub(crate) fn completion_seq(completion: &Completion) -> Option<Seq> {
    match completion {
        Completion::ToolCallCompleted { seq, .. } | Completion::ToolCallFailed { seq, .. } => {
            Some(*seq)
        }
        _ => None,
    }
}

/// Matches the commands the fork's script emitted against its parent's. Returns where the fork
/// stops following, if it does; the calls matched before `at` are inherited.
fn follow(
    cursor: &mut ReplayCursor<'_>,
    emitted: &[Emitted],
    at: u64,
    inherited: &mut BTreeSet<Seq>,
) -> Result<(), ForkPoint> {
    for emitted in emitted {
        let Emitted::Command(command) = emitted else {
            continue;
        };
        match cursor.on_command(command) {
            Ok(Replayed::Matched { index }) if index < at => {
                if let Command::ScheduleToolCall { seq, .. } = command {
                    inherited.insert(*seq);
                }
            }
            Ok(Replayed::Matched { index }) => return Err(ForkPoint::At(index)),
            Ok(Replayed::Aborted { index, .. }) => return Err(ForkPoint::ParentAborted(index)),
            Err(mismatch) => return Err(ForkPoint::Diverged(mismatch)),
        }
    }
    Ok(())
}
