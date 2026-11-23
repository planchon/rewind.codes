//! Run handles on the run plane (`archi/06-storage-and-scale.md`, "Run handles"): what a run
//! is the run of, its suffix, which the owner of its partition keeps unique in its namespace,
//! and the run a handle names.

#![allow(clippy::unwrap_used)]

mod support;

use rewind_core::{Event, Lifecycle, RunHandle, SUFFIX_LEN, Script, suffix_len_apart};
use rewind_engine::ForkRun;
use rewind_engine::adapters::{MemoryPartitions, SeededEntropy};
use rewind_engine::ports::link::{
    NodeReply, NodeRequest, PartitionReply, PartitionRequest, RunView, StartAtOwner,
};
use rewind_engine::ports::{Entropy, NamespaceName, RunMetadata, RunRef, TenantId};
use serde_json::json;
use support::plane::{World, config};
use support::{FakeExecutor, RecordingDispatcher, request};

const TRIAGE: &str = "// Triage new issues\nexport default async () => 1";
const PLAIN: &str = "export default async () => 2";

fn programs() -> FakeExecutor {
    let done = |run: &mut support::FakeRun| run.complete(json!(1));
    FakeExecutor::new()
        .with(TRIAGE, done, |_, _| {})
        .with(PLAIN, done, |_, _| {})
}

fn dispatcher() -> RecordingDispatcher {
    RecordingDispatcher::new(|call| Ok(call.args.clone()))
}

async fn find(world: &mut World, handle: &RunHandle) -> Option<RunView> {
    let mut gateway = world.driver.gateway(&world.at.clone());
    world
        .edge
        .find_run(&mut gateway, &TenantId::local(), handle)
        .await
        .unwrap()
}

fn in_namespace(script: &str, namespace: &str) -> rewind_engine::StartRun {
    let mut start = request(script, json!({}));
    start.namespace = NamespaceName::new(namespace).unwrap();
    start
}

#[tokio::test]
async fn a_run_is_named_by_its_namespace_its_origin_and_its_suffix() {
    let mut world = World::new(programs(), dispatcher()).await;
    let triage = world.run(request(TRIAGE, json!({}))).await;
    let view = world.view(&triage).await;
    assert_eq!(view.head.origin, "triage-new-issues", "the comment's title");
    assert_eq!(usize::from(view.head.suffix_len), SUFFIX_LEN);
    let summary = view.summary.as_ref().unwrap();
    assert_eq!(summary.handle(), view.head.handle());
    let handle = view.head.handle();
    assert!(handle.to_string().starts_with("local/triage-new-issues/"));
    assert_eq!(find(&mut world, &handle).await.unwrap().head.run, triage);

    let plain = world.run(request(PLAIN, json!({}))).await;
    assert_eq!(world.view(&plain).await.head.origin, "script");

    // Another origin, another namespace, or a longer suffix name no run.
    let mut other = handle.clone();
    other.origin = "script".into();
    assert!(find(&mut world, &other).await.is_none());
    let mut elsewhere = handle.clone();
    elsewhere.namespace = "bob".into();
    assert!(find(&mut world, &elsewhere).await.is_none());
    let longer: RunHandle = format!("{handle}0").parse().unwrap();
    assert!(find(&mut world, &longer).await.is_none());

    // A fork is the run of what its parent ran, and names its parent by its handle.
    let mut gateway = world.driver.gateway(&world.at.clone());
    let fork = world
        .edge
        .fork(
            &mut gateway,
            ForkRun::new(triage.clone(), Script::new(PLAIN)),
        )
        .await
        .unwrap()
        .run;
    world.idle().await;
    let forked = world.view(&fork).await;
    assert_eq!(forked.head.origin, "triage-new-issues");
    assert_eq!(forked.parent, Some(handle));
    assert_eq!(view.parent, None);
}

#[tokio::test]
async fn runs_that_share_their_first_bits_get_longer_suffixes() {
    let mut world = World::drawing(
        MemoryPartitions::new(),
        programs(),
        dispatcher(),
        1,
        &[config("node-a", 1)],
        SeededEntropy::sharing_suffixes(7),
    )
    .await;
    let mut runs: Vec<RunRef> = Vec::new();
    for _ in 0..3 {
        runs.push(world.run(in_namespace(TRIAGE, "alice")).await);
    }
    let elsewhere = world.run(in_namespace(TRIAGE, "bob")).await;
    let mut handles = Vec::new();
    for run in &runs {
        handles.push(world.view(run).await.head.handle());
    }
    assert_eq!(handles[0].suffix.len(), SUFFIX_LEN, "the first keeps 12");
    assert!(handles[1].suffix.len() > SUFFIX_LEN);
    assert!(handles[2].suffix.len() > SUFFIX_LEN);
    for (run, handle) in runs.iter().zip(&handles) {
        assert_eq!(find(&mut world, handle).await.unwrap().head.run, *run);
    }
    let names: std::collections::BTreeSet<String> =
        handles.iter().map(ToString::to_string).collect();
    assert_eq!(names.len(), 3, "no two handles alike");
    let mut heads = Vec::new();
    for run in &runs {
        heads.push(world.view(run).await.head);
    }
    told_apart(&heads);
    // Another namespace tells its handle apart by its namespace.
    let other = world.view(&elsewhere).await.head.handle();
    assert_eq!(other.suffix.len(), SUFFIX_LEN);
    assert_eq!(other.suffix, handles[0].suffix);
}

/// Starts that reach the owner together, before either is durable, still get handles apart.
#[tokio::test]
async fn starts_in_flight_together_get_handles_apart() {
    let mut world = World::drawing(
        MemoryPartitions::new(),
        programs(),
        dispatcher(),
        1,
        &[config("node-a", 1)],
        SeededEntropy::sharing_suffixes(9),
    )
    .await;
    // A first run stores the script and the toolset the two others run.
    let first = world.run(request(TRIAGE, json!({}))).await;
    let head = world.view(&first).await.head;
    let started = world
        .history(&first)
        .await
        .into_iter()
        .find_map(|entry| match entry.event {
            Event::Lifecycle(Lifecycle::RunStarted(started)) => Some(started),
            _ => None,
        })
        .unwrap();
    let partition = world.local_partition();
    let start = |world: &World| {
        let run = RunRef::new(TenantId::local(), world.plane.entropy.run_id_in(partition));
        NodeRequest::Partition {
            partition,
            request: Box::new(PartitionRequest::Start(Box::new(StartAtOwner {
                run,
                namespace: head.namespace.clone(),
                origin: head.origin.clone(),
                started: started.clone(),
                metadata: RunMetadata::default(),
                start_key: None,
                headline: "export default".into(),
                created_at: world.now(),
            }))),
        }
    };
    let requests = vec![start(&world), start(&world)];
    let at = world.at.clone();
    let replies = world.driver.requests(&at, requests).await.unwrap();
    let mut runs = Vec::new();
    for reply in replies {
        let NodeReply::Partition(reply) = reply else {
            panic!("{reply:?}");
        };
        let PartitionReply::Started(Ok(created)) = *reply else {
            panic!("{reply:?}");
        };
        runs.push(created.run);
    }
    world.idle().await;
    let a = world.view(&runs[0]).await.head;
    let b = world.view(&runs[1]).await.head;
    // Whichever the owner created second was told apart from the other, not durable yet.
    assert!(
        apart(&[head.clone(), a.clone(), b.clone()]) || apart(&[head, b, a]),
        "two runs created together share their handle's suffix"
    );
}

/// Whether each run's suffix, of the runs in the order they were created, is long enough to
/// tell it apart from the runs created before it, which keep theirs.
fn apart(heads: &[rewind_engine::ports::RunHead]) -> bool {
    heads.iter().enumerate().all(|(created, head)| {
        let before = heads[..created].iter().map(|other| other.run.id);
        usize::from(head.suffix_len) >= suffix_len_apart(head.run.id, before)
    })
}

fn told_apart(heads: &[rewind_engine::ports::RunHead]) {
    assert!(apart(heads), "{heads:?}");
}
