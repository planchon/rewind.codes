//! The memory control plane sends a `ControlPlaneChange` for every write that changes
//! something, as the stores of the control plane do in their transaction
//! (`archi/07-code-architecture.md`, "`Cluster` and `NodeLink`", "The other ports").

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use futures_util::{FutureExt, StreamExt};
use rewind_core::{Digest, SecretName, Timestamp};
use rewind_engine::adapters::{ChangeStream, ControlPlaneChanges, MemoryAccess, MemorySecrets};
use rewind_engine::ports::{
    AccessStore, ApiKey, ControlPlaneChange, Grant, NamespaceName, NewSecret, Principal, Role,
    Secret, SecretId, SecretStore, TenantId, Token, TokenKey,
};

fn tenant() -> TenantId {
    TenantId::new("acme").unwrap()
}

const NOW: Timestamp = Timestamp(1_767_225_600_000);

/// The changes sent since the last call.
fn sent(changes: &mut ChangeStream) -> Vec<ControlPlaneChange> {
    let mut sent = Vec::new();
    while let Some(Some(change)) = changes.next().now_or_never() {
        sent.push(change);
    }
    sent
}

#[tokio::test]
async fn every_write_of_access_sends_its_change() {
    let changes = ControlPlaneChanges::new();
    let mut stream = changes.subscribe();
    let store = MemoryAccess::with_changes(changes);
    let tenant = tenant();

    let key = ApiKey {
        hash: Digest::of(b"key"),
        tenant: tenant.clone(),
        name: "ci".into(),
        created_by: None,
        created_at: NOW,
        revoked_at: None,
    };
    store.put_key(&key).await.unwrap();
    assert!(store.revoke_key(key.hash, NOW).await.unwrap());
    assert!(
        !store.revoke_key(key.hash, NOW).await.unwrap(),
        "revoked once"
    );
    let key_changed = ControlPlaneChange::Key {
        tenant: tenant.clone(),
        hash: key.hash,
    };
    assert_eq!(sent(&mut stream), [key_changed.clone(), key_changed]);

    let role = Role {
        tenant: tenant.clone(),
        name: "reader".into(),
        permissions: Default::default(),
        description: String::new(),
        created_at: NOW,
        updated_at: NOW,
    };
    store.put_role(&role).await.unwrap();
    let user = Principal::User("user_1".into());
    let grant = Grant {
        tenant: tenant.clone(),
        principal: user.clone(),
        role: "reader".into(),
        allowed_labels: BTreeMap::new(),
        granted_by: None,
        granted_at: NOW,
    };
    store.put_grant(&grant, None).await.unwrap();
    store.set_default_role(&tenant, "reader").await.unwrap();
    assert_eq!(
        sent(&mut stream),
        [
            ControlPlaneChange::Role {
                tenant: tenant.clone(),
                name: "reader".into()
            },
            ControlPlaneChange::Grant {
                tenant: tenant.clone(),
                principal: user.clone()
            },
            ControlPlaneChange::Tenant {
                tenant: tenant.clone()
            },
        ]
    );
    store.delete_grant(&tenant, &user, None).await.unwrap();
    store.delete_role(&tenant, "reader").await.unwrap();
    assert_eq!(sent(&mut stream).len(), 2);

    let namespace = NamespaceName::new("alice").unwrap();
    let token = Token {
        hash: Digest::of(b"token"),
        tenant: tenant.clone(),
        namespace: namespace.clone(),
        permissions: Default::default(),
        labels: BTreeMap::new(),
        expires_at: Timestamp(NOW.0 + 1_000),
        created_by: Principal::User("user_1".into()),
        created_at: NOW,
        revoked_at: None,
    };
    store.put_token(&token).await.unwrap();
    assert_eq!(
        store.revoke_tokens(&tenant, &namespace, NOW).await.unwrap(),
        1
    );
    assert_eq!(
        store
            .purge_tokens(Timestamp(NOW.0 + 10_000), 10)
            .await
            .unwrap(),
        1
    );
    let token_changed = ControlPlaneChange::Token {
        tenant: tenant.clone(),
        hash: token.hash,
    };
    assert_eq!(
        sent(&mut stream),
        [token_changed.clone(), token_changed.clone(), token_changed]
    );

    let token_key = TokenKey {
        kid: "rk_1".into(),
        tenant: tenant.clone(),
        name: "backend".into(),
        jwk: "{}".into(),
        permissions: Default::default(),
        created_by: Principal::User("user_1".into()),
        created_at: NOW,
    };
    store.put_token_key(&token_key).await.unwrap();
    assert!(store.delete_token_key(&tenant, "rk_1").await.unwrap());
    assert_eq!(sent(&mut stream).len(), 2);

    // Reads send nothing.
    store.list_keys(&tenant).await.unwrap();
    store.list_grants(&tenant).await.unwrap();
    assert!(sent(&mut stream).is_empty());
}

#[tokio::test]
async fn a_secret_put_deleted_or_erased_with_its_namespace_sends_its_change() {
    let changes = ControlPlaneChanges::new();
    let mut stream = changes.subscribe();
    let store = MemorySecrets::with_changes(changes);
    let tenant = tenant();
    let alice = NamespaceName::new("alice").unwrap();
    let id = |name: &str| SecretId::of_namespace(alice.clone(), SecretName::new(name).unwrap());
    for name in ["github", "slack"] {
        store
            .put(
                &tenant,
                &NewSecret {
                    id: id(name),
                    value: Secret::new("value"),
                    labels: BTreeMap::new(),
                    by: "user:user_1".into(),
                },
                NOW,
            )
            .await
            .unwrap();
    }
    assert!(store.delete(&tenant, &id("github")).await.unwrap());
    assert_eq!(store.delete_namespace(&tenant, &alice).await.unwrap(), 1);
    let changed = |name: &str| ControlPlaneChange::Secret {
        tenant: tenant.clone(),
        secret: id(name),
    };
    assert_eq!(
        sent(&mut stream),
        [
            changed("github"),
            changed("slack"),
            changed("github"),
            changed("slack")
        ]
    );
    store.signing_key(&tenant, NOW).await.unwrap();
    assert!(sent(&mut stream).is_empty(), "signing keys send nothing");
}
