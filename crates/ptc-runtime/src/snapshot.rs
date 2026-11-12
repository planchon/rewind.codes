//! V8 startup snapshots (`archi/03-execution.md`, "Sticky execution"): one per epoch, holding
//! the state of an isolate once its bootstrap has run (the glue, the deterministic globals, the
//! web-standard globals of deno_web). Every instance starts from it, so a cache miss or a replay
//! does not pay for the bootstrap again.
//!
//! A snapshot is built once per process, the first time an instance of its epoch starts, on a
//! thread of its own: it holds the ops of this crate, so it cannot be built by a build script
//! without compiling them, and V8, a second time. What belongs to one run (its id, its seed, its
//! start, whether `fetch` exists) is not in the snapshot: the glue's `init` hook sets it.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex, PoisonError};

use deno_core::{JsRuntimeForSnapshot, RuntimeOptions};
use ptc_core::Epoch;
use ptc_engine::ports::ExecutorError;

use crate::epochs::Glue;
use crate::ops;

static SNAPSHOTS: LazyLock<Mutex<BTreeMap<Epoch, &'static [u8]>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// The snapshot of an epoch, built on first use. Concurrent first uses wait for one build.
pub(crate) fn of(glue: &'static Glue) -> Result<&'static [u8], ExecutorError> {
    let mut snapshots = SNAPSHOTS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(snapshot) = snapshots.get(&glue.epoch) {
        return Ok(snapshot);
    }
    let snapshot = std::thread::Builder::new()
        .name(format!("ptc-snapshot-{}", glue.epoch))
        .spawn(move || build(glue))
        .map_err(|err| internal(format!("cannot spawn the snapshot thread: {err}")))?
        .join()
        .map_err(|_| internal("building the snapshot panicked".into()))??;
    snapshots.insert(glue.epoch, snapshot);
    Ok(snapshot)
}

/// Runs the bootstrap in a fresh isolate, and keeps its state for the life of the process.
pub(crate) fn build(glue: &'static Glue) -> Result<&'static [u8], ExecutorError> {
    // V8 schedules its delayed tasks on the current tokio runtime.
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|err| internal(err.to_string()))?;
    let _context = tokio.enter();
    crate::executor::init_v8();
    let mut runtime = JsRuntimeForSnapshot::try_new(RuntimeOptions {
        extensions: ops::extensions(),
        ..Default::default()
    })
    .map_err(|err| internal(err.to_string()))?;
    runtime
        .execute_script("ptc:bootstrap", glue.bootstrap)
        .map_err(|err| internal(format!("the bootstrap of {} failed: {err}", glue.epoch)))?;
    Ok(Box::leak(runtime.snapshot()))
}

fn internal(message: String) -> ExecutorError {
    ExecutorError::Internal(message)
}
