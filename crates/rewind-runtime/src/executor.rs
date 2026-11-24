//! `V8Executor`: the engine's `CodeExecutor` port on V8.
//!
//! V8 objects cannot change threads, so each instance runs on a dedicated thread with its
//! own single-threaded tokio runtime (for module loading). The engine talks to it through
//! channels and never handles a V8 type: `V8Instance` is a `Send` handle that sends
//! deliveries to the thread and awaits the replies.

use std::collections::BTreeSet;
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use rewind_core::{ActionSet, Code, Epoch, Timestamp};
use rewind_engine::ports::{
    CodeExecutor, CodeInstance, CompiledCode, Emitted, ExecutorError, Handoff, InvalidCode,
    StartCode,
};
use tokio::sync::oneshot;

use crate::compile;
use crate::epochs::{self, EPOCHS, Glue};
use crate::instance::Instance;
use crate::isolate::SandboxLimits;
use crate::memory;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetupError {
    /// `Date` and `Intl` must not depend on the host (`archi/01-product.md`, "What exists in
    /// the sandbox"). Set `TZ=UTC` before the process starts any thread.
    #[error("Rewind requires TZ=UTC (found TZ={0:?})")]
    TimeZoneNotUtc(Option<String>),
}

/// Runs workflows in V8 isolates, under every determinism epoch whose glue this build embeds. A
/// clone runs workflows as the original does: each instance has a thread of its own.
#[derive(Debug, Clone)]
pub struct V8Executor {
    limits: SandboxLimits,
    epochs: Vec<Epoch>,
}

impl V8Executor {
    /// Checks the process (`TZ=UTC`) and initializes V8 and ICU (`en-US`) once per process.
    pub fn new(limits: SandboxLimits) -> Result<Self, SetupError> {
        match std::env::var("TZ") {
            Ok(tz) if tz == "UTC" => {}
            other => return Err(SetupError::TimeZoneNotUtc(other.ok())),
        }
        init_v8();
        Ok(Self {
            limits,
            epochs: EPOCHS.iter().map(|glue| glue.epoch).collect(),
        })
    }
}

/// Initializes V8 and ICU (`en-US`), once per process.
///
/// V8 runs single-threaded: it compiles and collects garbage on the isolate's thread only. With
/// concurrent compilation, optimized code was installed at a time that varied between two runs
/// of the same code, and the size of its frames with it: the depth at which recursion
/// overflows the stack, which a code can catch and observe, was not deterministic.
pub(crate) fn init_v8() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        deno_core::v8::V8::set_flags_from_string("--single-threaded");
        deno_core::JsRuntime::init_platform(None);
        deno_core::v8::icu::set_default_locale("en-US");
    });
}

impl CodeExecutor for V8Executor {
    type Instance = V8Instance;

    fn epochs(&self) -> &[Epoch] {
        &self.epochs
    }

    fn compile(&self, code: &Code, action_set: &ActionSet) -> Result<CompiledCode, InvalidCode> {
        compile::compile(code, action_set, epochs::latest())
    }

    fn action_imports(&self, code: &Code) -> Option<BTreeSet<String>> {
        compile::action_imports(code)
    }

    async fn start(&self, start: StartCode) -> Result<(V8Instance, Vec<Emitted>), ExecutorError> {
        let glue = epochs::glue(start.epoch).ok_or(ExecutorError::UnsupportedEpoch(start.epoch))?;
        let (requests, inbox) = mpsc::channel();
        let (started, emitted) = oneshot::channel();
        let limits = self.limits.clone();
        std::thread::Builder::new()
            .name(format!("rewind-isolate-{}", start.run_id))
            .stack_size(memory::ISOLATE_THREAD_STACK)
            .spawn(move || run_isolate(start, glue, limits, started, inbox))
            .map_err(|err| {
                ExecutorError::Internal(format!("cannot spawn the isolate thread: {err}"))
            })?;
        let ran = emitted.await.map_err(|_| stopped())?;
        let emitted = ran.emitted?;
        Ok((
            V8Instance {
                requests: Mutex::new(requests),
                memory: ran.memory,
                cpu: ran.cpu,
            },
            emitted,
        ))
    }
}

/// What a call into the isolate produced, the memory the isolate holds after it, and the CPU
/// time the call used, whether it succeeded or not.
struct Ran {
    emitted: Result<Vec<Emitted>, ExecutorError>,
    memory: u64,
    cpu: Duration,
}

type Reply = oneshot::Sender<Ran>;

struct Request {
    handoff: Handoff,
    now: Timestamp,
    reply: Reply,
}

/// A running code. Dropping it stops its thread and frees the isolate.
#[derive(Debug)]
pub struct V8Instance {
    // `mpsc::Sender` is `Send` but not `Sync`; the mutex makes the instance `Sync` as well.
    requests: Mutex<mpsc::Sender<Request>>,
    /// The memory the isolate held after its last call, measured on its thread.
    memory: u64,
    /// The CPU time of its last call, measured on its thread.
    cpu: Duration,
}

impl CodeInstance for V8Instance {
    async fn deliver(
        &mut self,
        handoff: Handoff,
        now: Timestamp,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        let (reply, emitted) = oneshot::channel();
        self.requests
            .get_mut()
            .map_err(|_| stopped())?
            .send(Request {
                handoff,
                now,
                reply,
            })
            .map_err(|_| stopped())?;
        let ran = emitted.await.map_err(|_| stopped())?;
        self.cpu = ran.cpu;
        if ran.emitted.is_ok() {
            self.memory = ran.memory;
        }
        ran.emitted
    }

    /// The isolate's heap statistics after its last call: its heap's total size, plus what V8
    /// allocated outside it (`malloc`, array buffers). The sticky cache sums it against its
    /// memory bound.
    fn memory_usage(&self) -> u64 {
        self.memory
    }

    /// The CPU time of the isolate's thread in the last call, as the watchdog measured it:
    /// loading the modules and `main(input)` after a start, one handoff after `deliver`.
    fn cpu_time(&self) -> Duration {
        self.cpu
    }
}

fn stopped() -> ExecutorError {
    ExecutorError::Internal("the isolate thread has stopped".into())
}

/// The isolate thread: load, start, then serve deliveries until the instance is dropped or
/// the isolate dies.
fn run_isolate(
    start: StartCode,
    glue: &'static Glue,
    limits: SandboxLimits,
    started: Reply,
    inbox: mpsc::Receiver<Request>,
) {
    // Module loading runs on this runtime, and V8 schedules its delayed tasks on it for the life
    // of the isolate.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = started.send(Ran {
                emitted: Err(ExecutorError::Internal(err.to_string())),
                memory: 0,
                cpu: Duration::ZERO,
            });
            return;
        }
    };
    let _context = runtime.enter();
    let mut isolate = match runtime.block_on(Instance::load(&start, glue, limits)) {
        Ok(isolate) => isolate,
        Err(err) => {
            let _ = started.send(Ran {
                emitted: Err(err),
                memory: 0,
                cpu: Duration::ZERO,
            });
            return;
        }
    };
    let result = isolate.start(&start.input);
    let alive = result.is_ok();
    if started.send(measured(&mut isolate, result)).is_err() || !alive {
        return;
    }
    while let Ok(request) = inbox.recv() {
        let result = isolate.deliver(&request.handoff, request.now);
        let alive = result.is_ok();
        let _ = request.reply.send(measured(&mut isolate, result));
        if !alive {
            return;
        }
    }
}

/// A call's result, with the memory the isolate holds after it and the CPU time it used.
fn measured(isolate: &mut Instance, result: Result<Vec<Emitted>, ExecutorError>) -> Ran {
    let cpu = isolate.take_cpu_time();
    let memory = if result.is_ok() {
        isolate.memory_usage()
    } else {
        0
    };
    Ran {
        emitted: result,
        memory,
        cpu,
    }
}
