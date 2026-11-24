//! The world of an episode: the engine's run plane with the V8 sandbox and the MCP dispatcher,
//! the fake MCP servers it calls, and the outside world reacting in virtual time.
//!
//! The run plane is wired in process (`archi/08-decisions.md`, "Taken while building the
//! evals"): one node holding the one partition, over the engine's memory adapters, stepped by its
//! driver; `McpDispatcher` for MCP actions, the type checker `rewind-check`, codegen
//! `rewind-codegen`, and one virtual clock shared with the fake servers. Time only moves when the
//! world is driven: a run's `wait`, a server's webhook, a signal sent by the world after a delay,
//! a timer.
//!
//! The agent's action set is registered as `POST /v1/action-sets` registers it, so that workflows
//! are saved with it; workflows are saved and run as the server saves and runs them, at the node's
//! edge, with the agent's labels as the labels of the key that saves them.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rewind_check::TsgoChecker;
use rewind_core::{
    Action, ActionRoute, ActionSet, Builtin, Code, Command, Completion, Digest, Event,
    HistoryEntry, McpServer, McpTransport, RetiredTest, RunId, RunOutcome, RunState, RunStatus,
    Seq, Timestamp, UpdatedTest,
};
use rewind_engine::adapters::{
    ControlPlaneChanges, MemoryArchive, MemoryBlobs, MemoryCatalog, MemoryCluster,
    MemoryPartitions, MemorySecrets, MemorySpanExporter, MemoryWebhooks, RoutingDispatcher,
    SeededEntropy, Unrouted,
};
use rewind_engine::driver::{DriveError, Driver, PlaneClock};
use rewind_engine::edge::{Edge, EdgeError, RecordFrom, Recording, RecordingError};
use rewind_engine::plane::{NodeConfig, Wiring};
use rewind_engine::ports::link::RunView;
use rewind_engine::ports::{
    ActionDeclarations, CallTask, CatalogError, CatalogStore, Clock, Created, Enqueued,
    NamespaceName, NodeId, Page, RunMetadata, RunOrder, RunQuery, RunRef, RunSummary, RunWorkflow,
    Scope, StoredWorkflow, TenantId, VirtualTime, WaitKind, WorkflowFilter,
};
use rewind_engine::suites::{LoadedTest, SuiteLoadError, TestFormat, TestKit};
use rewind_engine::testing::TestOptions;
use rewind_engine::{ForkError, ForkRun, StartError, StartRun, StartWorkflow};
use rewind_mcp::{ListingAuth, McpConfig, McpDispatcher};
use rewind_outbound::ActionEgress;
use rewind_runtime::{SandboxLimits, TsMockLoader, V8Executor};
use serde_json::Value;

use crate::clock::{DAY, SharedClock, T0};
use crate::fake::{self, Running, Service};

/// The run plane of an episode.
pub type EvalPlane = Wiring<
    MemoryCatalog,
    MemoryPartitions,
    MemoryArchive,
    MemoryBlobs,
    V8Executor,
    RoutingDispatcher<McpDispatcher, Unrouted, Unrouted, Unrouted>,
    MemorySecrets,
    MemorySpanExporter,
    MemoryWebhooks,
    SharedClock,
    SeededEntropy,
>;

type EvalDriver = Driver<EvalPlane, MemoryCluster>;

/// The base of the URLs `webhook(name)` returns.
pub const PUBLIC_URL: &str = "https://rewind.example";

/// How far the world goes on after the conversation, at most: the runs left waiting for
/// timers end within it.
pub const SETTLE_HORIZON: u64 = 60 * DAY;

/// Jumps of virtual time in one drive, at most: a guard against workflows that never stop.
const MAX_STEPS: usize = 20_000;

/// Steps of the driver in one pass at the current time, at most: a guard against workflows that
/// never wait.
const MAX_DRIVER_STEPS: usize = 200_000;

/// Summaries a page of a search of the world holds.
const SEARCH_PAGE: usize = 1_000;

/// The world answers a signal wait: when a run waits for `signal`, the world sends `payload`
/// `after` that much virtual time, once per run.
#[derive(Debug, Clone, PartialEq)]
pub struct Reaction {
    pub signal: &'static str,
    pub payload: Value,
    pub after: Duration,
}

/// A run of a workflow that production starts after the conversation: the latest version of
/// `workflow` the agent sees, with `input`. It shows whether what the agent saved works.
#[derive(Debug, Clone, PartialEq)]
pub struct ProductionRun {
    pub workflow: &'static str,
    pub input: Value,
}

/// What an episode's world is made of.
#[derive(Debug, Clone, Default)]
pub struct WorldSpec {
    /// Fake servers by name ([`fake::SERVERS`]).
    pub servers: Vec<&'static str>,
    /// The action set has the built-in `webhook`.
    pub webhook: bool,
    pub reactions: Vec<Reaction>,
    /// Started after the conversation, in order, each driven until it ends.
    pub production: Vec<ProductionRun>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorldError {
    #[error("unknown fake server {0}")]
    UnknownServer(String),
    #[error("cannot serve a fake server: {0}")]
    Serve(#[from] std::io::Error),
    #[error("cannot list the actions of {server}: {message}")]
    Discovery { server: String, message: String },
    #[error("invalid action set: {0}")]
    ActionSet(String),
    #[error("the sandbox cannot start: {0}")]
    Sandbox(String),
    #[error(transparent)]
    Edge(#[from] EdgeError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// The driver stopped: a request got no answer, or the node is gone.
    #[error("the run plane stopped: {0}")]
    Drive(String),
    /// What the engine answered the world cannot take as a refusal.
    #[error("{0}")]
    Engine(String),
    #[error("the activations of run {run} failed {failures} times in a row")]
    Activation { run: RunId, failures: u32 },
    #[error("the run of the workflow {workflow} cannot start: {message}")]
    Production { workflow: String, message: String },
    #[error("the stored tests cannot be read: {0}")]
    Tests(#[from] SuiteLoadError),
}

/// How a drive ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drove {
    /// The run the drive waited for has ended.
    Ended,
    /// The time limit was reached.
    TimeUp,
    /// Nothing can happen without the agent.
    Idle,
    /// The drive gave up: the runs kept going without reaching any of the above.
    Interrupted,
}

#[derive(Debug, Clone)]
struct Scheduled {
    run: RunRef,
    signal: String,
    payload: Value,
    at: Timestamp,
}

/// A signal or webhook the world delivered, for the episode's record.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Delivered {
    pub run: String,
    pub signal: String,
    pub at: String,
    /// `reaction` or `webhook`.
    pub source: &'static str,
}

#[derive(Debug, Default)]
struct Reactions {
    scheduled: Vec<Scheduled>,
    fired: BTreeSet<(RunId, String)>,
    delivered: Vec<Delivered>,
}

/// The world of an episode.
pub struct World {
    /// The adapters of the node.
    pub plane: Arc<EvalPlane>,
    /// The node's edge: what `rewind-server`'s handlers call.
    pub edge: Edge<EvalPlane>,
    /// How the node runs: its limits, its public URL.
    pub config: NodeConfig,
    pub clock: SharedClock,
    pub servers: Vec<Running>,
    /// The pinned action set the agent's workflows import from `rewind:actions`.
    pub action_set: ActionSet,
    /// What codegen generated for it.
    pub declarations: rewind_codegen::Declarations,
    pub checker: Arc<TsgoChecker>,
    /// Set on every run of the agent.
    pub labels: BTreeMap<String, String>,
    /// Steps the node; each request at the edge goes through it.
    driver: tokio::sync::Mutex<EvalDriver>,
    at: NodeId,
    mcp: McpDispatcher,
    reactions_spec: Vec<Reaction>,
    reactions: Mutex<Reactions>,
    production: Vec<ProductionRun>,
}

impl std::fmt::Debug for World {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("World")
            .field("servers", &self.servers)
            .field("action_set", &self.action_set.digest())
            .finish_non_exhaustive()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        self.mcp.close();
        for server in &self.servers {
            server.stop();
        }
    }
}

fn drive_error(error: DriveError) -> WorldError {
    WorldError::Drive(error.to_string())
}

impl World {
    /// Serves the fake servers, pins their actions as `POST /v1/action-sets` does (discovery
    /// through `McpDispatcher`), generates the declarations, and wires the run plane at [`T0`].
    pub async fn new(
        spec: &WorldSpec,
        checker: Arc<TsgoChecker>,
        labels: BTreeMap<String, String>,
    ) -> Result<Self, WorldError> {
        let clock = SharedClock::new(T0);
        let mut servers = Vec::new();
        for name in &spec.servers {
            let service: Arc<dyn Service> =
                fake::by_name(name).ok_or_else(|| WorldError::UnknownServer((*name).to_owned()))?;
            servers.push(fake::serve(service, clock.clone()).await?);
        }

        // The fake servers listen on 127.0.0.1, as a developer's would for the embedded server.
        let mcp = McpDispatcher::new(McpConfig {
            egress: ActionEgress::open(),
            ..McpConfig::default()
        });
        let tenant = TenantId::local();
        let mut pinned_servers = Vec::new();
        let mut actions: Vec<Action> = Vec::new();
        for running in &servers {
            let server = McpServer::new(
                running.service.name(),
                McpTransport::Http {
                    url: running.url.clone(),
                },
            );
            let listed = mcp
                .list_tools(&tenant, &server, ListingAuth::None)
                .await
                .map_err(|err| WorldError::Discovery {
                    server: server.name.clone(),
                    message: err.to_string(),
                })?;
            actions.extend(listed);
            pinned_servers.push(server);
        }
        if spec.webhook {
            actions.push(Action::new(
                "webhook",
                ActionRoute::Builtin(Builtin::Webhook),
                rewind_runtime::builtins::webhook_input_schema(),
            ));
        }
        let action_set = ActionSet::new(pinned_servers, actions)
            .map_err(|err| WorldError::ActionSet(err.to_string()))?;

        let executor = V8Executor::new(SandboxLimits::default())
            .map_err(|err| WorldError::Sandbox(err.to_string()))?;
        let changes = ControlPlaneChanges::new();
        let cluster = MemoryCluster::with_changes(1, changes.clone());
        let blobs = MemoryBlobs::new();
        let plane = Arc::new(Wiring {
            catalog: MemoryCatalog::with_changes(changes.clone()),
            partitions: MemoryPartitions::new(),
            archive: MemoryArchive::with_blobs(blobs.clone()),
            blobs,
            executor,
            dispatcher: RoutingDispatcher::new(mcp.clone(), Unrouted, Unrouted, Unrouted),
            secrets: MemorySecrets::with_changes(changes),
            spans: MemorySpanExporter::new(),
            webhooks: MemoryWebhooks::new(),
            clock: clock.clone(),
            entropy: SeededEntropy::new(1),
        });
        let node = NodeId::new("evals").map_err(|err| WorldError::Engine(err.to_string()))?;
        let mut config = NodeConfig::new(node, NonZeroU16::MIN);
        config.public_url = Some(PUBLIC_URL.into());
        // Every timer of an episode is held by the wheel: the world moves time itself.
        config.horizon = Duration::from_secs(100 * 365 * 86_400);
        let mut driver = Driver::new(cluster, Arc::new(PlaneClock(Arc::clone(&plane))));
        driver.max_steps = MAX_DRIVER_STEPS;
        let at = driver
            .add_node(Arc::clone(&plane), config.clone())
            .await
            .map_err(drive_error)?;
        driver.run_until_idle().await.map_err(drive_error)?;
        let edge = Edge::new(Arc::clone(&plane), config.clone());
        let declarations = register_action_set(&plane.catalog, &action_set).await?;
        Ok(Self {
            plane,
            edge,
            config,
            clock,
            servers,
            action_set,
            declarations,
            checker,
            labels,
            driver: tokio::sync::Mutex::new(driver),
            at,
            mcp,
            reactions_spec: spec.reactions.clone(),
            reactions: Mutex::new(Reactions::default()),
            production: spec.production.clone(),
        })
    }

    /// The catalog of the control plane.
    pub fn catalog(&self) -> &MemoryCatalog {
        &self.plane.catalog
    }

    /// Registers an action set as `POST /v1/action-sets` does: its declarations generated, then
    /// stored with it. An action set registered again changes nothing.
    pub async fn register_action_set(
        &self,
        action_set: &ActionSet,
    ) -> Result<rewind_codegen::Declarations, WorldError> {
        register_action_set(self.catalog(), action_set).await
    }

    /// The declarations stored with the action set, as the engine and the checker take them.
    pub fn action_declarations(&self) -> ActionDeclarations {
        ActionDeclarations {
            typescript: self.declarations.typescript.clone(),
            prompt: self.declarations.prompt.clone(),
        }
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// A run of the tenant `local`.
    pub fn run_ref(&self, id: RunId) -> RunRef {
        RunRef::new(TenantId::local(), id)
    }

    /// Starts a run of the agent: with its labels, live, in virtual time. The caller has
    /// type-checked the code.
    pub fn start_request(&self, code: Code, input: Value) -> StartRun {
        self.start_request_on(code, &self.action_set, self.action_declarations(), input)
    }

    /// [`start_request`](Self::start_request), with another action set: one derived from the
    /// world's.
    pub fn start_request_on(
        &self,
        code: Code,
        action_set: &ActionSet,
        declarations: ActionDeclarations,
        input: Value,
    ) -> StartRun {
        let mut request = StartRun::new(code, action_set.clone(), input);
        request.declarations = declarations;
        request.metadata.labels = self.labels.clone();
        request
    }

    /// What running a suite takes: the TypeScript mock handlers' loader, and the stored format
    /// of tests.
    pub fn test_kit<'a>(&self, loader: &'a TsMockLoader) -> TestKit<'a, TsMockLoader> {
        TestKit {
            loader,
            format: test_format(),
            options: TestOptions::default(),
        }
    }

    // ---------------------------------------------------------------------------------------
    // The edge, each request stepped by the driver to its answer

    /// Starts a run at the edge, type-checked first unless `unchecked`.
    pub async fn start(&self, request: StartRun, unchecked: bool) -> Result<Created, StartError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        if unchecked {
            self.edge.start_run(&mut gateway, request).await
        } else {
            self.edge
                .start_run_checked(&mut gateway, request, &*self.checker)
                .await
        }
    }

    /// Forks a run at the edge, type-checked first unless `unchecked`.
    pub async fn fork(&self, request: ForkRun, unchecked: bool) -> Result<Created, ForkError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        if unchecked {
            self.edge.fork(&mut gateway, request).await
        } else {
            self.edge
                .fork_checked(&mut gateway, request, &*self.checker)
                .await
        }
    }

    /// The run, live or sealed: its head and its summary.
    pub async fn run_view(&self, run: &RunRef) -> Result<Option<RunView>, EdgeError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge.get_run(&mut gateway, run).await
    }

    /// The run of the world's tenant a handle names (`archi/06-storage-and-scale.md`, "Run
    /// handles").
    pub async fn find_run(
        &self,
        handle: &rewind_core::RunHandle,
    ) -> Result<Option<RunView>, EdgeError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge
            .find_run(
                &mut gateway,
                &rewind_engine::ports::TenantId::local(),
                handle,
            )
            .await
    }

    /// The run's history, every payload loaded.
    pub async fn history(&self, run: &RunRef) -> Result<Vec<HistoryEntry>, EdgeError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        Ok(self.edge.export_journal(&mut gateway, run).await?.entries)
    }

    /// The run's call tasks.
    pub async fn call_tasks(&self, run: &RunRef) -> Result<Vec<CallTask>, EdgeError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge.call_tasks(&mut gateway, run).await
    }

    pub async fn signal(
        &self,
        run: &RunRef,
        name: &str,
        payload: Value,
        key: Option<String>,
    ) -> Result<Enqueued, EdgeError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge
            .signal(&mut gateway, run, name, payload, key)
            .await
    }

    pub async fn cancel(
        &self,
        run: &RunRef,
        message: Option<String>,
    ) -> Result<Enqueued, EdgeError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge.cancel(&mut gateway, run, message).await
    }

    /// The run's LLM trace.
    pub async fn trace(&self, run: &RunRef) -> Result<String, EdgeError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge.trace(&mut gateway, run).await
    }

    /// Records tests from these runs of `workflow`, in this order, and stores them.
    pub async fn record_runs(
        &self,
        workflow: &str,
        runs: Vec<RunRef>,
        limit: usize,
    ) -> Result<Recording, RecordingError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge
            .record_tests(
                &mut gateway,
                workflow,
                RecordFrom::Runs(runs),
                limit,
                test_format(),
            )
            .await
    }

    /// The summaries of every run of the tenant `local`, oldest first.
    pub async fn summaries(&self) -> Result<Vec<RunSummary>, EdgeError> {
        let tenant = TenantId::local();
        let query = RunQuery {
            tenant: tenant.clone(),
            terms: Vec::new(),
        };
        let scope = Scope::Tenant(tenant);
        let mut summaries = Vec::new();
        let mut page = Page::first(SEARCH_PAGE);
        loop {
            let found = {
                let mut driver = self.driver.lock().await;
                let mut gateway = driver.gateway(&self.at);
                self.edge
                    .search(&mut gateway, &scope, &query, RunOrder::Oldest, &page)
                    .await?
            };
            summaries.extend(found.summaries);
            match found.cursor {
                Some(cursor) => page = Page::after(cursor, SEARCH_PAGE),
                None => return Ok(summaries),
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Workflows

    /// The versions of a workflow the agent sees, newest first.
    pub async fn workflow_versions(&self, name: &str) -> Result<Vec<StoredWorkflow>, WorldError> {
        Ok(self
            .catalog()
            .workflow_versions(&TenantId::local(), name, &self.labels)
            .await?)
    }

    /// Starts a run of the latest version of a workflow the agent sees, with its labels, as
    /// `POST /v1/runs { workflow }` does.
    pub async fn start_workflow(&self, name: &str, input: Value) -> Result<RunRef, WorldError> {
        let refused = |message: String| WorldError::Production {
            workflow: name.to_owned(),
            message,
        };
        let latest = self
            .workflow_versions(name)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| refused("the agent sees no version of it".into()))?;
        let namespace =
            NamespaceName::new(NamespaceName::LOCAL).map_err(|error| refused(error.to_string()))?;
        let request = StartWorkflow {
            tenant: TenantId::local(),
            namespace,
            version: latest.workflow.version(),
            input,
            ttl: None,
            metadata: RunMetadata {
                labels: self.labels.clone(),
                ..RunMetadata::default()
            },
            idempotency_key: None,
        };
        let created = {
            let mut driver = self.driver.lock().await;
            let mut gateway = driver.gateway(&self.at);
            self.edge
                .start_workflow(&mut gateway, request)
                .await
                .map_err(|error: StartError| refused(error.to_string()))?
        };
        Ok(created.run)
    }

    /// After the conversation: the production runs, each started and driven until it ends.
    /// Their ids, in order.
    pub async fn run_production(&self) -> Result<Vec<RunId>, WorldError> {
        let mut ids = Vec::new();
        for production in &self.production {
            let run = self
                .start_workflow(production.workflow, production.input.clone())
                .await?;
            self.drive(Some(&run), None).await?;
            ids.push(run.id);
        }
        Ok(ids)
    }

    /// Every version of every workflow, whatever its labels, oldest first, with its suite's
    /// tests loaded: what a checker reads.
    pub async fn workflow_reports(&self) -> Result<Vec<WorkflowReport>, WorldError> {
        let tenant = TenantId::local();
        let catalog = self.catalog();
        let latest = catalog
            .latest_workflows(&WorkflowFilter {
                tenant: tenant.clone(),
                labels: BTreeMap::new(),
                after: None,
                limit: None,
            })
            .await?;
        let suites = self.edge.suites(test_format());
        let mut reports = Vec::new();
        for stored in latest {
            let mut versions = catalog
                .workflow_versions(&tenant, stored.workflow.name(), &BTreeMap::new())
                .await?;
            versions.reverse();
            for version in versions {
                let tests = suites.load_suite(&tenant, &version.workflow).await?;
                let workflow = &version.workflow;
                reports.push(WorkflowReport {
                    name: workflow.name().to_owned(),
                    version: workflow.version(),
                    parent: workflow.suite().parent,
                    action_set: workflow.action_set(),
                    code: catalog
                        .get_script(&tenant, workflow.code())
                        .await?
                        .map(|stored| stored.code.source().to_owned())
                        .unwrap_or_default(),
                    tests,
                    retired: workflow.suite().retired.clone(),
                    updated: workflow.suite().updated.clone(),
                });
            }
        }
        Ok(reports)
    }

    /// Every run of the world, oldest first.
    pub async fn runs(&self) -> Result<Vec<RunRef>, WorldError> {
        Ok(self
            .summaries()
            .await?
            .into_iter()
            .map(|summary| summary.run)
            .collect())
    }

    /// The calls a fake server received.
    pub fn calls(&self, server: &str) -> Vec<fake::CallRecord> {
        self.servers
            .iter()
            .find(|running| running.service.name() == server)
            .map(Running::calls)
            .unwrap_or_default()
    }

    /// The signals the world sent, in order.
    pub fn delivered(&self) -> Vec<Delivered> {
        fake::lock(&self.reactions).delivered.clone()
    }

    // ---------------------------------------------------------------------------------------
    // Time

    /// Drives the world in virtual time: the node runs whatever is ready, the world reacts
    /// (webhooks of the servers, signals answering waits), and time jumps to the next thing
    /// that happens, until `run` has ended, until `until`, or until nothing can happen without
    /// the agent.
    pub async fn drive(
        &self,
        run: Option<&RunRef>,
        until: Option<Timestamp>,
    ) -> Result<Drove, WorldError> {
        self.drive_until(run, until, true).await
    }

    /// [`drive`](Self::drive); when nothing can happen any more before `until`, the clock
    /// moves to `until` only if `elapse` (a wait elapses; a world left alone does not). An
    /// activation that failed is the world's fault, not the agent's.
    async fn drive_until(
        &self,
        run: Option<&RunRef>,
        until: Option<Timestamp>,
        elapse: bool,
    ) -> Result<Drove, WorldError> {
        let drove = self.drive_steps(run, until, elapse).await?;
        for summary in self.summaries().await? {
            if let Some(view) = self.run_view(&summary.run).await?
                && view.head.failures > 0
            {
                return Err(WorldError::Activation {
                    run: summary.run.id,
                    failures: view.head.failures,
                });
            }
        }
        Ok(drove)
    }

    async fn drive_steps(
        &self,
        run: Option<&RunRef>,
        until: Option<Timestamp>,
        elapse: bool,
    ) -> Result<Drove, WorldError> {
        for _ in 0..MAX_STEPS {
            match self.driver.lock().await.run_until_idle().await {
                Ok(_) => {}
                Err(DriveError::NeverIdle(_)) => return Ok(Drove::Interrupted),
                Err(error) => return Err(drive_error(error)),
            }
            if let Some(run) = run
                && self.is_terminal(run).await?
            {
                return Ok(Drove::Ended);
            }
            if self.react().await? {
                continue;
            }
            let now = self.now();
            match (self.next_event().await, until) {
                (Some(next), Some(until)) if next > until => {
                    self.clock.advance_to(until);
                    return Ok(Drove::TimeUp);
                }
                (Some(next), _) if next > now => self.clock.advance_to(next),
                (Some(_), _) => return Ok(Drove::Interrupted),
                (None, Some(until)) => {
                    if !elapse {
                        return Ok(Drove::Idle);
                    }
                    self.clock.advance_to(until);
                    return Ok(Drove::TimeUp);
                }
                (None, None) => return Ok(Drove::Idle),
            }
        }
        Ok(Drove::Interrupted)
    }

    /// After the conversation: the world goes on, up to [`SETTLE_HORIZON`] of virtual time, so
    /// that a run sleeping for days does what it was written to do.
    pub async fn settle(&self) -> Result<Drove, WorldError> {
        let horizon = Timestamp(self.now().0 + SETTLE_HORIZON);
        self.drive_until(None, Some(horizon), false).await
    }

    async fn is_terminal(&self, run: &RunRef) -> Result<bool, WorldError> {
        Ok(self
            .run_view(run)
            .await?
            .is_some_and(|view| view.head.status.is_terminal()))
    }

    /// The next time something happens by itself: a timer of the node that wakes a run (or a
    /// run's deadline before it), a webhook of a server, a signal the world will send.
    async fn next_event(&self) -> Option<Timestamp> {
        let node = self.driver.lock().await.next_jump();
        let scheduled = fake::lock(&self.reactions)
            .scheduled
            .iter()
            .map(|scheduled| scheduled.at)
            .min();
        let webhooks = self
            .servers
            .iter()
            .filter_map(|server| server.service.outbox().and_then(|outbox| outbox.next()))
            .min();
        node.into_iter().chain(scheduled).chain(webhooks).min()
    }

    /// Delivers what is due now: the servers' webhooks, and the signals answering waits.
    /// Returns whether anything was delivered.
    async fn react(&self) -> Result<bool, WorldError> {
        let now = self.now();
        let mut delivered = false;

        for server in &self.servers {
            let Some(outbox) = server.service.outbox() else {
                continue;
            };
            for outbound in outbox.take_due(now) {
                // What the server's POST to `/hooks/{token}` does on the embedded server.
                let token = outbound
                    .url
                    .strip_prefix(PUBLIC_URL)
                    .and_then(|path| path.strip_prefix("/hooks/"));
                let Some(token) = token else {
                    continue;
                };
                let Ok((run, _)) = self.edge.partition_of_token(token).await else {
                    continue;
                };
                let run = self.run_ref(run);
                let enqueued = {
                    let mut driver = self.driver.lock().await;
                    let mut gateway = driver.gateway(&self.at);
                    self.edge
                        .webhook(&mut gateway, token, outbound.payload, Some(outbound.id))
                        .await
                };
                match enqueued {
                    Ok(_) => {}
                    // A token no longer bound to a run: its run ended.
                    Err(EdgeError::Refused(_)) => continue,
                    Err(error) => return Err(error.into()),
                }
                let name =
                    webhook_name(&self.history(&run).await?, &outbound.url).unwrap_or_default();
                self.record(&run, &name, now, "webhook");
                delivered = true;
            }
        }

        if !self.reactions_spec.is_empty() {
            for summary in self.summaries().await? {
                if summary.status.is_terminal() {
                    continue;
                }
                let awaited = summary
                    .waits
                    .iter()
                    .filter(|wait| wait.kind == WaitKind::Signal)
                    .filter_map(|wait| wait.name.as_ref());
                for name in awaited {
                    let Some(reaction) = self
                        .reactions_spec
                        .iter()
                        .find(|reaction| reaction.signal == name)
                    else {
                        continue;
                    };
                    let mut reactions = fake::lock(&self.reactions);
                    if reactions.fired.insert((summary.run.id, name.clone())) {
                        let after = u64::try_from(reaction.after.as_millis()).unwrap_or(u64::MAX);
                        reactions.scheduled.push(Scheduled {
                            run: summary.run.clone(),
                            signal: name.clone(),
                            payload: reaction.payload.clone(),
                            at: Timestamp(now.0.saturating_add(after)),
                        });
                    }
                }
            }
        }

        let due: Vec<Scheduled> = {
            let mut reactions = fake::lock(&self.reactions);
            let (due, later) = reactions
                .scheduled
                .drain(..)
                .partition(|scheduled| scheduled.at <= now);
            reactions.scheduled = later;
            due
        };
        for scheduled in due {
            self.signal(&scheduled.run, &scheduled.signal, scheduled.payload, None)
                .await?;
            self.record(&scheduled.run, &scheduled.signal, now, "reaction");
            delivered = true;
        }
        Ok(delivered)
    }

    fn record(&self, run: &RunRef, signal: &str, at: Timestamp, source: &'static str) {
        fake::lock(&self.reactions).delivered.push(Delivered {
            run: run.id.to_string(),
            signal: signal.to_owned(),
            at: crate::clock::rfc3339(at),
            source,
        });
    }

    /// Every run as a checker sees it.
    pub async fn run_reports(&self) -> Result<Vec<RunReport>, WorldError> {
        let mut reports = Vec::new();
        for run in self.runs().await? {
            let head = self.run_view(&run).await?.map(|view| view.head);
            let history = self.history(&run).await?;
            let state = RunState::from_history(&history).ok();
            let (output, error) = match state.as_ref().and_then(RunState::outcome) {
                Some(RunOutcome::Completed { output }) => (output.value().cloned(), None),
                Some(RunOutcome::Failed { error }) => {
                    (None, Some(format!("{}: {}", error.name, error.message)))
                }
                Some(RunOutcome::Aborted { reason }) => (None, Some(reason.to_string())),
                None => (None, None),
            };
            reports.push(RunReport {
                id: run.id,
                status: head.as_ref().map_or(RunStatus::Pending, |head| head.status),
                parent: head
                    .as_ref()
                    .and_then(|head| head.metadata.parent.as_ref())
                    .map(|parent| parent.run),
                workflow: head
                    .as_ref()
                    .and_then(|head| head.metadata.workflow.clone()),
                labels: head.map(|head| head.metadata.labels).unwrap_or_default(),
                output,
                error,
                history,
            });
        }
        Ok(reports)
    }
}

/// The name a run gave the webhook whose URL is `url`: the argument of its call of `webhook`
/// that answered it.
fn webhook_name(history: &[HistoryEntry], url: &str) -> Option<String> {
    let answered: Option<Seq> = history.iter().find_map(|entry| match &entry.event {
        Event::Completion(Completion::CallCompleted { seq, result, .. })
            if result
                .value()
                .and_then(|result| result.get("url"))
                .and_then(Value::as_str)
                == Some(url) =>
        {
            Some(*seq)
        }
        _ => None,
    });
    let seq = answered?;
    history.iter().find_map(|entry| match &entry.event {
        Event::Command(Command::ScheduleCall {
            seq: scheduled,
            action,
            args,
            ..
        }) if *scheduled == seq && action == "webhook" => args
            .value()
            .and_then(|args| args.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    })
}

/// A run at the end of an episode.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub id: RunId,
    pub status: RunStatus,
    /// For a fork, the run it corrects.
    pub parent: Option<RunId>,
    /// For a run of a workflow, its name and version.
    pub workflow: Option<RunWorkflow>,
    pub labels: BTreeMap<String, String>,
    /// With `completed`.
    pub output: Option<Value>,
    /// With `failed`, `cancelled` or `aborted`: `Name: message`.
    pub error: Option<String>,
    /// Every payload loaded.
    pub history: Vec<HistoryEntry>,
}

/// A workflow version at the end of an episode.
#[derive(Debug, Clone)]
pub struct WorkflowReport {
    pub name: String,
    pub version: Digest,
    /// The version it derives from.
    pub parent: Option<Digest>,
    /// The action set its runs call.
    pub action_set: Digest,
    /// Its code's source.
    pub code: String,
    /// The tests of its suite, in its order, each loaded.
    pub tests: Vec<LoadedTest>,
    pub retired: Vec<RetiredTest>,
    pub updated: Vec<UpdatedTest>,
}

impl WorkflowReport {
    /// The test of its suite named `name`, loaded.
    pub fn test(&self, name: &str) -> Option<&LoadedTest> {
        self.tests.iter().find(|test| test.name == name)
    }
}

/// The stored format of tests, v1, as the composition root gives it to the engine.
pub fn test_format() -> TestFormat {
    TestFormat {
        encode: rewind_format::encode_test,
        decode: |document| rewind_format::decode_test(document).map_err(|error| error.to_string()),
    }
}

/// Stores an action set with the declarations codegen generates for it.
async fn register_action_set(
    catalog: &MemoryCatalog,
    action_set: &ActionSet,
) -> Result<rewind_codegen::Declarations, WorldError> {
    let declarations = rewind_codegen::generate(action_set, &BTreeMap::new());
    catalog
        .put_action_set(
            &TenantId::local(),
            action_set,
            &ActionDeclarations {
                typescript: declarations.typescript.clone(),
                prompt: declarations.prompt.clone(),
            },
        )
        .await?;
    Ok(declarations)
}
