//! The call executor of a node (`archi/07-code-architecture.md`, "Inside a node";
//! `archi/04-tools.md`, "Tool task lifecycle", "Authenticating calls").
//!
//! An owner hands it a task once the task's batch is durable and the task holds its slots. The
//! executor prepares the call (its tool, its args, its idempotency key, its trace context, how it
//! proves who calls) and sends it, or prepares what the engine serves itself, and reports the
//! outcome to the owner, which commits it:
//!
//! ```text
//! Dispatch ─► job: prepare ─┬─ MCP, API, `http`, host tools in mock mode ─► dispatcher ─► Dispatched
//!                           ├─ `webhook` (live) ─────────────────────────────────────────► Webhook
//!                           ├─ `workflows.<name>.start` (live) ─ checks, the child's start ► Child
//!                           ├─ `workflows.<name>.signal` (live) ─ checks ────────────────► Signal
//!                           ├─ `workflows.<name>.start` (mock) ─ the mocks' answer, the end ► MockStart
//!                           └─ a host tool for an SDK stream ─ the hand-out ─────────────► HostReady
//! ```
//!
//! Nothing is written here: what a call became reaches the owner as a [`CallDone`].

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rewind_core::{
    Builtin, Interruption, JsonValue, Payload, RunEnd, RunId, RunMode, RunStarted, SecretScope,
    Seq, ToolAuth, ToolFailure, ToolFailureKind, ToolOutcome, ToolRoute, WorkflowOp,
};
use serde_json::json;

use crate::checks::{self, SignalCheck};
use crate::engine::idempotency_key;
use crate::executors::activation::Carried;
use crate::executors::derive::record_of;
use crate::fork::left_without_outcome;
use crate::node::message::{Effect, JobClass, Message, job_of};
use crate::partition::OwnerMsg;
use crate::plane::{LogOf, NodeConfig, Plane};
use crate::ports::link::{HostTask, StartAtOwner};
use crate::ports::{
    BlobOwner, CallAuth, CallClaims, CatalogStore, Clock, Dispatched, Entropy, ParkKind,
    ParkReason, PartitionLog, RunArchive, RunHead, RunMetadata, RunRef, RunWorkflow,
    ScriptExecutor, SecretId, SecretReader, SecretStore, StartedBy, TaskRecord, TaskRef, TaskToken,
    ToolCall, ToolDispatcher,
};
use crate::tasks::dispatch::{redacted, secret_texts};
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
/// attempt.
pub(crate) struct CallWork<P: Plane> {
    pub(crate) partition: rewind_core::PartitionId,
    pub(crate) log: Arc<LogOf<P>>,
    pub(crate) task: TaskRecord,
    pub(crate) head: RunHead,
    pub(crate) mode: Option<RunMode>,
    pub(crate) token: TaskToken,
    /// A host tool for an SDK stream: prepare its hand-out, send nothing.
    pub(crate) hand_out: bool,
}

/// What became of one attempt of a call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CallDone {
    pub(crate) task: TaskRef,
    pub(crate) attempt: u32,
    pub(crate) tool: String,
    pub(crate) outcome: CallOutcome,
}

/// The outcome of an attempt, for the owner to commit.
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
    /// `workflows.<name>.start` in mock mode: the mocks' answer stands for the child, whose end
    /// follows the start at once.
    MockStart {
        dispatched: Dispatched,
        end: Option<(RunId, JsonValue)>,
    },
    /// A host task ready to be handed to an SDK stream.
    HostReady(Box<HostTask>),
}

/// A workflow run a call starts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ChildStart {
    New(Box<StartAtOwner>),
    /// The call's key already started this run: a fork's call its parent left without an
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

/// Prepares and sends one attempt.
async fn call<P: Plane>(plane: &P, config: &NodeConfig, work: CallWork<P>) -> CallDone {
    let task = work.task.task.task.clone();
    let attempt = work.task.task.attempt;
    let tool = work.task.task.tool.clone();
    let span = tracing::info_span!(
        "tool_attempt",
        task = %task,
        attempt,
        traceparent = tracing::field::Empty,
        histogram.rewind.tool_attempt.duration = tracing::field::Empty,
        rewind.tool = tool.as_str(),
        rewind.outcome = tracing::field::Empty,
    );
    let outcome =
        tracing::Instrument::instrument(attempt_of(plane, config, work), span.clone()).await;
    if let CallOutcome::Dispatched(dispatched) | CallOutcome::MockStart { dispatched, .. } =
        &outcome
    {
        span.record(
            crate::metrics::OUTCOME,
            crate::metrics::attempt_outcome(dispatched),
        );
    }
    CallDone {
        task,
        attempt,
        tool,
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
    let toolset = match plane
        .catalog()
        .get_toolset(&run.tenant, record.toolset)
        .await
    {
        Ok(Some(stored)) => stored.toolset,
        Ok(None) => return failed(format!("toolset {} not found", record.toolset)),
        Err(error) => {
            return CallOutcome::NotSent(format!("the toolset could not be read: {error}"));
        }
    };
    let Some(tool) = toolset.tool(&record.tool).cloned() else {
        return failed(format!(
            "run {} called {}, which its toolset does not have",
            run.id, record.tool
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
        &record.tool,
        &record.args,
    )
    .await
    {
        Ok(key) => key,
        Err(error) => return CallOutcome::NotSent(error),
    };
    let trace_context = Some(crate::telemetry::tool_call_trace_context(
        &record_of(&head),
        record.task.seq,
        &config.telemetry,
    ));
    if let Some(context) = &trace_context {
        tracing::Span::current().record("traceparent", context.traceparent.as_str());
    }
    if hand_out {
        return CallOutcome::HostReady(Box::new(HostTask {
            token,
            task: record.task.clone(),
            namespace: head.namespace.clone(),
            tool: record.tool.clone(),
            args,
            idempotency_key: key,
            attempt: record.attempt,
            execution_timeout: record.policy.execution_timeout,
            labels: head.metadata.labels.clone(),
            trace_context,
        }));
    }
    let (server, api) = match &tool.route {
        ToolRoute::Mcp { server } => (toolset.server(server).cloned(), None),
        ToolRoute::Api { api } => (None, toolset.api(api).cloned()),
        ToolRoute::Builtin(_) | ToolRoute::Host { .. } => (None, None),
    };
    let auth = server
        .as_ref()
        .map(|server| server.auth.clone())
        .or_else(|| api.as_ref().map(|api| api.auth.clone()))
        .unwrap_or_default();
    let live = mode == Some(RunMode::Live);
    let call = ToolCall {
        run: run.clone(),
        namespace: head.namespace.clone(),
        labels: head.metadata.labels.clone(),
        seq: record.task.seq,
        tool,
        server,
        api,
        args,
        idempotency_key: key,
        attempt: record.attempt,
        schema_change_approved: record.schema_change_approved,
        token,
        trace_context,
    };
    match &call.tool.route {
        ToolRoute::Builtin(Builtin::Webhook) if live => {
            return webhook(config, &call);
        }
        ToolRoute::Builtin(Builtin::Workflow { workflow, op }) if live => {
            let workflow = workflow.clone();
            return match op {
                WorkflowOp::Start => {
                    start_child(
                        plane, config, &*log, partition, &call, &head, &toolset, &workflow,
                    )
                    .await
                }
                WorkflowOp::Signal => signal_child(plane, &*log, &call, &head, &workflow).await,
            };
        }
        ToolRoute::Builtin(Builtin::Workflow {
            op: WorkflowOp::Start,
            ..
        }) => {
            let answered = plane.dispatcher().dispatch(call, CallAuth::None).await;
            return mock_start(plane, partition, answered);
        }
        _ => {}
    }
    let auth = if auth == ToolAuth::None || !live {
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
    CallOutcome::Dispatched(Dispatched::Failed(ToolFailure {
        kind: ToolFailureKind::Error,
        message,
        data: None,
    }))
}

fn unavailable(message: String) -> CallOutcome {
    CallOutcome::Dispatched(Dispatched::Failed(ToolFailure {
        kind: ToolFailureKind::Unavailable,
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
/// inherited from its parent without an outcome, which carries its parent's key
/// (`archi/03-execution.md`, "Forks").
pub(crate) async fn call_key<P: Plane>(
    plane: &P,
    log: &LogOf<P>,
    head: &RunHead,
    seq: Seq,
    tool: &str,
    args: &Payload,
) -> Result<String, String> {
    let mut owner = head.run.id;
    let mut parent = head.metadata.parent;
    while let Some(link) = parent {
        let parent_ref = RunRef::new(head.run.tenant.clone(), link.run);
        let read = |error: String| format!("the parent {} could not be read: {error}", link.run);
        let Some(parent_head) = log
            .run(&parent_ref)
            .await
            .map_err(|error| read(error.to_string()))?
        else {
            break;
        };
        let history = if parent_head.sealed == Some(parent_head.attempt) {
            match plane
                .archive()
                .get(&parent_ref, parent_head.attempt)
                .await
                .map_err(|error| read(error.to_string()))?
            {
                Some(sealed) => sealed.entries,
                None => break,
            }
        } else {
            log.history(&parent_ref, parent_head.attempt, 0)
                .await
                .map_err(|error| read(error.to_string()))?
        };
        if !left_without_outcome(&history, seq, tool, args) {
            break;
        }
        owner = link.run;
        parent = parent_head.metadata.parent;
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
    call: &ToolCall,
    auth: &ToolAuth,
) -> Result<Result<CallAuth, CallOutcome>, String> {
    let tenant = &call.run.tenant;
    let server = call.tool.route.server().unwrap_or(&call.tool.name);
    let now = plane.clock().now();
    match auth {
        ToolAuth::None => Ok(Ok(CallAuth::None)),
        ToolAuth::Secret(placement) => {
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
        ToolAuth::Rewind => {
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
                tool: call.tool.name.clone(),
                idempotency_key: call.idempotency_key.clone(),
                seq: call.seq,
                attempt: call.attempt,
            };
            Ok(Ok(CallAuth::Signed { key, claims }))
        }
    }
}

/// `webhook(name)`: checked here, bound by the owner.
fn webhook(config: &NodeConfig, call: &ToolCall) -> CallOutcome {
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

/// `workflows.<name>.start` in mock mode: the mocks' answer is the child's end, and the start
/// resolves with a run id drawn from the engine's entropy (`archi/03-execution.md`, "Workflows
/// across runs").
fn mock_start<P: Plane>(
    plane: &P,
    partition: rewind_core::PartitionId,
    answered: Dispatched,
) -> CallOutcome {
    let end = match answered {
        Dispatched::Result(output) => RunEnd::from_mock(ToolOutcome::Result(output)),
        Dispatched::Failed(failure) => RunEnd::from_mock(ToolOutcome::Failure(failure)),
        transient @ Dispatched::Transient(_) => {
            return CallOutcome::MockStart {
                dispatched: transient,
                end: None,
            };
        }
        Dispatched::Deferred | Dispatched::Park(_) => {
            return failed("a mock cannot answer the start of a workflow later".into());
        }
    };
    let child = plane.entropy().run_id_in(partition);
    CallOutcome::MockStart {
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
    call: &ToolCall,
    head: &RunHead,
    toolset: &rewind_core::Toolset,
    name: &str,
) -> CallOutcome {
    let tenant = &call.run.tenant;
    let not_sent = |message: String| CallOutcome::NotSent(message);
    let Some(pinned) = toolset.workflow(name) else {
        return failed(format!("this run's toolset pins no workflow {name}"));
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
    let script = match plane.catalog().get_script(tenant, workflow.script()).await {
        Ok(Some(stored)) => stored.script,
        Ok(None) => return not_sent(format!("the script of {workflow} is not stored")),
        Err(error) => return not_sent(format!("the workflow's script could not be read: {error}")),
    };
    if let Err(error) = bounds.check_lines(&script) {
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
        parent: None,
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
    if let Err(nul) = crate::engine::refuse_nul_in_metadata(&metadata, Some(&key)) {
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
    CallOutcome::Child(Box::new(ChildStart::New(Box::new(StartAtOwner {
        run: child,
        namespace: head.namespace.clone(),
        started: RunStarted {
            script: workflow.script(),
            toolset: workflow.toolset(),
            input,
            seed,
            epoch,
            expires_at: now.saturating_add(ttl),
            mode: RunMode::Live,
        },
        metadata,
        start_key: Some(key),
        headline: crate::projector::headline(script.source()),
        created_at: now,
    }))))
}

/// `workflows.<name>.signal` of a live run: the target, checked against the caller's scope and
/// the target's version.
async fn signal_child<P: Plane>(
    plane: &P,
    log: &LogOf<P>,
    call: &ToolCall,
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

/// The failure of a task that must not be called again after an interruption: always under
/// `at_most_once`, past the maximum of attempts under `retry`.
pub(crate) fn interrupted(
    tool: &str,
    policy: Interruption,
    attempt: u32,
    max_attempts: u32,
) -> Option<ToolFailure> {
    let message = match policy {
        Interruption::AtMostOnce => format!(
            "the call to {tool} was interrupted and may or may not have taken effect; the tool \
             is at_most_once, so it is not called again"
        ),
        Interruption::Retry if attempt > max_attempts => format!(
            "the call to {tool} was interrupted after {} attempts and may or may not have taken \
             effect",
            attempt - 1
        ),
        Interruption::Retry => return None,
    };
    Some(ToolFailure {
        kind: ToolFailureKind::OutcomeUnknown,
        message,
        data: None,
    })
}

/// The wait a server asked for before a retry (`Retry-After`), if any.
pub(crate) fn asked_wait(failure: &ToolFailure) -> Duration {
    failure
        .data
        .as_ref()
        .and_then(|data| data.get(crate::ports::RETRY_AFTER_DATA))
        .and_then(serde_json::Value::as_u64)
        .map_or(Duration::ZERO, Duration::from_secs)
}
