//! Rebuilding a script instance from a history (`archi/03-execution.md`, "Replay").
//!
//! Replay creates a fresh instance, runs `main(input)`, and walks the history with the
//! domain's [`ReplayCursor`]: recorded completions are delivered, and every command the
//! script emits must match the history. No tool is called and nothing is written.

use ptc_core::{
    AbortReason, HistoryEntry, HistoryError, Next, NonDeterminism, ReplayCursor, Replayed, RunId,
    Toolset,
};

use crate::payloads;
use crate::ports::{
    BlobError, BlobStore, CompiledScript, Emitted, ExecutorError, InvalidScript, ScriptExecutor,
    ScriptInstance, StartScript,
};

/// What to replay.
#[derive(Debug, Clone, Copy)]
pub struct ReplayInput<'a> {
    /// The run's id, which the script can read (`run.id`).
    pub run_id: RunId,
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
}

/// Re-executes a script against a history. Annotations the script emits are dropped: they
/// are already recorded.
pub async fn replay<X, B>(
    executor: &X,
    blobs: &B,
    input: ReplayInput<'_>,
) -> Result<Rebuilt<X::Instance>, ReplayError>
where
    X: ScriptExecutor,
    B: BlobStore,
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
    let start = StartScript {
        run_id: input.run_id,
        compiled,
        toolset: input.toolset.clone(),
        input: payloads::load(blobs, &started.input).await?,
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
                let delivery = payloads::delivery(blobs, completion).await?;
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
