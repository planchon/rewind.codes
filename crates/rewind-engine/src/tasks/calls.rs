//! The call executor of a node (`archi/07-code-architecture.md`, "Inside a node";
//! `archi/04-tools.md`, "Call lifecycle", "Authenticating calls").
//!
//! An owner hands it a task once the task's batch is durable and the task holds its slots. The
//! executor prepares the call (its action, its args, its idempotency key, its trace context, how it
//! proves who calls) and sends it, or prepares what the engine serves itself, and reports the
//! outcome to the owner, which commits it:
//!
//! ```text
//! Dispatch ─► job: prepare ─┬─ MCP, API, `http`, harness actions in a test run ─► dispatcher ─► Dispatched
//!                           ├─ `webhook` (live) ─────────────────────────────────────────► Webhook
//!                           ├─ `workflows.<name>.start` (live) ─ checks, the child's start ► Child
//!                           ├─ `workflows.<name>.signal` (live) ─ checks ────────────────► Signal
//!                           ├─ `workflows.<name>.start` (test) ─ the handlers' answer, the end ► TestStart
//!                           └─ a harness action for an SDK stream ─ the hand-out ─────────────► HarnessReady
//! ```
//!
//! Nothing is written here: what a call became reaches the owner as a [`CallDone`].

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rewind_core::{
    ActionAuth, ActionFailure, ActionFailureKind, ActionOutcome, ActionRoute, Builtin, JsonValue,
    Payload, RunEnd, RunId, RunMode, RunStarted, SecretScope, Seq, WorkflowOp,
};
use serde_json::json;

use crate::checks::{self, SignalCheck};
use crate::executors::activation::Carried;
use crate::fork::left_without_outcome;
use crate::node::message::{Effect, JobClass, Message, job_of};
use crate::partition::OwnerMsg;
use crate::plane::{LogOf, NodeConfig, Plane};
use crate::ports::link::{HarnessTask, StartAtOwner};
use crate::ports::{
    ActionDispatcher, BlobOwner, Call, CallAuth, CallClaims, CatalogStore, Clock, CodeExecutor,
    Dispatched, Entropy, ParkKind, ParkReason, PartitionLog, RunArchive, RunHead, RunMetadata,
    RunRef, RunWorkflow, SecretId, SecretReader, SecretStore, StartedBy, TaskRecord, TaskRef,
    TaskToken,
};
use crate::tasks::idempotency_key;
use crate::tasks::{redacted, secret_texts};
use crate::workflows::{SignalArgs, StartArgs};
use crate::{payloads, text};

/// A message to the call executor.
pub(crate) enum CallMsg<P: Plane> {
    Dispatch(Box<CallWork<P>>),
    Finished(rewind_core::PartitionId, Box<CallDone>),
}

impl<P: Plane> fmt::Debug for CallMsg<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallMsg::Dispatch(work) => write!(f, "Dispatch({})", work.task.task.task),
            CallMsg::Finished(_, done) => write!(f, "Finished({})", done.task),
        }
    }
}

/// A call an owner hands over: the task as its records say, its run, and the token of this
/// delivery.
pub(crate) struct CallWork<P: Plane> {
    pub(crate) partition: rewind_core::PartitionId,
    pub(crate) log: Arc<LogOf<P>>,
    pub(crate) task: TaskRecord,
    pub(crate) head: RunHead,
    pub(crate) mode: Option<RunMode>,
    pub(crate) token: TaskToken,
    /// A harness action for an SDK stream: prepare its hand-out, send nothing.
    pub(crate) hand_out: bool,
}

/// What became of one delivery of a call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CallDone {
    pub(crate) task: TaskRef,
    pub(crate) delivery: u32,
    pub(crate) action: String,
    pub(crate) outcome: CallOutcome,
}

/// The outcome of a delivery, for the owner to commit.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CallOutcome {
    /// The dispatcher's answer.
    Dispatched(Dispatched),
    /// The call could not be prepared for a reason that may pass: it was not sent; it is tried
    /// again after a backoff.
    NotSent(String),
    /// `webhook(name)` of a live run: the owner binds a token to the run and the name.
    Webhook { name: String },
    /// `workflows.<name>.start` of a live run, checked: the child to create, or the run its key
    /// already started.
    Child(Box<ChildStart>),
    /// `workflows.<name>.signal` of a live run, checked.
    Signal(Box<ChildSignal>),
    /// `workflows.<name>.start` in a test run: the handlers' answer stands for the child, whose end
    /// follows the start at once.
    TestStart {
        dispatched: Dispatched,
        end: Option<(RunId, JsonValue)>,
    },
    /// A harness task ready to be handed to an SDK stream.
    HarnessReady(Box<HarnessTask>),
}

/// A workflow run a call starts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ChildStart {
    /// A new run, with the runs of its namespace its handle must tell apart from.
    New {
        start: Box<StartAtOwner>,
        neighbours: Vec<RunId>,
    },
    /// The call's key already started this run: a fork's call its source left without an
    /// outcome. The caller watches it.
    Found(RunId),
}

/// A signal a call sends to a run of its namespace.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChildSignal {
    pub(crate) target: RunRef,
    pub(crate) name: String,
    pub(crate) payload: JsonValue,
    pub(crate) key: String,
}

/// The call executor of a node.
pub(crate) struct Calls<P: Plane> {
    plane: Arc<P>,
    config: Arc<NodeConfig>,
    waiting: VecDeque<Box<CallWork<P>>>,
    running: usize,
}

impl<P: Plane> fmt::Debug for Calls<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Calls")
            .field("waiting", &self.waiting.len())
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

impl<P: Plane> Calls<P> {
    pub(crate) fn new(plane: Arc<P>, config: Arc<NodeConfig>) -> Self {
        Self {
            plane,
            config,
            waiting: VecDeque::new(),
            running: 0,
        }
    }

    pub(crate) fn handle(&mut self, message: CallMsg<P>) -> Vec<Effect<P>> {
        let mut out = Vec::new();
        match message {
            CallMsg::Dispatch(work) => self.waiting.push_back(work),
            CallMsg::Finished(partition, done) => {
                self.running = self.running.saturating_sub(1);
                out.push(Effect::Local(Message::Owner(
                    partition,
                    OwnerMsg::CallDone(done),
                )));
            }
        }
        while self.running < self.config.calls.max(1) {
            let Some(work) = self.waiting.pop_front() else {
                break;
            };
            self.running += 1;
            let plane = Arc::clone(&self.plane);
            let config = Arc::clone(&self.config);
            let class = JobClass::Call(work.task.task.task.run.id);
            out.push(job_of(class, async move {
                let partition = work.partition;
                let done = call(&*plane, &config, *work).await;
                Message::Calls(CallMsg::Finished(partition, Box::new(done)))
            }));
        }
        out
    }
}

/// Prepares and sends one delivery.
async fn call<P: Plane>(plane: &P, config: &NodeConfig, work: CallWork<P>) -> CallDone {
    let task = work.task.task.task.clone();
    let delivery = work.task.task.delivery;
    let action = work.task.task.action.clone();
    let span = tracing::info_span!(
        "delivery",
        task = %task,
        delivery,
        traceparent = tracing::field::Empty,
        histogram.rewind.delivery.duration = tracing::field::Empty,
        rewind.action = action.as_str(),
        rewind.outcome = tracing::field::Empty,
    );
    let outcome =
        tracing::Instrument::instrument(attempt_of(plane, config, work), span.clone()).await;
    if let CallOutcome::Dispatched(dispatched) | CallOutcome::TestStart { dispatched, .. } =
        &outcome
    {
        span.record(
            crate::metrics::OUTCOME,
            crate::metrics::attempt_outcome(dispatched),
        );
    }
    CallDone {
        task,
        delivery,
        action,
        outcome,
    }
}

async fn attempt_of<P: Plane>(plane: &P, config: &NodeConfig, work: CallWork<P>) -> CallOutcome {
    let CallWork {
        partition,
        log,
        task,
        head,
        mode,
        token,
        hand_out,
    } = work;
    let record = &task.task;
    let run = &head.run;
    let action_set = match plane
        .catalog()
        .get_action_set(&run.tenant, record.action_set)
        .await
    {
        Ok(Some(stored)) => stored.action_set,
        Ok(None) => return failed(format!("action set {} not found", record.action_set)),
        Err(error) => {
            return CallOutcome::NotSent(format!("the action set could not be read: {error}"));
        }
    };
    let Some(action) = action_set.action(&record.action).cloned() else {
        return failed(format!(
            "run {} called {}, which its action set does not have",
            run.id, record.action
        ));
    };
    let source = Carried {
        records: Default::default(),
        blobs: plane.blobs(),
        owners: vec![BlobOwner::Run(run.clone())],
    };
    let args = match args_of(&*log, &source, run, &record.args).await {
        Ok(args) => args,
        Err(error) => return CallOutcome::NotSent(error),
    };
    let key = match call_key(
        plane,
        &*log,
        &head,
        record.task.seq,
        &record.action,
        &record.args,
    )
    .await
    {
        Ok(key) => key,
        Err(error) => return CallOutcome::NotSent(error),
    };
    let trace_context = Some(crate::telemetry::call_trace_context(
        head.run.id,
        &head.metadata,
        record.task.seq,
        &config.telemetry,
    ));
    if let Some(context) = &trace_context {
        tracing::Span::current().record("traceparent", context.traceparent.as_str());
    }
    if hand_out {
        return CallOutcome::HarnessReady(Box::new(HarnessTask {
            token,
            task: record.task.clone(),
            namespace: head.namespace.clone(),
            action: record.action.clone(),
            args,
            idempotency_key: key,
            delivery: record.delivery,
            execution_timeout: record.policy.execution_timeout,
            labels: head.metadata.labels.clone(),
            trace_context,
        }));
    }
    let (server, api) = match &action.route {
        ActionRoute::Mcp { server } => (action_set.server(server).cloned(), None),
        ActionRoute::Api { api } => (None, action_set.api(api).cloned()),
        ActionRoute::Builtin(_) | ActionRoute::Harness { .. } => (None, None),
    };
    let auth = server
        .as_ref()
        .map(|server| server.auth.clone())
        .or_else(|| api.as_ref().map(|api| api.auth.clone()))
        .unwrap_or_default();
    let live = mode == Some(RunMode::Live);
    let call = Call {
        run: run.clone(),
        namespace: head.namespace.clone(),
        labels: head.metadata.labels.clone(),
        seq: record.task.seq,
        action,
        server,
        api,
        args,
        idempotency_key: key,
        delivery: record.delivery,
        schema_change_approved: record.schema_change_approved,
        token,
        trace_context,
    };
    match &call.action.route {
        ActionRoute::Builtin(Builtin::Webhook) if live => {
            return webhook(config, &call);
        }
        ActionRoute::Builtin(Builtin::Workflow { workflow, op }) if live => {
            let workflow = workflow.clone();
            return match op {
                WorkflowOp::Start => {
                    start_child(
                        plane,
                        config,
                        &*log,
                        partition,
                        &call,
                        &head,
                        &action_set,
                        &workflow,
                    )
                    .await
                }
                WorkflowOp::Signal => signal_child(plane, &*log, &call, &head, &workflow).await,
            };
        }
        ActionRoute::Builtin(Builtin::Workflow {
            op: WorkflowOp::Start,
            ..
        }) => {
            let answered = plane.dispatcher().dispatch(call, CallAuth::None).await;
            return test_start(plane, partition, answered);
        }
        _ => {}
    }
    let auth = if auth == ActionAuth::None || !live {
        CallAuth::None
    } else {
        match live_auth(plane, config, &head, &call, &auth).await {
            Ok(Ok(auth)) => auth,
            Ok(Err(outcome)) => return outcome,
            Err(error) => return CallOutcome::NotSent(error),
        }
    };
    let texts = secret_texts(&auth);
    CallOutcome::Dispatched(redacted(
        plane.dispatcher().dispatch(call, auth).await,
        &texts,
    ))
}

fn failed(message: String) -> CallOutcome {
    CallOutcome::Dispatched(Dispatched::Failed(ActionFailure {
        kind: ActionFailureKind::Error,
        message,
        data: None,
    }))
}

fn unavailable(message: String) -> CallOutcome {
    CallOutcome::Dispatched(Dispatched::Failed(ActionFailure {
        kind: ActionFailureKind::Unavailable,
        message,
        data: None,
    }))
}

/// The call's args, loaded from where they live.
async fn args_of<L: PartitionLog, B: crate::ports::BlobStore>(
    log: &L,
    source: &Carried<'_, B>,
    run: &RunRef,
    args: &Payload,
) -> Result<JsonValue, String> {
    if let Payload::Digest(digest) = args
        && let Some(record) = log
            .payload(run, *digest)
            .await
            .map_err(|error| format!("the call's args could not be read: {error}"))?
        && let Some(value) = record.value
    {
        return Ok(rewind_core::canonicalize(&value));
    }
    payloads::load(source, &run.tenant, args)
        .await
        .map_err(|error| format!("the call's args could not be read: {error}"))
}

/// The idempotency key of the call `seq` of a run: `{run_id}/{seq}`, except for a call a fork
/// inherited from its source without an outcome, which carries its source's key
/// (`archi/03-execution.md`, "Forks").
pub(crate) async fn call_key<P: Plane>(
    plane: &P,
    log: &LogOf<P>,
    head: &RunHead,
    seq: Seq,
    action: &str,
    args: &Payload,
) -> Result<String, String> {
    let mut owner = head.run.id;
    let mut fork_source = head.metadata.source;
    while let Some(link) = fork_source {
        let source_ref = RunRef::new(head.run.tenant.clone(), link.run);
        let read = |error: String| format!("the source {} could not be read: {error}", link.run);
        let Some(source_head) = log
            .run(&source_ref)
            .await
            .map_err(|error| read(error.to_string()))?
        else {
            break;
        };
        let history = if source_head.sealed == Some(source_head.attempt) {
            match plane
                .archive()
                .get(&source_ref, source_head.attempt)
                .await
                .map_err(|error| read(error.to_string()))?
            {
                Some(sealed) => sealed.events,
                None => break,
            }
        } else {
            log.history(&source_ref, source_head.attempt, 0)
                .await
                .map_err(|error| read(error.to_string()))?
        };
        if !left_without_outcome(&history, seq, action, args) {
            break;
        }
        owner = link.run;
        fork_source = source_head.metadata.source;
    }
    Ok(idempotency_key(owner, seq))
}

/// How a live call proves who calls (`archi/04-tools.md`, "Authenticating calls"): the secret
/// the run attached for the server, read in the run's namespace and counted, or the tenant's
/// signing key. A secret that no longer exists parks the call before anything is sent.
async fn live_auth<P: Plane>(
    plane: &P,
    config: &NodeConfig,
    head: &RunHead,
    call: &Call,
    auth: &ActionAuth,
) -> Result<Result<CallAuth, CallOutcome>, String> {
    let tenant = &call.run.tenant;
    let server = call.action.route.server().unwrap_or(&call.action.name);
    let now = plane.clock().now();
    match auth {
        ActionAuth::None => Ok(Ok(CallAuth::None)),
        ActionAuth::Secret(placement) => {
            let Some(attached) = head.metadata.secrets.get(server) else {
                return Ok(Err(unavailable(format!(
                    "{server} needs a secret, and the run attached none for it: fork the run \
                     with \"secrets\": {{\"{server}\": \"org/<name>\" or \"ns/<name>\"}}"
                ))));
            };
            let id = SecretId::resolve(attached, &head.namespace);
            let reader = SecretReader::Run {
                run: call.run.id,
                namespace: head.namespace.clone(),
            };
            let read = plane
                .secrets()
                .read(tenant, &id, &reader, now)
                .await
                .map_err(|error| format!("the secret could not be read: {error}"))?;
            Ok(match read {
                Some(value) => Ok(CallAuth::Secret {
                    value,
                    placement: placement.clone(),
                }),
                None => {
                    let put = match attached.scope {
                        SecretScope::Organization => format!("PUT /v1/secrets/{}", attached.name),
                        SecretScope::Namespace => format!(
                            "PUT /v1/namespaces/{}/secrets/{}",
                            head.namespace, attached.name
                        ),
                    };
                    Err(CallOutcome::Dispatched(Dispatched::Park(ParkReason {
                        kind: ParkKind::SecretMissing,
                        message: format!(
                            "the secret {attached} the run attached for {server} no longer \
                             exists: put it again ({put}), and the call is sent with the same \
                             idempotency key"
                        ),
                    })))
                }
            })
        }
        ActionAuth::Rewind => {
            let Some(public_url) = &config.public_url else {
                return Ok(Err(unavailable(format!(
                    "{server} checks Rewind's signature, and this server has no public URL \
                     where its public keys are published (--public-url)"
                ))));
            };
            let key = plane
                .secrets()
                .signing_key(tenant, now)
                .await
                .map_err(|error| format!("the signing key could not be read: {error}"))?;
            let claims = CallClaims {
                issuer: CallClaims::issuer(public_url, tenant),
                subject: head.namespace.clone(),
                labels: head.metadata.labels.clone(),
                run: call.run.id,
                action: call.action.name.clone(),
                idempotency_key: call.idempotency_key.clone(),
                seq: call.seq,
                delivery: call.delivery,
            };
            Ok(Ok(CallAuth::Signed { key, claims }))
        }
    }
}

/// `webhook(name)`: checked here, bound by the owner.
fn webhook(config: &NodeConfig, call: &Call) -> CallOutcome {
    if config.public_url.is_none() {
        return unavailable("this server has no public URL, so it cannot receive webhooks".into());
    }
    let Some(name) = call.args.get("name").and_then(JsonValue::as_str) else {
        return failed(r#"webhook expects {"name": string}"#.into());
    };
    if let Err(refused) = text::refuse_nul("the webhook name", name) {
        return failed(refused.to_string());
    }
    CallOutcome::Webhook {
        name: name.to_owned(),
    }
}

/// `workflows.<name>.start` in a test run: the handlers' answer is the child's end, and the start
/// resolves with a run id drawn from the engine's entropy (`archi/03-execution.md`, "Workflows
/// across runs").
fn test_start<P: Plane>(
    plane: &P,
    partition: rewind_core::PartitionId,
    answered: Dispatched,
) -> CallOutcome {
    let end = match answered {
        Dispatched::Result(output) => RunEnd::from_test(ActionOutcome::Result(output)),
        Dispatched::Failed(failure) => RunEnd::from_test(ActionOutcome::Failure(failure)),
        transient @ Dispatched::Transient(_) => {
            return CallOutcome::TestStart {
                dispatched: transient,
                end: None,
            };
        }
        Dispatched::Deferred | Dispatched::Park(_) => {
            return failed("a handler cannot answer the start of a workflow later".into());
        }
    };
    let child = plane.entropy().run_id_in(partition);
    CallOutcome::TestStart {
        dispatched: Dispatched::Result(json!({ "runId": child.to_string() })),
        end: Some((child, end.to_json())),
    }
}

/// `workflows.<name>.start` of a live run: the checks of the start, and the child to create, of
/// the pinned version with the call's input, under the call's key, attached unless detached.
#[allow(clippy::too_many_arguments)]
async fn start_child<P: Plane>(
    plane: &P,
    config: &NodeConfig,
    log: &LogOf<P>,
    partition: rewind_core::PartitionId,
    call: &Call,
    head: &RunHead,
    action_set: &rewind_core::ActionSet,
    name: &str,
) -> CallOutcome {
    let tenant = &call.run.tenant;
    let not_sent = |message: String| CallOutcome::NotSent(message);
    let Some(pinned) = action_set.workflow(name) else {
        return failed(format!("this run's action set pins no workflow {name}"));
    };
    let workflow = match plane.catalog().get_workflow(tenant, pinned.version).await {
        Ok(Some(stored)) => stored.workflow,
        Ok(None) => return failed(format!("the workflow version {pinned} is not stored")),
        Err(error) => return not_sent(format!("the workflow could not be read: {error}")),
    };
    let args = match StartArgs::parse(&call.args) {
        Ok(args) => args,
        Err(message) => return failed(message),
    };
    let mut labels = head.metadata.labels.clone();
    for (key, value) in args.labels {
        if let Some(fixed) = head.metadata.scope.get(&key)
            && *fixed != value
        {
            return failed(format!(
                "the label {key:?} is {fixed:?} for every run this run starts: its scope fixes it"
            ));
        }
        labels.insert(key, value);
    }
    let scope = &head.metadata.scope;
    if !scope
        .iter()
        .all(|(key, value)| workflow.labels().get(key) == Some(value))
    {
        return failed(format!(
            "the workflow version {pinned} was not saved under this run's scope ({}): a run \
             started under labels starts only the versions saved under them",
            scope
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let bounds = match checks::bounds(plane.catalog(), config, tenant, &head.namespace).await {
        Ok(bounds) => bounds,
        Err(checks::BoundsError::Unavailable(error)) => {
            return not_sent(format!("the run's namespace could not be read: {error}"));
        }
        Err(checks::BoundsError::Deleting) => {
            return failed(format!(
                "no run of {workflow} was started: the namespace is being erased: it takes no run"
            ));
        }
    };
    let ttl = match checks::ttl(args.ttl, &bounds) {
        Ok(ttl) => ttl,
        Err(ttl) => {
            return failed(format!(
                "invalid ttl {ttl:?}: it must be positive and at most {:?}",
                bounds.max_ttl
            ));
        }
    };
    let code = match plane.catalog().get_script(tenant, workflow.code()).await {
        Ok(Some(stored)) => stored.code,
        Ok(None) => return not_sent(format!("the code of {workflow} is not stored")),
        Err(error) => return not_sent(format!("the workflow's code could not be read: {error}")),
    };
    if let Err(error) = bounds.check_lines(&code) {
        return failed(format!("no run of {workflow} was started: {error}"));
    }
    let input = Payload::Value(args.input);
    let size = payloads::value_size(&input).unwrap_or_default();
    if size > config.limits.max_payload_bytes {
        return failed(format!(
            "the input is {size} bytes as canonical JSON, above the limit of {} bytes",
            config.limits.max_payload_bytes
        ));
    }
    let Some(epoch) = plane.executor().epochs().last().copied() else {
        return not_sent("this node supports no determinism epoch".into());
    };
    let now = plane.clock().now();
    match checks::version_attachments(plane.secrets(), now, tenant, &head.namespace, &workflow)
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(refused)) => {
            return unavailable(format!("no run of {workflow} was started: {refused}"));
        }
        Err(error) => {
            return not_sent(format!("the workflow's secrets could not be read: {error}"));
        }
    }
    let metadata = RunMetadata {
        labels,
        trace_context: call.trace_context.clone(),
        source: None,
        workflow: Some(RunWorkflow {
            name: workflow.name().to_owned(),
            version: workflow.version(),
        }),
        started_by: Some(StartedBy {
            run: call.run.id,
            seq: call.seq,
            detached: args.detached,
        }),
        scope: head.metadata.scope.clone(),
        secrets: workflow.secrets().clone(),
    };
    let key = format!("rewind:{}", call.idempotency_key);
    if let Err(nul) = crate::start::refuse_nul_in_metadata(&metadata, Some(&key)) {
        return failed(nul.to_string());
    }
    // The child's id and seed are drawn before its key is looked up, as every start draws them.
    let child = RunRef::new(tenant.clone(), plane.entropy().run_id_in(partition));
    let seed = plane.entropy().seed();
    match log.started(tenant, &head.namespace, &key).await {
        Ok(Some(existing)) => return CallOutcome::Child(Box::new(ChildStart::Found(existing))),
        Ok(None) => {}
        Err(error) => return not_sent(format!("the start key could not be read: {error}")),
    }
    let neighbours = match crate::partition::runs::neighbours(log, &child, &head.namespace).await {
        Ok(neighbours) => neighbours,
        Err(error) => {
            return not_sent(format!(
                "the runs of the namespace could not be read: {error}"
            ));
        }
    };
    let start = Box::new(StartAtOwner {
        run: child,
        namespace: head.namespace.clone(),
        // A workflow's run is the run of its workflow.
        origin: workflow.name().to_owned(),
        started: RunStarted {
            code: workflow.code(),
            action_set: workflow.action_set(),
            input,
            seed,
            epoch,
            expires_at: now.saturating_add(ttl),
            mode: RunMode::Live,
        },
        metadata,
        start_key: Some(key),
        headline: crate::projector::headline(code.source()),
        created_at: now,
    });
    CallOutcome::Child(Box::new(ChildStart::New { start, neighbours }))
}

/// `workflows.<name>.signal` of a live run: the target, checked against the caller's scope and
/// the target's version.
async fn signal_child<P: Plane>(
    plane: &P,
    log: &LogOf<P>,
    call: &Call,
    head: &RunHead,
    name: &str,
) -> CallOutcome {
    let args = match SignalArgs::parse(&call.args) {
        Ok(args) => args,
        Err(message) => return failed(message),
    };
    let target = RunRef::new(call.run.tenant.clone(), args.run);
    let invisible = || {
        failed(format!(
            "no run {} of workflow {name} is visible to this run",
            args.run
        ))
    };
    let found = match log.run(&target).await {
        Ok(Some(found)) => found,
        Ok(None) => return invisible(),
        Err(error) => return CallOutcome::NotSent(format!("the run could not be read: {error}")),
    };
    let in_scope = found.namespace == head.namespace
        && head
            .metadata
            .scope
            .iter()
            .all(|(key, value)| found.metadata.labels.get(key) == Some(value));
    if !in_scope {
        return invisible();
    }
    if found.metadata.workflow.as_ref().map(|of| of.name.as_str()) != Some(name) {
        return failed(format!("run {} is not a run of workflow {name}", args.run));
    }
    if found.is_ended() {
        return failed(format!("run {} has ended", args.run));
    }
    match checks::signal(plane.catalog(), &found, &args.name, &args.payload).await {
        Ok(()) => {}
        Err(SignalCheck::Refused(refused)) => return failed(refused),
        Err(SignalCheck::Unavailable(error)) => {
            return CallOutcome::NotSent(format!("the run's workflow could not be read: {error}"));
        }
    }
    CallOutcome::Signal(Box::new(ChildSignal {
        target,
        name: args.name,
        payload: args.payload,
        key: format!("rewind:{}", call.idempotency_key),
    }))
}

/// The failure of a task that must not be called again after an interruption: always when its
/// action is not idempotent, past the maximum of deliveries when it is.
pub(crate) fn interrupted(
    action: &str,
    idempotent: bool,
    delivery: u32,
    max_deliveries: u32,
) -> Option<ActionFailure> {
    let message = if !idempotent {
        format!(
            "the call to {action} was interrupted and may or may not have taken effect; the action \
             is not idempotent, so it is not called again"
        )
    } else if delivery > max_deliveries {
        format!(
            "the call to {action} was interrupted after {} deliveries and may or may not have taken \
             effect",
            delivery - 1
        )
    } else {
        return None;
    };
    Some(ActionFailure {
        kind: ActionFailureKind::OutcomeUnknown,
        message,
        data: None,
    })
}

/// The wait a server asked for before a retry (`Retry-After`), if any.
pub(crate) fn asked_wait(failure: &ActionFailure) -> Duration {
    failure
        .data
        .as_ref()
        .and_then(|data| data.get(crate::ports::RETRY_AFTER_DATA))
        .and_then(serde_json::Value::as_u64)
        .map_or(Duration::ZERO, Duration::from_secs)
}
