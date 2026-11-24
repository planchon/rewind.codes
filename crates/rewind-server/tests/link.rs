//! The node link over HTTP/2 (`archi/07-code-architecture.md`, "`Cluster` and `NodeLink`",
//! "Tests"): several nodes in one process, each over the same memory adapters, linked by their
//! `HttpLink` and their link servers on loopback. A start and a signal go to the owner on
//! another node, a subscription streams from another node, an activation runs on a remote
//! executor, a node answers `NotOwner` for a partition it handed over, a node whose link stopped
//! is unreachable and its partitions are taken over, a wrong secret and a wrong version are
//! refused.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use std::collections::BTreeSet;
use std::time::Duration;

use cluster::{PATIENCE, World, config, epochs, start, until_all};
use futures_util::StreamExt as _;
use rewind_core::{Command, Epoch, Event, RunStatus};
use rewind_engine::ports::link::{
    NodeReply, NodeRequest, PartitionRequest, StreamId, SubscriptionKey, TenantRequest,
};
use rewind_engine::ports::{
    ClusterError, LinkError, NamespaceName, NodeId, NodeInfo, NodeLink, RunRef, TenantId,
    namespace_partition,
};
use rewind_server::link::{CALL, ClusterSecret, HttpLink, LINK_VERSION, Members, VERSION_HEADER};
use serde_json::json;

/// Waits for the signal `go` and returns its payload.
const WAITER: &str = r#"import { signal } from "rewind";

export default async function main() {
  return await signal("go");
}
"#;

fn namespace(name: &str) -> NamespaceName {
    NamespaceName::new(name).unwrap()
}

fn detach() -> NodeRequest {
    NodeRequest::Tenant {
        tenant: TenantId::local(),
        request: Box::new(TenantRequest::Detach {
            stream: StreamId("none".into()),
        }),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_and_a_signal_reach_the_owner_on_another_node_and_its_changes_stream_back() {
    let mut world = World::new(4).await;
    let a = world.join(config("node-a", 4)).await;
    let b = world.join(config("node-b", 4)).await;
    let map = world.owned_by(&[&a, &b]).await;
    let namespace = world.namespace_on(&b);

    // The edge of node a starts a run whose partition node b owns.
    let run = world.start(&a, start(WAITER, &namespace, json!({}))).await;
    let partition = namespace_partition(&TenantId::local(), &namespace, world.partitions);
    assert_eq!(map.owner(partition), Some(&b));

    // Node a subscribes to the run at its owner, from its first event.
    let mut changes = world
        .node(&a)
        .link
        .subscribe(&b, SubscriptionKey::Run(run.clone()), Some(0))
        .await
        .unwrap();
    world.signal(&a, &run, "go", json!({ "from": "a" })).await;

    let mut events = BTreeSet::new();
    let ended = tokio::time::timeout(PATIENCE, async {
        while let Some(change) = changes.next().await {
            assert_eq!(change.run, run);
            assert_eq!(change.namespace, namespace);
            events.extend(change.events.iter().map(|entry| entry.index));
            let completed = change
                .events
                .iter()
                .any(|entry| matches!(entry.event, Event::Command(Command::CompleteRun { .. })));
            if completed {
                return change.summary;
            }
        }
        None
    })
    .await
    .expect("the run's changes stream from its owner");
    let summary = ended.expect("the run ends");
    assert_eq!(summary.status, RunStatus::Completed);
    let len = summary.history_len;
    assert_eq!(
        events,
        (0..len).collect(),
        "every event streamed, from the first"
    );

    // Its owner answers through the link.
    let view = world.view(&a, &run).await;
    assert_eq!(view.head.status, RunStatus::Completed);
    assert_eq!(view.head.history_len, len);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_does_not_run_the_epoch_sends_the_activations_to_a_node_that_does() {
    let mut world = World::new(1).await;
    // The executors of the run's epoch, owning nothing, join first: the owner sees them when it
    // reads the members.
    let mut remote = config("node-b", 1);
    remote.executors_only = true;
    remote.epochs = Some(epochs(&world.plane));
    let b = world.join(remote).await;
    let mut owner = config("node-a", 1);
    owner.epochs = Some(BTreeSet::from([Epoch(9_999)]));
    let a = world.join(owner).await;
    world.owned_by(&[&a]).await;
    assert!(
        world.map.owned_by(&b).is_empty(),
        "an executor node owns nothing"
    );

    let mut changes = world.changes();
    let run = world
        .start(&a, start(WAITER, &namespace("local"), json!({})))
        .await;
    world
        .signal(&a, &run, "go", json!({ "remote": true }))
        .await;
    until_all(
        &mut changes,
        std::slice::from_ref(&run),
        RunStatus::Completed,
    )
    .await;
    let view = world.view(&a, &run).await;
    assert_eq!(view.head.status, RunStatus::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_answers_not_owner_for_a_partition_it_handed_over_and_the_runs_go_on() {
    let mut world = World::new(4).await;
    let a = world.join(config("node-a", 4)).await;
    world.owned_by(&[&a]).await;
    let mut runs: Vec<(RunRef, u16)> = Vec::new();
    for n in 0..12 {
        let namespace = namespace(&format!("user-{n}"));
        let partition = namespace_partition(&TenantId::local(), &namespace, world.partitions);
        let run = world.start(&a, start(WAITER, &namespace, json!({}))).await;
        runs.push((run, partition.get()));
    }

    // Node b joins and takes the partitions it wins.
    let b = world.join(config("node-b", 4)).await;
    let map = world.owned_by(&[&a, &b]).await;
    let (moved_run, moved) = runs
        .iter()
        .find(|(_, partition)| map.owner(rewind_core::PartitionId::new(*partition)) == Some(&b))
        .cloned()
        .expect("a run in a partition that moved");
    let moved = rewind_core::PartitionId::new(moved);

    // Asked directly, node a answers NotOwner once it took the new map in.
    let reply = tokio::time::timeout(PATIENCE, async {
        loop {
            let reply = world
                .node(&b)
                .link
                .call(
                    &a,
                    NodeRequest::Partition {
                        partition: moved,
                        request: Box::new(PartitionRequest::GetRun {
                            run: moved_run.clone(),
                        }),
                    },
                )
                .await
                .unwrap();
            if matches!(reply, NodeReply::NotOwner { .. }) {
                return reply;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("node a hands the partition over");
    assert_eq!(reply, NodeReply::NotOwner { partition: moved });

    // The edge of node a reaches every run wherever its partition is now.
    let mut changes = world.changes();
    for (run, _) in &runs {
        world.signal(&a, run, "go", json!({ "moved": true })).await;
    }
    let all: Vec<RunRef> = runs.iter().map(|(run, _)| run.clone()).collect();
    until_all(&mut changes, &all, RunStatus::Completed).await;
    assert_eq!(
        world.view(&a, &moved_run).await.head.status,
        RunStatus::Completed
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_whose_link_stopped_is_unreachable_and_its_partitions_are_taken_over() {
    let mut world = World::new(4).await;
    let a = world.join(config("node-a", 4)).await;
    let b = world.join(config("node-b", 4)).await;
    world.owned_by(&[&a, &b]).await;
    let namespace = world.namespace_on(&b);
    let run = world.start(&a, start(WAITER, &namespace, json!({}))).await;
    let mut subscription = world
        .node(&a)
        .link
        .subscribe(&b, SubscriptionKey::Tenant(TenantId::local()), None)
        .await
        .unwrap();

    // Node b's link stops: its subscriptions end, and nothing reaches it any more.
    world.node_mut(&b).stop_link().await;
    tokio::time::timeout(PATIENCE, async {
        while subscription.next().await.is_some() {}
    })
    .await
    .expect("the subscription ends with the link");
    assert_eq!(
        world.node(&a).link.call(&b, detach()).await,
        Err(LinkError::Unreachable(b.clone()))
    );

    // Node b's process is gone: node a takes its partitions over, and the run goes on there.
    assert!(world.cluster.kill(&b));
    world.owned_by(&[&a]).await;
    let mut changes = world.changes();
    world
        .signal(&a, &run, "go", json!({ "after": "takeover" }))
        .await;
    until_all(
        &mut changes,
        std::slice::from_ref(&run),
        RunStatus::Completed,
    )
    .await;
    assert_eq!(world.view(&a, &run).await.head.status, RunStatus::Completed);
}

/// The members a link of another deployment knows.
struct Listed(Vec<NodeInfo>);

impl Members for Listed {
    async fn nodes(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        Ok(self.0.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_secret_and_a_wrong_version_are_refused() {
    let mut world = World::new(1).await;
    let a = world.join(config("node-a", 1)).await;
    world.owned_by(&[&a]).await;
    let address = world.node(&a).address;
    let member = NodeInfo {
        id: a.clone(),
        address: address.to_string(),
        build: "another".into(),
        epochs: BTreeSet::new(),
        pool: None,
        executors_only: false,
    };

    // A link with another secret is refused; a node it does not know is unreachable.
    let stranger = HttpLink::new(
        Listed(vec![member]),
        ClusterSecret::new("the-secret-of-another-cluster").unwrap(),
    )
    .unwrap();
    match stranger.call(&a, detach()).await {
        Err(LinkError::Failed(message)) => assert!(message.contains("401"), "{message}"),
        other => panic!("{other:?}"),
    }
    let unknown = NodeId::new("node-z").unwrap();
    assert_eq!(
        stranger.call(&unknown, detach()).await,
        Err(LinkError::Unreachable(unknown))
    );
    assert_eq!(
        world.node(&a).link.call(&a, detach()).await,
        Ok(NodeReply::Done),
        "the node's own link is admitted"
    );

    // Over HTTP/2 without TLS, a request of another version is refused, naming both.
    let http = reqwest::Client::builder()
        .http2_prior_knowledge()
        .no_proxy()
        .build()
        .unwrap();
    let url = format!("http://{address}{CALL}");
    let body = serde_json::to_vec(&rewind_server::link::dto::NodeRequest::from(&detach())).unwrap();
    let send = |version: Option<&str>, secret: Option<&str>| {
        let mut request = http
            .post(&url)
            .header("content-type", "application/json")
            .body(body.clone());
        if let Some(version) = version {
            request = request.header(VERSION_HEADER, version);
        }
        if let Some(secret) = secret {
            request = request.bearer_auth(secret);
        }
        request.send()
    };
    let secret = "the-secret-of-the-tests-cluster";
    let newer = (LINK_VERSION + 1).to_string();
    let refused = send(Some(&newer), Some(secret)).await.unwrap();
    assert_eq!(refused.version(), reqwest::Version::HTTP_2);
    assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
    let message = refused.text().await.unwrap();
    assert!(
        message.contains(&format!("version {LINK_VERSION}"))
            && message.contains(&format!("version {newer}")),
        "{message}"
    );
    let unversioned = send(None, Some(secret)).await.unwrap();
    assert_eq!(unversioned.status(), reqwest::StatusCode::BAD_REQUEST);
    let anonymous = send(Some(&LINK_VERSION.to_string()), None).await.unwrap();
    assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.headers()["www-authenticate"], "Bearer");
    let admitted = send(Some(&LINK_VERSION.to_string()), Some(secret))
        .await
        .unwrap();
    assert_eq!(admitted.status(), reqwest::StatusCode::OK);
    assert_eq!(admitted.text().await.unwrap(), r#""done""#);
}
