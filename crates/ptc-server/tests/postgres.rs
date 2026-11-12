//! The API end to end on Postgres (Docker): a server with the api role only and two servers with
//! the worker role only share one database and one blob directory. Runs started through the api
//! server are activated by the workers; host-tool tasks are long-polled through the api server;
//! quotas hold across them. Skipped, with a message, when Docker is absent, unless
//! `PTC_REQUIRE_DOCKER` is set (CI): they fail then.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ptc_api::admin::Role;
use ptc_api::common::RunStatus;
use ptc_api::runs::StartRun;
use ptc_api::tasks::{CompleteTask, TaskQuery};
use ptc_api::toolsets::{CreateToolset, HostTool};
use serde_json::json;
use support::{TestServer, wait};

/// A Postgres container, removed on drop.
struct Postgres {
    name: String,
    address: String,
}

impl Postgres {
    fn start() -> Option<Self> {
        let docker = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .output();
        if !matches!(&docker, Ok(output) if output.status.success()) {
            eprintln!("SKIPPED: the Postgres tests of ptc-server need Docker, which is absent");
            assert!(
                std::env::var_os("PTC_REQUIRE_DOCKER").is_none(),
                "PTC_REQUIRE_DOCKER is set: Docker must be running"
            );
            return None;
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("ptc-server-postgres-{}-{nanos}", std::process::id());
        run(&[
            "run",
            "--detach",
            "--rm",
            "--name",
            &name,
            "--publish",
            "127.0.0.1::5432",
            "--tmpfs",
            "/var/lib/postgresql/data",
            "--env",
            "POSTGRES_USER=ptc",
            "--env",
            "POSTGRES_PASSWORD=ptc-secret",
            "postgres:17",
        ]);
        let mut postgres = Postgres {
            name,
            address: String::new(),
        };
        postgres.address = run(&["port", &postgres.name, "5432/tcp"])
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !Command::new("docker")
            .args([
                "exec",
                &postgres.name,
                "psql",
                "-h",
                "127.0.0.1",
                "-U",
                "ptc",
                "-d",
                "postgres",
                "-tAc",
                "SELECT 1",
            ])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            assert!(Instant::now() < deadline, "Postgres did not start in time");
            std::thread::sleep(Duration::from_millis(200));
        }
        Some(postgres)
    }

    fn url(&self) -> String {
        format!("postgres://ptc:ptc-secret@{}/postgres", self.address)
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .output();
    }
}

fn run(args: &[&str]) -> String {
    let output = Command::new("docker").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "docker {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

const SLEEP: &str = r#"import { sleep, signal } from "ptc";
export default async function main(input: { n: number }) {
  await sleep(100);
  const go = await signal<number>("go");
  return { n: input.n, go };
}
"#;

const ASK: &str = r#"import { ask_user } from "ptc:tools";
export default async function main() {
  return { answer: await ask_user({ question: "ready?" }) };
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_api_server_and_two_workers_share_one_postgres() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let shared = tempfile::tempdir().unwrap();
    let blobs = shared.path().join("blobs");
    let url = postgres.url();
    let worker = |name: &'static str| {
        let (url, blobs) = (url.clone(), blobs.clone());
        async move {
            TestServer::start_on(
                tempfile::tempdir().unwrap(),
                &url,
                &blobs,
                &[
                    "--role",
                    "worker",
                    "--worker-name",
                    name,
                    "--run-quota",
                    "20",
                ],
            )
            .await
        }
    };
    let first = worker("worker-1").await;
    let second = worker("worker-2").await;
    let api = TestServer::start_on(
        tempfile::tempdir().unwrap(),
        &url,
        &blobs,
        &["--role", "api", "--run-quota", "20"],
    )
    .await;
    let info = api.admin.server_info().await.unwrap();
    assert_eq!(info.roles, [ptc_api::admin::ServerRole::Api]);
    let harness = api.key(Role::Harness, &[]).await;

    let mut runs = Vec::new();
    for n in 0..6 {
        let mut request = StartRun::new(SLEEP);
        request.input = json!({ "n": n });
        runs.push(harness.start_run(&request, None).await.unwrap().id);
    }
    for (n, run) in runs.iter().enumerate() {
        harness
            .signal_run(run, "go", &json!(n * 10), None)
            .await
            .unwrap();
    }
    for (n, run) in runs.iter().enumerate() {
        let ended = harness.get_run(run, wait("60s")).await.unwrap();
        assert_eq!(ended.status, RunStatus::Completed, "{ended:?}");
        assert_eq!(
            ended.output.unwrap().value,
            Some(json!({ "n": n, "go": n * 10 }))
        );
    }

    // A host tool, long-polled through the api server.
    let toolset = harness
        .create_toolset(&CreateToolset {
            host: vec![HostTool {
                name: "ask_user".into(),
                description: "Ask the user.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "question": { "type": "string" } },
                    "required": ["question"]
                }),
                output_schema: None,
                policy: None,
            }],
            ..CreateToolset::default()
        })
        .await
        .unwrap();
    let mut request = StartRun::new(ASK);
    request.toolset = Some(toolset.id.clone());
    let run = harness.start_run(&request, None).await.unwrap();
    let task = harness
        .claim_tasks(&TaskQuery {
            wait: Some("30s".parse().unwrap()),
            ..TaskQuery::default()
        })
        .await
        .unwrap()
        .tasks
        .remove(0);
    harness
        .complete_task(
            &task.token,
            &CompleteTask {
                result: json!("yes"),
            },
        )
        .await
        .unwrap();
    let ended = harness.get_run(&run.id, wait("60s")).await.unwrap();
    assert_eq!(
        ended.output.unwrap().value,
        Some(json!({ "answer": "yes" }))
    );

    api.stop().await;
    first.stop().await;
    second.stop().await;
    drop(postgres);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookups_with_a_nul_character_match_nothing_on_postgres() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let shared = tempfile::tempdir().unwrap();
    let server = TestServer::start_on(
        tempfile::tempdir().unwrap(),
        &postgres.url(),
        &shared.path().join("blobs"),
        &[],
    )
    .await;
    support::nul_lookups_match_nothing(&server).await;
    server.stop().await;
}
