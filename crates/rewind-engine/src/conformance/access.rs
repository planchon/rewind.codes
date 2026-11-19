//! The conformance suite of `AccessStore`: API keys, roles and the role in use, one grant per
//! principal, the kept role's last holder, claims of an invitation's grant, default roles,
//! end users' tokens and token keys, tenant isolation, and people's connections: OAuth clients,
//! codes exchanged once, one connection per person and client, refresh tokens rotated once,
//! revocations and purges.

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{Digest, Timestamp};

use super::{T0, check, tenant};
use crate::ports::{
    Access, AccessStore, ApiKey, ClientAuthMethod, ClientKind, CodeRedemption, Connection,
    ConnectionToken, ConnectionTokenKind, Grant, GrantClaim, GrantDeletion, GrantWrite,
    NamespaceName, NewConnection, NewToken, OAuthClient, OAuthCode, OpenedOrganization, Principal,
    Role, RoleDeletion, Rotation, StoreError, TenantId, Token, TokenKey, UserId,
};

/// The role every kept-role check keeps, as the server does.
const OWNER: &str = "owner";

/// Runs every check, each on a new store.
pub async fn run_all<S: AccessStore>(new_store: impl Fn() -> S) {
    check!(
        "access",
        new_store(),
        keys_are_found_by_hash_and_revoked_once
    );
    check!(
        "access",
        new_store(),
        keys_are_listed_by_tenant_oldest_first
    );
    check!("access", new_store(), roles_are_stored_replaced_and_listed);
    check!("access", new_store(), a_role_in_use_is_not_deleted);
    check!("access", new_store(), a_principal_has_one_grant_per_tenant);
    check!(
        "access",
        new_store(),
        the_last_holder_of_the_kept_role_is_neither_demoted_nor_removed
    );
    check!(
        "access",
        new_store(),
        writes_that_keep_a_holder_or_change_no_holder_pass
    );
    check!(
        "access",
        new_store(),
        an_invitation_s_grant_is_claimed_by_its_user
    );
    check!(
        "access",
        new_store(),
        a_user_with_a_grant_keeps_it_and_the_invitation_s_is_deleted
    );
    check!(
        "access",
        new_store(),
        a_claim_without_grants_changes_nothing
    );
    check!("access", new_store(), default_roles_are_set_per_tenant);
    check!("access", new_store(), tenants_never_see_each_other);
    check!(
        "access",
        new_store(),
        tokens_are_found_by_hash_and_revoked_per_namespace
    );
    check!("access", new_store(), tokens_expired_or_revoked_are_purged);
    check!(
        "access",
        new_store(),
        token_keys_are_found_by_kid_and_deleted_per_tenant
    );
    check!(
        "access",
        new_store(),
        oauth_clients_are_replaced_counted_and_purged_unless_used
    );
    check!(
        "access",
        new_store(),
        a_code_is_redeemed_once_and_its_replay_revokes_the_connection
    );
    check!(
        "access",
        new_store(),
        a_person_has_one_connection_per_client
    );
    check!(
        "access",
        new_store(),
        a_refresh_token_rotates_once_and_its_replay_revokes_the_connection
    );
    check!(
        "access",
        new_store(),
        connections_are_listed_changed_touched_and_revoked
    );
    check!(
        "access",
        new_store(),
        codes_tokens_and_connections_are_purged
    );
}

fn acme() -> TenantId {
    tenant("acme")
}

fn at(seconds: u64) -> Timestamp {
    Timestamp(T0.0 + seconds * 1_000)
}

fn user(id: &str) -> Principal {
    Principal::User(format!("user_{id}"))
}

fn invitation(id: &str) -> Principal {
    Principal::Invitation(format!("invitation_{id}"))
}

fn key(secret: &str, tenant: TenantId, created_at: Timestamp) -> ApiKey {
    ApiKey {
        hash: Digest::of(secret.as_bytes()),
        tenant,
        name: format!("{secret}'s key"),
        created_by: Some(user("ada")),
        created_at,
        revoked_at: None,
    }
}

fn role(tenant: TenantId, name: &str, permissions: &[&str], at: Timestamp) -> Role {
    Role {
        tenant,
        name: name.to_owned(),
        description: format!("The role {name}."),
        permissions: permissions.iter().map(|&p| p.to_owned()).collect(),
        created_at: at,
        updated_at: at,
    }
}

fn grant(tenant: TenantId, principal: Principal, role: &str) -> Grant {
    Grant {
        tenant,
        principal,
        role: role.to_owned(),
        allowed_labels: BTreeMap::new(),
        granted_by: None,
        granted_at: T0,
    }
}

async fn put<S: AccessStore>(store: &S, grant: &Grant) {
    assert_eq!(
        store.put_grant(grant, None).await.expect("put_grant"),
        GrantWrite::Written
    );
}

async fn grant_of<S: AccessStore>(
    store: &S,
    tenant: &TenantId,
    principal: &Principal,
) -> Option<Grant> {
    store.grant(tenant, principal).await.expect("grant")
}

async fn principals<S: AccessStore>(store: &S, tenant: &TenantId) -> Vec<String> {
    store
        .list_grants(tenant)
        .await
        .expect("list_grants")
        .iter()
        .map(|grant| grant.principal.to_string())
        .collect()
}

pub async fn keys_are_found_by_hash_and_revoked_once<S: AccessStore>(store: &S) {
    let ada = key("rewind_sk_ada", acme(), T0);
    store.put_key(&ada).await.expect("put_key");
    assert!(
        matches!(store.put_key(&ada).await, Err(StoreError::Conflict(_))),
        "a hash is stored once"
    );
    assert_eq!(
        store.find_key(ada.hash).await.expect("find_key"),
        Some(ada.clone())
    );
    assert_eq!(
        store
            .find_key(Digest::of(b"rewind_sk_unknown"))
            .await
            .expect("find_key"),
        None
    );
    let operator = ApiKey {
        created_by: None,
        ..key("rewind_sk_operator", acme(), T0)
    };
    store.put_key(&operator).await.expect("put_key");
    assert_eq!(
        store.find_key(operator.hash).await.expect("find_key"),
        Some(operator)
    );

    assert!(store.revoke_key(ada.hash, at(5)).await.expect("revoke_key"));
    assert!(
        !store.revoke_key(ada.hash, at(6)).await.expect("revoke_key"),
        "already revoked"
    );
    assert!(
        !store
            .revoke_key(Digest::of(b"rewind_sk_unknown"), at(6))
            .await
            .expect("revoke_key")
    );
    let found = store
        .find_key(ada.hash)
        .await
        .expect("find_key")
        .expect("a revoked key is kept");
    assert_eq!(found.revoked_at, Some(at(5)), "the first revocation stays");
}

pub async fn keys_are_listed_by_tenant_oldest_first<S: AccessStore>(store: &S) {
    let second = key("rewind_sk_second", acme(), at(2));
    let first = key("rewind_sk_first", acme(), at(1));
    let theirs = key("rewind_sk_theirs", tenant("beta"), at(0));
    let same_time_a = key("rewind_sk_a", acme(), at(3));
    let same_time_b = key("rewind_sk_b", acme(), at(3));
    for key in [&second, &first, &theirs, &same_time_b, &same_time_a] {
        store.put_key(key).await.expect("put_key");
    }
    store
        .revoke_key(first.hash, at(4))
        .await
        .expect("revoke_key");
    let (earlier, later) = if same_time_a.hash < same_time_b.hash {
        (same_time_a, same_time_b)
    } else {
        (same_time_b, same_time_a)
    };
    let first = ApiKey {
        revoked_at: Some(at(4)),
        ..first
    };
    assert_eq!(
        store.list_keys(&acme()).await.expect("list_keys"),
        [first, second, earlier, later],
        "by creation time then hash, revoked keys included"
    );
    assert_eq!(
        store.list_keys(&tenant("beta")).await.expect("list_keys"),
        [theirs]
    );
    assert_eq!(
        store.list_keys(&tenant("empty")).await.expect("list_keys"),
        []
    );
}

pub async fn roles_are_stored_replaced_and_listed<S: AccessStore>(store: &S) {
    assert_eq!(
        store.get_role(&acme(), "triage").await.expect("get_role"),
        None
    );
    let triage = role(acme(), "triage", &["runs:read", "runs:control"], T0);
    let auditor = role(acme(), "auditor", &["runs:read"], at(1));
    store.put_role(&triage).await.expect("put_role");
    store.put_role(&auditor).await.expect("put_role");
    assert_eq!(
        store.get_role(&acme(), "triage").await.expect("get_role"),
        Some(triage.clone())
    );

    let changed = Role {
        description: "Triage, and erase.".into(),
        permissions: BTreeSet::from(["runs:read".to_owned(), "runs:delete".to_owned()]),
        created_at: at(9),
        updated_at: at(9),
        ..triage.clone()
    };
    store.put_role(&changed).await.expect("put_role replaces");
    let stored = store
        .get_role(&acme(), "triage")
        .await
        .expect("get_role")
        .expect("stored");
    assert_eq!(
        stored,
        Role {
            created_at: T0,
            ..changed
        },
        "a replaced role keeps its first created_at"
    );
    let names: Vec<String> = store
        .list_roles(&acme())
        .await
        .expect("list_roles")
        .into_iter()
        .map(|role| role.name)
        .collect();
    assert_eq!(names, ["auditor", "triage"], "by name");

    assert_eq!(
        store
            .delete_role(&acme(), "auditor")
            .await
            .expect("delete_role"),
        RoleDeletion::Deleted
    );
    assert_eq!(
        store
            .delete_role(&acme(), "auditor")
            .await
            .expect("delete_role"),
        RoleDeletion::NotFound
    );
    assert_eq!(
        store.get_role(&acme(), "auditor").await.expect("get_role"),
        None
    );
}

pub async fn a_role_in_use_is_not_deleted<S: AccessStore>(store: &S) {
    store
        .put_role(&role(acme(), "triage", &["runs:read"], T0))
        .await
        .expect("put_role");
    store
        .put_role(&role(tenant("beta"), "triage", &["runs:read"], T0))
        .await
        .expect("put_role");
    let held = grant(acme(), Principal::Key(Digest::of(b"rewind_sk_k")), "triage");
    put(store, &held).await;
    // Another tenant's grant of a role with the same name is not a use.
    put(store, &grant(tenant("beta"), user("bob"), "triage")).await;
    assert_eq!(
        store
            .delete_role(&acme(), "triage")
            .await
            .expect("delete_role"),
        RoleDeletion::InUse
    );
    assert!(
        store
            .get_role(&acme(), "triage")
            .await
            .expect("get_role")
            .is_some(),
        "nothing deleted"
    );
    assert_eq!(
        store
            .delete_grant(&acme(), &held.principal, None)
            .await
            .expect("delete_grant"),
        GrantDeletion::Deleted
    );
    assert_eq!(
        store
            .delete_role(&acme(), "triage")
            .await
            .expect("delete_role"),
        RoleDeletion::Deleted
    );
}

pub async fn a_principal_has_one_grant_per_tenant<S: AccessStore>(store: &S) {
    let ada = user("ada");
    assert_eq!(grant_of(store, &acme(), &ada).await, None);
    let first = Grant {
        allowed_labels: BTreeMap::from([("team".to_owned(), "support".to_owned())]),
        granted_by: Some(user("owner")),
        ..grant(acme(), ada.clone(), "reader")
    };
    put(store, &first).await;
    assert_eq!(grant_of(store, &acme(), &ada).await, Some(first));
    let second = Grant {
        granted_at: at(3),
        ..grant(acme(), ada.clone(), "harness")
    };
    put(store, &second).await;
    assert_eq!(
        grant_of(store, &acme(), &ada).await,
        Some(second.clone()),
        "replaced, labels included"
    );
    let theirs = grant(tenant("beta"), ada.clone(), "worker");
    put(store, &theirs).await;
    assert_eq!(grant_of(store, &acme(), &ada).await, Some(second));
    assert_eq!(grant_of(store, &tenant("beta"), &ada).await, Some(theirs));

    put(store, &grant(acme(), invitation("i1"), "reader")).await;
    put(store, &grant(acme(), user("bob"), "reader")).await;
    let key = Principal::Key(Digest::of(b"rewind_sk_k"));
    put(store, &grant(acme(), key.clone(), "worker")).await;
    assert_eq!(
        principals(store, &acme()).await,
        [
            "invitation:invitation_i1".to_owned(),
            key.to_string(),
            "user:user_ada".to_owned(),
            "user:user_bob".to_owned(),
        ],
        "by principal, as their text sorts"
    );

    assert_eq!(
        store
            .delete_grant(&acme(), &ada, None)
            .await
            .expect("delete_grant"),
        GrantDeletion::Deleted
    );
    assert_eq!(
        store
            .delete_grant(&acme(), &ada, None)
            .await
            .expect("delete_grant"),
        GrantDeletion::NotFound
    );
    assert_eq!(grant_of(store, &acme(), &ada).await, None);
    assert!(
        grant_of(store, &tenant("beta"), &ada).await.is_some(),
        "the other tenant's grant stays"
    );
}

pub async fn the_last_holder_of_the_kept_role_is_neither_demoted_nor_removed<S: AccessStore>(
    store: &S,
) {
    let ada = user("ada");
    let bob = user("bob");
    let owner = grant(acme(), ada.clone(), OWNER);
    assert_eq!(
        store
            .put_grant(&owner, Some(OWNER))
            .await
            .expect("put_grant"),
        GrantWrite::Written
    );
    // Another tenant's owner does not count.
    put(store, &grant(tenant("beta"), bob.clone(), OWNER)).await;

    let demoted = grant(acme(), ada.clone(), "reader");
    assert_eq!(
        store
            .put_grant(&demoted, Some(OWNER))
            .await
            .expect("put_grant"),
        GrantWrite::LastHolder
    );
    assert_eq!(
        store
            .delete_grant(&acme(), &ada, Some(OWNER))
            .await
            .expect("delete_grant"),
        GrantDeletion::LastHolder
    );
    assert_eq!(
        grant_of(store, &acme(), &ada).await,
        Some(owner.clone()),
        "nothing changed"
    );
    // Rewriting the owner as owner, with other labels, keeps a holder.
    let narrowed = Grant {
        allowed_labels: BTreeMap::from([("env".to_owned(), "prod".to_owned())]),
        ..owner.clone()
    };
    assert_eq!(
        store
            .put_grant(&narrowed, Some(OWNER))
            .await
            .expect("put_grant"),
        GrantWrite::Written
    );

    // A second owner makes it possible.
    put(store, &grant(acme(), bob.clone(), OWNER)).await;
    assert_eq!(
        store
            .put_grant(&demoted, Some(OWNER))
            .await
            .expect("put_grant"),
        GrantWrite::Written
    );
    assert_eq!(
        store
            .delete_grant(&acme(), &bob, Some(OWNER))
            .await
            .expect("delete_grant"),
        GrantDeletion::LastHolder,
        "bob is now the last owner"
    );
    assert_eq!(grant_of(store, &acme(), &ada).await, Some(demoted));

    // Without `keep`, nothing is kept.
    assert_eq!(
        store
            .delete_grant(&acme(), &bob, None)
            .await
            .expect("delete_grant"),
        GrantDeletion::Deleted
    );
}

pub async fn writes_that_keep_a_holder_or_change_no_holder_pass<S: AccessStore>(store: &S) {
    // A tenant without any owner (its keys made by the operator) still gets grants.
    let key = Principal::Key(Digest::of(b"rewind_sk_k"));
    assert_eq!(
        store
            .put_grant(&grant(acme(), key.clone(), "harness"), Some(OWNER))
            .await
            .expect("put_grant"),
        GrantWrite::Written
    );
    assert_eq!(
        store
            .delete_grant(&acme(), &key, Some(OWNER))
            .await
            .expect("delete_grant"),
        GrantDeletion::Deleted
    );
    assert_eq!(
        store
            .delete_grant(&acme(), &key, Some(OWNER))
            .await
            .expect("delete_grant"),
        GrantDeletion::NotFound
    );

    // Only members hold the kept role: a pending invitation or a key of that role does not.
    let pending = invitation("i1");
    let owner_key = Principal::Key(Digest::of(b"rewind_sk_owner"));
    put(store, &grant(acme(), user("ada"), OWNER)).await;
    put(store, &grant(acme(), pending.clone(), OWNER)).await;
    put(store, &grant(acme(), owner_key.clone(), OWNER)).await;
    assert_eq!(
        store
            .delete_grant(&acme(), &user("ada"), Some(OWNER))
            .await
            .expect("delete_grant"),
        GrantDeletion::LastHolder
    );
    assert_eq!(
        store
            .put_grant(&grant(acme(), user("ada"), "reader"), Some(OWNER))
            .await
            .expect("put_grant"),
        GrantWrite::LastHolder
    );
    for principal in [&pending, &owner_key] {
        assert_eq!(
            store
                .delete_grant(&acme(), principal, Some(OWNER))
                .await
                .expect("delete_grant"),
            GrantDeletion::Deleted,
            "{principal} is not a member"
        );
    }
    // A grant of another role is never held back.
    put(store, &grant(acme(), user("bob"), "reader")).await;
    assert_eq!(
        store
            .delete_grant(&acme(), &user("bob"), Some(OWNER))
            .await
            .expect("delete_grant"),
        GrantDeletion::Deleted
    );
}

pub async fn an_invitation_s_grant_is_claimed_by_its_user<S: AccessStore>(store: &S) {
    let pending = Grant {
        allowed_labels: BTreeMap::from([("team".to_owned(), "support".to_owned())]),
        granted_by: Some(user("owner")),
        granted_at: at(1),
        ..grant(acme(), invitation("i1"), "triage")
    };
    put(store, &pending).await;
    // The same invitation's principal in another tenant is not claimed.
    put(store, &grant(tenant("beta"), invitation("i1"), "reader")).await;
    let claimed = Grant {
        principal: user("ada"),
        ..pending
    };
    assert_eq!(
        store
            .claim_grant(&acme(), &invitation("i1"), &user("ada"))
            .await
            .expect("claim_grant"),
        GrantClaim::Claimed(claimed.clone())
    );
    assert_eq!(grant_of(store, &acme(), &user("ada")).await, Some(claimed));
    assert_eq!(grant_of(store, &acme(), &invitation("i1")).await, None);
    assert!(
        grant_of(store, &tenant("beta"), &invitation("i1"))
            .await
            .is_some()
    );
    assert_eq!(
        store
            .claim_grant(&acme(), &invitation("i1"), &user("ada"))
            .await
            .expect("claim_grant"),
        GrantClaim::AlreadyGranted(
            grant_of(store, &acme(), &user("ada"))
                .await
                .expect("claimed")
        ),
        "a second claim finds the grant already there"
    );
}

pub async fn a_user_with_a_grant_keeps_it_and_the_invitation_s_is_deleted<S: AccessStore>(
    store: &S,
) {
    let existing = grant(acme(), user("ada"), "reader");
    put(store, &existing).await;
    put(store, &grant(acme(), invitation("i1"), "harness")).await;
    assert_eq!(
        store
            .claim_grant(&acme(), &invitation("i1"), &user("ada"))
            .await
            .expect("claim_grant"),
        GrantClaim::AlreadyGranted(existing.clone())
    );
    assert_eq!(grant_of(store, &acme(), &user("ada")).await, Some(existing));
    assert_eq!(grant_of(store, &acme(), &invitation("i1")).await, None);
}

pub async fn a_claim_without_grants_changes_nothing<S: AccessStore>(store: &S) {
    put(store, &grant(acme(), user("bob"), OWNER)).await;
    assert_eq!(
        store
            .claim_grant(&acme(), &invitation("i1"), &user("ada"))
            .await
            .expect("claim_grant"),
        GrantClaim::NoGrant
    );
    assert_eq!(grant_of(store, &acme(), &user("ada")).await, None);
    assert_eq!(
        principals(store, &acme()).await,
        ["user:user_bob".to_owned()]
    );
}

pub async fn default_roles_are_set_per_tenant<S: AccessStore>(store: &S) {
    assert_eq!(
        store.default_role(&acme()).await.expect("default_role"),
        None
    );
    store
        .set_default_role(&acme(), "harness")
        .await
        .expect("set_default_role");
    store
        .set_default_role(&acme(), "triage")
        .await
        .expect("set_default_role replaces");
    assert_eq!(
        store.default_role(&acme()).await.expect("default_role"),
        Some("triage".to_owned())
    );
    assert_eq!(
        store
            .default_role(&tenant("beta"))
            .await
            .expect("default_role"),
        None
    );
}

pub async fn tenants_never_see_each_other<S: AccessStore>(store: &S) {
    let beta = tenant("beta");
    let ada = user("ada");
    store
        .put_key(&key("rewind_sk_acme", acme(), T0))
        .await
        .expect("put_key");
    store
        .put_role(&role(acme(), "triage", &["runs:read"], T0))
        .await
        .expect("put_role");
    put(store, &grant(acme(), ada.clone(), "triage")).await;
    put(store, &grant(acme(), invitation("i1"), "reader")).await;
    store
        .set_default_role(&acme(), "triage")
        .await
        .expect("set_default_role");

    assert_eq!(store.list_keys(&beta).await.expect("list_keys"), []);
    assert_eq!(
        store.get_role(&beta, "triage").await.expect("get_role"),
        None
    );
    assert_eq!(store.list_roles(&beta).await.expect("list_roles"), []);
    assert_eq!(
        store
            .delete_role(&beta, "triage")
            .await
            .expect("delete_role"),
        RoleDeletion::NotFound
    );
    assert_eq!(grant_of(store, &beta, &ada).await, None);
    assert_eq!(store.list_grants(&beta).await.expect("list_grants"), []);
    assert_eq!(
        store
            .delete_grant(&beta, &ada, None)
            .await
            .expect("delete_grant"),
        GrantDeletion::NotFound
    );
    assert_eq!(
        store
            .claim_grant(&beta, &invitation("i1"), &user("bob"))
            .await
            .expect("claim_grant"),
        GrantClaim::NoGrant
    );
    assert_eq!(store.default_role(&beta).await.expect("default_role"), None);

    assert_eq!(
        principals(store, &acme()).await.len(),
        2,
        "acme's are intact"
    );
    assert!(
        store
            .get_role(&acme(), "triage")
            .await
            .expect("get_role")
            .is_some()
    );
}

fn token(secret: &str, tenant: TenantId, namespace: &str, expires_at: Timestamp) -> Token {
    Token {
        hash: Digest::of(secret.as_bytes()),
        tenant,
        namespace: NamespaceName::new(namespace).expect("the suites use valid namespace names"),
        permissions: BTreeSet::from(["runs:read".to_owned(), "runs:start".to_owned()]),
        labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
        expires_at,
        created_by: Principal::Key(Digest::of(b"rewind_sk_minter")),
        created_at: at(0),
        revoked_at: None,
    }
}

pub async fn tokens_are_found_by_hash_and_revoked_per_namespace<S: AccessStore>(store: &S) {
    let alice = NamespaceName::new("alice").expect("valid");
    let first = token("rewind_nt_1", acme(), "alice", at(3_600));
    let second = token("rewind_nt_2", acme(), "alice", at(60));
    let bobs = token("rewind_nt_3", acme(), "bob", at(3_600));
    let others = token("rewind_nt_4", tenant("other"), "alice", at(3_600));
    for written in [&first, &second, &bobs, &others] {
        store.put_token(written).await.expect("put_token");
    }
    assert!(
        matches!(store.put_token(&first).await, Err(StoreError::Conflict(_))),
        "a hash is stored once"
    );
    assert_eq!(
        store.find_token(first.hash).await.expect("find_token"),
        Some(first.clone())
    );
    assert_eq!(
        store
            .find_token(Digest::of(b"unknown"))
            .await
            .expect("find_token"),
        None
    );
    assert_eq!(
        store
            .count_tokens(&acme(), &alice, at(30))
            .await
            .expect("count_tokens"),
        2
    );
    assert_eq!(
        store
            .count_tokens(&acme(), &alice, at(60))
            .await
            .expect("count_tokens"),
        1,
        "a token expired at its expiry is not live"
    );
    assert!(
        !store
            .revoke_token(&tenant("other"), first.hash, at(10))
            .await
            .expect("revoke_token"),
        "another tenant's token is not revoked"
    );
    assert!(
        store
            .revoke_token(&acme(), first.hash, at(10))
            .await
            .expect("revoke_token")
    );
    assert!(
        !store
            .revoke_token(&acme(), first.hash, at(11))
            .await
            .expect("revoke_token"),
        "a token is revoked once"
    );
    let revoked = store
        .find_token(first.hash)
        .await
        .expect("find_token")
        .expect("a revoked token is still found");
    assert_eq!(revoked.revoked_at, Some(at(10)));
    assert!(!revoked.is_live(at(20)));
    assert_eq!(
        store
            .revoke_tokens(&acme(), &alice, at(20))
            .await
            .expect("revoke_tokens"),
        1,
        "the namespace's tokens not yet revoked, expired or not"
    );
    assert_eq!(
        store
            .find_token(second.hash)
            .await
            .expect("find_token")
            .and_then(|token| token.revoked_at),
        Some(at(20))
    );
    assert_eq!(
        store
            .count_tokens(&acme(), &alice, at(30))
            .await
            .expect("count_tokens"),
        0
    );
    for untouched in [&bobs, &others] {
        assert_eq!(
            store
                .find_token(untouched.hash)
                .await
                .expect("find_token")
                .and_then(|token| token.revoked_at),
            None,
            "other namespaces and tenants keep their tokens"
        );
    }
}

pub async fn tokens_expired_or_revoked_are_purged<S: AccessStore>(store: &S) {
    let live = token("rewind_nt_live", acme(), "alice", at(3_600));
    let expired = token("rewind_nt_expired", acme(), "alice", at(60));
    let just_expired = token("rewind_nt_just", acme(), "alice", at(100));
    let revoked = token("rewind_nt_revoked", tenant("other"), "bob", at(3_600));
    let revoked_late = token("rewind_nt_late", acme(), "bob", at(3_600));
    for written in [&live, &expired, &just_expired, &revoked, &revoked_late] {
        store.put_token(written).await.expect("put_token");
    }
    store
        .revoke_token(&tenant("other"), revoked.hash, at(50))
        .await
        .expect("revoke_token");
    store
        .revoke_token(&acme(), revoked_late.hash, at(100))
        .await
        .expect("revoke_token");
    assert_eq!(
        store.purge_tokens(at(100), 1).await.expect("purge_tokens"),
        1,
        "at most the limit"
    );
    assert_eq!(
        store.purge_tokens(at(100), 10).await.expect("purge_tokens"),
        1,
        "the rest, in every tenant"
    );
    assert_eq!(
        store.purge_tokens(at(100), 10).await.expect("purge_tokens"),
        0
    );
    for (token, kept) in [
        (&live, true),
        (&expired, false),
        (&just_expired, true),
        (&revoked, false),
        (&revoked_late, true),
    ] {
        assert_eq!(
            store
                .find_token(token.hash)
                .await
                .expect("find_token")
                .is_some(),
            kept,
            "only the tokens expired or revoked before the time are deleted"
        );
    }
}

fn token_key(kid: &str, tenant: TenantId, created_at: Timestamp) -> TokenKey {
    TokenKey {
        kid: kid.to_owned(),
        tenant,
        name: format!("key {kid}"),
        jwk: format!(r#"{{"kty":"OKP","crv":"Ed25519","x":"{kid}"}}"#),
        permissions: BTreeSet::from(["runs:read".to_owned()]),
        created_by: user("ada"),
        created_at,
    }
}

pub async fn token_keys_are_found_by_kid_and_deleted_per_tenant<S: AccessStore>(store: &S) {
    let late = token_key("rk_b", acme(), at(2));
    let early = token_key("rk_c", acme(), at(1));
    let same_time = token_key("rk_a", acme(), at(2));
    let others = token_key("rk_d", tenant("other"), at(0));
    for written in [&late, &early, &same_time, &others] {
        store.put_token_key(written).await.expect("put_token_key");
    }
    assert!(
        matches!(
            store.put_token_key(&late).await,
            Err(StoreError::Conflict(_))
        ),
        "a kid is stored once"
    );
    assert_eq!(
        store.find_token_key("rk_d").await.expect("find_token_key"),
        Some(others.clone()),
        "a kid is found in any tenant"
    );
    assert_eq!(
        store.find_token_key("rk_x").await.expect("find_token_key"),
        None
    );
    assert_eq!(
        store
            .list_token_keys(&acme())
            .await
            .expect("list_token_keys"),
        [early.clone(), same_time.clone(), late.clone()],
        "by creation, then kid"
    );
    assert!(
        !store
            .delete_token_key(&acme(), "rk_d")
            .await
            .expect("delete_token_key"),
        "another tenant's key is not deleted"
    );
    assert!(
        store
            .delete_token_key(&acme(), "rk_b")
            .await
            .expect("delete_token_key")
    );
    assert!(
        !store
            .delete_token_key(&acme(), "rk_b")
            .await
            .expect("delete_token_key")
    );
    assert_eq!(
        store
            .list_token_keys(&acme())
            .await
            .expect("list_token_keys"),
        [early, same_time]
    );
    assert_eq!(
        store
            .list_token_keys(&tenant("other"))
            .await
            .expect("list_token_keys"),
        [others]
    );
}

// -------------------------------------------------------------------------------------------
// Connections

const CLAUDE: &str = "https://claude.ai/oauth/claude-code-client-metadata";

fn client(id: &str, kind: ClientKind, created_at: Timestamp) -> OAuthClient {
    OAuthClient {
        id: id.to_owned(),
        kind,
        name: format!("client {id}"),
        uri: Some("https://example.net".into()),
        logo_uri: None,
        redirect_uris: vec![
            "https://claude.ai/api/mcp/auth_callback".into(),
            "http://localhost/callback".into(),
        ],
        auth_method: match kind {
            ClientKind::MetadataDocument => ClientAuthMethod::None,
            ClientKind::Registered => ClientAuthMethod::ClientSecretBasic,
        },
        secret_hash: match kind {
            ClientKind::MetadataDocument => None,
            ClientKind::Registered => Some(Digest::of(b"rewind_cs_secret")),
        },
        fetched_at: (kind == ClientKind::MetadataDocument).then_some(created_at),
        expires_at: (kind == ClientKind::MetadataDocument).then_some(at(3_600)),
        created_at,
        last_used_at: None,
    }
}

fn person(id: &str) -> UserId {
    UserId::new(format!("user_{id}"))
}

fn opened(tenant: TenantId, access: Access, labels: &[(&str, &str)]) -> OpenedOrganization {
    OpenedOrganization {
        tenant,
        access,
        allowed_labels: labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
    }
}

fn code(secret: &str, user: &UserId, client: &str, expires_at: Timestamp) -> OAuthCode {
    OAuthCode {
        hash: Digest::of(secret.as_bytes()),
        client_id: client.to_owned(),
        user: user.clone(),
        redirect_uri: "https://claude.ai/api/mcp/auth_callback".into(),
        code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".into(),
        resource: "https://api.rewind.test/mcp".into(),
        organizations: BTreeMap::from([(acme(), Access::Run), (tenant("beta"), Access::Read)]),
        labels: BTreeMap::from([("via".to_owned(), "claude".to_owned())]),
        expires_at,
        used_at: None,
        connection: None,
    }
}

fn issued(secret: &str, expires_at: Timestamp) -> NewToken {
    NewToken {
        hash: Digest::of(secret.as_bytes()),
        expires_at,
    }
}

/// A connection `id` of `user` through `client`, opening acme (`run`) and beta (`read`), with
/// the tokens `{id}-access` and `{id}-refresh`.
fn new_connection(id: &str, user: &UserId, client: &str, created_at: Timestamp) -> NewConnection {
    NewConnection {
        connection: Connection {
            id: id.to_owned(),
            user: user.clone(),
            client_id: client.to_owned(),
            client_name: "Claude".into(),
            created_at,
            last_used_at: None,
            revoked_at: None,
            expires_at: Timestamp(created_at.0 + 30 * 86_400_000),
        },
        organizations: vec![
            opened(acme(), Access::Run, &[("via", "claude")]),
            opened(tenant("beta"), Access::Read, &[]),
        ],
        access: issued(&format!("{id}-access"), Timestamp(created_at.0 + 3_600_000)),
        refresh: issued(
            &format!("{id}-refresh"),
            Timestamp(created_at.0 + 30 * 86_400_000),
        ),
    }
}

/// Puts a code and exchanges it for `new`.
async fn redeemed<S: AccessStore>(store: &S, secret: &str, new: &NewConnection) -> Connection {
    let code = code(
        secret,
        &new.connection.user,
        &new.connection.client_id,
        at(300),
    );
    store.put_oauth_code(&code).await.expect("put_oauth_code");
    match store
        .redeem_oauth_code(code.hash, new, new.connection.created_at)
        .await
        .expect("redeem_oauth_code")
    {
        CodeRedemption::Redeemed(connection) => connection,
        other => panic!("not redeemed: {other:?}"),
    }
}

async fn token_of<S: AccessStore>(store: &S, secret: &str) -> Option<ConnectionToken> {
    store
        .connection_token(Digest::of(secret.as_bytes()))
        .await
        .expect("connection_token")
}

async fn tenants_opened<S: AccessStore>(store: &S, id: &str) -> Vec<(String, String)> {
    store
        .connection_grants(id)
        .await
        .expect("connection_grants")
        .into_iter()
        .map(|grant| (grant.tenant.to_string(), grant.role))
        .collect()
}

pub async fn oauth_clients_are_replaced_counted_and_purged_unless_used<S: AccessStore>(store: &S) {
    let claude = client(CLAUDE, ClientKind::MetadataDocument, T0);
    let registered = client("rc_one", ClientKind::Registered, T0);
    let fresh = client("rc_fresh", ClientKind::Registered, at(100));
    for written in [&claude, &registered, &fresh] {
        store
            .put_oauth_client(written)
            .await
            .expect("put_oauth_client");
    }
    assert_eq!(
        store.oauth_client(CLAUDE).await.expect("oauth_client"),
        Some(claude.clone())
    );
    assert_eq!(
        store.oauth_client("rc_none").await.expect("oauth_client"),
        None
    );
    assert_eq!(
        store
            .count_oauth_clients(ClientKind::Registered)
            .await
            .expect("count_oauth_clients"),
        2
    );
    assert_eq!(
        store
            .count_oauth_clients(ClientKind::MetadataDocument)
            .await
            .expect("count_oauth_clients"),
        1
    );

    // Fetched again: the new copy, under the first creation.
    let refetched = OAuthClient {
        name: "Claude Code".into(),
        redirect_uris: vec!["http://localhost/callback".into()],
        fetched_at: Some(at(50)),
        expires_at: Some(at(3_650)),
        created_at: at(50),
        ..claude.clone()
    };
    store
        .put_oauth_client(&refetched)
        .await
        .expect("put_oauth_client replaces");
    assert_eq!(
        store.oauth_client(CLAUDE).await.expect("oauth_client"),
        Some(OAuthClient {
            created_at: T0,
            ..refetched
        }),
        "a replaced client keeps its first created_at"
    );

    // A connection through `rc_one` keeps it; Claude's copy was fetched at 50.
    redeemed(
        store,
        "code",
        &new_connection("cn_1", &person("ada"), "rc_one", at(10)),
    )
    .await;
    assert_eq!(
        store
            .oauth_client("rc_one")
            .await
            .expect("oauth_client")
            .and_then(|client| client.last_used_at),
        Some(at(10)),
        "its last use is the exchange"
    );
    assert_eq!(
        store
            .purge_oauth_clients(at(60), 10)
            .await
            .expect("purge_oauth_clients"),
        1,
        "only Claude's copy, fetched at 50 and used by no connection"
    );
    assert_eq!(
        store.oauth_client(CLAUDE).await.expect("oauth_client"),
        None
    );
    assert!(
        store
            .oauth_client("rc_one")
            .await
            .expect("oauth_client")
            .is_some()
    );
    assert!(
        store
            .oauth_client("rc_fresh")
            .await
            .expect("oauth_client")
            .is_some(),
        "created after the bound"
    );
    store
        .revoke_connection("cn_1", at(20))
        .await
        .expect("revoke_connection");
    assert_eq!(
        store
            .purge_oauth_clients(at(200), 10)
            .await
            .expect("purge_oauth_clients"),
        1,
        "a revoked connection still uses its client, until it is purged"
    );
    assert!(
        store
            .oauth_client("rc_one")
            .await
            .expect("oauth_client")
            .is_some()
    );
    assert_eq!(
        store.oauth_client("rc_fresh").await.expect("oauth_client"),
        None
    );
}

pub async fn a_code_is_redeemed_once_and_its_replay_revokes_the_connection<S: AccessStore>(
    store: &S,
) {
    store
        .put_oauth_client(&client(CLAUDE, ClientKind::MetadataDocument, T0))
        .await
        .expect("put_oauth_client");
    let ada = person("ada");
    let issued = code("the-code", &ada, CLAUDE, at(300));
    store.put_oauth_code(&issued).await.expect("put_oauth_code");
    assert!(
        matches!(
            store.put_oauth_code(&issued).await,
            Err(StoreError::Conflict(_))
        ),
        "a hash is stored once"
    );
    assert_eq!(
        store.oauth_code(issued.hash).await.expect("oauth_code"),
        Some(issued.clone())
    );
    assert_eq!(
        store
            .redeem_oauth_code(
                Digest::of(b"unknown"),
                &new_connection("cn_x", &ada, CLAUDE, at(1)),
                at(1)
            )
            .await
            .expect("redeem_oauth_code"),
        CodeRedemption::Unknown
    );
    assert_eq!(
        store.connection("cn_x").await.expect("connection"),
        None,
        "nothing written"
    );

    let new = new_connection("cn_1", &ada, CLAUDE, at(1));
    let connection = match store
        .redeem_oauth_code(issued.hash, &new, at(1))
        .await
        .expect("redeem_oauth_code")
    {
        CodeRedemption::Redeemed(connection) => connection,
        other => panic!("not redeemed: {other:?}"),
    };
    assert_eq!(connection, new.connection);
    assert_eq!(
        store.connection("cn_1").await.expect("connection"),
        Some(new.connection.clone())
    );
    let used = store
        .oauth_code(issued.hash)
        .await
        .expect("oauth_code")
        .expect("kept");
    assert_eq!(
        (used.used_at, used.connection.as_deref()),
        (Some(at(1)), Some("cn_1"))
    );
    let grants = store
        .connection_grants("cn_1")
        .await
        .expect("connection_grants");
    assert_eq!(
        grants,
        [
            Grant {
                tenant: acme(),
                principal: Principal::Connection("cn_1".into()),
                role: "harness".into(),
                allowed_labels: BTreeMap::from([("via".to_owned(), "claude".to_owned())]),
                granted_by: Some(Principal::User("user_ada".into())),
                granted_at: at(1),
            },
            Grant {
                tenant: tenant("beta"),
                principal: Principal::Connection("cn_1".into()),
                role: "reader".into(),
                allowed_labels: BTreeMap::new(),
                granted_by: Some(Principal::User("user_ada".into())),
                granted_at: at(1),
            },
        ],
        "a grant per organization, by tenant"
    );
    assert_eq!(
        grant_of(store, &acme(), &Principal::Connection("cn_1".into())).await,
        Some(grants[0].clone()),
        "an ordinary grant of the tenant"
    );
    assert_eq!(
        token_of(store, "cn_1-access").await,
        Some(ConnectionToken {
            hash: Digest::of(b"cn_1-access"),
            connection: "cn_1".into(),
            kind: ConnectionTokenKind::Access,
            expires_at: at(3_601),
            created_at: at(1),
            used_at: None,
        })
    );
    assert_eq!(
        token_of(store, "cn_1-refresh")
            .await
            .map(|token| token.kind),
        Some(ConnectionTokenKind::Refresh)
    );

    // Exchanged again: nothing new, and the connection is revoked.
    assert_eq!(
        store
            .redeem_oauth_code(
                issued.hash,
                &new_connection("cn_2", &person("ada"), CLAUDE, at(2)),
                at(2)
            )
            .await
            .expect("redeem_oauth_code"),
        CodeRedemption::Used
    );
    assert_eq!(store.connection("cn_2").await.expect("connection"), None);
    let revoked = store
        .connection("cn_1")
        .await
        .expect("connection")
        .expect("a revoked connection is kept");
    assert_eq!(revoked.revoked_at, Some(at(2)));
    assert_eq!(tenants_opened(store, "cn_1").await, []);
    assert_eq!(grant_of(store, &acme(), &revoked.principal()).await, None);
    assert_eq!(token_of(store, "cn_1-access").await, None);
    assert_eq!(token_of(store, "cn_1-refresh").await, None);
    assert_eq!(
        store.connections_of(&ada).await.expect("connections_of"),
        []
    );
}

pub async fn a_person_has_one_connection_per_client<S: AccessStore>(store: &S) {
    let ada = person("ada");
    let first = redeemed(store, "first", &new_connection("cn_1", &ada, CLAUDE, at(1))).await;
    // Ada consents again, on another device: the same connection, its organizations replaced.
    let mut again = new_connection("cn_2", &ada, CLAUDE, at(5));
    again.organizations = vec![opened(tenant("gamma"), Access::Read, &[])];
    let kept = redeemed(store, "second", &again).await;
    assert_eq!(
        kept,
        Connection {
            expires_at: again.connection.expires_at,
            ..first.clone()
        },
        "the live connection is kept, its lapse renewed"
    );
    assert_eq!(store.connection("cn_2").await.expect("connection"), None);
    assert_eq!(
        tenants_opened(store, "cn_1").await,
        [("gamma".to_owned(), "reader".to_owned())]
    );
    for secret in ["cn_1-access", "cn_1-refresh", "cn_2-access", "cn_2-refresh"] {
        assert_eq!(
            token_of(store, secret).await.map(|token| token.connection),
            Some("cn_1".to_owned()),
            "{secret}: the tokens of both devices are the connection's"
        );
    }
    // Another client, and another person: other connections.
    let other_client = redeemed(
        store,
        "third",
        &new_connection("cn_3", &ada, "rc_cursor", at(6)),
    )
    .await;
    assert_eq!(other_client.id, "cn_3");
    let grace = redeemed(
        store,
        "fourth",
        &new_connection("cn_4", &person("grace"), CLAUDE, at(7)),
    )
    .await;
    assert_eq!(grace.id, "cn_4");
    assert_eq!(
        store
            .connections_of(&ada)
            .await
            .expect("connections_of")
            .iter()
            .map(|connection| connection.id.clone())
            .collect::<Vec<_>>(),
        ["cn_1", "cn_3"]
    );
    // Once revoked, a new consent makes a new connection.
    assert!(
        store
            .revoke_connection("cn_1", at(8))
            .await
            .expect("revoke_connection")
    );
    let renewed = redeemed(store, "fifth", &new_connection("cn_5", &ada, CLAUDE, at(9))).await;
    assert_eq!(renewed.id, "cn_5");
}

pub async fn a_refresh_token_rotates_once_and_its_replay_revokes_the_connection<S: AccessStore>(
    store: &S,
) {
    let ada = person("ada");
    redeemed(store, "code", &new_connection("cn_1", &ada, CLAUDE, at(1))).await;
    let refresh = Digest::of(b"cn_1-refresh");
    let lapse = at(40 * 86_400);
    assert_eq!(
        store
            .rotate_refresh_token(
                Digest::of(b"cn_1-access"),
                &issued("a2", at(3_700)),
                &issued("r2", lapse),
                at(100)
            )
            .await
            .expect("rotate_refresh_token"),
        Rotation::Unknown,
        "an access token is not a refresh token"
    );
    let rotated = store
        .rotate_refresh_token(
            refresh,
            &issued("a2", at(3_700)),
            &issued("r2", lapse),
            at(100),
        )
        .await
        .expect("rotate_refresh_token");
    let Rotation::Rotated(connection) = rotated else {
        panic!("not rotated: {rotated:?}");
    };
    assert_eq!(
        (connection.id.as_str(), connection.expires_at),
        ("cn_1", lapse),
        "the connection lapses with its new refresh token"
    );
    assert_eq!(
        store
            .connection("cn_1")
            .await
            .expect("connection")
            .map(|connection| connection.expires_at),
        Some(lapse)
    );
    assert_eq!(
        token_of(store, "cn_1-refresh")
            .await
            .and_then(|token| token.used_at),
        Some(at(100))
    );
    assert_eq!(
        token_of(store, "r2")
            .await
            .map(|token| (token.kind, token.connection)),
        Some((ConnectionTokenKind::Refresh, "cn_1".to_owned()))
    );
    assert_eq!(
        token_of(store, "a2")
            .await
            .map(|token| (token.kind, token.expires_at)),
        Some((ConnectionTokenKind::Access, at(3_700)))
    );

    // A refresh token past its lapse is refused, and nothing changes.
    assert_eq!(
        store
            .rotate_refresh_token(
                Digest::of(b"r2"),
                &issued("a3", at(1)),
                &issued("r3", at(2)),
                lapse
            )
            .await
            .expect("rotate_refresh_token"),
        Rotation::Expired
    );
    assert_eq!(token_of(store, "a3").await, None);

    // The first refresh token, used again: the connection is revoked.
    assert_eq!(
        store
            .rotate_refresh_token(
                refresh,
                &issued("a4", at(3_800)),
                &issued("r4", lapse),
                at(200)
            )
            .await
            .expect("rotate_refresh_token"),
        Rotation::Used
    );
    assert_eq!(token_of(store, "a4").await, None);
    assert_eq!(
        store
            .connection("cn_1")
            .await
            .expect("connection")
            .and_then(|connection| connection.revoked_at),
        Some(at(200))
    );
    assert_eq!(token_of(store, "r2").await, None, "its tokens are gone");
    assert_eq!(tenants_opened(store, "cn_1").await, []);
    assert_eq!(
        store
            .rotate_refresh_token(refresh, &issued("a5", at(1)), &issued("r5", at(2)), at(300))
            .await
            .expect("rotate_refresh_token"),
        Rotation::Unknown,
        "a revoked connection's token is unknown"
    );
}

pub async fn connections_are_listed_changed_touched_and_revoked<S: AccessStore>(store: &S) {
    let ada = person("ada");
    redeemed(store, "one", &new_connection("cn_b", &ada, CLAUDE, at(1))).await;
    redeemed(
        store,
        "two",
        &new_connection("cn_a", &ada, "rc_cursor", at(1)),
    )
    .await;
    redeemed(
        store,
        "three",
        &new_connection("cn_c", &person("grace"), CLAUDE, at(1)),
    )
    .await;
    assert_eq!(
        store
            .connections_of(&ada)
            .await
            .expect("connections_of")
            .iter()
            .map(|connection| connection.id.as_str())
            .collect::<Vec<_>>()
            .join(","),
        "cn_a,cn_b",
        "by creation, then id"
    );

    // The person changes what it opens.
    assert!(
        store
            .set_connection_organizations(
                "cn_b",
                &[opened(tenant("gamma"), Access::Run, &[("team", "a")])],
                at(5)
            )
            .await
            .expect("set_connection_organizations")
    );
    let grants = store
        .connection_grants("cn_b")
        .await
        .expect("connection_grants");
    assert_eq!(
        grants,
        [Grant {
            tenant: tenant("gamma"),
            principal: Principal::Connection("cn_b".into()),
            role: "harness".into(),
            allowed_labels: BTreeMap::from([("team".to_owned(), "a".to_owned())]),
            granted_by: Some(Principal::User("user_ada".into())),
            granted_at: at(5),
        }]
    );
    assert_eq!(
        grant_of(store, &acme(), &Principal::Connection("cn_b".into())).await,
        None,
        "acme is no longer opened"
    );
    assert_eq!(
        tenants_opened(store, "cn_a").await.len(),
        2,
        "another connection keeps its own"
    );
    assert!(
        store
            .set_connection_organizations("cn_b", &[], at(6))
            .await
            .expect("set_connection_organizations"),
        "a connection may open nothing"
    );
    assert_eq!(tenants_opened(store, "cn_b").await, []);
    assert!(
        !store
            .set_connection_organizations("cn_none", &[], at(6))
            .await
            .expect("set_connection_organizations")
    );

    store
        .touch_connection("cn_b", at(7))
        .await
        .expect("touch_connection");
    assert_eq!(
        store
            .connection("cn_b")
            .await
            .expect("connection")
            .and_then(|connection| connection.last_used_at),
        Some(at(7))
    );

    assert!(
        store
            .revoke_connection("cn_a", at(8))
            .await
            .expect("revoke_connection")
    );
    assert!(
        !store
            .revoke_connection("cn_a", at(9))
            .await
            .expect("revoke_connection"),
        "already revoked"
    );
    assert!(
        !store
            .revoke_connection("cn_none", at(9))
            .await
            .expect("revoke_connection")
    );
    assert_eq!(
        store
            .connection("cn_a")
            .await
            .expect("connection")
            .and_then(|connection| connection.revoked_at),
        Some(at(8)),
        "the first revocation stays"
    );
    assert!(
        !store
            .set_connection_organizations("cn_a", &[opened(acme(), Access::Read, &[])], at(9))
            .await
            .expect("set_connection_organizations"),
        "a revoked connection opens nothing again"
    );
    assert_eq!(tenants_opened(store, "cn_a").await, []);
    assert_eq!(
        store
            .connections_of(&ada)
            .await
            .expect("connections_of")
            .iter()
            .map(|connection| connection.id.as_str())
            .collect::<Vec<_>>(),
        ["cn_b"]
    );
    assert_eq!(
        tenants_opened(store, "cn_c").await.len(),
        2,
        "Grace's connection is untouched"
    );
}

pub async fn codes_tokens_and_connections_are_purged<S: AccessStore>(store: &S) {
    let ada = person("ada");
    // cn_1 lapses at 30 days + 1 s, cn_2 is revoked at 10 s.
    redeemed(store, "one", &new_connection("cn_1", &ada, CLAUDE, at(1))).await;
    redeemed(
        store,
        "two",
        &new_connection("cn_2", &ada, "rc_cursor", at(2)),
    )
    .await;
    store
        .put_oauth_code(&code("late", &ada, CLAUDE, at(1_000)))
        .await
        .expect("put_oauth_code");
    store
        .revoke_connection("cn_2", at(10))
        .await
        .expect("revoke_connection");

    assert_eq!(
        store
            .purge_oauth_codes(at(500), 10)
            .await
            .expect("purge_oauth_codes"),
        2,
        "the codes that expired at 300 s"
    );
    assert!(
        store
            .oauth_code(Digest::of(b"late"))
            .await
            .expect("oauth_code")
            .is_some()
    );

    assert_eq!(
        store
            .purge_connection_tokens(at(3_602), 10)
            .await
            .expect("purge_connection_tokens"),
        1,
        "cn_1's access token, which expired at 3 601 s"
    );
    assert_eq!(token_of(store, "cn_1-access").await, None);
    assert!(token_of(store, "cn_1-refresh").await.is_some());

    assert_eq!(
        store
            .purge_connections(at(100), at(5), 10)
            .await
            .expect("purge_connections"),
        0,
        "neither lapsed nor revoked before the bounds"
    );
    assert_eq!(
        store
            .purge_connections(at(100), at(11), 10)
            .await
            .expect("purge_connections"),
        1,
        "the revoked one"
    );
    assert_eq!(store.connection("cn_2").await.expect("connection"), None);
    assert_eq!(
        store
            .purge_connections(at(30 * 86_400 + 2), at(11), 10)
            .await
            .expect("purge_connections"),
        1,
        "the lapsed one, with its grants and tokens"
    );
    assert_eq!(store.connection("cn_1").await.expect("connection"), None);
    assert_eq!(tenants_opened(store, "cn_1").await, []);
    assert_eq!(token_of(store, "cn_1-refresh").await, None);
    assert_eq!(store.list_grants(&acme()).await.expect("list_grants"), []);
}
