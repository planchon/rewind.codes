//! Several nodes in one process over the memory adapters, stepped by the driver
//! (`archi/07-code-architecture.md`, "Tests"; `archi/06-storage-and-scale.md`, "Partitions,
//! owners and nodes"): a node killed during an activation, a partition moved, a lost message,
//! an owner fenced while cut off, a remote executor for an epoch a node does not run, and an
//! SDK stream that closes and comes back within its grace, or does not.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rewind_core::{
    Epoch, Interruption, McpServer, McpTransport, PartitionId, RunStatus, Tool, ToolFailureKind,
    ToolPolicy, ToolRoute, Toolset,
};
use rewind_engine::edge::EdgeError;
use rewind_engine::plane::HostTools;
use rewind_engine::ports::link::{
    NodeReply, NodeRequest, OwnerNotice, PartitionReply, PartitionRequest, Refusal, SignalAtOwner,
    StreamAttach, StreamId,
};
use rewind_engine::ports::{
    Delivery, Enqueued, NamespaceName, NodeId, RunRef, TaskState, TenantId, home_partition,
    namespace_partition,
};
use serde_json::json;
use support::plane::{World, config, node};
use support::{FakeExecutor, RecordingDispatcher, request};

/// A namespace whose partition the node `owner` holds.
fn namespace_on(world: &World, owner: &NodeId) -> (NamespaceName, PartitionId) {
    let map = world.driver.map(&world.at).unwrap();
    (0..1_000)
        .map(|n| NamespaceName::new(format!("user-{n}")).unwrap())
        .map(|name| {
            let partition = namespace_partition(&TenantId::local(), &name, world.partitions);
            (name, partition)
        })
        .find(|(_, partition)| map.owner(*partition) == Some(owner))
        .expect("the node owns a partition")
}

fn in_namespace(
    mut request: rewind_engine::StartRun,
    namespace: &NamespaceName,
) -> rewind_engine::StartRun {
    request.namespace = namespace.clone();
    request
}

fn sleeper() -> FakeExecutor {
    FakeExecutor::new().with(
        "sleeper",
        |run| {
            run.timer(1_000);
        },
        |run, _| run.complete(json!("awake")),
    )
}

fn waiter() -> FakeExecutor {
    FakeExecutor::new().with(
        "waiter",
        |run| run.await_signal("go"),
        |run, delivery| match delivery {
            Delivery::SignalReceived { payload, .. } => run.complete(payload.clone()),
            other => panic!("unexpected {other:?}"),
        },
    )
}

fn drain(stream: &mut support::plane::Stream) -> Vec<rewind_engine::ports::link::HostTask> {
    support::plane::received(stream).0
}

async fn two_nodes(
    programs: FakeExecutor,
    dispatcher: RecordingDispatcher,
    partitions: u16,
) -> World {
    World::with(
        programs,
        dispatcher,
        partitions,
        &[config("node-a", partitions), config("node-b", partitions)],
    )
    .await
}

#[tokio::test]
async fn a_node_killed_during_an_activation_is_taken_over_and_the_run_goes_on() {
    let mut world = two_nodes(sleeper(), RecordingDispatcher::echo(), 4).await;
    let (a, b) = (node("node-a"), node("node-b"));
    let (namespace, partition) = namespace_on(&world, &a);
    world.at = b.clone();
    let run = world
        .start(in_namespace(request("sleeper", json!({})), &namespace))
        .await
        .unwrap()
        .run;
    world.settle().await;
    assert_eq!(world.events(&run).await, ["run_started", "start_timer"]);

    // The timer fires on node a: its activation is in flight when the node dies.
    world.advance(1_000);
    while world.driver.pending_activations(&a) == 0 {
        assert!(world.driver.step().await, "the activation never started");
    }
    world.driver.kill(&a);
    world.settle().await;

    let map = world.driver.map(&b).unwrap();
    assert_eq!(
        map.owner(partition),
        Some(&b),
        "node b took the partition over"
    );
    let view = world.view(&run).await;
    assert_eq!(view.head.status, RunStatus::Completed);
    assert_eq!(view.head.attempts, 0, "the commit reset the count");
    assert_eq!(
        world.events(&run).await,
        ["run_started", "start_timer", "timer_fired", "complete_run"]
    );
}

#[tokio::test]
async fn a_partition_moves_to_a_node_that_joins_and_its_runs_go_on() {
    let mut world = World::with(
        waiter(),
        RecordingDispatcher::echo(),
        8,
        &[config("node-a", 8)],
    )
    .await;
    let a = node("node-a");
    let mut runs = Vec::new();
    for n in 0..8 {
        let namespace = NamespaceName::new(format!("user-{n}")).unwrap();
        let run = world
            .start(in_namespace(request("waiter", json!({})), &namespace))
            .await
            .unwrap()
            .run;
        runs.push((run, namespace));
    }
    world.settle().await;
    let before = world.driver.map(&a).unwrap();
    assert_eq!(before.owned_by(&a).len(), 8);

    let b = world
        .driver
        .add_node(world.plane.clone(), config("node-b", 8))
        .await
        .unwrap();
    world.settle().await;
    let after = world.driver.map(&a).unwrap();
    let moved = after.owned_by(&b);
    assert!(!moved.is_empty(), "node b takes the partitions it wins");
    assert_eq!(after.owned_by(&a).len() + moved.len(), 8);
    for partition in &moved {
        assert!(
            after.epoch(*partition) > before.epoch(*partition),
            "a move raises the epoch"
        );
    }
    assert_eq!(
        world.driver.node(&a).unwrap().partitions().len(),
        8 - moved.len()
    );
    assert_eq!(world.driver.node(&b).unwrap().partitions(), moved);

    // Every run goes on, wherever its partition is now.
    for (run, _) in &runs {
        assert_eq!(
            world
                .signal(run, "go", json!({ "moved": true }), None)
                .await
                .unwrap(),
            Enqueued::Accepted
        );
    }
    world.settle().await;
    for (run, _) in &runs {
        assert_eq!(world.view(run).await.head.status, RunStatus::Completed);
    }
}

/// A toolset whose tool `srv.a` is an MCP server's, whose calls need a slot of the
/// organization.
fn mcp_request(name: &str) -> rewind_engine::StartRun {
    let server = McpServer::new(
        "srv".to_owned(),
        McpTransport::Http {
            url: "https://mcp.example.com/srv".to_owned(),
        },
    );
    let tool = Tool::new(
        "srv.a",
        ToolRoute::Mcp {
            server: "srv".into(),
        },
        json!({}),
    );
    let mut request = request(name, json!({}));
    request.toolset = Toolset::new(vec![server], vec![tool]).unwrap();
    request
}

fn mcp_caller() -> FakeExecutor {
    FakeExecutor::new().with(
        "mcp",
        |run| {
            run.call("srv.a", json!({ "x": 1 }));
        },
        |run, delivery| run.complete(support::result_of(delivery).clone()),
    )
}

#[tokio::test]
async fn a_lost_message_is_rebuilt_from_the_records_when_the_partition_moves() {
    let mut world = two_nodes(mcp_caller(), RecordingDispatcher::echo(), 4).await;
    let (a, b) = (node("node-a"), node("node-b"));
    // The run lives on one node, its tenant's organization actor on the other.
    let map = world.driver.map(&a).unwrap();
    let home = home_partition(&TenantId::local(), world.partitions);
    let home_node = map.owner(home).unwrap().clone();
    let other = if home_node == a { b.clone() } else { a.clone() };
    let (namespace, _) = namespace_on(&world, &other);
    world.at = home_node.clone();

    // The grant of the call's slot is lost on the link.
    world.driver.faults().drop_next(1, |request| {
        matches!(
            request,
            NodeRequest::Owner { notice, .. } if matches!(**notice, OwnerNotice::SlotGranted { .. })
        )
    });
    let run = world
        .start(in_namespace(mcp_request("mcp"), &namespace))
        .await
        .unwrap()
        .run;
    world.settle().await;
    assert_eq!(world.driver.faults().dropped, 1);
    assert!(
        world.plane.dispatcher.calls().is_empty(),
        "the call waits for a grant it lost"
    );
    assert_eq!(world.view(&run).await.head.status, RunStatus::Active);

    // Nothing asks again: the call goes once the partition's next owner rebuilds it from the
    // records, as an interrupted call, attempt + 1.
    world.driver.kill(&other);
    world.settle().await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
    let calls = world.plane.dispatcher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].attempt, 2);
}

#[tokio::test]
async fn an_owner_cut_off_is_fenced_at_its_next_commit() {
    let mut world = two_nodes(waiter(), RecordingDispatcher::echo(), 4).await;
    let (a, b) = (node("node-a"), node("node-b"));
    let (namespace, partition) = namespace_on(&world, &a);
    world.at = b.clone();
    let run = world
        .start(in_namespace(request("waiter", json!({})), &namespace))
        .await
        .unwrap()
        .run;
    world.settle().await;

    // Node a is cut off from the cluster: its liveness ends, node b takes its partitions over,
    // and node a goes on as if it still owned them.
    let held = world.driver.node(&a).unwrap().partitions();
    world.driver.cut_off(&a);
    assert!(world.cluster.kill(&a));
    world.settle().await;
    assert_eq!(world.driver.map(&b).unwrap().owner(partition), Some(&b));
    assert_eq!(
        world.driver.node(&a).unwrap().partitions(),
        held,
        "node a does not know"
    );

    // An edge's request reaches node a's stale owner: its commit is refused as fenced, and the
    // owner drops the partition.
    let signal = PartitionRequest::Signal(SignalAtOwner {
        run: run.clone(),
        name: "go".into(),
        payload: json!({ "from": "a" }),
        key: None,
    });
    let reply = world
        .driver
        .request(
            &a,
            NodeRequest::Partition {
                partition,
                request: Box::new(signal),
            },
        )
        .await
        .unwrap();
    assert!(matches!(reply, NodeReply::NotOwner { .. }), "{reply:?}");
    assert!(
        !world
            .driver
            .node(&a)
            .unwrap()
            .partitions()
            .contains(&partition)
    );
    assert_eq!(
        world.view(&run).await.head.status,
        RunStatus::Active,
        "nothing of it is visible"
    );

    // Node b owns the run.
    world
        .signal(&run, "go", json!({ "from": "b" }), None)
        .await
        .unwrap();
    world.settle().await;
    let view = world.view(&run).await;
    assert_eq!(view.head.status, RunStatus::Completed);
    let history = world.history(&run).await;
    assert_eq!(history.len(), 4);
}

#[tokio::test]
async fn a_node_that_does_not_run_the_run_s_epoch_sends_its_activations_to_one_that_does() {
    let mut owner = config("node-a", 1);
    owner.epochs = Some(BTreeSet::from([Epoch(9)]));
    let mut remote = config("node-b", 1);
    remote.executors_only = true;
    remote.epochs = Some(BTreeSet::from([Epoch(1)]));
    let mut world = World::with(waiter(), RecordingDispatcher::echo(), 1, &[owner, remote]).await;
    let (a, b) = (node("node-a"), node("node-b"));
    assert_eq!(
        world.driver.map(&a).unwrap().owned_by(&a).len(),
        1,
        "an executor node owns nothing"
    );
    let run = world.run(request("waiter", json!({}))).await;
    assert_eq!(world.events(&run).await, ["run_started", "signal_awaited"]);
    assert_eq!(world.driver.node(&a).unwrap().cached_instances(), 0);
    assert_eq!(
        world.driver.node(&b).unwrap().cached_instances(),
        1,
        "the instance stays warm there"
    );
    let starts = world.plane.executor.starts();

    world
        .signal(&run, "go", json!({ "ok": true }), None)
        .await
        .unwrap();
    world.settle().await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
    assert_eq!(
        world.plane.executor.starts(),
        starts,
        "the same node, warm: no replay"
    );
}

fn host_call(policy: Interruption) -> rewind_engine::StartRun {
    let mut request = request("host", json!({}));
    let mut tools: Vec<Tool> = request.toolset.tools().to_vec();
    for tool in &mut tools {
        if tool.name == "a" {
            let mut with = ToolPolicy::default_for(&tool.route);
            with.on_interruption = policy;
            with.heartbeat_timeout = Duration::from_secs(60);
            tool.policy = with;
        }
    }
    request.toolset = Toolset::new(Vec::new(), tools).unwrap();
    request
}

fn host() -> FakeExecutor {
    FakeExecutor::new().with(
        "host",
        |run| {
            run.call("a", json!({ "ask": "user" }));
        },
        |run, delivery| match delivery {
            Delivery::ToolCallCompleted { result, .. } => run.complete(result.clone()),
            Delivery::ToolCallFailed { error, .. } => {
                run.complete(json!({ "kind": error.kind.as_str() }));
            }
            other => panic!("unexpected {other:?}"),
        },
    )
}

/// Two nodes: the stream at one, the organization actor and the run wherever they land.
async fn streams_world() -> (World, NodeId) {
    let mut a = config("node-a", 2);
    a.host_tools = HostTools::Streams;
    let mut b = config("node-b", 2);
    b.host_tools = HostTools::Streams;
    let world = World::with(host(), RecordingDispatcher::echo(), 2, &[a, b]).await;
    (world, node("node-b"))
}

fn attach(stream: &str, worker: &str, at: &NodeId) -> StreamAttach {
    StreamAttach {
        stream: StreamId(stream.into()),
        worker: worker.into(),
        node: at.clone(),
        namespace: None,
        labels: BTreeMap::new(),
        toolset: None,
        tools: BTreeSet::new(),
        slots: 4,
    }
}

#[tokio::test]
async fn a_stream_that_comes_back_within_its_grace_keeps_its_tasks() {
    let (mut world, edge) = streams_world().await;
    let mut first = world
        .driver
        .attach_stream(&edge, TenantId::local(), attach("s1", "worker-1", &edge))
        .unwrap();
    world.settle().await;
    let run = world.run(host_call(Interruption::Retry)).await;
    let handed = drain(&mut first);
    assert_eq!(handed.len(), 1);

    // The worker's stream closes; it opens another within the grace.
    world.driver.detach_stream(&edge, &StreamId("s1".into()));
    world.settle().await;
    world.advance(30_000);
    let mut second = world
        .driver
        .attach_stream(&edge, TenantId::local(), attach("s2", "worker-1", &edge))
        .unwrap();
    world.advance(60_000);
    world.settle().await;
    assert!(
        drain(&mut second).is_empty(),
        "the task stays the worker's: not handed again"
    );
    world
        .complete(&handed[0].token, json!({ "answer": 42 }))
        .await
        .unwrap();
    world.settle().await;
    let view = world.view(&run).await;
    assert_eq!(view.head.status, RunStatus::Completed);
    assert_eq!(world.history(&run).await[2].recorded_at, world.now());
}

#[tokio::test]
async fn a_stream_that_does_not_come_back_has_its_tasks_handed_again_or_failed() {
    let (mut world, edge) = streams_world().await;
    let mut first = world
        .driver
        .attach_stream(&edge, TenantId::local(), attach("s1", "worker-1", &edge))
        .unwrap();
    world.settle().await;
    let retried = world.run(host_call(Interruption::Retry)).await;
    let at_most_once = world.run(host_call(Interruption::AtMostOnce)).await;
    let handed = drain(&mut first);
    assert_eq!(handed.len(), 2);

    // The worker dies: its stream closes for good. Another worker opens a stream.
    world.driver.detach_stream(&edge, &StreamId("s1".into()));
    let mut second = world
        .driver
        .attach_stream(&edge, TenantId::local(), attach("s2", "worker-2", &edge))
        .unwrap();
    world.settle().await;
    assert!(
        drain(&mut second).is_empty(),
        "within the grace, the tasks wait for their worker"
    );
    world.advance(61_000);
    world.settle().await;

    // Under retry: handed again, attempt + 1, with a new token; the old one is gone.
    let again = drain(&mut second);
    assert_eq!(again.len(), 1, "{again:?}");
    assert_eq!(again[0].task.run, retried);
    assert_eq!(again[0].attempt, 2);
    let old = handed.iter().find(|task| task.task.run == retried).unwrap();
    assert_ne!(again[0].token, old.token);
    assert!(matches!(
        world.complete(&old.token, json!({})).await,
        Err(EdgeError::Refused(Refusal::TaskGone))
    ));
    world
        .complete(&again[0].token, json!({ "answer": 7 }))
        .await
        .unwrap();
    world.settle().await;
    assert_eq!(world.view(&retried).await.head.status, RunStatus::Completed);

    // Under at_most_once: the call may have taken effect, so it is not handed again.
    let view = world.view(&at_most_once).await;
    assert_eq!(view.head.status, RunStatus::Completed);
    let history = world.history(&at_most_once).await;
    let rewind_core::Event::Command(rewind_core::Command::CompleteRun { output }) =
        &history.last().unwrap().event
    else {
        panic!("{history:?}")
    };
    assert_eq!(
        output.value().unwrap()["kind"],
        json!(ToolFailureKind::OutcomeUnknown.as_str())
    );
    let _ = TaskState::Approval;
    let _ = RunRef::new(TenantId::local(), retried.id);
    let _ = PartitionReply::Done(Ok(true));
}

/// A stream held at a node that dies is closed with it: no edge is left to say so, and the
/// organization actor learns it from the members. Its tasks get their grace, then are handed to
/// a stream of another node.
#[tokio::test]
async fn a_stream_at_a_node_that_dies_has_its_tasks_handed_again_after_the_grace() {
    let (mut world, dying) = streams_world().await;
    let survivor = node("node-a");
    world.at = survivor.clone();
    let mut first = world
        .driver
        .attach_stream(&dying, TenantId::local(), attach("s1", "worker-1", &dying))
        .unwrap();
    world.settle().await;
    let run = world.run(host_call(Interruption::Retry)).await;
    let handed = drain(&mut first);
    assert_eq!(handed.len(), 1);

    world.driver.kill(&dying);
    world.settle().await;
    let mut second = world
        .driver
        .attach_stream(
            &survivor,
            TenantId::local(),
            attach("s2", "worker-2", &survivor),
        )
        .unwrap();
    world.advance(61_000);
    world.settle().await;
    let again = drain(&mut second);
    assert_eq!(again.len(), 1, "handed again to the live stream: {again:?}");
    assert_eq!(again[0].task.run, run);
    world
        .complete(&again[0].token, json!({ "answer": 1 }))
        .await
        .unwrap();
    world.settle().await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
}

#[tokio::test]
async fn a_tenant_s_activations_run_on_its_pool_and_the_others_on_the_shared_one() {
    let acme = TenantId::new("acme").unwrap();
    let pools = std::collections::BTreeMap::from([(acme.clone(), "acme".to_owned())]);
    let mut owner = config("node-a", 1);
    owner.pools = pools.clone();
    let mut pool = config("node-b", 1);
    pool.executors_only = true;
    pool.pool = Some("acme".into());
    pool.pools = pools;
    let mut world = World::with(waiter(), RecordingDispatcher::echo(), 1, &[owner, pool]).await;
    let (a, b) = (node("node-a"), node("node-b"));

    let mut of_acme = request("waiter", json!({}));
    of_acme.tenant = acme.clone();
    let run = world.run(of_acme).await;
    assert_eq!(world.events(&run).await, ["run_started", "signal_awaited"]);
    assert_eq!(world.driver.node(&a).unwrap().cached_instances(), 0);
    assert_eq!(
        world.driver.node(&b).unwrap().cached_instances(),
        1,
        "the tenant's run ran in its pool"
    );

    // A tenant without a pool of its own runs on the shared pool: the owner's executors.
    let shared = world.run(request("waiter", json!({}))).await;
    assert_eq!(
        world.events(&shared).await,
        ["run_started", "signal_awaited"]
    );
    assert_eq!(world.driver.node(&a).unwrap().cached_instances(), 1);
    assert_eq!(world.driver.node(&b).unwrap().cached_instances(), 1);

    world
        .signal(&run, "go", json!({ "ok": true }), None)
        .await
        .unwrap();
    world.settle().await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
}

#[tokio::test]
async fn an_executor_node_that_joins_is_seen_at_once() {
    // The owner does not run the run's epoch, and no node does yet: its activation fails, and
    // is tried again by a timer.
    let mut owner = config("node-a", 1);
    owner.epochs = Some(BTreeSet::from([Epoch(9)]));
    let mut world = World::with(waiter(), RecordingDispatcher::echo(), 1, &[owner]).await;
    let a = node("node-a");
    let run = world.start(request("waiter", json!({}))).await.unwrap().run;
    world.settle().await;
    assert!(
        world.events(&run).await.is_empty(),
        "no executor runs its epoch"
    );
    assert_eq!(world.view(&run).await.head.failures, 1);

    // An executors-only node of that epoch joins: the owner hears it at once, before anything
    // asks it to read the members again.
    let mut remote = config("node-b", 1);
    remote.executors_only = true;
    remote.epochs = Some(BTreeSet::from([Epoch(1)]));
    let plane = std::sync::Arc::clone(&world.plane);
    let now = world.now();
    world.driver.add_node(plane, remote).await.unwrap();
    world.settle().await;
    assert_eq!(world.now(), now);
    let members: Vec<String> = world
        .driver
        .node(&a)
        .unwrap()
        .members()
        .iter()
        .map(|member| member.id.to_string())
        .collect();
    assert_eq!(members, ["node-a", "node-b"]);

    // The activation tried again goes to it.
    world.idle().await;
    assert_eq!(world.events(&run).await, ["run_started", "signal_awaited"]);
    assert_eq!(
        world
            .driver
            .node(&node("node-b"))
            .unwrap()
            .cached_instances(),
        1
    );
}

#[tokio::test]
async fn a_node_whose_membership_ends_gives_its_partitions_up_at_once() {
    let mut world = two_nodes(sleeper(), RecordingDispatcher::echo(), 4).await;
    let (a, b) = (node("node-a"), node("node-b"));
    let (namespace, partition) = namespace_on(&world, &a);
    world.at = b.clone();
    let run = world
        .start(in_namespace(request("sleeper", json!({})), &namespace))
        .await
        .unwrap()
        .run;
    world.settle().await;
    assert!(!world.driver.node(&a).unwrap().partitions().is_empty());

    // Node a's liveness ends (its lock's connection is lost) while its process lives on.
    assert!(world.cluster.kill(&a));
    world.settle().await;
    let lost = world.driver.node(&a).unwrap();
    assert!(lost.is_lost());
    assert!(
        lost.partitions().is_empty(),
        "it holds no partition any more"
    );
    assert_eq!(world.driver.map(&b).unwrap().owner(partition), Some(&b));

    world.advance(1_000);
    world.idle().await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
}

#[tokio::test]
async fn an_open_that_ends_after_the_next_owner_s_leaves_the_partition_to_its_owner() {
    use rewind_engine::ports::PartitionStore as _;
    let mut world = two_nodes(waiter(), RecordingDispatcher::echo(), 4).await;
    let (a, b) = (node("node-a"), node("node-b"));
    let (namespace, partition) = namespace_on(&world, &b);
    // Node a's open of the partition, begun while the map named it, ends after node b's: it
    // fences node b, whose next commit is refused.
    let late = world.plane.partitions.open(partition, &a).await.unwrap();
    drop(late);
    world.at = a.clone();
    let run = world
        .run(in_namespace(request("waiter", json!({})), &namespace))
        .await;
    // Node b read the map again, which still names it, and opened the partition again.
    assert_eq!(world.events(&run).await, ["run_started", "signal_awaited"]);
    assert_eq!(world.driver.map(&a).unwrap().owner(partition), Some(&b));
    assert!(
        world
            .driver
            .node(&b)
            .unwrap()
            .partitions()
            .contains(&partition)
    );
    assert!(
        !world
            .driver
            .node(&a)
            .unwrap()
            .partitions()
            .contains(&partition)
    );
}
