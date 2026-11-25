//! The control plane on Postgres in Docker: the conformance suites of `CatalogStore`,
//! `CatalogMigration` and `Cluster`, each check on a new database; a node's liveness across connections and processes
//! (a lock freed by a killed process wakes its watcher at once); the changes that access and
//! secrets send; payloads above Postgres' limit; namespaces whose rows have no partition.

use crate::support;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead as _, BufReader};
use std::num::NonZeroU16;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use rewind_core::{Digest, Epoch, SecretName, Timestamp};
use rewind_engine::conformance::{self, T0, tenant};
use rewind_engine::ports::cluster::Membership;
use rewind_engine::ports::{
    AccessStore, ApiKey, CatalogStore, Cluster, ClusterError, ControlPlaneChange, Departed, Grant,
    NamespaceName, NamespaceWrite, NewSecret, NodeId, NodeInfo, Principal, Role, Secret, SecretId,
    SecretStore, Token, TokenKey, namespace_partition,
};
use rewind_store_postgres::{PostgresCluster, PostgresMembership, PostgresStore};
use support::Postgres;

/// The environment of the child process that holds a node's lock until it is killed.
const CHILD_URL: &str = "REWIND_CLUSTER_CHILD_URL";

/// What the child prints once it joined.
const JOINED: &str = "rewind-cluster-child-joined";

fn partitions(count: u16) -> NonZeroU16 {
    NonZeroU16::new(count).expect("at least one partition")
}

fn info(id: &str) -> NodeInfo {
    NodeInfo {
        id: NodeId::new(id).expect("node id"),
        address: format!("{id}.internal:7070"),
        build: "build-1".into(),
        epochs: BTreeSet::from([Epoch(1)]),
        pool: None,
        executors_only: false,
    }
}

/// Runs async work from a synchronous factory of the suites.
fn block_on<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(work))
}

async fn cluster_of(store: &PostgresStore, count: u16) -> PostgresCluster {
    PostgresCluster::open(store, partitions(count))
        .await
        .expect("open the cluster")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_control_plane_conforms_on_postgres() {
    let Some(postgres) = Postgres::start("control-plane") else {
        return;
    };
    conformance::catalog::run_all(|| {
        block_on(async {
            let store = postgres.store().await;
            let cluster = cluster_of(&store, 4).await;
            (store.catalog(), cluster)
        })
    })
    .await;
    conformance::catalog::run_migration(|| block_on(async { postgres.store().await.catalog() }))
        .await;
    conformance::cluster::run_all(|| {
        block_on(async {
            let store = postgres.store().await;
            cluster_of(&store, conformance::cluster::PARTITIONS).await
        })
    })
    .await;
}

/// The test the child process runs: it joins as `node-child` and holds its lock until it is
/// killed. Without its environment, it does nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_node() {
    let Ok(url) = std::env::var(CHILD_URL) else {
        return;
    };
    let mut config = rewind_store_postgres::Config::new(url);
    config.max_connections = 2;
    let store = PostgresStore::connect(
        &config,
        rewind_store_postgres::MasterKey::from_bytes([1; 32]),
    )
    .await
    .expect("connect");
    let cluster = cluster_of(&store, 4).await;
    let _member = cluster.join(info("node-child")).await.expect("join");
    println!("{JOINED}");
    std::future::pending::<()>().await;
}

async fn departs_within(member: &PostgresMembership, node: &str, kill: impl FnOnce()) -> Duration {
    let mut watch = Box::pin(member.watch());
    // The watch reaches its wait on the lock before the node dies.
    assert!(
        tokio::time::timeout(Duration::from_millis(500), watch.as_mut())
            .await
            .is_err(),
        "{node} is alive"
    );
    let killed = Instant::now();
    kill();
    let departed = tokio::time::timeout(Duration::from_secs(10), watch)
        .await
        .expect("the watch ends")
        .expect("watch");
    assert_eq!(
        departed,
        Departed {
            node: NodeId::new(node).expect("node id")
        }
    );
    let elapsed = killed.elapsed();
    eprintln!("{node} departed {elapsed:?} after it was killed");
    elapsed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_whose_connection_or_process_dies_wakes_its_watcher_at_once() {
    let Some(postgres) = Postgres::start("liveness") else {
        return;
    };
    let database = postgres.create_database();
    let a_store = postgres.connect(&database).await;
    let b_store = postgres.connect(&database).await;
    let a = cluster_of(&a_store, 4)
        .await
        .join(info("node-a"))
        .await
        .expect("join");
    let b = cluster_of(&b_store, 4)
        .await
        .join(info("node-b"))
        .await
        .expect("join");

    // The connection holding node-b's lock is killed: its watcher is told, and node-b learns
    // that it is no member any more.
    let elapsed = departs_within(&a, "node-b", || {
        postgres.psql(
            &database,
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE application_name = 'rewind node node-b'",
        );
    })
    .await;
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    let lost = tokio::time::timeout(Duration::from_secs(10), b.watch())
        .await
        .expect("node-b notices");
    assert_eq!(
        lost,
        Err(ClusterError::NotMember(
            NodeId::new("node-b").expect("node id")
        ))
    );
    assert!(matches!(b.nodes().await, Err(ClusterError::NotMember(_))));
    let ids: Vec<String> = a
        .nodes()
        .await
        .expect("nodes")
        .into_iter()
        .map(|node| node.id.to_string())
        .collect();
    assert_eq!(ids, ["node-a"]);

    // Another process holds node-child's lock and is killed.
    let mut child = Command::new(std::env::current_exe().expect("the test binary"))
        .args([
            "--exact",
            "control_plane::child_node",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_URL, postgres.url(&database))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the child");
    let stdout = child.stdout.take().expect("its output");
    let (joined, child_joined) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if line.is_ok_and(|line| line.contains(JOINED)) {
                let _ = joined.send(());
                return;
            }
        }
    });
    child_joined
        .recv_timeout(Duration::from_secs(60))
        .expect("the child joined");
    let elapsed = departs_within(&a, "node-child", || {
        child.kill().expect("kill the child");
    })
    .await;
    let _ = child.wait();
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");

    // Node-b may join again, under the same id.
    let again = cluster_of(&b_store, 4)
        .await
        .join(info("node-b"))
        .await
        .expect("join again");
    assert_eq!(a.nodes().await.expect("nodes").len(), 2);
    again.leave().await.expect("leave");
}

async fn next(changes: &mut rewind_store_postgres::PostgresChanges) -> ControlPlaneChange {
    tokio::time::timeout(Duration::from_secs(10), changes.next())
        .await
        .expect("a change in time")
        .expect("a change")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn access_and_secrets_send_their_changes() {
    let Some(postgres) = Postgres::start("changes") else {
        return;
    };
    let store = postgres.store().await;
    let member = cluster_of(&store, 4)
        .await
        .join(info("listener"))
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

    let tokens: Vec<Token> = (0..2u8)
        .map(|n| Token {
            hash: Digest::of(&[n]),
            tenant: acme.clone(),
            namespace: alice.clone(),
            permissions: BTreeSet::from(["runs:read".to_owned()]),
            labels: BTreeMap::new(),
            expires_at: Timestamp(T0.0 + 1_000),
            created_by: Principal::Key(key.hash),
            created_at: T0,
            revoked_at: None,
        })
        .collect();
    for token in &tokens {
        store.put_token(token).await.expect("put_token");
        assert_eq!(
            next(&mut changes).await,
            ControlPlaneChange::Token {
                tenant: acme.clone(),
                hash: token.hash
            }
        );
    }
    assert_eq!(store.revoke_tokens(&acme, &alice, T0).await, Ok(2));
    let revoked = [next(&mut changes).await, next(&mut changes).await];
    for token in &tokens {
        let changed = ControlPlaneChange::Token {
            tenant: acme.clone(),
            hash: token.hash,
        };
        assert!(
            revoked.contains(&changed),
            "every token the namespace's revocation touched: {revoked:?}"
        );
    }

    store
        .put_token_key(&TokenKey {
            kid: "rk_1".into(),
            tenant: acme.clone(),
            name: "signer".into(),
            jwk: "{}".into(),
            permissions: BTreeSet::new(),
            created_by: Principal::Key(key.hash),
            created_at: T0,
        })
        .await
        .expect("put_token_key");
    assert_eq!(
        next(&mut changes).await,
        ControlPlaneChange::TokenKey {
            tenant: acme.clone(),
            kid: "rk_1".into()
        }
    );

    // A role whose name is above Postgres' limit on a payload goes in frames.
    let role = Role {
        tenant: acme.clone(),
        name: "r".repeat(20_000),
        description: String::new(),
        permissions: BTreeSet::new(),
        created_at: T0,
        updated_at: T0,
    };
    store.put_role(&role).await.expect("put_role");
    assert_eq!(
        next(&mut changes).await,
        ControlPlaneChange::Role {
            tenant: acme.clone(),
            name: role.name.clone()
        }
    );
    let user = Principal::Member("user_1".into());
    let grant = Grant {
        tenant: acme.clone(),
        principal: user.clone(),
        role: "owner".into(),
        allowed_labels: BTreeMap::new(),
        granted_by: None,
        granted_at: T0,
    };
    store.put_grant(&grant, None).await.expect("put_grant");
    assert_eq!(
        next(&mut changes).await,
        ControlPlaneChange::Grant {
            tenant: acme.clone(),
            principal: user
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
            by: "member:user_1".into(),
        },
        T0,
    )
    .await
    .expect("put a secret");
    let secret_changed = ControlPlaneChange::Secret {
        tenant: acme.clone(),
        secret: github.clone(),
    };
    assert_eq!(next(&mut changes).await, secret_changed);
    assert_eq!(
        SecretStore::delete_namespace(&store, &acme, &alice).await,
        Ok(1)
    );
    assert_eq!(
        next(&mut changes).await,
        secret_changed,
        "a namespace's erasure sends each secret it deleted"
    );
}

/// Rows created before `namespaces.partition` have none: `ensure_namespace` places the one it
/// is asked for, `place_namespaces` every other one, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn namespaces_without_a_partition_are_placed_in_theirs() {
    let Some(postgres) = Postgres::start("placement") else {
        return;
    };
    let database = postgres.create_database();
    let store = postgres.connect(&database).await;
    let catalog = store.catalog();
    let acme = tenant("acme");
    let (alice, bob) = (
        NamespaceName::new("alice").expect("namespace"),
        NamespaceName::new("bob").expect("namespace"),
    );
    for name in [&alice, &bob] {
        postgres.psql(
            &database,
            &format!(
                "INSERT INTO namespaces (tenant, name, created_at, last_active_at) \
                 VALUES ('acme', '{name}', {at}, {at})",
                at = T0.0
            ),
        );
    }
    assert!(
        catalog.get_namespace(&acme, &alice).await.is_err(),
        "a namespace without a partition cannot be read"
    );
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
    assert_eq!(catalog.place_namespaces(partitions(64)).await, Ok(0));
    assert!(
        PostgresCluster::open(&store, partitions(4)).await.is_ok()
            && PostgresCluster::open(&store, partitions(8)).await.is_ok(),
        "partitions are added"
    );
    assert!(
        PostgresCluster::open(&store, partitions(4)).await.is_err(),
        "never removed"
    );
}
