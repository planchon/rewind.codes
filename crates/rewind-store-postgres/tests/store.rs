//! What the conformance suites cannot see: the database itself. Its schema version and
//! migrations, the keys of version 1 becoming keys and grants, servers starting together, what a
//! call dropped half-way leaves, secrets at rest, the master key replaced and the credentials of
//! version 9 dropped, the handles and numbers given to what version 12 stored, the saved
//! workflows of version 15 becoming the workflow model's, the byte order of tenants, and owners
//! demoting each other at once.
//!
//! The checks share one Postgres container, each on a new database.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use futures_util::future::join_all;
use rewind_core::{
    Digest, RunId, SecretName, Suite, SuiteTest, Timestamp, WorkflowId, WorkflowVersion,
};
use rewind_engine::ports::legacy::LegacyRuns;
use rewind_engine::ports::{
    AccessStore, ApiKey, CatalogMigration, CatalogStore, CodeTitle, Grant, GrantDeletion,
    GrantWrite, NamespaceName, NewSecret, NewVersion, NewWorkflow, Principal, RunWorkflow,
    ScopeFilter, Secret, SecretError, SecretId, SecretReader, SecretStore, StoredWorkflow,
    SuiteMember, Tenant, TenantId, TestRecord, WorkflowCreated, WorkflowFilter, WorkflowScope,
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
    check!(saved_workflows_of_version_15_become_workflows);
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

/// The migrations released before the workflow model, version 1 first.
const RELEASED: [&str; 15] = [
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
    include_str!("../migrations/0014_control_plane.sql"),
    include_str!("../migrations/0015_legacy.sql"),
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
/// use nothing.
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
        "the run is kept, set aside for migrate-runs, and uses nothing"
    );
    let (tenant, secret) = secret_of("acme", "a-1");
    store.put(&tenant, &secret, T0).await.expect("put");
    assert_eq!(store.master_keys_in_use().await, Ok(vec![key().id()]));
}

/// A version of the saved workflow `name` that runs `code`, as a build before the workflow model
/// stored it in `workflows.document` (the names of before the renames: `script`, `toolset`), and
/// its digest. `fields` are set over it; a `null` removes one.
fn saved_version(name: &str, code: &str, fields: serde_json::Value) -> (String, Digest) {
    let mut document = serde_json::json!({
        "v": 1,
        "name": name,
        "script": Digest::of(code.as_bytes()).to_hex(),
        "toolset": Digest::of(b"action_set").to_hex(),
        "schemas": { "input": { "type": "object" }, "output": {}, "signals": {} },
    });
    if let (Some(document), serde_json::Value::Object(fields)) = (document.as_object_mut(), fields)
    {
        for (field, value) in fields {
            if value.is_null() {
                document.remove(&field);
            } else {
                document.insert(field, value);
            }
        }
    }
    let text = rewind_core::canonical_json(&document);
    let digest = Digest::of(text.as_bytes());
    (text, digest)
}

/// The runs and workflow versions of a server of schema version 12 get handles and numbers once
/// the store is upgraded (migration 13, as released), and the runs keep them set aside in
/// `legacy` for `migrate-runs`. Runs of a tenant whose ids share their first 64 bits take the
/// full suffix; the others keep the short one, as do the runs of another tenant. A run's origin
/// is its workflow, or `script`. Versions are numbered by workflow and tenant, by their creation,
/// then in the order they were stored, and keep their numbers in the workflow model (migration
/// 16), where a new one takes the next number. A run of a saved version is a run of its
/// workflow's version.
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

    let none = serde_json::json!({});
    let triage = saved_version("triage", "t1", none.clone());
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
        ("acme", other_twin, Some(("triage", Digest::of(b"gone")))),
        ("acme", alone, Some(("triage", triage.1))),
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
        .bind(workflow.map(|(name, _)| name))
        .bind(workflow.map(|(_, version)| version.as_bytes().to_vec()))
        .execute(&pool)
        .await
        .expect("a run of version 12");
    }
    // Versions of `triage` in `acme`: two created at once, then one created before them but
    // stored last; one of another workflow, and one of `triage` in `beta`.
    let second = saved_version("triage", "t2", none.clone());
    let first = saved_version("triage", "t3", none.clone());
    let digest = saved_version("digest", "d1", none.clone());
    let theirs = saved_version("triage", "x1", none);
    let versions = [
        ("acme", "triage", &triage, 10),
        ("acme", "triage", &second, 10),
        ("acme", "triage", &first, 5),
        ("acme", "digest", &digest, 1),
        ("beta", "triage", &theirs, 1),
    ];
    for (tenant, name, (document, version), created_at) in versions {
        sqlx::query(
            "INSERT INTO workflows (tenant, version, name, document, labels, created_at) \
             VALUES ($1, $2, $3, $4, '{}', $5)",
        )
        .bind(tenant)
        .bind(version.as_bytes().to_vec())
        .bind(name)
        .bind(document)
        .bind(created_at)
        .execute(&pool)
        .await
        .expect("a workflow version of version 12");
    }
    pool.close().await;

    let store = postgres.connect(&database).await;
    assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
    let acme = TenantId::new("acme").expect("tenant");
    let set_aside = store
        .legacy()
        .runs(None, 10)
        .await
        .expect("the runs set aside");
    let handles: BTreeMap<RunId, (String, String, usize)> = set_aside
        .iter()
        .map(|run| {
            (
                run.run.id,
                (
                    run.run.tenant.to_string(),
                    run.origin.clone(),
                    usize::from(run.suffix_len),
                ),
            )
        })
        .collect();
    let handle =
        |tenant: &str, origin: &str, len: usize| (tenant.to_owned(), origin.to_owned(), len);
    assert_eq!(
        handles,
        BTreeMap::from([
            (twin, handle("acme", "script", MAX_SUFFIX_LEN)),
            (other_twin, handle("acme", "triage", MAX_SUFFIX_LEN)),
            (alone, handle("acme", "triage", SUFFIX_LEN)),
            (elsewhere, handle("beta", "script", SUFFIX_LEN)),
        ]),
        "a run of a workflow is its workflow's; the runs of another tenant do not count"
    );
    let triage_id = WorkflowId::of_saved("acme", "triage");
    let workflows: BTreeMap<RunId, Option<RunWorkflow>> = set_aside
        .into_iter()
        .map(|run| (run.run.id, run.metadata.workflow))
        .collect();
    assert_eq!(
        workflows,
        BTreeMap::from([
            (twin, None),
            (other_twin, None),
            (
                alone,
                Some(RunWorkflow {
                    id: triage_id.clone(),
                    name: Some("triage".into()),
                    version: 2,
                    digest: triage.1,
                    interface: true,
                })
            ),
            (elsewhere, None),
        ]),
        "a run of a saved version runs its workflow's version; one of a version the catalog \
         does not have, none"
    );

    let number = |tenant: &str, version: Digest| {
        postgres.psql(
            &database,
            &format!(
                "SELECT w.name || ' ' || v.version FROM workflow_versions v \
                 JOIN workflows w ON w.tenant = v.tenant AND w.id = v.workflow \
                 WHERE v.tenant = '{tenant}' AND v.digest = '\\x{}'",
                version.to_hex()
            ),
        )
    };
    assert_eq!(number("acme", first.1), "triage 1", "created first");
    assert_eq!(
        number("acme", triage.1),
        "triage 2",
        "stored before the other"
    );
    assert_eq!(number("acme", second.1), "triage 3");
    assert_eq!(number("acme", digest.1), "digest 1");
    assert_eq!(number("beta", theirs.1), "triage 1");
    let catalog = store.catalog();
    let next = catalog
        .put_version(
            &acme,
            &triage_id,
            &NewVersion {
                document: WorkflowVersion::new(Digest::of(b"t4"), Digest::of(b"action_set")),
                pulled: None,
            },
            T0,
        )
        .await
        .expect("put_version");
    assert_eq!(next.version, 4, "a new version takes the next number");
    assert_eq!(
        catalog
            .workflow(&acme, &triage_id)
            .await
            .expect("workflow")
            .map(|workflow| workflow.latest),
        Some(4)
    );
}

/// What a server of schema version 15 saved becomes the workflow model's (migration 16): each
/// saved workflow the organization's, its id derived from its tenant and name, with its name, its
/// latest version's description, its first version's labels, its versions numbered as they were
/// and their documents as they are, the codes they ran (each the first workflow's to run it) and
/// the suite of its latest version, with the tests older versions left and why. The titles people
/// gave codes wait for `rename-data`; the permission to write scripts, toolsets or actions writes
/// workflows.
async fn saved_workflows_of_version_15_become_workflows(postgres: &Postgres) {
    use serde_json::json;

    let database = postgres.create_database();
    let pool = sqlx::PgPool::connect(&postgres.url(&database))
        .await
        .expect("connect");
    sqlx::migrate::Migrator::new(Released(15))
        .await
        .expect("migrations 1 to 15")
        .run(&pool)
        .await
        .expect("apply migrations 1 to 15");

    let hex = |text: &str| Digest::of(text.as_bytes()).to_hex();
    let test = |name: &str| Digest::of(name.as_bytes());
    let (a, b, b2, c, erased) = (test("a"), test("b"), test("b2"), test("c"), test("erased"));
    for digest in [a, b, b2, c] {
        sqlx::query(
            "INSERT INTO tests (tenant, digest, namespace, size, reserved_at) \
             VALUES ('acme', $1, NULL, 100, 1)",
        )
        .bind(digest.as_bytes().to_vec())
        .execute(&pool)
        .await
        .expect("a test of version 15");
    }
    let suite = |tests: &[(&str, Digest)]| {
        tests
            .iter()
            .map(|(name, test)| json!({ "name": name, "test": test.to_hex() }))
            .collect::<Vec<_>>()
    };
    // `triage`: version 1 as written before the code was named so; version 2, under labels,
    // named so, retires `a` and replaces `b`; version 3 takes `a` back.
    let v1 = saved_version(
        "triage",
        "t1",
        json!({ "description": "Triage.", "tests": suite(&[("a", a), ("b", b)]) }),
    );
    let v2 = saved_version(
        "triage",
        "t2",
        json!({
            "script": null,
            "toolset": null,
            "code": hex("t2"),
            "action_set": hex("action_set"),
            "description": "Triage issues.",
            "labels": { "env": "dev" },
            "tests": suite(&[("b", b2), ("c", c)]),
            "retired": [{ "name": "a", "test": a.to_hex(), "reason": "deprecated" }],
            "replaced": [{
                "name": "b", "old": b.to_hex(), "new": b2.to_hex(), "reason": "out of date"
            }],
        }),
    );
    let v3 = saved_version(
        "triage",
        "t2",
        json!({
            "description": "Triage the issues.",
            "tests": suite(&[("a", a), ("b", b2), ("c", c)]),
        }),
    );
    // `digest` runs `triage`'s first code, after it; the row of its test was erased.
    let digest = saved_version("digest", "t1", json!({ "tests": suite(&[("d", erased)]) }));
    let gym = saved_version("prod_gym", "g1", json!({}));
    let saved = [
        ("acme", "triage", &v1, "{}", 10, 1),
        ("acme", "digest", &digest, "{}", 15, 1),
        ("acme", "triage", &v2, r#"{"env": "dev"}"#, 20, 2),
        ("acme", "triage", &v3, "{}", 30, 3),
        ("dax-staging", "prod_gym", &gym, "{}", 5, 1),
    ];
    for (tenant, name, (document, version), labels, created_at, number) in saved {
        sqlx::query(
            "INSERT INTO workflows (tenant, version, name, document, labels, created_at, number) \
             VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7)",
        )
        .bind(tenant)
        .bind(version.as_bytes().to_vec())
        .bind(name)
        .bind(document)
        .bind(labels)
        .bind(created_at)
        .bind(number)
        .execute(&pool)
        .await
        .expect("a saved version of version 15");
    }
    let creator = format!("key:{}", Digest::of(b"key").to_hex());
    for statement in [
        "INSERT INTO script_titles (tenant, digest, title, description, updated_by, updated_at) \
         VALUES ('acme', sha256('t3'), 'Weekly report', 'Sends it.', 'member:user_1', 1)"
            .to_owned(),
        "INSERT INTO roles (tenant, name, description, permissions, created_at, updated_at) \
         VALUES ('acme', 'writer', '', '{scripts:write,runs:read}', 1, 1), \
                ('acme', 'saver', '', '{toolsets:write}', 1, 1), \
                ('acme', 'reader', '', '{runs:read}', 1, 1), \
                ('acme', 'worker', '', '{tasks:work,tasks:approve,toolsets:read}', 1, 1)"
            .to_owned(),
        "INSERT INTO grants (tenant, principal, role, allowed_labels, granted_by, granted_at) \
         VALUES ('acme', 'user:user_9', 'reader', '{}', 'user:user_1', 1)"
            .to_owned(),
        "INSERT INTO subscriptions (tenant, id, url, secret, events, labels) \
         VALUES ('acme', 'sub_1', 'https://example.com', 's', \
                 '[\"credentials.expired\",\"run.completed\",\"run.suspended\"]', '{}')"
            .to_owned(),
        format!(
            "INSERT INTO tokens (hash, tenant, namespace, permissions, labels, expires_at, \
             created_by, created_at) \
             VALUES (sha256('token'), 'acme', 'alice', '{{scripts:write,workflows:write}}', \
             '{{}}', 9000000000000, '{creator}', 1)"
        ),
        format!(
            "INSERT INTO token_keys (kid, tenant, name, jwk, permissions, created_by, \
             created_at) \
             VALUES ('rk_1', 'acme', 'signer', '{{}}', '{{actions:write,runs:read}}', \
             '{creator}', 1)"
        ),
    ] {
        sqlx::query(&statement)
            .execute(&pool)
            .await
            .expect("a row of version 15");
    }
    pool.close().await;

    let store = postgres.connect(&database).await;
    assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
    let catalog = store.catalog();
    let acme = TenantId::new("acme").expect("tenant");
    let triage_id = WorkflowId::of_saved("acme", "triage");
    let digest_id = WorkflowId::of_saved("acme", "digest");
    assert_eq!(
        postgres.psql(
            &database,
            "SELECT id FROM workflows WHERE tenant = 'dax-staging' AND name = 'prod_gym'"
        ),
        "wf_rs1wp4r9g4hf",
        "the id SQL derives is WorkflowId::of_saved's"
    );
    assert_eq!(triage_id.as_str(), "wf_e2nzwv04tf3f");
    let saved_workflow =
        |id: &WorkflowId, name: &str, description: Option<&str>, created_at, latest| {
            StoredWorkflow {
                id: id.clone(),
                scope: WorkflowScope::Organization,
                name: Some(name.to_owned()),
                description: description.map(str::to_owned),
                labels: BTreeMap::new(),
                source: None,
                created_by: "rename-data".into(),
                created_at: Timestamp(created_at),
                promoted_at: None,
                latest,
            }
        };
    let mut expected = vec![
        saved_workflow(&digest_id, "digest", None, 15, 1),
        saved_workflow(&triage_id, "triage", Some("Triage the issues."), 10, 3),
    ];
    expected.sort_by(|one, other| one.id.cmp(&other.id));
    assert_eq!(
        catalog
            .list_workflows(&WorkflowFilter {
                tenant: acme.clone(),
                scope: ScopeFilter::All,
                code: None,
                labels: BTreeMap::new(),
                after: None,
                limit: 10,
            })
            .await
            .expect("list_workflows"),
        expected,
        "the organization's, by id; the latest version's description, the first's labels"
    );
    assert_eq!(
        postgres.psql(
            &database,
            "SELECT string_agg(v.version || ' ' || encode(v.digest, 'hex') || ' ' \
                 || encode(v.code, 'hex') || ' ' || v.labels::text || ' ' || v.created_at, ', ' \
                 ORDER BY v.version) \
             FROM workflow_versions v WHERE v.tenant = 'acme' AND v.workflow = 'wf_e2nzwv04tf3f'"
        ),
        format!(
            "1 {} {} {{}} 10, 2 {} {} {{\"env\": \"dev\"}} 20, 3 {} {} {{}} 30",
            v1.1.to_hex(),
            hex("t1"),
            v2.1.to_hex(),
            hex("t2"),
            v3.1.to_hex(),
            hex("t2")
        ),
        "its versions keep their numbers and digests; the code of a document of either shape"
    );
    assert_eq!(
        postgres.psql(
            &database,
            &format!(
                "SELECT document FROM workflow_versions \
                 WHERE tenant = 'acme' AND digest = '\\x{}'",
                v2.1.to_hex()
            )
        ),
        v2.0,
        "documents stay as they are, for rename-data"
    );
    assert_eq!(
        postgres.psql(
            &database,
            "SELECT string_agg(c.namespace || ' ' || encode(c.code, 'hex') || ' ' \
                 || c.labels::text || ' ' || w.name || ' ' || c.added_at, ', ' ORDER BY c.ordinal) \
             FROM workflow_codes c JOIN workflows w ON w.tenant = c.tenant AND w.id = c.workflow \
             WHERE c.tenant = 'acme'"
        ),
        format!(
            "* {t1} {{}} triage 10, * {t2} {{\"env\": \"dev\"}} triage 20, * {t2} {{}} triage 30",
            t1 = hex("t1"),
            t2 = hex("t2")
        ),
        "each code under each set of labels, the first workflow's to run it, in that order"
    );
    let member =
        |name: &str, digest, added_at, retired_at: Option<u64>, reason: Option<&str>| SuiteMember {
            name: name.to_owned(),
            digest,
            added_at: Timestamp(added_at),
            retired_at: retired_at.map(Timestamp),
            reason: reason.map(str::to_owned),
        };
    assert_eq!(
        catalog.suite(&acme, &triage_id).await.expect("suite"),
        [
            member("a", a, 30, None, None),
            member("b", b, 10, Some(20), Some("out of date")),
            member("b", b2, 20, None, None),
            member("c", c, 20, None, None),
        ],
        "the latest version's tests, each since the version that took it last; the tests that \
         left, when and why"
    );
    assert_eq!(
        catalog.suite(&acme, &digest_id).await.expect("suite"),
        [],
        "a test whose row is gone is left out"
    );
    assert_eq!(
        catalog.code_titles().await.expect("code_titles"),
        [CodeTitle {
            tenant: acme.clone(),
            code: Digest::of(b"t3"),
            title: "Weekly report".into(),
            description: Some("Sends it.".into()),
        }]
    );

    let permissions = |names: &[&str]| -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    };
    let role = |name: &'static str| {
        let (store, acme) = (&store, &acme);
        async move {
            store
                .get_role(acme, name)
                .await
                .expect("get_role")
                .expect("the role")
                .permissions
        }
    };
    assert_eq!(
        role("writer").await,
        permissions(&["runs:read", "workflows:write"])
    );
    assert_eq!(
        role("saver").await,
        permissions(&["actions:write", "workflows:write"]),
        "and migration 17 renames toolsets:write"
    );
    assert_eq!(
        role("worker").await,
        permissions(&["actions:read", "calls:approve", "calls:work"]),
        "migration 17 renames the permissions of calls and actions"
    );
    let pool = sqlx::PgPool::connect(&postgres.url(&database))
        .await
        .expect("the database");
    let (principal, granted_by): (String, String) =
        sqlx::query_as("SELECT principal, granted_by FROM grants WHERE tenant = 'acme'")
            .fetch_one(&pool)
            .await
            .expect("the grant");
    assert_eq!(
        (principal.as_str(), granted_by.as_str()),
        ("member:user_9", "member:user_1"),
        "a person is a member"
    );
    let (types,): (String,) = sqlx::query_as("SELECT types FROM subscriptions WHERE id = 'sub_1'")
        .fetch_one(&pool)
        .await
        .expect("the subscription");
    assert_eq!(
        types, r#"["secret.expired","run.completed","run.waiting"]"#,
        "a subscription lists the new webhook types"
    );
    pool.close().await;
    assert_eq!(role("reader").await, permissions(&["runs:read"]));
    assert_eq!(
        store
            .find_token(Digest::of(b"token"))
            .await
            .expect("find_token")
            .expect("the token")
            .permissions,
        permissions(&["workflows:write"])
    );
    assert_eq!(
        store
            .find_token_key("rk_1")
            .await
            .expect("find_token_key")
            .expect("the token key")
            .permissions,
        permissions(&["actions:write", "runs:read", "workflows:write"])
    );
    assert_eq!(
        postgres.psql(
            &database,
            "SELECT permissions::text FROM roles WHERE name = 'writer'"
        ),
        "{runs:read,workflows:write}",
        "sorted by their bytes"
    );

    // The workflow model goes on from there.
    let next = catalog
        .put_version(
            &acme,
            &triage_id,
            &NewVersion {
                document: WorkflowVersion::new(Digest::of(b"t4"), Digest::of(b"action_set")),
                pulled: None,
            },
            T0,
        )
        .await
        .expect("put_version");
    assert_eq!(next.version, 4);
    assert_eq!(
        catalog
            .suite(&acme, &triage_id)
            .await
            .expect("suite")
            .iter()
            .filter(|member| member.is_current())
            .count(),
        0,
        "a version without tests retires them"
    );
    let named = catalog
        .workflow_named(&acme, None, "triage", &BTreeMap::new())
        .await
        .expect("workflow_named");
    assert_eq!(named.map(|workflow| workflow.id), Some(triage_id));
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

/// A workflow named `triage` whose version 1's suite is `tests`.
fn workflow_with_tests(tests: &[TestRecord]) -> NewWorkflow {
    let version = WorkflowVersion::new(Digest::of(b"code"), Digest::of(b"action_set"))
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
            replaced: Vec::new(),
        })
        .expect("a valid suite");
    NewWorkflow {
        id: WorkflowId::from_bits(7),
        scope: WorkflowScope::Organization,
        name: Some("triage".into()),
        description: None,
        labels: BTreeMap::new(),
        source: None,
        created_by: "key:k1".into(),
        version,
    }
}

/// A workflow created with its version and suite, cancelled at any point (the caller timed out,
/// its task was aborted), is written whole or not at all; and the store keeps working.
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
    let workflow = workflow_with_tests(&tests);
    let rows = || {
        postgres.psql(
            &database,
            &format!(
                "SELECT (SELECT count(*) FROM workflow_versions WHERE workflow = '{id}') || ' ' \
                     || (SELECT count(*) FROM workflow_codes WHERE workflow = '{id}') || ' ' \
                     || (SELECT count(*) FROM workflow_tests WHERE workflow = '{id}')",
                id = workflow.id
            ),
        )
    };
    let mut written = false;
    for step in 0..400u64 {
        let create = catalog.create_workflow(&tenant, &workflow, T0);
        let outcome = tokio::time::timeout(Duration::from_micros(step * 25), create).await;
        let found = catalog
            .workflow(&tenant, &workflow.id)
            .await
            .expect("workflow");
        match found {
            None => assert_eq!(rows(), "0 0 0", "a workflow not written has nothing"),
            Some(stored) => {
                assert_eq!(stored.name.as_deref(), Some("triage"));
                assert_eq!(rows(), format!("1 1 {}", tests.len()));
                if let Ok(result) = outcome {
                    assert!(
                        matches!(result, Ok(WorkflowCreated::Created(_))),
                        "a write that returned succeeded: {result:?}"
                    );
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
