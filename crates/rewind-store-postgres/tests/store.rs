//! What the conformance suites cannot see: the database itself. Its schema version and
//! migrations, the keys of version 1 becoming keys and grants, servers starting together, what a
//! call dropped half-way leaves, secrets at rest, the byte order of tenants, claims that read
//! several pages of due tasks, owners demoting each other at once, projectors applying the same
//! runs at once, and the percentiles of Postgres.
//!
//! The checks share one Postgres container, each on a new database.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::future::join_all;
use rewind_core::{
    Annotation, Command, Digest, Epoch, Event, HistoryEntry, Lifecycle, LogLevel, RunId, RunMode,
    RunStarted, RunStatus, Seed, Seq, Timestamp, ToolPolicy, ToolRoute,
};
use rewind_engine::ports::{
    AccessStore, AcquireRunnable, Activation, ApiKey, CreateRun, Credentials, Grant, GrantDeletion,
    GrantWrite, JournalStore, NewRun, NewToolTask, Predicate, Principal, ProjectedRun,
    QuarantineKind, QuarantineNote, ReadModel, RunMetadata, RunOrder, RunQuery, RunRef,
    RunSnapshot, STICKY_GRACE, Secret, SecretError, SecretOwner, SecretStore, ServerCredentials,
    TaskFilter, TaskPhase, TaskProgress, Tenant, TenantId, TenantScope, Wait, WaitKind,
};
use rewind_store_postgres::{Config, Error, MasterKey, PostgresStore, SCHEMA_VERSION};
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
    check!(keys_of_version_1_become_keys_and_grants);
    check!(servers_starting_together_migrate_once);
    check!(runs_survive_a_restart);
    check!(a_call_dropped_half_way_writes_nothing);
    check!(secrets_are_encrypted_at_rest);
    check!(tenants_are_in_byte_order);
    check!(a_claim_reads_past_tasks_without_a_free_slot);
    check!(a_drawn_tenant_whose_runs_are_all_taken_yields_to_the_next);
    check!(concurrent_claims_keep_to_a_servers_limit);
    check!(concurrent_claims_keep_to_a_tools_limit);
    check!(owners_demoting_each_other_at_once_leave_one_owner);
    check!(projectors_applying_together_keep_the_longest_snapshot);
    check!(percentiles_are_nearest_rank);
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

/// Only migration 1: a database as a build of schema version 1 left it.
#[derive(Debug)]
struct Version1;

impl<'s> sqlx::migrate::MigrationSource<'s> for Version1 {
    fn resolve(
        self,
    ) -> futures_util::future::BoxFuture<
        's,
        Result<Vec<sqlx::migrate::Migration>, sqlx::error::BoxDynError>,
    > {
        Box::pin(async {
            Ok(vec![sqlx::migrate::Migration::new(
                1,
                "schema".into(),
                sqlx::migrate::MigrationType::Simple,
                include_str!("../migrations/0001_schema.sql").into(),
                false,
            )])
        })
    }
}

/// A key as version 1 kept it: its role and allowed labels on the key itself.
struct KeyOfVersion1 {
    secret: &'static str,
    tenant: &'static str,
    role: &'static str,
    labels: &'static str,
    created_at: Timestamp,
    revoked_at: Option<Timestamp>,
}

/// Keys of a server running schema version 1 (in production, the operator's key among them)
/// keep working once the store is upgraded: each becomes a key with a name and a grant of its
/// role, `admin` becoming `operator`.
async fn keys_of_version_1_become_keys_and_grants(postgres: &Postgres) {
    let database = postgres.create_database();
    let pool = sqlx::PgPool::connect(&postgres.url(&database))
        .await
        .expect("connect");
    sqlx::migrate::Migrator::new(Version1)
        .await
        .expect("migration 1")
        .run(&pool)
        .await
        .expect("apply migration 1");
    let at = |seconds: u64| Timestamp(T0.0 + seconds * 1_000);
    let keys = [
        KeyOfVersion1 {
            secret: "rewind_sk_admin",
            tenant: "acme",
            role: "admin",
            labels: "{}",
            created_at: T0,
            revoked_at: None,
        },
        KeyOfVersion1 {
            secret: "rewind_sk_agent",
            tenant: "acme",
            role: "harness",
            labels: "{\"conversation\":\"c-1\"}",
            created_at: at(1),
            revoked_at: None,
        },
        KeyOfVersion1 {
            secret: "rewind_sk_gone",
            tenant: "beta",
            role: "reader",
            labels: "{}",
            created_at: at(2),
            revoked_at: Some(at(3)),
        },
    ];
    let millis = |at: Timestamp| i64::try_from(at.0).expect("in range");
    for key in &keys {
        sqlx::query(
            "INSERT INTO api_keys (hash, tenant, role, allowed_labels, created_at, revoked_at) \
             VALUES ($1, $2, $3, $4::jsonb, $5, $6)",
        )
        .bind(Digest::of(key.secret.as_bytes()).as_bytes().to_vec())
        .bind(key.tenant)
        .bind(key.role)
        .bind(key.labels)
        .bind(millis(key.created_at))
        .bind(key.revoked_at.map(millis))
        .execute(&pool)
        .await
        .expect("a key of version 1");
    }
    pool.close().await;

    let store = postgres.connect(&database).await;
    assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
    for (key, role) in keys.iter().zip(["operator", "harness", "reader"]) {
        let hash = Digest::of(key.secret.as_bytes());
        let tenant = TenantId::new(key.tenant).expect("tenant");
        assert_eq!(
            store.find_key(hash).await.expect("find_key"),
            Some(ApiKey {
                hash,
                tenant: tenant.clone(),
                name: format!("unnamed {role} key"),
                created_by: None,
                created_at: key.created_at,
                revoked_at: key.revoked_at,
            }),
            "{}",
            key.secret
        );
        let labels: BTreeMap<String, String> = serde_json::from_str(key.labels).expect("labels");
        assert_eq!(
            store
                .grant(&tenant, &Principal::Key(hash))
                .await
                .expect("grant"),
            Some(Grant {
                tenant,
                principal: Principal::Key(hash),
                role: role.to_owned(),
                allowed_labels: labels,
                granted_by: None,
                granted_at: key.created_at,
            }),
            "{}",
            key.secret
        );
    }
    let acme: Vec<Digest> = store
        .list_keys(&TenantId::new("acme").expect("tenant"))
        .await
        .expect("list_keys")
        .into_iter()
        .map(|key| key.hash)
        .collect();
    assert_eq!(
        acme,
        [
            Digest::of(b"rewind_sk_admin"),
            Digest::of(b"rewind_sk_agent")
        ]
    );
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

// -------------------------------------------------------------------------------------------
// The read model

const PROJECTED_RUNS: u128 = 12;
/// The entries of each projected run, each applied alone with the snapshot up to it.
const PROJECTED_ENTRIES: u64 = 12;

fn projected_run(n: u128) -> ProjectedRun {
    ProjectedRun {
        run: RunRef::new(
            TenantId::local(),
            RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0001_0000 | n),
        ),
        script: Digest::of(b"script"),
        headline: format!("// Run {n}"),
        toolset: Digest::of(b"toolset"),
        labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
        parent: None,
        workflow: None,
        started_by: None,
    }
}

/// The entry at `index`: `run_started`, then logs.
fn projected_entry(index: u64) -> HistoryEntry {
    let event = if index == 0 {
        Event::Lifecycle(Lifecycle::RunStarted(started()))
    } else {
        Event::Annotation(Annotation::Log {
            level: LogLevel::Info,
            message: format!("tick {index}"),
            callsite: None,
        })
    };
    HistoryEntry {
        index,
        recorded_at: Timestamp(T0.0 + index * 10),
        event,
    }
}

/// The snapshot of the first `len` entries: a call, and timers that name the snapshot, more or
/// fewer of them.
fn projected_snapshot(len: u64) -> RunSnapshot {
    let call = Wait {
        kind: WaitKind::Call,
        seq: Some(Seq(1)),
        name: Some("ask_user".into()),
        since: T0,
        until: None,
        callsite: None,
        task: None,
    };
    let timers = (0..len % 3).map(|n| Wait {
        kind: WaitKind::Timer,
        seq: Some(Seq(u32::try_from(len * 10 + n).expect("a seq"))),
        name: None,
        since: T0,
        until: Some(Timestamp(T0.0 + 100_000 - len * 1_000 + n)),
        callsite: None,
        task: None,
    });
    RunSnapshot {
        history_len: len,
        status: RunStatus::Active,
        mode: Some(RunMode::Live),
        waits: std::iter::once(call).chain(timers).collect(),
        ending: None,
    }
}

fn quarantine_at(at: u64) -> QuarantineNote {
    QuarantineNote {
        kind: QuarantineKind::Failures,
        reason: format!("recorded at {at}"),
        since: Timestamp(T0.0 + at),
    }
}

/// Some items in the order of `order`: as given, backwards, or the odd ones first.
fn reorder<T: Clone>(items: &[T], order: usize) -> Vec<T> {
    let mut items = items.to_vec();
    match order % 3 {
        0 => {}
        1 => items.reverse(),
        _ => {
            let (odd, even): (Vec<_>, Vec<_>) =
                items.into_iter().enumerate().partition(|(n, _)| n % 2 == 1);
            items = odd.into_iter().chain(even).map(|(_, item)| item).collect();
        }
    }
    items
}

/// Projectors on three servers apply the entries of the same runs at once, each entry with its
/// snapshot, each server in its own order; services record the tasks' states and the
/// quarantines at once, in their own orders. Every run keeps the snapshot of its whole history
/// and nothing of the shorter ones, the furthest state of its task, its latest quarantine, and
/// one row of labels and of search text.
fn member_grant(tenant: &TenantId, user: &str, role: &str) -> Grant {
    Grant {
        tenant: tenant.clone(),
        principal: Principal::User(user.to_owned()),
        role: role.to_owned(),
        allowed_labels: BTreeMap::new(),
        granted_by: None,
        granted_at: T0,
    }
}

async fn owners<S: AccessStore>(store: &S, tenant: &TenantId) -> Vec<Principal> {
    store
        .list_grants(tenant)
        .await
        .expect("list_grants")
        .into_iter()
        .filter(|grant| grant.role == "owner")
        .map(|grant| grant.principal)
        .collect()
}

/// Two owners demoting, then removing, each other at once, through two servers: the kept role's
/// check and its write are atomic across connections, so one write passes and the other finds
/// its target the last owner. The organization keeps exactly one owner.
async fn owners_demoting_each_other_at_once_leave_one_owner(postgres: &Postgres) {
    let database = postgres.create_database();
    let first = postgres.connect_with(&database, 4).await;
    let second = postgres.connect_with(&database, 4).await;
    for round in 0..25 {
        let tenant = TenantId::new(format!("org-{round}")).expect("tenant");
        for user in ["user_ada", "user_bob"] {
            let written = first
                .put_grant(&member_grant(&tenant, user, "owner"), Some("owner"))
                .await
                .expect("put_grant");
            assert_eq!(written, GrantWrite::Written);
        }
        let ada_demotes_bob = member_grant(&tenant, "user_bob", "reader");
        let bob_demotes_ada = member_grant(&tenant, "user_ada", "reader");
        let (one, other) = tokio::join!(
            first.put_grant(&ada_demotes_bob, Some("owner")),
            second.put_grant(&bob_demotes_ada, Some("owner")),
        );
        let mut outcomes = [one.expect("put_grant"), other.expect("put_grant")];
        outcomes.sort_by_key(|outcome| *outcome == GrantWrite::LastHolder);
        assert_eq!(
            outcomes,
            [GrantWrite::Written, GrantWrite::LastHolder],
            "round {round}"
        );
        let left = owners(&first, &tenant).await;
        assert_eq!(left.len(), 1, "round {round}: {left:?}");

        // A second owner again, then each removes the other.
        let carol = member_grant(&tenant, "user_carol", "owner");
        first
            .put_grant(&carol, Some("owner"))
            .await
            .expect("put_grant");
        let (one, other) = tokio::join!(
            first.delete_grant(&tenant, &left[0], Some("owner")),
            second.delete_grant(&tenant, &carol.principal, Some("owner")),
        );
        let mut outcomes = [one.expect("delete_grant"), other.expect("delete_grant")];
        outcomes.sort_by_key(|outcome| *outcome == GrantDeletion::LastHolder);
        assert_eq!(
            outcomes,
            [GrantDeletion::Deleted, GrantDeletion::LastHolder],
            "round {round}"
        );
        assert_eq!(owners(&first, &tenant).await.len(), 1, "round {round}");
    }
}

async fn projectors_applying_together_keep_the_longest_snapshot(postgres: &Postgres) {
    let database = postgres.create_database();
    let stores = join_all((0..3).map(|_| postgres.connect_with(&database, 4))).await;
    let lengths: Vec<u64> = (1..=PROJECTED_ENTRIES).collect();
    let tasks: Vec<TaskProgress> = [
        (0, TaskPhase::Approval),
        (0, TaskPhase::Ready),
        (1, TaskPhase::Claimed),
        (1, TaskPhase::Parked),
        (1, TaskPhase::Ready),
        (2, TaskPhase::Claimed),
        (2, TaskPhase::Deferred),
    ]
    .into_iter()
    .map(|(attempt, phase)| TaskProgress {
        seq: Seq(1),
        attempt,
        phase,
    })
    .collect();
    // Released at even times, quarantined at odd ones, and both at the last.
    let quarantines: Vec<(Option<QuarantineNote>, u64)> = (1..=6)
        .map(|at| ((at % 2 == 1).then(|| quarantine_at(at)), at))
        .chain([(Some(quarantine_at(6)), 6)])
        .collect();
    let work = stores.iter().enumerate().flat_map(|(server, store)| {
        let (lengths, tasks, quarantines) = (&lengths, &tasks, &quarantines);
        (0..PROJECTED_RUNS).map(move |n| async move {
            let order = server + usize::try_from(n).expect("small");
            let run = projected_run(n);
            for len in reorder(lengths, order) {
                let entries = [projected_entry(len - 1)];
                let snapshot = projected_snapshot(len);
                store.apply(&run, &entries, &snapshot).await.expect("apply");
            }
            for task in reorder(tasks, order + 1) {
                store
                    .record_tasks(&run.run, &[task])
                    .await
                    .expect("record_tasks");
            }
            for (note, at) in reorder(quarantines, order + 2) {
                let at = Timestamp(T0.0 + at);
                store
                    .record_quarantine(&run.run, note.as_ref(), at)
                    .await
                    .expect("record_quarantine");
            }
        })
    });
    join_all(work).await;

    let store = &stores[0];
    for n in 0..PROJECTED_RUNS {
        let run = projected_run(n);
        let query =
            RunQuery::new(TenantId::local()).with(Predicate::Id(vec![run.run.id.to_string()]));
        let found = store
            .search_runs(&query, RunOrder::Started, None, 10)
            .await
            .expect("search_runs");
        assert_eq!(found.len(), 1, "run {n} is projected once");
        let summary = &found[0];
        let mut waits = projected_snapshot(PROJECTED_ENTRIES).waits;
        waits[0].task = Some(TaskPhase::Deferred);
        assert_eq!(
            summary.waits, waits,
            "run {n}: the waits of the longest snapshot, and only them"
        );
        assert_eq!(
            summary.wakes_at,
            waits.iter().filter_map(|wait| wait.until).min()
        );
        assert_eq!(
            (summary.history_len, summary.last_activity_at),
            (
                PROJECTED_ENTRIES,
                Timestamp(T0.0 + (PROJECTED_ENTRIES - 1) * 10)
            )
        );
        assert_eq!(summary.quarantine, Some(quarantine_at(6)), "run {n}");
        assert_eq!(
            store.attempts(&run.run).await,
            Ok(BTreeMap::from([(Seq(1), 2)])),
            "run {n}: the highest attempt"
        );
    }
    for table in ["run_summaries", "run_labels", "run_search"] {
        assert_eq!(
            postgres.psql(&database, &format!("SELECT count(*) FROM {table}")),
            PROJECTED_RUNS.to_string(),
            "one row of {table} per run"
        );
    }
}

/// `percentile_disc`, which the statistics use, is the nearest-rank percentile of `Latency`:
/// the value at position `ceil(p × n / 100)`, counting from 1.
async fn percentiles_are_nearest_rank(postgres: &Postgres) {
    let database = postgres.create_database();
    let mismatches = postgres.psql(
        &database,
        "SELECT count(*) FROM generate_series(1, 500) AS n, \
             (VALUES (50, 0.5), (95, 0.95), (99, 0.99)) AS p(p, fraction) \
         WHERE (SELECT percentile_disc(p.fraction) WITHIN GROUP (ORDER BY v) \
                FROM generate_series(1, n) AS v) <> (p.p * n + 99) / 100",
    );
    assert_eq!(mismatches, "0", "percentiles that are not nearest-rank");
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
    let config = Config::new("postgres://rewind:hunter2@db.internal/rewind");
    let printed = format!("{config:?}");
    assert!(!printed.contains("hunter2"), "{printed}");
}
