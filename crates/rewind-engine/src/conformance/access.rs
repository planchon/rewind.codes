//! The conformance suite of `AccessStore`: API keys, roles and the role in use, one grant per
//! principal, the kept role's last holder, claims of an invitation's grant, default roles,
//! end users' tokens and token keys, and tenant isolation.

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{Digest, Timestamp};

use super::{T0, check, tenant};
use crate::ports::{
    AccessStore, ApiKey, Grant, GrantClaim, GrantDeletion, GrantWrite, NamespaceName, Principal,
    Role, RoleDeletion, StoreError, TenantId, Token, TokenKey,
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
    check!(
        "access",
        new_store(),
        tokens_expired_or_revoked_are_purged
    );
    check!(
        "access",
        new_store(),
        token_keys_are_found_by_kid_and_deleted_per_tenant
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
