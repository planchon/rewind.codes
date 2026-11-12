//! Rebuilding a script instance from a history (`archi/03-execution.md`, "Replay").
//!
//! Replay creates a fresh instance, runs `main(input)`, and walks the history with the
//! domain's [`ReplayCursor`]: recorded completions are delivered, and every command the
//! script emits must match the history. No tool is called and nothing is written: annotations
//! the script emits are already recorded, so they are dropped, and nothing about the script is
//! emitted as telemetry. The replay itself is a span of service telemetry, timed as
//! `ptc.replay.duration`.

use std::future::Future;

use tracing::Instrument as _;

use ptc_core::{
    AbortReason, HistoryEntry, HistoryError, Next, NonDeterminism, ReplayCursor, Replayed, Toolset,
};

pub use crate::payloads::{PayloadSource, StoredPayloads};

use crate::ports::{
    BlobError, CompiledScript, Emitted, ExecutorError, InvalidScript, RunRef, ScriptExecutor,
    ScriptInstance, StartScript, StoreError,
};
use crate::{metrics, payloads};

/// What to replay.
#[derive(Debug, Clone, Copy)]
pub struct ReplayInput<'a> {
    /// The run: its id, which the script can read (`run.id`), and its tenant, whose payloads
    /// the history references.
    pub run: &'a RunRef,
    /// The script as compiled, or why it does not compile.
    pub compiled: Result<&'a CompiledScript, &'a InvalidScript>,
    pub toolset: &'a Toolset,
    pub history: &'a [HistoryEntry],
}

/// A script rebuilt from its history.
#[derive(Debug)]
pub struct Rebuilt<I> {
    /// The instance, in exactly the state it was in after the last commit. `None` when the
    /// original run was aborted before the script could start.
    pub instance: Option<I>,
    pub end: ReplayEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayEnd {
    /// The history is exhausted: a run that has not ended can continue live from here.
    EndOfHistory,
    /// The host aborted the original run at this point: replay stops here.
    Aborted(AbortReason),
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ReplayError {
    #[error("invalid history: {0}")]
    History(#[from] HistoryError),
    #[error("non-determinism: {0}")]
    NonDeterminism(#[from] NonDeterminism),
    #[error(transparent)]
    Executor(#[from] ExecutorError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    /// The heartbeat could not renew the lease: another worker may hold the run now.
    #[error("the lease was lost during the replay: {0}")]
    LeaseLost(StoreError),
}

/// Called between the steps of a replay, so that a long replay keeps its lease
/// (`archi/06-storage-and-scale.md`, "Leases, fencing, workers"). Each step is one call into
/// the script, bounded by its CPU budget.
pub trait Heartbeat: Send {
    fn beat(&mut self) -> impl Future<Output = Result<(), StoreError>> + Send;
}

/// No lease to keep: a replay outside an activation (`ptc replay`, the corpus).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHeartbeat;

impl Heartbeat for NoHeartbeat {
    async fn beat(&mut self) -> Result<(), StoreError> {
        Ok(())
    }
}

/// Re-executes a script against a history, loading the payloads it delivers from `payloads`.
pub async fn replay<X, P>(
    executor: &X,
    payloads: &P,
    input: ReplayInput<'_>,
) -> Result<Rebuilt<X::Instance>, ReplayError>
where
    X: ScriptExecutor,
    P: PayloadSource,
{
    replay_with_heartbeat(executor, payloads, &mut NoHeartbeat, input).await
}

/// [`replay`], calling `heartbeat` before each call into the script.
pub async fn replay_with_heartbeat<X, P, H>(
    executor: &X,
    payloads: &P,
    heartbeat: &mut H,
    input: ReplayInput<'_>,
) -> Result<Rebuilt<X::Instance>, ReplayError>
where
    X: ScriptExecutor,
    P: PayloadSource,
    H: Heartbeat,
{
    let span = tracing::info_span!(
        "replay",
        entries = input.history.len() as u64,
        histogram.ptc.replay.duration = tracing::field::Empty,
        ptc.outcome = tracing::field::Empty,
    );
    let replayed = rebuild(executor, payloads, heartbeat, input)
        .instrument(span.clone())
        .await;
    span.record(metrics::OUTCOME, metrics::replay_outcome(&replayed));
    replayed
}

async fn rebuild<X, P, H>(
    executor: &X,
    payloads: &P,
    heartbeat: &mut H,
    input: ReplayInput<'_>,
) -> Result<Rebuilt<X::Instance>, ReplayError>
where
    X: ScriptExecutor,
    P: PayloadSource,
    H: Heartbeat,
{
    let mut cursor = ReplayCursor::new(input.history)?;
    let compiled = match input.compiled {
        Ok(compiled) => compiled.clone(),
        Err(invalid) => {
            let err = ExecutorError::InvalidScript(invalid.message.clone());
            return stopped::<X::Instance>(&mut cursor, None, err);
        }
    };
    let started = cursor.run_started();
    let tenant = &input.run.tenant;
    let start = StartScript {
        run_id: input.run.id,
        compiled,
        toolset: input.toolset.clone(),
        input: payloads::load(payloads, tenant, &started.input).await?,
        seed: started.seed,
        epoch: started.epoch,
        started_at: cursor.started_at(),
    };

    heartbeat.beat().await.map_err(ReplayError::LeaseLost)?;
    let (mut instance, emitted) = match executor.start(start).await {
        Ok(started) => started,
        Err(err) => return stopped(&mut cursor, None, err),
    };
    if let Some(reason) = check(&mut cursor, emitted)? {
        return Ok(aborted(Some(instance), reason));
    }

    loop {
        match cursor.on_idle()? {
            Next::Deliver { completion, .. } => {
                let delivery = payloads::delivery(payloads, tenant, completion).await?;
                heartbeat.beat().await.map_err(ReplayError::LeaseLost)?;
                let emitted = match instance.deliver(delivery, cursor.logical_now()).await {
                    Ok(emitted) => emitted,
                    Err(err) => return stopped(&mut cursor, Some(instance), err),
                };
                if let Some(reason) = check(&mut cursor, emitted)? {
                    return Ok(aborted(Some(instance), reason));
                }
            }
            Next::Aborted { reason, .. } => return Ok(aborted(Some(instance), reason)),
            Next::End => {
                return Ok(Rebuilt {
                    instance: Some(instance),
                    end: ReplayEnd::EndOfHistory,
                });
            }
        }
    }
}

/// Matches the emitted commands against the history. Returns the abort reason if the original
/// run was aborted at this point.
fn check<'h>(
    cursor: &mut ReplayCursor<'h>,
    emitted: Vec<Emitted>,
) -> Result<Option<&'h AbortReason>, NonDeterminism> {
    for emitted in emitted {
        if let Emitted::Command(command) = emitted
            && let Replayed::Aborted { reason, .. } = cursor.on_command(&command)?
        {
            return Ok(Some(reason));
        }
    }
    Ok(None)
}

/// The script could not go on. That is a faithful replay if the original run was aborted at
/// the same point (it could not load, or hit the same limit), and an error otherwise.
fn stopped<I>(
    cursor: &mut ReplayCursor<'_>,
    instance: Option<I>,
    err: ExecutorError,
) -> Result<Rebuilt<I>, ReplayError> {
    match cursor.aborted_here() {
        Some(reason) => Ok(aborted(instance, reason)),
        None => Err(ReplayError::Executor(err)),
    }
}

fn aborted<I>(instance: Option<I>, reason: &AbortReason) -> Rebuilt<I> {
    Rebuilt {
        instance,
        end: ReplayEnd::Aborted(reason.clone()),
    }
}
