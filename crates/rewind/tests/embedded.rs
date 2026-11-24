//! The embedded server's lifecycle, through the `rewind` binary (`archi/07-code-architecture.md`,
//! "Embedded server and CLI"): one server per directory (its lock), the socket (mode 0600, no
//! key, tenant `local`), the stop asked through the API, the idle stop, `kill -9` and a restart
//! that finishes the runs, and virtual time kept across restarts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Exit code of a server that finds the directory locked.
const LOCKED: i32 = 75;

struct Server {
    child: Child,
    socket: PathBuf,
}

fn start(dir: &Path, extra: &[&str]) -> Server {
    let child = Command::new(env!("CARGO_BIN_EXE_rewind"))
        .args(["server", "--embedded", "--dir"])
        .arg(dir)
        .args(["--log", "warn"])
        .args(extra)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    Server {
        child,
        socket: dir.join("server.sock"),
    }
}

fn client(socket: &Path) -> reqwest::Client {
    reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .unwrap()
}

/// Waits until the server answers `GET /v1/server`.
async fn ready(server: &Server) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if server.socket.exists()
            && let Ok(response) = client(&server.socket)
                .get("http://localhost/v1/server")
                .send()
                .await
            && response.status().is_success()
        {
            return response.json().await.unwrap();
        }
        assert!(Instant::now() < deadline, "the server did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits for the process to exit by itself.
async fn exited(child: &mut Child, within: Duration) -> ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "the server did not exit");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn post(socket: &Path, path: &str, body: Value) -> (u16, Value) {
    let response = client(socket)
        .post(format!("http://localhost{path}"))
        // The runs act for one end user: a request over the socket names its namespace too.
        .header("Rewind-Namespace", "local")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

async fn get(socket: &Path, path: &str) -> Value {
    client(socket)
        .get(format!("http://localhost{path}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn sleeping(duration: &str) -> Value {
    json!({
        "code": format!(
            "import {{ sleep }} from \"rewind\";\nexport default async function main() {{\n  await sleep(\"{duration}\");\n  return \"woke\";\n}}\n"
        )
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_server_holds_the_directory_until_it_is_asked_to_stop() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".rewind");
    let mut server = start(&dir, &[]);
    let info = ready(&server).await;
    assert_eq!(info["embedded"], true);
    assert_eq!(
        (info["partitions"].clone(), info["nodes"].clone()),
        (json!(1), json!(1))
    );
    let mode = std::fs::metadata(&server.socket)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let me = get(&server.socket, "/v1/whoami").await;
    assert_eq!(
        (me["tenant"].as_str(), me["grant"]["role"].as_str()),
        (Some("local"), Some("operator"))
    );
    assert_eq!(me["operator"], true);

    let mut second = start(&dir, &[]);
    let status = exited(&mut second.child, Duration::from_secs(60)).await;
    assert_eq!(
        status.code(),
        Some(LOCKED),
        "a second server fails to take the lock"
    );

    let (status, _) = post(&server.socket, "/v1/server/stop", json!(null)).await;
    assert_eq!(status, 202);
    let status = exited(&mut server.child, Duration::from_secs(60)).await;
    assert!(status.success());
    assert!(!server.socket.exists(), "the socket is removed");
}

/// What the CLI waits for: `ready` on the pipe given as standard output, once the socket answers;
/// then nothing more reaches the pipe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_told_to_notify_writes_ready_once_it_listens() {
    use std::io::{BufRead, BufReader, Read};

    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".rewind");
    let mut child = Command::new(env!("CARGO_BIN_EXE_rewind"))
        .args(["server", "--embedded", "--dir"])
        .arg(&dir)
        .args(["--log", "warn", "--notify-ready"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let line = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        (line, reader)
    });
    let (line, mut reader) = tokio::time::timeout(Duration::from_secs(60), line)
        .await
        .expect("ready within a minute")
        .unwrap();
    assert_eq!(line, "ready\n");
    // It listens: the first request answers.
    let socket = dir.join("server.sock");
    let info = get(&socket, "/v1/server").await;
    assert_eq!(info["embedded"], true);
    // Standard output now goes where standard error goes: the pipe is closed.
    let rest = tokio::task::spawn_blocking(move || {
        let mut rest = String::new();
        reader.read_to_string(&mut rest).unwrap();
        rest
    });
    let rest = tokio::time::timeout(Duration::from_secs(10), rest)
        .await
        .expect("the pipe closes")
        .unwrap();
    assert_eq!(rest, "");
    let (status, _) = post(&socket, "/v1/server/stop", json!(null)).await;
    assert_eq!(status, 202);
    assert!(exited(&mut child, Duration::from_secs(60)).await.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_with_an_idle_timeout_stops_by_itself() {
    let temp = tempfile::tempdir().unwrap();
    let mut server = start(&temp.path().join(".rewind"), &["--idle-timeout", "1s"]);
    ready(&server).await;
    let status = exited(&mut server.child, Duration::from_secs(60)).await;
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_server_frees_the_directory_and_the_next_one_finishes_the_run() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".rewind");
    let mut first = start(&dir, &[]);
    ready(&first).await;
    let (status, run) = post(&first.socket, "/v1/runs", sleeping("2s")).await;
    assert_eq!(status, 201, "{run}");
    let id = run["id"].as_str().unwrap().to_owned();
    // Let it commit its timer, then kill -9.
    tokio::time::sleep(Duration::from_millis(500)).await;
    first.child.kill().unwrap();
    first.child.wait().unwrap();

    let mut second = start(&dir, &[]);
    ready(&second).await;
    let run = get(&second.socket, &format!("/v1/runs/{id}?wait=60s")).await;
    assert_eq!(run["status"], "completed", "{run}");
    assert_eq!(run["output"]["value"], "woke");
    second.child.kill().unwrap();
    second.child.wait().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn virtual_time_jumps_over_sleeps_and_is_kept_across_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".rewind");
    let mut server = start(&dir, &["--virtual-time"]);
    assert_eq!(ready(&server).await["virtualTime"], true);
    let mut ten_days = sleeping("10d");
    ten_days["wait"] = json!("30s");
    let started = Instant::now();
    let (status, run) = post(&server.socket, "/v1/runs", ten_days).await;
    assert_eq!(status, 201, "{run}");
    assert_eq!(run["status"], "completed", "{run}");
    assert!(started.elapsed() < Duration::from_secs(30));
    post(&server.socket, "/v1/server/stop", json!(null)).await;
    exited(&mut server.child, Duration::from_secs(60)).await;

    let mut again = start(&dir, &[]);
    assert_eq!(
        ready(&again).await["virtualTime"],
        true,
        "kept in the store"
    );
    post(&again.socket, "/v1/server/stop", json!(null)).await;
    exited(&mut again.child, Duration::from_secs(60)).await;

    let mut real = start(&dir, &["--real-time"]);
    assert_eq!(ready(&real).await["virtualTime"], false);
    post(&real.socket, "/v1/server/stop", json!(null)).await;
    exited(&mut real.child, Duration::from_secs(60)).await;
}
