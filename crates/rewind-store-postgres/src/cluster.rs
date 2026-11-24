//! `Cluster` on Postgres: the nodes alive, the partition map and its generations, a node's liveness,
//! and the changes of the control plane (`archi/07-code-architecture.md`, "`Cluster` and
//! `NodeLink`"; `archi/06-storage-and-scale.md`, "Partitions, owners and nodes"). The in-memory
//! `MemoryCluster` of `rewind-engine` is the reference of every behavior here; both pass the
//! same conformance suite.
//!
//! ```text
//!  node a ── lock conn: holds lock(a) ───────────────┐
//!         └─ watch conn: waits lock_shared(b) ──┐    │
//!  node b ── lock conn: holds lock(b) ◄─────────┘    │   the ring, by id: a → b → c → a
//!         └─ watch conn: waits lock_shared(c)        │
//!  node c ── lock conn: holds lock(c)                │
//!         └─ watch conn: waits lock_shared(a) ◄──────┘
//! ```
//!
//! - **Liveness.** [`Cluster::join`] opens a connection of its own, outside the pool, to the
//!   lock database ([`PostgresCluster::open_in`], `postgres` by default), and takes on it the
//!   session advisory lock of the node, [`node_lock`]: the first 8 bytes, big-endian, of
//!   SHA-256(`rewind node/<control plane's database>/<id>`), as a `bigint`, so the deployments
//!   sharing a server and its lock database never take each other's locks. A second member of
//!   the same id cannot take it ([`ClusterError::AlreadyMember`]). The node's row in `nodes` is
//!   written once the lock is held, so a row whose lock is free is a node that died. A task
//!   keeps the connection, and waits for it to end: once it does, or once it fails, the
//!   membership has ended ([`ClusterError::NotMember`]), since the lock went with it, and the
//!   node gives its partitions up. Dropping the membership, or [`Membership::leave`], releases
//!   the lock and closes the connection; the row stays for the node's watcher, as a dead node's
//!   does.
//! - **The ring.** The rows of `nodes`, by id, form the ring, the dead ones too: each member
//!   [`watch`](Membership::watch)es the row after its own, the last the first. `watch` waits on
//!   another connection of its own, to the lock database, for the shared form of that node's
//!   lock; once it is granted, that node's session ended: the watcher deletes its row while it
//!   holds the lock (no new member of that id can take the lock meanwhile), notifies the
//!   channel `nodes`, releases the lock, and names the node. A node whose lock is already free
//!   is named at once, so a node that died while nobody watched it is named by the next watch of
//!   its predecessor. A member alone listens on the channel `nodes`, which `join` notifies, until
//!   another row appears. A watch dropped while it waits terminates its backend
//!   (`pg_terminate_backend`), which waits on a lock and would not notice its client left.
//! - **Members.** [`Membership::nodes`] lists the rows whose node holds its lock in the lock
//!   database, from `pg_locks`: the members alive. [`Membership::assign`] moves partitions only
//!   to them. A join and a departure seen send `ControlPlaneChange::Nodes`.
//! - **Moves.** `assign` locks every row of `partitions` (`FOR UPDATE`), so moves are decided
//!   one after the other; it raises the generation of each partition whose owner changes and sends
//!   the whole map on the channel `partition_map` in the same transaction (`crate::changes`).
//! - **Changes.** [`Membership::changes`] listens (`LISTEN control_plane, partition_map, nodes`)
//!   on a connection of its own, to the control plane's database, before it returns. The stream ends when that connection is lost, or
//!   when the membership ends: notifications sent meanwhile are lost, so its consumer drops what
//!   it cached and listens again.
//!
//! The dedicated connections (the lock, a watch, a listener) set the server's TCP keepalive:
//! `tcp_keepalives_idle = 5`, `tcp_keepalives_interval = 1`, `tcp_keepalives_count = 5` and
//! `tcp_user_timeout = 10000` ms. A machine that vanishes without closing its connections has
//! its sessions ended by the server about 10 s later, which frees its node's lock; a process that
//! dies, or a connection closed, frees it at once.
//!
//! A watch waits inside a statement, and Postgres keeps the snapshot of a running statement:
//! while a watch waits, its backend holds back the horizon below which `VACUUM` removes dead
//! rows of every table of its database (`backend_xmin` in `pg_stat_activity`). The locks are
//! taken in a database where nothing is stored, so a watch that waits for days holds back no
//! table of the control plane.

use std::collections::BTreeSet;
use std::num::NonZeroU16;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;
use futures_util::stream::BoxStream;
use rewind_core::{Epoch, PartitionId};
use rewind_engine::ports::cluster::Membership;
use rewind_engine::ports::{
    Cluster, ClusterError, ControlPlaneChange, Departed, Moves, NodeId, NodeInfo,
    PartitionGeneration, PartitionMap, PartitionOwner,
};
use sqlx::postgres::{PgConnectOptions, PgListener, PgPoolOptions, PgRow};
use sqlx::{ConnectOptions as _, Connection as _, PgConnection, PgPool};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::changes::{CONTROL_PLANE, Decoder, NODES, PARTITION_MAP, notify};
use crate::sql::Columns;
use crate::{Error, PostgresStore};

/// The server's TCP keepalive on the dedicated connections: about 10 s to end the session of a
/// machine that vanished.
const KEEPALIVE: [(&str, &str); 4] = [
    ("tcp_keepalives_idle", "5"),
    ("tcp_keepalives_interval", "1"),
    ("tcp_keepalives_count", "5"),
    ("tcp_user_timeout", "10000"),
];

/// The advisory lock of the node whose id is the SQL expression `id`, as a `bigint`: the first 8
/// bytes, big-endian, of SHA-256(`<prefix><id>`), the SQL expression `prefix` being
/// `rewind node/<control plane's database>/` ([`PostgresCluster`]'s `prefix`).
fn node_lock(prefix: &str, id: &str) -> String {
    format!(
        "('x' || left(encode(sha256(convert_to({prefix} || {id}, 'UTF8')), 'hex'), 16))\
         ::bit(64)::bigint"
    )
}

/// Whether the node whose id is the SQL expression `id` holds its lock: a session of the lock
/// database, named by the SQL expression `database`, holds the exclusive advisory lock of its
/// key (`pg_locks`, which lists the locks of every database of the server, shows a `bigint` key
/// as its high half in `classid` and its low half in `objid`, with `objsubid` 1).
fn holds_lock(prefix: &str, database: &str, id: &str) -> String {
    let key = node_lock(prefix, id);
    format!(
        "EXISTS (SELECT 1 FROM pg_locks l WHERE l.locktype = 'advisory' AND l.granted \
         AND l.mode = 'ExclusiveLock' AND l.objsubid = 1 \
         AND l.database = (SELECT oid FROM pg_database WHERE datname = {database}) \
         AND l.classid = (({key} >> 32) & 4294967295)::oid \
         AND l.objid = ({key} & 4294967295)::oid)"
    )
}

fn failed(err: impl Into<Error>) -> ClusterError {
    match err.into() {
        err @ Error::Unreachable(_) => ClusterError::Unavailable(err.to_string()),
        err => ClusterError::Backend(err.to_string()),
    }
}

/// `Cluster` on the database of a [`PostgresStore`] ([`PostgresCluster::open`]), its liveness
/// locks in a database of their own on the same server.
#[derive(Clone)]
pub struct PostgresCluster {
    pool: PgPool,
    /// What the dedicated connections to the control plane's database are opened with.
    options: PgConnectOptions,
    /// What the connections that take and wait for locks are opened with: the lock database.
    locks: PgConnectOptions,
    /// The lock database's name.
    lock_database: String,
    /// `rewind node/<control plane's database>/`: what a node's lock key hashes before its id.
    prefix: String,
}

impl std::fmt::Debug for PostgresCluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresCluster").finish_non_exhaustive()
    }
}

impl PostgresCluster {
    /// The database the liveness locks are taken in when the deployment names none: every
    /// server has it.
    pub const DEFAULT_LOCK_DATABASE: &str = "postgres";

    /// [`open_in`](Self::open_in), the locks in [`DEFAULT_LOCK_DATABASE`](Self::DEFAULT_LOCK_DATABASE).
    pub async fn open(store: &PostgresStore, partitions: NonZeroU16) -> Result<Self, ClusterError> {
        Self::open_in(store, partitions, Self::DEFAULT_LOCK_DATABASE).await
    }

    /// The cluster of a deployment of `partitions` partitions, on the store's database: the rows
    /// of the partitions it lacks are created, unowned at generation 0 (the column `epoch`). A database with more
    /// partitions is refused: a partition, once created, holds runs. The liveness locks are
    /// taken in `lock_database` (`--cluster-database`), a database of the same server reached
    /// with the same credentials, where nothing is stored (`archi/06-storage-and-scale.md`,
    /// "Nodes").
    pub async fn open_in(
        store: &PostgresStore,
        partitions: NonZeroU16,
        lock_database: &str,
    ) -> Result<Self, ClusterError> {
        let pool = store.pool.clone();
        let options = (*pool.connect_options()).clone().options(KEEPALIVE);
        let locks = options.clone().database(lock_database);
        let database: String = sqlx::query_scalar("SELECT current_database()::text")
            .fetch_one(&pool)
            .await
            .map_err(failed)?;
        let mut tx = pool.begin().await.map_err(failed)?;
        sqlx::query(
            "INSERT INTO partitions (id) SELECT generate_series(0, $1 - 1) ON CONFLICT DO NOTHING",
        )
        .bind(i32::from(partitions.get()))
        .execute(&mut *tx)
        .await
        .map_err(failed)?;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM partitions")
            .fetch_one(&mut *tx)
            .await
            .map_err(failed)?;
        if count > i64::from(partitions.get()) {
            return Err(ClusterError::Backend(format!(
                "the database has {count} partitions, more than the {partitions} configured: \
                 a partition is never removed"
            )));
        }
        tx.commit().await.map_err(failed)?;
        Ok(Self {
            pool,
            options,
            locks,
            lock_database: lock_database.to_owned(),
            prefix: format!("rewind node/{database}/"),
        })
    }

    /// The options of a dedicated connection to the control plane's database, named for
    /// `pg_stat_activity`.
    fn dedicated(&self, purpose: &str) -> PgConnectOptions {
        self.options
            .clone()
            .application_name(&format!("rewind {purpose}"))
    }

    /// The options of a dedicated connection to the lock database.
    fn locking(&self, purpose: &str) -> PgConnectOptions {
        self.locks
            .clone()
            .application_name(&format!("rewind {purpose}"))
    }

    /// A listener on a connection of its own to the control plane's database.
    async fn listener(&self, purpose: &str, channels: &[&str]) -> Result<PgListener, ClusterError> {
        listener_on(self.dedicated(purpose), channels).await
    }
}

/// A listener on a connection of its own, which it never opens again once lost: its
/// one-connection pool closes the connection instead of keeping it.
async fn listener_on(
    options: PgConnectOptions,
    channels: &[&str],
) -> Result<PgListener, ClusterError> {
    {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .idle_timeout(None)
            .max_lifetime(None)
            .test_before_acquire(false)
            .after_release(|_, _| Box::pin(async { Ok(false) }))
            .connect_lazy_with(options);
        let mut listener = PgListener::connect_with(&pool).await.map_err(failed)?;
        listener.eager_reconnect(false);
        listener.ignore_pool_close_event(true);
        if !channels.is_empty() {
            listener
                .listen_all(channels.iter().copied())
                .await
                .map_err(failed)?;
        }
        Ok(listener)
    }
}

fn info_from_row(row: &PgRow) -> Result<NodeInfo, Error> {
    let id = row.text(0)?;
    let epochs: Vec<i64> = sqlx::Row::try_get(row, 3)?;
    Ok(NodeInfo {
        id: NodeId::new(id).map_err(|err| Error::Corrupt(err.to_string()))?,
        address: row.text(1)?,
        build: row.text(2)?,
        epochs: epochs
            .into_iter()
            .map(|epoch| {
                u32::try_from(epoch)
                    .map(Epoch)
                    .map_err(|_| Error::Corrupt(format!("epoch {epoch}")))
            })
            .collect::<Result<BTreeSet<_>, _>>()?,
        pool: row.opt_text(4)?,
        executors_only: row.boolean(5)?,
    })
}

/// The members alive, by id: the rows whose node holds its lock in the lock database.
async fn members(
    conn: &mut PgConnection,
    cluster: &PostgresCluster,
) -> Result<Vec<NodeInfo>, Error> {
    let rows = sqlx::query(&format!(
        "SELECT n.id, n.address, n.build, n.epochs, n.pool, n.executors_only FROM nodes n \
         WHERE {} ORDER BY n.id",
        holds_lock("$1", "$2", "n.id")
    ))
    .bind(cluster.prefix.as_str())
    .bind(cluster.lock_database.as_str())
    .fetch_all(&mut *conn)
    .await?;
    rows.iter().map(info_from_row).collect()
}

async fn read_map(conn: &mut PgConnection, lock: bool) -> Result<PartitionMap, Error> {
    let rows = sqlx::query(&format!(
        "SELECT id::bigint, node, epoch FROM partitions ORDER BY id{}",
        if lock { " FOR UPDATE" } else { "" }
    ))
    .fetch_all(&mut *conn)
    .await?;
    let mut map = PartitionMap::default();
    for row in &rows {
        let id = row.i64(0)?;
        let id = u16::try_from(id).map_err(|_| Error::Corrupt(format!("partition {id}")))?;
        let node = row
            .opt_text(1)?
            .map(|node| NodeId::new(node).map_err(|err| Error::Corrupt(err.to_string())))
            .transpose()?;
        map.partitions.insert(
            PartitionId::new(id),
            PartitionOwner {
                node,
                generation: PartitionGeneration(row.u64(2)?),
            },
        );
    }
    Ok(map)
}

impl Cluster for PostgresCluster {
    type Membership = PostgresMembership;

    async fn join(&self, node: NodeInfo) -> Result<PostgresMembership, ClusterError> {
        let mut lock = listener_on(self.locking(&format!("node {}", node.id)), &[]).await?;
        let taken: bool = sqlx::query_scalar(&format!(
            "SELECT pg_try_advisory_lock({})",
            node_lock("$2", "$1")
        ))
        .bind(node.id.as_str())
        .bind(self.prefix.as_str())
        .fetch_one(&mut lock)
        .await
        .map_err(failed)?;
        if !taken {
            return Err(ClusterError::AlreadyMember(node.id));
        }
        // The lock is held: the row may be written. Dropping `lock` on an error frees it.
        let epochs: Vec<i64> = node.epochs.iter().map(|epoch| i64::from(epoch.0)).collect();
        let mut tx = self.pool.begin().await.map_err(failed)?;
        sqlx::query(
            "INSERT INTO nodes (id, address, build, epochs, pool, executors_only, joined_at) \
             VALUES ($1, $2, $3, $4, $5, $6, (extract(epoch FROM clock_timestamp()) * 1000)::bigint) \
             ON CONFLICT (id) DO UPDATE SET address = EXCLUDED.address, build = EXCLUDED.build, \
                 epochs = EXCLUDED.epochs, pool = EXCLUDED.pool, \
                 executors_only = EXCLUDED.executors_only, joined_at = EXCLUDED.joined_at",
        )
        .bind(node.id.as_str())
        .bind(node.address.as_str())
        .bind(node.build.as_str())
        .bind(epochs)
        .bind(node.pool.as_deref())
        .bind(node.executors_only)
        .execute(&mut *tx)
        .await
        .map_err(failed)?;
        notify(&mut tx, &[ControlPlaneChange::Nodes])
            .await
            .map_err(failed)?;
        tx.commit().await.map_err(failed)?;

        let (alive_tx, alive) = watch::channel(true);
        let (stop, stopped) = oneshot::channel();
        let holder = tokio::spawn(hold(
            lock,
            node.id.clone(),
            self.prefix.clone(),
            stopped,
            alive_tx,
        ));
        Ok(PostgresMembership {
            node: node.id,
            cluster: self.clone(),
            alive,
            stop: Some(stop),
            holder: Some(holder),
        })
    }
}

/// Keeps the connection that holds a node's lock until the membership ends (`stop` is sent or
/// dropped) or the connection is lost or fails, then says the membership is over: the lock went
/// with the connection, and the node is no member any more.
async fn hold(
    mut lock: PgListener,
    node: NodeId,
    prefix: String,
    stop: oneshot::Receiver<()>,
    alive: watch::Sender<bool>,
) {
    // With no channel listened to, the connection receives nothing until it ends.
    let lost = async {
        loop {
            match lock.try_recv().await {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return,
            }
        }
    };
    let stopped = tokio::select! {
        _ = stop => true,
        () = lost => false,
    };
    if stopped {
        let unlocked = sqlx::query(&format!(
            "SELECT pg_advisory_unlock({})",
            node_lock("$2", "$1")
        ))
        .bind(node.as_str())
        .bind(prefix.as_str())
        .execute(&mut lock)
        .await;
        if let Err(error) = unlocked {
            tracing::warn!(%error, %node, "releasing the node's lock failed: its connection closes");
        }
    } else {
        tracing::warn!(%node, "the connection holding the node's lock was lost");
    }
    let _ = alive.send(false);
}

/// A member of a [`PostgresCluster`]; dropping it ends the membership: its lock is released and
/// its connection closed.
pub struct PostgresMembership {
    node: NodeId,
    cluster: PostgresCluster,
    /// `false` once the membership ended.
    alive: watch::Receiver<bool>,
    /// Sent, or dropped with the membership, to release the lock.
    stop: Option<oneshot::Sender<()>>,
    holder: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PostgresMembership {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresMembership")
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

impl PostgresMembership {
    fn check(&self) -> Result<(), ClusterError> {
        if *self.alive.borrow() {
            Ok(())
        } else {
            Err(self.not_member())
        }
    }

    fn not_member(&self) -> ClusterError {
        ClusterError::NotMember(self.node.clone())
    }

    /// The row after this node's in the ring, dead or alive; `None` when it is alone.
    async fn successor(&self) -> Result<Option<NodeId>, ClusterError> {
        let next: Option<String> = sqlx::query_scalar(
            "SELECT id FROM nodes WHERE id <> $1 \
             ORDER BY id > $1 DESC, id LIMIT 1",
        )
        .bind(self.node.as_str())
        .fetch_optional(&self.cluster.pool)
        .await
        .map_err(failed)?;
        next.map(|id| NodeId::new(id).map_err(|err| ClusterError::Backend(err.to_string())))
            .transpose()
    }

    /// Waits until another node has a row, and returns the one after this node's.
    async fn another(&self) -> Result<NodeId, ClusterError> {
        let mut alive = self.alive.clone();
        loop {
            // Listening first, a node that joins after the read below is heard.
            let mut joins = self.cluster.listener("watch", &[NODES]).await?;
            if let Some(next) = self.successor().await? {
                return Ok(next);
            }
            tokio::select! {
                heard = joins.try_recv() => {
                    heard.map_err(failed)?;
                }
                _ = alive.wait_for(|alive| !*alive) => return Err(self.not_member()),
            }
        }
    }
}

/// Terminates the backend of a watch dropped while it waited for its lock: a backend waiting on a
/// lock does not read its connection, and would not notice it closed.
struct TerminateOnDrop {
    pool: PgPool,
    pid: Option<i32>,
}

impl Drop for TerminateOnDrop {
    fn drop(&mut self) {
        let (Some(pid), Ok(runtime)) = (self.pid, tokio::runtime::Handle::try_current()) else {
            return;
        };
        let pool = self.pool.clone();
        runtime.spawn(async move {
            let terminated = sqlx::query(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE pid = $1 AND application_name = 'rewind watch'",
            )
            .bind(pid)
            .execute(&pool)
            .await;
            if let Err(error) = terminated {
                tracing::debug!(%error, pid, "terminating an abandoned watch failed");
            }
        });
    }
}

impl Drop for PostgresMembership {
    fn drop(&mut self) {
        // Dropping `stop` tells the holder to release the lock; the holder runs on.
        self.stop.take();
        self.holder.take();
    }
}

impl Membership for PostgresMembership {
    type Changes = PostgresChanges;

    fn node(&self) -> &NodeId {
        &self.node
    }

    async fn watch(&self) -> Result<Departed, ClusterError> {
        self.check()?;
        let next = match self.successor().await? {
            Some(next) => next,
            None => self.another().await?,
        };
        let mut conn = self
            .cluster
            .locking("watch")
            .connect()
            .await
            .map_err(failed)?;
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut conn)
            .await
            .map_err(failed)?;
        let mut abandoned = TerminateOnDrop {
            pool: self.cluster.pool.clone(),
            pid: Some(pid),
        };
        let mut alive = self.alive.clone();
        let wait_sql = format!("SELECT pg_advisory_lock_shared({})", node_lock("$2", "$1"));
        let wait = sqlx::query(&wait_sql)
            .bind(next.as_str())
            .bind(self.cluster.prefix.as_str())
            .execute(&mut conn);
        tokio::select! {
            granted = wait => {
                granted.map_err(failed)?;
            }
            _ = alive.wait_for(|alive| !*alive) => return Err(self.not_member()),
        }
        abandoned.pid = None;
        // Its session ended. While this lock is held, no new member of its id takes the lock:
        // the row deleted, in the control plane's database, is the dead node's.
        let mut tx = self.cluster.pool.begin().await.map_err(failed)?;
        sqlx::query("DELETE FROM nodes WHERE id = $1")
            .bind(next.as_str())
            .execute(&mut *tx)
            .await
            .map_err(failed)?;
        notify(&mut tx, &[ControlPlaneChange::Nodes])
            .await
            .map_err(failed)?;
        tx.commit().await.map_err(failed)?;
        sqlx::query(&format!(
            "SELECT pg_advisory_unlock_shared({})",
            node_lock("$2", "$1")
        ))
        .bind(next.as_str())
        .bind(self.cluster.prefix.as_str())
        .execute(&mut conn)
        .await
        .map_err(failed)?;
        let _ = conn.close().await;
        Ok(Departed { node: next })
    }

    async fn assign(&self, moves: &Moves) -> Result<PartitionMap, ClusterError> {
        self.check()?;
        let mut tx = self.cluster.pool.begin().await.map_err(failed)?;
        let mut map = read_map(&mut tx, true).await.map_err(failed)?;
        let live: BTreeSet<NodeId> = members(&mut tx, &self.cluster)
            .await
            .map_err(failed)?
            .into_iter()
            .map(|info| info.id)
            .collect();
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
            let generation: i64 = sqlx::query_scalar(
                "UPDATE partitions SET node = $2, epoch = epoch + 1 WHERE id = $1 RETURNING epoch",
            )
            .bind(i32::from(partition.get()))
            .bind(node.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(failed)?;
            owner.node = Some(node.clone());
            owner.generation = PartitionGeneration(u64::try_from(generation).unwrap_or_default());
        }
        notify(&mut tx, &[ControlPlaneChange::PartitionMap(map.clone())])
            .await
            .map_err(failed)?;
        tx.commit().await.map_err(failed)?;
        Ok(map)
    }

    async fn partition_map(&self) -> Result<PartitionMap, ClusterError> {
        self.check()?;
        let mut conn = self.cluster.pool.acquire().await.map_err(failed)?;
        read_map(&mut conn, false).await.map_err(failed)
    }

    async fn nodes(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        self.check()?;
        let mut conn = self.cluster.pool.acquire().await.map_err(failed)?;
        members(&mut conn, &self.cluster).await.map_err(failed)
    }

    async fn changes(&self) -> Result<PostgresChanges, ClusterError> {
        self.check()?;
        let listener = self
            .cluster
            .listener("changes", &[CONTROL_PLANE, PARTITION_MAP, NODES])
            .await?;
        Ok(PostgresChanges::new(listener, self.alive.clone()))
    }

    async fn leave(mut self) -> Result<(), ClusterError> {
        self.check()?;
        // The lock goes as a dead node's does; its watcher deletes the row.
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(holder) = self.holder.take() {
            holder
                .await
                .map_err(|err| ClusterError::Backend(format!("the lock's holder failed: {err}")))?;
        }
        Ok(())
    }
}

/// The changes of the control plane a [`PostgresMembership`] listens to, in commit order. It
/// ends when its connection is lost or the membership ends.
pub struct PostgresChanges {
    inner: BoxStream<'static, ControlPlaneChange>,
}

impl std::fmt::Debug for PostgresChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresChanges").finish_non_exhaustive()
    }
}

struct Listening {
    listener: PgListener,
    decoder: Decoder,
    alive: watch::Receiver<bool>,
}

impl PostgresChanges {
    fn new(listener: PgListener, alive: watch::Receiver<bool>) -> Self {
        let state = Listening {
            listener,
            decoder: Decoder::new(),
            alive,
        };
        let inner = futures_util::stream::unfold(state, |mut state| async move {
            let change = next_change(&mut state).await?;
            Some((change, state))
        });
        Self {
            inner: Box::pin(inner),
        }
    }
}

/// The next change, or `None` once the stream must end.
async fn next_change(state: &mut Listening) -> Option<ControlPlaneChange> {
    let Listening {
        listener,
        decoder,
        alive,
    } = state;
    loop {
        let notification = tokio::select! {
            received = listener.try_recv() => match received {
                Ok(Some(notification)) => notification,
                Ok(None) => {
                    tracing::warn!("the connection listening to the control plane was lost");
                    return None;
                }
                Err(error) => {
                    tracing::warn!(%error, "listening to the control plane failed");
                    return None;
                }
            },
            _ = alive.wait_for(|alive| !*alive) => return None,
        };
        match decoder.feed(notification.payload()) {
            Ok(Some(change)) => return Some(change),
            Ok(None) => {}
            Err(error) => {
                // A change this build cannot read: what it changed is unknown.
                tracing::warn!(%error, channel = notification.channel(), "ending the changes");
                return None;
            }
        }
    }
}

impl Stream for PostgresChanges {
    type Item = ControlPlaneChange;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_lock_of_a_node_is_derived_in_sql_from_its_database_and_its_id() {
        assert_eq!(
            super::node_lock("$2", "$1"),
            "('x' || left(encode(sha256(convert_to($2 || $1, 'UTF8')), 'hex'), 16))\
             ::bit(64)::bigint"
        );
    }
}
