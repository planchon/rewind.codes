//! One load: the action set, the code, the runs driven through the API, the harness's worker,
//! and the checks once every run has ended.
//!
//! Each run makes an MCP call (`fake.echo`), a harness call (`host.square`, answered by this
//! process through `GET /v1/calls`), an MCP call that fails transiently once and is retried
//! (`fake.flaky`), a durable sleep, then reads a signal sent right after its start (sent twice
//! with one idempotency key) and waits for a second one that never comes (a timer).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt as _;
use rewind_api::Client;
use rewind_api::access::{BuiltInRole, CreateApiKey};
use rewind_api::action_sets::{CreateActionSet, HarnessAction, McpServer};
use rewind_api::calls::{CallStreamMessage, CallStreamQuery, CompleteCall};
use rewind_api::common::{Labels, RunStatus};
use rewind_api::headers::IdempotencyKey;
use rewind_api::namespaces::NamespaceName;
use rewind_api::reading::ActionStatsFilter;
use rewind_api::runs::{Run, StartRun};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::mcp::FakeServer;
use crate::report::{ActionStatsLine, Latencies, Percentiles, Report};

/// The code of every run; `SLEEP` is replaced by the load's sleep.
const SCRIPT: &str = r#"import { signal, sleep } from "rewind";
import { fake, host } from "rewind:actions";

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
    pub(crate) harness_workers: usize,
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
}

/// What the harness's worker saw.
#[derive(Debug, Default)]
struct HarnessRecord {
    /// Hand-outs by run and seq.
    handed: HashMap<(String, u32), u32>,
    /// When the result of each run's harness call was accepted.
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
    violations: Vec<String>,
    errors: Vec<String>,
}

struct Shared {
    harness: Client,
    action_set: String,
    labels: Labels,
    id: String,
    code: String,
    pad: String,
    stop: CancellationToken,
    deadline: Instant,
}

/// Prepares the tenant's keys and the action set, drives the runs, and checks them.
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
    let action_set = harness
        .create_action_set(&action_set(&settings.mcp_url))
        .await
        .map_err(|error| {
            Error::new(format!(
                "cannot register the action set (the server must reach the fake MCP server at {}): \
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
        action_set: action_set.id.clone(),
        labels: [("load", id.as_str())].into_iter().collect(),
        id: id.clone(),
        code: SCRIPT.replace("SLEEP", &settings.sleep),
        pad: "x".repeat(settings.payload_bytes),
        stop: interrupt.clone(),
        deadline: Instant::now() + settings.timeout,
    });

    let host = Arc::new(Mutex::new(HarnessRecord::default()));
    let errors = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
    let workers_stop = CancellationToken::new();
    let workers: Vec<_> = (0..settings.harness_workers.max(1))
        .map(|index| {
            tokio::spawn(harness_worker(
                worker.clone(),
                CallStreamQuery {
                    action_set: Some(action_set.id.clone()),
                    slots: Some(HARNESS_SLOTS),
                    worker: Some(format!("{id}-host-{index}")),
                    ..CallStreamQuery::default()
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
    for (kind, count) in errors.lock().unwrap_or_else(|p| p.into_inner()).iter() {
        *report.errors.entry(kind.clone()).or_default() += count;
    }
    let host = std::mem::take(&mut *host.lock().unwrap_or_else(|p| p.into_inner()));
    check(&mut report, &outcomes, mcp, &host);
    thresholds(&mut report, &settings.thresholds);
    if let Ok(stats) = harness
        .action_stats(&ActionStatsFilter {
            action_set: Some(action_set.id.clone()),
            since: Some(began),
            ..ActionStatsFilter::default()
        })
        .await
    {
        report.server_action_stats = stats
            .actions
            .into_iter()
            .map(|action| ActionStatsLine {
                action: action.action,
                calls: action.calls,
                errors: action.errors,
                retries: action.redeliveries,
                p50_ms: action.latency_ms.p50,
                p99_ms: action.latency_ms.p99,
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
            organization_id: Some(tenant.to_owned()),
            name: "rewind-load".into(),
            role: role.as_str().to_owned(),
            allowed_labels: Labels::new(),
        })
        .await?;
    Ok(Client::remote(base_url, key.key.as_deref())?)
}

fn action_set(mcp_url: &str) -> CreateActionSet {
    CreateActionSet {
        mcp: vec![McpServer {
            name: "fake".into(),
            url: Some(mcp_url.to_owned()),
            ..McpServer::default()
        }],
        harness: vec![HarnessAction {
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
        ..CreateActionSet::default()
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

/// The tasks a harness worker holds at once.
const HARNESS_SLOTS: u32 = 16;

/// How long a harness worker waits before it opens its stream again.
const STREAM_BACKOFF: Duration = Duration::from_millis(200);

/// Answers `host.square` on a call stream until stopped: the tasks handed to the worker as
/// they become ready, `query.slots` at once. A stream that fails or ends is opened again with the
/// same worker id, after a backoff: the tasks handed on it stay this worker's.
async fn harness_worker(
    worker: Client,
    query: CallStreamQuery,
    record: Arc<Mutex<HarnessRecord>>,
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
            opened = worker.call_stream(&query) => opened,
        };
        match opened {
            Ok(mut stream) => loop {
                let event = tokio::select! {
                    () = stop.cancelled() => return,
                    event = stream.next() => event,
                };
                match event {
                    Some(Ok(CallStreamMessage::Call(task))) => {
                        answer(&worker, *task, &record, &error).await;
                    }
                    // Its run ended, or its call timed out: nothing to answer.
                    Some(Ok(CallStreamMessage::Gone(_))) => {}
                    Some(Err(failure)) => {
                        error(format!("call stream: {}", kind(&failure)));
                        break;
                    }
                    None => break,
                }
            },
            Err(failure) => error(format!("call stream: {}", kind(&failure))),
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
    task: rewind_api::calls::HandedCall,
    record: &Mutex<HarnessRecord>,
    error: &impl Fn(String),
) {
    *record
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .handed
        .entry((task.run_id.clone(), task.seq))
        .or_default() += 1;
    let n = task.args["n"].as_u64().unwrap_or(0);
    let result = CompleteCall {
        result: json!({ "square": n * n }),
    };
    match worker.complete_call(&task.token, &result).await {
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
        violations: Vec::new(),
        errors: Vec::new(),
    };
    if shared.stop.is_cancelled() {
        outcome.status = "not started".into();
        return outcome;
    }
    let mut start = StartRun::new(shared.code.clone());
    start.action_set = Some(shared.action_set.clone());
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

    // The signal arrives before the code waits for it, twice with the same key. The second is
    // a duplicate once the first was acknowledged; after a first that failed, the server may or
    // may not have taken it, so either answer holds.
    if let Ok(key) = IdempotencyKey::new(format!("{}-{n}-go", shared.id)) {
        let payload = json!({ "n": n });
        let mut acknowledged = false;
        for attempt in [1, 2] {
            match shared
                .harness
                .signal_run(&run.id, "go", &payload, Some(&key))
                .await
            {
                Ok(accepted) => {
                    let expected = match attempt {
                        1 => Some(false),
                        _ if acknowledged => Some(true),
                        _ => None,
                    };
                    if expected.is_some_and(|duplicate| duplicate != accepted.duplicate) {
                        outcome.violations.push(format!(
                            "run {} (n={n}): signal {attempt} with one key answered duplicate={}",
                            run.id, accepted.duplicate
                        ));
                    }
                    acknowledged = true;
                }
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
            Ok(state) if state.status.is_ended() => return Some(state),
            Ok(_) => {}
            Err(error) => {
                outcome.errors.push(format!("get run: {}", kind(&error)));
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

fn status_name(status: RunStatus) -> &'static str {
    status.as_str()
}

/// Fills the report from the outcomes, and checks the invariants:
///
/// - every run completed, with its expected output;
/// - each call reached its action as many times as its failures call for, never more: `echo`
///   and the harness call once, `flaky` twice (its transient failure, then the retry). A run's
///   deliveries live in its call tasks, which its end clears (`archi/04-tools.md`): the load
///   knows them from its actions;
/// - every call belongs to a run of this load and carries an idempotency key.
fn check(report: &mut Report, outcomes: &[Outcome], mcp: &FakeServer, host: &HarnessRecord) {
    let mut start = Vec::new();
    let mut first_call = Vec::new();
    let mut activation = Vec::new();
    let mut end_to_end = Vec::new();
    let mut first_sent: Option<Instant> = None;
    let mut last_end: Option<Instant> = None;
    let record = mcp.record();

    // The MCP calls of each run, by action.
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
            report.violation(format!("a harness task of run {run}, not one of this load"));
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
        let run_calls = calls.get(run).map(Vec::as_slice).unwrap_or_default();
        let mut actions: BTreeMap<&str, u32> = BTreeMap::new();
        for (seq, call) in run_calls {
            *actions.entry(call.action.as_str()).or_default() += 1;
            let expected = if call.action == "flaky" { 2 } else { 1 };
            if call.count != expected {
                report.violation(format!(
                    "run {run} (n={}): {} #{seq} reached the server {} times, expected {expected}",
                    outcome.n, call.action, call.count
                ));
            }
            match call.action.as_str() {
                "echo" => first_call.push(call.first.saturating_duration_since(started)),
                "flaky" => {
                    if let Some(answered) = host.answered.get(run) {
                        activation.push(call.first.saturating_duration_since(*answered));
                    }
                }
                _ => {}
            }
        }
        if actions.get("echo") != Some(&1) || actions.get("flaky") != Some(&1) {
            report.violation(format!(
                "run {run} (n={}): MCP calls by action {actions:?}, expected one echo and one flaky",
                outcome.n
            ));
        }
        let harness_calls: Vec<(u32, u32)> = host
            .handed
            .iter()
            .filter(|((handed, _), _)| handed == run)
            .map(|((_, seq), count)| (*seq, *count))
            .collect();
        if harness_calls.len() != 1 {
            report.violation(format!(
                "run {run} (n={}): {} harness calls, expected one",
                outcome.n,
                harness_calls.len()
            ));
        }
        for (seq, count) in harness_calls {
            if count != 1 {
                report.violation(format!(
                    "run {run} (n={}): host.square #{seq} was handed out {count} times, expected \
                     once",
                    outcome.n
                ));
            }
        }
    }

    let calls: u64 = record
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
            report.calls_per_s = (calls as f64 / elapsed * 10.0).round() / 10.0;
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

    fn completed(run: &str, n: usize) -> Outcome {
        let now = Instant::now();
        Outcome {
            n,
            sent: now,
            run: Some(run.into()),
            started: Some(now),
            ended: Some(now + Duration::from_secs(2)),
            status: "completed".into(),
            violations: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn calls(server: &FakeServer, key: &str, action: &str, times: u32) {
        for _ in 0..times {
            server.called(Some(key), action);
        }
    }

    #[test]
    fn calls_matching_their_failures_hold_the_invariants() {
        let server = FakeServer::default();
        calls(&server, "r1/1", "echo", 1);
        calls(&server, "r1/3", "flaky", 2);
        let mut host = HarnessRecord::default();
        host.handed.insert(("r1".into(), 2), 1);
        let outcomes = [completed("r1", 1)];
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
            },
        );
        assert_eq!(met.violation_count, 0);
        thresholds(
            &mut report,
            &Thresholds {
                min_runs_per_s: Some(10.0),
                max_p99_end_to_end: Some(Duration::from_secs(10)),
                max_p99_activation: Some(Duration::from_millis(500)),
            },
        );
        assert_eq!(report.violation_count, 3, "{:?}", report.violations);
    }

    #[test]
    fn a_call_sent_beyond_its_failures_breaks_an_invariant() {
        let server = FakeServer::default();
        calls(&server, "r1/1", "echo", 2);
        calls(&server, "r1/3", "flaky", 2);
        calls(&server, "other/1", "echo", 1);
        let mut host = HarnessRecord::default();
        host.handed.insert(("r1".into(), 2), 2);
        let outcomes = [completed("r1", 1)];
        let mut report = Report {
            runs: 1,
            ..Report::default()
        };
        check(&mut report, &outcomes, &server, &host);
        assert_eq!(report.violation_count, 3, "{:?}", report.violations);
        assert!(!report.passed());
    }
}
