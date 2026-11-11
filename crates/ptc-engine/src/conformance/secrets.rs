//! The conformance suite of `SecretStore`: the run's credentials override the toolset's, whole;
//! replacement; deletion; tenant isolation.

use std::collections::BTreeMap;

use ptc_core::{Digest, RunId};

use super::{check, tenant};
use crate::ports::{Credentials, Secret, SecretOwner, SecretStore, ServerCredentials};

/// Runs every check, each on a new store.
pub async fn run_all<S: SecretStore>(new_store: impl Fn() -> S) {
    check!(
        "secrets",
        new_store(),
        a_run_s_credentials_override_its_toolset_s
    );
    check!("secrets", new_store(), credentials_are_replaced_and_deleted);
    check!("secrets", new_store(), credentials_never_cross_tenants);
}

fn run() -> RunId {
    RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0001)
}

fn toolset() -> Digest {
    Digest::of(b"toolset")
}

fn credentials(token: &str) -> Credentials {
    Credentials {
        servers: BTreeMap::from([
            (
                "github".to_owned(),
                ServerCredentials {
                    headers: BTreeMap::from([(
                        "Authorization".to_owned(),
                        Secret::new(format!("Bearer {token}")),
                    )]),
                    env: BTreeMap::new(),
                },
            ),
            (
                "files".to_owned(),
                ServerCredentials {
                    headers: BTreeMap::new(),
                    env: BTreeMap::from([("FILES_TOKEN".to_owned(), Secret::new(token))]),
                },
            ),
        ]),
    }
}

pub async fn a_run_s_credentials_override_its_toolset_s<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    assert_eq!(
        store
            .resolve(&acme, run(), toolset())
            .await
            .expect("resolve"),
        None
    );
    store
        .put(
            &acme,
            &SecretOwner::Toolset(toolset()),
            &credentials("toolset"),
        )
        .await
        .expect("put");
    assert_eq!(
        store
            .resolve(&acme, run(), toolset())
            .await
            .expect("resolve"),
        Some(credentials("toolset"))
    );
    let only_github = Credentials {
        servers: BTreeMap::from([(
            "github".to_owned(),
            credentials("run").servers["github"].clone(),
        )]),
    };
    store
        .put(&acme, &SecretOwner::Run(run()), &only_github)
        .await
        .expect("put");
    assert_eq!(
        store
            .resolve(&acme, run(), toolset())
            .await
            .expect("resolve"),
        Some(only_github),
        "the run's credentials, whole, never merged with the toolset's"
    );
    let other_run = RunId::from_u128(2);
    assert_eq!(
        store
            .resolve(&acme, other_run, toolset())
            .await
            .expect("resolve"),
        Some(credentials("toolset"))
    );
    assert_eq!(
        store
            .resolve(&acme, other_run, Digest::of(b"other"))
            .await
            .expect("resolve"),
        None
    );
}

pub async fn credentials_are_replaced_and_deleted<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    let owner = SecretOwner::Toolset(toolset());
    store
        .put(&acme, &owner, &credentials("old"))
        .await
        .expect("put");
    store
        .put(&acme, &owner, &credentials("renewed"))
        .await
        .expect("put replaces");
    assert_eq!(
        store
            .resolve(&acme, run(), toolset())
            .await
            .expect("resolve"),
        Some(credentials("renewed"))
    );
    store
        .put(&acme, &SecretOwner::Run(run()), &credentials("run"))
        .await
        .expect("put");
    assert!(
        store
            .delete(&acme, &SecretOwner::Run(run()))
            .await
            .expect("delete")
    );
    assert!(
        !store
            .delete(&acme, &SecretOwner::Run(run()))
            .await
            .expect("delete")
    );
    assert_eq!(
        store
            .resolve(&acme, run(), toolset())
            .await
            .expect("resolve"),
        Some(credentials("renewed")),
        "back to the toolset's"
    );
    assert!(store.delete(&acme, &owner).await.expect("delete"));
    assert_eq!(
        store
            .resolve(&acme, run(), toolset())
            .await
            .expect("resolve"),
        None
    );
}

pub async fn credentials_never_cross_tenants<S: SecretStore>(store: &S) {
    store
        .put(
            &tenant("acme"),
            &SecretOwner::Toolset(toolset()),
            &credentials("acme"),
        )
        .await
        .expect("put");
    store
        .put(
            &tenant("acme"),
            &SecretOwner::Run(run()),
            &credentials("acme-run"),
        )
        .await
        .expect("put");
    assert_eq!(
        store
            .resolve(&tenant("other"), run(), toolset())
            .await
            .expect("resolve"),
        None
    );
    assert!(
        !store
            .delete(&tenant("other"), &SecretOwner::Toolset(toolset()))
            .await
            .expect("delete")
    );
    assert_eq!(
        store
            .resolve(&tenant("acme"), run(), toolset())
            .await
            .expect("resolve"),
        Some(credentials("acme-run"))
    );
}
