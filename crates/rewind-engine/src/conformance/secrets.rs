//! The conformance suite of `SecretStore` (`archi/07-code-architecture.md`, "The other ports"):
//! secrets of the organization and of each namespace, created and replaced; listings by
//! namespace and labels, never a value; reads counted per day with the last read; deletion of a
//! secret and of a namespace's; tenant isolation; the signing key, made once, rotated, its
//! retired keys published 24 hours.
//!
//! The master key's replacement (`master_keys_in_use`, `rewrap_data_keys`) needs a store
//! reopened with other keys: each store's own tests check it.

use std::collections::BTreeMap;
use std::time::Duration;

use aws_lc_rs::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};
use rewind_core::{RunId, SecretName, Timestamp};

use super::{T0, check, tenant};
use crate::ports::{
    DailyReads, Day, LastRead, NamespaceName, NewSecret, PublicKey, PutSecret,
    RETIRED_KEY_PUBLISHED, Secret, SecretError, SecretId, SecretMeta, SecretNamespace,
    SecretReader, SecretStore, SigningKey, TenantId,
};

/// Runs every check, each on a new store.
pub async fn run_all<S: SecretStore>(new_store: impl Fn() -> S) {
    check!("secrets", new_store(), a_secret_is_created_then_replaced);
    check!(
        "secrets",
        new_store(),
        listings_answer_one_namespace_and_the_labels_asked
    );
    check!(
        "secrets",
        new_store(),
        a_namespace_s_secret_carries_no_label
    );
    check!(
        "secrets",
        new_store(),
        reads_are_counted_per_day_with_the_last_read
    );
    check!("secrets", new_store(), a_registration_s_read_names_no_run);
    check!(
        "secrets",
        new_store(),
        a_deleted_secret_is_gone_with_its_reads
    );
    check!(
        "secrets",
        new_store(),
        an_erased_namespace_loses_its_secrets_only
    );
    check!("secrets", new_store(), secrets_never_cross_tenants);
    check!(
        "secrets",
        new_store(),
        the_signing_key_is_made_once_and_signs
    );
    check!(
        "secrets",
        new_store(),
        a_rotated_key_stays_published_for_a_day
    );
    check!("secrets", new_store(), signing_keys_never_cross_tenants);
}

const DAY: Duration = Duration::from_secs(24 * 3600);

fn name(name: &str) -> SecretName {
    SecretName::new(name).expect("the suite uses valid secret names")
}

fn namespace(name: &str) -> NamespaceName {
    NamespaceName::new(name).expect("the suite uses valid namespaces")
}

fn of_org(secret: &str) -> SecretId {
    SecretId::organization(name(secret))
}

fn of(user: &str, secret: &str) -> SecretId {
    SecretId::of_namespace(namespace(user), name(secret))
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn secret(id: &SecretId, value: &str, labels: BTreeMap<String, String>, by: &str) -> NewSecret {
    NewSecret {
        id: id.clone(),
        value: Secret::new(value),
        labels,
        by: by.to_owned(),
    }
}

fn run(n: u128) -> RunId {
    RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000 + n)
}

fn reader(n: u128, user: &str) -> SecretReader {
    SecretReader::Run {
        run: run(n),
        namespace: namespace(user),
    }
}

fn at(days: u32, millis: u64) -> Timestamp {
    T0.saturating_add(DAY * days)
        .saturating_add(Duration::from_millis(millis))
}

async fn put<S: SecretStore>(store: &S, tenant: &TenantId, secret: &NewSecret, now: Timestamp) {
    store.put(tenant, secret, now).await.expect("put");
}

async fn value<S: SecretStore>(store: &S, tenant: &TenantId, id: &SecretId) -> Option<String> {
    store
        .read(tenant, id, &reader(99, "reader"), at(9, 0))
        .await
        .expect("read")
        .map(|secret| secret.expose().to_owned())
}

async fn listed<S: SecretStore>(
    store: &S,
    tenant: &TenantId,
    namespace: &SecretNamespace,
    labels: &BTreeMap<String, String>,
) -> Vec<String> {
    store
        .list(tenant, namespace, labels, Day::of(T0))
        .await
        .expect("list")
        .into_iter()
        .map(|meta| meta.id.name.to_string())
        .collect()
}

pub async fn a_secret_is_created_then_replaced<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    let zendesk = of_org("zendesk");
    let first = secret(&zendesk, "z-1", labels(&[("env", "prod")]), "user:ada");
    assert_eq!(
        store.put(&acme, &first, at(0, 1)).await.expect("put"),
        PutSecret::Created
    );
    store
        .read(&acme, &zendesk, &reader(1, "alice"), at(0, 2))
        .await
        .expect("read");
    let second = secret(&zendesk, "z-2", labels(&[("env", "dev")]), "key:ops");
    assert_eq!(
        store.put(&acme, &second, at(1, 0)).await.expect("put"),
        PutSecret::Replaced
    );
    let meta = store
        .meta(&acme, &zendesk, Day::of(T0))
        .await
        .expect("meta")
        .expect("the secret exists");
    assert_eq!(
        meta,
        SecretMeta {
            id: zendesk.clone(),
            labels: labels(&[("env", "dev")]),
            created_by: "user:ada".into(),
            created_at: at(0, 1),
            updated_by: "key:ops".into(),
            updated_at: at(1, 0),
            reads: vec![DailyReads {
                day: Day::of(T0),
                reads: 1
            }],
            last_read: Some(LastRead {
                by: reader(1, "alice"),
                at: at(0, 2),
            }),
        },
        "a replacement keeps the creation, the reads and the last read"
    );
    assert_eq!(value(store, &acme, &zendesk).await.as_deref(), Some("z-2"));
    assert_eq!(
        store
            .meta(&acme, &of_org("missing"), Day::of(T0))
            .await
            .expect("meta"),
        None
    );
}

pub async fn listings_answer_one_namespace_and_the_labels_asked<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    let none = BTreeMap::new();
    for (id, secret_labels) in [
        (
            of_org("zendesk"),
            labels(&[("env", "prod"), ("team", "support")]),
        ),
        (of_org("linear"), labels(&[("env", "dev")])),
        (of_org("anthropic"), none.clone()),
        (of("alice", "github"), none.clone()),
        (of("alice", "calendar"), none.clone()),
        (of("bob", "github"), none.clone()),
    ] {
        put(
            store,
            &acme,
            &secret(&id, "v", secret_labels, "user:ada"),
            at(0, 0),
        )
        .await;
    }
    let org = SecretNamespace::Organization;
    assert_eq!(
        listed(store, &acme, &org, &none).await,
        ["anthropic", "linear", "zendesk"],
        "the organization's, sorted by name"
    );
    assert_eq!(
        listed(store, &acme, &org, &labels(&[("env", "prod")])).await,
        ["zendesk"],
        "those carrying all the labels asked"
    );
    assert!(
        listed(
            store,
            &acme,
            &org,
            &labels(&[("env", "prod"), ("team", "sales")])
        )
        .await
        .is_empty()
    );
    let alice = SecretNamespace::Namespace(namespace("alice"));
    assert_eq!(
        listed(store, &acme, &alice, &none).await,
        ["calendar", "github"],
        "a namespace's, never another's nor the organization's"
    );
    assert!(
        listed(
            store,
            &acme,
            &SecretNamespace::Namespace(namespace("carol")),
            &none
        )
        .await
        .is_empty()
    );
    let metas = store
        .list(&acme, &alice, &none, Day::of(T0))
        .await
        .expect("list");
    let printed = format!("{metas:?}");
    assert!(!printed.contains("\"v\""), "no value is listed: {printed}");
}

pub async fn a_namespace_s_secret_carries_no_label<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    let labelled = secret(
        &of("alice", "github"),
        "v",
        labels(&[("env", "prod")]),
        "user:ada",
    );
    assert!(matches!(
        store.put(&acme, &labelled, at(0, 0)).await,
        Err(SecretError::Invalid(_))
    ));
    assert_eq!(value(store, &acme, &of("alice", "github")).await, None);
}

pub async fn reads_are_counted_per_day_with_the_last_read<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    let zendesk = of_org("zendesk");
    put(
        store,
        &acme,
        &secret(&zendesk, "z", BTreeMap::new(), "user:ada"),
        at(0, 0),
    )
    .await;
    for (n, user, now) in [
        (1, "alice", at(0, 10)),
        (2, "bob", at(0, 20)),
        (3, "alice", at(2, 5)),
    ] {
        let read = store
            .read(&acme, &zendesk, &reader(n, user), now)
            .await
            .expect("read");
        assert_eq!(read, Some(Secret::new("z")));
    }
    assert_eq!(
        store
            .read(&acme, &of_org("missing"), &reader(4, "alice"), at(2, 6))
            .await
            .expect("read"),
        None,
        "a secret that does not exist reads nothing"
    );
    let meta = |since: Day| {
        let (acme, zendesk) = (acme.clone(), zendesk.clone());
        async move {
            store
                .meta(&acme, &zendesk, since)
                .await
                .expect("meta")
                .expect("the secret exists")
        }
    };
    let all = meta(Day::of(T0)).await;
    assert_eq!(
        all.reads,
        [
            DailyReads {
                day: Day::of(at(0, 0)),
                reads: 2
            },
            DailyReads {
                day: Day::of(at(2, 0)),
                reads: 1
            },
        ],
        "per day, oldest first, the days without a read left out"
    );
    assert_eq!(
        all.last_read,
        Some(LastRead {
            by: reader(3, "alice"),
            at: at(2, 5),
        })
    );
    assert_eq!(
        meta(Day::of(at(1, 0))).await.reads,
        [DailyReads {
            day: Day::of(at(2, 0)),
            reads: 1
        }],
        "from the day asked"
    );
    let listed = store
        .list(
            &acme,
            &SecretNamespace::Organization,
            &BTreeMap::new(),
            Day::of(T0),
        )
        .await
        .expect("list");
    assert_eq!(listed, [all], "a listing gives the same metadata");
}

/// An action set's registration reads a secret to list a server's tools before any run exists: the
/// read is counted, and the last read names no run until a run reads it again.
pub async fn a_registration_s_read_names_no_run<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    let notion = of_org("notion");
    put(
        store,
        &acme,
        &secret(&notion, "n", BTreeMap::new(), "user:ada"),
        at(0, 0),
    )
    .await;
    let last_after = |by: SecretReader, now: Timestamp| {
        let (acme, notion) = (acme.clone(), notion.clone());
        async move {
            let read = store.read(&acme, &notion, &by, now).await.expect("read");
            assert_eq!(read, Some(Secret::new("n")));
            let meta = store
                .meta(&acme, &notion, Day::of(T0))
                .await
                .expect("meta")
                .expect("the secret exists");
            (meta.last_read, meta.reads)
        }
    };
    let (last, reads) = last_after(SecretReader::Registration, at(0, 10)).await;
    assert_eq!(
        last,
        Some(LastRead {
            by: SecretReader::Registration,
            at: at(0, 10),
        }),
        "a registration's read names no run"
    );
    assert_eq!(
        reads,
        [DailyReads {
            day: Day::of(at(0, 0)),
            reads: 1
        }],
        "a registration's read is counted"
    );
    let (last, _) = last_after(reader(1, "alice"), at(0, 20)).await;
    assert_eq!(
        last,
        Some(LastRead {
            by: reader(1, "alice"),
            at: at(0, 20),
        })
    );
    let (last, reads) = last_after(SecretReader::Registration, at(0, 30)).await;
    assert_eq!(
        last,
        Some(LastRead {
            by: SecretReader::Registration,
            at: at(0, 30),
        }),
        "a registration's read after a run's leaves no run named"
    );
    assert_eq!(
        reads,
        [DailyReads {
            day: Day::of(at(0, 0)),
            reads: 3
        }]
    );
}

pub async fn a_deleted_secret_is_gone_with_its_reads<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    let github = of("alice", "github");
    put(
        store,
        &acme,
        &secret(&github, "g", BTreeMap::new(), "token:alice"),
        at(0, 0),
    )
    .await;
    store
        .read(&acme, &github, &reader(1, "alice"), at(0, 1))
        .await
        .expect("read");
    assert!(store.delete(&acme, &github).await.expect("delete"));
    assert!(!store.delete(&acme, &github).await.expect("delete"));
    assert_eq!(
        store.meta(&acme, &github, Day::of(T0)).await.expect("meta"),
        None
    );
    put(
        store,
        &acme,
        &secret(&github, "g2", BTreeMap::new(), "token:alice"),
        at(1, 0),
    )
    .await;
    let meta = store
        .meta(&acme, &github, Day::of(T0))
        .await
        .expect("meta")
        .expect("put again");
    assert!(meta.reads.is_empty(), "its reads went with it");
    assert_eq!(meta.last_read, None);
    assert_eq!(meta.created_at, at(1, 0));
}

pub async fn an_erased_namespace_loses_its_secrets_only<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    for id in [
        of("alice", "github"),
        of("alice", "calendar"),
        of("bob", "github"),
        of_org("github"),
    ] {
        put(
            store,
            &acme,
            &secret(&id, "v", BTreeMap::new(), "user:ada"),
            at(0, 0),
        )
        .await;
    }
    assert_eq!(
        store
            .delete_namespace(&tenant("other"), &namespace("alice"))
            .await
            .expect("delete_namespace"),
        0
    );
    assert_eq!(
        store
            .delete_namespace(&acme, &namespace("alice"))
            .await
            .expect("delete_namespace"),
        2
    );
    assert_eq!(value(store, &acme, &of("alice", "github")).await, None);
    assert_eq!(value(store, &acme, &of("alice", "calendar")).await, None);
    assert_eq!(
        value(store, &acme, &of("bob", "github")).await.as_deref(),
        Some("v")
    );
    assert_eq!(
        value(store, &acme, &of_org("github")).await.as_deref(),
        Some("v"),
        "the organization's secret of the same name stays"
    );
}

pub async fn secrets_never_cross_tenants<S: SecretStore>(store: &S) {
    let (acme, globex) = (tenant("acme"), tenant("globex"));
    for id in [of_org("zendesk"), of("alice", "github")] {
        put(
            store,
            &acme,
            &secret(&id, "acme", BTreeMap::new(), "user:ada"),
            at(0, 0),
        )
        .await;
        assert_eq!(value(store, &globex, &id).await, None);
        assert_eq!(
            store.meta(&globex, &id, Day::of(T0)).await.expect("meta"),
            None
        );
        assert!(!store.delete(&globex, &id).await.expect("delete"));
        put(
            store,
            &globex,
            &secret(&id, "globex", BTreeMap::new(), "user:bo"),
            at(0, 0),
        )
        .await;
        assert_eq!(value(store, &acme, &id).await.as_deref(), Some("acme"));
        assert_eq!(value(store, &globex, &id).await.as_deref(), Some("globex"));
    }
    assert_eq!(
        listed(
            store,
            &tenant("initech"),
            &SecretNamespace::Organization,
            &BTreeMap::new()
        )
        .await,
        Vec::<String>::new()
    );
}

/// The private key is the seed of its public key, and its `kid` is the start of its SHA-256.
fn assert_key_pair(key: &SigningKey) {
    assert_eq!(key.public.kid, PublicKey::kid_of(&key.public.key));
    let pair = Ed25519KeyPair::from_seed_unchecked(key.private.expose())
        .expect("the private key is an Ed25519 seed");
    assert_eq!(pair.public_key().as_ref(), key.public.key.as_slice());
    let signature = pair.sign(b"rewind");
    UnparsedPublicKey::new(&ED25519, key.public.key)
        .verify(b"rewind", signature.as_ref())
        .expect("the public key checks what the private key signs");
}

pub async fn the_signing_key_is_made_once_and_signs<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    assert_eq!(
        store
            .public_keys(&acme, at(0, 0))
            .await
            .expect("public_keys"),
        [],
        "no key before the first call"
    );
    let key = store
        .signing_key(&acme, at(0, 0))
        .await
        .expect("signing_key");
    assert_key_pair(&key);
    assert_eq!(key.public.created_at, at(0, 0));
    assert_eq!(key.public.retired_at, None);
    assert_eq!(
        store
            .signing_key(&acme, at(3, 0))
            .await
            .expect("signing_key"),
        key,
        "the same key until it is replaced"
    );
    assert_eq!(
        store
            .public_keys(&acme, at(3, 0))
            .await
            .expect("public_keys"),
        [key.public]
    );
}

pub async fn a_rotated_key_stays_published_for_a_day<S: SecretStore>(store: &S) {
    let acme = tenant("acme");
    let old = store
        .signing_key(&acme, at(0, 0))
        .await
        .expect("signing_key");
    let new = store
        .rotate_signing_key(&acme, at(1, 0))
        .await
        .expect("rotate_signing_key");
    assert_key_pair(&new);
    assert_ne!(new.public.kid, old.public.kid);
    assert_eq!(
        store
            .signing_key(&acme, at(1, 1))
            .await
            .expect("signing_key"),
        new,
        "the new key signs"
    );
    let retired = PublicKey {
        retired_at: Some(at(1, 0)),
        ..old.public.clone()
    };
    let last_published = at(1, 0).saturating_add(RETIRED_KEY_PUBLISHED);
    assert_eq!(
        store
            .public_keys(&acme, at(1, 1))
            .await
            .expect("public_keys"),
        [new.public.clone(), retired.clone()],
        "the current key, then the retired one, newest first"
    );
    assert_eq!(
        store
            .public_keys(&acme, Timestamp(last_published.as_millis() - 1))
            .await
            .expect("public_keys"),
        [new.public.clone(), retired]
    );
    assert_eq!(
        store
            .public_keys(&acme, last_published)
            .await
            .expect("public_keys"),
        std::slice::from_ref(&new.public),
        "24 hours after it stopped signing, a key is no longer published"
    );
    let first = store
        .rotate_signing_key(&tenant("globex"), at(0, 0))
        .await
        .expect("rotate_signing_key");
    assert_eq!(
        store
            .public_keys(&tenant("globex"), at(0, 0))
            .await
            .expect("public_keys"),
        [first.public],
        "replacing no key makes the first one"
    );
}

pub async fn signing_keys_never_cross_tenants<S: SecretStore>(store: &S) {
    let acme = store
        .signing_key(&tenant("acme"), at(0, 0))
        .await
        .expect("signing_key");
    let globex = store
        .signing_key(&tenant("globex"), at(0, 0))
        .await
        .expect("signing_key");
    assert_ne!(acme.public.key, globex.public.key);
    assert_ne!(acme.private, globex.private);
    store
        .rotate_signing_key(&tenant("globex"), at(1, 0))
        .await
        .expect("rotate_signing_key");
    assert_eq!(
        store
            .signing_key(&tenant("acme"), at(1, 1))
            .await
            .expect("signing_key"),
        acme,
        "another tenant's rotation leaves the key alone"
    );
    assert_eq!(
        store
            .public_keys(&tenant("acme"), at(1, 1))
            .await
            .expect("public_keys"),
        [acme.public]
    );
}
