//! `Cluster` on Turso: the embedded server's cluster (`archi/07-code-architecture.md`,
//! "`Cluster` and `NodeLink`", "Embedded server and CLI").
//!
//! Only one process opens the database, so the members of the cluster live in that process: the
//! embedded server is its one member, whose liveness is the process itself (it holds
//! `.rewind/server.lock`). Members, their ring and their watches are those of `MemoryCluster`,
//! whose change streams are the store's broadcast ([`TursoStore::changes`]): every member
//! receives the changes of the catalog, of access and of secrets. The partition map is kept in
//! the database (`partitions`), so the epochs of its moves outlive the process; a move is one
//! transaction, and sends the map once it committed. The in-memory `MemoryCluster` of
//! `rewind-engine` is the reference of every behavior here; both pass the same conformance suite.

use std::collections::BTreeSet;
use std::num::NonZeroU16;

use rewind_core::PartitionId;
use rewind_engine::adapters::{ChangeStream, MemoryCluster, MemoryMembership};
use rewind_engine::ports::cluster::Membership;
use rewind_engine::ports::{
    Cluster, ClusterError, ControlPlaneChange, Departed, Moves, NodeId, NodeInfo, PartitionEpoch,
    PartitionMap, PartitionOwner,
};
use turso::Connection;

use crate::sql::{self, Columns, int, text, transaction};
use crate::{Error, TursoStore};

impl From<Error> for ClusterError {
    fn from(err: Error) -> Self {
        ClusterError::Backend(err.to_string())
    }
}

/// `Cluster` on the database of a [`TursoStore`] ([`TursoCluster::open`]): members in the
/// process, the partition map in the database.
#[derive(Clone, Debug)]
pub struct TursoCluster {
    store: TursoStore,
    members: MemoryCluster,
}

impl TursoCluster {
    /// The cluster of `partitions` partitions on the store's database: the rows of the
    /// partitions it lacks are created, unowned at epoch 0. A database with more partitions is
    /// refused: a partition, once created, holds runs.
    pub async fn open(store: &TursoStore, partitions: NonZeroU16) -> Result<Self, ClusterError> {
        let connection = store.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            for id in 0..partitions.get() {
                sql::execute(
                    connection,
                    "INSERT OR IGNORE INTO partitions (id, epoch) VALUES (?, 0)",
                    vec![int(u64::from(id))],
                )
                .await?;
            }
            let count = sql::query_row(connection, "SELECT COUNT(*) FROM partitions", Vec::new())
                .await?
                .map_or(Ok(0), |row| row.u64(0))?;
            if count > u64::from(partitions.get()) {
                return Err(ClusterError::Backend(format!(
                    "the database has {count} partitions, more than the {partitions} configured: \
                     a partition is never removed"
                )));
            }
            Ok(())
        })
        .await?;
        Ok(Self {
            store: store.clone(),
            members: MemoryCluster::with_changes(partitions.get(), store.changes().clone()),
        })
    }
}

async fn read_map(connection: &Connection) -> Result<PartitionMap, Error> {
    let rows = sql::query(
        connection,
        "SELECT id, node, epoch FROM partitions ORDER BY id",
        Vec::new(),
    )
    .await?;
    let mut map = PartitionMap::default();
    for row in &rows {
        let id = row.u64(0)?;
        let id = u16::try_from(id).map_err(|_| Error::Corrupt(format!("partition {id}")))?;
        let node = row
            .opt_text(1)?
            .map(|node| NodeId::new(node).map_err(|err| Error::Corrupt(err.to_string())))
            .transpose()?;
        map.partitions.insert(
            PartitionId::new(id),
            PartitionOwner {
                node,
                epoch: PartitionEpoch(row.u64(2)?),
            },
        );
    }
    Ok(map)
}

impl Cluster for TursoCluster {
    type Membership = TursoMembership;

    async fn join(&self, node: NodeInfo) -> Result<TursoMembership, ClusterError> {
        Ok(TursoMembership {
            inner: self.members.join(node).await?,
            store: self.store.clone(),
        })
    }
}

/// A member of a [`TursoCluster`]; dropping it ends the membership.
#[derive(Debug)]
pub struct TursoMembership {
    inner: MemoryMembership,
    store: TursoStore,
}

impl Membership for TursoMembership {
    type Changes = ChangeStream;

    fn node(&self) -> &NodeId {
        self.inner.node()
    }

    fn watch(&self) -> impl Future<Output = Result<Departed, ClusterError>> + Send {
        self.inner.watch()
    }

    async fn assign(&self, moves: &Moves) -> Result<PartitionMap, ClusterError> {
        let live: BTreeSet<NodeId> = self
            .inner
            .nodes()
            .await?
            .into_iter()
            .map(|info| info.id)
            .collect();
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let map = transaction(connection, async {
            let mut map = read_map(connection).await?;
            for (partition, node) in &moves.moves {
                if !map.partitions.contains_key(partition) {
                    return Err(ClusterError::UnknownPartition(*partition));
                }
                if !live.contains(node) {
                    return Err(ClusterError::UnknownNode(node.clone()));
                }
            }
            for (partition, node) in &moves.moves {
                let Some(owner) = map.partitions.get_mut(partition) else {
                    continue;
                };
                if owner.node.as_ref() == Some(node) {
                    continue;
                }
                owner.node = Some(node.clone());
                owner.epoch = owner.epoch.next();
                sql::execute(
                    connection,
                    "UPDATE partitions SET node = ?, epoch = ? WHERE id = ?",
                    vec![
                        text(node.as_str()),
                        int(owner.epoch.0),
                        int(u64::from(partition.get())),
                    ],
                )
                .await?;
            }
            Ok(map)
        })
        .await?;
        self.store
            .send([ControlPlaneChange::PartitionMap(map.clone())]);
        Ok(map)
    }

    async fn partition_map(&self) -> Result<PartitionMap, ClusterError> {
        // A member that left reads nothing.
        self.inner.partition_map().await?;
        let connection = self.store.connection().await?;
        Ok(read_map(&connection).await?)
    }

    async fn nodes(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        self.inner.nodes().await
    }

    async fn changes(&self) -> Result<ChangeStream, ClusterError> {
        self.inner.changes().await
    }

    async fn leave(self) -> Result<(), ClusterError> {
        self.inner.leave().await
    }
}
