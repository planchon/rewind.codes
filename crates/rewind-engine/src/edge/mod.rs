//! The use cases at the edge of the run plane (`archi/07-code-architecture.md`, "Inside a
//! node": the rules the engine keeps; `archi/03-execution.md`, "Starting a run", "Forks";
//! `archi/05-testing-and-observability.md`, "Read model", "Statistics").
//!
//! Every process is an edge. A use case checks what it can without the run's partition (the
//! script, the TTL, the input, the attachments, the namespace, read from the control plane), then
//! asks the owner of the run's partition through a [`Gateway`]: the router of a process
//! ([`NodeHandle`](crate::node::runtime::NodeHandle)) or the [`Driver`](crate::driver::Driver)
//! in tests. The owner keeps every rule that needs the partition: start keys, admission, signal
//! keys, the lifecycle of tasks, the reads.
//!
//! | Use case | The edge checks | The owner |
//! | --- | --- | --- |
//! | [`Edge::start_run`], [`Edge::start_workflow`], [`Edge::fork`] | the namespace, the script, the TTL, the input, the attachments | the start key, the admission, `CreateRun` |
//! | [`Edge::signal`], [`Edge::cancel`], [`Edge::webhook`] | NUL characters | the run, its workflow's signals, the signal key |
//! | [`Edge::release`], [`Edge::give_up`], [`Edge::erase`] | | the quarantine, the archive, the records |
//! | [`Edge::complete_task`], [`Edge::fail_task`], [`Edge::defer_task`], [`Edge::approve_task`], [`Edge::reject_task`] | the token's run | the task's state |
//! | [`Edge::get_run`], [`Edge::history`], [`Edge::export_journal`], [`Edge::tool_tasks`] | | the partition, or the archive |
//! | [`Edge::search`], [`Edge::statistics`], [`Edge::shapes`] | the partitions of the scope, merged | each partition's index and counters |

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rewind_core::{
    HistoryEntry, JournalDocument, JsonValue, PartitionId, Payload, RunId, RunMode, RunStarted,
    Script, Shape,
};

use crate::attachments::{self, AttachmentCheck};
use crate::checks::{self, BoundsError};
use crate::engine::{StartError, StartRun, refuse_nul_in_metadata, type_check};
use crate::fork::{ForkError, ForkRun};
use crate::plane::{NodeConfig, Plane};
use crate::ports::link::{
    Decision, HostFailure, NodeReply, PartitionReply, PartitionRequest, Refusal, RunView,
    SignalAtOwner, StartAtOwner, TaskAnswer,
};
use crate::ports::{
    BlobOwner, CatalogStore, Clock, Counters, Created, Enqueued, Entropy, Found, Gauges,
    NamespaceName, NamespaceWrite, Page, RunOrder, RunQuery, RunRef, RunWorkflow,
    ScriptExecutor, SearchPosition, Scope, StoreError, TaskRecord, TaskRef, TaskToken, TenantId,
    ToolTask, TypeChecker, Window, namespace_partition,
};
use crate::text::{self, NulCharacter};
use crate::workflows::StartWorkflow;
use crate::{payloads, projector};

/// Why an edge's request failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EdgeError {
    /// The owner refused it.
    #[error(transparent)]
    Refused(#[from] Refusal),
    /// No owner could be reached, or a store failed: the request may be tried again.
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    NulCharacter(#[from] NulCharacter),
}

/// How an edge reaches the owner of a partition.
pub trait Gateway: Send {
    /// Sends a request to the owner of `partition`, and waits for its reply.
    fn partition(
        &mut self,
        partition: PartitionId,
        request: PartitionRequest,
    ) -> impl Future<Output = Result<PartitionReply, EdgeError>> + Send;
}

/// Sends a request through a gateway and unwraps its reply.
async fn ask<G: Gateway>(
    gateway: &mut G,
    partition: PartitionId,
    request: PartitionRequest,
) -> Result<PartitionReply, EdgeError> {
    gateway.partition(partition, request).await
}

/// The edge of a process.
pub struct Edge<P: Plane> {
    plane: Arc<P>,
    config: Arc<NodeConfig>,
}

impl<P: Plane> std::fmt::Debug for Edge<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Edge")
            .field("node", &self.config.node)
            .finish_non_exhaustive()
    }
}

fn unavailable(error: impl ToString) -> EdgeError {
    EdgeError::Unavailable(error.to_string())
}

fn start_unavailable(error: impl ToString) -> StartError {
    StartError::Store(StoreError::Backend(format!(
        "unavailable: {}",
        error.to_string()
    )))
}

fn start_refused(refusal: Refusal) -> StartError {
    match refusal {
        Refusal::Quota(exceeded) => StartError::QuotaExceeded(exceeded),
        Refusal::NamespaceDeleting => StartError::NamespaceDeleting,
        other => start_unavailable(other),
    }
}

/// An unexpected reply.
fn unexpected(reply: &PartitionReply) -> EdgeError {
    EdgeError::Unavailable(format!("unexpected reply {reply:?}"))
}

impl<P: Plane> Edge<P> {
    pub fn new(plane: Arc<P>, config: NodeConfig) -> Self {
        Self {
            plane,
            config: Arc::new(config),
        }
    }

    pub fn plane(&self) -> &Arc<P> {
        &self.plane
    }

    /// The partition of a run, from its id: the 16 bits of a run of the run plane, the control
    /// plane's `legacy_runs` for a run of the previous design, the only one of an embedded
    /// deployment.
    pub async fn partition_of(&self, run: RunId) -> Result<PartitionId, EdgeError> {
        if let Some(partition) = run.partition() {
            return Ok(partition);
        }
        if let Some(partition) = self
            .plane
            .catalog()
            .legacy_partition(run)
            .await
            .map_err(unavailable)?
        {
            return Ok(partition);
        }
        if self.config.partitions.get() == 1 {
            return Ok(PartitionId::new(0));
        }
        Err(Refusal::RunNotFound(run).into())
    }

    /// The partition a token names, from its run.
    pub async fn partition_of_token(&self, token: &str) -> Result<(RunId, PartitionId), EdgeError> {
        let run = crate::partition::tokens::run_of(token).ok_or(Refusal::TaskGone)?;
        Ok((run, self.partition_of(run).await?))
    }

    // ---------------------------------------------------------------------------------------
    // Starts

    /// Checks the script and starts a run (`POST /v1/runs` with a script).
    pub async fn start_run<G: Gateway>(
        &self,
        gateway: &mut G,
        request: StartRun,
    ) -> Result<Created, StartError> {
        self.start(gateway, request, None::<&crate::adapters::StaticTypeChecker>)
            .await
    }

    /// [`start_run`](Self::start_run), type-checked first against the toolset's declarations.
    pub async fn start_run_checked<G: Gateway, K: TypeChecker>(
        &self,
        gateway: &mut G,
        request: StartRun,
        checker: &K,
    ) -> Result<Created, StartError> {
        self.start(gateway, request, Some(checker)).await
    }

    async fn start<G: Gateway, K: TypeChecker>(
        &self,
        gateway: &mut G,
        request: StartRun,
        checker: Option<&K>,
    ) -> Result<Created, StartError> {
        let namespace = self
            .namespace(&request.tenant, &request.namespace)
            .await?;
        let bounds = self.bounds(&request.tenant, &request.namespace).await?;
        let ttl = checks::ttl(request.ttl, &bounds).map_err(|requested| StartError::InvalidTtl {
            requested,
            max: bounds.max_ttl,
        })?;
        bounds.check_lines(&request.script)?;
        let input = self.bounded_input(request.input)?;
        text::refuse_nul("the script", request.script.source())?;
        text::refuse_nul(text::TOOLSET_TEXT, &request.declarations.typescript)?;
        text::refuse_nul(text::TOOLSET_TEXT, &request.declarations.prompt)?;
        refuse_nul_in_metadata(&request.metadata, request.idempotency_key.as_deref())?;
        let executor = self.plane.executor();
        let epoch = *executor.epochs().last().ok_or(StartError::NoEpoch)?;
        let compiled = executor
            .compile(&request.script, &request.toolset)
            .map_err(StartError::InvalidScript)?;
        if request.mode == RunMode::Live {
            self.check_attachments(
                &request.tenant,
                &request.namespace,
                &request.script,
                &request.toolset,
                &request.metadata.secrets,
                &request.attach,
            )
            .await?;
        }
        if let Some(checker) = checker {
            type_check(checker, &request.script, &request.declarations).await?;
        }
        let catalog = self.plane.catalog();
        catalog
            .put_script(&request.tenant, &request.script, &compiled)
            .await
            .map_err(start_unavailable)?;
        catalog
            .put_toolset(&request.tenant, &request.toolset, &request.declarations)
            .await
            .map_err(start_unavailable)?;
        let started = |seed| RunStarted {
            script: request.script.digest(),
            toolset: request.toolset.digest(),
            input,
            seed,
            epoch,
            expires_at: self.now().saturating_add(ttl),
            mode: request.mode,
        };
        let run = RunRef::new(
            request.tenant.clone(),
            self.plane.entropy().run_id_in(namespace),
        );
        let seed = self.plane.entropy().seed();
        self.create(
            gateway,
            namespace,
            StartAtOwner {
                run,
                namespace: request.namespace,
                started: started(seed),
                metadata: request.metadata,
                start_key: request.idempotency_key,
                headline: projector::headline(request.script.source()),
                created_at: self.now(),
            },
        )
        .await
    }

    /// Starts a run of a workflow version (`POST /v1/runs` with a workflow): its input checked
    /// against the version's schema, its attachments the version's.
    pub async fn start_workflow<G: Gateway>(
        &self,
        gateway: &mut G,
        request: StartWorkflow,
    ) -> Result<Created, StartError> {
        let catalog = self.plane.catalog();
        let stored = catalog
            .get_workflow(&request.tenant, request.version)
            .await
            .map_err(start_unavailable)?
            .ok_or_else(|| StartError::WorkflowNotFound(request.version.to_hex()))?;
        let workflow = stored.workflow;
        if let Err(refused) = crate::schema::Validator::compile(&workflow.schemas().input)
            .check("input", &request.input)
        {
            return Err(StartError::InvalidInput(crate::workflows::refused_message(
                &format!("the input schema of {workflow}"),
                refused,
            )));
        }
        let partition = self.namespace(&request.tenant, &request.namespace).await?;
        let bounds = self.bounds(&request.tenant, &request.namespace).await?;
        let ttl = checks::ttl(request.ttl, &bounds).map_err(|requested| StartError::InvalidTtl {
            requested,
            max: bounds.max_ttl,
        })?;
        let script = catalog
            .get_script(&request.tenant, workflow.script())
            .await
            .map_err(start_unavailable)?
            .ok_or_else(|| start_unavailable(format!("the script of {workflow} is not stored")))?;
        bounds.check_lines(&script.script)?;
        match checks::version_attachments(
            self.plane.secrets(),
            self.now(),
            &request.tenant,
            &request.namespace,
            &workflow,
        )
        .await?
        {
            Ok(()) => {}
            Err(refused) => return Err(StartError::SecretRequired(refused)),
        }
        let input = self.bounded_input(request.input)?;
        let mut metadata = request.metadata;
        metadata.workflow = Some(RunWorkflow {
            name: workflow.name().to_owned(),
            version: workflow.version(),
        });
        metadata.secrets = workflow.secrets().clone();
        refuse_nul_in_metadata(&metadata, request.idempotency_key.as_deref())?;
        let epoch = *self
            .plane
            .executor()
            .epochs()
            .last()
            .ok_or(StartError::NoEpoch)?;
        let run = RunRef::new(request.tenant.clone(), self.plane.entropy().run_id_in(partition));
        let seed = self.plane.entropy().seed();
        self.create(
            gateway,
            partition,
            StartAtOwner {
                run,
                namespace: request.namespace,
                started: RunStarted {
                    script: workflow.script(),
                    toolset: workflow.toolset(),
                    input,
                    seed,
                    epoch,
                    expires_at: self.now().saturating_add(ttl),
                    mode: RunMode::Live,
                },
                metadata,
                start_key: request.idempotency_key,
                headline: projector::headline(script.script.source()),
                created_at: self.now(),
            },
        )
        .await
    }

    /// Forks a run with a corrected script (`POST /v1/runs/{id}/fork`). The fork lives in its
    /// parent's namespace, and so in its partition.
    pub async fn fork<G: Gateway>(&self, gateway: &mut G, request: ForkRun) -> Result<Created, ForkError> {
        self.fork_with(gateway, request, None::<&crate::adapters::StaticTypeChecker>)
            .await
    }

    /// [`fork`](Self::fork), type-checked first against the parent's toolset.
    pub async fn fork_checked<G: Gateway, K: TypeChecker>(
        &self,
        gateway: &mut G,
        request: ForkRun,
        checker: &K,
    ) -> Result<Created, ForkError> {
        self.fork_with(gateway, request, Some(checker)).await
    }

    async fn fork_with<G: Gateway, K: TypeChecker>(
        &self,
        gateway: &mut G,
        request: ForkRun,
        checker: Option<&K>,
    ) -> Result<Created, ForkError> {
        let parent = request.parent.clone();
        let start_failed = |error: StartError| ForkError::Start(error);
        let edge_failed = |error: EdgeError| ForkError::Start(start_unavailable(error));
        let partition = self.partition_of(parent.id).await.map_err(|_| ForkError::ParentNotFound(parent.id))?;
        let view = self
            .get_run(gateway, &parent)
            .await
            .map_err(edge_failed)?
            .ok_or(ForkError::ParentNotFound(parent.id))?;
        let history = self
            .history(gateway, &parent, 0, true)
            .await
            .map_err(edge_failed)?
            .unwrap_or_default();
        let Some(HistoryEntry {
            event:
                rewind_core::Event::Lifecycle(rewind_core::Lifecycle::RunStarted(parent_started)),
            ..
        }) = history.first()
        else {
            return Err(ForkError::ParentNotStarted(parent.id));
        };
        let head = view.head;
        let bounds = self
            .bounds(&parent.tenant, &head.namespace)
            .await
            .map_err(start_failed)?;
        let ttl = checks::ttl(request.ttl, &bounds).map_err(|requested| {
            start_failed(StartError::InvalidTtl {
                requested,
                max: bounds.max_ttl,
            })
        })?;
        bounds.check_lines(&request.script).map_err(start_failed)?;
        let refused = |nul| ForkError::Start(StartError::NulCharacter(nul));
        text::refuse_nul("the script", request.script.source()).map_err(refused)?;
        let metadata = crate::ports::RunMetadata {
            labels: request.labels.unwrap_or(head.metadata.labels.clone()),
            trace_context: request.trace_context,
            parent: Some(crate::ports::ForkParent {
                run: parent.id,
                at: request.at,
            }),
            workflow: None,
            started_by: None,
            scope: head.metadata.scope.clone(),
            secrets: request
                .secrets
                .clone()
                .unwrap_or_else(|| head.metadata.secrets.clone()),
        };
        refuse_nul_in_metadata(&metadata, request.idempotency_key.as_deref()).map_err(refused)?;
        let stored = self
            .plane
            .catalog()
            .get_toolset(&parent.tenant, head.toolset)
            .await
            .map_err(|error| start_failed(start_unavailable(error)))?
            .ok_or_else(|| start_failed(start_unavailable(format!("toolset {} not found", head.toolset))))?;
        let toolset = stored.toolset;
        let compiled = self
            .plane
            .executor()
            .compile(&request.script, &toolset)
            .map_err(|invalid| start_failed(StartError::InvalidScript(invalid)))?;
        if parent_started.mode == RunMode::Live {
            self.check_attachments(
                &parent.tenant,
                &head.namespace,
                &request.script,
                &toolset,
                &metadata.secrets,
                &request.attach,
            )
            .await
            .map_err(start_failed)?;
        }
        if let Some(checker) = checker {
            type_check(checker, &request.script, &stored.declarations)
                .await
                .map_err(start_failed)?;
        }
        let input = match request.input {
            Some(input) => input,
            None => {
                let source = crate::executors::activation::Carried {
                    records: Default::default(),
                    blobs: self.plane.blobs(),
                    owners: vec![BlobOwner::Run(parent.clone())],
                };
                payloads::load(&source, &parent.tenant, &parent_started.input)
                    .await
                    .map_err(|error| ForkError::Engine(error.into()))?
            }
        };
        let input = self.bounded_input(input).map_err(start_failed)?;
        self.plane
            .catalog()
            .put_script(&parent.tenant, &request.script, &compiled)
            .await
            .map_err(|error| start_failed(start_unavailable(error)))?;
        let run = RunRef::new(parent.tenant.clone(), self.plane.entropy().run_id_in(partition));
        let start = StartAtOwner {
            run,
            namespace: head.namespace.clone(),
            started: RunStarted {
                script: request.script.digest(),
                toolset: head.toolset,
                input,
                seed: parent_started.seed,
                epoch: parent_started.epoch,
                expires_at: self.now().saturating_add(ttl),
                mode: parent_started.mode,
            },
            metadata,
            start_key: request.idempotency_key,
            headline: projector::headline(request.script.source()),
            created_at: self.now(),
        };
        self.create(gateway, partition, start)
            .await
            .map_err(start_failed)
    }

    async fn create<G: Gateway>(
        &self,
        gateway: &mut G,
        partition: PartitionId,
        start: StartAtOwner,
    ) -> Result<Created, StartError> {
        let reply = ask(gateway, partition, PartitionRequest::Start(Box::new(start)))
            .await
            .map_err(start_unavailable)?;
        match reply {
            PartitionReply::Started(Ok(created)) => {
                tracing::debug!(run = %created.run, created = created.created, "run started");
                Ok(created)
            }
            PartitionReply::Started(Err(refusal)) => Err(start_refused(refusal)),
            other => Err(start_unavailable(unexpected(&other))),
        }
    }

    /// The namespace's partition, the namespace created at its first use.
    async fn namespace(&self, tenant: &TenantId, name: &NamespaceName) -> Result<PartitionId, StartError> {
        match self
            .plane
            .catalog()
            .ensure_namespace(tenant, name, self.config.partitions, self.now())
            .await
            .map_err(start_unavailable)?
        {
            NamespaceWrite::Written(namespace) => Ok(namespace.partition),
            NamespaceWrite::Deleting => Err(StartError::NamespaceDeleting),
        }
    }

    async fn bounds(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
    ) -> Result<crate::engine::NamespaceBounds, StartError> {
        checks::bounds(self.plane.catalog(), &self.config, tenant, namespace)
            .await
            .map_err(|error| match error {
                BoundsError::Deleting => StartError::NamespaceDeleting,
                BoundsError::Unavailable(error) => start_unavailable(error),
            })
    }

    fn bounded_input(&self, input: JsonValue) -> Result<Payload, StartError> {
        let input = Payload::Value(input);
        let size = payloads::value_size(&input).unwrap_or_default();
        let max = self.config.limits.max_payload_bytes;
        if size > max {
            return Err(StartError::InputTooLarge { size, max });
        }
        Ok(input)
    }

    #[allow(clippy::too_many_arguments)]
    async fn check_attachments(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        script: &Script,
        toolset: &rewind_core::Toolset,
        attachments: &rewind_core::Attachments,
        rights: &attachments::AttachRights,
    ) -> Result<(), StartError> {
        if attachments.is_empty() && toolset.taking_secrets().next().is_none() {
            return Ok(());
        }
        let imports = self.plane.executor().tool_imports(script);
        let check = AttachmentCheck {
            tenant,
            namespace: Some(namespace),
            toolset,
            imports: imports.as_ref(),
            attachments,
            rights,
        };
        attachments::check(self.plane.secrets(), self.now(), check)
            .await?
            .map_err(StartError::SecretRequired)
    }

    fn now(&self) -> rewind_core::Timestamp {
        self.plane.clock().now()
    }

    // ---------------------------------------------------------------------------------------
    // The way in

    /// Sends a signal to a run.
    pub async fn signal<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
        name: impl Into<String>,
        payload: JsonValue,
        key: Option<String>,
    ) -> Result<Enqueued, EdgeError> {
        if let Some(key) = &key {
            text::refuse_nul("the idempotency key", key)?;
        }
        let partition = self.partition_of(run.id).await?;
        let request = PartitionRequest::Signal(SignalAtOwner {
            run: run.clone(),
            name: name.into(),
            payload,
            key,
        });
        enqueued(ask(gateway, partition, request).await?)
    }

    /// Asks a run to cancel.
    pub async fn cancel<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
        message: Option<String>,
    ) -> Result<Enqueued, EdgeError> {
        let partition = self.partition_of(run.id).await?;
        let request = PartitionRequest::Cancel {
            run: run.clone(),
            message,
        };
        enqueued(ask(gateway, partition, request).await?)
    }

    /// A request posted to a webhook's URL: the signal it is bound to.
    pub async fn webhook<G: Gateway>(
        &self,
        gateway: &mut G,
        token: &str,
        payload: JsonValue,
        key: Option<String>,
    ) -> Result<Enqueued, EdgeError> {
        let (_, partition) = self.partition_of_token(token).await.map_err(|_| {
            EdgeError::Unavailable("this webhook is bound to no run".into())
        })?;
        let request = PartitionRequest::Webhook {
            token: token.to_owned(),
            payload,
            key,
        };
        enqueued(ask(gateway, partition, request).await?)
    }

    /// Lifts a quarantine; returns whether the run was quarantined.
    pub async fn release<G: Gateway>(&self, gateway: &mut G, run: &RunRef) -> Result<bool, EdgeError> {
        self.done(gateway, run, PartitionRequest::Release { run: run.clone() }).await
    }

    /// Ends a quarantined run; returns whether it was quarantined.
    pub async fn give_up<G: Gateway>(&self, gateway: &mut G, run: &RunRef) -> Result<bool, EdgeError> {
        self.done(gateway, run, PartitionRequest::GiveUp { run: run.clone() }).await
    }

    /// Erases a run now (`DELETE /v1/runs/{id}`); returns whether it existed.
    pub async fn erase<G: Gateway>(&self, gateway: &mut G, run: &RunRef) -> Result<bool, EdgeError> {
        self.done(gateway, run, PartitionRequest::Erase { run: run.clone() }).await
    }

    async fn done<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
        request: PartitionRequest,
    ) -> Result<bool, EdgeError> {
        let partition = self.partition_of(run.id).await?;
        match ask(gateway, partition, request).await? {
            PartitionReply::Done(done) => Ok(done?),
            other => Err(unexpected(&other)),
        }
    }

    // ---------------------------------------------------------------------------------------
    // Tasks

    /// A handler's result (`POST /v1/tasks/{token}/complete`).
    pub async fn complete_task<G: Gateway>(
        &self,
        gateway: &mut G,
        tenant: &TenantId,
        token: &TaskToken,
        result: JsonValue,
    ) -> Result<(), EdgeError> {
        self.answer(gateway, tenant, token, TaskAnswer::Complete(result)).await
    }

    /// A handler's failure (`POST /v1/tasks/{token}/fail`).
    pub async fn fail_task<G: Gateway>(
        &self,
        gateway: &mut G,
        tenant: &TenantId,
        token: &TaskToken,
        failure: HostFailure,
    ) -> Result<(), EdgeError> {
        self.answer(gateway, tenant, token, TaskAnswer::Fail(failure)).await
    }

    /// The handler answers later (`POST /v1/tasks/{token}/defer`).
    pub async fn defer_task<G: Gateway>(
        &self,
        gateway: &mut G,
        tenant: &TenantId,
        token: &TaskToken,
        timeout: Option<Duration>,
    ) -> Result<(), EdgeError> {
        self.answer(gateway, tenant, token, TaskAnswer::Defer { timeout }).await
    }

    async fn answer<G: Gateway>(
        &self,
        gateway: &mut G,
        tenant: &TenantId,
        token: &TaskToken,
        answer: TaskAnswer,
    ) -> Result<(), EdgeError> {
        let (_, partition) = self.partition_of_token(&token.0).await?;
        let request = PartitionRequest::Answer {
            tenant: tenant.clone(),
            token: token.clone(),
            answer,
        };
        match ask(gateway, partition, request).await? {
            PartitionReply::Answered(answered) => Ok(answered?),
            other => Err(unexpected(&other)),
        }
    }

    /// Lets a task that waits for the harness go on.
    pub async fn approve_task<G: Gateway>(&self, gateway: &mut G, task: &TaskRef) -> Result<(), EdgeError> {
        self.decide(gateway, task, Decision::Approve).await
    }

    /// Refuses a task that waits for the harness: the call fails with a `ToolError`.
    pub async fn reject_task<G: Gateway>(
        &self,
        gateway: &mut G,
        task: &TaskRef,
        message: Option<String>,
    ) -> Result<(), EdgeError> {
        self.decide(gateway, task, Decision::Reject(message)).await
    }

    async fn decide<G: Gateway>(
        &self,
        gateway: &mut G,
        task: &TaskRef,
        decision: Decision,
    ) -> Result<(), EdgeError> {
        let partition = self.partition_of(task.run.id).await?;
        let request = PartitionRequest::Decide {
            task: task.clone(),
            decision,
        };
        match ask(gateway, partition, request).await? {
            PartitionReply::Answered(answered) => Ok(answered?),
            other => Err(unexpected(&other)),
        }
    }

    /// The run's tool tasks, by seq.
    pub async fn tool_tasks<G: Gateway>(&self, gateway: &mut G, run: &RunRef) -> Result<Vec<ToolTask>, EdgeError> {
        Ok(self
            .task_records(gateway, run)
            .await?
            .into_iter()
            .map(|record| record.task)
            .collect())
    }

    /// The run's tool tasks as its partition keeps them.
    pub async fn task_records<G: Gateway>(&self, gateway: &mut G, run: &RunRef) -> Result<Vec<TaskRecord>, EdgeError> {
        let partition = self.partition_of(run.id).await?;
        match ask(gateway, partition, PartitionRequest::Tasks { run: run.clone() }).await? {
            PartitionReply::Tasks(tasks) => tasks?.ok_or(Refusal::RunNotFound(run.id).into()),
            other => Err(unexpected(&other)),
        }
    }

    // ---------------------------------------------------------------------------------------
    // Reads

    /// The run, live or sealed.
    pub async fn get_run<G: Gateway>(&self, gateway: &mut G, run: &RunRef) -> Result<Option<RunView>, EdgeError> {
        let partition = self.partition_of(run.id).await?;
        match ask(gateway, partition, PartitionRequest::GetRun { run: run.clone() }).await? {
            PartitionReply::Run(view) => Ok(view?),
            other => Err(unexpected(&other)),
        }
    }

    /// The entries of the run's attempt from `from`: from its partition, or its sealed journal.
    pub async fn history<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
        from: u64,
        values: bool,
    ) -> Result<Option<Vec<HistoryEntry>>, EdgeError> {
        let partition = self.partition_of(run.id).await?;
        let request = PartitionRequest::History {
            run: run.clone(),
            from,
            values,
        };
        match ask(gateway, partition, request).await? {
            PartitionReply::History(history) => Ok(history?),
            other => Err(unexpected(&other)),
        }
    }

    /// The run's journal with every payload loaded: a self-contained document.
    pub async fn export_journal<G: Gateway>(&self, gateway: &mut G, run: &RunRef) -> Result<JournalDocument, EdgeError> {
        let view = self
            .get_run(gateway, run)
            .await?
            .ok_or(Refusal::RunNotFound(run.id))?;
        let mut entries = self
            .history(gateway, run, 0, true)
            .await?
            .ok_or(Refusal::RunNotFound(run.id))?;
        let source = crate::executors::activation::Carried {
            records: Default::default(),
            blobs: self.plane.blobs(),
            owners: vec![BlobOwner::Run(run.clone())],
        };
        payloads::load_all(&source, &run.tenant, &mut entries)
            .await
            .map_err(unavailable)?;
        let catalog = self.plane.catalog();
        let script = catalog
            .get_script(&run.tenant, view.head.script)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| unavailable(format!("script {} not found", view.head.script)))?;
        let toolset = catalog
            .get_toolset(&run.tenant, view.head.toolset)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| unavailable(format!("toolset {} not found", view.head.toolset)))?;
        Ok(JournalDocument {
            run: run.id,
            script: script.script,
            toolset: toolset.toolset,
            entries,
        })
    }

    /// Replays a run's journal with its script, or another one, on this node's executor:
    /// whether it reproduces the history. Nothing is written and no tool is called.
    pub async fn replay<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
        script: Option<&Script>,
    ) -> Result<crate::engine::ReplayVerdict, EdgeError> {
        let journal = self.export_journal(gateway, run).await?;
        let script = script.cloned().unwrap_or(journal.script.clone());
        let executor = self.plane.executor();
        let compiled = executor.compile(&script, &journal.toolset);
        let input = crate::replay::ReplayInput {
            run,
            compiled: compiled.as_ref(),
            toolset: &journal.toolset,
            history: &journal.entries,
        };
        let blobs = self.plane.blobs();
        let source = crate::executors::activation::Carried {
            records: Default::default(),
            blobs,
            owners: vec![BlobOwner::Run(run.clone())],
        };
        Ok(match crate::replay::replay(executor, &source, input).await {
            Ok(rebuilt) => crate::engine::ReplayVerdict::Matches(rebuilt.end),
            Err(crate::replay::ReplayError::NonDeterminism(mismatch)) => {
                crate::engine::ReplayVerdict::Diverges(mismatch)
            }
            Err(crate::replay::ReplayError::Executor(error)) => {
                crate::engine::ReplayVerdict::Fails(error)
            }
            Err(error) => return Err(unavailable(error)),
        })
    }

    /// The partitions that hold a scope: a namespace's, or every one for a tenant.
    pub async fn partitions_of(&self, scope: &Scope) -> Result<Vec<PartitionId>, EdgeError> {
        match scope {
            Scope::Namespace { tenant, namespace } => {
                let partition = match self
                    .plane
                    .catalog()
                    .get_namespace(tenant, namespace)
                    .await
                    .map_err(unavailable)?
                {
                    Some(found) => found.partition,
                    None => namespace_partition(tenant, namespace, self.config.partitions),
                };
                Ok(vec![partition])
            }
            Scope::Tenant(_) => Ok((0..self.config.partitions.get())
                .map(PartitionId::new)
                .collect()),
        }
    }

    /// A page of the summaries of the scope that match the query, merged from the partitions
    /// that hold it, in `order`. A partition that stopped at its scan budget before the others
    /// cuts the page there: the page may be short, with a cursor.
    pub async fn search<G: Gateway>(
        &self,
        gateway: &mut G,
        scope: &Scope,
        query: &RunQuery,
        order: RunOrder,
        page: &Page,
    ) -> Result<Found, EdgeError> {
        let mut found = Vec::new();
        let mut stop: Option<SearchPosition> = None;
        for partition in self.partitions_of(scope).await? {
            let request = PartitionRequest::Search {
                scope: scope.clone(),
                query: query.clone(),
                order,
                page: *page,
            };
            let page_of = match ask(gateway, partition, request).await? {
                PartitionReply::Found(page_of) => page_of?,
                other => return Err(unexpected(&other)),
            };
            if let Some(cursor) = &page_of.cursor {
                let position = SearchPosition::of(order, cursor);
                stop = Some(match stop {
                    Some(known) => known.min(position),
                    None => position,
                });
            }
            found.extend(page_of.summaries);
        }
        found.sort_by_key(|summary| SearchPosition::of_summary(order, summary));
        if let Some(stop) = &stop {
            found.retain(|summary| SearchPosition::of_summary(order, summary) <= *stop);
        }
        let mut cursor = None;
        if found.len() > page.limit {
            found.truncate(page.limit);
            cursor = found.last().map(|summary| summary.cursor(order));
        } else if stop.is_some() {
            cursor = found.last().map(|summary| summary.cursor(order));
        }
        Ok(Found {
            summaries: found,
            cursor,
        })
    }

    /// The counters of a scope over a window and its gauges, added up from the partitions that
    /// hold it.
    pub async fn statistics<G: Gateway>(
        &self,
        gateway: &mut G,
        scope: &Scope,
        window: Window,
    ) -> Result<(Counters, Gauges), EdgeError> {
        let mut counters = Counters::default();
        let mut gauges = Gauges::default();
        for partition in self.partitions_of(scope).await? {
            let request = PartitionRequest::Counters {
                scope: scope.clone(),
                window,
            };
            match ask(gateway, partition, request).await? {
                PartitionReply::Counted(counted) => {
                    let (partition_counters, partition_gauges) = counted?;
                    counters.add(&partition_counters);
                    gauges.add(&partition_gauges);
                }
                other => return Err(unexpected(&other)),
            }
        }
        Ok((counters, gauges))
    }

    /// The shapes observed for some tools, merged from the partitions of the scope.
    pub async fn shapes<G: Gateway>(
        &self,
        gateway: &mut G,
        scope: &Scope,
        tools: &[String],
    ) -> Result<BTreeMap<String, Shape>, EdgeError> {
        let mut merged: BTreeMap<String, Shape> = BTreeMap::new();
        for partition in self.partitions_of(scope).await? {
            let request = PartitionRequest::Shapes {
                tenant: scope.tenant().clone(),
                namespace: scope.namespace().cloned(),
                tools: tools.to_vec(),
            };
            match ask(gateway, partition, request).await? {
                PartitionReply::Shapes(shapes) => {
                    for (tool, shape) in shapes? {
                        let shape = match merged.remove(&tool) {
                            Some(known) => known.merge(shape),
                            None => shape,
                        };
                        merged.insert(tool, shape);
                    }
                }
                other => return Err(unexpected(&other)),
            }
        }
        Ok(merged)
    }
}

fn enqueued(reply: PartitionReply) -> Result<Enqueued, EdgeError> {
    match reply {
        PartitionReply::Enqueued(enqueued) => Ok(enqueued?),
        other => Err(unexpected(&other)),
    }
}

/// The reply of a gateway's request, from what the node answered.
pub(crate) fn partition_reply(reply: NodeReply) -> Result<PartitionReply, EdgeError> {
    match reply {
        NodeReply::Partition(reply) => Ok(*reply),
        NodeReply::NotOwner { partition } => Err(EdgeError::Unavailable(format!(
            "partition {partition} is moving: try again"
        ))),
        other => Err(EdgeError::Unavailable(format!("unexpected reply {other:?}"))),
    }
}
