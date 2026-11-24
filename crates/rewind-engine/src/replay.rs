//! Rebuilding a code instance from a history (`archi/03-execution.md`, "Replay").
//!
//! Replay creates a fresh instance, runs `main(input)`, and walks the history with the
//! domain's [`ReplayCursor`]: recorded completions are delivered, and every command the
//! code emits must match the history. No action is called and nothing is written: annotations
//! the code emits are already recorded, so they are dropped, and nothing about the code is
//! emitted as telemetry. The replay itself is a span of service telemetry, timed as
//! `rewind.replay.duration`.

use tracing::Instrument as _;

use rewind_core::{
    AbortReason, ActionSet, HistoryError, HistoryEvent, Next, NonDeterminism, ReplayCursor,
    Replayed,
};

pub use crate::payloads::PayloadSource;

/// Where the payloads of a run export are: in the document, every one a value. A replay
/// of a run export reads nothing from elsewhere.
#[derive(Debug, Clone, Copy, Default)]
pub struct InExport;

impl PayloadSource for InExport {
    async fn fetch(
        &self,
        _tenant: &crate::ports::TenantId,
        _digest: rewind_core::Digest,
    ) -> Result<Option<rewind_core::JsonValue>, BlobError> {
        Ok(None)
    }
}

use crate::ports::{
    BlobError, CodeExecutor, CodeInstance, CompiledCode, Emitted, ExecutorError, InvalidCode,
    RunRef, StartCode,
};
use crate::{metrics, payloads};

/// What to replay.
#[derive(Debug, Clone, Copy)]
pub struct ReplayInput<'a> {
    /// The run: its id, which the code can read (`run.id`), and its tenant, whose payloads
    /// the history references.
    pub run: &'a RunRef,
    /// The code as compiled, or why it does not compile.
    pub compiled: Result<&'a CompiledCode, &'a InvalidCode>,
    pub action_set: &'a ActionSet,
    pub history: &'a [HistoryEvent],
}

/// A code rebuilt from its history.
#[derive(Debug)]
pub struct Rebuilt<I> {
    /// The instance, in exactly the state it was in after the last commit. `None` when the
    /// original run was aborted before the code could start.
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
}

/// Re-executes a code against a history, loading the payloads it delivers from `payloads`.
pub async fn replay<X, P>(
    executor: &X,
    payloads: &P,
    input: ReplayInput<'_>,
) -> Result<Rebuilt<X::Instance>, ReplayError>
where
    X: CodeExecutor,
    P: PayloadSource,
{
    let span = tracing::info_span!(
        "replay",
        events = input.history.len() as u64,
        histogram.rewind.replay.duration = tracing::field::Empty,
        rewind.outcome = tracing::field::Empty,
    );
    let replayed = rebuild(executor, payloads, input)
        .instrument(span.clone())
        .await;
    span.record(metrics::OUTCOME, metrics::replay_outcome(&replayed));
    replayed
}

async fn rebuild<X, P>(
    executor: &X,
    payloads: &P,
    input: ReplayInput<'_>,
) -> Result<Rebuilt<X::Instance>, ReplayError>
where
    X: CodeExecutor,
    P: PayloadSource,
{
    let mut cursor = ReplayCursor::new(input.history)?;
    let compiled = match input.compiled {
        Ok(compiled) => compiled.clone(),
        Err(invalid) => {
            let err = ExecutorError::InvalidCode(invalid.message.clone());
            return stopped::<X::Instance>(&mut cursor, None, err);
        }
    };
    let started = cursor.run_started();
    let tenant = &input.run.tenant;
    let start = StartCode {
        run_id: input.run.id,
        compiled,
        action_set: input.action_set.clone(),
        input: payloads::load(payloads, tenant, &started.input).await?,
        seed: started.seed,
        epoch: started.epoch,
        started_at: cursor.started_at(),
    };

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

/// The code could not go on. That is a faithful replay if the original run was aborted at
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

/// The verdict of a replay of a run with its code, or another one ([`Edge::replay`](crate::edge::Edge::replay)).
#[derive(Debug, Clone, PartialEq)]
pub enum ReplayVerdict {
    /// The code reproduces the history.
    Matches(ReplayEnd),
    /// The code diverges from the history, at the event the error names.
    Diverges(NonDeterminism),
    /// The code could not go on where the original run went on.
    Fails(ExecutorError),
}
