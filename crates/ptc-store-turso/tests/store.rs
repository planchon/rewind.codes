//! What the conformance suites cannot see: the database file itself. Its schema version and
//! migrations, what survives a restart, the settings in `meta`, secrets at rest, and claims
//! that read several pages of due tasks.

use std::collections::BTreeMap;
use std::path::Path;

use ptc_core::{
    Digest, Epoch, Event, HistoryEntry, Lifecycle, RunId, RunMode, RunStarted, RunStatus, Seed,
    Seq, Timestamp, ToolPolicy, ToolRoute,
};
use ptc_engine::ports::{
    Activation, CreateRun, Credentials, JournalStore, NewRun, NewToolTask, RunMetadata, RunRef,
    Secret, SecretError, SecretOwner, SecretStore, ServerCredentials, TaskFilter, TenantId,
};
use ptc_store_turso::{Error, MasterKey, SCHEMA_VERSION, TursoStore};
use serde_json::json;
use tempfile::TempDir;

const T0: Timestamp = Timestamp(1_767_225_600_000);

fn key() -> MasterKey {
    MasterKey::from_bytes([3; 32])
}

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

async fn open(path: &Path) -> TursoStore {
    TursoStore::open(path, key()).await.expect("open the store")
}

/// Rewrites the schema version recorded in a database file, as a newer build would.
async fn write_schema_version(path: &Path, version: u32) {
    let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
        .build()
        .await
        .expect("open the file");
    let connection = database.connect().expect("connect");
    connection
        .execute(
            "UPDATE meta SET value = ? WHERE key = 'schema_version'",
            vec![turso::Value::Text(version.to_string())],
        )
        .await
        .expect("update meta");
}

#[tokio::test]
async fn a_new_database_gets_the_current_schema() {
    let dir = TempDir::new().expect("temporary directory");
    let store = open(&dir.path().join("journal.db")).await;
    assert_eq!(
        store.schema_version().await.expect("version"),
        SCHEMA_VERSION
    );
}

#[tokio::test]
async fn a_newer_schema_is_refused() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("journal.db");
    drop(open(&path).await);
    write_schema_version(&path, SCHEMA_VERSION + 1).await;
    let refused = TursoStore::open(&path, key()).await.map(|_| ());
    assert_eq!(
        refused,
        Err(Error::NewerSchema {
            found: SCHEMA_VERSION + 1,
            supported: SCHEMA_VERSION
        })
    );
}

#[tokio::test]
async fn runs_survive_a_restart() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("journal.db");
    let entry = HistoryEntry {
        index: 0,
        recorded_at: T0,
        event: Event::Lifecycle(Lifecycle::RunStarted(started())),
    };
    {
        let store = open(&path).await;
        let new_run = NewRun {
            run: run(),
            started: started(),
            metadata: RunMetadata {
                labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
                ..RunMetadata::default()
            },
            created_at: T0,
        };
        let created = store
            .create_run(new_run, Some("call-1".into()))
            .await
            .expect("create_run");
        assert!(matches!(created, CreateRun::Created(ref c) if c.created));
        let lease = store
            .acquire_run(&run(), "worker", T0, std::time::Duration::from_secs(30))
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
    }

    let store = open(&path).await;
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
            NewRun {
                run: RunRef::new(TenantId::local(), RunId::from_u128(2)),
                started: started(),
                metadata: RunMetadata::default(),
                created_at: T0,
            },
            Some("call-1".into()),
        )
        .await
        .expect("create_run");
    assert!(
        matches!(retried, CreateRun::Created(ref c) if !c.created && c.run == run()),
        "idempotency keys survive too"
    );
}

#[tokio::test]
async fn settings_live_in_meta() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("journal.db");
    {
        let store = open(&path).await;
        assert_eq!(store.virtual_time().await.expect("virtual_time"), None);
        store
            .set_virtual_time(Some(T0))
            .await
            .expect("set_virtual_time");
        store
            .set_setting("dashboard", Some("on"))
            .await
            .expect("set_setting");
        assert_eq!(
            store.set_setting("schema_version", Some("9")).await,
            Err(Error::ReservedSetting("schema_version".into()))
        );
    }
    let store = open(&path).await;
    assert_eq!(store.virtual_time().await.expect("virtual_time"), Some(T0));
    assert_eq!(
        store
            .setting("dashboard")
            .await
            .expect("setting")
            .as_deref(),
        Some("on")
    );
    store
        .set_virtual_time(None)
        .await
        .expect("set_virtual_time");
    assert_eq!(store.virtual_time().await.expect("virtual_time"), None);
    assert_eq!(
        store.schema_version().await.expect("version"),
        SCHEMA_VERSION
    );
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

#[tokio::test]
async fn secrets_are_encrypted_at_rest() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("journal.db");
    let owner = SecretOwner::Toolset(Digest::of(b"toolset"));
    {
        let store = open(&path).await;
        store
            .put(&TenantId::local(), &owner, &credentials())
            .await
            .expect("put");
    }

    // Neither the database nor its write-ahead log holds the secret in clear.
    for file in std::fs::read_dir(dir.path()).expect("read the directory") {
        let bytes = std::fs::read(file.expect("entry").path()).expect("read the file");
        let found = bytes
            .windows(b"ghp_plaintext_token".len())
            .any(|window| window == b"ghp_plaintext_token");
        assert!(!found, "a secret is stored in clear");
    }

    let store = open(&path).await;
    assert_eq!(
        store
            .resolve(
                &TenantId::local(),
                RunId::from_u128(1),
                Digest::of(b"toolset")
            )
            .await
            .expect("resolve"),
        Some(credentials())
    );

    // Another master key reads nothing.
    drop(store);
    let stranger = TursoStore::open(&path, MasterKey::from_bytes([4; 32]))
        .await
        .expect("open");
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

#[test]
fn master_keys_are_generated_at_random() {
    let first = MasterKey::generate().expect("generate");
    let second = MasterKey::generate().expect("generate");
    assert_ne!(first, second);
    assert_eq!(MasterKey::from_bytes(first.to_bytes()), first);
}

#[tokio::test]
async fn a_claim_reads_past_tasks_without_a_free_slot() {
    let dir = TempDir::new().expect("temporary directory");
    let store = open(&dir.path().join("journal.db")).await;
    let new_run = NewRun {
        run: run(),
        started: started(),
        metadata: RunMetadata::default(),
        created_at: T0,
    };
    store.create_run(new_run, None).await.expect("create_run");
    let lease = store
        .acquire_run(&run(), "worker", T0, std::time::Duration::from_secs(30))
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
