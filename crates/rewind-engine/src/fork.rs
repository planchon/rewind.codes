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
    Command, Completion, Event, HistoryEntry, Interruption, JsonValue, Lifecycle, Next,
    NonDeterminism, Payload, ReplayCursor, Replayed, RunId, RunStarted, RunStatus, Script, Seq,
    ToolFailure, ToolFailureKind, Toolset,
};

use crate::activation::{Batch, Grace, abort_reason};
use crate::adapters::StaticTypeChecker;
use crate::engine::{
    Engine, EngineError, StartError, idempotency_key, refuse_nul_in_metadata, type_check,
};
use crate::payloads;
use crate::ports::{
    BlobStore, Clock, CompiledScript, CreateRun, Created, Emitted, Entropy, ForkParent,
    JournalStore, NewRun, RunMetadata, RunRecord, RunRef, ScriptExecutor, ScriptInstance,
    SecretStore, StartScript, StoreError, ToolDispatcher, TraceContext, TypeChecker, WebhookSender,
};
use crate::replay::{Heartbeat, NoHeartbeat};
use crate::text;

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
struct Followed<I> {
    instance: Option<I>,
    point: ForkPoint,
    reused: Vec<Completion>,
    redispatched: Vec<Seq>,
    unknown: Vec<Seq>,
}

impl<J, B, X, D, C, E, S, W> Engine<J, B, X, D, C, E, S, W>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: Clock,
    E: Entropy,
    S: SecretStore,
    W: WebhookSender,
{
    /// Starts a fork of a run with a corrected script. The fork's first activation copies what
    /// it can of the parent's history, then goes live.
    pub async fn fork(&self, request: ForkRun) -> Result<Created, ForkError> {
        self.fork_with(request, None::<&StaticTypeChecker>).await
    }

    /// [`fork`](Self::fork), with the corrected script type-checked first against the parent's
    /// toolset, as a start is: a script with a type error is refused before the fork exists.
    pub async fn fork_checked<K: TypeChecker>(
        &self,
        request: ForkRun,
        checker: &K,
    ) -> Result<Created, ForkError> {
        self.fork_with(request, Some(checker)).await
    }

    async fn fork_with<K: TypeChecker>(
        &self,
        request: ForkRun,
        checker: Option<&K>,
    ) -> Result<Created, ForkError> {
        let parent = &request.parent;
        let record = self
            .journal
            .get_run(parent)
            .await?
            .ok_or(ForkError::ParentNotFound(parent.id))?;
        if record.archive.is_some() {
            return Err(ForkError::ParentArchived(parent.id));
        }
        let history = match self.journal.load_history(parent, 0).await {
            Err(StoreError::Archived(id)) => return Err(ForkError::ParentArchived(id)),
            other => other?,
        };
        let Some(HistoryEntry {
            event: Event::Lifecycle(Lifecycle::RunStarted(parent_started)),
            ..
        }) = history.first()
        else {
            return Err(ForkError::ParentNotStarted(parent.id));
        };

        // A fork stays in its parent's namespace, and passes its bounds.
        let bounds = self
            .namespace_bounds(&parent.tenant, &record.namespace)
            .await
            .map_err(ForkError::Start)?;
        let ttl = request.ttl.unwrap_or(bounds.default_ttl);
        if ttl.is_zero() || ttl > bounds.max_ttl {
            return Err(ForkError::Start(StartError::InvalidTtl {
                requested: ttl,
                max: bounds.max_ttl,
            }));
        }
        bounds
            .check_lines(&request.script)
            .map_err(ForkError::Start)?;
        let refused = |nul| ForkError::Start(StartError::NulCharacter(nul));
        text::refuse_nul("the script", request.script.source()).map_err(refused)?;
        let metadata = RunMetadata {
            labels: request.labels.unwrap_or(record.metadata.labels),
            trace_context: request.trace_context,
            parent: Some(ForkParent {
                run: parent.id,
                at: request.at,
            }),
        };
        refuse_nul_in_metadata(&metadata, request.idempotency_key.as_deref()).map_err(refused)?;
        let stored = self.toolset(&parent.tenant, record.toolset).await?;
        let toolset = stored.toolset;
        let compiled = self
            .executor
            .compile(&request.script, &toolset)
            .map_err(|invalid| ForkError::Start(StartError::InvalidScript(invalid)))?;
        if let Some(checker) = checker {
            type_check(checker, &request.script, &stored.declarations)
                .await
                .map_err(ForkError::Start)?;
        }
        let input = match request.input {
            Some(input) => input,
            None => self
                .load_payload(&parent.tenant, &parent_started.input)
                .await
                .map_err(EngineError::from)?,
        };
        let input = Payload::Value(input);
        let size = payloads::value_size(&input).unwrap_or_default();
        if size > self.config.limits.max_payload_bytes {
            return Err(ForkError::Start(StartError::InputTooLarge {
                size,
                max: self.config.limits.max_payload_bytes,
            }));
        }
        self.journal
            .put_script(&parent.tenant, &request.script, &compiled)
            .await?;

        let now = self.clock.now();
        let fork = NewRun {
            run: RunRef::new(parent.tenant.clone(), self.entropy.run_id()),
            namespace: record.namespace.clone(),
            started: RunStarted {
                script: request.script.digest(),
                toolset: record.toolset,
                input,
                seed: parent_started.seed,
                // The parent's epoch: the fork must compute what the parent computed to follow
                // it.
                epoch: parent_started.epoch,
                expires_at: now.saturating_add(ttl),
                mode: parent_started.mode,
            },
            metadata,
            created_at: now,
        };
        match self
            .journal
            .create_run(fork, request.idempotency_key)
            .await?
        {
            CreateRun::Created(created) => {
                tracing::debug!(run = %created.run, parent = %parent, "fork started");
                Ok(created)
            }
            CreateRun::QuotaExceeded(exceeded) => {
                Err(ForkError::Start(StartError::QuotaExceeded(exceeded)))
            }
            CreateRun::NamespaceDeleting => Err(ForkError::Start(StartError::NamespaceDeleting)),
        }
    }

    /// Whether `script` is compatible with the run's history, and if not, where it diverges;
    /// and what a fork with it would reuse. The script is replayed as the run itself (it reads
    /// the run's id). Nothing is written and no tool is called.
    pub async fn compatibility(
        &self,
        run: &RunRef,
        script: &Script,
        at: Option<u64>,
    ) -> Result<Compatibility, ForkError> {
        let record = self
            .journal
            .get_run(run)
            .await?
            .ok_or(ForkError::ParentNotFound(run.id))?;
        let history = match self.journal.load_history(run, 0).await {
            Err(StoreError::Archived(id)) => return Err(ForkError::ParentArchived(id)),
            other => other?,
        };
        let Some(HistoryEntry {
            event: Event::Lifecycle(Lifecycle::RunStarted(started)),
            ..
        }) = history.first()
        else {
            return Err(ForkError::ParentNotStarted(run.id));
        };
        let toolset = self.toolset(&run.tenant, record.toolset).await?.toolset;
        let compiled = self
            .executor
            .compile(script, &toolset)
            .map_err(|invalid| ForkError::Start(StartError::InvalidScript(invalid)))?;
        let started = RunStarted {
            script: script.digest(),
            ..started.clone()
        };
        let mut batch = Batch::for_run(run, &self.config.limits);
        let followed = self
            .follow_parent(
                &mut batch,
                &mut NoHeartbeat,
                ForkSource {
                    run,
                    parent: ForkParent { run: run.id, at },
                    started,
                    compiled: &compiled,
                    toolset: &toolset,
                },
                &history,
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

    /// The first activation of a fork: follows the parent's history, then delivers the
    /// outcomes of the inherited calls. Returns the fork's instance.
    pub(crate) async fn start_fork<H: Heartbeat>(
        &self,
        batch: &mut Batch<'_>,
        heartbeat: &mut H,
        source: ForkSource<'_>,
    ) -> Result<Option<X::Instance>, EngineError> {
        let parent = RunRef::new(source.run.tenant.clone(), source.parent.run);
        let history = self.journal.load_history(&parent, 0).await?;
        let followed = self
            .follow_parent(batch, heartbeat, source, &history)
            .await?;
        tracing::info!(parent = %parent, point = %followed.point, "the fork goes live");
        let Some(mut instance) = followed.instance else {
            return Ok(None);
        };
        let unknown = followed.unknown.into_iter().map(|seq| {
            let error = ToolFailure {
                kind: ToolFailureKind::OutcomeUnknown,
                message: format!(
                    "the parent run {} left this call without an outcome, and the tool's policy \
                     is at_most_once: it may or may not have taken effect",
                    parent.id
                ),
                data: None,
            };
            Completion::ToolCallFailed { seq, error }
        });
        for completion in followed.reused.into_iter().chain(unknown) {
            if batch.state.is_terminal() {
                break;
            }
            let recorded_at = batch.recorded_at(self.clock.now());
            self.deliver(batch, heartbeat, &mut instance, completion, recorded_at)
                .await?;
        }
        Ok(Some(instance))
    }

    /// Runs the fork's script against the parent's history, copying the parent's triggers into
    /// `batch` while the script's commands match the parent's.
    async fn follow_parent<H: Heartbeat>(
        &self,
        batch: &mut Batch<'_>,
        heartbeat: &mut H,
        source: ForkSource<'_>,
        history: &[HistoryEntry],
    ) -> Result<Followed<X::Instance>, EngineError> {
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
        let input = self.load_payload(tenant, &source.started.input).await?;
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
        let (mut instance, mut point) = match self.executor.start(start).await {
            Ok((instance, emitted)) => {
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
            let completion = self.with_values(tenant, completion).await?;
            let recorded_at = batch.recorded_at(cursor.logical_now());
            let delivery = payloads::delivery(&self.payloads(), tenant, &completion).await?;
            heartbeat.beat().await?;
            let trigger = Event::Completion(completion);
            match instance.deliver(delivery, recorded_at).await {
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
                    reused.push(self.with_values(tenant, &completion).await?);
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

    /// A completion with its payload loaded: a fork's history holds its own values.
    async fn with_values(
        &self,
        tenant: &crate::ports::TenantId,
        completion: &Completion,
    ) -> Result<Completion, EngineError> {
        Ok(match completion {
            Completion::ToolCallCompleted { seq, result } => Completion::ToolCallCompleted {
                seq: *seq,
                result: Payload::Value(self.load_payload(tenant, result).await?),
            },
            Completion::SignalReceived { name, payload } => Completion::SignalReceived {
                name: name.clone(),
                payload: Payload::Value(self.load_payload(tenant, payload).await?),
            },
            other => other.clone(),
        })
    }

    /// The idempotency key of the call `seq` of a run: `{run_id}/{seq}`, except for a call a
    /// fork inherited from its parent without an outcome. That call carries its parent's key,
    /// so that the tool recognizes a duplicate: a fork's call carries its parent's key when the
    /// parent scheduled the same call (same `seq`, tool and args) and recorded no outcome for it
    /// before its cancellation, and the parent's own key follows the same rule.
    pub(crate) async fn call_key(
        &self,
        record: &RunRecord,
        seq: Seq,
        tool: &str,
        args: &Payload,
    ) -> Result<String, EngineError> {
        let mut owner = record.run.id;
        let mut parent = record.metadata.parent;
        while let Some(link) = parent {
            let parent_ref = RunRef::new(record.run.tenant.clone(), link.run);
            let Some(parent_record) = self.journal.get_run(&parent_ref).await? else {
                break;
            };
            let history = match self.journal.load_history(&parent_ref, 0).await {
                Ok(history) => history,
                Err(StoreError::Archived(_)) => break,
                Err(err) => return Err(err.into()),
            };
            if !left_without_outcome(&history, seq, tool, args) {
                break;
            }
            owner = link.run;
            parent = parent_record.metadata.parent;
        }
        Ok(idempotency_key(owner, seq))
    }
}

/// Whether a history scheduled the call `seq` to `tool` with `args`, and recorded no outcome for
/// it before its cancellation.
fn left_without_outcome(history: &[HistoryEntry], seq: Seq, tool: &str, args: &Payload) -> bool {
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

fn completion_seq(completion: &Completion) -> Option<Seq> {
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
