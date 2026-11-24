//! Forks (`archi/03-execution.md`, "Forks"; `archi/05-testing-and-observability.md`, "Replay,
//! debugging and forks").
//!
//! A fork is a new run started from an existing one with a corrected code, so that an agent
//! can fix step 7 without redoing the side effects of steps 1 to 6. It has its own id, the
//! source's seed, input (unless overridden) and action set, and a new TTL.
//!
//! Starting a fork only creates the run, with its source and `at` as run metadata. Its first
//! activation replays the new code against the source's history, in the same transaction as
//! everything else it commits:
//!
//! - While the code's commands match the source's, the source's triggers are copied into the
//!   fork's history, with their recorded results and `recorded_at`: no action is called again.
//! - At the first command that differs, at the event `at`, at the source's cancellation or
//!   abort, or at the end of the source's history, the fork goes live.
//! - The calls the fork inherited (their commands matched before it went live) and that are
//!   still pending: the source's outcome, if it recorded one, is delivered; otherwise the call is
//!   dispatched again with the source's idempotency key when its action is idempotent, and fails
//!   with `outcome_unknown` when it is not.
//!
//! The fork's history is self-contained: it replays without the source.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use rewind_core::{
    ActionSet, Code, Command, Completion, Event, HistoryEvent, JsonValue, Lifecycle, Next,
    NonDeterminism, Payload, ReplayCursor, Replayed, RunExport, RunId, RunStarted, RunStatus, Seq,
    ServerSecrets,
};

use crate::batch::EngineError;
use crate::batch::{Batch, CountCpu, Grace, abort_reason};
use crate::payloads;
use crate::plane::Limits;
use crate::ports::{
    CodeExecutor, CodeInstance, CompiledCode, Emitted, ForkSource, RunRef, StartCode, StoreError,
    TraceContext,
};
use crate::secrets::SecretRights;
use crate::start::StartError;

/// A request to fork a run (`POST /v1/runs/{id}/fork`, `rewind fork`, `retry_run`).
#[derive(Debug, Clone)]
pub struct ForkRun {
    pub source: RunRef,
    /// The corrected code.
    pub code: Code,
    /// Go live from this event of the source's history, even if the commands still match.
    pub at: Option<u64>,
    /// The fork's input; the source's when `None`.
    pub input: Option<JsonValue>,
    /// The fork's TTL, from now; the namespace's default
    /// ([`NodeConfig::default_ttl`](crate::plane::NodeConfig::default_ttl) within its bounds) when
    /// `None`.
    pub ttl: Option<Duration>,
    /// The fork's labels; the source's when `None`.
    pub labels: Option<BTreeMap<String, String>>,
    pub trace_context: Option<TraceContext>,
    /// A retried request with the same key returns the same fork.
    pub idempotency_key: Option<String>,
    /// The fork's secrets; the source's when `None`. Either way they are checked against
    /// `secret_rights`: the code is new code (`archi/04-tools.md`, "Secrets").
    pub secrets: Option<ServerSecrets>,
    /// What the caller may use.
    pub secret_rights: SecretRights,
}

impl ForkRun {
    pub fn new(source: RunRef, code: Code) -> Self {
        Self {
            source,
            code,
            at: None,
            input: None,
            ttl: None,
            labels: None,
            trace_context: None,
            idempotency_key: None,
            secrets: None,
            secret_rights: SecretRights::NamespaceOnly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ForkError {
    #[error("run {0} not found")]
    SourceNotFound(RunId),
    #[error("run {0} has not started yet: there is nothing to fork")]
    SourceNotStarted(RunId),
    /// The fork cannot start, for the reasons a run cannot: an invalid code, TTL or input, the
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

/// Where a fork stops following its source and goes live.
#[derive(Debug, Clone, PartialEq)]
pub enum ForkPoint {
    /// The code emitted a command the source did not: the fork goes live there.
    Diverged(NonDeterminism),
    /// The event `at` of the request: the commands still matched.
    At(u64),
    /// The source was cancelled at this event: its history ends there for the fork.
    SourceCancelled(u64),
    /// The source was aborted at this event.
    SourceAborted(u64),
    /// The code followed the whole history of the source, its end included when the source
    /// ended by itself.
    EndOfSource,
    /// The fork's code could not go on while it followed its source: it could not load, or
    /// hit a limit. The fork is aborted.
    Stopped,
}

impl fmt::Display for ForkPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForkPoint::Diverged(mismatch) => write!(f, "it diverges: {mismatch}"),
            ForkPoint::At(index) => write!(f, "it is forced live at event {index}"),
            ForkPoint::SourceCancelled(index) => {
                write!(f, "the source was cancelled at event {index}")
            }
            ForkPoint::SourceAborted(index) => write!(f, "the source was aborted at event {index}"),
            ForkPoint::EndOfSource => f.write_str("it follows the source's whole history"),
            ForkPoint::Stopped => f.write_str("the code could not go on"),
        }
    }
}

/// Whether a corrected code is compatible with a run's history, and what a fork of the run
/// with it would reuse (`rewind replay <run> --code fixed.ts`).
#[derive(Debug, Clone, PartialEq)]
pub struct Compatibility {
    pub point: ForkPoint,
    /// The calls whose recorded outcome a fork reuses: they are not called again.
    pub reused: Vec<Seq>,
    /// Calls the run left without an outcome, of idempotent actions: a fork calls them again with
    /// the run's idempotency key.
    pub redispatched: Vec<Seq>,
    /// Calls the run left without an outcome, of actions that are not idempotent: they fail in the
    /// fork with `outcome_unknown`.
    pub unknown: Vec<Seq>,
    /// The fork's status once it has followed the run: `completed` or `failed` when it ended as
    /// the run did, `active` when it goes live.
    pub status: RunStatus,
}

impl Compatibility {
    /// Whether the code reproduces the whole history: replay stops at a cancellation or an
    /// abort of the run.
    pub fn is_compatible(&self) -> bool {
        matches!(
            self.point,
            ForkPoint::EndOfSource | ForkPoint::SourceCancelled(_) | ForkPoint::SourceAborted(_)
        )
    }
}

/// A fork's first activation, as the activation hands it over.
pub(crate) struct ForkStart<'a> {
    pub(crate) run: &'a RunRef,
    pub(crate) source: ForkSource,
    pub(crate) started: RunStarted,
    pub(crate) compiled: &'a CompiledCode,
    pub(crate) action_set: &'a ActionSet,
}

/// What following a source's history gave.
pub(crate) struct Followed<I> {
    pub(crate) instance: Option<I>,
    pub(crate) point: ForkPoint,
    pub(crate) reused: Vec<Completion>,
    pub(crate) redispatched: Vec<Seq>,
    pub(crate) unknown: Vec<Seq>,
}

/// What following a source's history runs on: the executor, where the payloads are loaded
/// from, and what counts the CPU time of the calls into the code.
pub(crate) struct FollowWith<'a, X, S, C> {
    pub(crate) executor: &'a X,
    pub(crate) payloads: &'a S,
    pub(crate) cpu: &'a C,
}

/// Whether `code` is compatible with a run's history, and if not, where it diverges; and what
/// a fork of the run with it would reuse (`rewind replay <run> --code fixed.ts`,
/// `archi/05-testing-and-observability.md`, "Replay, debugging and forks"). The code is
/// followed against the history on `executor` as a fork's first activation follows its source,
/// as the run itself (it reads the run's id); every payload of the history is held as a value.
/// Nothing is written and no action is called.
pub async fn compatibility<X: CodeExecutor>(
    executor: &X,
    export: &RunExport,
    code: &Code,
    at: Option<u64>,
    limits: &Limits,
) -> Result<Compatibility, ForkError> {
    let Some(HistoryEvent {
        event: Event::Lifecycle(Lifecycle::RunStarted(started)),
        ..
    }) = export.events.first()
    else {
        return Err(ForkError::SourceNotStarted(export.run));
    };
    let compiled = executor
        .compile(code, &export.action_set)
        .map_err(|invalid| ForkError::Start(StartError::InvalidCode(invalid)))?;
    let started = RunStarted {
        code: code.digest(),
        ..started.clone()
    };
    let run = RunRef::new(crate::ports::TenantId::local(), export.run);
    let mut batch = Batch::for_check(&run, limits);
    let with = FollowWith {
        executor,
        payloads: &crate::replay::InExport,
        cpu: &Uncounted,
    };
    let followed = follow_source(
        &with,
        &mut batch,
        ForkStart {
            run: &run,
            source: ForkSource {
                run: export.run,
                at,
            },
            started,
            compiled: &compiled,
            action_set: &export.action_set,
        },
        &export.events,
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

/// Runs the fork's code against the source's history, copying the source's triggers into
/// `batch` while the code's commands match the source's.
pub(crate) async fn follow_source<X, S, C>(
    with: &FollowWith<'_, X, S, C>,
    batch: &mut Batch<'_>,
    start: ForkStart<'_>,
    history: &[HistoryEvent],
) -> Result<Followed<X::Instance>, EngineError>
where
    X: CodeExecutor,
    S: payloads::PayloadSource,
    C: CountCpu,
{
    {
        let source_id = start.source.run;
        let tenant = &start.run.tenant;
        // A source's cancellation is where its history ends for the fork.
        let followed_len = history
            .iter()
            .position(|entry| {
                matches!(
                    entry.event,
                    Event::Completion(Completion::CancelRequested { .. })
                )
            })
            .unwrap_or(history.len());
        let followed = &history[..followed_len];
        let mut cursor =
            ReplayCursor::new(followed).map_err(|error| EngineError::CorruptHistory {
                run_id: source_id,
                error,
            })?;
        let at = start.source.at.unwrap_or(u64::MAX);
        let corrupt = |error| EngineError::CorruptHistory {
            run_id: start.run.id,
            error,
        };

        batch.copying = true;
        // The fork starts when its source started: its logical time is the source's while it
        // follows it.
        let recorded_at = batch.recorded_at(cursor.started_at());
        let input = payloads::load(with.payloads, tenant, &start.started.input).await?;
        let code_start = StartCode {
            run_id: start.run.id,
            compiled: start.compiled.clone(),
            action_set: start.action_set.clone(),
            input,
            seed: start.started.seed,
            epoch: start.started.epoch,
            started_at: recorded_at,
        };
        let trigger = Event::Lifecycle(Lifecycle::RunStarted(start.started));
        let mut inherited = BTreeSet::new();
        let (mut instance, mut point) = match with.executor.start(code_start).await {
            Ok((instance, emitted)) => {
                // The fork's code runs for the first time: its calls execute, though they
                // follow the source's history.
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
            // The code ended as the followed did, its last command matching the source's; or
            // the host aborted it.
            if batch.state.is_terminal() {
                let aborted = batch.events().last().is_some_and(|entry| {
                    matches!(entry.event, Event::Lifecycle(Lifecycle::RunAborted(_)))
                });
                point = Some(if aborted {
                    ForkPoint::Stopped
                } else {
                    ForkPoint::EndOfSource
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
                    point = Some(ForkPoint::SourceCancelled(followed_len as u64));
                    break;
                }
                Next::End => {
                    point = Some(ForkPoint::EndOfSource);
                    break;
                }
                Next::Aborted { index, .. } => {
                    point = Some(ForkPoint::SourceAborted(index));
                    break;
                }
                Next::Deliver { index, .. } if index >= at => {
                    point = Some(ForkPoint::At(index));
                    break;
                }
                Next::Deliver { index, completion } => (index, completion),
            };
            // The source's trigger is copied with its value, so that the fork references its
            // payloads itself, and with its `recorded_at`.
            let completion = with_values(with.payloads, tenant, completion).await?;
            let recorded_at = batch.recorded_at(cursor.logical_now());
            let delivery = payloads::delivery(with.payloads, tenant, &completion).await?;
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
            tracing::trace!(index, "followed event followed");
        }
        batch.copying = false;

        // The inherited calls still pending: the source's outcome is reused if it recorded one.
        let mut reused = Vec::new();
        let mut redispatched = Vec::new();
        let mut unknown = Vec::new();
        if !batch.state.is_terminal() {
            let pending = batch.state.pending_calls().clone();
            let mut outcomes: BTreeMap<Seq, Completion> = BTreeMap::new();
            let mut order = Vec::new();
            for entry in followed {
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
            for (seq, action) in &pending {
                if !inherited.contains(seq)
                    || reused.iter().any(|c| completion_seq(c) == Some(*seq))
                {
                    continue;
                }
                let idempotent = start
                    .action_set
                    .action(action)
                    .map(|action| action.policy.idempotent);
                match idempotent {
                    Some(false) => unknown.push(*seq),
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
        Completion::CallCompleted { seq, result } => Completion::CallCompleted {
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

/// Whether a history scheduled the call `seq` to `action` with `args`, and recorded no outcome for
/// it before its cancellation.
pub(crate) fn left_without_outcome(
    history: &[HistoryEvent],
    seq: Seq,
    action: &str,
    args: &Payload,
) -> bool {
    let mut scheduled = false;
    for entry in history {
        match &entry.event {
            Event::Completion(Completion::CancelRequested { .. }) => break,
            Event::Command(Command::ScheduleCall {
                seq: recorded,
                action: recorded_action,
                args: recorded_args,
                ..
            }) if *recorded == seq => {
                scheduled = recorded_action == action && recorded_args.same_as(args);
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
        Completion::CallCompleted { seq, .. } | Completion::CallFailed { seq, .. } => Some(*seq),
        _ => None,
    }
}

/// Matches the commands the fork's code emitted against its source's. Returns where the fork
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
                if let Command::ScheduleCall { seq, .. } = command {
                    inherited.insert(*seq);
                }
            }
            Ok(Replayed::Matched { index }) => return Err(ForkPoint::At(index)),
            Ok(Replayed::Aborted { index, .. }) => return Err(ForkPoint::SourceAborted(index)),
            Err(mismatch) => return Err(ForkPoint::Diverged(mismatch)),
        }
    }
    Ok(())
}
