//! The run plane the sandbox's tests drive (`archi/07-code-architecture.md`, "Tests"): one node
//! holding the one partition, over the engine's memory adapters, with the V8 executor and a
//! test's dispatcher, stepped by the engine's driver in virtual time from [`T0`].
//!
//! Harness actions go to the dispatcher, as in mock mode: the tests' dispatchers answer every
//! action.

#![allow(dead_code, unreachable_pub, clippy::unwrap_used)]

use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use rewind_core::{Code, HistoryEvent, JsonValue, Timestamp};
use rewind_engine::adapters::{
    ControlPlaneChanges, MemoryArchive, MemoryBlobs, MemoryCatalog, MemoryCluster,
    MemoryPartitions, MemorySecrets, MemorySpanExporter, MemoryWebhooks, SeededEntropy,
    VirtualClock,
};
use rewind_engine::driver::{Driver, PlaneClock};
use rewind_engine::edge::{Edge, EdgeError};
use rewind_engine::plane::{HarnessActions, NodeConfig, Wiring};
use rewind_engine::ports::link::RunView;
use rewind_engine::ports::{
    ActionDispatcher, Clock, Enqueued, NodeId, Page, Predicate, RunOrder, RunQuery, RunRef,
    RunSummary, Scope, TenantId, Term, VirtualTime,
};
use rewind_engine::{ReplayVerdict, StartError, StartRun};
use rewind_runtime::V8Executor;
use tokio::sync::Mutex;

/// Every run starts at 2026-01-01T00:00:00Z, in virtual time.
pub const T0: Timestamp = Timestamp(1_767_225_600_000);

pub type RuntimePlane<D> = Wiring<
    MemoryCatalog,
    MemoryPartitions,
    MemoryArchive,
    MemoryBlobs,
    V8Executor,
    D,
    MemorySecrets,
    MemorySpanExporter,
    MemoryWebhooks,
    VirtualClock,
    SeededEntropy,
>;

type RuntimeDriver<D> = Driver<RuntimePlane<D>, MemoryCluster>;

/// One node of one partition, its edge, and the driver that steps it.
pub struct World<D: ActionDispatcher + 'static> {
    pub plane: Arc<RuntimePlane<D>>,
    pub edge: Edge<RuntimePlane<D>>,
    pub at: NodeId,
    driver: Mutex<RuntimeDriver<D>>,
}

impl<D: ActionDispatcher + 'static> World<D> {
    /// A node running `executor` and `dispatcher`, its entropy seeded with `seed`, configured by
    /// `configure` past the tests' defaults.
    pub async fn new(
        executor: V8Executor,
        dispatcher: D,
        seed: u64,
        configure: impl FnOnce(&mut NodeConfig),
    ) -> Self {
        let changes = ControlPlaneChanges::new();
        let cluster = MemoryCluster::with_changes(1, changes.clone());
        let blobs = MemoryBlobs::new();
        let plane = Arc::new(Wiring {
            catalog: MemoryCatalog::with_changes(changes.clone()),
            partitions: MemoryPartitions::new(),
            archive: MemoryArchive::with_blobs(blobs.clone()),
            blobs,
            executor,
            dispatcher,
            secrets: MemorySecrets::with_changes(changes),
            spans: MemorySpanExporter::new(),
            webhooks: MemoryWebhooks::new(),
            clock: VirtualClock::new(T0),
            entropy: SeededEntropy::new(seed),
        });
        let mut config = NodeConfig::new(NodeId::new("runtime").unwrap(), NonZeroU16::MIN);
        config.harness_actions = HarnessActions::Dispatched;
        config.horizon = Duration::from_secs(100 * 365 * 86_400);
        configure(&mut config);
        let mut driver = Driver::new(cluster, Arc::new(PlaneClock(Arc::clone(&plane))));
        let at = driver
            .add_node(Arc::clone(&plane), config.clone())
            .await
            .unwrap();
        driver.run_until_idle().await.unwrap();
        let edge = Edge::new(Arc::clone(&plane), config);
        Self {
            plane,
            edge,
            at,
            driver: Mutex::new(driver),
        }
    }

    pub fn now(&self) -> Timestamp {
        self.plane.clock.now()
    }

    /// Moves the clock to `at`.
    pub fn advance_to(&self, at: Timestamp) {
        self.plane.clock.advance_to(at);
    }

    pub async fn start(&self, request: StartRun) -> Result<RunRef, StartError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        Ok(self.edge.start_run(&mut gateway, request).await?.run)
    }

    /// Steps until nothing is left to do now; the clock does not move.
    pub async fn settle(&self) {
        self.driver.lock().await.run_until_idle().await.unwrap();
    }

    /// Steps until only the outside world can wake a run, in virtual time; then every run must
    /// be healthy: no failed activation, no quarantine.
    pub async fn idle(&self) {
        self.driver
            .lock()
            .await
            .run_until_idle_virtual()
            .await
            .unwrap();
        for summary in self.runs(Vec::new()).await {
            let view = self.view(&summary.run).await;
            assert_eq!(view.head.failures, 0, "{:?}", view.head);
            assert!(view.head.quarantine.is_none(), "{:?}", view.head);
        }
    }

    /// The run, live or sealed.
    pub async fn view(&self, run: &RunRef) -> RunView {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge.get_run(&mut gateway, run).await.unwrap().unwrap()
    }

    /// The run's history, every payload loaded.
    pub async fn history(&self, run: &RunRef) -> Vec<HistoryEvent> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge
            .export_run(&mut gateway, run)
            .await
            .unwrap()
            .events
    }

    /// Replays the run's history with its code, or `code`.
    pub async fn replay(&self, run: &RunRef, code: Option<&Code>) -> ReplayVerdict {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge.replay(&mut gateway, run, code).await.unwrap()
    }

    pub async fn signal(
        &self,
        run: &RunRef,
        name: &str,
        payload: JsonValue,
    ) -> Result<Enqueued, EdgeError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge
            .signal(&mut gateway, run, name, payload, None)
            .await
    }

    pub async fn cancel(&self, run: &RunRef, message: Option<&str>) -> Result<Enqueued, EdgeError> {
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge
            .cancel(&mut gateway, run, message.map(str::to_owned))
            .await
    }

    /// The summaries of the tenant `local`'s runs that match every predicate, oldest first.
    pub async fn runs(&self, predicates: Vec<Predicate>) -> Vec<RunSummary> {
        let tenant = TenantId::local();
        let query = RunQuery {
            tenant: tenant.clone(),
            terms: predicates
                .into_iter()
                .map(|predicate| Term {
                    negated: false,
                    predicate,
                })
                .collect(),
        };
        let mut driver = self.driver.lock().await;
        let mut gateway = driver.gateway(&self.at);
        self.edge
            .search(
                &mut gateway,
                &Scope::Tenant(tenant),
                &query,
                RunOrder::Oldest,
                &Page::first(1_000),
            )
            .await
            .unwrap()
            .summaries
    }

    /// The instances the node's executors keep.
    pub async fn cached_instances(&self) -> usize {
        self.driver
            .lock()
            .await
            .node(&self.at)
            .unwrap()
            .cached_instances()
    }
}
