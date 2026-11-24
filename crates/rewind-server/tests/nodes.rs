//! Several nodes of one deployment, each a server in this process, on Postgres and MinIO in
//! Docker, linked over HTTP/2 (`archi/07-code-architecture.md`, "Tests", `rewind-server`'s row):
//! runs started at one node and read, signalled and answered at another; the task stream and
//! the change stream across nodes; a node that joins and one that leaves; a tenant's pool of
//! executors-only nodes; a node killed while its runs are activated. Skipped, with a message, when Docker is absent, unless
//! `REWIND_REQUIRE_DOCKER` is set (CI): they fail then.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use clap::Parser;
use futures_util::StreamExt;
use rewind_api::Client;
use rewind_api::access::{BuiltInRole, CreateApiKey};
use rewind_api::common::{Labels, RunStatus};
use rewind_api::events::ChangeEvent;
use rewind_api::runs::StartRun;
use rewind_api::tasks::{CompleteTask, TaskStreamQuery};
use rewind_api::toolsets::{CreateToolset, HostTool};
use rewind_server::{Running, ServerArgs};
use serde_json::json;
use support::{ADMIN_KEY, Postgres, TaskWorker, eventually, wait};

const SECRET: &str = "the-cluster-secret-of-the-tests-0123456789";
const USER: &str = "rewind";
const PASSWORD: &str = "rewind-secret";
const MINIO_IMAGE: &str = "cgr.dev/chainguard/minio:latest";
const MC_IMAGE: &str = "cgr.dev/chainguard/minio-client:latest";

const SIGNAL: &str = r#"import { signal } from "rewind";
export default async function main(input: { n: number }) {
  const go = await signal<number>("go");
  return { n: input.n, go };
}
"#;

const ASK: &str = r#"import { ask_user } from "rewind:tools";
export default async function main() {
  return { answer: await ask_user({ question: "ready?" }) };
}
"#;

/// A script whose activation computes for a while before it sleeps and ends.
const BUSY: &str = r#"import { sleep } from "rewind";
export default async function main(input: { n: number }) {
  let x = 0;
  for (let i = 0; i < 30_000_000; i++) { x = (x + i) % 1_000_003; }
  await sleep(50);
  return { n: input.n, x };
}
"#;

/// Postgres and MinIO, and the nodes on them.
struct Deployment {
    postgres: Postgres,
    minio: Minio,
}

/// A node: a server in this process.
struct Node {
    running: Option<Running>,
    url: String,
}

impl Deployment {
    fn start() -> Option<Self> {
        let postgres = Postgres::start()?;
        let minio = Minio::start()?;
        Some(Self { postgres, minio })
    }

    /// A node of the deployment, of 4 partitions.
    async fn node(&self, id: &str) -> Node {
        self.node_with(id, &[]).await
    }

    /// A node of the deployment, of 4 partitions, with more options.
    async fn node_with(&self, id: &str, more: &[&str]) -> Node {
        let running = self.launch(id, more).await;
        let url = running.url().unwrap();
        Node {
            running: Some(running),
            url,
        }
    }

    /// A node of the deployment started with `more` options.
    async fn launch(&self, id: &str, more: &[&str]) -> Running {
        self.launch_with_key(id, ADMIN_KEY, more).await
    }

    /// A node of the deployment whose operator's key is `admin_key`.
    async fn launch_with_key(&self, id: &str, admin_key: &str, more: &[&str]) -> Running {
        let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        let store = self.postgres.url();
        let mut args = vec![
            "rewind server",
            "--store",
            &store,
            "--bucket",
            "s3://rewind",
            "--s3-endpoint",
            &self.minio.endpoint,
            "--s3-region",
            "us-east-1",
            "--s3-allow-http",
            "--s3-access-key-id",
            USER,
            "--s3-secret-access-key",
            PASSWORD,
            "--partitions",
            "4",
            "--cluster-secret",
            SECRET,
            "--link-listen",
            "127.0.0.1:0",
            "--node-id",
            id,
            "--master-key",
            &key,
            "--admin-key",
            admin_key,
            "--admin-tenant",
            "acme",
            "--listen",
            "127.0.0.1:0",
            "--executors",
            "2",
            "--calls",
            "4",
        ];
        args.extend(more);
        let args = ServerArgs::try_parse_from(args).unwrap();
        rewind_server::start(args).await.unwrap()
    }
}

impl Node {
    fn admin(&self) -> Client {
        Client::remote(&self.url, Some(ADMIN_KEY)).unwrap()
    }

    /// A key of the tenant `acme` (made by any node: the control plane is shared).
    async fn key(&self, role: BuiltInRole) -> String {
        self.key_of("acme", role).await
    }

    /// A key of `tenant`.
    async fn key_of(&self, tenant: &str, role: BuiltInRole) -> String {
        self.admin()
            .create_api_key(&CreateApiKey {
                tenant: Some(tenant.into()),
                name: format!("a {} key of the tests", role.as_str()),
                role: role.as_str().into(),
                allowed_labels: Labels::new(),
            })
            .await
            .unwrap()
            .key
            .unwrap()
    }

    fn client(&self, key: &str, namespace: &str) -> Client {
        Client::remote(&self.url, Some(key))
            .unwrap()
            .with_namespace(namespace.parse().unwrap())
    }

    async fn stop(&mut self) {
        if let Some(running) = self.running.take() {
            running.stop().await;
        }
    }

    fn kill(&mut self) {
        if let Some(running) = self.running.take() {
            running.kill();
        }
    }
}

/// Waits until the deployment counts `nodes` nodes, as `node` sees it.
async fn members(node: &Node, nodes: u32) {
    let admin = node.admin();
    eventually(|| async {
        let info = admin.server_info().await.ok()?;
        (info.nodes == nodes && info.partitions == 4).then_some(())
    })
    .await;
}

fn start(script: &str, input: serde_json::Value) -> StartRun {
    let mut request = StartRun::new(script);
    request.input = input;
    request
}

/// The namespaces the tests spread their runs over: with 4 partitions, several owners.
fn namespaces() -> Vec<String> {
    (0..8).map(|n| format!("end-user-{n}")).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_tasks_and_changes_reach_their_owners_across_nodes() {
    let Some(deployment) = Deployment::start() else {
        return;
    };
    let mut a = deployment.node("node-a").await;
    let mut b = deployment.node("node-b").await;
    members(&a, 2).await;
    members(&b, 2).await;
    let key = a.key(BuiltInRole::Harness).await;

    // Started at a, in namespaces spread over the partitions; signalled and read at b.
    let mut runs = Vec::new();
    for (n, namespace) in namespaces().iter().enumerate() {
        let run = a
            .client(&key, namespace)
            .start_run(&start(SIGNAL, json!({ "n": n })), None)
            .await
            .unwrap();
        runs.push((namespace.clone(), run.id));
    }
    for (n, (namespace, id)) in runs.iter().enumerate() {
        let at_b = b.client(&key, namespace);
        at_b.signal_run(id, "go", &json!(n * 10), None)
            .await
            .unwrap();
        let ended = at_b.get_run(id, wait("30s")).await.unwrap();
        assert_eq!(ended.status, RunStatus::Completed, "{id}");
        assert_eq!(
            ended.output.unwrap().value,
            Some(json!({ "n": n, "go": n * 10 }))
        );
    }

    // A host task of a run started at a, handed on a stream held at b, answered at a.
    let worker = b.key(BuiltInRole::Worker).await;
    let toolset = a
        .client(&key, "end-user-0")
        .create_toolset(&CreateToolset {
            host: vec![HostTool {
                name: "ask_user".into(),
                description: "Ask the user".into(),
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
    let mut stream = TaskWorker::open(
        &Client::remote(&b.url, Some(&worker)).unwrap(),
        &TaskStreamQuery {
            toolset: Some(toolset.id.clone()),
            worker: Some("worker-at-b".into()),
            ..TaskStreamQuery::default()
        },
    )
    .await;
    let mut changes = Client::remote(&a.url, Some(&key))
        .unwrap()
        .changes()
        .await
        .unwrap();
    for namespace in ["end-user-3", "end-user-6"] {
        let mut request = start(ASK, json!({}));
        request.toolset = Some(toolset.id.clone());
        let run = a
            .client(&key, namespace)
            .start_run(&request, None)
            .await
            .unwrap();
        let task = stream.task().await;
        assert_eq!(task.run_id, run.id);
        a.client(&key, namespace)
            .complete_task(
                &task.token,
                &CompleteTask {
                    result: json!("yes"),
                },
            )
            .await
            .unwrap();
        let ended = b
            .client(&key, namespace)
            .get_run(&run.id, wait("30s"))
            .await
            .unwrap();
        assert_eq!(ended.status, RunStatus::Completed);
        // The change stream opened at a follows the runs of every owner.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let event = tokio::time::timeout(Duration::from_secs(30), changes.next())
                .await
                .expect("a change within 30 s")
                .expect("the stream stays open")
                .unwrap();
            if let ChangeEvent::Run(summary) = event
                && summary.id == run.id
                && summary.status == RunStatus::Completed
            {
                break;
            }
            assert!(Instant::now() < deadline, "no change of {} at a", run.id);
        }
    }
    a.stop().await;
    b.stop().await;
}

/// Nodes of one deployment that start at the same time register the operator's key once: the
/// node that writes it second finds the other's row and starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nodes_that_start_together_register_the_operator_key_once() {
    let Some(deployment) = Deployment::start() else {
        return;
    };
    for round in 0..3 {
        let admin_key = format!("rewind_sk_admin_of_round_{round}_0123456789abcdefghij");
        let (id_a, id_b) = (format!("together-a-{round}"), format!("together-b-{round}"));
        let (a, b) = tokio::join!(
            deployment.launch_with_key(&id_a, &admin_key, &[]),
            deployment.launch_with_key(&id_b, &admin_key, &[]),
        );
        for running in [&a, &b] {
            let admin = Client::remote(&running.url().unwrap(), Some(&admin_key)).unwrap();
            admin.whoami().await.unwrap();
        }
        a.stop().await;
        b.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_that_joins_and_one_that_leaves_hand_their_partitions_over() {
    let Some(deployment) = Deployment::start() else {
        return;
    };
    let mut a = deployment.node("node-a").await;
    members(&a, 1).await;
    let key = a.key(BuiltInRole::Harness).await;
    let mut runs = Vec::new();
    for (n, namespace) in namespaces().iter().enumerate() {
        let run = a
            .client(&key, namespace)
            .start_run(&start(SIGNAL, json!({ "n": n })), None)
            .await
            .unwrap();
        runs.push((namespace.clone(), run.id));
    }

    // b joins and takes some of a's partitions: their runs go on at b.
    let mut b = deployment.node("node-b").await;
    members(&a, 2).await;
    for (namespace, id) in &runs[..4] {
        let at_b = b.client(&key, namespace);
        eventually(|| async { at_b.signal_run(id, "go", &json!(1), None).await.ok() }).await;
        assert_eq!(
            at_b.get_run(id, wait("30s")).await.unwrap().status,
            RunStatus::Completed
        );
    }

    // a leaves: it hands every partition over to b first.
    a.stop().await;
    members(&b, 1).await;
    for (namespace, id) in &runs[4..] {
        let at_b = b.client(&key, namespace);
        eventually(|| async { at_b.signal_run(id, "go", &json!(2), None).await.ok() }).await;
        assert_eq!(
            at_b.get_run(id, wait("30s")).await.unwrap().status,
            RunStatus::Completed
        );
    }
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_with_a_pool_runs_on_the_nodes_of_its_pool_only() {
    let Some(deployment) = Deployment::start() else {
        return;
    };
    // a owns the partitions and runs the activations of every tenant but acme; p, a node of
    // executors only, runs acme's.
    let pools = ["--tenant-pool", "acme=acme-pool"];
    let mut a = deployment.node_with("node-a", &pools).await;
    let p = deployment
        .launch(
            "node-p",
            &[
                "--executors-only",
                "--pool",
                "acme-pool",
                "--tenant-pool",
                "acme=acme-pool",
            ],
        )
        .await;
    assert!(
        p.url().is_none(),
        "a node of executors only serves no HTTP route"
    );
    members(&a, 2).await;
    let acme = a.key(BuiltInRole::Harness).await;
    let beta = a.key_of("beta", BuiltInRole::Harness).await;
    for (n, (key, namespace)) in [(&acme, "end-user-1"), (&beta, "end-user-2")]
        .into_iter()
        .enumerate()
    {
        let client = a.client(key, namespace);
        let run = client
            .start_run(&start(SIGNAL, json!({ "n": n })), None)
            .await
            .unwrap();
        eventually(|| async { client.signal_run(&run.id, "go", &json!(n), None).await.ok() }).await;
        let ended = client.get_run(&run.id, wait("30s")).await.unwrap();
        assert_eq!(ended.status, RunStatus::Completed, "{}", run.id);
    }

    // Without a node of its pool, acme's runs wait for one; beta's run on at a.
    p.stop().await;
    members(&a, 1).await;
    let waiting = a
        .client(&acme, "end-user-3")
        .start_run(&start(SIGNAL, json!({ "n": 3 })), None)
        .await
        .unwrap();
    let client = a.client(&beta, "end-user-4");
    let run = client
        .start_run(&start(SIGNAL, json!({ "n": 4 })), None)
        .await
        .unwrap();
    eventually(|| async { client.signal_run(&run.id, "go", &json!(4), None).await.ok() }).await;
    let ended = client.get_run(&run.id, wait("30s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Completed);
    let at_a = a
        .client(&acme, "end-user-3")
        .get_run(&waiting.id, None)
        .await
        .unwrap();
    assert_eq!(
        at_a.status,
        RunStatus::Pending,
        "no node of acme-pool activated it"
    );
    a.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_killed_during_its_activations_loses_no_run() {
    let Some(deployment) = Deployment::start() else {
        return;
    };
    let mut a = deployment.node("node-a").await;
    let mut b = deployment.node("node-b").await;
    members(&a, 2).await;
    members(&b, 2).await;
    let key = a.key(BuiltInRole::Harness).await;
    let mut runs = Vec::new();
    for (n, namespace) in namespaces().iter().enumerate() {
        let run = a
            .client(&key, namespace)
            .start_run(&start(BUSY, json!({ "n": n })), None)
            .await
            .unwrap();
        runs.push((namespace.clone(), run.id));
    }
    // Killed while its executors run the activations of the runs it owns: nothing is handed
    // over; b takes its partitions once its lock comes free, and runs them again.
    a.kill();
    members(&b, 1).await;
    for (n, (namespace, id)) in runs.iter().enumerate() {
        let at_b = b.client(&key, namespace);
        // While b takes the partitions over, a request to one of them is 503: tried again.
        let ended = eventually(|| async { at_b.get_run(id, wait("60s")).await.ok() }).await;
        assert_eq!(ended.status, RunStatus::Completed, "{id}");
        assert_eq!(ended.output.unwrap().value.unwrap()["n"], json!(n));
    }
    b.stop().await;
}

/// A MinIO container with the bucket `rewind`, removed on drop.
struct Minio {
    name: String,
    endpoint: String,
}

impl Minio {
    fn start() -> Option<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let name = format!("rewind-server-minio-{}-{nanos}", std::process::id());
        docker(&[
            "run",
            "--detach",
            "--rm",
            "--name",
            &name,
            "--publish",
            "127.0.0.1::9000",
            "--tmpfs",
            "/data",
            "--env",
            &format!("MINIO_ROOT_USER={USER}"),
            "--env",
            &format!("MINIO_ROOT_PASSWORD={PASSWORD}"),
            MINIO_IMAGE,
            "server",
            "/data",
        ]);
        let mut minio = Minio {
            name,
            endpoint: String::new(),
        };
        let port = docker(&["port", &minio.name, "9000/tcp"]);
        let address = String::from_utf8_lossy(&port.stdout)
            .lines()
            .next()
            .expect("a published port")
            .trim()
            .to_owned();
        minio.endpoint = format!("http://{address}");
        let deadline = Instant::now() + Duration::from_secs(60);
        while !live(&address) {
            assert!(Instant::now() < deadline, "MinIO did not start in 60 s");
            std::thread::sleep(Duration::from_millis(200));
        }
        docker(&[
            "run",
            "--rm",
            "--network",
            &format!("container:{}", minio.name),
            "--env",
            &format!("MC_HOST_rewind=http://{USER}:{PASSWORD}@127.0.0.1:9000"),
            MC_IMAGE,
            "mb",
            "rewind/rewind",
        ]);
        Some(minio)
    }
}

impl Drop for Minio {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .output();
    }
}

fn docker(args: &[&str]) -> Output {
    let output = Command::new("docker").args(args).output().expect("docker");
    assert!(
        output.status.success(),
        "docker {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn live(address: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(address) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request =
        format!("GET /minio/health/live HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.starts_with("HTTP/1.1 200")
}
