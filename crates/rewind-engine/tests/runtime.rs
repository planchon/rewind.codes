//! Nodes in tokio tasks, in real time, over the in-process link (`archi/07-code-architecture.md`,
//! "Inside a node": in a process, tasks drive the actors).

#![allow(clippy::unwrap_used)]

mod support;

use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use rewind_core::RunStatus;
use rewind_engine::adapters::{
    ChangeStream, ControlPlaneChanges, LocalLink, MemoryArchive, MemoryBlobs, MemoryCatalog,
    MemoryCluster, MemoryMembership, MemoryPartitions, MemorySecrets, MemorySpanExporter,
    MemoryWebhooks, SeededEntropy, SystemClock,
};
use rewind_engine::edge::Edge;
use rewind_engine::node::runtime::{self, NodeHandle};
use rewind_engine::plane::{HarnessActions, NodeConfig, Wiring};
use rewind_engine::ports::cluster::Membership;
use rewind_engine::ports::link::{NodeRequest, OrgRequest, SubscriptionKey};
use rewind_engine::ports::{
    Cluster, ClusterError, Departed, Moves, NodeId, NodeInfo, NodeLink, PartitionMap,
    ScriptExecutor, TenantId,
};
use serde_json::json;
use support::{FakeExecutor, RecordingDispatcher, request, result_of};

type RealPlane = Wiring<
    MemoryCatalog,
    MemoryPartitions,
    MemoryArchive,
    MemoryBlobs,
    FakeExecutor,
    RecordingDispatcher,
    MemorySecrets,
    MemorySpanExporter,
    MemoryWebhooks,
    SystemClock,
    SeededEntropy,
>;

fn chain() -> FakeExecutor {
    FakeExecutor::new().with(
        "chain",
        |run| {
            run.timer(20);
        },
        |run, delivery| match delivery {
            rewind_engine::ports::Delivery::TimerFired { .. } => {
                run.call("a", json!({ "n": 1 }));
            }
            other => run.complete(result_of(other).clone()),
        },
    )
}

async fn node(
    plane: &Arc<RealPlane>,
    cluster: &MemoryCluster,
    link: &Arc<LocalLink<RealPlane>>,
    id: &str,
) -> (NodeHandle<RealPlane>, NodeConfig) {
    let mut config = NodeConfig::new(NodeId::new(id).unwrap(), NonZeroU16::new(4).unwrap());
    config.harness_actions = HarnessActions::Dispatched;
    let membership = cluster
        .join(NodeInfo {
            id: config.node.clone(),
            address: format!("{id}.local"),
            build: config.build.clone(),
            epochs: plane.executor.epochs().iter().copied().collect(),
            pool: None,
            executors_only: false,
        })
        .await
        .unwrap();
    let handle = runtime::spawn(
        Arc::clone(plane),
        config.clone(),
        membership,
        Arc::clone(link),
    );
    link.register(handle.clone());
    (handle, config)
}

#[tokio::test]
async fn two_nodes_run_a_run_through_the_link_in_real_time() {
    let changes = ControlPlaneChanges::new();
    let cluster = MemoryCluster::with_changes(4, changes.clone());
    let blobs = MemoryBlobs::new();
    let plane = Arc::new(Wiring {
        catalog: MemoryCatalog::with_changes(changes.clone()),
        partitions: MemoryPartitions::new(),
        archive: MemoryArchive::with_blobs(blobs.clone()),
        blobs,
        executor: chain(),
        dispatcher: RecordingDispatcher::echo(),
        secrets: MemorySecrets::with_changes(changes),
        spans: MemorySpanExporter::new(),
        webhooks: MemoryWebhooks::new(),
        clock: SystemClock,
        entropy: SeededEntropy::new(7),
    });
    let link = Arc::new(LocalLink::new());
    let (a, config) = node(&plane, &cluster, &link, "node-a").await;
    let (b, _) = node(&plane, &cluster, &link, "node-b").await;
    let mut changed_a = a
        .subscribe(SubscriptionKey::Tenant(TenantId::local()), None)
        .unwrap();
    let mut changed_b = b
        .subscribe(SubscriptionKey::Tenant(TenantId::local()), None)
        .unwrap();

    // The nodes take their partitions; an edge's request may wait for the map.
    let edge = Edge::new(Arc::clone(&plane), config);
    let mut gateway = a.clone();
    let created = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match edge
                .start_run(&mut gateway, request("chain", json!({})))
                .await
            {
                Ok(created) => return created,
                Err(error) => {
                    assert!(error.to_string().contains("unavailable"), "{error}");
                    tokio::task::yield_now().await;
                }
            }
        }
    })
    .await
    .unwrap();

    // The run ends on whichever node owns its partition: its change says so.
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        let mut changes = futures_util::stream::select(&mut changed_a, &mut changed_b);
        while let Some(change) = changes.next().await {
            if change.run == created.run
                && change
                    .summary
                    .as_ref()
                    .is_some_and(|summary| summary.status == RunStatus::Completed)
            {
                return true;
            }
        }
        false
    })
    .await
    .unwrap();
    assert!(ended);
    let view = edge
        .get_run(&mut gateway, &created.run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(view.head.status, RunStatus::Completed);

    // The link carries requests to a node, and refuses a node it cannot reach.
    link.cut(b.node().clone());
    let refused = link
        .call(
            b.node(),
            NodeRequest::Organization {
                tenant: TenantId::local(),
                request: Box::new(OrgRequest::Detach {
                    stream: rewind_engine::ports::link::StreamId("none".into()),
                }),
            },
        )
        .await;
    assert!(refused.is_err());
    link.heal(b.node());
    a.stop().await.unwrap();
}

/// A membership whose writes of the map take a while, as the control plane's over a network.
struct SlowMembership {
    inner: Arc<MemoryMembership>,
    delay: Duration,
}

impl Membership for SlowMembership {
    type Changes = ChangeStream;

    fn node(&self) -> &NodeId {
        self.inner.node()
    }

    async fn watch(&self) -> Result<Departed, ClusterError> {
        self.inner.watch().await
    }

    async fn assign(&self, moves: &Moves) -> Result<PartitionMap, ClusterError> {
        tokio::time::sleep(self.delay).await;
        self.inner.assign(moves).await
    }

    async fn partition_map(&self) -> Result<PartitionMap, ClusterError> {
        self.inner.partition_map().await
    }

    async fn nodes(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        self.inner.nodes().await
    }

    async fn changes(&self) -> Result<ChangeStream, ClusterError> {
        self.inner.changes().await
    }

    async fn leave(self) -> Result<(), ClusterError> {
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => inner.leave().await,
            Err(_) => Ok(()),
        }
    }
}

/// A stop is answered once the node's partitions are handed over, however long the control
/// plane takes to write the map: the process exits after the write, never during it (a node
/// stopped while it opened its partitions kept them, its hand-over cut by the exit).
#[tokio::test]
async fn a_stop_is_answered_once_the_partitions_are_handed_over() {
    let changes = ControlPlaneChanges::new();
    let cluster = MemoryCluster::with_changes(4, changes.clone());
    let blobs = MemoryBlobs::new();
    let plane = Arc::new(Wiring {
        catalog: MemoryCatalog::with_changes(changes.clone()),
        partitions: MemoryPartitions::new(),
        archive: MemoryArchive::with_blobs(blobs.clone()),
        blobs,
        executor: chain(),
        dispatcher: RecordingDispatcher::echo(),
        secrets: MemorySecrets::with_changes(changes),
        spans: MemorySpanExporter::new(),
        webhooks: MemoryWebhooks::new(),
        clock: SystemClock,
        entropy: SeededEntropy::new(7),
    });
    let link = Arc::new(LocalLink::new());
    let (a, _) = node(&plane, &cluster, &link, "node-a").await;
    let mut config = NodeConfig::new(NodeId::new("node-b").unwrap(), NonZeroU16::new(4).unwrap());
    config.harness_actions = HarnessActions::Dispatched;
    let joined = cluster
        .join(NodeInfo {
            id: config.node.clone(),
            address: "node-b.local".into(),
            build: config.build.clone(),
            epochs: plane.executor.epochs().iter().copied().collect(),
            pool: None,
            executors_only: false,
        })
        .await
        .unwrap();
    let membership = Arc::new(joined);
    let b = runtime::spawn(
        Arc::clone(&plane),
        config.clone(),
        SlowMembership {
            inner: Arc::clone(&membership),
            delay: Duration::from_millis(200),
        },
        Arc::clone(&link),
    );
    link.register(b.clone());
    let owned = || async {
        membership
            .partition_map()
            .await
            .unwrap()
            .owned_by(&config.node)
            .len()
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while owned().await == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    b.stop().await.unwrap();
    assert_eq!(
        owned().await,
        0,
        "the stop was answered before the hand-over"
    );
    a.stop().await.unwrap();
}
