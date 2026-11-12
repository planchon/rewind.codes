//! What the conformance suites cannot see: the database itself. Its schema version and
//! migrations, servers starting together, what a call dropped half-way leaves, secrets at rest,
//! the byte order of tenants, and claims that read several pages of due tasks.
//!
//! The checks share one Postgres container, each on a new database.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::future::join_all;
use ptc_core::{
    Command, Digest, Epoch, Event, HistoryEntry, Lifecycle, RunId, RunMode, RunStarted, RunStatus,
    Seed, Seq, Timestamp, ToolPolicy, ToolRoute,
};
use ptc_engine::ports::{
    AcquireRunnable, Activation, CreateRun, Credentials, JournalStore, NewRun, NewToolTask,
    RunMetadata, RunRef, STICKY_GRACE, Secret, SecretError, SecretOwner, SecretStore,
    ServerCredentials, TaskFilter, Tenant, TenantId, TenantScope,
};
use ptc_store_postgres::{Config, Error, MasterKey, PostgresStore, SCHEMA_VERSION};
use serde_json::json;
use support::Postgres;

const T0: Timestamp = Timestamp(1_767_225_600_000);

fn run() -> RunRef {
    RunRef::new(
        TenantId::local(),
        RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0001),
    )
}

fn started() -> RunStarted {
    RunStarted {
        script: Digest::of(b"script"),
        toolset: Digest::of(b"toolset"),
        input: json!({ "issue": 42 }).into(),
        seed: Seed::from_bytes([5; 32]),
        epoch: Epoch(1),
        expires_at: Timestamp(T0.0 + 86_400_000),
        mode: RunMode::Live,
    }
}

fn new_run(run: RunRef) -> NewRun {
    NewRun {
        run,
        started: started(),
        metadata: RunMetadata::default(),
        created_at: T0,
    }
}

async fn connect(
    postgres: &Postgres,
    database: &str,
    key: MasterKey,
) -> Result<PostgresStore, Error> {
    PostgresStore::connect(&Config::new(postgres.url(database)), key).await
}

fn key() -> MasterKey {
    MasterKey::from_bytes([1; 32])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_database_behind_the_store() {
    let Some(postgres) = Postgres::start("store") else {
        return;
    };
    macro_rules! check {
        ($check:ident) => {{
            eprintln!(concat!("store: ", stringify!($check)));
            $check(&postgres).await;
        }};
    }
    check!(a_new_database_gets_the_current_schema);
    check!(a_newer_schema_is_refused);
    check!(a_changed_migration_is_refused);
    check!(servers_starting_together_migrate_once);
    check!(runs_survive_a_restart);
    check!(a_call_dropped_half_way_writes_nothing);
    check!(secrets_are_encrypted_at_rest);
    check!(tenants_are_in_byte_order);
    check!(a_claim_reads_past_tasks_without_a_free_slot);
    check!(a_drawn_tenant_whose_runs_are_all_taken_yields_to_the_next);
    check!(concurrent_claims_keep_to_a_servers_limit);
    check!(concurrent_claims_keep_to_a_tools_limit);
}

async fn a_new_database_gets_the_current_schema(postgres: &Postgres) {
    let store = postgres.store().await;
    assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
}

async fn a_newer_schema_is_refused(postgres: &Postgres) {
    let database = postgres.create_database();
    drop(postgres.connect(&database).await);
    // A newer build applied a migration this one does not know.
    postgres.psql(
        &database,
        &format!(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, \
             execution_time) VALUES ({}, 'from the future', true, '\\x00', 0)",
            SCHEMA_VERSION + 1
        ),
    );
    let refused = connect(postgres, &database, key()).await.map(|_| ());
    assert_eq!(
        refused,
        Err(Error::NewerSchema {
            found: SCHEMA_VERSION + 1,
            supported: SCHEMA_VERSION
        })
    );
}

async fn a_changed_migration_is_refused(postgres: &Postgres) {
    let database = postgres.create_database();
    drop(postgres.connect(&database).await);
    postgres.psql(
        &database,
        "UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = 1",
    );
    let refused = connect(postgres, &database, key()).await.map(|_| ());
    assert!(matches!(refused, Err(Error::Migration(_))), "{refused:?}");
}

async fn servers_starting_together_migrate_once(postgres: &Postgres) {
    let database = postgres.create_database();
    let stores = join_all((0..6).map(|_| connect(postgres, &database, key()))).await;
    for store in stores {
        assert_eq!(
            store.expect("every server starts").schema_version().await,
            Ok(SCHEMA_VERSION)
        );
    }
    assert_eq!(
        postgres.psql(&database, "SELECT count(*) FROM _sqlx_migrations"),
        SCHEMA_VERSION.to_string()
    );
}

async fn runs_survive_a_restart(postgres: &Postgres) {
    let database = postgres.create_database();
    let entry = HistoryEntry {
        index: 0,
        recorded_at: T0,
        event: Event::Lifecycle(Lifecycle::RunStarted(started())),
    };
    {
        let store = postgres.connect(&database).await;
        let mut created = new_run(run());
        created.metadata.labels = BTreeMap::from([("agent".to_owned(), "triage".to_owned())]);
        let created = store
            .create_run(created, Some("call-1".into()))
            .await
            .expect("create_run");
        assert!(matches!(created, CreateRun::Created(ref c) if c.created));
        let lease = store
            .acquire_run(&run(), "worker", T0, Duration::from_secs(30))
            .await
            .expect("acquire_run")
            .expect("leasable");
        let consumed = store
            .take_inbox(&lease)
            .await
            .expect("take_inbox")
            .iter()
            .map(|item| item.id)
            .collect();
        store
            .commit_activation(
                &lease,
                Activation::new(vec![entry.clone()], consumed, RunStatus::Active),
                T0,
            )
            .await
            .expect("commit");
        store.close().await;
    }

    let store = postgres.connect(&database).await;
    assert_eq!(
        store.load_history(&run(), 0).await.expect("load_history"),
        [entry]
    );
    let record = store
        .get_run(&run())
        .await
        .expect("get_run")
        .expect("the run is still there");
    assert_eq!(record.status, RunStatus::Active);
    assert_eq!(record.metadata.labels["agent"], "triage");
    assert_eq!(record.sticky_worker.as_deref(), Some("worker"));
    let retried = store
        .create_run(
            new_run(RunRef::new(TenantId::local(), RunId::from_u128(2))),
            Some("call-1".into()),
        )
        .await
        .expect("create_run");
    assert!(
        matches!(retried, CreateRun::Created(ref c) if !c.created && c.run == run()),
        "idempotency keys survive too"
    );
}

/// A commit cancelled at any point (the caller timed out, its task was aborted) leaves the run
/// as it was, or committed whole; and the store keeps working.
async fn a_call_dropped_half_way_writes_nothing(postgres: &Postgres) {
    let store = postgres.store().await;
    store
        .create_run(new_run(run()), None)
        .await
        .expect("create");
    let lease = store
        .acquire_run(&run(), "worker", T0, Duration::from_secs(30))
        .await
        .expect("acquire")
        .expect("leasable");
    let consumed: Vec<u64> = store
        .take_inbox(&lease)
        .await
        .expect("take_inbox")
        .iter()
        .map(|item| item.id)
        .collect();
    let entries = vec![
        HistoryEntry {
            index: 0,
            recorded_at: T0,
            event: Event::Lifecycle(Lifecycle::RunStarted(started())),
        },
        HistoryEntry {
            index: 1,
            recorded_at: T0,
            event: Event::Command(Command::CompleteRun {
                output: json!("done").into(),
            }),
        },
    ];
    let mut committed = false;
    for step in 0..400u64 {
        let activation = Activation::new(entries.clone(), consumed.clone(), RunStatus::Completed);
        let commit = store.commit_activation(&lease, activation, T0);
        let outcome = tokio::time::timeout(Duration::from_micros(step * 25), commit).await;
        let history = store.load_history(&run(), 0).await.expect("load_history");
        let record = store.get_run(&run()).await.expect("get").expect("the run");
        match history.len() {
            0 => {
                assert_eq!(record.status, RunStatus::Pending);
                assert_eq!(record.history_len, 0);
            }
            2 => {
                assert_eq!(record.status, RunStatus::Completed);
                assert_eq!(record.history_len, 2);
                if let Ok(result) = outcome {
                    result.expect("a commit that returned succeeded");
                }
                committed = true;
                break;
            }
            other => panic!("a commit left {other} entries"),
        }
    }
    assert!(committed, "a commit with enough time succeeds");
}

fn credentials() -> Credentials {
    Credentials {
        servers: BTreeMap::from([(
            "github".to_owned(),
            ServerCredentials {
                headers: BTreeMap::from([(
                    "Authorization".to_owned(),
                    Secret::new("Bearer ghp_plaintext_token"),
                )]),
                env: BTreeMap::new(),
            },
        )]),
    }
}

async fn secrets_are_encrypted_at_rest(postgres: &Postgres) {
    let database = postgres.create_database();
    let owner = SecretOwner::Toolset(Digest::of(b"toolset"));
    let store = postgres.connect(&database).await;
    // Two servers storing a tenant's first secrets at once agree on its data key.
    let other = postgres.connect(&database).await;
    let (local, credentials) = (TenantId::local(), credentials());
    let run_owner = SecretOwner::Run(RunId::from_u128(9));
    let (first, second) = tokio::join!(
        store.put(&local, &owner, &credentials),
        other.put(&local, &run_owner, &credentials),
    );
    first.expect("put");
    second.expect("put");
    assert_eq!(
        postgres.psql(&database, "SELECT count(*) FROM data_keys"),
        "1"
    );

    // No column holds the secret in clear.
    let clear = postgres.psql(
        &database,
        "SELECT count(*) FROM secrets WHERE position('ghp_plaintext_token'::bytea IN ciphertext) > 0",
    );
    assert_eq!(clear, "0", "a secret is stored in clear");

    for reader in [&store, &other] {
        assert_eq!(
            reader
                .resolve(
                    &TenantId::local(),
                    RunId::from_u128(1),
                    Digest::of(b"toolset")
                )
                .await
                .expect("resolve"),
            Some(credentials.clone())
        );
    }
    assert_eq!(
        store
            .resolve(
                &TenantId::local(),
                RunId::from_u128(9),
                Digest::of(b"other")
            )
            .await
            .expect("resolve"),
        Some(credentials),
        "the run's own credentials"
    );

    // Another master key reads nothing.
    let stranger = connect(postgres, &database, MasterKey::from_bytes([4; 32]))
        .await
        .expect("connect");
    assert!(matches!(
        stranger
            .resolve(
                &TenantId::local(),
                RunId::from_u128(1),
                Digest::of(b"toolset")
            )
            .await,
        Err(SecretError::Crypto(_))
    ));
}

/// Tenants are ordered by their bytes, as the domain orders them, whatever the database's
/// collation: the draw of `acquire_runnable` indexes them in that order.
async fn tenants_are_in_byte_order(postgres: &Postgres) {
    let store = postgres.store().await;
    let mut names = vec!["b", "B", "a_b", "a-b", "aB", "A", "_z", "-z", "0"];
    for name in &names {
        store
            .put_tenant(&Tenant::new(TenantId::new(*name).expect("tenant")))
            .await
            .expect("put_tenant");
    }
    names.sort_unstable();
    let listed: Vec<String> = store
        .list_tenants()
        .await
        .expect("list_tenants")
        .into_iter()
        .map(|tenant| tenant.id.as_str().to_owned())
        .collect();
    assert_eq!(listed, names);
}

async fn a_claim_reads_past_tasks_without_a_free_slot(postgres: &Postgres) {
    let store = postgres.store().await;
    store
        .create_run(new_run(run()), None)
        .await
        .expect("create_run");
    let lease = store
        .acquire_run(&run(), "worker", T0, Duration::from_secs(30))
        .await
        .expect("acquire_run")
        .expect("leasable");
    // Forty calls of a tool limited to one call in flight, then one call of another tool: the
    // second task to hand out is past the first page of due tasks.
    let task = |seq: u32, tool: &str, concurrency: u32| {
        let route = ToolRoute::Host { group: None };
        let mut policy = ToolPolicy::default_for(&route);
        policy.concurrency = concurrency;
        NewToolTask {
            seq: Seq(seq),
            tool: tool.into(),
            args: json!({ "seq": seq }).into(),
            policy,
            route,
            server_concurrency: None,
        }
    };
    let mut activation = Activation::new(Vec::new(), Vec::new(), RunStatus::Active);
    activation.tool_calls = (1..=40).map(|seq| task(seq, "ask_user", 1)).collect();
    activation.tool_calls.push(task(41, "notify", 16));
    store
        .commit_activation(&lease, activation, T0)
        .await
        .expect("commit");

    let claimed = store
        .claim_tool_tasks("worker", &TaskFilter::All, T0, 2)
        .await
        .expect("claim_tool_tasks");
    let seqs: Vec<u32> = claimed.iter().map(|task| task.task.task.seq.0).collect();
    assert_eq!(seqs, [1, 41]);
    assert!(
        store
            .claim_tool_tasks("worker", &TaskFilter::All, T0, 10)
            .await
            .expect("claim_tool_tasks")
            .is_empty(),
        "every slot is taken"
    );
}

/// When another worker holds every runnable run of the drawn tenant, the worker takes the next
/// tenant's oldest run rather than nothing.
async fn a_drawn_tenant_whose_runs_are_all_taken_yields_to_the_next(postgres: &Postgres) {
    let database = postgres.create_database();
    let store = postgres.connect(&database).await;
    let acme = RunRef::new(TenantId::new("acme").expect("tenant"), RunId::from_u128(1));
    let globex = RunRef::new(
        TenantId::new("globex").expect("tenant"),
        RunId::from_u128(2),
    );
    for run in [&acme, &globex] {
        store
            .create_run(new_run(run.clone()), None)
            .await
            .expect("create");
    }
    let request = |draw| AcquireRunnable {
        owner: "worker",
        epochs: &[Epoch(1)],
        tenants: &TenantScope::All,
        now: T0,
        lease_duration: Duration::from_secs(30),
        sticky_grace: STICKY_GRACE,
        draw,
    };

    // Another worker is in the middle of taking acme's run: its row is locked.
    let pool = sqlx::PgPool::connect(&postgres.url(&database))
        .await
        .expect("connect");
    let mut other = pool.begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM runs WHERE id = $1 FOR UPDATE")
        .bind(acme.id.as_uuid())
        .execute(&mut *other)
        .await
        .expect("lock acme's run");
    let lease = store
        .acquire_runnable(&request(0))
        .await
        .expect("acquire")
        .expect("a run of another tenant");
    assert_eq!(
        lease.run, globex,
        "acme is drawn, its run is taken: globex's"
    );
    other.rollback().await.expect("rollback");

    let lease = store
        .acquire_runnable(&request(0))
        .await
        .expect("acquire")
        .expect("acme's run, free again");
    assert_eq!(lease.run, acme);
}

/// Creates run `n` with these tool tasks, all due at `T0`.
async fn run_with_tasks(store: &PostgresStore, n: u128, tasks: Vec<NewToolTask>) {
    let run = RunRef::new(TenantId::local(), RunId::from_u128(n));
    store
        .create_run(new_run(run.clone()), None)
        .await
        .expect("create");
    let lease = store
        .acquire_run(&run, "engine", T0, Duration::from_secs(30))
        .await
        .expect("acquire")
        .expect("leasable");
    let mut activation = Activation::new(Vec::new(), Vec::new(), RunStatus::Active);
    activation.tool_calls = tasks;
    store
        .commit_activation(&lease, activation, T0)
        .await
        .expect("commit");
}

/// Claims one task for each owner, all at once, then one at a time until nothing comes.
/// Returns how many the claims at once got, and how many in all. Each claim locks its own page
/// of due tasks: claims at once read different pages.
async fn claim_together(store: &PostgresStore, owners: &[String]) -> (usize, usize) {
    let claim = |owner: &str| {
        let owner = owner.to_owned();
        async move {
            store
                .claim_tool_tasks(&owner, &TaskFilter::All, T0, 1)
                .await
                .expect("claim")
                .len()
        }
    };
    let together: usize = join_all(owners.iter().map(|owner| claim(owner)))
        .await
        .into_iter()
        .sum();
    let mut all = together;
    loop {
        let more = claim(&owners[0]).await;
        if more == 0 {
            return (together, all);
        }
        all += more;
    }
}

/// Claims of one worker running at once never exceed an MCP server's limit together.
async fn concurrent_claims_keep_to_a_servers_limit(postgres: &Postgres) {
    let store = postgres.connect_with(&postgres.create_database(), 8).await;
    for round in 1..=4 {
        let tasks = (1..=48)
            .map(|seq| {
                let route = ToolRoute::Mcp {
                    server: "github".into(),
                };
                let mut policy = ToolPolicy::default_for(&route);
                policy.concurrency = 64;
                NewToolTask {
                    seq: Seq(seq),
                    tool: "github.get_issue".into(),
                    args: json!({ "number": seq }).into(),
                    policy,
                    route,
                    server_concurrency: Some(2),
                }
            })
            .collect();
        run_with_tasks(&store, round, tasks).await;
        // A new worker each round: the limit is per worker.
        let owners = vec![format!("dispatcher-{round}"); 6];
        let (together, all) = claim_together(&store, &owners).await;
        assert!(
            together <= 2,
            "{together} calls in flight to a server limited to 2"
        );
        assert_eq!(
            all, 2,
            "two calls in flight to the server under this worker"
        );
    }
}

/// Claims running at once never exceed a tool's limit in a run together, whatever the worker.
async fn concurrent_claims_keep_to_a_tools_limit(postgres: &Postgres) {
    let store = postgres.connect_with(&postgres.create_database(), 8).await;
    for round in 1..=4 {
        let tasks = (1..=48)
            .map(|seq| {
                let route = ToolRoute::Host { group: None };
                let mut policy = ToolPolicy::default_for(&route);
                policy.concurrency = 2;
                NewToolTask {
                    seq: Seq(seq),
                    tool: "desk.ask".into(),
                    args: json!({ "question": seq }).into(),
                    policy,
                    route,
                    server_concurrency: None,
                }
            })
            .collect();
        // The runs of earlier rounds have their two calls in flight: only this one's are due.
        run_with_tasks(&store, round, tasks).await;
        let owners: Vec<String> = (0..6).map(|n| format!("worker-{round}-{n}")).collect();
        let (together, all) = claim_together(&store, &owners).await;
        assert!(
            together <= 2,
            "{together} calls of a tool limited to 2 in flight"
        );
        assert_eq!(all, 2, "two calls of the tool in flight in the run");
    }
}

#[test]
fn master_keys_are_generated_at_random() {
    let first = MasterKey::generate().expect("generate");
    let second = MasterKey::generate().expect("generate");
    assert_ne!(first, second);
    assert_eq!(MasterKey::from_bytes(first.to_bytes()), first);
}

#[test]
fn configurations_never_print_the_connection_string() {
    let config = Config::new("postgres://ptc:hunter2@db.internal/ptc");
    let printed = format!("{config:?}");
    assert!(!printed.contains("hunter2"), "{printed}");
}
