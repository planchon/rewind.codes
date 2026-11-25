//! The use cases at the edge of the run plane (`archi/07-code-architecture.md`, "Inside a
//! node": the rules the engine keeps; `archi/03-execution.md`, "Starting a run", "Forks";
//! `archi/05-testing-and-observability.md`, "Read model", "Statistics").
//!
//! Every process is an edge. A use case checks what it can without the run's partition (the
//! code, the TTL, the input, the secrets, the namespace, read from the control plane), then
//! asks the owner of the run's partition through a [`Gateway`]: the router of a process
//! ([`NodeHandle`](crate::node::runtime::NodeHandle)) or the [`Driver`](crate::driver::Driver)
//! in tests. The owner keeps every rule that needs the partition: start keys, admission, signal
//! keys, the lifecycle of tasks, the reads.
//!
//! | Use case | The edge checks | The owner |
//! | --- | --- | --- |
//! | [`Edge::start_run`], [`Edge::start_workflow`], [`Edge::fork`] | the namespace, the code, the TTL, the input, the secrets | the start key, the admission, `CreateRun` |
//! | [`Edge::signal`], [`Edge::cancel`], [`Edge::signal_by_url`] | NUL characters | the run, its workflow's signals, the signal key |
//! | [`Edge::release`], [`Edge::give_up`], [`Edge::erase`] | | the quarantine, the archive, the records |
//! | [`Edge::complete_task`], [`Edge::fail_task`], [`Edge::defer_task`], [`Edge::approve_task`], [`Edge::reject_task`] | the token's run | the task's state |
//! | [`Edge::get_run`], [`Edge::find_run`], [`Edge::history`], [`Edge::export_run`], [`Edge::call_tasks`] | | the partition, or the archive |
//! | [`Edge::search`], [`Edge::statistics`], [`Edge::series`], [`Edge::usage`], [`Edge::shapes`] | the partitions of the scope, merged | each partition's index and counters |
//! | [`Edge::quota_alerts`] | | the owner of the tenant's home partition, from its tenant actor |
//! | [`Edge::transcript`], [`Edge::transcript_event`] | | the run's head and history |
//! | [`Edge::record_tests`] | the tests stored in the control plane | the runs searched, their history |
//! | [`Edge::create_workflow`], [`Edge::make_version`], [`Edge::change_tests`], [`Edge::update_workflow`], [`Edge::promote_workflow`], [`Edge::fork_workflow`], [`Edge::pull_workflow`], [`Edge::run_tests`], [`Edge::suites`] | the catalog, the gate's tests on a driver of their own | |

mod test_runs;
mod transcript;
mod workflows;

pub use test_runs::{RecordFrom, Recording, RecordingError};
pub use workflows::{MIN_VERSION_DIGITS, ResolveError, WorkflowKit};

use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use rewind_core::{
    Code, HistoryEvent, JsonValue, PartitionId, Payload, RunExport, RunHandle, RunId, RunMode,
    RunStarted, Shape, UNTITLED_ORIGIN, WorkflowVersion, code_doc, origin_slug,
};

use crate::checks::{self, BoundsError};
use crate::fork::{ForkError, ForkRun};
use crate::plane::{NodeConfig, Plane};
use crate::ports::link::{
    Decision, HarnessFailure, NodeReply, PartitionReply, PartitionRequest, Refusal, RunView,
    SignalAtOwner, StartAtOwner, TaskAnswer,
};
use crate::ports::{
    BlobOwner, CallTask, CatalogStore, Clock, CodeExecutor, Counters, Created, Enqueued, Entropy,
    Found, Gauges, Labels, Month, NamespaceName, NamespaceWrite, NewWorkflow, Page, QuotaAlert,
    RunOrder, RunQuery, RunRef, RunWorkflow, Scope, SearchPosition, StoreError, TaskRecord,
    TaskRef, TaskToken, TenantId, TypeChecker, Usage, Window, WorkflowCreated, WorkflowScope,
    home_partition, namespace_partition,
};
use crate::secrets::{self, SecretCheck};
use crate::start::{StartError, StartRun, refuse_nul_in_metadata, type_check};
use crate::text::{self, NulCharacter};
use crate::workflows::StartWorkflow;

/// What a new run is the run of, the middle of its handle (`archi/06-storage-and-scale.md`,
/// "Run handles"): its workflow's name, else its short id. A test run has no workflow: the first
/// line of its code's top comment as a slug, else `untitled` (a run stored before workflows had
/// ids kept `script`).
fn origin_of(metadata: &crate::ports::RunMetadata, code: &Code) -> String {
    match &metadata.workflow {
        Some(workflow) => workflow.origin(),
        None => code_doc(code.source())
            .and_then(|doc| origin_slug(&doc.title))
            .unwrap_or_else(|| UNTITLED_ORIGIN.to_owned()),
    }
}
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

    /// The partition of a run, from its id: the 16 bits its id holds, the control plane's
    /// `legacy_runs` for a run `migrate-runs` moved, the only one of an embedded deployment.
    pub async fn partition_of(&self, run: RunId) -> Result<PartitionId, EdgeError> {
        crate::router::partition_of(self.plane.catalog(), self.config.partitions, run)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| Refusal::RunNotFound(run).into())
    }

    /// The partition a token names, from its run.
    pub async fn partition_of_token(&self, token: &str) -> Result<(RunId, PartitionId), EdgeError> {
        let run = crate::partition::tokens::run_of(token).ok_or(Refusal::TaskGone)?;
        Ok((run, self.partition_of(run).await?))
    }

    // ---------------------------------------------------------------------------------------
    // Starts

    /// Checks the code and starts a run (`POST /v1/runs` with a code).
    pub async fn start_run<G: Gateway>(
        &self,
        gateway: &mut G,
        request: StartRun,
    ) -> Result<Created, StartError> {
        self.start(
            gateway,
            request,
            None::<&crate::adapters::StaticTypeChecker>,
        )
        .await
    }

    /// [`start_run`](Self::start_run), type-checked first against the action set's declarations.
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
        // What the request alone refuses is refused before its namespace's first use.
        let input = self.bounded_input(request.input)?;
        text::refuse_nul("the code", request.code.source())?;
        text::refuse_nul(text::ACTION_SET_TEXT, &request.declarations.typescript)?;
        text::refuse_nul(text::ACTION_SET_TEXT, &request.declarations.prompt)?;
        refuse_nul_in_metadata(&request.metadata, request.idempotency_key.as_deref())?;
        let namespace = self.namespace(&request.tenant, &request.namespace).await?;
        let bounds = self.bounds(&request.tenant, &request.namespace).await?;
        let ttl =
            checks::ttl(request.ttl, &bounds).map_err(|requested| StartError::InvalidTtl {
                requested,
                max: bounds.max_ttl,
            })?;
        bounds.check_lines(&request.code)?;
        let executor = self.plane.executor();
        let epoch = *executor.epochs().last().ok_or(StartError::NoEpoch)?;
        let compiled = executor
            .compile(&request.code, &request.action_set)
            .map_err(StartError::InvalidCode)?;
        if request.mode == RunMode::Live {
            self.check_secrets(
                &request.tenant,
                &request.namespace,
                &request.code,
                &request.action_set,
                &request.metadata.secrets,
                &request.secret_rights,
            )
            .await?;
        }
        if let Some(checker) = checker {
            type_check(checker, &request.code, &request.declarations).await?;
        }
        let catalog = self.plane.catalog();
        catalog
            .put_code(&request.tenant, &request.code, &compiled)
            .await
            .map_err(start_unavailable)?;
        catalog
            .put_action_set(&request.tenant, &request.action_set, &request.declarations)
            .await
            .map_err(start_unavailable)?;
        // The run is a run of the workflow its code is in the namespace or the organization,
        // else of a new workflow of the namespace (`archi/03-execution.md`, "Starting a run").
        let mut metadata = request.metadata;
        if request.mode == RunMode::Live {
            let workflow = self
                .workflow_of_code(
                    &request.tenant,
                    &request.namespace,
                    &request.code,
                    &request.action_set,
                    &metadata.secrets,
                    &metadata.allowed_labels,
                    &request.principal,
                )
                .await?;
            metadata.workflow = Some(workflow);
        }
        // The run is created now: its deadline is its TTL from then.
        let now = self.now();
        let started = |seed| RunStarted {
            code: request.code.digest(),
            action_set: request.action_set.digest(),
            input,
            seed,
            epoch,
            expires_at: now.saturating_add(ttl),
            mode: request.mode,
        };
        let run = RunRef::new(
            request.tenant.clone(),
            self.plane.entropy().run_id_in(namespace),
        );
        let seed = self.plane.entropy().seed();
        let origin = origin_of(&metadata, &request.code);
        self.create(
            gateway,
            namespace,
            StartAtOwner {
                run,
                namespace: request.namespace,
                origin,
                started: started(seed),
                metadata,
                start_key: request.idempotency_key,
                headline: projector::headline(request.code.source()),
                created_at: now,
            },
        )
        .await
    }

    /// Starts a run of a workflow (`POST /v1/runs` with a workflow): its id or its name, which
    /// resolves in the run's namespace first, `@v12` or `@<digest>` for a version, else its
    /// latest. The input is checked against the version's interface, if it has one; its code,
    /// its action set and its secrets are the version's.
    pub async fn start_workflow<G: Gateway>(
        &self,
        gateway: &mut G,
        request: StartWorkflow,
    ) -> Result<Created, StartError> {
        let catalog = self.plane.catalog();
        let (workflow, version) = self
            .resolve_workflow(
                &request.tenant,
                Some(&request.namespace),
                &request.metadata.allowed_labels,
                &request.reference,
            )
            .await
            .map_err(|error| match error {
                ResolveError::Invalid(message) => StartError::InvalidWorkflowReference(message),
                ResolveError::NotFound(message) => StartError::WorkflowNotFound(message),
                ResolveError::Store(error) => start_unavailable(error),
            })?;
        let document = &version.document;
        let named = format!("{}@v{}", workflow.reference(), version.version);
        if let Some(schemas) = document.interface()
            && let Err(refused) =
                crate::schema::Validator::compile(&schemas.input).check("input", &request.input)
        {
            return Err(StartError::InvalidInput(crate::workflows::refused_message(
                &format!("the input schema of {named}"),
                refused,
            )));
        }
        let input = self.bounded_input(request.input)?;
        let partition = self.namespace(&request.tenant, &request.namespace).await?;
        let bounds = self.bounds(&request.tenant, &request.namespace).await?;
        let ttl =
            checks::ttl(request.ttl, &bounds).map_err(|requested| StartError::InvalidTtl {
                requested,
                max: bounds.max_ttl,
            })?;
        let code = catalog
            .get_code(&request.tenant, document.code())
            .await
            .map_err(start_unavailable)?
            .ok_or_else(|| start_unavailable(format!("the code of {named} is not stored")))?;
        bounds.check_lines(&code.code)?;
        match checks::version_secrets(
            self.plane.secrets(),
            self.now(),
            &request.tenant,
            &request.namespace,
            document,
        )
        .await?
        {
            Ok(()) => {}
            Err(refused) => return Err(StartError::SecretRequired(refused)),
        }
        let mut metadata = request.metadata;
        metadata.workflow = Some(RunWorkflow::of(&workflow, &version));
        metadata.secrets = document.secrets().clone();
        refuse_nul_in_metadata(&metadata, request.idempotency_key.as_deref())?;
        let epoch = *self
            .plane
            .executor()
            .epochs()
            .last()
            .ok_or(StartError::NoEpoch)?;
        let run = RunRef::new(
            request.tenant.clone(),
            self.plane.entropy().run_id_in(partition),
        );
        let seed = self.plane.entropy().seed();
        let now = self.now();
        self.create(
            gateway,
            partition,
            StartAtOwner {
                run,
                namespace: request.namespace,
                // A workflow's run is the run of its workflow.
                origin: workflow.origin(),
                started: RunStarted {
                    code: document.code(),
                    action_set: document.action_set(),
                    input,
                    seed,
                    epoch,
                    expires_at: now.saturating_add(ttl),
                    mode: RunMode::Live,
                },
                metadata,
                start_key: request.idempotency_key,
                headline: projector::headline(code.code.source()),
                created_at: now,
            },
        )
        .await
    }

    /// The workflow a run given `code` is a run of (`archi/03-execution.md`, "Starting a run"):
    /// the one that has it in the namespace, else in the organization, under the caller's
    /// labels, at its latest version with that code; else a new workflow of the namespace, whose
    /// version 1 is that code, without an interface, with the run's action set and secrets.
    #[allow(clippy::too_many_arguments)]
    async fn workflow_of_code(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        code: &Code,
        action_set: &rewind_core::ActionSet,
        secrets: &rewind_core::ServerSecrets,
        labels: &Labels,
        principal: &str,
    ) -> Result<RunWorkflow, StartError> {
        let catalog = self.plane.catalog();
        if let Some((workflow, version)) = catalog
            .workflow_with_code(tenant, namespace, code.digest(), labels)
            .await
            .map_err(start_unavailable)?
        {
            return Ok(RunWorkflow::of(&workflow, &version));
        }
        let version = WorkflowVersion::new(code.digest(), action_set.digest())
            .with_labels(labels.clone())
            .with_secrets(secrets.clone());
        let created = catalog
            .create_workflow(
                tenant,
                &NewWorkflow {
                    id: self.plane.entropy().workflow_id(),
                    scope: WorkflowScope::Namespace(namespace.clone()),
                    name: None,
                    description: None,
                    labels: labels.clone(),
                    source: None,
                    created_by: principal.to_owned(),
                    version,
                },
                self.now(),
            )
            .await
            .map_err(start_unavailable)?;
        let workflow = match created {
            WorkflowCreated::Created(workflow) | WorkflowCreated::Existing(workflow) => workflow,
            WorkflowCreated::NameTaken => {
                return Err(start_unavailable("a workflow without a name took a name"));
            }
        };
        let version = catalog
            .versions(tenant, &workflow.id, labels)
            .await
            .map_err(start_unavailable)?
            .into_iter()
            .find(|version| version.document.code() == code.digest())
            .ok_or_else(|| {
                start_unavailable(format!(
                    "the workflow {} has no version of its code",
                    workflow.id
                ))
            })?;
        Ok(RunWorkflow::of(&workflow, &version))
    }

    /// Forks a run with a corrected code (`POST /v1/runs/{id}/fork`). The fork lives in its
    /// source's namespace, and so in its partition.
    pub async fn fork<G: Gateway>(
        &self,
        gateway: &mut G,
        request: ForkRun,
    ) -> Result<Created, ForkError> {
        self.fork_with(
            gateway,
            request,
            None::<&crate::adapters::StaticTypeChecker>,
        )
        .await
    }

    /// [`fork`](Self::fork), type-checked first against the source's action set.
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
        let source = request.source.clone();
        let start_failed = |error: StartError| ForkError::Start(error);
        let edge_failed = |error: EdgeError| ForkError::Start(start_unavailable(error));
        let partition = self
            .partition_of(source.id)
            .await
            .map_err(|_| ForkError::SourceNotFound(source.id))?;
        let view = self
            .get_run(gateway, &source)
            .await
            .map_err(edge_failed)?
            .ok_or(ForkError::SourceNotFound(source.id))?;
        let history = self
            .history(gateway, &source, 0, true)
            .await
            .map_err(edge_failed)?
            .unwrap_or_default();
        let Some(HistoryEvent {
            event: rewind_core::Event::Lifecycle(rewind_core::Lifecycle::RunStarted(source_started)),
            ..
        }) = history.first()
        else {
            return Err(ForkError::SourceNotStarted(source.id));
        };
        let head = view.head;
        let bounds = self
            .bounds(&source.tenant, &head.namespace)
            .await
            .map_err(start_failed)?;
        let ttl = checks::ttl(request.ttl, &bounds).map_err(|requested| {
            start_failed(StartError::InvalidTtl {
                requested,
                max: bounds.max_ttl,
            })
        })?;
        bounds.check_lines(&request.code).map_err(start_failed)?;
        let refused = |nul| ForkError::Start(StartError::NulCharacter(nul));
        text::refuse_nul("the code", request.code.source()).map_err(refused)?;
        let mut metadata = crate::ports::RunMetadata {
            labels: request.labels.unwrap_or(head.metadata.labels.clone()),
            trace_context: request.trace_context,
            source: Some(crate::ports::ForkSource {
                run: source.id,
                at: request.at,
            }),
            workflow: None,
            started_by: None,
            allowed_labels: head.metadata.allowed_labels.clone(),
            secrets: request
                .secrets
                .clone()
                .unwrap_or_else(|| head.metadata.secrets.clone()),
        };
        refuse_nul_in_metadata(&metadata, request.idempotency_key.as_deref()).map_err(refused)?;
        let stored = self
            .plane
            .catalog()
            .get_action_set(&source.tenant, head.action_set)
            .await
            .map_err(|error| start_failed(start_unavailable(error)))?
            .ok_or_else(|| {
                start_failed(start_unavailable(format!(
                    "action set {} not found",
                    head.action_set
                )))
            })?;
        let action_set = stored.action_set;
        let compiled = self
            .plane
            .executor()
            .compile(&request.code, &action_set)
            .map_err(|invalid| start_failed(StartError::InvalidCode(invalid)))?;
        if source_started.mode == RunMode::Live {
            self.check_secrets(
                &source.tenant,
                &head.namespace,
                &request.code,
                &action_set,
                &metadata.secrets,
                &request.secret_rights,
            )
            .await
            .map_err(start_failed)?;
        }
        if let Some(checker) = checker {
            type_check(checker, &request.code, &stored.declarations)
                .await
                .map_err(start_failed)?;
        }
        let input = match request.input {
            Some(input) => input,
            None => {
                let carried = crate::executors::activation::Carried {
                    records: Default::default(),
                    blobs: self.plane.blobs(),
                    owners: vec![BlobOwner::Run(source.clone())],
                };
                payloads::load(&carried, &source.tenant, &source_started.input)
                    .await
                    .map_err(|error| ForkError::Engine(error.into()))?
            }
        };
        let input = self.bounded_input(input).map_err(start_failed)?;
        self.plane
            .catalog()
            .put_code(&source.tenant, &request.code, &compiled)
            .await
            .map_err(|error| start_failed(start_unavailable(error)))?;
        // A fork given no new code is a run of its source's version; one with new code is a
        // run of the workflow its code is in the namespace, else of a new one
        // (`archi/03-execution.md`, "Forks").
        if source_started.mode == RunMode::Live {
            metadata.workflow = match &head.metadata.workflow {
                Some(of) if request.code.digest() == source_started.code => Some(of.clone()),
                _ => Some(
                    self.workflow_of_code(
                        &source.tenant,
                        &head.namespace,
                        &request.code,
                        &action_set,
                        &metadata.secrets,
                        &metadata.allowed_labels,
                        &request.principal,
                    )
                    .await
                    .map_err(start_failed)?,
                ),
            };
        }
        let run = RunRef::new(
            source.tenant.clone(),
            self.plane.entropy().run_id_in(partition),
        );
        let now = self.now();
        let start = StartAtOwner {
            run,
            namespace: head.namespace.clone(),
            // A fork is the run of what its source ran.
            origin: head.origin.clone(),
            started: RunStarted {
                code: request.code.digest(),
                action_set: head.action_set,
                input,
                seed: source_started.seed,
                epoch: source_started.epoch,
                expires_at: now.saturating_add(ttl),
                mode: source_started.mode,
            },
            metadata,
            start_key: request.idempotency_key,
            headline: projector::headline(request.code.source()),
            created_at: now,
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
    async fn namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<PartitionId, StartError> {
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
    ) -> Result<crate::start::NamespaceBounds, StartError> {
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
    async fn check_secrets(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        code: &Code,
        action_set: &rewind_core::ActionSet,
        secrets: &rewind_core::ServerSecrets,
        rights: &secrets::SecretRights,
    ) -> Result<(), StartError> {
        if secrets.is_empty() && action_set.taking_secrets().next().is_none() {
            return Ok(());
        }
        let imports = self.plane.executor().action_imports(code);
        let check = SecretCheck {
            tenant,
            namespace: Some(namespace),
            action_set,
            imports: imports.as_ref(),
            secrets,
            rights,
        };
        secrets::check(self.plane.secrets(), self.now(), check)
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
    pub async fn signal_by_url<G: Gateway>(
        &self,
        gateway: &mut G,
        token: &str,
        payload: JsonValue,
        key: Option<String>,
    ) -> Result<Enqueued, EdgeError> {
        let request = |payload| PartitionRequest::SignalUrl {
            token: token.to_owned(),
            payload,
            key: key.clone(),
        };
        if let Some(run) = crate::partition::tokens::run_of(token) {
            let partition = self.partition_of(run).await?;
            return enqueued(ask(gateway, partition, request(payload)).await?);
        }
        // The token of a run `migrate-runs` moved names no run: the partition that holds its
        // hash is asked, each in turn, as only such a token is.
        for partition in 0..self.config.partitions.get() {
            let partition = PartitionId::new(partition);
            match enqueued(ask(gateway, partition, request(payload.clone())).await?) {
                Err(EdgeError::Refused(Refusal::TaskGone)) => {}
                answered => return answered,
            }
        }
        Err(Refusal::TaskGone.into())
    }

    /// Lifts a quarantine; returns whether the run was quarantined.
    pub async fn release<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
    ) -> Result<bool, EdgeError> {
        self.done(gateway, run, PartitionRequest::Release { run: run.clone() })
            .await
    }

    /// Ends a quarantined run; returns whether it was quarantined.
    pub async fn give_up<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
    ) -> Result<bool, EdgeError> {
        self.done(gateway, run, PartitionRequest::GiveUp { run: run.clone() })
            .await
    }

    /// Erases a run now (`DELETE /v1/runs/{id}`); returns whether it existed.
    pub async fn erase<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
    ) -> Result<bool, EdgeError> {
        self.done(gateway, run, PartitionRequest::Erase { run: run.clone() })
            .await
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
        self.answer(gateway, tenant, token, TaskAnswer::Complete(result))
            .await
    }

    /// A handler's failure (`POST /v1/tasks/{token}/fail`).
    pub async fn fail_task<G: Gateway>(
        &self,
        gateway: &mut G,
        tenant: &TenantId,
        token: &TaskToken,
        failure: HarnessFailure,
    ) -> Result<(), EdgeError> {
        self.answer(gateway, tenant, token, TaskAnswer::Fail(failure))
            .await
    }

    /// The handler answers later (`POST /v1/tasks/{token}/defer`).
    pub async fn defer_task<G: Gateway>(
        &self,
        gateway: &mut G,
        tenant: &TenantId,
        token: &TaskToken,
        timeout: Option<Duration>,
    ) -> Result<(), EdgeError> {
        self.answer(gateway, tenant, token, TaskAnswer::Defer { timeout })
            .await
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
    pub async fn approve_task<G: Gateway>(
        &self,
        gateway: &mut G,
        task: &TaskRef,
    ) -> Result<(), EdgeError> {
        self.decide(gateway, task, Decision::Approve).await
    }

    /// Refuses a task that waits for the harness: the call fails with an `ActionError`.
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

    /// The run's call tasks, by seq.
    pub async fn call_tasks<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
    ) -> Result<Vec<CallTask>, EdgeError> {
        Ok(self
            .task_records(gateway, run)
            .await?
            .into_iter()
            .map(|record| record.task)
            .collect())
    }

    /// The run's call tasks as its partition keeps them.
    pub async fn task_records<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
    ) -> Result<Vec<TaskRecord>, EdgeError> {
        let partition = self.partition_of(run.id).await?;
        match ask(
            gateway,
            partition,
            PartitionRequest::Tasks { run: run.clone() },
        )
        .await?
        {
            PartitionReply::Tasks(tasks) => tasks?.ok_or(Refusal::RunNotFound(run.id).into()),
            other => Err(unexpected(&other)),
        }
    }

    // ---------------------------------------------------------------------------------------
    // Reads

    /// The run, live or sealed.
    pub async fn get_run<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
    ) -> Result<Option<RunView>, EdgeError> {
        let partition = self.partition_of(run.id).await?;
        match ask(
            gateway,
            partition,
            PartitionRequest::GetRun { run: run.clone() },
        )
        .await?
        {
            PartitionReply::Run(view) => Ok(view?),
            other => Err(unexpected(&other)),
        }
    }

    /// The run of the tenant a handle names (`archi/06-storage-and-scale.md`, "Run handles"):
    /// asked to the owner of its namespace's partition. `None` when the namespace or the run
    /// does not exist.
    pub async fn find_run<G: Gateway>(
        &self,
        gateway: &mut G,
        tenant: &TenantId,
        handle: &RunHandle,
    ) -> Result<Option<RunView>, EdgeError> {
        let Ok(namespace) = NamespaceName::new(handle.namespace.clone()) else {
            return Ok(None);
        };
        let Some(namespace) = self
            .plane
            .catalog()
            .get_namespace(tenant, &namespace)
            .await
            .map_err(unavailable)?
        else {
            return Ok(None);
        };
        let request = PartitionRequest::Find {
            tenant: tenant.clone(),
            handle: handle.clone(),
        };
        match ask(gateway, namespace.partition, request).await? {
            PartitionReply::Run(view) => Ok(view?),
            other => Err(unexpected(&other)),
        }
    }

    /// The events of the run's attempt from `from`: from its partition, or its sealed attempt.
    pub async fn history<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
        from: u64,
        values: bool,
    ) -> Result<Option<Vec<HistoryEvent>>, EdgeError> {
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

    /// The run's history with every payload loaded: a self-contained document.
    pub async fn export_run<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
    ) -> Result<RunExport, EdgeError> {
        let view = self
            .get_run(gateway, run)
            .await?
            .ok_or(Refusal::RunNotFound(run.id))?;
        let mut events = self
            .history(gateway, run, 0, true)
            .await?
            .ok_or(Refusal::RunNotFound(run.id))?;
        let source = crate::executors::activation::Carried {
            records: Default::default(),
            blobs: self.plane.blobs(),
            owners: vec![BlobOwner::Run(run.clone())],
        };
        payloads::load_all(&source, &run.tenant, &mut events)
            .await
            .map_err(unavailable)?;
        let catalog = self.plane.catalog();
        let code = catalog
            .get_code(&run.tenant, view.head.code)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| unavailable(format!("code {} not found", view.head.code)))?;
        let action_set = catalog
            .get_action_set(&run.tenant, view.head.action_set)
            .await
            .map_err(unavailable)?
            .ok_or_else(|| unavailable(format!("action set {} not found", view.head.action_set)))?;
        Ok(RunExport {
            run: run.id,
            code: code.code,
            action_set: action_set.action_set,
            events,
        })
    }

    /// Replays a run's history with its code, or another one, on this node's executor:
    /// whether it reproduces the history. Nothing is written and no action is called.
    pub async fn replay<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
        code: Option<&Code>,
    ) -> Result<crate::replay::ReplayVerdict, EdgeError> {
        let export = self.export_run(gateway, run).await?;
        let code = code.cloned().unwrap_or(export.code.clone());
        let executor = self.plane.executor();
        let compiled = executor.compile(&code, &export.action_set);
        let input = crate::replay::ReplayInput {
            run,
            compiled: compiled.as_ref(),
            action_set: &export.action_set,
            history: &export.events,
        };
        let blobs = self.plane.blobs();
        let source = crate::executors::activation::Carried {
            records: Default::default(),
            blobs,
            owners: vec![BlobOwner::Run(run.clone())],
        };
        Ok(
            match crate::replay::replay(executor, &source, input).await {
                Ok(rebuilt) => crate::replay::ReplayVerdict::Matches(rebuilt.end),
                Err(crate::replay::ReplayError::NonDeterminism(mismatch)) => {
                    crate::replay::ReplayVerdict::Diverges(mismatch)
                }
                Err(crate::replay::ReplayError::Executor(error)) => {
                    crate::replay::ReplayVerdict::Fails(error)
                }
                Err(error) => return Err(unavailable(error)),
            },
        )
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
        let (mut intervals, gauges) = self.counted(gateway, scope, window, None).await?;
        let counters = intervals
            .pop()
            .map(|(_, counters)| counters)
            .unwrap_or_default();
        Ok((counters, gauges))
    }

    /// The counters of a scope over a window in intervals of `interval` whole hours from the
    /// window's start, the last one cut at the window's end, each added up from the partitions
    /// that hold the scope: one request per partition, whatever the number of intervals.
    pub async fn series<G: Gateway>(
        &self,
        gateway: &mut G,
        scope: &Scope,
        window: Window,
        interval: NonZeroU32,
    ) -> Result<Vec<(Window, Counters)>, EdgeError> {
        Ok(self
            .counted(gateway, scope, window, Some(interval))
            .await?
            .0)
    }

    async fn counted<G: Gateway>(
        &self,
        gateway: &mut G,
        scope: &Scope,
        window: Window,
        interval: Option<NonZeroU32>,
    ) -> Result<(Vec<(Window, Counters)>, Gauges), EdgeError> {
        let mut intervals: Vec<(Window, Counters)> = crate::partition::intervals(window, interval)
            .into_iter()
            .map(|of| (of, Counters::default()))
            .collect();
        let mut gauges = Gauges::default();
        for partition in self.partitions_of(scope).await? {
            let request = PartitionRequest::Counters {
                scope: scope.clone(),
                window,
                interval,
            };
            match ask(gateway, partition, request).await? {
                PartitionReply::Counted(counted) => {
                    let (partition_counters, partition_gauges) = counted?;
                    for ((_, sum), counters) in intervals.iter_mut().zip(&partition_counters) {
                        sum.add(counters);
                    }
                    gauges.add(&partition_gauges);
                }
                other => return Err(unexpected(&other)),
            }
        }
        Ok((intervals, gauges))
    }

    /// The use in a month of each namespace of the scope, from the partitions that hold it: CPU
    /// time and runs created over the month, with its days when `days`; active runs and distinct
    /// workflows now (`GET /v1/namespaces`, `GET /v1/namespaces/{name}`). A namespace that used
    /// nothing is left out; the tenant's use is the sum of its namespaces'.
    pub async fn usage<G: Gateway>(
        &self,
        gateway: &mut G,
        scope: &Scope,
        month: Month,
        days: bool,
    ) -> Result<BTreeMap<NamespaceName, Usage>, EdgeError> {
        let mut used = BTreeMap::new();
        for partition in self.partitions_of(scope).await? {
            let request = PartitionRequest::Usage {
                scope: scope.clone(),
                month,
                days,
            };
            match ask(gateway, partition, request).await? {
                PartitionReply::Used(partition_used) => used.extend(partition_used?),
                other => return Err(unexpected(&other)),
            }
        }
        Ok(used)
    }

    /// The quota alerts of a month, of one namespace or every alert of the tenant (its own
    /// included), the newest first, from the tenant actor on the owner of the tenant's
    /// home partition (`GET /v1/organization/quota-alerts`).
    pub async fn quota_alerts<G: Gateway>(
        &self,
        gateway: &mut G,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        month: Month,
    ) -> Result<Vec<QuotaAlert>, EdgeError> {
        let home = home_partition(tenant, self.config.partitions);
        let request = PartitionRequest::QuotaAlerts {
            tenant: tenant.clone(),
            namespace: namespace.cloned(),
            month,
        };
        match ask(gateway, home, request).await? {
            PartitionReply::Alerts(alerts) => Ok(alerts?),
            other => Err(unexpected(&other)),
        }
    }

    /// The shapes observed for some actions, merged from the partitions of the scope.
    pub async fn shapes<G: Gateway>(
        &self,
        gateway: &mut G,
        scope: &Scope,
        actions: &[String],
    ) -> Result<BTreeMap<String, Shape>, EdgeError> {
        let mut merged: BTreeMap<String, Shape> = BTreeMap::new();
        for partition in self.partitions_of(scope).await? {
            let request = PartitionRequest::Shapes {
                tenant: scope.tenant().clone(),
                namespace: scope.namespace().cloned(),
                actions: actions.to_vec(),
            };
            match ask(gateway, partition, request).await? {
                PartitionReply::Shapes(shapes) => {
                    for (action, shape) in shapes? {
                        let shape = match merged.remove(&action) {
                            Some(known) => known.merge(shape),
                            None => shape,
                        };
                        merged.insert(action, shape);
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
        other => Err(EdgeError::Unavailable(format!(
            "unexpected reply {other:?}"
        ))),
    }
}
