//! The CLI end to end, through the `ptc` binary and the embedded server it starts
//! (`archi/09-roadmap.md`, "0.1 is done when"): V8, the engine, Turso and files, across
//! processes.
//!
//! - A run sleeps, its server is killed, and it resumes in another process: `ptc run`, `ptc
//!   signal`, `ptc status`, `ptc trace`, `ptc replay`.
//! - A signal sent from another process while `ptc run` waits.
//! - `kill -9` of the embedded server in the middle of an activation: the next command starts a
//!   new server, and the run finishes once its lease expires.
//! - The e2e corpus driven through the CLI: every committed journal replays with `ptc replay`,
//!   the red replays fail as specified, and the cases without tools run live on the embedded
//!   server, in virtual time.
//! - `ptc test` on a cassette recorded from a run of the embedded server.
//! - `ptc fork` of a failed run, which reuses its recorded calls.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// A project directory with its `.ptc/`, whose servers are killed at the end.
struct Project {
    _dir: tempfile::TempDir,
    /// The directory without symbolic links (macOS's `/var` is `/private/var`): as the CLI
    /// names the state directory to the server it starts.
    root: PathBuf,
}

impl Project {
    fn new() -> Self {
        // A short path: the socket's path is limited to 103 bytes.
        let dir = tempfile::Builder::new().prefix("ptc-").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        Self { _dir: dir, root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn state(&self) -> PathBuf {
        self.path(".ptc")
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ptc"));
        command
            .current_dir(&self.root)
            .env_remove("PTC_URL")
            .env_remove("PTC_DIR")
            .args(args);
        command
    }

    /// `ptc <args…>` in the project directory, to its end.
    fn ptc(&self, args: &[&str]) -> Ran {
        let output = self.command(args).output().unwrap();
        Ran::from(output)
    }

    /// `ptc --json <args…>`, its result parsed.
    fn json(&self, args: &[&str]) -> (i32, Value) {
        let mut all = vec!["--json"];
        all.extend(args);
        let ran = self.ptc(&all);
        let value = serde_json::from_str(&ran.stdout).unwrap_or_else(|error| {
            panic!(
                "ptc {args:?}: {error}\nstdout: {}\nstderr: {}",
                ran.stdout, ran.stderr
            )
        });
        (ran.code, value)
    }

    /// The pids of the embedded servers of this project.
    fn servers(&self) -> Vec<u32> {
        let output = Command::new("pgrep")
            .args([
                "-f",
                &format!("server --embedded --dir {}", self.state().display()),
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }

    fn kill_servers(&self) -> Vec<u32> {
        let pids = self.servers();
        for pid in &pids {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
        wait_until("the servers are gone", Duration::from_secs(20), || {
            pids.iter().all(|pid| !alive(*pid))
        });
        pids
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for pid in self.servers() {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    }
}

#[derive(Debug)]
struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
}

impl From<Output> for Ran {
    fn from(output: Output) -> Self {
        Self {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

fn alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .is_ok_and(|stat| !stat.is_empty() && !stat.starts_with('Z'))
}

fn wait_until(what: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../ptc-e2e/corpus/epoch-1")
}

/// `expected` is found in `actual`: objects key by key, recursively, as scenarios compare.
fn contains(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|found| contains(found, value))),
        _ => actual == expected,
    }
}

const SLEEPER: &str = r#"import { signal, sleep } from "ptc";

export default async function main(input: { n: number }) {
  await sleep("5s");
  console.log("slept");
  const go = await signal<{ n: number }>("go");
  return { total: input.n + go.n };
}
"#;

/// The local acceptance criterion of 0.1: a run that sleeps, is interrupted, and resumes in
/// another process.
#[test]
fn a_run_sleeps_is_interrupted_and_resumes_in_another_process() {
    let project = Project::new();
    project.write("sleeper.ts", SLEEPER);
    let started = Instant::now();
    let (code, run) = project.json(&["run", "sleeper.ts", "--input", r#"{"n": 1}"#, "--wait", "0"]);
    assert_eq!(code, 4, "still waiting: {run}");
    let id = run["id"].as_str().unwrap().to_owned();
    // Its first activation starts the sleep.
    let mut run = run;
    wait_until("the run sleeps", Duration::from_secs(30), || {
        run = project.json(&["status", &id]).1;
        run["status"] == "active"
    });
    assert_eq!(run["waitingFor"][0]["kind"], "timer", "{run}");

    // The server is killed while the run sleeps.
    let killed = project.kill_servers();
    assert_eq!(killed.len(), 1, "one embedded server ran");
    std::thread::sleep(Duration::from_millis(5500).saturating_sub(started.elapsed()));

    // Another process: a new server starts, fires the timer, and the signal resumes the run.
    let ran = project.ptc(&["signal", &id, "go", r#"{"n": 41}"#]);
    assert_eq!(ran.code, 0, "{ran:?}");
    let servers = project.servers();
    assert_eq!(servers.len(), 1, "a new server: {servers:?}");
    assert_ne!(servers, killed);

    let (code, run) = project.json(&["status", &id, "--wait", "60s"]);
    assert_eq!(code, 0, "completed: {run}");
    assert_eq!(run["output"]["value"], json!({"total": 42}));

    let ran = project.ptc(&["trace", &id]);
    assert_eq!(ran.code, 0, "{ran:?}");
    assert!(ran.stdout.contains("\"go\""), "{}", ran.stdout);
    assert!(ran.stdout.contains("slept"), "{}", ran.stdout);

    let ran = project.ptc(&["replay", &id]);
    assert_eq!(ran.code, 0, "{ran:?}");
    assert!(ran.stdout.contains("the script replays"), "{}", ran.stdout);

    let ran = project.ptc(&["export", &id, "--output", "run.json"]);
    assert_eq!(ran.code, 0, "{ran:?}");
    let ran = project.ptc(&["replay", "run.json"]);
    assert_eq!(ran.code, 0, "{ran:?}");

    // The run has ended: a signal is refused.
    let ran = project.ptc(&["signal", &id, "go", "{}"]);
    assert_eq!(ran.code, 6, "{ran:?}");
    let ran = project.ptc(&["status", "01890000-0000-7000-8000-000000000000"]);
    assert_eq!(ran.code, 6, "an unknown run: {ran:?}");
}

#[test]
fn a_signal_from_another_process_ends_a_waiting_run() {
    let project = Project::new();
    project.write(
        "approval.ts",
        "import { signal } from \"ptc\";\nexport default async function main() {\n  const decision = await signal<{ ok: boolean }>(\"approval\");\n  return decision;\n}\n",
    );
    let mut waiting: Child = project
        .command(&["run", "approval.ts", "--wait", "60s"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // `ptc run` says which run it started on stderr.
    let stderr = waiting.stderr.take().unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stderr));
    let id = loop {
        let line = lines.next().expect("ptc run printed its run").unwrap();
        if let Some(id) = line.strip_prefix("started run ") {
            break id.to_owned();
        }
    };
    let ran = project.ptc(&["signal", &id, "approval", r#"{"ok": true}"#, "--key", "d-1"]);
    assert_eq!(ran.code, 0, "{ran:?}");
    let output = waiting.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&format!("run {id} completed")), "{stdout}");
    assert!(stdout.contains(r#"output: {"ok":true}"#), "{stdout}");

    // The same key again: not delivered twice, and the run has ended anyway.
    let ran = project.ptc(&["signal", &id, "approval", "{}", "--key", "d-1"]);
    assert_eq!(ran.code, 6, "{ran:?}");
}

/// A first Ctrl-C abandons the wait of `ptc run`: the run it started goes on, and the command
/// reports it as still waiting (07, "Embedded server and CLI").
#[test]
fn a_first_ctrl_c_stops_waiting_and_reports_the_run() {
    let project = Project::new();
    project.write(
        "approval.ts",
        "import { signal } from \"ptc\";\nexport default async function main() {\n  return await signal(\"approval\");\n}\n",
    );
    let mut waiting: Child = project
        .command(&["run", "approval.ts", "--wait", "60s"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = waiting.stderr.take().unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stderr));
    let id = loop {
        let line = lines.next().expect("ptc run printed its run").unwrap();
        if let Some(id) = line.strip_prefix("started run ") {
            break id.to_owned();
        }
    };
    let interrupted = Instant::now();
    let status = Command::new("kill")
        .args(["-INT", &waiting.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let output = waiting.wait_with_output().unwrap();
    assert!(
        interrupted.elapsed() < Duration::from_secs(10),
        "the wait is abandoned at once"
    );
    assert_eq!(output.status.code(), Some(4), "still waiting");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&format!("run {id}")), "{stdout}");
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    assert!(
        rest.iter().any(|line| line.starts_with("interrupted")),
        "{rest:?}"
    );

    // The run goes on: a signal from another process ends it.
    let ran = project.ptc(&["signal", &id, "approval", "1"]);
    assert_eq!(ran.code, 0, "{ran:?}");
    let (code, run) = project.json(&["status", &id, "--wait", "30s"]);
    assert_eq!(code, 0, "{run}");
}

/// A server for `--url` that answers the registration of a toolset only once released, and
/// records the requests it gets: `(method and path, …)`.
fn slow_registration_server() -> (
    String,
    std::sync::mpsc::Receiver<String>,
    std::sync::mpsc::Sender<()>,
) {
    use std::io::{BufRead, BufReader, Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (seen, requests) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            if reader.read_line(&mut request).is_err() {
                continue;
            }
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let line: Vec<&str> = request.split_whitespace().take(2).collect();
            let what = line.join(" ");
            let _ = seen.send(what.clone());
            let (status, body) = if what == "POST /v1/toolsets" {
                let _ = released.recv();
                let id = "a".repeat(64);
                (
                    "201 Created",
                    format!(
                        r#"{{"id":"{id}","servers":[],"tools":[],"declarations":{{"dts":"","prompt":""}}}}"#
                    ),
                )
            } else {
                (
                    "500 Internal Server Error",
                    r#"{"type":"urn:ptc:problem:internal","title":"Internal error","status":500,"detail":"stub"}"#.to_owned(),
                )
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, requests, release)
}

/// A first Ctrl-C while `ptc run` registers its toolset: the registration finishes, and no run
/// is started.
#[test]
fn a_first_ctrl_c_before_the_start_starts_no_run() {
    let project = Project::new();
    project.write("script.ts", "export default async () => 1;\n");
    project.write("toolset.json", "{}");
    let (url, requests, release) = slow_registration_server();
    let mut running: Child = project
        .command(&["--url", &url, "run", "script.ts", "--toolset", "toolset.json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(30)).unwrap(),
        "POST /v1/toolsets"
    );
    let status = Command::new("kill")
        .args(["-INT", &running.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    // The CLI has seen the Ctrl-C before the registration is answered.
    std::thread::sleep(Duration::from_millis(500));
    release.send(()).unwrap();
    let output = running.wait_with_output().unwrap();
    let after: Vec<String> = requests.try_iter().collect();
    assert!(
        after.is_empty(),
        "nothing is sent after the call in flight: {after:?}, {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(130), "interrupted");
}

/// `kill -9` of the embedded server while an activation runs: nothing of it was committed,
/// and the run is taken again once its lease expires.
#[test]
fn a_server_killed_mid_activation_is_replaced() {
    let project = Project::new();
    project.write(
        "busy.ts",
        "export default async function main() {\n  let total = 0;\n  for (let i = 0; i < 150_000_000; i++) total += i % 7;\n  return total;\n}\n",
    );
    let (code, run) = project.json(&["run", "busy.ts", "--wait", "0"]);
    assert_eq!(code, 4, "{run}");
    let id = run["id"].as_str().unwrap().to_owned();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(project.kill_servers().len(), 1, "the server was killed");

    let (code, run) = project.json(&["status", &id, "--wait", "120s"]);
    assert_eq!(code, 0, "the run finished on a new server: {run}");
    assert!(run["output"]["value"].is_number(), "{run}");
    let ran = project.ptc(&["replay", &id]);
    assert_eq!(ran.code, 0, "{ran:?}");
}

/// Every committed journal of the corpus replays through `ptc replay <file>`, and the red
/// cases that alter their replay with another script fail with the divergence they specify.
#[test]
fn the_corpus_journals_replay_through_the_binary() {
    let project = Project::new();
    let mut replayed = 0;
    for (name, case) in cases() {
        let journal = case.join("journal.v1.json");
        if !journal.exists() {
            continue;
        }
        let ran = project.ptc(&["replay", journal.to_str().unwrap()]);
        assert_eq!(ran.code, 0, "{name}: {ran:?}");
        replayed += 1;
        let scenario = scenario(&case);
        if let Some(script) = scenario["replay"]["script"].as_str() {
            let expected = scenario["replay"]["diverges"].as_str().unwrap();
            let script = case.join(script);
            let ran = project.ptc(&[
                "replay",
                journal.to_str().unwrap(),
                "--script",
                script.to_str().unwrap(),
            ]);
            assert_eq!(ran.code, 1, "{name}: {ran:?}");
            assert!(ran.stdout.contains(expected), "{name}: {ran:?}");
        }
    }
    assert!(replayed >= 54, "{replayed} journals");
    assert!(
        project.servers().is_empty(),
        "replaying a file needs no server"
    );
}

/// The corpus cases run live on the embedded server, in virtual time, through `ptc run`, when
/// their tools are host tools this test can answer as the harness's worker (values, echoed
/// arguments, failures, sequences of them) and they take no steps: their status, output,
/// error, abort or refused start are the corpus's.
#[test]
fn the_corpus_runs_live_on_the_embedded_server() {
    let project = Project::new();
    // A server run in the foreground switches the directory to virtual time; the servers the
    // CLI starts keep it.
    let state = project.state();
    let mut foreground = project
        .command(&[
            "server",
            "--embedded",
            "--dir",
            state.to_str().unwrap(),
            "--virtual-time",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_until(
        "the foreground server listens",
        Duration::from_secs(60),
        || project.state().join("server.sock").exists(),
    );

    let mut ran_cases = Vec::new();
    let mut other_outputs = Vec::new();
    for (name, case) in cases() {
        let scenario = scenario(&case);
        let tools = scenario["tools"].as_object().cloned().unwrap_or_default();
        let simple = scenario
            .as_object()
            .unwrap()
            .keys()
            .all(|key| ["description", "input", "expect", "tools"].contains(&key.as_str()))
            && tools.iter().all(|(tool, response)| {
                !matches!(tool.as_str(), "http" | "webhook") && answerable(response)
            });
        // Its toolset lacks the `webhook` built-in, which every toolset of a server pins.
        if !simple || name == "red/webhook_without_the_built_in" {
            continue;
        }
        let expect = &scenario["expect"];
        let input = scenario
            .get("input")
            .cloned()
            .unwrap_or(json!({}))
            .to_string();
        let script = case.join("script.ts");
        let status = expect["status"].as_str().unwrap_or("completed");
        // A run that waits for the outside world is seen waiting.
        let wait = if status == "active" { "3s" } else { "60s" };
        // The corpus checks scripts at start, not their types.
        let mut args = vec![
            "--json".to_owned(),
            "run".to_owned(),
            script.to_str().unwrap().to_owned(),
            "--input".to_owned(),
            input,
            "--wait".to_owned(),
            wait.to_owned(),
            "--no-check".to_owned(),
        ];
        let toolset = (!tools.is_empty()).then(|| {
            let host: Vec<Value> = tools
                .keys()
                .map(|tool| json!({ "name": tool, "description": tool, "inputSchema": {} }))
                .collect();
            let file = project.write(
                &format!("{}.toolset.json", name.replace('/', "_")),
                &json!({ "host": host }).to_string(),
            );
            let (code, registered) = project.json(&["toolsets", "add", file.to_str().unwrap()]);
            assert_eq!(code, 0, "{name}: {registered}");
            let id = registered["id"].as_str().unwrap().to_owned();
            args.push("--toolset".to_owned());
            args.push(id.clone());
            id
        });
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let child = project
            .command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let ran = match &toolset {
            Some(toolset) => work(&project, toolset, &tools, child),
            None => Ran::from(child.wait_with_output().unwrap()),
        };

        if let Some(message) = expect["start_error"].as_str() {
            assert_eq!(ran.code, 65, "{name}: {ran:?}");
            let problem: Value = serde_json::from_str(ran.stderr.trim()).unwrap();
            // The server words the refusal its own way; the reason is the engine's.
            let reason = message
                .strip_prefix("the script is invalid: ")
                .unwrap_or(message);
            let detail = problem["detail"].as_str().unwrap_or_default();
            assert!(detail.contains(reason), "{name}: {problem}");
            ran_cases.push(name);
            continue;
        }
        let run: Value = serde_json::from_str(&ran.stdout)
            .unwrap_or_else(|error| panic!("{name}: {error}: {ran:?}"));
        assert_eq!(run["status"], status, "{name}: {run}");
        let wanted = match status {
            "completed" => 0,
            "failed" => 1,
            "cancelled" => 2,
            "aborted" => 3,
            _ => 4,
        };
        assert_eq!(ran.code, wanted, "{name}");
        if let Some(output) = expect.get("output")
            && !contains(&run["output"]["value"], output)
        {
            other_outputs.push(name.clone());
        }
        if let Some(error) = expect.get("error") {
            assert_eq!(run["error"]["name"], error["name"], "{name}: {run}");
            if let Some(message) = error["message"].as_str() {
                let found = run["error"]["message"].as_str().unwrap_or_default();
                assert!(found.contains(message), "{name}: {run}");
            }
        }
        if let Some(abort) = expect.get("abort") {
            assert_eq!(run["abort"]["kind"], abort["kind"], "{name}: {run}");
        }
        if let Some(awaiting) = expect.get("awaiting").and_then(Value::as_array) {
            let waiting: Vec<&Value> = run["waitingFor"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|waiting| waiting["kind"] == "signal")
                .map(|waiting| &waiting["name"])
                .collect();
            assert_eq!(
                waiting,
                awaiting.iter().collect::<Vec<_>>(),
                "{name}: {run}"
            );
        }
        ran_cases.push(name);
    }
    let _ = foreground.kill();
    let _ = foreground.wait();
    eprintln!("ran live: {ran_cases:?}");
    assert!(ran_cases.len() >= 20, "{ran_cases:?}");
    // Recorded at 2026-01-01 with the corpus's seed: a live run has its own time and seed.
    assert_eq!(
        other_outputs,
        ["blue/deterministic_globals", "blue/durable_sleep"],
        "the outputs that depend on the time or the seed"
    );
}

/// A response the harness's worker of this test gives: a value, the arguments echoed, a
/// failure, or a sequence of them.
fn answerable(response: &Value) -> bool {
    let Some(fields) = response.as_object() else {
        return true;
    };
    if !fields.keys().any(|key| key.starts_with('$')) {
        return true;
    }
    match fields.get("$sequence") {
        Some(Value::Array(sequence)) => fields.len() == 1 && sequence.iter().all(answerable),
        _ => {
            fields.get("$echo") == Some(&Value::Bool(true)) && fields.len() == 1
                || fields.get("$error").is_some_and(Value::is_string)
                    && fields
                        .keys()
                        .all(|key| ["$error", "$data"].contains(&key.as_str()))
        }
    }
}

/// The harness's worker: claims the host-tool tasks of `toolset` and answers them from the
/// case's responses, until the `ptc run` in `child` ends.
fn work(
    project: &Project,
    toolset: &str,
    tools: &serde_json::Map<String, Value>,
    mut child: Child,
) -> Ran {
    let socket = project.state().join("server.sock");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut answered: std::collections::BTreeMap<String, usize> = Default::default();
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .unix_socket(socket)
            .build()
            .unwrap();
        while child.try_wait().unwrap().is_none() {
            let Ok(response) = client
                .get(format!(
                    "http://localhost/v1/tasks?toolset={toolset}&wait=500ms"
                ))
                .send()
                .await
            else {
                continue;
            };
            let batch: Value = response.json().await.unwrap_or(Value::Null);
            for task in batch["tasks"].as_array().cloned().unwrap_or_default() {
                let tool = task["tool"].as_str().unwrap().to_owned();
                let token = task["token"].as_str().unwrap();
                let count = answered.entry(tool.clone()).or_default();
                let mut response = tools[&tool].clone();
                if let Some(Value::Array(sequence)) = response.get("$sequence") {
                    response = sequence
                        .get(*count)
                        .cloned()
                        .unwrap_or(json!({ "$error": format!("{tool} has no response left") }));
                }
                *count += 1;
                let (path, body) = if response.get("$echo").is_some() {
                    ("complete", json!({ "result": task["args"] }))
                } else if let Some(message) = response.get("$error") {
                    let mut failure = json!({ "message": message });
                    if let Some(data) = response.get("$data") {
                        failure["data"] = data.clone();
                    }
                    ("fail", failure)
                } else {
                    ("complete", json!({ "result": response }))
                };
                let answer = client
                    .post(format!("http://localhost/v1/tasks/{token}/{path}"))
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert!(answer.status().is_success(), "{answer:?}");
            }
        }
    });
    Ran::from(child.wait_with_output().unwrap())
}

/// `ptc test` replays a run of the embedded server as a scenario, from its cassette; `ptc fork`
/// continues a failed run with a corrected script without calling again what it recorded.
#[test]
fn a_recorded_run_is_tested_and_its_failure_forked() {
    let project = Project::new();
    // A host tool: the harness's worker answers it. This test is the worker.
    project.write(
        "toolset.json",
        &json!({
            "host": [{
                "name": "github.list_issues",
                "description": "List the issues of a repository.",
                "inputSchema": { "type": "object" }
            }]
        })
        .to_string(),
    );
    project.write(
        "failing.ts",
        "import { github } from \"ptc:tools\";\nexport default async function main() {\n  const issues = await github.list_issues({ repo: \"acme/api\" });\n  return issues[0].title.toUpperCase();\n}\n",
    );
    project.write(
        "fixed.ts",
        "import { github } from \"ptc:tools\";\nexport default async function main() {\n  const issues = await github.list_issues({ repo: \"acme/api\" });\n  return issues.length;\n}\n",
    );
    let (code, run) = project.json(&[
        "run",
        "failing.ts",
        "--toolset",
        "toolset.json",
        "--wait",
        "1s",
        "--no-check",
    ]);
    assert_eq!(code, 4, "the call waits for the harness's worker: {run}");
    let id = run["id"].as_str().unwrap().to_owned();
    assert_eq!(run["waitingFor"][0]["kind"], "tool_call", "{run}");

    // The harness's worker answers through the API: claim the task and complete it.
    complete_host_task(
        &project,
        run["toolset"].as_str().unwrap(),
        json!([{ "number": 4 }]),
    );
    let (code, run) = project.json(&["status", &id, "--wait", "60s"]);
    assert_eq!(code, 1, "the run failed: {run}");
    assert_eq!(run["error"]["name"], "TypeError", "{run}");

    // The recorded run as a cassette, for `ptc test` with the corrected script.
    let ran = project.ptc(&[
        "export",
        &id,
        "--as",
        "cassette",
        "--output",
        "recorded.json",
    ]);
    assert_eq!(ran.code, 0, "{ran:?}");
    project.write(
        "scenario.json",
        &json!({
            "script": "fixed.ts",
            "toolset": "toolset.json",
            "cassette": "recorded.json",
            "expect": { "status": "completed", "output": 1, "calls": { "github.list_issues": 1 } }
        })
        .to_string(),
    );
    let ran = project.ptc(&["test", "scenario.json"]);
    assert_eq!(ran.code, 0, "{ran:?}");
    assert!(ran.stdout.starts_with("PASS"), "{}", ran.stdout);

    // What a fork would reuse: the corrected script diverges where the run failed.
    let ran = project.ptc(&["replay", &id, "--script", "fixed.ts"]);
    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.stdout.contains("diverges"), "{}", ran.stdout);
    assert!(
        ran.stdout.contains("a fork reuses: #1 github.list_issues"),
        "{}",
        ran.stdout
    );
    // The fork completes without a worker: the recorded call is not made again.
    let (code, fork) = project.json(&[
        "fork",
        &id,
        "--script",
        "fixed.ts",
        "--wait",
        "60s",
        "--no-check",
    ]);
    assert_eq!(code, 0, "the fork completed: {fork}");
    assert_eq!(fork["output"]["value"], 1);
    assert_eq!(fork["parent"]["run"], id.as_str());
}

/// Claims the one host-tool task of the embedded server and completes it, as an SDK worker
/// would, over the server's socket.
fn complete_host_task(project: &Project, toolset: &str, result: Value) {
    let socket = project.state().join("server.sock");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .unix_socket(socket)
            .build()
            .unwrap();
        let batch: Value = client
            .get(format!(
                "http://localhost/v1/tasks?toolset={toolset}&wait=10s"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = batch["tasks"][0]["token"]
            .as_str()
            .expect("a task")
            .to_owned();
        let response = client
            .post(format!("http://localhost/v1/tasks/{token}/complete"))
            .json(&json!({ "result": result }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{response:?}");
    });
}

fn cases() -> Vec<(String, PathBuf)> {
    let mut cases = Vec::new();
    for color in ["blue", "red"] {
        for entry in std::fs::read_dir(corpus().join(color)).unwrap() {
            let path = entry.unwrap().path();
            if path.join("scenario.json").exists() {
                let name = format!("{color}/{}", path.file_name().unwrap().to_str().unwrap());
                cases.push((name, path));
            }
        }
    }
    cases.sort();
    cases
}

fn scenario(case: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(case.join("scenario.json")).unwrap()).unwrap()
}
