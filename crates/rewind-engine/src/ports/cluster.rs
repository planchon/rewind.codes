//! The `Cluster` port: the nodes alive, the partition map and its epochs, a node's liveness, and
//! the changes of the control plane (`archi/07-code-architecture.md`, "`Cluster` and
//! `NodeLink`"; `archi/06-storage-and-scale.md`, "Partitions, owners and nodes").
//!
//! ```text
//!   Cluster::join(NodeInfo) ──► Membership ── watch() ──► Departed { node }   the next node of
//!                                  │                                          the ring died
//!                                  ├── assign(Moves) ──► PartitionMap          owners, epochs + 1
//!                                  ├── partition_map(), nodes()
//!                                  ├── changes() ──► stream of ControlPlaneChange
//!                                  └── leave()                                 after the hand-over
//! ```
//!
//! A node's liveness is its membership: on Postgres, a session advisory lock on the node's id
//! held on a connection of its own, released when the process or its connection dies. The nodes
//! form a ring, ordered by id, in which each waits on the lock of the next.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;

use futures_util::Stream;
use rewind_core::{Digest, Epoch, PartitionId};

use crate::ports::access::Principal;
use crate::ports::namespaces::NamespaceName;
use crate::ports::secrets::SecretId;
use crate::ports::tenancy::TenantId;

/// A node of the cluster: a process, which is both an HTTP edge and the owner of some
/// partitions. 1 to 64 ASCII letters, digits, `-`, `_` and `.`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid node id {0:?}: expected 1 to 64 ASCII letters, digits, -, _ and .")]
pub struct InvalidNodeId(pub String);

impl NodeId {
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidNodeId> {
        let id = id.into();
        let valid = (1..=64).contains(&id.len())
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte));
        if valid {
            Ok(Self(id))
        } else {
            Err(InvalidNodeId(id))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a node declares when it joins: the `nodes` row of the control plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    pub id: NodeId,
    /// Where the other nodes reach it through the node link: `host:port` on the private network.
    pub address: String,
    /// The build it runs.
    pub build: String,
    /// The determinism epochs its executors support (`archi/03-execution.md`, "Determinism
    /// epoch").
    pub epochs: BTreeSet<Epoch>,
    /// The pool of executors it belongs to (`--pool <name>`); `None`: the shared pool.
    pub pool: Option<String>,
    /// It runs activations for other owners and owns no partition (`--executors-only`).
    pub executors_only: bool,
}

/// How many times a partition has moved: raised by one at each move, so that a stale owner is
/// told apart from the current one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PartitionEpoch(pub u64);

impl PartitionEpoch {
    /// The epoch after a move.
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for PartitionEpoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A partition's place in the map: its owner, and the epoch of its last move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionOwner {
    /// `None` until a node takes it.
    pub node: Option<NodeId>,
    pub epoch: PartitionEpoch,
}

/// Which node owns each partition of the deployment: every partition, from 0 to the number of
/// partitions, is listed. A new cluster's partitions have no owner, at epoch 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartitionMap {
    pub partitions: BTreeMap<PartitionId, PartitionOwner>,
}

impl PartitionMap {
    /// A map of `count` partitions, none owned, at epoch 0.
    pub fn unowned(count: u16) -> Self {
        Self {
            partitions: (0..count)
                .map(|id| {
                    (
                        PartitionId::new(id),
                        PartitionOwner {
                            node: None,
                            epoch: PartitionEpoch::default(),
                        },
                    )
                })
                .collect(),
        }
    }

    /// The owner of a partition, if it has one.
    pub fn owner(&self, partition: PartitionId) -> Option<&NodeId> {
        self.partitions.get(&partition)?.node.as_ref()
    }

    /// The epoch of a partition; `None` for a partition the deployment does not have.
    pub fn epoch(&self, partition: PartitionId) -> Option<PartitionEpoch> {
        self.partitions.get(&partition).map(|owner| owner.epoch)
    }

    /// The partitions a node owns, in order.
    pub fn owned_by(&self, node: &NodeId) -> Vec<PartitionId> {
        self.partitions
            .iter()
            .filter(|(_, owner)| owner.node.as_ref() == Some(node))
            .map(|(partition, _)| *partition)
            .collect()
    }
}

/// Partitions to move, each to its new owner, in one transaction.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Moves {
    pub moves: BTreeMap<PartitionId, NodeId>,
}

impl Moves {
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves `partition` to `node`.
    pub fn to(mut self, partition: PartitionId, node: NodeId) -> Self {
        self.moves.insert(partition, node);
        self
    }
}

/// The node a member waited on is gone: its lock came free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Departed {
    pub node: NodeId,
}

/// What a write of the control plane changed (`archi/07-code-architecture.md`, "`Cluster` and
/// `NodeLink`"). Every process listens, and drops what it had cached of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlPlaneChange {
    /// `assign` moved partitions: the whole map, as it committed.
    PartitionMap(PartitionMap),
    /// A node joined the cluster, or its departure was seen: the members are read again, so an
    /// executor that joins takes activations at once.
    Nodes,
    Namespace {
        tenant: TenantId,
        name: NamespaceName,
    },
    Tenant {
        tenant: TenantId,
    },
    /// An API key, by the SHA-256 of its secret.
    Key {
        tenant: TenantId,
        hash: Digest,
    },
    /// An end user's opaque token, by the SHA-256 of its secret.
    Token {
        tenant: TenantId,
        hash: Digest,
    },
    /// A key a tenant signs its end users' tokens with (`rk_…`).
    TokenKey {
        tenant: TenantId,
        kid: String,
    },
    Grant {
        tenant: TenantId,
        principal: Principal,
    },
    Role {
        tenant: TenantId,
        name: String,
    },
    /// A person's connection (`cn_…`), which belongs to no tenant.
    Connection {
        id: String,
    },
    Secret {
        tenant: TenantId,
        secret: SecretId,
    },
    Workflow {
        tenant: TenantId,
        name: String,
        version: Digest,
    },
    Subscription {
        tenant: TenantId,
        id: String,
    },
    Script {
        tenant: TenantId,
        digest: Digest,
    },
    Toolset {
        tenant: TenantId,
        digest: Digest,
    },
    /// The index row of a test: reserved, marked for deletion, or dropped.
    Test {
        tenant: TenantId,
        digest: Digest,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClusterError {
    /// The membership ended: the node left, or its liveness was lost (its connection, its
    /// process). A node that is not a member owns nothing any more.
    #[error("node {0} is not a member of the cluster")]
    NotMember(NodeId),
    /// `join` of a node id that is a member already.
    #[error("node {0} is a member already")]
    AlreadyMember(NodeId),
    /// `assign` to a node that is not a member.
    #[error("node {0} is not a member of the cluster")]
    UnknownNode(NodeId),
    /// `assign` of a partition the deployment does not have.
    #[error("the deployment has no partition {0}")]
    UnknownPartition(PartitionId),
    /// The control plane cannot be reached; the call may be tried again.
    #[error("the cluster is unavailable: {0}")]
    Unavailable(String),
    #[error("cluster backend error: {0}")]
    Backend(String),
}

/// The cluster's membership: joining it is a node's liveness.
///
/// The contract, checked by the conformance suite (feature `conformance`, module
/// `conformance::cluster`):
///
/// - **Liveness.** A member stays a member until it leaves or its liveness is lost: dropping its
///   [`Membership`] loses it (on Postgres, the connection that holds its lock closes).
/// - **The ring.** Members are ordered by id, and each [`Membership::watch`]es the next one,
///   the last the first. `watch` returns [`Departed`] once the membership of that node ended,
///   and waits while the member is alone.
/// - **Moves.** [`Membership::assign`] moves partitions in one transaction: each partition whose
///   owner changes gets the new owner and its epoch + 1; a partition moved to its current owner
///   keeps its epoch. Every member then reads the new map, and receives it as a
///   [`ControlPlaneChange::PartitionMap`].
/// - **Changes.** [`Membership::changes`] yields every [`ControlPlaneChange`] committed by any
///   process after it returned, in commit order. A stream that ends may have missed changes:
///   its listener drops everything it cached and listens again.
pub trait Cluster: Send + Sync {
    type Membership: Membership;

    /// Joins the cluster: records the node and takes its liveness. A node id that is a member
    /// already is refused ([`ClusterError::AlreadyMember`]).
    fn join(
        &self,
        node: NodeInfo,
    ) -> impl Future<Output = Result<Self::Membership, ClusterError>> + Send;
}

/// A node's membership of the cluster. Every call of a membership that ended fails with
/// [`ClusterError::NotMember`].
pub trait Membership: Send + Sync {
    /// The changes of the control plane, as [`Membership::changes`] streams them.
    type Changes: Stream<Item = ControlPlaneChange> + Send + Unpin + 'static;

    /// The member's node.
    fn node(&self) -> &NodeId;

    /// Waits until the membership of the next node of the ring ends, and names that node.
    fn watch(&self) -> impl Future<Output = Result<Departed, ClusterError>> + Send;

    /// Moves partitions in one transaction, raising each moved partition's epoch, and sends the
    /// new map as it commits. Refuses, moving nothing, a node that is not a member and a
    /// partition the deployment does not have.
    fn assign(
        &self,
        moves: &Moves,
    ) -> impl Future<Output = Result<PartitionMap, ClusterError>> + Send;

    fn partition_map(&self) -> impl Future<Output = Result<PartitionMap, ClusterError>> + Send;

    /// The members, by id.
    fn nodes(&self) -> impl Future<Output = Result<Vec<NodeInfo>, ClusterError>> + Send;

    /// The changes committed by any process from now on.
    fn changes(&self) -> impl Future<Output = Result<Self::Changes, ClusterError>> + Send;

    /// Leaves the cluster, once the node handed its partitions over: its liveness ends, and the
    /// node that watches it is told.
    fn leave(self) -> impl Future<Output = Result<(), ClusterError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids_are_safe_in_keys() {
        for valid in ["node-1", "ip-10-0-0-7.eu", "a_b", &"x".repeat(64)] {
            assert_eq!(NodeId::new(valid).unwrap().as_str(), valid);
        }
        for invalid in ["", "a b", "a/b", "é", &"x".repeat(65)] {
            assert_eq!(NodeId::new(invalid), Err(InvalidNodeId(invalid.to_owned())));
        }
    }

    #[test]
    fn a_new_map_lists_every_partition_unowned() {
        let map = PartitionMap::unowned(3);
        assert_eq!(map.partitions.len(), 3);
        assert_eq!(map.owner(PartitionId::new(2)), None);
        assert_eq!(map.epoch(PartitionId::new(2)), Some(PartitionEpoch(0)));
        assert_eq!(map.epoch(PartitionId::new(3)), None);
        let node = NodeId::new("a").unwrap();
        assert!(map.owned_by(&node).is_empty());
        assert_eq!(PartitionEpoch(4).next(), PartitionEpoch(5));
    }
}
