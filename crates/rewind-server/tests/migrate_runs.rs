//! `rewind server migrate-runs` on the stores the old tables filled (`archi/08-decisions.md`,
//! "Taken while writing the object-storage spec": the migration): a Postgres database and an
//! embedded `.rewind/`, kept as fixtures (`tests/fixtures/`). The command runs twice, the second
//! time moving nothing; then a server on the result reads every run back as the tables held it,
//! named by the handle they gave it, and lets the live ones go on: a timer fires, signals and a
//! webhook end them, a workflow run's end reaches its starter, harness calls in flight are handed
//! again. The fixtures are as a build of schema version 12 (Postgres) and 10 (Turso) left them,
//! their documents as `rename-data` writes them: the server runs the migrations that give runs
//! their handles, those of the run plane and of the workflow model; between the move and the
//! server, `rename-data`'s step gives every run a workflow.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use clap::Parser;
use rewind_api::Client;
use rewind_api::calls::{CallStreamQuery, CompleteCall};
use rewind_api::common::RunStatus;
use rewind_api::runs::ExportAs;
use rewind_core::WorkflowId;
use rewind_engine::adapters::SystemEntropy;
use rewind_engine::ports::legacy::{LegacyBlobs, LegacyHistory, LegacyRuns};
use rewind_engine::ports::{CatalogMigration, WorkflowScope};
use rewind_engine::rename::CodeUse;
use rewind_server::{MigrateArgs, ServerArgs, migrate_runs};
use serde_json::{Value, json};
use support::{ADMIN_KEY, Postgres, TaskWorker, copy_tree, eventually};

const ARCHIVED: &str = "63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f";
const COMPLETED: &str = "031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a";
const FAILED: &str = "2af4fb3b-d947-7ebb-a55c-0d794c298dc9";
const CANCELLED: &str = "551d70f7-e034-761a-b0bb-4c12308730b1";
const TIMER: &str = "1f2f83db-2016-79eb-9387-81ecf0cd83a5";
const SIGNAL: &str = "a54b0d91-1850-7392-919d-d7d434ec2c56";
const INBOX: &str = "18b72207-70d2-77c5-8642-0c8599a04aef";
const BIG_LIVE: &str = "2c582b9e-1961-750f-be22-215ba1ff1918";
const WEBHOOK: &str = "6e4cacb5-4098-7d30-88c3-933c639bdaad";
const PARENT: &str = "85df88e9-7e9c-7d21-8f60-46463925e0b1";
const CHILD: &str = "e1dced5d-e291-7195-be97-f0d0ea8e5bf6";
const QUARANTINED: &str = "55872074-cf3c-79e1-8326-dad575e80391";
const DEFERRED: &str = "f9c322e4-79c4-735d-ae70-d0f08fa8a8b1";
const FORK: &str = "e206c2c7-f3cd-701f-b2d5-81899e11fb5d";
const HOST: &str = "9e5651b0-ef95-7636-82c9-c08ce3ca2634";

/// The payload above 64 KiB the ended and live runs hold: `{"text": "b" × 100 000}`.
const BIG: &str = "fb3a224297affd9c23852a92179568da187a161043a76d58ca6b12ef6e529618";
/// The test written by hand.
const TEST: &str = "fa89ec08d9fd85b9458a351547bb1c3262ebcc720090b21ba2a448a8a4710016";

const CLUSTER_SECRET: &str = "the-cluster-secret-of-the-tests-0123456789";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn master_key() -> String {
    base64::engine::general_purpose::STANDARD.encode([9u8; 32])
}

/// The event names of each run's history, and its handle, as the set-aside tables hold them.
type Histories = BTreeMap<String, (Vec<&'static str>, String)>;

/// The histories of the set-aside runs, and what each used, as `rename-data` reads it from the
/// runs' records.
async fn histories<L: LegacyRuns, B: LegacyBlobs>(
    legacy: &L,
    blobs: &B,
) -> (Histories, Vec<CodeUse>) {
    let mut histories = BTreeMap::new();
    let mut uses = Vec::new();
    for run in legacy.runs(None, 100).await.unwrap() {
        uses.push(CodeUse {
            tenant: run.run.tenant.clone(),
            namespace: run.namespace.clone(),
            run: run.run.id,
            code: run.code,
            action_set: run.action_set,
            secrets: run.metadata.secrets.clone(),
            labels: run.metadata.allowed_labels.clone(),
            version: run
                .metadata
                .workflow
                .as_ref()
                .map(|workflow| workflow.digest),
            started_at: run.created_at,
        });
        let handle = rewind_core::RunHandle::new(
            run.namespace.as_str(),
            &run.origin,
            run.run.id,
            usize::from(run.suffix_len),
        )
        .to_string();
        let events = match run.history {
            LegacyHistory::Events(events) => events,
            LegacyHistory::Archived(document) => blobs
                .export(&run.run.tenant, document)
                .await
                .unwrap()
                .unwrap(),
        };
        histories.insert(
            run.run.id.to_string(),
            (
                events.iter().map(|entry| entry.event.name()).collect(),
                handle,
            ),
        );
    }
    (histories, uses)
}

/// `rename-data`'s step for the workflows, on the catalog the runs moved beside: every run gets
/// a workflow, the saved `triage` that one namespace ran becomes that namespace's, and the step
/// run again changes nothing.
async fn rename_workflows<C: CatalogMigration>(catalog: &C, uses: &[CodeUse]) {
    let now = rewind_core::Timestamp(1_800_000_000_000);
    let renamed = rewind_engine::rename::workflows(catalog, &SystemEntropy, uses, now)
        .await
        .unwrap();
    assert_eq!(renamed.runs.len(), uses.len(), "{renamed:?}");
    let child = renamed.runs[&CHILD.parse::<rewind_core::RunId>().unwrap()].clone();
    let tenant = &uses[0].tenant;
    let triage = WorkflowId::of_saved(tenant.as_str(), "triage");
    assert_eq!(
        (&child.id, child.name.as_deref(), child.version),
        (&triage, Some("triage"), 1)
    );
    let ran: std::collections::BTreeSet<_> = uses
        .iter()
        .filter(|used| used.run.to_string() == CHILD)
        .map(|used| used.namespace.clone())
        .collect();
    let namespace = ran.into_iter().next().unwrap();
    assert_eq!(
        renamed.scoped,
        [(tenant.clone(), triage.clone(), namespace.clone())],
        "the saved workflow one namespace ran is that namespace's"
    );
    let moved = catalog.workflow(tenant, &triage).await.unwrap().unwrap();
    assert_eq!(moved.scope, WorkflowScope::Namespace(namespace));
    // Every other code that ran got a workflow of the namespace of its first run.
    assert!(!renamed.created.is_empty());
    for (_, created) in &renamed.created {
        assert!(
            matches!(created.scope, WorkflowScope::Namespace(_)),
            "{created:?}"
        );
        assert_eq!(created.latest, 1);
    }
    for used in uses {
        let workflow = &renamed.runs[&used.run];
        assert_eq!(
            renamed.codes[&(used.tenant.clone(), used.namespace.clone(), used.code)],
            workflow.id,
            "{used:?}"
        );
    }

    let again = rewind_engine::rename::workflows(catalog, &SystemEntropy, uses, now)
        .await
        .unwrap();
    assert_eq!(again.runs, renamed.runs);
    assert!(
        again.scoped.is_empty() && again.created.is_empty(),
        "{again:?}"
    );
}

fn status(run: &rewind_api::runs::Run) -> RunStatus {
    run.status
}

fn output(run: &rewind_api::runs::Run) -> Value {
    run.output
        .as_ref()
        .and_then(|payload| payload.value.clone())
        .unwrap_or(Value::Null)
}

/// The run once it ended, within 30 s.
async fn ended(client: &Client, run: &str) -> rewind_api::runs::Run {
    eventually(|| async {
        let found = client.get_run(run, None).await.unwrap();
        (!matches!(found.status, RunStatus::Pending | RunStatus::Active)).then_some(found)
    })
    .await
}

/// Every run reads back as the tables held it, and the live ones go on.
async fn check(url: &str, client: &Client, histories: &Histories) {
    assert_eq!(histories.len(), 15, "{histories:?}");
    for (run, (events, handle)) in histories {
        // The run keeps the handle the tables gave it, and is found by it. A server that has
        // just started may not own every partition yet: it answers 503, to be asked again.
        let found = eventually(|| async {
            match client.get_run(handle, None).await {
                Ok(found) => Some(found),
                Err(rewind_api::Error::Problem(problem)) if problem.status == 503 => None,
                Err(error) => panic!("{handle}: {error}"),
            }
        })
        .await;
        assert_eq!((&found.id, &found.handle), (run, handle));
        let export = client.export_run(run, ExportAs::Run).await.unwrap();
        let read: Vec<String> = export["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["event"]["type"].as_str().unwrap().to_owned())
            .collect();
        // A live run may already have gone on: what the tables held comes first.
        assert!(
            read.len() >= events.len() && read[..events.len()] == events[..],
            "the history of {run}: {read:?}, expected {events:?} first"
        );
    }

    // The ended runs.
    let run = client.get_run(ARCHIVED, None).await.unwrap();
    assert_eq!(status(&run), RunStatus::Completed);
    assert_eq!(
        output(&run),
        json!({ "a": { "ok": true }, "m": 2_000, "b": 100_000, "n": 1 })
    );
    let run = client.get_run(COMPLETED, None).await.unwrap();
    assert_eq!(
        output(&run),
        json!({ "a": { "ok": true }, "m": 2_000, "b": 100_000, "tail": "bbbbb" })
    );
    for holder in [ARCHIVED, COMPLETED, BIG_LIVE] {
        assert_eq!(
            client.get_payload(BIG, holder).await.unwrap(),
            json!({ "text": "b".repeat(100_000) }),
            "{holder} holds its payload above 64 KiB"
        );
    }
    assert_eq!(
        status(&client.get_run(FAILED, None).await.unwrap()),
        RunStatus::Failed
    );
    assert_eq!(
        status(&client.get_run(CANCELLED, None).await.unwrap()),
        RunStatus::Cancelled
    );
    let fork = client.get_run(FORK, None).await.unwrap();
    assert_eq!(output(&fork), json!({ "fixed": true }));
    assert_eq!(fork.source.map(|parent| parent.run), Some(FAILED.into()));

    // A timer that came due while nothing ran fires; a signal in the inbox is delivered.
    assert_eq!(
        output(&ended(client, TIMER).await),
        json!({ "slept": true, "size": 3_000 })
    );
    assert_eq!(
        output(&ended(client, INBOX).await),
        json!({ "first": { "n": 1 }, "second": { "n": 2 } })
    );

    // Signals end the runs that wait for them; a signal key moved with its run.
    let again = client
        .signal_run(
            INBOX,
            "first",
            &json!({ "n": 3 }),
            Some(&"key-first".parse().unwrap()),
        )
        .await;
    assert!(again.is_err() || again.unwrap().duplicate);
    client
        .signal_run(SIGNAL, "approval", &json!({ "ok": true }), None)
        .await
        .unwrap();
    assert_eq!(
        output(&ended(client, SIGNAL).await),
        json!({ "approval": { "ok": true } })
    );
    client
        .signal_run(BIG_LIVE, "done", &json!("yes"), None)
        .await
        .unwrap();
    assert_eq!(
        output(&ended(client, BIG_LIVE).await),
        json!({ "b": 100_000, "m": 2_000, "done": "yes" })
    );

    // The workflow run its starter waits for passed its deadline (30 days, the default) while
    // nothing ran: its deadline's timer moved with it and cancels it, and its end reaches the run
    // that waits for it, whose `result()` rejects.
    assert_eq!(status(&ended(client, CHILD).await), RunStatus::Cancelled);
    let parent = ended(client, PARENT).await;
    assert_eq!(status(&parent), RunStatus::Failed, "{parent:?}");

    // The harness calls in flight, a deferred call among them, are handed again.
    let mut worker = TaskWorker::open(
        client,
        &CallStreamQuery {
            actions: vec!["ask_human".into(), "slow".into()],
            slots: Some(10),
            ..CallStreamQuery::default()
        },
    )
    .await;
    let mut handed = BTreeMap::new();
    while handed.len() < 2 {
        let task = worker.task().await;
        handed.insert(task.action.clone(), task);
    }
    for (action, task) in &handed {
        client
            .complete_call(
                &task.token,
                &CompleteCall {
                    result: json!({ "from": action }),
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(handed["ask_human"].run_id, HOST);
    assert_eq!(handed["slow"].run_id, DEFERRED);
    assert_eq!(
        output(&ended(client, HOST).await),
        json!({ "answer": { "from": "ask_human" } })
    );
    assert_eq!(
        output(&ended(client, DEFERRED).await),
        json!({ "from": "slow" })
    );

    // The signal URL the code was given still signals it: its token names no run, and the
    // partition that holds its hash answers.
    let export = client.export_run(WEBHOOK, ExportAs::Run).await.unwrap();
    let hook = export["events"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|entry| entry["event"]["result"]["value"]["url"].as_str())
        .unwrap()
        .to_owned();
    let token = hook.rsplit('/').next().unwrap();
    let response = reqwest::Client::new()
        .post(format!("{url}/hooks/{token}"))
        .json(&json!({ "merged": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    assert_eq!(
        output(&ended(client, WEBHOOK).await),
        json!({ "url": hook, "merged": true })
    );

    // The quarantined run is not lost: it waits, quarantined or tried again by this build.
    let quarantined = client.get_run(QUARANTINED, None).await.unwrap();
    assert!(
        matches!(quarantined.status, RunStatus::Active),
        "{quarantined:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runs_of_an_embedded_directory_move_into_the_run_plane() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".rewind");
    copy_tree(&fixture("embedded/rewind"), &dir);
    let expected = {
        let copy = temp.path().join("expected");
        copy_tree(&fixture("embedded/rewind"), &copy);
        let store = rewind_store_turso::TursoStore::open(
            copy.join("journal.db"),
            rewind_store_turso::MasterKey::from_bytes([9; 32]),
        )
        .await
        .unwrap();
        histories(
            &store.legacy(),
            &rewind_blob::LegacyFiles::new(copy.join("blobs")),
        )
        .await
    };
    let (expected, uses) = expected;
    let dir_text = dir.display().to_string();

    // The embedded server refuses a directory whose runs did not move, and says how.
    let Err(refused) = rewind_server::start(
        ServerArgs::try_parse_from(["rewind server", "--embedded", "--dir", &dir_text]).unwrap(),
    )
    .await
    else {
        panic!("a directory of the old tables is refused");
    };
    assert_eq!(rewind_server::exit_code(&refused), 70);
    assert!(
        refused
            .to_string()
            .contains("rewind server migrate-runs --embedded"),
        "{refused}"
    );

    let migrate = || {
        MigrateArgs::try_parse_from([
            "rewind server migrate-runs",
            "--embedded",
            "--dir",
            &dir_text,
        ])
        .unwrap()
    };
    let report = migrate_runs(migrate()).await.unwrap();
    assert_eq!(report.moved.len(), 15, "{report:?}");
    assert!(report.failed.is_empty() && report.dropped, "{report:?}");
    let again = migrate_runs(migrate()).await.unwrap();
    assert!(
        again.moved.is_empty() && again.failed.is_empty(),
        "{again:?}"
    );

    // The layout of the run plane, and nothing of the old tables's.
    assert!(!dir.join("journal.db").exists());
    assert!(dir.join("control.db").exists());
    assert!(dir.join("partitions/0").is_dir());
    assert!(
        dir.join(format!("tests/local/sha256/{}/{TEST}", &TEST[..2]))
            .exists()
    );
    assert!(!dir.join("blobs/tests").exists());
    assert!(!dir.join("blobs/local/sha256").exists());
    for run in [ARCHIVED, COMPLETED, FAILED, CANCELLED, FORK] {
        assert!(
            dir.join(format!("runs/local/{run}/1.json")).exists(),
            "{run}"
        );
    }
    for run in [ARCHIVED, COMPLETED, BIG_LIVE] {
        assert!(
            dir.join(format!("blobs/local/{run}/sha256/{}/{BIG}", &BIG[..2]))
                .exists(),
            "{run}"
        );
    }
    let store = rewind_store_turso::TursoStore::open(
        dir.join("control.db"),
        rewind_store_turso::MasterKey::from_bytes([9; 32]),
    )
    .await
    .unwrap();
    assert!(store.legacy().runs(None, 1).await.unwrap().is_empty());
    rename_workflows(&store.catalog(), &uses).await;
    drop(store);

    // A server on the directory: the runs read back, and the live ones go on.
    let key = master_key();
    let bucket = dir.display().to_string();
    let store = format!("turso:{}", dir.join("control.db").display());
    let running = rewind_server::start(
        ServerArgs::try_parse_from([
            "rewind server",
            "--store",
            &store,
            "--bucket",
            &bucket,
            "--master-key",
            &key,
            "--admin-key",
            ADMIN_KEY,
            "--admin-organization",
            "local",
            "--listen",
            "127.0.0.1:0",
            "--executors",
            "2",
        ])
        .unwrap(),
    )
    .await
    .unwrap();
    let url = running.url().unwrap();
    let client = Client::remote(&url, Some(ADMIN_KEY)).unwrap();
    check(&url, &client, &expected).await;
    running.stop().await;

    // The embedded server starts on it.
    let running = rewind_server::start(
        ServerArgs::try_parse_from(["rewind server", "--embedded", "--dir", &dir_text]).unwrap(),
    )
    .await
    .unwrap();
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runs_of_a_postgres_database_move_into_the_partitions_and_the_archive() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    postgres.load(&fixture("postgres/dump.sql"));
    let temp = tempfile::tempdir().unwrap();
    let bucket = temp.path().join("bucket");
    copy_tree(&fixture("postgres/bucket"), &bucket);
    let expected = {
        let store = rewind_store_postgres::PostgresStore::connect(
            &rewind_store_postgres::Config::new(postgres.url()),
            rewind_store_postgres::MasterKey::from_bytes([9; 32]),
        )
        .await
        .unwrap();
        let expected = histories(&store.legacy(), &rewind_blob::LegacyFiles::new(&bucket)).await;
        store.close().await;
        expected
    };
    let (expected, uses) = expected;
    let key = master_key();
    let url = postgres.url();
    let bucket_text = bucket.display().to_string();
    let options = |command: &str| {
        let mut options = vec![
            command.to_owned(),
            "--store".into(),
            url.clone(),
            "--bucket".into(),
            bucket_text.clone(),
            "--partitions".into(),
            "4".into(),
            "--master-key".into(),
            key.clone(),
            "--cluster-secret".into(),
            CLUSTER_SECRET.into(),
        ];
        if command == "rewind server" {
            options.extend(
                [
                    "--link-listen",
                    "127.0.0.1:0",
                    "--admin-key",
                    ADMIN_KEY,
                    "--admin-organization",
                    "acme",
                    "--listen",
                    "127.0.0.1:0",
                    "--executors",
                    "2",
                ]
                .map(str::to_owned),
            );
        }
        options
    };
    let report =
        migrate_runs(MigrateArgs::try_parse_from(options("rewind server migrate-runs")).unwrap())
            .await
            .unwrap();
    assert_eq!(report.moved.len(), 15, "{report:?}");
    assert!(report.failed.is_empty() && report.dropped, "{report:?}");
    let partitions: std::collections::BTreeSet<_> =
        report.moved.iter().map(|moved| moved.partition).collect();
    assert_eq!(partitions.len(), 2, "alice and bob have a partition each");
    let again =
        migrate_runs(MigrateArgs::try_parse_from(options("rewind server migrate-runs")).unwrap())
            .await
            .unwrap();
    assert!(again.moved.is_empty(), "{again:?}");
    assert_eq!(
        postgres.psql_scalar("SELECT count(*) FROM pg_namespace WHERE nspname = 'legacy'"),
        "0",
        "the schema legacy is dropped"
    );
    assert_eq!(
        postgres.psql_scalar("SELECT count(*) FROM legacy_runs"),
        "15",
        "every version 7 id finds its partition"
    );
    for run in [ARCHIVED, COMPLETED, FAILED, CANCELLED, FORK] {
        assert!(
            bucket.join(format!("runs/acme/{run}/1.json")).exists(),
            "{run}"
        );
    }
    {
        let store = rewind_store_postgres::PostgresStore::connect(
            &rewind_store_postgres::Config::new(postgres.url()),
            rewind_store_postgres::MasterKey::from_bytes([9; 32]),
        )
        .await
        .unwrap();
        rename_workflows(&store.catalog(), &uses).await;
        store.close().await;
    }

    let running =
        rewind_server::start(ServerArgs::try_parse_from(options("rewind server")).unwrap())
            .await
            .unwrap();
    let server = running.url().unwrap();
    let client = Client::remote(&server, Some(ADMIN_KEY)).unwrap();
    check(&server, &client, &expected).await;
    running.stop().await;
}
