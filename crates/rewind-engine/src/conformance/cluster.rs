//! The conformance suite of `Cluster`: members and their liveness, the ring each member
//! watches, the partition map and the epochs of its moves, the changes every member receives,
//! the members' joins and departures among them.
//!
//! The factory creates an empty cluster of [`PARTITIONS`] partitions. A member's liveness ends
//! when its `Membership` is dropped or leaves. A check that a `watch` still waits polls it once:
//! on a backend whose lock comes free a little later, it holds trivially.

use std::collections::BTreeSet;

use futures_util::{FutureExt as _, StreamExt as _};
use rewind_core::{Epoch, PartitionId};

use crate::ports::cluster::Membership;
use crate::ports::{
    Cluster, ClusterError, ControlPlaneChange, Departed, Moves, NodeId, NodeInfo, PartitionEpoch,
    PartitionMap,
};

/// The partitions of the clusters the suite runs on.
pub const PARTITIONS: u16 = 4;

/// Runs every check, each on a new cluster.
pub async fn run_all<C: Cluster>(new_cluster: impl Fn() -> C) {
    macro_rules! cluster_check {
        ($check:ident) => {{
            eprintln!(concat!("cluster conformance: ", stringify!($check)));
            $check(&new_cluster()).await;
        }};
    }
    cluster_check!(members_are_listed_by_id_until_they_leave);
    cluster_check!(a_node_joins_once_at_a_time);
    cluster_check!(a_new_cluster_has_every_partition_unowned_at_epoch_zero);
    cluster_check!(assign_moves_partitions_and_raises_their_epochs);
    cluster_check!(assign_refuses_unknown_nodes_and_partitions_and_moves_nothing);
    cluster_check!(changes_reach_every_member_from_when_it_listens);
    cluster_check!(a_node_that_joins_or_leaves_is_announced);
    cluster_check!(watch_names_the_next_node_of_the_ring_once_it_is_gone);
    cluster_check!(a_member_alone_waits_for_another);
}

fn node(id: &str) -> NodeId {
    NodeId::new(id).expect("the suite uses valid node ids")
}

fn info(id: &str) -> NodeInfo {
    NodeInfo {
        id: node(id),
        address: format!("{id}.internal:7070"),
        build: "build-1".into(),
        epochs: BTreeSet::from([Epoch(1), Epoch(2)]),
        pool: None,
        executors_only: false,
    }
}

async fn join<C: Cluster>(cluster: &C, id: &str) -> C::Membership {
    cluster.join(info(id)).await.expect("join")
}

fn partition(id: u16) -> PartitionId {
    PartitionId::new(id)
}

fn owners(map: &PartitionMap) -> Vec<(u16, Option<String>, u64)> {
    map.partitions
        .iter()
        .map(|(partition, owner)| {
            (
                partition.get(),
                owner.node.as_ref().map(ToString::to_string),
                owner.epoch.0,
            )
        })
        .collect()
}

pub async fn members_are_listed_by_id_until_they_leave<C: Cluster>(cluster: &C) {
    let b = join(cluster, "node-b").await;
    let mut pooled = info("node-a");
    pooled.pool = Some("sandboxed".into());
    pooled.executors_only = true;
    let a = cluster.join(pooled.clone()).await.expect("join");
    assert_eq!(a.node(), &node("node-a"));
    let nodes = b.nodes().await.expect("nodes");
    assert_eq!(nodes, [pooled, info("node-b")], "by id, as they joined");
    b.leave().await.expect("leave");
    let ids: Vec<NodeId> = a
        .nodes()
        .await
        .expect("nodes")
        .into_iter()
        .map(|info| info.id)
        .collect();
    assert_eq!(ids, [node("node-a")]);
    drop(a);
    let c = join(cluster, "node-c").await;
    let ids: Vec<NodeId> = c
        .nodes()
        .await
        .expect("nodes")
        .into_iter()
        .map(|info| info.id)
        .collect();
    assert_eq!(ids, [node("node-c")], "a dropped membership is no member");
}

pub async fn a_node_joins_once_at_a_time<C: Cluster>(cluster: &C) {
    let a = join(cluster, "node-a").await;
    let again = cluster.join(info("node-a")).await;
    assert!(
        matches!(&again, Err(ClusterError::AlreadyMember(id)) if *id == node("node-a")),
        "{:?}",
        again.as_ref().map(Membership::node)
    );
    a.leave().await.expect("leave");
    let rejoined = join(cluster, "node-a").await;
    assert_eq!(rejoined.nodes().await.expect("nodes").len(), 1);
}

pub async fn a_new_cluster_has_every_partition_unowned_at_epoch_zero<C: Cluster>(cluster: &C) {
    let a = join(cluster, "node-a").await;
    let map = a.partition_map().await.expect("partition_map");
    assert_eq!(map, PartitionMap::unowned(PARTITIONS));
    assert_eq!(map.epoch(partition(0)), Some(PartitionEpoch(0)));
    assert_eq!(map.owner(partition(0)), None);
}

pub async fn assign_moves_partitions_and_raises_their_epochs<C: Cluster>(cluster: &C) {
    let (a, b) = (join(cluster, "node-a").await, join(cluster, "node-b").await);
    let all = (0..PARTITIONS).fold(Moves::new(), |moves, id| {
        moves.to(partition(id), node("node-a"))
    });
    let map = a.assign(&all).await.expect("assign");
    assert_eq!(
        owners(&map),
        (0..PARTITIONS)
            .map(|id| (id, Some("node-a".to_owned()), 1))
            .collect::<Vec<_>>()
    );
    let moved = b
        .assign(
            &Moves::new()
                .to(partition(1), node("node-b"))
                .to(partition(2), node("node-b"))
                .to(partition(3), node("node-a")),
        )
        .await
        .expect("assign");
    assert_eq!(
        owners(&moved),
        [
            (0, Some("node-a".to_owned()), 1),
            (1, Some("node-b".to_owned()), 2),
            (2, Some("node-b".to_owned()), 2),
            (3, Some("node-a".to_owned()), 1),
        ],
        "a partition moved to its owner keeps its epoch"
    );
    assert_eq!(a.partition_map().await.expect("partition_map"), moved);
    assert_eq!(b.partition_map().await.expect("partition_map"), moved);
    assert_eq!(
        moved.owned_by(&node("node-b")),
        [partition(1), partition(2)]
    );
    let empty = a.assign(&Moves::new()).await.expect("assign nothing");
    assert_eq!(empty, moved);
}

pub async fn assign_refuses_unknown_nodes_and_partitions_and_moves_nothing<C: Cluster>(
    cluster: &C,
) {
    let a = join(cluster, "node-a").await;
    let unknown_node = a
        .assign(
            &Moves::new()
                .to(partition(0), node("node-a"))
                .to(partition(1), node("node-x")),
        )
        .await;
    assert_eq!(unknown_node, Err(ClusterError::UnknownNode(node("node-x"))));
    let unknown_partition = a
        .assign(
            &Moves::new()
                .to(partition(0), node("node-a"))
                .to(partition(PARTITIONS), node("node-a")),
        )
        .await;
    assert_eq!(
        unknown_partition,
        Err(ClusterError::UnknownPartition(partition(PARTITIONS)))
    );
    assert_eq!(
        a.partition_map().await.expect("partition_map"),
        PartitionMap::unowned(PARTITIONS),
        "a refused assignment moves nothing"
    );
}

pub async fn changes_reach_every_member_from_when_it_listens<C: Cluster>(cluster: &C) {
    let (a, b) = (join(cluster, "node-a").await, join(cluster, "node-b").await);
    let mut changes_a = a.changes().await.expect("changes");
    let mut changes_b = b.changes().await.expect("changes");
    let first = a
        .assign(&Moves::new().to(partition(0), node("node-a")))
        .await
        .expect("assign");
    let mut late = b.changes().await.expect("changes");
    let second = b
        .assign(&Moves::new().to(partition(0), node("node-b")))
        .await
        .expect("assign");
    for changes in [&mut changes_a, &mut changes_b] {
        assert_eq!(
            changes.next().await,
            Some(ControlPlaneChange::PartitionMap(first.clone()))
        );
        assert_eq!(
            changes.next().await,
            Some(ControlPlaneChange::PartitionMap(second.clone())),
            "in commit order"
        );
    }
    assert_eq!(
        late.next().await,
        Some(ControlPlaneChange::PartitionMap(second)),
        "a stream receives what was committed after it was opened, and only that"
    );
}

pub async fn a_node_that_joins_or_leaves_is_announced<C: Cluster>(cluster: &C) {
    let a = join(cluster, "node-a").await;
    let mut changes = a.changes().await.expect("changes");
    let b = join(cluster, "node-b").await;
    assert_eq!(
        changes.next().await,
        Some(ControlPlaneChange::Nodes),
        "a node that joins is announced"
    );
    // Its departure is seen once the node that watches it finds its lock free.
    let mut watching = Box::pin(a.watch());
    assert!(
        watching.as_mut().now_or_never().is_none(),
        "node-b is alive"
    );
    drop(b);
    assert_eq!(
        watching.await.expect("watch"),
        Departed {
            node: node("node-b")
        }
    );
    assert_eq!(
        changes.next().await,
        Some(ControlPlaneChange::Nodes),
        "a node that left is announced"
    );
    assert!(
        changes.next().now_or_never().is_none(),
        "nothing else was announced"
    );
}

pub async fn watch_names_the_next_node_of_the_ring_once_it_is_gone<C: Cluster>(cluster: &C) {
    let a = join(cluster, "node-a").await;
    let b = join(cluster, "node-b").await;
    let c = join(cluster, "node-c").await;
    let mut a_watches_b = Box::pin(a.watch());
    let mut c_watches_a = Box::pin(c.watch());
    let mut b_watches_c = Box::pin(b.watch());
    assert!(
        a_watches_b.as_mut().now_or_never().is_none(),
        "node-b is alive"
    );
    assert!(
        c_watches_a.as_mut().now_or_never().is_none(),
        "node-a is alive"
    );
    assert!(
        b_watches_c.as_mut().now_or_never().is_none(),
        "node-c is alive"
    );
    drop(b_watches_c);

    drop(b);
    assert_eq!(
        a_watches_b.await.expect("watch"),
        Departed {
            node: node("node-b")
        }
    );
    assert!(
        c_watches_a.as_mut().now_or_never().is_none(),
        "node-a is still alive"
    );
    drop(c_watches_a);
    let ids: Vec<NodeId> = a
        .nodes()
        .await
        .expect("nodes")
        .into_iter()
        .map(|info| info.id)
        .collect();
    assert_eq!(ids, [node("node-a"), node("node-c")]);

    let mut a_watches_c = Box::pin(a.watch());
    assert!(
        a_watches_c.as_mut().now_or_never().is_none(),
        "node-c is alive"
    );
    c.leave().await.expect("leave");
    assert_eq!(
        a_watches_c.await.expect("watch"),
        Departed {
            node: node("node-c")
        },
        "a node that leaves frees its lock as a dead one does"
    );
}

pub async fn a_member_alone_waits_for_another<C: Cluster>(cluster: &C) {
    let a = join(cluster, "node-a").await;
    let mut alone = Box::pin(a.watch());
    assert!(alone.as_mut().now_or_never().is_none());
    drop(alone);
    let b = join(cluster, "node-b").await;
    let mut a_watches_b = Box::pin(a.watch());
    assert!(a_watches_b.as_mut().now_or_never().is_none());
    drop(b);
    assert_eq!(
        a_watches_b.await.expect("watch"),
        Departed {
            node: node("node-b")
        }
    );
}
