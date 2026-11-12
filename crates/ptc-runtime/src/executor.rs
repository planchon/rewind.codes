//! `V8Executor`: the engine's `ScriptExecutor` port on V8.
//!
//! V8 objects cannot change threads, so each instance runs on a dedicated thread with its
//! own single-threaded tokio runtime (for module loading). The engine talks to it through
//! channels and never handles a V8 type: `V8Instance` is a `Send` handle that sends
//! deliveries to the thread and awaits the replies.

use std::sync::{Mutex, mpsc};

use ptc_core::{Epoch, Script, Timestamp, Toolset};
use ptc_engine::ports::{
    CompiledScript, Delivery, Emitted, ExecutorError, InvalidScript, ScriptExecutor,
    ScriptInstance, StartScript,
};
use tokio::sync::oneshot;

use crate::compile;
use crate::epochs::{self, EPOCHS, Glue};
use crate::instance::Instance;
use crate::isolate::SandboxLimits;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetupError {
    /// `Date` and `Intl` must not depend on the host (`archi/01-product.md`, "What exists in
    /// the sandbox"). Set `TZ=UTC` before the process starts any thread.
    #[error("ai-ptc requires TZ=UTC (found TZ={0:?})")]
    TimeZoneNotUtc(Option<String>),
}

/// Runs scripts in V8 isolates, under every determinism epoch whose glue this build embeds.
#[derive(Debug)]
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
pub(crate) fn init_v8() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        deno_core::JsRuntime::init_platform(None);
        deno_core::v8::icu::set_default_locale("en-US");
    });
}

impl ScriptExecutor for V8Executor {
    type Instance = V8Instance;

    fn epochs(&self) -> &[Epoch] {
        &self.epochs
    }

    fn compile(&self, script: &Script, toolset: &Toolset) -> Result<CompiledScript, InvalidScript> {
        compile::compile(script, toolset, epochs::latest())
    }

    async fn start(&self, start: StartScript) -> Result<(V8Instance, Vec<Emitted>), ExecutorError> {
        let glue = epochs::glue(start.epoch).ok_or(ExecutorError::UnsupportedEpoch(start.epoch))?;
        let (requests, inbox) = mpsc::channel();
        let (started, emitted) = oneshot::channel();
        let limits = self.limits.clone();
        std::thread::Builder::new()
            .name(format!("ptc-isolate-{}", start.run_id))
            .spawn(move || run_isolate(start, glue, limits, started, inbox))
            .map_err(|err| {
                ExecutorError::Internal(format!("cannot spawn the isolate thread: {err}"))
            })?;
        let ran = emitted.await.map_err(|_| stopped())??;
        Ok((
            V8Instance {
                requests: Mutex::new(requests),
                memory: ran.memory,
            },
            ran.emitted,
        ))
    }
}

/// What a call into the isolate produced, and the memory the isolate holds after it.
struct Ran {
    emitted: Vec<Emitted>,
    memory: u64,
}

type Reply = oneshot::Sender<Result<Ran, ExecutorError>>;

struct Request {
    delivery: Delivery,
    now: Timestamp,
    reply: Reply,
}

/// A running script. Dropping it stops its thread and frees the isolate.
#[derive(Debug)]
pub struct V8Instance {
    // `mpsc::Sender` is `Send` but not `Sync`; the mutex makes the instance `Sync` as well.
    requests: Mutex<mpsc::Sender<Request>>,
    /// The memory the isolate held after its last call, measured on its thread.
    memory: u64,
}

impl ScriptInstance for V8Instance {
    async fn deliver(
        &mut self,
        delivery: Delivery,
        now: Timestamp,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        let (reply, emitted) = oneshot::channel();
        self.requests
            .get_mut()
            .map_err(|_| stopped())?
            .send(Request {
                delivery,
                now,
                reply,
            })
            .map_err(|_| stopped())?;
        let ran = emitted.await.map_err(|_| stopped())??;
        self.memory = ran.memory;
        Ok(ran.emitted)
    }

    /// The isolate's heap statistics after its last call: its heap's total size, plus what V8
    /// allocated outside it (`malloc`, array buffers). The sticky cache sums it against its
    /// memory bound.
    fn memory_usage(&self) -> u64 {
        self.memory
    }
}

fn stopped() -> ExecutorError {
    ExecutorError::Internal("the isolate thread has stopped".into())
}

/// The isolate thread: load, start, then serve deliveries until the instance is dropped or
/// the isolate dies.
fn run_isolate(
    start: StartScript,
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
            let _ = started.send(Err(ExecutorError::Internal(err.to_string())));
            return;
        }
    };
    let _context = runtime.enter();
    let mut isolate = match runtime.block_on(Instance::load(&start, glue, limits)) {
        Ok(isolate) => isolate,
        Err(err) => {
            let _ = started.send(Err(err));
            return;
        }
    };
    let result = isolate.start(&start.input);
    let alive = result.is_ok();
    if started.send(measured(&mut isolate, result)).is_err() || !alive {
        return;
    }
    while let Ok(request) = inbox.recv() {
        let result = isolate.deliver(&request.delivery, request.now);
        let alive = result.is_ok();
        let _ = request.reply.send(measured(&mut isolate, result));
        if !alive {
            return;
        }
    }
}

/// A call's result, with the memory the isolate holds after it.
fn measured(
    isolate: &mut Instance,
    result: Result<Vec<Emitted>, ExecutorError>,
) -> Result<Ran, ExecutorError> {
    result.map(|emitted| Ran {
        emitted,
        memory: isolate.memory_usage(),
    })
}
