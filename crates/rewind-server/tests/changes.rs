//! The change stream (`GET /v1/changes`) end to end on Turso: a run's summary as it starts and
//! ends, each stream with the runs its caller may see (a grant's labels, an end user's
//! namespace), `resync` when more runs changed at once than a pass reads, and the end of a
//! stream at its access token's expiry, once its end user's token is revoked, once its key is
//! revoked or its grant no longer gives `runs:read`; an access token already expired is refused.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use rewind_api::access::{BuiltInRole, CreateApiKey, Permission, PutRole};
use rewind_api::common::{Labels, RunStatus};
use rewind_api::events::Change;
use rewind_api::namespaces::{CreateToken, NamespaceName};
use rewind_api::organization::CreateOrganization;
use rewind_api::runs::{RunSummary, StartRun};
use rewind_api::{ChangeStream, Client};
use serde_json::json;
use support::{Browser, TestServer, identity, token};

/// A run that waits for a signal, then returns it.
const SIGNAL: &str = r#"import { signal } from "rewind";
export default async function main() {
  return { decision: await signal<{ ok: boolean }>("approval") };
}
"#;

/// A run of `SIGNAL`, marked so that each run of a test has its own code, with `labels`.
fn waiting(marker: &str, labels: &[(&str, &str)]) -> StartRun {
    let mut start = StartRun::new(format!("// {marker}\n{SIGNAL}"));
    start.labels = labels.iter().copied().collect::<Labels>();
    start
}

/// The next event of a stream, within 30 s.
async fn next(stream: &mut ChangeStream) -> Change {
    tokio::time::timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("an event within 30 s")
        .expect("the stream is open")
        .expect("a valid event")
}

/// The runs of the stream's `run` events, until one is of `id` and `holds`; every run seen,
/// that one last.
async fn until(
    stream: &mut ChangeStream,
    id: &str,
    holds: impl Fn(&RunSummary) -> bool,
) -> Vec<RunSummary> {
    let mut seen = Vec::new();
    loop {
        if let Change::Run(run) = next(stream).await {
            let done = run.id == id && holds(&run);
            seen.push(*run);
            if done {
                return seen;
            }
        }
    }
}

async fn signal(client: &Client, run: &str) {
    client
        .signal_run(run, "approval", &json!({ "ok": true }), None)
        .await
        .unwrap();
}

/// A member's stream gets a run's summary, the object `GET /v1/runs` lists, when the run starts
/// and when it ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_follows_a_run_from_its_start_to_its_end() {
    let (identity, _clock) = identity();
    let server = TestServer::signed_in(&identity, &[]).await;
    identity.add_user("ada@example.net", true);
    let mut browser = Browser::new(&server.url);
    let state = browser
        .sign_in(&identity, "ada@example.net", None)
        .await
        .unwrap();
    let acme = server
        .client(Some(&token(&state).0))
        .create_organization(&CreateOrganization {
            name: "Acme".into(),
        })
        .await
        .unwrap();
    let owner = server.client(Some(&token(&browser.switch(&acme.id).await.unwrap()).0));

    let mut stream = owner.changes().await.unwrap();
    let run = owner
        .start_run(&waiting("member", &[]), None)
        .await
        .unwrap();
    let started = until(&mut stream, &run.id, |run| run.status == RunStatus::Active)
        .await
        .pop()
        .unwrap();
    assert_eq!(started.namespace, run.namespace);
    assert!(started.history_len > 0);

    signal(&owner, &run.id).await;
    let ended = until(&mut stream, &run.id, |run| {
        run.status == RunStatus::Completed
    })
    .await
    .pop()
    .unwrap();
    assert!(ended.ended_at.is_some());
    assert!(ended.waiting_for.is_empty());
    let listed = owner
        .list_runs(&rewind_api::runs::RunFilter::default())
        .await
        .unwrap()
        .runs
        .into_iter()
        .find(|listed| listed.id == run.id)
        .unwrap();
    // What moves while the run's last events are projected aside, the same object.
    let stable = |run: &RunSummary| {
        let mut value = serde_json::to_value(run).unwrap();
        for moving in ["historyLen", "lastActivityAt", "calls", "errors"] {
            value.as_object_mut().unwrap().remove(moving);
        }
        value
    };
    assert_eq!(
        stable(&ended),
        stable(&listed),
        "the event holds what GET /v1/runs lists"
    );
    server.stop().await;
}

/// A key limited to labels sees the runs carrying them all, and no other: not even their id.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_limited_to_labels_sees_only_the_runs_carrying_them() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let limited = server.key(BuiltInRole::Reader, &[("team", "a")]).await;
    let mut everything = harness.changes().await.unwrap();
    let mut only_a = limited.changes().await.unwrap();

    let other = harness
        .start_run(&waiting("b", &[("team", "b")]), None)
        .await
        .unwrap();
    let mine = harness
        .start_run(&waiting("a", &[("team", "a"), ("agent", "x")]), None)
        .await
        .unwrap();
    signal(&harness, &other.id).await;
    // The other run ends before this one: its events reach every stream before this one's end.
    until(&mut everything, &other.id, |run| {
        run.status == RunStatus::Completed
    })
    .await;
    signal(&harness, &mine.id).await;
    let seen = until(&mut only_a, &mine.id, |run| {
        run.status == RunStatus::Completed
    })
    .await;
    assert!(
        seen.iter().all(|run| run.id == mine.id),
        "{:?}",
        seen.iter().map(|run| &run.id).collect::<Vec<_>>()
    );
    server.stop().await;
}

/// An end user's token sees its namespace's runs only, and its stream closes once the token is
/// revoked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_end_users_token_sees_its_namespace_only() {
    let server = TestServer::start(&[]).await;
    let customer = server.tenant_key("acme", "harness").await;
    let mint = |namespace: &'static str| {
        let customer = &customer;
        async move {
            customer
                .create_token(
                    &namespace.parse::<NamespaceName>().unwrap(),
                    &CreateToken::default(),
                )
                .await
                .unwrap()
        }
    };
    let alice_token = mint("alice").await;
    let alice = server.bare(alice_token.token.as_deref().unwrap());
    let bob = server.bare(mint("bob").await.token.as_deref().unwrap());
    let mut stream = alice.changes().await.unwrap();
    let mut operator = customer.changes().await.unwrap();

    let his = bob.start_run(&waiting("bob", &[]), None).await.unwrap();
    let hers = alice.start_run(&waiting("alice", &[]), None).await.unwrap();
    until(&mut operator, &his.id, |_| true).await;
    let seen = until(&mut stream, &hers.id, |_| true).await;
    assert!(
        seen.iter().all(|run| run.id == hers.id),
        "{:?}",
        seen.iter().map(|run| &run.id).collect::<Vec<_>>()
    );
    signal(&bob, &his.id).await;
    until(&mut operator, &his.id, |run| {
        run.status == RunStatus::Completed
    })
    .await;
    signal(&alice, &hers.id).await;
    let seen = until(&mut stream, &hers.id, |run| {
        run.status == RunStatus::Completed
    })
    .await;
    assert!(seen.iter().all(|run| run.id == hers.id));

    customer.revoke_token(&alice_token.id).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while stream.next().await.is_some() {}
    })
    .await;
    assert!(
        closed.is_ok(),
        "the stream closes once its token is revoked"
    );
    server.stop().await;
}

/// A stream opened with a member's access token ends at the token's expiry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_opened_with_an_access_token_ends_at_its_expiry() {
    let (identity, clock) = identity();
    let server = TestServer::signed_in(&identity, &[]).await;
    identity.add_user("ada@example.net", true);
    let mut browser = Browser::new(&server.url);
    let state = browser
        .sign_in(&identity, "ada@example.net", None)
        .await
        .unwrap();
    let acme = server
        .client(Some(&token(&state).0))
        .create_organization(&CreateOrganization {
            name: "Acme".into(),
        })
        .await
        .unwrap();
    // A token with 3 s left.
    clock.set(-297_000);
    let short = server.client(Some(&token(&browser.switch(&acme.id).await.unwrap()).0));
    let started = Instant::now();
    let mut stream = short.changes().await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(30), async {
        while stream.next().await.is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "the stream ends");
    let lasted = started.elapsed();
    assert!(
        (Duration::from_secs(1)..Duration::from_secs(20)).contains(&lasted),
        "{lasted:?}"
    );
    server.stop().await;
}

/// The stream is still open after a while: nothing came, and it did not end.
async fn still_open(stream: &mut ChangeStream) {
    assert!(
        tokio::time::timeout(Duration::from_millis(2_500), stream.next())
            .await
            .is_err(),
        "the stream is open and quiet"
    );
}

/// The stream ends within 10 s.
async fn closes(stream: &mut ChangeStream, why: &str) {
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while stream.next().await.is_some() {}
    })
    .await;
    assert!(closed.is_ok(), "{why}");
}

/// A key's stream closes once the key is revoked, or once its grant no longer gives `runs:read`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_keys_stream_closes_once_the_key_or_its_right_is_gone() {
    let server = TestServer::start(&[]).await;
    let key = |role: &str| {
        let admin = &server.admin;
        let role = role.to_owned();
        async move {
            admin
                .create_api_key(&CreateApiKey {
                    tenant: Some("acme".into()),
                    name: format!("a {role} key of the change stream"),
                    role,
                    allowed_labels: Labels::new(),
                })
                .await
                .unwrap()
        }
    };

    let revoked = key(BuiltInRole::Reader.as_str()).await;
    let mut stream = server
        .client(revoked.key.as_deref())
        .changes()
        .await
        .unwrap();
    still_open(&mut stream).await;
    server.admin.revoke_api_key(&revoked.id).await.unwrap();
    closes(&mut stream, "the stream closes once its key is revoked").await;

    let watching = PutRole {
        description: "follows the runs".into(),
        permissions: vec![Permission::RunsRead],
    };
    server.admin.put_role("watcher", &watching).await.unwrap();
    let watcher = key("watcher").await;
    let mut stream = server
        .client(watcher.key.as_deref())
        .changes()
        .await
        .unwrap();
    still_open(&mut stream).await;
    let narrowed = PutRole {
        description: "no longer follows the runs".into(),
        permissions: vec![Permission::ActionsRead],
    };
    server.admin.put_role("watcher", &narrowed).await.unwrap();
    closes(
        &mut stream,
        "the stream closes once its grant no longer gives runs:read",
    )
    .await;
    server.stop().await;
}

/// An access token already expired, which the identity provider's leeway still accepts, is
/// refused with 401: the client renews it, rather than reconnecting to a stream that ends at
/// once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_access_token_already_expired_is_refused() {
    let (identity, clock) = identity();
    let server = TestServer::signed_in(&identity, &[]).await;
    identity.add_user("ada@example.net", true);
    let mut browser = Browser::new(&server.url);
    let state = browser
        .sign_in(&identity, "ada@example.net", None)
        .await
        .unwrap();
    let acme = server
        .client(Some(&token(&state).0))
        .create_organization(&CreateOrganization {
            name: "Acme".into(),
        })
        .await
        .unwrap();
    // A token that expired a second ago, by the world's clock.
    clock.set(-301_000);
    let expired = server.client(Some(&token(&browser.switch(&acme.id).await.unwrap()).0));
    let refused = expired.changes().await.unwrap_err();
    assert_eq!(refused.problem().map(|problem| problem.status), Some(401));
    server.stop().await;
}
