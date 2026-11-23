//! The embedded control plane: the conformance suites of `CatalogStore` and `Cluster`, each check
//! on a new database file; the changes that access and secrets send; namespaces whose rows have
//! no partition; the partition map kept across a restart.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt as _;
use rewind_core::{Digest, Epoch, PartitionId, SecretName, Timestamp};
use rewind_engine::adapters::ChangeStream;
use rewind_engine::conformance::{self, T0, tenant};
use rewind_engine::ports::cluster::Membership;
use rewind_engine::ports::{
    AccessStore, ApiKey, CatalogStore, Cluster, ControlPlaneChange, Grant, Moves, NamespaceName,
    NamespaceWrite, NewSecret, NodeId, NodeInfo, PartitionEpoch, Principal, Secret, SecretId,
    SecretStore, Token, namespace_partition,
};
use rewind_store_turso::{MasterKey, TursoCluster, TursoStore};
use tempfile::TempDir;

fn partitions(count: u16) -> NonZeroU16 {
    NonZeroU16::new(count).expect("at least one partition")
}

fn block_on<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(work))
}

async fn open(path: &Path) -> TursoStore {
    TursoStore::open(path, MasterKey::from_bytes([1; 32]))
        .await
        .expect("open the store")
}

/// A new database file in `dir` for each call.
fn paths(dir: &TempDir) -> impl Fn() -> PathBuf + '_ {
    let next = AtomicUsize::new(0);
    move || {
        dir.path().join(format!(
            "control-{}.db",
            next.fetch_add(1, Ordering::SeqCst)
        ))
    }
}

fn info(id: &str) -> NodeInfo {
    NodeInfo {
        id: NodeId::new(id).expect("node id"),
        address: "local".into(),
        build: "build-1".into(),
        epochs: BTreeSet::from([Epoch(1)]),
        pool: None,
        executors_only: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_catalog_conforms() {
    let dir = TempDir::new().expect("temporary directory");
    let path = paths(&dir);
    conformance::catalog::run_all(|| {
        block_on(async {
            let store = open(&path()).await;
            let cluster = TursoCluster::open(&store, partitions(1))
                .await
                .expect("cluster");
            (store.catalog(), cluster)
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cluster_conforms() {
    let dir = TempDir::new().expect("temporary directory");
    let path = paths(&dir);
    conformance::cluster::run_all(|| {
        block_on(async {
            let store = open(&path()).await;
            TursoCluster::open(&store, partitions(conformance::cluster::PARTITIONS))
                .await
                .expect("cluster")
        })
    })
    .await;
}

async fn next(changes: &mut ChangeStream) -> ControlPlaneChange {
    changes.next().await.expect("a change")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_and_secrets_send_their_changes() {
    let dir = TempDir::new().expect("temporary directory");
    let store = open(&dir.path().join("control.db")).await;
    let member = TursoCluster::open(&store, partitions(1))
        .await
        .expect("cluster")
        .join(info("embedded"))
        .await
        .expect("join");
    let mut changes = member.changes().await.expect("changes");
    let acme = tenant("acme");
    let alice = NamespaceName::new("alice").expect("namespace");

    let key = ApiKey {
        hash: Digest::of(b"key"),
        tenant: acme.clone(),
        name: "ci".into(),
        created_by: None,
        created_at: T0,
        revoked_at: None,
    };
    store.put_key(&key).await.expect("put_key");
    let key_changed = ControlPlaneChange::Key {
        tenant: acme.clone(),
        hash: key.hash,
    };
    assert_eq!(next(&mut changes).await, key_changed);
    assert!(store.revoke_key(key.hash, T0).await.expect("revoke_key"));
    assert_eq!(next(&mut changes).await, key_changed);
    assert!(!store.revoke_key(key.hash, T0).await.expect("revoke_key"));

    let token = Token {
        hash: Digest::of(b"token"),
        tenant: acme.clone(),
        namespace: alice.clone(),
        permissions: BTreeSet::new(),
        labels: BTreeMap::new(),
        expires_at: Timestamp(T0.0 + 1_000),
        created_by: Principal::Key(key.hash),
        created_at: T0,
        revoked_at: None,
    };
    let token_changed = ControlPlaneChange::Token {
        tenant: acme.clone(),
        hash: token.hash,
    };
    store.put_token(&token).await.expect("put_token");
    assert_eq!(next(&mut changes).await, token_changed);
    assert_eq!(store.revoke_tokens(&acme, &alice, T0).await, Ok(1));
    assert_eq!(next(&mut changes).await, token_changed);
    assert_eq!(store.purge_tokens(Timestamp(T0.0 + 1), 10).await, Ok(1));
    assert_eq!(next(&mut changes).await, token_changed, "a purged token");

    let user = Principal::User("user_1".into());
    store
        .put_grant(
            &Grant {
                tenant: acme.clone(),
                principal: user.clone(),
                role: "owner".into(),
                allowed_labels: BTreeMap::new(),
                granted_by: None,
                granted_at: T0,
            },
            None,
        )
        .await
        .expect("put_grant");
    assert_eq!(
        next(&mut changes).await,
        ControlPlaneChange::Grant {
            tenant: acme.clone(),
            principal: user
        }
    );
    store
        .set_default_role(&acme, "reader")
        .await
        .expect("set_default_role");
    assert_eq!(
        next(&mut changes).await,
        ControlPlaneChange::Tenant {
            tenant: acme.clone()
        }
    );

    let github = SecretId::of_namespace(alice.clone(), SecretName::new("github").expect("name"));
    SecretStore::put(
        &store,
        &acme,
        &NewSecret {
            id: github.clone(),
            value: Secret::new("ghp_value"),
            labels: BTreeMap::new(),
            by: "user:user_1".into(),
        },
        T0,
    )
    .await
    .expect("put a secret");
    let secret_changed = ControlPlaneChange::Secret {
        tenant: acme.clone(),
        secret: github,
    };
    assert_eq!(next(&mut changes).await, secret_changed);
    assert_eq!(
        SecretStore::delete_namespace(&store, &acme, &alice).await,
        Ok(1)
    );
    assert_eq!(next(&mut changes).await, secret_changed);

    member.leave().await.expect("leave");
    assert_eq!(
        changes.next().await,
        None,
        "a stream ends with its membership"
    );
}

/// Rows created before `namespaces.partition` have none: `ensure_namespace` places the one it
/// is asked for, `place_namespaces` every other one, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespaces_without_a_partition_are_placed_in_theirs() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("control.db");
    drop(open(&path).await);
    {
        let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
            .build()
            .await
            .expect("open the file");
        let connection = database.connect().expect("connect");
        for name in ["alice", "bob"] {
            connection
                .execute(
                    &format!(
                        "INSERT INTO namespaces (tenant, name, created_at, last_active_at) \
                         VALUES ('acme', '{name}', {at}, {at})",
                        at = T0.0
                    ),
                    (),
                )
                .await
                .expect("a namespace without a partition");
        }
    }
    let store = open(&path).await;
    let catalog = store.catalog();
    let acme = tenant("acme");
    let (alice, bob) = (
        NamespaceName::new("alice").expect("namespace"),
        NamespaceName::new("bob").expect("namespace"),
    );
    assert!(catalog.get_namespace(&acme, &alice).await.is_err());
    let placed = catalog
        .ensure_namespace(&acme, &alice, partitions(64), T0)
        .await
        .expect("ensure_namespace");
    let NamespaceWrite::Written(placed) = placed else {
        panic!("alice is not being erased: {placed:?}");
    };
    assert_eq!(
        placed.partition,
        namespace_partition(&acme, &alice, partitions(64))
    );
    assert_eq!(catalog.place_namespaces(partitions(64)).await, Ok(1));
    assert_eq!(
        catalog
            .get_namespace(&acme, &bob)
            .await
            .expect("get_namespace")
            .expect("bob")
            .partition,
        namespace_partition(&acme, &bob, partitions(64))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_partition_map_outlives_the_process() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("control.db");
    {
        let store = open(&path).await;
        let member = TursoCluster::open(&store, partitions(1))
            .await
            .expect("cluster")
            .join(info("embedded"))
            .await
            .expect("join");
        let map = member
            .assign(&Moves::new().to(PartitionId::new(0), NodeId::new("embedded").expect("id")))
            .await
            .expect("assign");
        assert_eq!(map.epoch(PartitionId::new(0)), Some(PartitionEpoch(1)));
    }
    let store = open(&path).await;
    let cluster = TursoCluster::open(&store, partitions(1))
        .await
        .expect("cluster");
    let member = cluster.join(info("restarted")).await.expect("join");
    let map = member.partition_map().await.expect("partition_map");
    assert_eq!(
        (
            map.owner(PartitionId::new(0)).map(ToString::to_string),
            map.epoch(PartitionId::new(0))
        ),
        (Some("embedded".to_owned()), Some(PartitionEpoch(1))),
        "the map as it was, its owner gone"
    );
    let moved = member
        .assign(&Moves::new().to(PartitionId::new(0), NodeId::new("restarted").expect("id")))
        .await
        .expect("assign");
    assert_eq!(moved.epoch(PartitionId::new(0)), Some(PartitionEpoch(2)));
    assert!(TursoCluster::open(&store, partitions(2)).await.is_ok());
    assert!(
        TursoCluster::open(&store, partitions(1)).await.is_err(),
        "a partition is never removed"
    );
}
