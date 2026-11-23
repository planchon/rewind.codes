//! One load: the toolset, the script, the runs driven through the API, the host-tool worker,
//! and the checks once every run has ended.
//!
//! Each run makes an MCP call (`fake.echo`), a host-tool call (`host.square`, answered by this
//! process through `GET /v1/tasks`), an MCP call that fails transiently once and is retried
//! (`fake.flaky`), a durable sleep, then reads a signal sent right after its start (sent twice
//! with one idempotency key) and waits for a second one that never comes (a timer).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt as _;
use rewind_api::Client;
use rewind_api::access::{BuiltInRole, CreateApiKey};
use rewind_api::common::{Labels, RunStatus};
use rewind_api::headers::IdempotencyKey;
use rewind_api::namespaces::NamespaceName;
use rewind_api::reading::ToolStatsFilter;
use rewind_api::runs::{CallAttempts, Run, StartRun};
use rewind_api::tasks::{CompleteTask, TaskStreamEvent, TaskStreamQuery};
use rewind_api::toolsets::{CreateToolset, HostTool, McpServer};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::mcp::FakeServer;
use crate::report::{Latencies, Percentiles, Report, ToolStatsLine};

/// The script of every run; `SLEEP` is replaced by the load's sleep.
const SCRIPT: &str = r#"import { signal, sleep } from "rewind";
import { fake, host } from "rewind:tools";

export default async function main(input: { n: number; pad: string }) {
  const echoed = await fake.echo({ n: input.n, pad: input.pad });
  const squared = await host.square({ n: input.n });
  const flaky = await fake.flaky({ n: input.n });
  await sleep("SLEEP");
  const go = await signal<{ n: number }>("go");
  const again = await signal("go", { timeout: "100ms" });
  return {
    n: input.n,
    echoed: echoed.n,
    padded: echoed.pad.length,
    squared: squared.square,
    flaky: flaky.n,
    go,
    again: again ?? null,
  };
}
"#;

/// What run `n` must return, with a padding of `pad` bytes.
fn expected_output(n: usize, pad: usize) -> Value {
    json!({
        "n": n,
        "echoed": n,
        "padded": pad,
        "squared": n * n,
        "flaky": n,
        "go": { "n": n },
        "again": null,
    })
}

/// What a load is given.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) runs: usize,
    pub(crate) concurrency: usize,
    pub(crate) host_workers: usize,
    /// The durable sleep of each run, such as `1s`.
    pub(crate) sleep: String,
    /// The size of the padding each run's input carries, echoed by `fake.echo`: above 1 KiB
    /// the payload leaves the event, above 64 KiB it goes to the blob store.
    pub(crate) payload_bytes: usize,
    /// The URL the deployment reaches the fake MCP server at.
    pub(crate) mcp_url: String,
    pub(crate) tenant: String,
    /// The whole load gives up after this long.
    pub(crate) timeout: Duration,
    pub(crate) thresholds: Thresholds,
}

/// What the load must achieve to pass, besides its invariants.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Thresholds {
    pub(crate) min_runs_per_s: Option<f64>,
    pub(crate) max_p99_end_to_end: Option<Duration>,
    pub(crate) max_p99_activation: Option<Duration>,
    /// The longest the read model may take, after the last run ended, to hold every run.
    pub(crate) max_read_model_lag: Duration,
}

/// What the host-tool worker saw.
#[derive(Debug, Default)]
struct HostRecord {
    /// Hand-outs by run and seq.
    handed: HashMap<(String, u32), u32>,
    /// When the result of each run's host call was accepted.
    answered: HashMap<String, Instant>,
}

/// How one run went, as its driver saw it.
#[derive(Debug)]
struct Outcome {
    n: usize,
    sent: Instant,
    run: Option<String>,
    /// When the start was answered.
    started: Option<Instant>,
    ended: Option<Instant>,
    /// The final status, or `refused`, `unfinished`.
    status: String,
    attempts: Vec<CallAttempts>,
    violations: Vec<String>,
    errors: Vec<String>,
}

struct Shared {
    harness: Client,
    toolset: String,
    labels: Labels,
    id: String,
    script: String,
    pad: String,
    stop: CancellationToken,
    deadline: Instant,
}

/// Prepares the tenant's keys and the toolset, drives the runs, and checks them.
pub(crate) async fn run(
    admin: &Client,
    base_url: &str,
    mcp: &FakeServer,
    settings: &Settings,
    interrupt: CancellationToken,
) -> Result<Report, Error> {
    wait_until_ready(admin).await?;
    // The runs act for one end user, the namespace `load`: a key must name one to start a run.
    let namespace =
        NamespaceName::new("load").map_err(|error| Error::new(format!("namespace: {error}")))?;
    let harness = key(admin, base_url, &settings.tenant, BuiltInRole::Harness)
        .await?
        .with_namespace(namespace);
    let worker = key(admin, base_url, &settings.tenant, BuiltInRole::Worker).await?;
    let toolset = harness
        .create_toolset(&toolset(&settings.mcp_url))
        .await
        .map_err(|error| {
            Error::new(format!(
                "cannot register the toolset (the server must reach the fake MCP server at {}): \
                 {error}",
                settings.mcp_url
            ))
        })?;
    let began = rewind_api::common::Timestamp::now();
    let id = format!(
        "load-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis())
    );
    let shared = Arc::new(Shared {
        harness: harness.clone(),
        toolset: toolset.id.clone(),
        labels: [("load", id.as_str())].into_iter().collect(),
        id: id.clone(),
        script: SCRIPT.replace("SLEEP", &settings.sleep),
        pad: "x".repeat(settings.payload_bytes),
        stop: interrupt.clone(),
        deadline: Instant::now() + settings.timeout,
    });

    let host = Arc::new(Mutex::new(HostRecord::default()));
    let errors = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
    let workers_stop = CancellationToken::new();
    let workers: Vec<_> = (0..settings.host_workers.max(1))
        .map(|index| {
            tokio::spawn(host_worker(
                worker.clone(),
                TaskStreamQuery {
                    toolset: Some(toolset.id.clone()),
                    slots: Some(HOST_SLOTS),
                    worker: Some(format!("{id}-host-{index}")),
                    ..TaskStreamQuery::default()
                },
                host.clone(),
                errors.clone(),
                workers_stop.clone(),
            ))
        })
        .collect();

    let permits = Arc::new(Semaphore::new(settings.concurrency.max(1)));
    let drivers: Vec<_> = (0..settings.runs)
        .map(|n| {
            let shared = shared.clone();
            let permits = permits.clone();
            tokio::spawn(async move {
                let _permit = permits.acquire_owned().await;
                drive(&shared, n).await
            })
        })
        .collect();
    let mut outcomes = Vec::with_capacity(settings.runs);
    for driver in drivers {
        match driver.await {
            Ok(outcome) => outcomes.push(outcome),
            Err(error) => {
                *errors
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .entry(format!("driver panicked: {error}"))
                    .or_default() += 1
            }
        }
    }
    workers_stop.cancel();
    for worker in workers {
        let _ = worker.await;
    }

    let mut report = Report {
        runs: settings.runs,
        concurrency: settings.concurrency,
        payload_bytes: settings.payload_bytes,
        ..Report::default()
    };
    // A run's attempts are read from the read model: wait until it holds every completed run
    // before checking them.
    let last_end = outcomes.iter().filter_map(|outcome| outcome.ended).max();
    let unprojected = await_projection(
        &harness,
        &mut outcomes,
        settings.thresholds.max_read_model_lag,
    )
    .await;
    if let Some(last_end) = last_end {
        report.read_model_lag_s = (last_end.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    }
    if unprojected > 0 {
        report.violation(format!(
            "{unprojected} completed runs were still missing from the read model {:?} after the \
             last run ended",
            settings.thresholds.max_read_model_lag
        ));
    }
    for (kind, count) in errors.lock().unwrap_or_else(|p| p.into_inner()).iter() {
        *report.errors.entry(kind.clone()).or_default() += count;
    }
    let host = std::mem::take(&mut *host.lock().unwrap_or_else(|p| p.into_inner()));
    check(&mut report, &outcomes, mcp, &host);
    thresholds(&mut report, &settings.thresholds);
    if let Ok(stats) = harness
        .tool_stats(&ToolStatsFilter {
            toolset: Some(toolset.id.clone()),
            since: Some(began),
            ..ToolStatsFilter::default()
        })
        .await
    {
        report.server_tool_stats = stats
            .tools
            .into_iter()
            .map(|tool| ToolStatsLine {
                tool: tool.tool,
                calls: tool.calls,
                errors: tool.errors,
                retries: tool.retries,
                p50_ms: tool.latency_ms.p50,
                p99_ms: tool.latency_ms.p99,
            })
            .collect();
    }
    Ok(report)
}

async fn wait_until_ready(admin: &Client) -> Result<(), Error> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match admin.server_info().await {
            Ok(_) => return Ok(()),
            Err(error) if Instant::now() > deadline => {
                return Err(Error::new(format!(
                    "the server did not answer within 2 minutes: {error}"
                )));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

async fn key(
    admin: &Client,
    base_url: &str,
    tenant: &str,
    role: BuiltInRole,
) -> Result<Client, Error> {
    let key = admin
        .create_api_key(&CreateApiKey {
            tenant: Some(tenant.to_owned()),
            name: "rewind-load".into(),
            role: role.as_str().to_owned(),
            allowed_labels: Labels::new(),
        })
        .await?;
    Ok(Client::remote(base_url, key.key.as_deref())?)
}

fn toolset(mcp_url: &str) -> CreateToolset {
    CreateToolset {
        mcp: vec![McpServer {
            name: "fake".into(),
            url: Some(mcp_url.to_owned()),
            ..McpServer::default()
        }],
        host: vec![HostTool {
            name: "host.square".into(),
            description: "Squares a number, in the harness.".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "n": { "type": "integer" } },
                "required": ["n"]
            }),
            output_schema: Some(json!({
                "type": "object",
                "properties": { "square": { "type": "integer" } },
                "required": ["square"]
            })),
            policy: None,
        }],
        ..CreateToolset::default()
    }
}

/// Reads each completed run again until the read model holds it: its attempts then show the
/// retry of `fake.flaky`. Returns how many runs it still lacked after `limit`.
async fn await_projection(harness: &Client, outcomes: &mut [Outcome], limit: Duration) -> usize {
    let deadline = Instant::now() + limit;
    let mut pending: Vec<usize> = outcomes
        .iter()
        .enumerate()
        .filter(|(_, outcome)| outcome.status == "completed" && outcome.run.is_some())
        .map(|(index, _)| index)
        .collect();
    let permits = Arc::new(Semaphore::new(32));
    loop {
        let mut reads = tokio::task::JoinSet::new();
        for &index in &pending {
            let Some(run) = outcomes[index].run.clone() else {
                continue;
            };
            let harness = harness.clone();
            let permits = permits.clone();
            reads.spawn(async move {
                let _permit = permits.acquire_owned().await;
                (index, harness.get_run(&run, None).await)
            });
        }
        let mut still = Vec::new();
        while let Some(read) = reads.join_next().await {
            let Ok((index, state)) = read else {
                continue;
            };
            match state {
                Ok(state) if state.attempts.iter().any(|call| call.attempts >= 2) => {
                    outcomes[index].attempts = state.attempts;
                }
                _ => still.push(index),
            }
        }
        pending = still;
        if pending.is_empty() || Instant::now() >= deadline {
            return pending.len();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Adds a violation for each threshold the load missed.
fn thresholds(report: &mut Report, thresholds: &Thresholds) {
    if let Some(min) = thresholds.min_runs_per_s
        && report.runs_per_s < min
    {
        report.violation(format!(
            "{:.1} runs/s, below the threshold of {min:.1}",
            report.runs_per_s
        ));
    }
    let above =
        |p99: f64, max: Option<Duration>| max.filter(|max| p99 > max.as_secs_f64() * 1_000.0);
    if let Some(max) = above(
        report.latency_ms.end_to_end.p99,
        thresholds.max_p99_end_to_end,
    ) {
        report.violation(format!(
            "p99 end to end {:.0} ms, above the threshold of {} ms",
            report.latency_ms.end_to_end.p99,
            max.as_millis()
        ));
    }
    if let Some(max) = above(
        report.latency_ms.activation.p99,
        thresholds.max_p99_activation,
    ) {
        report.violation(format!(
            "p99 activation {:.0} ms, above the threshold of {} ms",
            report.latency_ms.activation.p99,
            max.as_millis()
        ));
    }
}

/// The tasks a host worker holds at once.
const HOST_SLOTS: u32 = 16;

/// How long a host worker waits before it opens its stream again.
const STREAM_BACKOFF: Duration = Duration::from_millis(200);

/// Answers `host.square` on a task stream until stopped: the tasks handed to the worker as
/// they become ready, `query.slots` at once. A stream that fails or ends is opened again with the
/// same worker id, after a backoff: the tasks handed on it stay this worker's.
async fn host_worker(
    worker: Client,
    query: TaskStreamQuery,
    record: Arc<Mutex<HostRecord>>,
    errors: Arc<Mutex<BTreeMap<String, usize>>>,
    stop: CancellationToken,
) {
    let error = |kind: String| {
        *errors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(kind)
            .or_default() += 1;
    };
    loop {
        let opened = tokio::select! {
            () = stop.cancelled() => return,
            opened = worker.task_stream(&query) => opened,
        };
        match opened {
            Ok(mut stream) => loop {
                let event = tokio::select! {
                    () = stop.cancelled() => return,
                    event = stream.next() => event,
                };
                match event {
                    Some(Ok(TaskStreamEvent::Task(task))) => {
                        answer(&worker, *task, &record, &error).await;
                    }
                    // Its run ended, or its call timed out: nothing to answer.
                    Some(Ok(TaskStreamEvent::Gone(_))) => {}
                    Some(Err(failure)) => {
                        error(format!("task stream: {}", kind(&failure)));
                        break;
                    }
                    None => break,
                }
            },
            Err(failure) => error(format!("task stream: {}", kind(&failure))),
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(STREAM_BACKOFF) => {}
        }
    }
}

/// Answers one task of `host.square`.
async fn answer(
    worker: &Client,
    task: rewind_api::tasks::HandedTask,
    record: &Mutex<HostRecord>,
    error: &impl Fn(String),
) {
    *record
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .handed
        .entry((task.run_id.clone(), task.seq))
        .or_default() += 1;
    let n = task.args["n"].as_u64().unwrap_or(0);
    let result = CompleteTask {
        result: json!({ "square": n * n }),
    };
    match worker.complete_task(&task.token, &result).await {
        Ok(()) => {
            record
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .answered
                .entry(task.run_id)
                .or_insert_with(Instant::now);
        }
        Err(failure) => error(format!("complete task: {}", kind(&failure))),
    }
}

/// A short name for an error of the API, for counting.
fn kind(error: &rewind_api::Error) -> String {
    match error {
        rewind_api::Error::Problem(problem) => format!("{} {}", problem.status, problem.title),
        rewind_api::Error::Transport { .. } => "transport".into(),
        rewind_api::Error::Decode(_) => "decode".into(),
        other => other.to_string(),
    }
}

/// Starts run `n`, signals it, and waits for its end.
async fn drive(shared: &Shared, n: usize) -> Outcome {
    let mut outcome = Outcome {
        n,
        sent: Instant::now(),
        run: None,
        started: None,
        ended: None,
        status: "refused".into(),
        attempts: Vec::new(),
        violations: Vec::new(),
        errors: Vec::new(),
    };
    if shared.stop.is_cancelled() {
        outcome.status = "not started".into();
        return outcome;
    }
    let mut start = StartRun::new(shared.script.clone());
    start.toolset = Some(shared.toolset.clone());
    start.input = json!({ "n": n, "pad": shared.pad });
    start.labels = shared.labels.clone();
    let Ok(key) = IdempotencyKey::new(format!("{}-{n}", shared.id)) else {
        outcome.errors.push("invalid idempotency key".into());
        return outcome;
    };
    // A start lost in transit is sent again with its key: the server answers with the run.
    let mut run = None;
    for _ in 0..3 {
        match shared.harness.start_run(&start, Some(&key)).await {
            Ok(started) => {
                run = Some(started);
                break;
            }
            Err(error @ rewind_api::Error::Transport { .. }) => {
                outcome.errors.push(format!("start: {}", kind(&error)));
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(error) => {
                outcome.errors.push(format!("start: {}", kind(&error)));
                return outcome;
            }
        }
    }
    let Some(run) = run else {
        return outcome;
    };
    outcome.started = Some(Instant::now());
    outcome.run = Some(run.id.clone());

    // The signal arrives before the script waits for it, twice with the same key.
    if let Ok(key) = IdempotencyKey::new(format!("{}-{n}-go", shared.id)) {
        let payload = json!({ "n": n });
        for (attempt, duplicate) in [(1, false), (2, true)] {
            match shared
                .harness
                .signal_run(&run.id, "go", &payload, Some(&key))
                .await
            {
                Ok(accepted) if accepted.duplicate != duplicate => {
                    outcome.violations.push(format!(
                        "run {} (n={n}): signal {attempt} with one key answered duplicate={}",
                        run.id, accepted.duplicate
                    ));
                }
                Ok(_) => {}
                Err(error) => outcome.errors.push(format!("signal: {}", kind(&error))),
            }
        }
    }

    let last = wait_for_end(shared, &run.id, &mut outcome).await;
    outcome.ended = Some(Instant::now());
    let Some(last) = last else {
        outcome.status = "unfinished".into();
        return outcome;
    };
    outcome.status = status_name(last.status).into();
    outcome.attempts = last.attempts.clone();
    if last.status == RunStatus::Completed {
        let output = last
            .output
            .as_ref()
            .and_then(|payload| payload.value.clone());
        if output.as_ref() != Some(&expected_output(n, shared.pad.len())) {
            outcome.violations.push(format!(
                "run {} (n={n}) returned {}",
                run.id,
                output.map_or_else(|| "nothing".into(), |value| value.to_string())
            ));
        }
    } else {
        outcome.violations.push(format!(
            "run {} (n={n}) ended {}: {}",
            run.id,
            status_name(last.status),
            last.error
                .as_ref()
                .map(|error| format!("{}: {}", error.name, error.message))
                .or_else(|| last.abort.as_ref().map(|abort| format!("{abort:?}")))
                .unwrap_or_default()
        ));
    }
    outcome
}

/// The run once it has ended; `None` at the deadline or when interrupted.
async fn wait_for_end(shared: &Shared, run: &str, outcome: &mut Outcome) -> Option<Run> {
    loop {
        let left = shared.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || shared.stop.is_cancelled() {
            return None;
        }
        let wait = left.min(Duration::from_secs(60));
        let answer = tokio::select! {
            () = shared.stop.cancelled() => return None,
            answer = shared.harness.get_run(run, Some(wait.into())) => answer,
        };
        match answer {
            Ok(state) if is_terminal(state.status) => return Some(state),
            Ok(_) => {}
            Err(error) => {
                outcome.errors.push(format!("get run: {}", kind(&error)));
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

fn is_terminal(status: RunStatus) -> bool {
    !matches!(status, RunStatus::Pending | RunStatus::Active)
}

fn status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "pending",
        RunStatus::Active => "active",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Aborted => "aborted",
    }
}

/// Fills the report from the outcomes, and checks the invariants:
///
/// - every run completed, with its expected output;
/// - each tool call reached its tool exactly as many times as the run says it was attempted:
///   once, or more after a retry (`flaky` at least twice), never more;
/// - every call belongs to a run of this load and carries an idempotency key.
fn check(report: &mut Report, outcomes: &[Outcome], mcp: &FakeServer, host: &HostRecord) {
    let mut start = Vec::new();
    let mut first_call = Vec::new();
    let mut activation = Vec::new();
    let mut end_to_end = Vec::new();
    let mut first_sent: Option<Instant> = None;
    let mut last_end: Option<Instant> = None;
    let record = mcp.record();

    // The MCP calls of each run, by tool.
    let mut calls: HashMap<&str, Vec<(u32, &crate::mcp::Calls)>> = HashMap::new();
    for (key, call) in &record.calls {
        let Some((run, seq)) = key
            .rsplit_once('/')
            .and_then(|(run, seq)| Some((run, seq.parse::<u32>().ok()?)))
        else {
            report.violation(format!(
                "a call with the key {key:?}, not {{run_id}}/{{seq}}"
            ));
            continue;
        };
        calls.entry(run).or_default().push((seq, call));
    }
    let runs: HashMap<&str, &Outcome> = outcomes
        .iter()
        .filter_map(|outcome| Some((outcome.run.as_deref()?, outcome)))
        .collect();
    for run in calls.keys() {
        if !runs.contains_key(run) {
            report.violation(format!(
                "the fake MCP server got a call of run {run}, not one of this load"
            ));
        }
    }
    if record.without_key > 0 {
        report.violation(format!(
            "{} MCP calls came without an idempotency key",
            record.without_key
        ));
    }
    for (run, _) in host.handed.keys() {
        if !runs.contains_key(run.as_str()) {
            report.violation(format!("a host task of run {run}, not one of this load"));
        }
    }

    for outcome in outcomes {
        *report.statuses.entry(outcome.status.clone()).or_default() += 1;
        for error in &outcome.errors {
            report.error(error.clone());
        }
        for violation in &outcome.violations {
            report.violation(violation.clone());
        }
        first_sent = Some(first_sent.map_or(outcome.sent, |first| first.min(outcome.sent)));
        if let Some(ended) = outcome.ended {
            last_end = Some(last_end.map_or(ended, |last| last.max(ended)));
        }
        let (Some(run), Some(started)) = (outcome.run.as_deref(), outcome.started) else {
            continue;
        };
        start.push(started - outcome.sent);
        if outcome.status != "completed" {
            continue;
        }
        if let Some(ended) = outcome.ended {
            end_to_end.push(ended - outcome.sent);
        }
        let attempts = |seq: u32| {
            outcome
                .attempts
                .iter()
                .find(|attempt| attempt.seq == seq)
                .map_or(1, |attempt| attempt.attempts)
        };
        let run_calls = calls.get(run).map(Vec::as_slice).unwrap_or_default();
        let mut tools: BTreeMap<&str, u32> = BTreeMap::new();
        for (seq, call) in run_calls {
            *tools.entry(call.tool.as_str()).or_default() += 1;
            let expected = attempts(*seq);
            if call.count != expected {
                report.violation(format!(
                    "run {run} (n={}): {} #{seq} reached the server {} times, attempted {expected} \
                     times",
                    outcome.n, call.tool, call.count
                ));
            }
            if call.tool == "flaky" && call.count < 2 {
                report.violation(format!(
                    "run {run} (n={}): flaky #{seq} was not retried after its transient failure",
                    outcome.n
                ));
            }
            match call.tool.as_str() {
                "echo" => first_call.push(call.first.saturating_duration_since(started)),
                "flaky" => {
                    if let Some(answered) = host.answered.get(run) {
                        activation.push(call.first.saturating_duration_since(*answered));
                    }
                }
                _ => {}
            }
        }
        if tools.get("echo") != Some(&1) || tools.get("flaky") != Some(&1) {
            report.violation(format!(
                "run {run} (n={}): MCP calls by tool {tools:?}, expected one echo and one flaky",
                outcome.n
            ));
        }
        let host_calls: Vec<(u32, u32)> = host
            .handed
            .iter()
            .filter(|((handed, _), _)| handed == run)
            .map(|((_, seq), count)| (*seq, *count))
            .collect();
        if host_calls.len() != 1 {
            report.violation(format!(
                "run {run} (n={}): {} host calls, expected one",
                outcome.n,
                host_calls.len()
            ));
        }
        for (seq, count) in host_calls {
            let expected = attempts(seq);
            if count != expected {
                report.violation(format!(
                    "run {run} (n={}): host.square #{seq} was handed out {count} times, attempted \
                     {expected} times",
                    outcome.n
                ));
            }
        }
    }

    let tool_calls: u64 = record
        .calls
        .values()
        .map(|call| u64::from(call.count))
        .sum::<u64>()
        + host
            .handed
            .values()
            .map(|count| u64::from(*count))
            .sum::<u64>();
    let ended = outcomes
        .iter()
        .filter(|outcome| {
            !matches!(
                outcome.status.as_str(),
                "unfinished" | "refused" | "not started"
            )
        })
        .count();
    if let (Some(first), Some(last)) = (first_sent, last_end) {
        let elapsed = (last - first).as_secs_f64();
        report.elapsed_s = (elapsed * 10.0).round() / 10.0;
        if elapsed > 0.0 {
            report.runs_per_s = (ended as f64 / elapsed * 10.0).round() / 10.0;
            report.tool_calls_per_s = (tool_calls as f64 / elapsed * 10.0).round() / 10.0;
        }
    }
    report.latency_ms = Latencies {
        start: Percentiles::of(&start),
        first_call: Percentiles::of(&first_call),
        activation: Percentiles::of(&activation),
        end_to_end: Percentiles::of(&end_to_end),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_returns_what_the_check_expects() {
        assert!(SCRIPT.contains("SLEEP"));
        assert_eq!(
            expected_output(3, 10),
            json!({
                "n": 3,
                "echoed": 3,
                "padded": 10,
                "squared": 9,
                "flaky": 3,
                "go": { "n": 3 },
                "again": null
            })
        );
    }

    fn completed(run: &str, n: usize, attempts: Vec<CallAttempts>) -> Outcome {
        let now = Instant::now();
        Outcome {
            n,
            sent: now,
            run: Some(run.into()),
            started: Some(now),
            ended: Some(now + Duration::from_secs(2)),
            status: "completed".into(),
            attempts,
            violations: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn calls(server: &FakeServer, key: &str, tool: &str, times: u32) {
        for _ in 0..times {
            server.called(Some(key), tool);
        }
    }

    #[test]
    fn calls_matching_their_attempts_hold_the_invariants() {
        let server = FakeServer::default();
        calls(&server, "r1/1", "echo", 1);
        calls(&server, "r1/3", "flaky", 2);
        let mut host = HostRecord::default();
        host.handed.insert(("r1".into(), 2), 1);
        let outcomes = [completed(
            "r1",
            1,
            vec![CallAttempts {
                seq: 3,
                attempts: 2,
            }],
        )];
        let mut report = Report {
            runs: 1,
            ..Report::default()
        };
        check(&mut report, &outcomes, &server, &host);
        assert_eq!(report.violations, Vec::<String>::new());
        assert!(report.passed());
    }

    #[test]
    fn missed_thresholds_are_violations() {
        let mut report = Report {
            runs_per_s: 9.0,
            ..Report::default()
        };
        report.latency_ms.end_to_end.p99 = 12_000.0;
        report.latency_ms.activation.p99 = 900.0;
        let mut met = report.clone();
        thresholds(
            &mut met,
            &Thresholds {
                min_runs_per_s: Some(5.0),
                max_p99_end_to_end: Some(Duration::from_secs(15)),
                max_p99_activation: Some(Duration::from_secs(1)),
                max_read_model_lag: Duration::from_secs(60),
            },
        );
        assert_eq!(met.violation_count, 0);
        thresholds(
            &mut report,
            &Thresholds {
                min_runs_per_s: Some(10.0),
                max_p99_end_to_end: Some(Duration::from_secs(10)),
                max_p99_activation: Some(Duration::from_millis(500)),
                max_read_model_lag: Duration::from_secs(60),
            },
        );
        assert_eq!(report.violation_count, 3, "{:?}", report.violations);
    }

    #[test]
    fn a_call_sent_beyond_its_attempts_breaks_an_invariant() {
        let server = FakeServer::default();
        calls(&server, "r1/1", "echo", 2);
        calls(&server, "r1/3", "flaky", 2);
        calls(&server, "other/1", "echo", 1);
        let mut host = HostRecord::default();
        host.handed.insert(("r1".into(), 2), 2);
        let outcomes = [completed(
            "r1",
            1,
            vec![CallAttempts {
                seq: 3,
                attempts: 2,
            }],
        )];
        let mut report = Report {
            runs: 1,
            ..Report::default()
        };
        check(&mut report, &outcomes, &server, &host);
        assert_eq!(report.violation_count, 3, "{:?}", report.violations);
        assert!(!report.passed());
    }
}
