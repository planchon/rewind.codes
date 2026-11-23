//! Test support for the run plane: memory adapters, the fake executor and the recording
//! dispatcher, wired as nodes stepped by the driver in virtual time.

use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use rewind_core::{JsonValue, PartitionId};
use rewind_engine::StartRun;
use rewind_engine::adapters::{
    ControlPlaneChanges, MemoryArchive, MemoryBlobs, MemoryCatalog, MemoryCluster,
    MemoryPartitions, MemorySecrets, MemorySpanExporter, MemoryWebhooks, SeededEntropy,
    VirtualClock,
};
use rewind_engine::driver::{Driver, PlaneClock};
use rewind_engine::edge::{Edge, EdgeError};
use rewind_engine::plane::{HostTools, NodeConfig, Wiring};
use rewind_engine::ports::link::{HostFailure, HostTask, RunView, StreamEvent};
use rewind_engine::ports::{
    Clock, Created, Enqueued, NodeId, PartitionStore, RunRef, TaskRef, TaskToken, TenantId,
    ToolTask,
};

use super::{FakeExecutor, RecordingDispatcher, T0};

/// The plane of the tests, over the memory partitions unless a test wires its own store.
pub type TestPlane<S = MemoryPartitions> = Wiring<
    MemoryCatalog,
    S,
    MemoryArchive,
    MemoryBlobs,
    FakeExecutor,
    RecordingDispatcher,
    MemorySecrets,
    MemorySpanExporter,
    MemoryWebhooks,
    VirtualClock,
    SeededEntropy,
>;

pub type TestDriver<S = MemoryPartitions> = Driver<TestPlane<S>, MemoryCluster>;

/// Nodes of one cluster in one process, sharing the plane's stores.
pub struct World<S: PartitionStore<Log: 'static> + 'static = MemoryPartitions> {
    pub plane: Arc<TestPlane<S>>,
    pub driver: TestDriver<S>,
    pub edge: Edge<TestPlane<S>>,
    pub cluster: MemoryCluster,
    /// The node the edge's requests enter.
    pub at: NodeId,
    pub partitions: NonZeroU16,
}

/// What an SDK stream receives.
pub type Stream = tokio::sync::mpsc::UnboundedReceiver<StreamEvent>;

/// What a stream received since it was last read: the tasks handed, and the tokens of those
/// gone.
pub fn received(stream: &mut Stream) -> (Vec<HostTask>, Vec<TaskToken>) {
    let mut tasks = Vec::new();
    let mut gone = Vec::new();
    for event in TestDriver::<MemoryPartitions>::drain(stream) {
        match event {
            StreamEvent::Task(task) => tasks.push(task),
            StreamEvent::Gone(token) => gone.push(token),
        }
    }
    (tasks, gone)
}

/// The tasks a stream was handed since it was last read; none of its tasks is gone.
pub fn handed(stream: &mut Stream) -> Vec<HostTask> {
    let (tasks, gone) = received(stream);
    assert!(gone.is_empty(), "no task is gone: {gone:?}");
    tasks
}

pub fn node(id: &str) -> NodeId {
    NodeId::new(id).unwrap()
}

/// The configuration of a node of the tests: host tools dispatched as in mock mode, a horizon
/// past every timer.
pub fn config(id: &str, partitions: u16) -> NodeConfig {
    let mut config = NodeConfig::new(node(id), NonZeroU16::new(partitions).unwrap());
    config.host_tools = HostTools::Dispatched;
    config.horizon = Duration::from_secs(100 * 365 * 86_400);
    config
}

impl World {
    /// One node, one partition.
    pub async fn new(programs: FakeExecutor, dispatcher: RecordingDispatcher) -> Self {
        Self::with(programs, dispatcher, 1, &[config("node-a", 1)]).await
    }

    /// Nodes with these configurations, over `partitions` partitions.
    pub async fn with(
        programs: FakeExecutor,
        dispatcher: RecordingDispatcher,
        partitions: u16,
        nodes: &[NodeConfig],
    ) -> Self {
        Self::over(
            MemoryPartitions::new(),
            programs,
            dispatcher,
            partitions,
            nodes,
        )
        .await
    }
}

impl<S: PartitionStore<Log: 'static> + 'static> World<S> {
    /// Nodes with these configurations, over `partitions` partitions kept in `store`.
    pub async fn over(
        store: S,
        programs: FakeExecutor,
        dispatcher: RecordingDispatcher,
        partitions: u16,
        nodes: &[NodeConfig],
    ) -> Self {
        Self::drawing(
            store,
            programs,
            dispatcher,
            partitions,
            nodes,
            SeededEntropy::new(42),
        )
        .await
    }

    /// [`World::over`], its run ids drawn by `entropy`.
    pub async fn drawing(
        store: S,
        programs: FakeExecutor,
        dispatcher: RecordingDispatcher,
        partitions: u16,
        nodes: &[NodeConfig],
        entropy: SeededEntropy,
    ) -> Self {
        let changes = ControlPlaneChanges::new();
        let cluster = MemoryCluster::with_changes(partitions, changes.clone());
        let blobs = MemoryBlobs::new();
        let plane = Arc::new(Wiring {
            catalog: MemoryCatalog::with_changes(changes.clone()),
            partitions: store,
            archive: MemoryArchive::with_blobs(blobs.clone()),
            blobs,
            executor: programs,
            dispatcher,
            secrets: MemorySecrets::with_changes(changes),
            spans: MemorySpanExporter::new(),
            webhooks: MemoryWebhooks::new(),
            clock: VirtualClock::new(T0),
            entropy,
        });
        let mut driver = Driver::new(cluster.clone(), Arc::new(PlaneClock(Arc::clone(&plane))));
        for config in nodes {
            driver
                .add_node(Arc::clone(&plane), config.clone())
                .await
                .unwrap();
            driver.run_until_idle().await.unwrap();
        }
        let first = nodes.first().expect("a world has a node");
        let edge = Edge::new(Arc::clone(&plane), first.clone());
        Self {
            plane,
            driver,
            edge,
            cluster,
            at: first.node.clone(),
            partitions: NonZeroU16::new(partitions).unwrap(),
        }
    }

    pub fn now(&self) -> rewind_core::Timestamp {
        self.plane.clock.now()
    }

    /// Moves the clock by `ms`.
    pub fn advance(&self, ms: u64) {
        use rewind_engine::ports::VirtualTime;
        self.plane
            .clock
            .advance_to(self.now().saturating_add(Duration::from_millis(ms)));
    }

    /// Steps until nothing is left to do now.
    pub async fn settle(&mut self) {
        self.driver.run_until_idle().await.unwrap();
    }

    /// Steps until only the outside world can wake a run, in virtual time.
    pub async fn idle(&mut self) {
        self.driver.run_until_idle_virtual().await.unwrap();
    }

    pub async fn start(&mut self, request: StartRun) -> Result<Created, rewind_engine::StartError> {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge.start_run(&mut gateway, request).await
    }

    /// Starts a run and drives it until it waits for the outside world.
    pub async fn run(&mut self, request: StartRun) -> RunRef {
        let created = self.start(request).await.unwrap();
        self.idle().await;
        created.run
    }

    pub async fn view(&mut self, run: &RunRef) -> RunView {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge.get_run(&mut gateway, run).await.unwrap().unwrap()
    }

    pub async fn history(&mut self, run: &RunRef) -> Vec<rewind_core::HistoryEntry> {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge
            .history(&mut gateway, run, 0, true)
            .await
            .unwrap()
            .unwrap()
    }

    pub async fn trace(&mut self, run: &RunRef) -> String {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge.trace(&mut gateway, run).await.unwrap()
    }

    pub async fn events(&mut self, run: &RunRef) -> Vec<&'static str> {
        self.history(run)
            .await
            .iter()
            .map(|entry| entry.event.name())
            .collect()
    }

    pub async fn signal(
        &mut self,
        run: &RunRef,
        name: &str,
        payload: JsonValue,
        key: Option<&str>,
    ) -> Result<Enqueued, EdgeError> {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge
            .signal(&mut gateway, run, name, payload, key.map(str::to_owned))
            .await
    }

    pub async fn cancel(&mut self, run: &RunRef) -> Result<Enqueued, EdgeError> {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge.cancel(&mut gateway, run, None).await
    }

    pub async fn tasks(&mut self, run: &RunRef) -> Vec<ToolTask> {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge.tool_tasks(&mut gateway, run).await.unwrap()
    }

    pub async fn approve(&mut self, task: &TaskRef) -> Result<(), EdgeError> {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge.approve_task(&mut gateway, task).await
    }

    pub async fn reject(&mut self, task: &TaskRef, message: &str) -> Result<(), EdgeError> {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge
            .reject_task(&mut gateway, task, Some(message.into()))
            .await
    }

    pub async fn complete(
        &mut self,
        token: &TaskToken,
        result: JsonValue,
    ) -> Result<(), EdgeError> {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge
            .complete_task(&mut gateway, &TenantId::local(), token, result)
            .await
    }

    pub async fn fail(&mut self, token: &TaskToken, failure: HostFailure) -> Result<(), EdgeError> {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge
            .fail_task(&mut gateway, &TenantId::local(), token, failure)
            .await
    }

    pub async fn release(&mut self, run: &RunRef) -> bool {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge.release(&mut gateway, run).await.unwrap()
    }

    pub async fn give_up(&mut self, run: &RunRef) -> bool {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge.give_up(&mut gateway, run).await.unwrap()
    }

    pub async fn erase(&mut self, run: &RunRef) -> bool {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge.erase(&mut gateway, run).await.unwrap()
    }

    /// The partition of the namespace `local`.
    pub fn local_partition(&self) -> PartitionId {
        rewind_engine::ports::namespace_partition(
            &TenantId::local(),
            &rewind_engine::ports::NamespaceName::new("local").unwrap(),
            self.partitions,
        )
    }
}
