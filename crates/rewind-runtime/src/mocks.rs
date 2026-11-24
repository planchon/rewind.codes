//! TypeScript mock handlers (`archi/05-testing-and-observability.md`, "Testing and mocks"), as a
//! [`ActionDispatcher`] for mock mode.
//!
//! The handlers run in an isolate of their own, on a thread of their own: nothing they do can
//! reach the code's isolate, so they cannot break its determinism, and what they return is
//! journaled like any action result. Their isolate is the sandbox of the latest epoch: the same
//! globals, randomness seeded with zeros, and the time of the clock the dispatcher is given.
//!
//! A module of mock handlers imports nothing, and default-exports its handlers, by full action
//! name or by namespace:
//!
//! ```ts
//! export default {
//!   "github.list_issues": ({ repo }) => [{ number: 4, title: "Crash", repo }],
//!   slack: {
//!     post_message: async ({ channel }, call) => ({ ok: true, channel, key: call.idempotencyKey }),
//!   },
//! };
//! ```
//!
//! A handler receives the call's arguments and `{ action, runId, seq, delivery, idempotencyKey }`,
//! and returns the result, or a promise of it. What it throws fails the call, with the error's
//! message, its `data`, and its `kind`: `error` (the default), `unavailable`, `timeout` or
//! `outcome_unknown`. A handler cannot wait for anything (no timer, signal or action): a promise
//! still pending once its microtasks are drained fails the call. An action without a handler fails
//! with `unavailable`; [`TsMocks::handles`] lets mock mode try another source first. `console`
//! goes to `tracing`, at the debug level.

use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};

use deno_core::{ModuleSpecifier, PollEventLoopOptions, v8};
use rewind_core::{
    ActionFailure, ActionFailureKind, ActionSet, Annotation, Code, RunId, Seed, Timestamp,
};
use rewind_engine::adapters::VirtualClock;
use rewind_engine::mock::Mocks as Answering;
use rewind_engine::ports::{ActionDispatcher, Call, CallAuth, Clock, Dispatched, Emitted};
use rewind_engine::testing::{MockLoadError, MockLoader};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::compile;
use crate::epochs;
use crate::isolate::{Isolate, SandboxLimits};
use crate::loader::{Loader, MOCKS_SPECIFIER};
use crate::memory;
use crate::ops::ExecState;

/// The glue of the mock isolate.
const MOCKS_JS: &str = include_str!("js/mocks.js");

/// A module of mock handlers that cannot be used.
/// Loads the TypeScript mock handlers of a test, for [`rewind_engine::testing`]: each test's
/// handlers in an isolate of their own, whose clock starts at the test's start.
#[derive(Debug, Clone, Default)]
pub struct TsMockLoader {
    pub limits: SandboxLimits,
}

impl MockLoader for TsMockLoader {
    type Handlers = TsMocks;

    async fn load(
        &self,
        source: &str,
        start: Timestamp,
    ) -> Result<Answering<TsMocks>, MockLoadError> {
        let clock: Arc<dyn Clock> = Arc::new(VirtualClock::new(start));
        let handlers = TsMocks::new(&Code::new(source), clock, self.limits.clone())
            .await
            .map_err(|error| match error {
                MockError::Invalid(message) => MockLoadError::Invalid(message),
                MockError::Internal(message) => MockLoadError::Internal(message),
            })?;
        let actions = handlers.actions().clone();
        Ok(Answering { handlers, actions })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MockError {
    /// A syntax error, an import, a default export that is not an object of handlers, an
    /// error thrown while the module loads. Written for the developer of the mocks.
    #[error("invalid mock handlers: {0}")]
    Invalid(String),
    /// A fault of the runtime itself.
    #[error("internal error of the mock handlers: {0}")]
    Internal(String),
}

/// TypeScript mock handlers, in an isolate of their own. Dropping it stops their thread.
pub struct TsMocks {
    // `mpsc::Sender` is `Send` but not `Sync`; the mutex makes the dispatcher `Sync`.
    requests: Mutex<mpsc::Sender<Request>>,
    actions: BTreeSet<String>,
}

impl std::fmt::Debug for TsMocks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TsMocks")
            .field("actions", &self.actions)
            .finish_non_exhaustive()
    }
}

struct Request {
    action: String,
    args: String,
    call: String,
    reply: oneshot::Sender<Dispatched>,
}

impl TsMocks {
    /// Compiles and loads a module of mock handlers. `clock` is the time the handlers see;
    /// `limits` bound each call (CPU time) and the isolate (heap).
    pub async fn new(
        source: &Code,
        clock: Arc<dyn Clock>,
        limits: SandboxLimits,
    ) -> Result<Self, MockError> {
        let compiled = compile::transpile(source, MOCKS_SPECIFIER)
            .map_err(|invalid| MockError::Invalid(invalid.message))?;
        let (requests, inbox) = mpsc::channel();
        let (loaded, actions) = oneshot::channel();
        let started_at = clock.now();
        std::thread::Builder::new()
            .name("rewind-mocks".into())
            .stack_size(memory::ISOLATE_THREAD_STACK)
            .spawn(move || serve(compiled, limits, started_at, clock, loaded, inbox))
            .map_err(|err| MockError::Internal(format!("cannot spawn the mock thread: {err}")))?;
        let actions = actions
            .await
            .map_err(|_| MockError::Internal("the mock thread has stopped".into()))??;
        Ok(Self {
            requests: Mutex::new(requests),
            actions,
        })
    }

    /// The actions that have a handler, sorted.
    pub fn actions(&self) -> &BTreeSet<String> {
        &self.actions
    }

    /// Whether `action` has a handler.
    pub fn handles(&self, action: &str) -> bool {
        self.actions.contains(action)
    }
}

impl ActionDispatcher for TsMocks {
    /// The handler's result or failure. An action without a handler fails with `unavailable`, for
    /// good: retrying would not make one appear.
    async fn dispatch(&self, call: Call, _auth: CallAuth) -> Dispatched {
        let action = call.action.name.clone();
        if !self.handles(&action) {
            return failed(
                ActionFailureKind::Unavailable,
                format!("no mock handler for {action}"),
            );
        }
        let context = json!({
            "action": action,
            // The name `action` had, for handlers already written (`archi/08-decisions.md`).
            "tool": action,
            "runId": call.run.id.to_string(),
            "seq": call.seq.0,
            "delivery": call.delivery,
            // The name `delivery` had, for handlers already written.
            "attempt": call.delivery,
            "idempotencyKey": call.idempotency_key,
        });
        let (reply, answer) = oneshot::channel();
        let request = Request {
            action,
            args: call.args.to_string(),
            call: context.to_string(),
            reply,
        };
        let sent = match self.requests.lock() {
            Ok(requests) => requests.send(request).is_ok(),
            Err(_) => false,
        };
        if !sent {
            return stopped();
        }
        answer.await.unwrap_or_else(|_| stopped())
    }
}

fn failed(kind: ActionFailureKind, message: String) -> Dispatched {
    Dispatched::Failed(ActionFailure {
        kind,
        message,
        data: None,
    })
}

fn stopped() -> Dispatched {
    failed(
        ActionFailureKind::Error,
        "the mock handlers have stopped: a previous call exceeded a limit".into(),
    )
}

/// The mock thread: load the handlers, then answer calls until the dispatcher is dropped.
fn serve(
    compiled: compile::Transpiled,
    limits: SandboxLimits,
    started_at: Timestamp,
    clock: Arc<dyn Clock>,
    loaded: oneshot::Sender<Result<BTreeSet<String>, MockError>>,
    inbox: mpsc::Receiver<Request>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = loaded.send(Err(MockError::Internal(err.to_string())));
            return;
        }
    };
    let _context = runtime.enter();
    let mut mocks = match runtime.block_on(Mocks::load(compiled, limits, started_at)) {
        Ok(mocks) => mocks,
        Err(err) => {
            let _ = loaded.send(Err(err));
            return;
        }
    };
    if loaded.send(Ok(mocks.actions.clone())).is_err() {
        return;
    }
    while let Ok(request) = inbox.recv() {
        let answer = mocks.call(&request, clock.now());
        let alive = !mocks.dead;
        let _ = request.reply.send(answer);
        if !alive {
            return;
        }
    }
}

struct Mocks {
    isolate: Isolate,
    call: v8::Global<v8::Function>,
    actions: BTreeSet<String>,
    dead: bool,
}

impl Mocks {
    async fn load(
        compiled: compile::Transpiled,
        limits: SandboxLimits,
        started_at: Timestamp,
    ) -> Result<Self, MockError> {
        let internal =
            |err: rewind_engine::ports::ExecutorError| MockError::Internal(err.to_string());
        let invalid = |err: &dyn std::fmt::Display| MockError::Invalid(err.to_string());
        let loader = Rc::new(Loader::mocks(
            compiled.javascript,
            compiled.source_map.into_bytes(),
        ));
        let state = ExecState::new(
            RunId::from_u128(0),
            &Seed::from_bytes([0; 32]),
            ActionSet::empty(),
            started_at,
        );
        let (mut isolate, _hooks) =
            Isolate::new(epochs::latest(), limits, loader, state).map_err(internal)?;
        let setup = isolate
            .runtime()
            .execute_script("rewind:mocks", MOCKS_JS)
            .map_err(|err| MockError::Internal(err.to_string()))?;

        let specifier = ModuleSpecifier::parse(MOCKS_SPECIFIER)
            .map_err(|err| MockError::Internal(err.to_string()))?;
        isolate.begin().map_err(internal)?;
        let evaluated = async {
            let runtime = isolate.runtime();
            let module = runtime
                .load_main_es_module(&specifier)
                .await
                .map_err(|err| invalid(&err))?;
            let evaluation = runtime.mod_evaluate(module);
            runtime
                .run_event_loop(PollEventLoopOptions::default())
                .await
                .map_err(|err| invalid(&err))?;
            evaluation.await.map_err(|err| invalid(&err))?;
            runtime
                .get_module_namespace(module)
                .map_err(|err| MockError::Internal(err.to_string()))
        }
        .await;
        isolate
            .end()
            .map_err(|err| MockError::Invalid(err.to_string()))?;
        let namespace = evaluated?;

        let (call, actions) = {
            let runtime = isolate.runtime();
            deno_core::scope!(scope, runtime);
            v8::tc_scope!(let scope, scope);
            let namespace = v8::Local::new(scope, namespace);
            let key = v8::String::new(scope, "default")
                .ok_or_else(|| MockError::Internal("cannot create a string".into()))?;
            let handlers = namespace
                .get(scope, key.into())
                .unwrap_or_else(|| v8::undefined(scope).into());
            let setup = v8::Local::new(scope, setup);
            let setup = v8::Local::<v8::Function>::try_from(setup)
                .map_err(|err| MockError::Internal(err.to_string()))?;
            let receiver = v8::undefined(scope).into();
            let Some(mocks) = setup.call(scope, receiver, &[handlers]) else {
                let message = scope.exception().map_or_else(
                    || "the default export cannot be read".into(),
                    |e| e.to_rust_string_lossy(scope),
                );
                return Err(MockError::Invalid(message));
            };
            let mocks = v8::Local::<v8::Object>::try_from(mocks)
                .map_err(|err| MockError::Internal(err.to_string()))?;
            let call = crate::isolate::function(scope, mocks, "call").map_err(internal)?;
            let key = v8::String::new(scope, "actions")
                .ok_or_else(|| MockError::Internal("cannot create a string".into()))?;
            let actions = mocks
                .get(scope, key.into())
                .and_then(|actions| v8::json::stringify(scope, actions))
                .map(|json| json.to_rust_string_lossy(scope))
                .ok_or_else(|| MockError::Internal("the mock actions cannot be read".into()))?;
            let actions: BTreeSet<String> = serde_json::from_str(&actions)
                .map_err(|err| MockError::Internal(err.to_string()))?;
            (call, actions)
        };
        Ok(Self {
            isolate,
            call,
            actions,
            dead: false,
        })
    }

    fn call(&mut self, request: &Request, now: Timestamp) -> Dispatched {
        self.isolate.state(|state| state.now = now);
        let answer = self.run(request);
        self.forward_logs(&request.action);
        match answer {
            Ok(dispatched) => dispatched,
            Err(message) => failed(ActionFailureKind::Error, message),
        }
    }

    fn run(&mut self, request: &Request) -> Result<Dispatched, String> {
        self.isolate.begin().map_err(|err| err.to_string())?;
        let settled = self.invoke(request);
        if let Err(limit) = self.isolate.end() {
            self.dead = true;
            return Err(format!(
                "the mock handler of {} {}",
                request.action,
                limit.to_string().trim_start_matches("the code ")
            ));
        }
        let json = settled?.ok_or_else(|| {
            format!(
                "the mock handler of {} did not settle: mock handlers cannot wait for timers, \
                 signals or actions",
                request.action
            )
        })?;
        let answer: Value = serde_json::from_str(&json).map_err(|err| err.to_string())?;
        if let Some(result) = answer.get("result") {
            return Ok(Dispatched::Result(result.clone()));
        }
        let failure = &answer["failure"];
        let kind = match failure["kind"].as_str() {
            Some("unavailable") => ActionFailureKind::Unavailable,
            Some("timeout") => ActionFailureKind::Timeout,
            Some("outcome_unknown") => ActionFailureKind::OutcomeUnknown,
            _ => ActionFailureKind::Error,
        };
        Ok(Dispatched::Failed(ActionFailure {
            kind,
            message: failure["message"].as_str().unwrap_or_default().to_owned(),
            data: failure.get("data").cloned().filter(|data| !data.is_null()),
        }))
    }

    /// Calls the handler and drains microtasks: the JSON answer, or `None` if the handler's
    /// promise is still pending.
    fn invoke(&mut self, request: &Request) -> Result<Option<String>, String> {
        let runtime = self.isolate.runtime();
        deno_core::scope!(scope, runtime);
        v8::tc_scope!(let scope, scope);
        let call = v8::Local::new(scope, &self.call);
        let receiver = v8::undefined(scope).into();
        let mut args: Vec<v8::Local<v8::Value>> = Vec::with_capacity(3);
        for arg in [&request.action, &request.args, &request.call] {
            let arg = v8::String::new(scope, arg).ok_or("the call is too large for V8")?;
            args.push(arg.into());
        }
        let promise = call.call(scope, receiver, &args);
        if promise.is_some() {
            scope.perform_microtask_checkpoint();
        }
        if scope.is_execution_terminating() || scope.has_terminated() {
            return Err("execution terminated".into());
        }
        let promise = promise
            .and_then(|value| v8::Local::<v8::Promise>::try_from(value).ok())
            .ok_or("the mock glue did not return a promise")?;
        match promise.state() {
            v8::PromiseState::Pending => Ok(None),
            v8::PromiseState::Fulfilled => {
                Ok(Some(promise.result(scope).to_rust_string_lossy(scope)))
            }
            v8::PromiseState::Rejected => Err(promise.result(scope).to_rust_string_lossy(scope)),
        }
    }

    fn forward_logs(&mut self, action: &str) {
        for emitted in self.isolate.state(ExecState::take_emitted) {
            if let Emitted::Annotation(Annotation::Log { message, .. }) = emitted {
                tracing::debug!(target: "rewind_runtime::mocks", action, "{message}");
            }
        }
    }
}
