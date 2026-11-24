//! Nodes of one cluster in the test's process, linked over real HTTP/2 on loopback
//! (`archi/07-code-architecture.md`, "Tests": `rewind-server`'s row).
//!
//! [`join`] starts one node of any plane and cluster: a listener on loopback, the membership
//! with that address, the node spawned with an `HttpLink`, its link served. [`World`] builds a
//! cluster over the memory adapters, the V8 executor and real time, and follows its partition
//! map. A test of other adapters (Postgres, MinIO) wires its own plane and cluster and calls
//! [`join`].

#![allow(dead_code, unreachable_pub, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt as _};
use rewind_core::{ActionSet, JsonValue, PartitionId, RunId, RunStatus, Script, Timestamp};
use rewind_engine::StartRun;
use rewind_engine::adapters::{
    ChangeStream, ControlPlaneChanges, MemoryArchive, MemoryBlobs, MemoryCatalog, MemoryCluster,
    MemoryMembership, MemoryPartitions, MemorySecrets, MemorySpanExporter, MemoryWebhooks,
    StaticDispatcher, SystemClock, SystemEntropy,
};
use rewind_engine::edge::{Edge, Gateway as _};
use rewind_engine::node::rendezvous;
use rewind_engine::node::runtime::{self, NodeHandle};
use rewind_engine::plane::{HostTools, NodeConfig, Plane, Wiring};
use rewind_engine::ports::cluster::Membership;
use rewind_engine::ports::link::{
    Changed, PartitionReply, PartitionRequest, RunView, SubscriptionKey,
};
use rewind_engine::ports::{
    Cluster, ControlPlaneChange, Enqueued, NamespaceName, NodeId, NodeInfo, PartitionMap, RunRef,
    ScriptExecutor, TenantId, namespace_partition,
};
use rewind_runtime::{SandboxLimits, V8Executor};
use rewind_server::link::{self, ClusterSecret, HttpLink, MembersOf, SharedMembership};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// How long a test waits for the cluster to do what it expects.
pub const PATIENCE: Duration = Duration::from_secs(20);

/// The secret of the tests' clusters.
pub fn secret() -> ClusterSecret {
    ClusterSecret::new("the-secret-of-the-tests-cluster").unwrap()
}

/// A node of the cluster, its link served on loopback.
pub struct LinkedNode<P: Plane, M: Membership + 'static> {
    pub config: NodeConfig,
    pub handle: NodeHandle<P>,
    /// The node's link to the others.
    pub link: Arc<HttpLink<MembersOf<M>>>,
    /// Where its link listens.
    pub address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    server: Option<JoinHandle<()>>,
}

impl<P: Plane, M: Membership + 'static> LinkedNode<P, M> {
    pub fn id(&self) -> &NodeId {
        &self.config.node
    }

    /// Stops the node's link server, gracefully: its subscriptions end, and nothing reaches the
    /// node through the link any more.
    pub async fn stop_link(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            tokio::time::timeout(PATIENCE, server)
                .await
                .expect("the link server stops")
                .unwrap();
        }
    }
}

/// Starts a node of `cluster` over `plane`: its link on a listener of loopback, the member's
/// address that listener's, the node spawned with an [`HttpLink`] that reads the members
/// through the node's membership.
pub async fn join<P, C>(
    plane: &Arc<P>,
    cluster: &C,
    config: NodeConfig,
    secret: &ClusterSecret,
) -> LinkedNode<P, C::Membership>
where
    P: Plane,
    C: Cluster,
    C::Membership: 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let epochs = config
        .epochs
        .clone()
        .unwrap_or_else(|| plane.executor().epochs().iter().copied().collect());
    let membership = cluster
        .join(NodeInfo {
            id: config.node.clone(),
            address: address.to_string(),
            build: config.build.clone(),
            epochs,
            pool: config.pool.clone(),
            executors_only: config.executors_only,
        })
        .await
        .unwrap();
    let membership = SharedMembership::new(membership);
    let link = Arc::new(HttpLink::new(membership.members(), secret.clone()).unwrap());
    let handle = runtime::spawn(
        Arc::clone(plane),
        config.clone(),
        membership,
        Arc::clone(&link),
    );
    let (stop, stopped) = oneshot::channel::<()>();
    let server = link::serve(listener, handle.clone(), secret.clone(), async {
        let _ = stopped.await;
    });
    LinkedNode {
        config,
        handle,
        link,
        address,
        stop: Some(stop),
        server: Some(server),
    }
}

/// The plane of [`World`]: memory adapters, the V8 executor, real time.
pub type MemoryPlane = Wiring<
    MemoryCatalog,
    MemoryPartitions,
    MemoryArchive,
    MemoryBlobs,
    V8Executor,
    StaticDispatcher,
    MemorySecrets,
    MemorySpanExporter,
    MemoryWebhooks,
    SystemClock,
    SystemEntropy,
>;

pub type MemoryNode = LinkedNode<MemoryPlane, MemoryMembership>;

/// The configuration of a node of `partitions` partitions; its calls are dispatched, not handed
/// to SDK streams.
pub fn config(id: &str, partitions: u16) -> NodeConfig {
    let mut config = NodeConfig::new(
        NodeId::new(id).unwrap(),
        NonZeroU16::new(partitions).unwrap(),
    );
    config.host_tools = HostTools::Dispatched;
    config
}

/// Nodes of one cluster over shared memory adapters, each linked to the others over HTTP/2.
pub struct World {
    pub plane: Arc<MemoryPlane>,
    pub cluster: MemoryCluster,
    pub secret: ClusterSecret,
    pub partitions: NonZeroU16,
    pub nodes: Vec<MemoryNode>,
    /// The partition map as last committed.
    pub map: PartitionMap,
    maps: ChangeStream,
}

impl World {
    pub async fn new(partitions: u16) -> Self {
        let changes = ControlPlaneChanges::new();
        let cluster = MemoryCluster::with_changes(partitions, changes.clone());
        let blobs = MemoryBlobs::new();
        let plane = Arc::new(Wiring {
            catalog: MemoryCatalog::with_changes(changes.clone()),
            partitions: MemoryPartitions::new(),
            archive: MemoryArchive::with_blobs(blobs.clone()),
            blobs,
            executor: V8Executor::new(SandboxLimits::default()).unwrap(),
            dispatcher: StaticDispatcher::new(),
            secrets: MemorySecrets::with_changes(changes.clone()),
            spans: MemorySpanExporter::new(),
            webhooks: MemoryWebhooks::new(),
            clock: SystemClock,
            entropy: SystemEntropy,
        });
        Self {
            plane,
            maps: changes.subscribe(),
            cluster,
            secret: secret(),
            partitions: NonZeroU16::new(partitions).unwrap(),
            nodes: Vec::new(),
            map: PartitionMap::unowned(partitions),
        }
    }

    /// Starts a node, and waits until the cluster settled: each partition at the node that wins
    /// it, and open there.
    pub async fn join(&mut self, config: NodeConfig) -> NodeId {
        let node = join(&self.plane, &self.cluster, config, &self.secret).await;
        let id = node.id().clone();
        self.nodes.push(node);
        self.settled().await;
        id
    }

    /// Waits until every partition is at the node that wins it among the nodes that own
    /// partitions, and its owner answers: a node that joins while another still opens a
    /// partition it is about to lose is not what these tests are about.
    pub async fn settled(&mut self) {
        let owners: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|node| !node.config.executors_only)
            .map(|node| node.id().clone())
            .collect();
        if owners.is_empty() {
            return;
        }
        self.map_where(|map| {
            map.partitions.iter().all(|(partition, owner)| {
                owner.node.as_ref() == rendezvous(*partition, owners.iter())
            })
        })
        .await;
        let mut gateway = self.nodes[0].handle.clone();
        for partition in 0..self.partitions.get() {
            let partition = PartitionId::new(partition);
            let probe = RunRef::new(
                TenantId::local(),
                RunId::in_partition(partition, Timestamp(0), 0),
            );
            patiently("the partition opens", async {
                loop {
                    let request = PartitionRequest::GetRun { run: probe.clone() };
                    if let Ok(PartitionReply::Run(_)) = gateway.partition(partition, request).await
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
        }
    }

    pub fn node(&self, id: &NodeId) -> &MemoryNode {
        self.nodes
            .iter()
            .find(|node| node.id() == id)
            .expect("a node of the world")
    }

    pub fn node_mut(&mut self, id: &NodeId) -> &mut MemoryNode {
        self.nodes
            .iter_mut()
            .find(|node| node.id() == id)
            .expect("a node of the world")
    }

    /// Waits for a partition map that `holds`, as the cluster commits them.
    pub async fn map_where(&mut self, holds: impl Fn(&PartitionMap) -> bool) -> PartitionMap {
        let wait = async {
            while !holds(&self.map) {
                match self.maps.next().await {
                    Some(ControlPlaneChange::PartitionMap(map)) => self.map = map,
                    Some(_) => {}
                    None => panic!("the cluster's changes ended"),
                }
            }
            self.map.clone()
        };
        tokio::time::timeout(PATIENCE, wait)
            .await
            .expect("the partition map the test waits for")
    }

    /// Waits until every partition has an owner among `owners`, each owning one at least.
    pub async fn owned_by(&mut self, owners: &[&NodeId]) -> PartitionMap {
        self.map_where(|map| {
            map.partitions.values().all(|owner| {
                owner
                    .node
                    .as_ref()
                    .is_some_and(|node| owners.contains(&node))
            }) && owners.iter().all(|node| !map.owned_by(node).is_empty())
        })
        .await
    }

    /// An edge whose requests enter the cluster at `at`.
    pub fn edge(&self, at: &NodeId) -> Edge<MemoryPlane> {
        Edge::new(Arc::clone(&self.plane), self.node(at).config.clone())
    }

    /// A namespace of the tenant `local` whose partition `owner` holds in the map last seen.
    pub fn namespace_on(&self, owner: &NodeId) -> NamespaceName {
        (0..10_000)
            .map(|n| NamespaceName::new(format!("user-{n}")).unwrap())
            .find(|name| {
                let partition = namespace_partition(&TenantId::local(), name, self.partitions);
                self.map.owner(partition) == Some(owner)
            })
            .expect("the node owns a partition")
    }
}

/// A start of `source` in `namespace`, with no tool.
pub fn start(source: &str, namespace: &NamespaceName, input: JsonValue) -> StartRun {
    let mut request = StartRun::new(Script::new(source), ActionSet::empty(), input);
    request.namespace = namespace.clone();
    request
}

/// Retries a request the cluster cannot answer yet: a node may not have taken the partition
/// map in, or not opened a partition it was given.
fn not_yet(error: &impl std::fmt::Display) -> bool {
    error.to_string().contains("unavailable")
}

async fn patiently<T>(what: &str, wait: impl Future<Output = T>) -> T {
    tokio::time::timeout(PATIENCE, wait)
        .await
        .unwrap_or_else(|_| panic!("{what} in time"))
}

impl World {
    /// Starts a run through the edge of `at`.
    pub async fn start(&self, at: &NodeId, request: StartRun) -> RunRef {
        let edge = self.edge(at);
        let mut gateway = self.node(at).handle.clone();
        let mut last = String::new();
        let started = tokio::time::timeout(PATIENCE, async {
            loop {
                match edge.start_run(&mut gateway, request.clone()).await {
                    Ok(created) => return created.run,
                    Err(error) if not_yet(&error) => {
                        last = error.to_string();
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(error) => panic!("the start was refused: {error}"),
                }
            }
        })
        .await;
        started.unwrap_or_else(|_| panic!("the run did not start: {last}"))
    }

    /// Signals a run through the edge of `at`.
    pub async fn signal(&self, at: &NodeId, run: &RunRef, name: &str, payload: JsonValue) {
        let edge = self.edge(at);
        let mut gateway = self.node(at).handle.clone();
        patiently("the signal is taken", async {
            loop {
                match edge
                    .signal(&mut gateway, run, name, payload.clone(), None)
                    .await
                {
                    Ok(Enqueued::Accepted) => return,
                    Ok(other) => panic!("the signal was not taken: {other:?}"),
                    Err(error) if not_yet(&error) => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(error) => panic!("the signal was refused: {error}"),
                }
            }
        })
        .await;
    }

    /// The run as its owner knows it, read through the edge of `at`.
    pub async fn view(&self, at: &NodeId, run: &RunRef) -> RunView {
        let edge = self.edge(at);
        let mut gateway = self.node(at).handle.clone();
        patiently("the run is read", async {
            loop {
                match edge.get_run(&mut gateway, run).await {
                    Ok(Some(view)) => return view,
                    Ok(None) => panic!("run {} not found", run.id),
                    Err(error) if not_yet(&error) => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(error) => panic!("the run cannot be read: {error}"),
                }
            }
        })
        .await
    }

    /// The changes of the tenant `local`, at every node: each run changes at its owner.
    pub fn changes(&self) -> impl Stream<Item = Changed> + Unpin + use<> {
        futures_util::stream::select_all(self.nodes.iter().map(|node| {
            node.handle
                .subscribe(SubscriptionKey::Tenant(TenantId::local()), None)
                .unwrap()
        }))
    }
}

/// Waits until every run of `runs` has a change with the status `status`.
pub async fn until_all(
    changes: &mut (impl Stream<Item = Changed> + Unpin),
    runs: &[RunRef],
    status: RunStatus,
) {
    let mut left: BTreeSet<RunRef> = runs.iter().cloned().collect();
    patiently("the runs reach their status", async {
        while !left.is_empty() {
            let change = changes.next().await.expect("the changes go on");
            if change
                .summary
                .as_ref()
                .is_some_and(|summary| summary.status == status)
            {
                left.remove(&change.run);
            }
        }
    })
    .await;
}

/// The epochs of the plane's executor.
pub fn epochs(plane: &MemoryPlane) -> BTreeSet<rewind_core::Epoch> {
    plane.executor.epochs().iter().copied().collect()
}
