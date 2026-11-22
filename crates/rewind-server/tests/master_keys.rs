//! The master key of the secret store replaced (`archi/04-tools.md`, "Secrets"), on Turso: a
//! server refuses to start while a data key is held under a master key it was not given, naming
//! its id (exit 70); it serves with the new key and the old one in `--previous-master-keys`;
//! `--rewrap-data-keys` encrypts the data keys again with the new key, after which the old key
//! can be removed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::Parser;
use rewind_core::{RunId, SecretName, Timestamp};
use rewind_engine::ports::{
    MasterKeyId, NamespaceName, NewSecret, Secret, SecretId, SecretReader, SecretStore, TenantId,
};
use rewind_server::{Rewrapped, ServerArgs, StartupError, exit};
use rewind_store_turso::{MasterKey, TursoStore};

const T0: Timestamp = Timestamp(1_767_225_600_000);
const OLD: [u8; 32] = [1; 32];
const NEW: [u8; 32] = [2; 32];

fn server(dir: &Path, keys: &[&str]) -> ServerArgs {
    let store = format!("turso:{}", dir.join("journal.db").display());
    let blobs = dir.join("blobs").display().to_string();
    let mut args = vec![
        "rewind server",
        "--store",
        &store,
        "--blobs",
        &blobs,
        "--listen",
        "127.0.0.1:0",
        "--activations",
        "1",
        "--dispatches",
        "1",
    ];
    args.extend_from_slice(keys);
    ServerArgs::try_parse_from(args).unwrap()
}

fn github() -> SecretId {
    SecretId::organization(SecretName::new("github").unwrap())
}

async fn read_with(dir: &Path, key: [u8; 32]) -> Option<Secret> {
    let store = TursoStore::open(dir.join("journal.db"), MasterKey::from_bytes(key))
        .await
        .unwrap();
    let reader = SecretReader::Run {
        run: RunId::from_u128(1),
        namespace: NamespaceName::new("alice").unwrap(),
    };
    store
        .read(&TenantId::new("acme").unwrap(), &github(), &reader, T0)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_master_key_is_replaced_and_a_key_left_out_refuses_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = (BASE64.encode(OLD), BASE64.encode(NEW));
    {
        // A secret put under the old key.
        let store = TursoStore::open(dir.path().join("journal.db"), MasterKey::from_bytes(OLD))
            .await
            .unwrap();
        let secret = NewSecret {
            id: github(),
            value: Secret::new("ghp_1"),
            labels: BTreeMap::new(),
            by: "user:ada".into(),
        };
        store
            .put(&TenantId::new("acme").unwrap(), &secret, T0)
            .await
            .unwrap();
    }

    // The new key alone: the old one would be removed too early.
    match rewind_server::start(server(dir.path(), &["--master-key", &new])).await {
        Err(error @ StartupError::Setup(_)) => {
            assert_eq!(rewind_server::exit_code(&error), exit::FAILED);
            let message = error.to_string();
            assert!(
                message.contains(MasterKeyId::of(&OLD).as_str()),
                "names the key's id: {message}"
            );
            assert!(message.contains("--previous-master-keys"), "{message}");
            assert!(!message.contains(&old), "{message}");
        }
        Err(other) => panic!("exit 70, naming the key: {other}"),
        Ok(running) => {
            running.stop().await;
            panic!("started without the key of a data key");
        }
    }
    let rewrapping = server(
        dir.path(),
        &["--master-key", &new, "--previous-master-keys", &old],
    );
    assert!(
        matches!(
            rewind_server::start(server(
                dir.path(),
                &["--master-key", &new, "--rewrap-data-keys"]
            ))
            .await,
            Err(StartupError::Config(_))
        ),
        "--rewrap-data-keys does not serve"
    );

    // The new key with the old one, which only decrypts: the server serves meanwhile.
    let running = rewind_server::start(rewrapping.clone()).await.unwrap();
    running.stop().await;
    assert_eq!(
        rewind_server::rewrap_data_keys(rewrapping.clone())
            .await
            .unwrap(),
        Rewrapped {
            changed: 1,
            master_key: MasterKeyId::of(&NEW),
        }
    );
    assert_eq!(
        rewind_server::rewrap_data_keys(rewrapping).await.unwrap(),
        Rewrapped {
            changed: 0,
            master_key: MasterKeyId::of(&NEW),
        },
        "nothing left to rewrap"
    );

    // The old key can go.
    let running = rewind_server::start(server(dir.path(), &["--master-key", &new]))
        .await
        .unwrap();
    running.stop().await;
    assert_eq!(read_with(dir.path(), NEW).await, Some(Secret::new("ghp_1")));
    let stale = rewind_server::start(server(dir.path(), &["--master-key", &old])).await;
    assert!(
        matches!(&stale, Err(StartupError::Setup(message)) if message.contains(MasterKeyId::of(&NEW).as_str())),
        "a server left with the old key refuses to start: {stale:?}"
    );
}
