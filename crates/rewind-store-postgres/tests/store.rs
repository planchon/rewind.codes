//! What the conformance suites cannot see: the database itself. Its schema version and
//! migrations, the keys of version 1 becoming keys and grants, servers starting together, what a
//! call dropped half-way leaves, secrets at rest, the master key replaced and the credentials of
//! version 9 dropped, the handles and numbers given to what version 12 stored, the byte order of
//! tenants, and owners demoting each other at once.
//!
//! The checks share one Postgres container, each on a new database.

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::future::join_all;
use rewind_core::{
    Digest, RunId, SecretName, Suite, SuiteTest, Timestamp, Workflow, WorkflowSchemas,
};
use rewind_engine::ports::legacy::LegacyRuns;
use rewind_engine::ports::{
    AccessStore, ApiKey, CatalogStore, Grant, GrantDeletion, GrantWrite, NamespaceName, NewSecret,
    Principal, Secret, SecretError, SecretId, SecretReader, SecretStore, Tenant, TenantId,
    TestRecord,
};
use rewind_store_postgres::{Config, Error, MasterKey, MasterKeys, PostgresStore, SCHEMA_VERSION};
use support::Postgres;

const T0: Timestamp = Timestamp(1_767_225_600_000);

async fn connect(
    postgres: &Postgres,
    database: &str,
    keys: impl Into<MasterKeys>,
) -> Result<PostgresStore, Error> {
    PostgresStore::connect(&Config::new(postgres.url(database)), keys).await
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
    check!(a_call_dropped_half_way_writes_nothing);
    check!(tenants_are_in_byte_order);
    check!(owners_demoting_each_other_at_once_leave_one_owner);
    check!(secrets_are_encrypted_at_rest);
    check!(the_master_key_is_replaced_by_a_rewrap);
    check!(two_rewraps_at_once_change_each_data_key_once);
    check!(the_credentials_of_version_9_are_dropped);
    check!(runs_and_workflows_of_version_12_get_handles_and_numbers);
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

fn github_token(name: &str) -> NewSecret {
    NewSecret {
        id: SecretId::organization(SecretName::new(name).expect("valid")),
        value: Secret::new("ghp_plaintext_token"),
        labels: BTreeMap::new(),
        by: "member:ada".into(),
    }
}

fn reader() -> SecretReader {
    SecretReader::Run {
        run: RunId::from_u128(1),
        namespace: NamespaceName::new("alice").expect("valid"),
    }
}

async fn secrets_are_encrypted_at_rest(postgres: &Postgres) {
    let database = postgres.create_database();
    let store = postgres.connect(&database).await;
    // Two servers storing a tenant's first secrets at once agree on its data key.
    let other = postgres.connect(&database).await;
    let local = TenantId::local();
    let (first, second) = (github_token("github"), github_token("linear"));
    let (put_first, put_second) = tokio::join!(
        store.put(&local, &first, T0),
        other.put(&local, &second, T0),
    );
    put_first.expect("put");
    put_second.expect("put");
    assert_eq!(
        postgres.psql(&database, "SELECT count(*) FROM data_keys"),
        "1"
    );
    assert_eq!(
        postgres.psql(&database, "SELECT master_key FROM data_keys"),
        key().id().as_str(),
        "a data key names the master key that holds it"
    );
    let signing = store.signing_key(&local, T0).await.expect("signing_key");

    // No column holds the secret or the private key in clear.
    let clear = postgres.psql(
        &database,
        "SELECT count(*) FROM secrets WHERE position('ghp_plaintext_token'::bytea IN ciphertext) > 0",
    );
    assert_eq!(clear, "0", "a secret is stored in clear");
    let seed: String = signing
        .private
        .expose()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let clear = postgres.psql(
        &database,
        &format!(
            "SELECT count(*) FROM signing_keys \
             WHERE position('\\x{seed}'::bytea IN private_ciphertext) > 0"
        ),
    );
    assert_eq!(clear, "0", "a private key is stored in clear");

    for reader_store in [&store, &other] {
        for secret in [&first, &second] {
            assert_eq!(
                reader_store
                    .read(&local, &secret.id, &reader(), T0)
                    .await
                    .expect("read"),
                Some(secret.value.clone())
            );
        }
    }

    // Another master key reads nothing.
    let stranger = connect(postgres, &database, MasterKey::from_bytes([4; 32]))
        .await
        .expect("connect");
    assert_eq!(
        stranger.read(&local, &first.id, &reader(), T0).await,
        Err(SecretError::UnknownMasterKey(key().id()))
    );
}

fn secret_of(tenant: &str, value: &str) -> (TenantId, NewSecret) {
    let secret = NewSecret {
        id: SecretId::of_namespace(
            NamespaceName::new("alice").expect("valid"),
            SecretName::new("github").expect("valid"),
        ),
        value: Secret::new(value),
        labels: BTreeMap::new(),
        by: "token:alice".into(),
    };
    (TenantId::new(tenant).expect("tenant"), secret)
}

async fn value_of(store: &PostgresStore, (tenant, secret): &(TenantId, NewSecret)) -> Secret {
    store
        .read(tenant, &secret.id, &reader(), T0)
        .await
        .expect("read")
        .expect("the secret exists")
}

/// The operator gives a new master key and the old one as a previous key, rewraps the data keys,
/// then removes the old key: the secrets and private keys are never encrypted again, and read
/// the same throughout.
async fn the_master_key_is_replaced_by_a_rewrap(postgres: &Postgres) {
    let database = postgres.create_database();
    let (old, new) = (
        MasterKey::from_bytes([1; 32]),
        MasterKey::from_bytes([2; 32]),
    );
    let secrets = [secret_of("acme", "a-1"), secret_of("globex", "g-1")];
    let acme = &secrets[0].0;
    let before = connect(postgres, &database, old.clone())
        .await
        .expect("connect");
    for (tenant, secret) in &secrets {
        before.put(tenant, secret, T0).await.expect("put");
    }
    let signing = before.signing_key(acme, T0).await.expect("signing_key");
    assert_eq!(
        before
            .master_keys_in_use()
            .await
            .expect("master_keys_in_use"),
        [old.id()]
    );
    before.close().await;
    let sealed = || {
        postgres.psql(
            &database,
            "SELECT string_agg(encode(ciphertext, 'hex'), ',' ORDER BY tenant, name) FROM secrets \
             WHERE tenant IN ('acme', 'globex') \
             UNION ALL SELECT encode(private_ciphertext, 'hex') FROM signing_keys",
        )
    };
    let sealed_before = sealed();

    let ring = MasterKeys::new(new.clone()).with_previous([old.clone()]);
    let during = connect(postgres, &database, ring).await.expect("connect");
    for secret in &secrets {
        assert_eq!(
            value_of(&during, secret).await,
            secret.1.value,
            "read through a previous key"
        );
    }
    let initech = secret_of("initech", "i-1");
    during.put(&initech.0, &initech.1, T0).await.expect("put");
    let mut in_use = vec![old.id(), new.id()];
    in_use.sort();
    assert_eq!(
        during
            .master_keys_in_use()
            .await
            .expect("master_keys_in_use"),
        in_use,
        "a new data key is made under the current key"
    );
    assert_eq!(during.rewrap_data_keys().await, Ok(2));
    assert_eq!(during.rewrap_data_keys().await, Ok(0), "nothing left");
    assert_eq!(
        during
            .master_keys_in_use()
            .await
            .expect("master_keys_in_use"),
        [new.id()]
    );
    assert_eq!(sealed(), sealed_before, "nothing is encrypted again");
    during.close().await;

    let after = connect(postgres, &database, new.clone())
        .await
        .expect("connect");
    for secret in secrets.iter().chain([&initech]) {
        assert_eq!(value_of(&after, secret).await, secret.1.value);
    }
    assert_eq!(after.signing_key(acme, T0).await, Ok(signing));
    let stale = connect(postgres, &database, old).await.expect("connect");
    assert_eq!(
        stale.read(acme, &secrets[0].1.id, &reader(), T0).await,
        Err(SecretError::UnknownMasterKey(new.id())),
        "the old key alone no longer reads"
    );
}

/// Two servers rewrapping at once change each data key once between them.
async fn two_rewraps_at_once_change_each_data_key_once(postgres: &Postgres) {
    let database = postgres.create_database();
    let (old, new) = (
        MasterKey::from_bytes([1; 32]),
        MasterKey::from_bytes([2; 32]),
    );
    let before = connect(postgres, &database, old.clone())
        .await
        .expect("connect");
    let secrets: Vec<_> = (0..24)
        .map(|n| secret_of(&format!("tenant-{n}"), &format!("v-{n}")))
        .collect();
    for (tenant, secret) in &secrets {
        before.put(tenant, secret, T0).await.expect("put");
    }
    before.close().await;
    let ring = MasterKeys::new(new.clone()).with_previous([old]);
    let first = connect(postgres, &database, ring.clone())
        .await
        .expect("connect");
    let second = connect(postgres, &database, ring).await.expect("connect");
    let (one, two) = tokio::join!(
        tokio::spawn(async move { first.rewrap_data_keys().await }),
        tokio::spawn(async move { second.rewrap_data_keys().await }),
    );
    let (one, two) = (
        one.expect("join").expect("rewrap"),
        two.expect("join").expect("rewrap"),
    );
    assert_eq!(one + two, secrets.len(), "{one} + {two}");
    let after = connect(postgres, &database, new.clone())
        .await
        .expect("connect");
    assert_eq!(
        after
            .master_keys_in_use()
            .await
            .expect("master_keys_in_use"),
        [new.id()]
    );
    for secret in &secrets {
        assert_eq!(value_of(&after, secret).await, secret.1.value);
    }
}

/// The migrations released before run handles, version 1 first.
const RELEASED: [&str; 13] = [
    include_str!("../migrations/0001_schema.sql"),
    include_str!("../migrations/0002_access.sql"),
    include_str!("../migrations/0003_workflows.sql"),
    include_str!("../migrations/0004_run_workflows.sql"),
    include_str!("../migrations/0005_namespaces.sql"),
    include_str!("../migrations/0006_tests.sql"),
    include_str!("../migrations/0007_connections.sql"),
    include_str!("../migrations/0008_namespace_display_names.sql"),
    include_str!("../migrations/0009_park_kinds.sql"),
    include_str!("../migrations/0010_secrets.sql"),
    include_str!("../migrations/0011_run_orders.sql"),
    include_str!("../migrations/0012_run_changes.sql"),
    include_str!("../migrations/0013_run_handles.sql"),
];

/// Migrations 1 to `n`: a database as a build of schema version `n` left it.
#[derive(Debug)]
struct Released(usize);

impl<'s> sqlx::migrate::MigrationSource<'s> for Released {
    fn resolve(
        self,
    ) -> futures_util::future::BoxFuture<
        's,
        Result<Vec<sqlx::migrate::Migration>, sqlx::error::BoxDynError>,
    > {
        Box::pin(async move {
            Ok(RELEASED[..self.0]
                .iter()
                .zip(1..)
                .map(|(sql, version)| {
                    sqlx::migrate::Migration::new(
                        version,
                        format!("version {version}").into(),
                        sqlx::migrate::MigrationType::Simple,
                        (*sql).to_owned().into(),
                        false,
                    )
                })
                .collect())
        })
    }
}

/// The credentials of version 9 cannot be named, and are dropped with their data keys; the runs
/// attach nothing.
async fn the_credentials_of_version_9_are_dropped(postgres: &Postgres) {
    let database = postgres.create_database();
    let pool = sqlx::PgPool::connect(&postgres.url(&database))
        .await
        .expect("connect");
    // A database as a build of schema version 9 left it, with credentials.
    sqlx::migrate::Migrator::new(Released(9))
        .await
        .expect("migrations 1 to 9")
        .run(&pool)
        .await
        .expect("apply migrations 1 to 9");
    for statement in [
        "INSERT INTO data_keys (tenant, wrapped_key) VALUES ('acme', '\\x00')",
        "INSERT INTO secrets (tenant, owner, ciphertext) VALUES ('acme', 'toolset:ab', '\\x00')",
        "INSERT INTO runs (tenant, id, script, toolset, status, epoch, expires_at, labels, \
         created_at, updated_at, namespace) \
         VALUES ('acme', '00000000-0000-0000-0000-000000000001', decode(repeat('ab', 32), 'hex'), \
         decode(repeat('cd', 32), 'hex'), 'pending', 1, 2, '{}', 1, 1, 'alice')",
    ] {
        sqlx::query(statement)
            .execute(&pool)
            .await
            .expect("a row of version 9");
    }
    pool.close().await;

    let store = postgres.connect(&database).await;
    assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
    let acme = TenantId::new("acme").expect("tenant");
    assert_eq!(store.master_keys_in_use().await, Ok(Vec::new()));
    assert_eq!(
        store
            .list(
                &acme,
                &rewind_engine::ports::SecretNamespace::Organization,
                &BTreeMap::new(),
                rewind_engine::ports::Day(0)
            )
            .await,
        Ok(Vec::new())
    );
    assert_eq!(
        postgres.psql(
            &database,
            "SELECT secrets::text FROM legacy.runs \
             WHERE id = '00000000-0000-0000-0000-000000000001'"
        ),
        "{}",
        "the run is kept, set aside for migrate-runs, and attaches nothing"
    );
    let (tenant, secret) = secret_of("acme", "a-1");
    store.put(&tenant, &secret, T0).await.expect("put");
    assert_eq!(store.master_keys_in_use().await, Ok(vec![key().id()]));
}

/// A version of the workflow `name` that runs `code`.
fn workflow(name: &str, code: &str) -> Workflow {
    Workflow::new(
        name,
        None,
        Digest::of(code.as_bytes()),
        Digest::of(b"action_set"),
        WorkflowSchemas {
            input: serde_json::json!({ "type": "object" }),
            output: serde_json::json!({}),
            signals: BTreeMap::new(),
        },
    )
    .expect("a valid workflow")
}

/// The runs and workflow versions of a server of schema version 12 get handles and numbers once
/// the store is upgraded (migration 13, as released), and the runs keep them set aside in
/// `legacy` for `migrate-runs`. Runs of a tenant whose ids share their first 64 bits take the
/// full suffix; the others keep the short one, as do the runs of another tenant. A run's origin
/// is its workflow, or `script`. Versions are numbered by workflow and tenant, by their creation,
/// then in the order they were stored, and a new one takes the next number.
async fn runs_and_workflows_of_version_12_get_handles_and_numbers(postgres: &Postgres) {
    use rewind_core::{MAX_SUFFIX_LEN, SUFFIX_LEN, run_suffix};

    let database = postgres.create_database();
    let pool = sqlx::PgPool::connect(&postgres.url(&database))
        .await
        .expect("connect");
    sqlx::migrate::Migrator::new(Released(12))
        .await
        .expect("migrations 1 to 12")
        .run(&pool)
        .await
        .expect("apply migrations 1 to 12");

    let triage = workflow("triage", "t1");
    let ms = 1_795_383_896_000;
    // Two runs of `acme` made in one millisecond with the same 12 bits after it, as ids were
    // before handles; a third with other bits; a run of `beta` with the twins' bits.
    let twin = RunId::from_v7_parts(ms, 0x0c5, 1);
    let other_twin = RunId::from_v7_parts(ms, 0x0c5, 2);
    let alone = RunId::from_v7_parts(ms, 0x0c6, 3);
    let elsewhere = RunId::from_v7_parts(ms, 0x0c5, 4);
    assert_eq!(
        run_suffix(twin, SUFFIX_LEN),
        run_suffix(other_twin, SUFFIX_LEN),
        "the short suffix cannot tell the twins apart"
    );
    let runs = [
        ("acme", twin, None),
        ("acme", other_twin, None),
        ("acme", alone, Some(&triage)),
        ("beta", elsewhere, None),
    ];
    for (tenant, id, workflow) in runs {
        sqlx::query(
            "INSERT INTO runs (tenant, id, script, toolset, status, epoch, expires_at, labels, \
             created_at, updated_at, namespace, workflow, workflow_version) \
             VALUES ($1, $2, $3, $4, 'completed', 1, 2, '{}', 1, 1, 'alice', $5, $6)",
        )
        .bind(tenant)
        .bind(id.as_uuid())
        .bind(Digest::of(b"script").as_bytes().to_vec())
        .bind(Digest::of(b"action_set").as_bytes().to_vec())
        .bind(workflow.map(|workflow| workflow.name()))
        .bind(workflow.map(|workflow| workflow.version().as_bytes().to_vec()))
        .execute(&pool)
        .await
        .expect("a run of version 12");
    }
    // Versions of `triage` in `acme`: two created at once, then one created before them but
    // stored last; one of another workflow, and one of `triage` in `beta`.
    let second = workflow("triage", "t2");
    let first = workflow("triage", "t3");
    let digest = workflow("digest", "d1");
    let theirs = workflow("triage", "x1");
    let versions = [
        ("acme", &triage, 10),
        ("acme", &second, 10),
        ("acme", &first, 5),
        ("acme", &digest, 1),
        ("beta", &theirs, 1),
    ];
    for (tenant, workflow, created_at) in versions {
        sqlx::query(
            "INSERT INTO workflows (tenant, version, name, document, labels, created_at) \
             VALUES ($1, $2, $3, $4, '{}', $5)",
        )
        .bind(tenant)
        .bind(workflow.version().as_bytes().to_vec())
        .bind(workflow.name())
        .bind(rewind_format::encode_workflow(workflow))
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("a workflow version of version 12");
    }
    pool.close().await;

    let store = postgres.connect(&database).await;
    assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
    let acme = TenantId::new("acme").expect("tenant");
    let beta = TenantId::new("beta").expect("tenant");
    let set_aside: BTreeMap<RunId, (String, String, usize)> = store
        .legacy()
        .runs(None, 10)
        .await
        .expect("the runs set aside")
        .into_iter()
        .map(|run| {
            (
                run.run.id,
                (
                    run.run.tenant.to_string(),
                    run.origin,
                    usize::from(run.suffix_len),
                ),
            )
        })
        .collect();
    let handle =
        |tenant: &str, origin: &str, len: usize| (tenant.to_owned(), origin.to_owned(), len);
    assert_eq!(
        set_aside,
        BTreeMap::from([
            (twin, handle("acme", "script", MAX_SUFFIX_LEN)),
            (other_twin, handle("acme", "script", MAX_SUFFIX_LEN)),
            (alone, handle("acme", "triage", SUFFIX_LEN)),
            (elsewhere, handle("beta", "script", SUFFIX_LEN)),
        ]),
        "a run of a workflow is its workflow's; the runs of another tenant do not count"
    );

    let catalog = store.catalog();
    let number = |tenant: &TenantId, workflow: &Workflow| {
        let (tenant, version) = (tenant.clone(), workflow.version());
        let catalog = &catalog;
        async move {
            catalog
                .get_workflow(&tenant, version)
                .await
                .expect("get_workflow")
                .expect("the version is kept")
                .number
        }
    };
    assert_eq!(number(&acme, &first).await, 1, "created first");
    assert_eq!(number(&acme, &triage).await, 2, "stored before the other");
    assert_eq!(number(&acme, &second).await, 3);
    assert_eq!(number(&acme, &digest).await, 1);
    assert_eq!(number(&beta, &theirs).await, 1);
    let numbered = catalog
        .workflow_number(&acme, "triage", 1)
        .await
        .expect("workflow_number")
        .expect("version 1");
    assert_eq!(numbered.workflow, first);
    let next = catalog
        .put_workflow(&acme, &workflow("triage", "t4"), T0)
        .await
        .expect("put_workflow");
    assert_eq!(next.number, 4, "a new version takes the next number");
    assert_eq!(
        catalog
            .put_workflow(&acme, &triage, T0)
            .await
            .expect("put_workflow")
            .number,
        2,
        "a version put again keeps its number"
    );
}

/// Tenants are ordered by their bytes, as the domain orders them, whatever the database's
/// collation.
async fn tenants_are_in_byte_order(postgres: &Postgres) {
    let catalog = postgres.store().await.catalog();
    let mut names = vec!["b", "B", "a_b", "a-b", "aB", "A", "_z", "-z", "0"];
    for name in &names {
        catalog
            .put_tenant(&Tenant::new(TenantId::new(*name).expect("tenant")))
            .await
            .expect("put_tenant");
    }
    names.sort_unstable();
    let listed: Vec<String> = catalog
        .list_tenants()
        .await
        .expect("list_tenants")
        .into_iter()
        .map(|tenant| tenant.id.as_str().to_owned())
        .collect();
    assert_eq!(listed, names);
}

/// A version of the workflow `triage` whose suite is `tests`.
fn version_with_tests(tests: &[TestRecord]) -> Workflow {
    Workflow::new(
        "triage",
        None,
        Digest::of(b"script"),
        Digest::of(b"action_set"),
        WorkflowSchemas {
            input: serde_json::json!({ "type": "object" }),
            output: serde_json::json!({}),
            signals: BTreeMap::new(),
        },
    )
    .expect("a valid workflow")
    .with_suite(Suite {
        base: None,
        tests: tests
            .iter()
            .enumerate()
            .map(|(n, test)| SuiteTest {
                name: format!("test {n}"),
                test: test.digest,
            })
            .collect(),
        retired: Vec::new(),
        updated: Vec::new(),
    })
    .expect("a valid suite")
}

/// A version written with its suite, cancelled at any point (the caller timed out, its task was
/// aborted), is written whole or not at all; and the store keeps working.
async fn a_call_dropped_half_way_writes_nothing(postgres: &Postgres) {
    let database = postgres.create_database();
    let catalog = postgres.connect(&database).await.catalog();
    let tenant = TenantId::new("acme").expect("tenant");
    let tests: Vec<TestRecord> = (0..8)
        .map(|n| TestRecord {
            digest: Digest::of(format!("test {n}").as_bytes()),
            namespace: None,
            size: 100,
        })
        .collect();
    catalog
        .reserve_tests(&tenant, &tests, T0)
        .await
        .expect("reserve_tests");
    let version = version_with_tests(&tests);
    let references = || {
        postgres.psql(
            &database,
            &format!(
                "SELECT count(*) FROM workflow_tests WHERE version = '\\x{}'",
                version.version()
            ),
        )
    };
    let mut written = false;
    for step in 0..400u64 {
        let put = catalog.put_workflow(&tenant, &version, T0);
        let outcome = tokio::time::timeout(Duration::from_micros(step * 25), put).await;
        let found = catalog
            .get_workflow(&tenant, version.version())
            .await
            .expect("get_workflow");
        match found {
            None => assert_eq!(
                references(),
                "0",
                "a version not written references nothing"
            ),
            Some(stored) => {
                assert_eq!(stored.workflow, version);
                assert_eq!(references(), tests.len().to_string());
                if let Ok(result) = outcome {
                    result.expect("a write that returned succeeded");
                }
                written = true;
                break;
            }
        }
    }
    assert!(written, "a write with enough time succeeds");
}

fn member_grant(tenant: &TenantId, user: &str, role: &str) -> Grant {
    Grant {
        tenant: tenant.clone(),
        principal: Principal::Member(user.to_owned()),
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
