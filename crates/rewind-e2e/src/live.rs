//! A case run live: by the engine's actors in the driver (`archi/07-code-architecture.md`,
//! "Inside a node", "Tests").
//!
//! One node holds the one partition of the deployment, over the memory adapters, with the V8
//! executor and the scenario's actions, in virtual time from [`T0`]. A harness's
//! worker is an SDK stream at the node's edge: it is handed the harness calls and answers them
//! through their token; a worker that crashes closes its stream, and another worker opens one.
//! The entropy draws the scenario's ids and seeds in order, whatever the partition, so a case's
//! run has the id its committed history holds.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use rewind_core::{PartitionId, RunId, RunMode, RunState, SecretRef, Seed, parse_duration};
use rewind_engine::adapters::{
    ControlPlaneChanges, MemoryArchive, MemoryBlobs, MemoryCatalog, MemoryCluster,
    MemoryPartitions, MemorySecrets, MemorySpanExporter, MemoryWebhooks, SeededEntropy,
    VirtualClock,
};
use rewind_engine::driver::{Driver, PlaneClock};
use rewind_engine::edge::Edge;
use rewind_engine::plane::{HarnessActions, NodeConfig, Wiring};
use rewind_engine::ports::link::{HarnessFailure, StreamAttach, StreamEvent, StreamId};
use rewind_engine::ports::{
    ActionDeclarations, CatalogStore, Clock, CodeExecutor, Entropy, NamespaceName, NewSecret,
    NodeId, ParkKind, Predicate, RunOrder, RunQuery, RunRef, Scope, Secret, SecretId, SecretStore,
    TaskRef, TaskState, TenantId, Term, VirtualTime,
};
use rewind_engine::{ForkRun, Limits, ReplayVerdict, StartRun};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::actions::{Answer, ScenarioActions};
use crate::scenario::Step;
use crate::{Case, Live, Recorded, T0, attachments, executor, scenario_action_set};

/// The ids and seeds of a scenario's entropy, drawn in order: a run of the one partition has
/// the version 7 id the committed history holds.
#[derive(Debug)]
pub struct CorpusEntropy(SeededEntropy);

impl Entropy for CorpusEntropy {
    fn run_id(&self) -> RunId {
        self.0.run_id()
    }

    fn run_id_in(&self, _partition: PartitionId) -> RunId {
        self.0.run_id()
    }

    fn seed(&self) -> Seed {
        self.0.seed()
    }
}

type CorpusPlane = Wiring<
    MemoryCatalog,
    MemoryPartitions,
    MemoryArchive,
    MemoryBlobs,
    rewind_runtime::V8Executor,
    ScenarioActions,
    MemorySecrets,
    MemorySpanExporter,
    MemoryWebhooks,
    VirtualClock,
    CorpusEntropy,
>;

/// A harness's worker: the SDK stream it holds.
struct Worker {
    stream: StreamId,
    events: UnboundedReceiver<StreamEvent>,
    generation: u32,
}

struct World {
    plane: Arc<CorpusPlane>,
    driver: Driver<CorpusPlane, MemoryCluster>,
    edge: Edge<CorpusPlane>,
    at: NodeId,
    worker: Option<Worker>,
}

fn fail(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn node_config(case: &Case) -> Result<NodeConfig, String> {
    let node = NodeId::new("corpus").map_err(fail)?;
    let mut config = NodeConfig::new(node, NonZeroU16::MIN);
    config.horizon = Duration::from_secs(100 * 365 * 86_400);
    let defaults = Limits::default();
    config.limits = Limits {
        max_commands_per_activation: case
            .scenario
            .limits
            .max_commands
            .unwrap_or(defaults.max_commands_per_activation),
        max_payload_bytes: case
            .scenario
            .limits
            .max_payload_bytes
            .unwrap_or(defaults.max_payload_bytes),
        ..defaults
    };
    if case.scenario.auth.values().any(|auth| auth == "rewind") {
        config.public_url = Some("https://rewind.example.com".into());
    }
    config.harness_actions = if case.scenario.harness_worker {
        HarnessActions::Streams
    } else {
        HarnessActions::Dispatched
    };
    config.reversed_calls = case.scenario.action_order == crate::scenario::ActionOrder::Reversed;
    Ok(config)
}

impl World {
    async fn new(case: &Case) -> Result<Self, String> {
        let changes = ControlPlaneChanges::new();
        let cluster = MemoryCluster::with_changes(1, changes.clone());
        let blobs = MemoryBlobs::new();
        let plane = Arc::new(Wiring {
            catalog: MemoryCatalog::with_changes(changes.clone()),
            partitions: MemoryPartitions::new(),
            archive: MemoryArchive::with_blobs(blobs.clone()),
            blobs,
            executor: executor(&case.scenario),
            dispatcher: ScenarioActions::new(&case.scenario.actions),
            secrets: MemorySecrets::with_changes(changes),
            spans: MemorySpanExporter::new(),
            webhooks: MemoryWebhooks::new(),
            clock: VirtualClock::new(T0),
            entropy: CorpusEntropy(SeededEntropy::new(case.scenario.seed)),
        });
        let config = node_config(case)?;
        let mut driver = Driver::new(cluster, Arc::new(PlaneClock(Arc::clone(&plane))));
        let at = driver
            .add_node(Arc::clone(&plane), config.clone())
            .await
            .map_err(fail)?;
        driver.run_until_idle().await.map_err(fail)?;
        let edge = Edge::new(Arc::clone(&plane), config);
        let mut world = Self {
            plane,
            driver,
            edge,
            at,
            worker: None,
        };
        if case.scenario.harness_worker {
            world.open_stream();
        }
        Ok(world)
    }

    /// A worker opens an SDK stream at the edge.
    fn open_stream(&mut self) {
        let generation = self
            .worker
            .as_ref()
            .map_or(0, |worker| worker.generation + 1);
        let stream = StreamId(format!("stream-{generation}"));
        let attach = StreamAttach {
            stream: stream.clone(),
            worker: format!("worker-{generation}"),
            node: self.at.clone(),
            namespace: None,
            labels: BTreeMap::new(),
            action_set: None,
            actions: BTreeSet::new(),
            slots: 100,
        };
        if let Some(events) = self
            .driver
            .attach_stream(&self.at, TenantId::local(), attach)
        {
            self.worker = Some(Worker {
                stream,
                events,
                generation,
            });
        }
    }

    /// Drives the node: with a worker, the node and the worker take turns until neither has
    /// work.
    async fn drive(&mut self, virtual_time: bool) -> Result<(), String> {
        let mut idle_turns = 0;
        loop {
            if virtual_time {
                self.driver.run_until_idle_virtual().await.map_err(fail)?;
            } else {
                self.driver.run_until_idle().await.map_err(fail)?;
            }
            if self.worker.is_none() {
                return Ok(());
            }
            if self.work().await? > 0 {
                idle_turns = 0;
            } else {
                idle_turns += 1;
                if idle_turns == 2 {
                    return Ok(());
                }
            }
        }
    }

    /// One turn of the harness's worker: answers each task it was handed, from the scenario's
    /// actions, through its token. Returns how many it was handed.
    async fn work(&mut self) -> Result<usize, String> {
        let Some(worker) = &mut self.worker else {
            return Ok(0);
        };
        // A task gone needs no answer.
        let tasks: Vec<_> = Driver::<CorpusPlane, MemoryCluster>::drain(&mut worker.events)
            .into_iter()
            .filter_map(|event| match event {
                StreamEvent::Task(task) => Some(task),
                StreamEvent::Gone(_) => None,
            })
            .collect();
        let tenant = TenantId::local();
        let mut crashed = false;
        for task in &tasks {
            let failed = |failure: rewind_core::ActionFailure, retryable| HarnessFailure {
                message: failure.message,
                data: failure.data,
                retryable,
            };
            let answer =
                self.plane
                    .dispatcher
                    .answer(&task.action, &task.args, &task.idempotency_key);
            let mut gateway = self.driver.gateway(&self.at);
            let answered = match answer {
                Answer::Result(result) => {
                    self.edge
                        .complete_task(&mut gateway, &tenant, &task.token, result)
                        .await
                }
                Answer::Failed(failure) => {
                    self.edge
                        .fail_task(&mut gateway, &tenant, &task.token, failed(failure, false))
                        .await
                }
                Answer::Transient(failure) => {
                    self.edge
                        .fail_task(&mut gateway, &tenant, &task.token, failed(failure, true))
                        .await
                }
                Answer::Unauthorized(_) => {
                    return Err(format!(
                        "{}: a harness action's worker cannot answer $unauthorized; only a dispatched \
                         call is parked",
                        task.action
                    ));
                }
                // The worker dies with the call: its stream closes, and another worker opens
                // one.
                Answer::Crash => {
                    crashed = true;
                    Ok(())
                }
                Answer::Deferred => Ok(()),
            };
            answered.map_err(fail)?;
        }
        if crashed && let Some(worker) = &self.worker {
            let stream = worker.stream.clone();
            self.driver.detach_stream(&self.at, &stream);
            self.open_stream();
        }
        Ok(tasks.len())
    }
}

/// Runs a case live on the run plane: starts the run, drives it, applies the steps, drives it
/// in virtual time.
pub async fn run_live(case: &Case) -> Result<Live, String> {
    let mut world = World::new(case).await?;
    put_secrets(case, &world).await?;
    register_workflows(case, &world).await?;
    let mut request = StartRun::new(
        case.code.clone(),
        case.action_set(),
        case.scenario.input.clone(),
    );
    request.metadata.secrets = attachments(&case.scenario.secrets)?;
    if case.scenario.attach.as_deref() == Some("organization") {
        request.attach = rewind_engine::attachments::AttachRights::Organization {
            labels: BTreeMap::new(),
        };
    }
    request.mode = if case.scenario.live {
        RunMode::Live
    } else {
        RunMode::Mock
    };
    if let Some(ttl) = &case.scenario.ttl {
        request.ttl = Some(parse_duration(ttl).map_err(|err| format!("ttl {ttl:?}: {err}"))?);
    }
    let started = {
        let World {
            edge, driver, at, ..
        } = &mut world;
        let mut gateway = driver.gateway(at);
        edge.start_run(&mut gateway, request).await
    };
    let mut run = match started {
        Ok(created) => created.run,
        Err(refused) => return Ok(Live::Refused(refused.to_string())),
    };
    world.drive(false).await?;
    apply_steps(&mut world, &run, &case.scenario.steps).await?;
    world.drive(true).await?;

    if let (Some(fork), Some(code)) = (&case.scenario.fork, &case.fork_code) {
        let mut request = ForkRun::new(run.clone(), code.clone());
        request.at = fork.at;
        request.input = fork.input.clone();
        let forked = {
            let World {
                edge, driver, at, ..
            } = &mut world;
            let mut gateway = driver.gateway(at);
            edge.fork(&mut gateway, request).await
        };
        run = match forked {
            Ok(created) => created.run,
            Err(refused) => return Ok(Live::Refused(refused.to_string())),
        };
        world.drive(false).await?;
        apply_steps(&mut world, &run, &fork.steps).await?;
        world.drive(true).await?;
    }

    // The engine's own replay, from its records, must agree with the live run.
    let verdict = {
        let World {
            edge, driver, at, ..
        } = &mut world;
        let mut gateway = driver.gateway(at);
        edge.replay(&mut gateway, &run, None).await.map_err(fail)?
    };
    match verdict {
        ReplayVerdict::Matches(_) => {}
        other => return Err(format!("the engine's replay of the live run: {other:?}")),
    }
    let export = {
        let World {
            edge, driver, at, ..
        } = &mut world;
        let mut gateway = driver.gateway(at);
        edge.export_run(&mut gateway, &run).await.map_err(fail)?
    };
    let state = RunState::from_history(&export.events).map_err(|err| err.to_string())?;
    let children = children(&mut world, &run).await?;
    let dispatcher = &world.plane.dispatcher;
    Ok(Live::Ran(Box::new(Recorded {
        run_id: run.id,
        export,
        state,
        dispatched: dispatcher.dispatched(),
        children,
        auth: dispatcher.auth(),
        keys: dispatcher.keys(),
    })))
}

/// The statuses of the runs the run started, in the order of its calls.
async fn children(world: &mut World, run: &RunRef) -> Result<Vec<String>, String> {
    let tenant = TenantId::local();
    let query = RunQuery {
        tenant: tenant.clone(),
        terms: vec![Term {
            negated: false,
            predicate: Predicate::StartedBy(run.id),
        }],
    };
    let found = {
        let World {
            edge, driver, at, ..
        } = world;
        let mut gateway = driver.gateway(at);
        edge.search(
            &mut gateway,
            &Scope::Tenant(tenant),
            &query,
            RunOrder::Oldest,
            &rewind_engine::ports::Page::first(1_000),
        )
        .await
        .map_err(fail)?
    };
    let mut children = Vec::new();
    for summary in found.summaries {
        let view = {
            let World {
                edge, driver, at, ..
            } = &mut *world;
            let mut gateway = driver.gateway(at);
            edge.get_run(&mut gateway, &summary.run)
                .await
                .map_err(fail)?
        };
        let Some(view) = view else { continue };
        let seq = view
            .head
            .metadata
            .started_by
            .as_ref()
            .map_or(0, |by| by.seq.0);
        children.push((seq, view.head.status.as_str().to_owned()));
    }
    children.sort();
    Ok(children.into_iter().map(|(_, status)| status).collect())
}

fn local() -> Result<NamespaceName, String> {
    NamespaceName::new(NamespaceName::LOCAL).map_err(fail)
}

async fn put_secrets(case: &Case, world: &World) -> Result<(), String> {
    let namespace = local()?;
    for (reference, value) in &case.scenario.put_secrets {
        let reference = SecretRef::parse(reference).map_err(fail)?;
        let secret = NewSecret {
            id: SecretId::resolve(&reference, &namespace),
            value: Secret::new(value.clone()),
            labels: BTreeMap::new(),
            by: "user:harness".into(),
        };
        world
            .plane
            .secrets
            .put(&TenantId::local(), &secret, world.plane.clock.now())
            .await
            .map_err(fail)?;
    }
    Ok(())
}

/// Stores the scenario's workflows in the catalog, with the scenario's actions as their action set.
/// Their suites are empty: nothing gates them.
async fn register_workflows(case: &Case, world: &World) -> Result<(), String> {
    if case.scenario.workflows.is_empty() {
        return Ok(());
    }
    let tenant = TenantId::local();
    let action_set = scenario_action_set(&case.scenario)?;
    let catalog = &world.plane.catalog;
    catalog
        .put_action_set(&tenant, &action_set, &ActionDeclarations::default())
        .await
        .map_err(fail)?;
    for workflow in case.workflows()? {
        let name = workflow.name().to_owned();
        let code = &case.workflow_scripts[&name];
        let compiled = world
            .plane
            .executor
            .compile(code, &action_set)
            .map_err(|err| format!("workflow {name}: {err}"))?;
        catalog
            .put_code(&tenant, code, &compiled)
            .await
            .map_err(fail)?;
        catalog
            .put_workflow(&tenant, &workflow, world.plane.clock.now())
            .await
            .map_err(|err| format!("workflow {name}: {err}"))?;
    }
    Ok(())
}

/// Applies steps to a run, each followed by a drive at the current time.
async fn apply_steps(world: &mut World, run: &RunRef, steps: &[Step]) -> Result<(), String> {
    for step in steps {
        match step {
            Step::Signal { name, payload, key } => {
                let World {
                    edge, driver, at, ..
                } = &mut *world;
                let mut gateway = driver.gateway(at);
                edge.signal(
                    &mut gateway,
                    run,
                    name.clone(),
                    payload.clone(),
                    key.clone(),
                )
                .await
                .map_err(fail)?;
            }
            Step::Cancel { message } => {
                let World {
                    edge, driver, at, ..
                } = &mut *world;
                let mut gateway = driver.gateway(at);
                edge.cancel(&mut gateway, run, message.clone())
                    .await
                    .map_err(fail)?;
            }
            Step::Advance(by) => {
                let by = parse_duration(by).map_err(|err| format!("advance {by:?}: {err}"))?;
                let now = world.plane.clock.now();
                world.plane.clock.advance_to(now.saturating_add(by));
            }
            Step::Approve(action) => {
                let task = waiting_task(world, run, action).await?;
                let World {
                    edge, driver, at, ..
                } = &mut *world;
                let mut gateway = driver.gateway(at);
                edge.approve_task(&mut gateway, &task).await.map_err(fail)?;
            }
            Step::Reject { action, message } => {
                let task = waiting_task(world, run, action).await?;
                let World {
                    edge, driver, at, ..
                } = &mut *world;
                let mut gateway = driver.gateway(at);
                edge.reject_task(&mut gateway, &task, message.clone())
                    .await
                    .map_err(fail)?;
            }
            Step::Credentials(servers) => {
                for (server, headers) in servers {
                    let value = headers.values().next().cloned().unwrap_or_default();
                    accept_again(world, run, server, value).await?;
                }
            }
            Step::DeleteSecret(server) => {
                let view = view_of(world, run).await?;
                let attached = view.head.metadata.secrets.get(server).ok_or_else(|| {
                    format!("delete_secret: the run attaches no secret for {server}")
                })?;
                let id = SecretId::resolve(attached, &view.head.namespace);
                if !world
                    .plane
                    .secrets
                    .delete(&run.tenant, &id)
                    .await
                    .map_err(fail)?
                {
                    return Err(format!("delete_secret: {id} does not exist"));
                }
            }
            Step::Parked { action, reason } => {
                let tasks = tasks_of(world, run).await?;
                let parked = tasks.iter().any(|task| {
                    task.action == *action
                        && matches!(&task.state, TaskState::Parked(why) if why.kind.as_str() == reason)
                });
                if !parked {
                    let states: Vec<String> = tasks
                        .iter()
                        .map(|task| format!("{} {:?}", task.action, task.state))
                        .collect();
                    return Err(format!(
                        "parked: no call of {action} is parked by {reason}: {states:?}"
                    ));
                }
            }
        }
        world.drive(false).await?;
    }
    Ok(())
}

async fn view_of(
    world: &mut World,
    run: &RunRef,
) -> Result<rewind_engine::ports::link::RunView, String> {
    let World {
        edge, driver, at, ..
    } = world;
    let mut gateway = driver.gateway(at);
    edge.get_run(&mut gateway, run)
        .await
        .map_err(fail)?
        .ok_or_else(|| format!("run {} not found", run.id))
}

async fn tasks_of(
    world: &mut World,
    run: &RunRef,
) -> Result<Vec<rewind_engine::ports::CallTask>, String> {
    let World {
        edge, driver, at, ..
    } = world;
    let mut gateway = driver.gateway(at);
    edge.call_tasks(&mut gateway, run).await.map_err(fail)
}

/// The harness makes `server` accept the run's calls again: it puts the secret the run
/// attached for it again, which resumes the calls parked by a refusal or a missing secret; for
/// a server whose run attaches no secret, it approves the refused calls.
async fn accept_again(
    world: &mut World,
    run: &RunRef,
    server: &str,
    value: String,
) -> Result<(), String> {
    let view = view_of(world, run).await?;
    if let Some(attached) = view.head.metadata.secrets.get(server) {
        let secret = NewSecret {
            id: SecretId::resolve(attached, &view.head.namespace),
            value: Secret::new(value),
            labels: BTreeMap::new(),
            by: "user:harness".into(),
        };
        world
            .plane
            .secrets
            .put(&run.tenant, &secret, world.plane.clock.now())
            .await
            .map_err(fail)?;
        return Ok(());
    }
    let tasks = tasks_of(world, run).await?;
    let refused: Vec<TaskRef> = tasks
        .into_iter()
        .filter(|task| {
            task.action.split_once('.').map(|(namespace, _)| namespace) == Some(server)
                && matches!(&task.state, TaskState::Parked(reason) if reason.kind == ParkKind::CredentialsRefused)
        })
        .map(|task| task.task)
        .collect();
    for task in refused {
        let World {
            edge, driver, at, ..
        } = &mut *world;
        let mut gateway = driver.gateway(at);
        edge.approve_task(&mut gateway, &task).await.map_err(fail)?;
    }
    Ok(())
}

/// The call of `action` that waits for the harness.
async fn waiting_task(world: &mut World, run: &RunRef, action: &str) -> Result<TaskRef, String> {
    tasks_of(world, run)
        .await?
        .into_iter()
        .find(|task| {
            task.action == action
                && matches!(task.state, TaskState::Approval | TaskState::Parked(_))
        })
        .map(|task| task.task)
        .ok_or_else(|| format!("no call of {action} waits for the harness"))
}
