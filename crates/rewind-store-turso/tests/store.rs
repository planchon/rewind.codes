//! What the conformance suites cannot see: the database file itself. Its schema version and
//! migrations, what survives a restart, the settings in `meta`, secrets at rest and the master key
//! replaced.

use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::path::Path;

use rewind_core::{ActionSet, Code, RunId, SecretName, Timestamp, Workflow, WorkflowSchemas};
use rewind_engine::ports::{
    ActionDeclarations, CatalogStore, CompiledCode, NamespaceName, NamespaceWrite, NewSecret,
    Secret, SecretError, SecretId, SecretReader, SecretStore, Tenant, TenantId,
};
use rewind_store_turso::{Error, MasterKey, MasterKeys, SCHEMA_VERSION, TursoStore};
use serde_json::json;
use tempfile::TempDir;

const T0: Timestamp = Timestamp(1_767_225_600_000);

fn key() -> MasterKey {
    MasterKey::from_bytes([3; 32])
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
    let store = open(&dir.path().join("control.db")).await;
    assert_eq!(
        store.schema_version().await.expect("version"),
        SCHEMA_VERSION
    );
}

#[tokio::test]
async fn a_newer_schema_is_refused() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("control.db");
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

/// The catalog of an embedded server is the same once it opens the file again: its tenant, its
/// namespace in its partition, its code, action set and workflow.
#[tokio::test]
async fn the_catalog_survives_a_restart() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("control.db");
    let tenant = TenantId::local();
    let alice = NamespaceName::new("alice").expect("namespace");
    let partitions = NonZeroU16::MIN;
    let code = Code::new("export default async () => 1;");
    let action_set = ActionSet::default();
    let workflow = Workflow::new(
        "triage",
        None,
        code.digest(),
        action_set.digest(),
        WorkflowSchemas {
            input: json!({ "type": "object" }),
            output: json!({}),
            signals: BTreeMap::new(),
        },
    )
    .expect("a valid workflow");
    let placed = {
        let catalog = open(&path).await.catalog();
        catalog
            .put_tenant(&Tenant::new(tenant.clone()))
            .await
            .expect("put_tenant");
        let NamespaceWrite::Written(placed) = catalog
            .ensure_namespace(&tenant, &alice, partitions, T0)
            .await
            .expect("ensure_namespace")
        else {
            panic!("alice is not being erased");
        };
        let compiled = CompiledCode {
            javascript: code.source().into(),
            source_map: String::new(),
        };
        catalog
            .put_code(&tenant, &code, &compiled)
            .await
            .expect("put_script");
        catalog
            .put_action_set(&tenant, &action_set, &ActionDeclarations::default())
            .await
            .expect("put_action_set");
        catalog
            .put_workflow(&tenant, &workflow, T0)
            .await
            .expect("put_workflow");
        placed
    };

    let catalog = open(&path).await.catalog();
    assert_eq!(
        catalog.get_tenant(&tenant).await.expect("get_tenant"),
        Some(Tenant::new(tenant.clone()))
    );
    assert_eq!(
        catalog
            .get_namespace(&tenant, &alice)
            .await
            .expect("get_namespace"),
        Some(placed)
    );
    assert_eq!(
        catalog
            .get_code(&tenant, code.digest())
            .await
            .expect("get_code")
            .map(|stored| stored.code),
        Some(code)
    );
    assert!(
        catalog
            .get_action_set(&tenant, action_set.digest())
            .await
            .expect("get_action_set")
            .is_some()
    );
    assert_eq!(
        catalog
            .get_workflow(&tenant, workflow.version())
            .await
            .expect("get_workflow")
            .map(|stored| stored.workflow),
        Some(workflow)
    );
}

#[tokio::test]
async fn settings_live_in_meta() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("control.db");
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

fn github_token() -> NewSecret {
    NewSecret {
        id: SecretId::organization(SecretName::new("github").expect("valid")),
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

#[tokio::test]
async fn secrets_are_encrypted_at_rest() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("control.db");
    let secret = github_token();
    let signing = {
        let store = open(&path).await;
        store
            .put(&TenantId::local(), &secret, T0)
            .await
            .expect("put");
        store
            .signing_key(&TenantId::local(), T0)
            .await
            .expect("signing_key")
    };

    // Neither the database nor its write-ahead log holds the secret or the private key in clear.
    for file in std::fs::read_dir(dir.path()).expect("read the directory") {
        let bytes = std::fs::read(file.expect("entry").path()).expect("read the file");
        for clear in [
            b"ghp_plaintext_token".as_slice(),
            signing.private.expose().as_slice(),
        ] {
            let found = bytes.windows(clear.len()).any(|window| window == clear);
            assert!(!found, "a secret or a private key is stored in clear");
        }
    }

    let store = open(&path).await;
    assert_eq!(
        store
            .read(&TenantId::local(), &secret.id, &reader(), T0)
            .await
            .expect("read"),
        Some(secret.value.clone())
    );

    assert_eq!(
        store.signing_key(&TenantId::local(), T0).await,
        Ok(signing),
        "the same key after a restart"
    );
    assert_eq!(store.master_keys_in_use().await, Ok(vec![key().id()]));

    // Another master key reads nothing.
    drop(store);
    let stranger = TursoStore::open(&path, MasterKey::from_bytes([4; 32]))
        .await
        .expect("open");
    assert_eq!(
        stranger
            .read(&TenantId::local(), &secret.id, &reader(), T0)
            .await,
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

async fn value_of(store: &TursoStore, (tenant, secret): &(TenantId, NewSecret)) -> Secret {
    store
        .read(tenant, &secret.id, &reader(), T0)
        .await
        .expect("read")
        .expect("the secret exists")
}

/// The operator gives a new master key and the old one as a previous key, rewraps the data keys,
/// then removes the old key: the secrets read the same throughout.
#[tokio::test]
async fn the_master_key_is_replaced_by_a_rewrap() {
    let dir = TempDir::new().expect("temporary directory");
    let path = dir.path().join("control.db");
    let (old, new) = (
        MasterKey::from_bytes([1; 32]),
        MasterKey::from_bytes([2; 32]),
    );
    let secrets = [secret_of("acme", "a-1"), secret_of("globex", "g-1")];
    let acme = &secrets[0].0;
    let signing = {
        let before = TursoStore::open(&path, old.clone()).await.expect("open");
        for (tenant, secret) in &secrets {
            before.put(tenant, secret, T0).await.expect("put");
        }
        assert_eq!(before.master_keys_in_use().await, Ok(vec![old.id()]));
        before.signing_key(acme, T0).await.expect("signing_key")
    };

    {
        let ring = MasterKeys::new(new.clone()).with_previous([old.clone()]);
        let during = TursoStore::open(&path, ring).await.expect("open");
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
            during.master_keys_in_use().await,
            Ok(in_use),
            "a new data key is made under the current key"
        );
        assert_eq!(during.rewrap_data_keys().await, Ok(2));
        assert_eq!(during.rewrap_data_keys().await, Ok(0), "nothing left");
        assert_eq!(during.master_keys_in_use().await, Ok(vec![new.id()]));
    }

    let after = TursoStore::open(&path, new.clone()).await.expect("open");
    for secret in &secrets {
        assert_eq!(value_of(&after, secret).await, secret.1.value);
    }
    assert_eq!(after.signing_key(acme, T0).await, Ok(signing));
    drop(after);
    let stale = TursoStore::open(&path, old).await.expect("open");
    assert_eq!(
        stale.read(acme, &secrets[0].1.id, &reader(), T0).await,
        Err(SecretError::UnknownMasterKey(new.id())),
        "the old key alone no longer reads"
    );
}

#[test]
fn master_keys_are_generated_at_random() {
    let first = MasterKey::generate().expect("generate");
    let second = MasterKey::generate().expect("generate");
    assert_ne!(first, second);
    assert_eq!(MasterKey::from_bytes(first.to_bytes()), first);
}
