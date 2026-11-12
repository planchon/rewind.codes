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

/// deno_core shares three small buffers of its Rust state with its JavaScript (tick, immediate
/// and timer information) as array buffers over that memory. `JsRuntimeForSnapshot::snapshot`
/// frees the state before V8 serializes the context, so V8 read freed memory while it copied
/// them into the snapshot. Arrays of V8's own replace them first; an isolate started from the
/// snapshot gets buffers over its own state again.
const DETACH_SHARED_STATE: &str = "Deno.core.__setTickInfo(new Uint8Array(2));\n\
     Deno.core.__setImmediateInfo(new Uint32Array(3));\n\
     Deno.core.__setTimerInfo(new Int32Array(1));";

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
    runtime
        .execute_script("ptc:snapshot", DETACH_SHARED_STATE)
        .map_err(|err| internal(format!("preparing the snapshot failed: {err}")))?;
    Ok(Box::leak(runtime.snapshot()))
}

fn internal(message: String) -> ExecutorError {
    ExecutorError::Internal(message)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use deno_core::JsRuntime;

    use super::*;
    use crate::epochs;

    fn average(runs: u32, mut start: impl FnMut()) -> Duration {
        start(); // warm-up
        let begin = Instant::now();
        for _ in 0..runs {
            start();
        }
        begin.elapsed() / runs
    }

    /// What the snapshot saves each new instance: creating an isolate and running the bootstrap,
    /// against creating it from the snapshot.
    /// `cargo test -p ptc-runtime --release --lib -- --ignored --nocapture startup`
    #[test]
    #[ignore = "a measurement, not a check"]
    fn startup_with_and_without_the_snapshot() {
        let glue = epochs::latest();
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _context = tokio.enter();
        crate::executor::init_v8();
        let begin = Instant::now();
        let snapshot = of(glue).unwrap();
        let built = begin.elapsed();
        let fresh = average(20, || {
            let mut runtime = JsRuntime::new(RuntimeOptions {
                extensions: ops::extensions(),
                ..Default::default()
            });
            runtime
                .execute_script("ptc:bootstrap", glue.bootstrap)
                .unwrap();
        });
        let restored = average(20, || {
            JsRuntime::new(RuntimeOptions {
                extensions: ops::extensions(),
                startup_snapshot: Some(snapshot),
                ..Default::default()
            });
        });
        eprintln!(
            "snapshot of {}: {} KiB, built once in {built:?}; a new isolate: {fresh:?} without \
             it, {restored:?} from it",
            glue.epoch,
            snapshot.len() / 1024
        );
    }
}
