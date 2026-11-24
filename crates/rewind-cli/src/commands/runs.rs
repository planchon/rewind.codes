//! The commands about runs, all through the API: `run`, `status`, `signal`, `cancel`, `fork`,
//! `transcript`, `runs`, `release`, `export`.

use std::time::{Duration as StdDuration, Instant};

use rewind_api::Client;
use rewind_api::common::Duration;
use rewind_api::headers::IdempotencyKey;
use rewind_api::runs::{CancelRun, ExportAs, ForkRun, Run, RunFilter, StartRun};
use serde_json::Value;

use super::Server;
use crate::args::{
    CancelArgs, ExportArgs, ExportFormat, ForkArgs, ReleaseArgs, RunArgs, RunsArgs, SignalArgs,
    StatusArgs, TranscriptArgs,
};
use crate::error::CliError;
use crate::exit::Exit;
use crate::input::{self, ActionSetRef};
use crate::interrupt;
use crate::output::Console;
use crate::render;

/// How long after the wait's deadline the request is given up if the server has not answered.
const ANSWER_GRACE: StdDuration = StdDuration::from_secs(10);
/// A wait whose connection broke after it was held this long was cut (by a proxy): it is sent
/// again with the time left.
const CUT_AFTER: StdDuration = StdDuration::from_secs(1);

pub(super) async fn run(
    server: &Server<'_>,
    args: RunArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let source = input::read_text(&args.code)?;
    let mut start = StartRun::new(source);
    if let Some(text) = &args.input {
        start.input = input::json_value("--input", text)?;
    }
    start.labels = input::labels(&args.start.labels)?;
    start.secrets = input::secrets(&args.start.secrets)?;
    start.ttl = args.start.ttl;
    if args.start.no_check {
        start.check = Some(false);
    }
    let key = idempotency_key(args.start.key.as_deref())?;
    let action_set = args
        .action_set
        .as_deref()
        .map(ActionSetRef::parse)
        .transpose()?;

    let client = server.starting_client().await?;
    let deadline = Instant::now() + args.start.wait.get();
    start.action_set = match action_set {
        None => None,
        Some(ActionSetRef::Id(id)) => Some(id),
        // Content-addressed: registering the same file again gives the same id.
        Some(ActionSetRef::File(path)) => {
            let create = input::read_json(&path)?;
            Some(client.create_action_set(&create).await?.id)
        }
    };
    // Answered as soon as the run exists, so that its id is known at once; the wait follows.
    interrupt::before_change("the start of the run")?;
    let run = client.start_run(&start, key.as_ref()).await?;
    started(console, &run);
    let run = wait_for_end(&client, run, deadline).await?;
    show_run(console, &run)?;
    Ok(Exit::of_run(&run))
}

pub(super) async fn fork(
    server: &Server<'_>,
    args: ForkArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let mut fork = ForkRun::new(input::read_text(&args.code)?);
    fork.at = args.at;
    fork.input = args
        .input
        .as_deref()
        .map(|text| input::json_value("--input", text))
        .transpose()?;
    if !args.start.labels.is_empty() {
        fork.labels = Some(input::labels(&args.start.labels)?);
    }
    if !args.start.secrets.is_empty() {
        fork.secrets = Some(input::secrets(&args.start.secrets)?);
    }
    fork.ttl = args.start.ttl;
    if args.start.no_check {
        fork.check = Some(false);
    }
    let key = idempotency_key(args.start.key.as_deref())?;
    let client = server.client().await?;
    let deadline = Instant::now() + args.start.wait.get();
    // Answered as soon as the fork exists; the wait follows.
    interrupt::before_change("the fork")?;
    let run = client.fork_run(&args.run, &fork, key.as_ref()).await?;
    started(console, &run);
    let run = wait_for_end(&client, run, deadline).await?;
    show_run(console, &run)?;
    Ok(Exit::of_run(&run))
}

pub(super) async fn status(
    server: &Server<'_>,
    args: StatusArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let client = server.client().await?;
    let deadline = Instant::now() + args.wait.map(Duration::get).unwrap_or_default();
    let run = match args.wait {
        Some(_) => held(&client, &args.run, deadline).await?,
        None => Some(client.get_run(&args.run, None).await?),
    };
    let run = match run {
        Some(run) => run,
        // Ctrl-C before the first answer: the run as it is now.
        None => client.get_run(&args.run, None).await?,
    };
    show_run(console, &run)?;
    Ok(Exit::of_run(&run))
}

pub(super) async fn signal(
    server: &Server<'_>,
    args: SignalArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let payload = match &args.payload {
        Some(text) => input::json_value("the payload", text)?,
        None => Value::Null,
    };
    let key = idempotency_key(args.key.as_deref())?;
    let client = server.client().await?;
    interrupt::before_change("the signal")?;
    let accepted = client
        .signal_run(&args.run, &args.name, &payload, key.as_ref())
        .await?;
    if console.json() {
        console.print_json(&accepted)?;
    } else if accepted.duplicate {
        console.line(format!(
            "signal {:?} already sent to run {} with this key: not sent again",
            accepted.name, accepted.run_id
        ));
    } else {
        console.line(format!(
            "signal {:?} sent to run {}",
            accepted.name, accepted.run_id
        ));
    }
    Ok(Exit::Success)
}

pub(super) async fn cancel(
    server: &Server<'_>,
    args: CancelArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let client = server.client().await?;
    let deadline = Instant::now() + args.wait.map(Duration::get).unwrap_or_default();
    // Answered at once, so that the change is known; the wait follows, which Ctrl-C abandons.
    let request = CancelRun {
        reason: args.reason,
        wait: None,
    };
    interrupt::before_change("the cancellation")?;
    let run = client.cancel_run(&args.run, &request).await?;
    match args.wait {
        Some(_) => {
            let run = wait_for_end(&client, run, deadline).await?;
            show_run(console, &run)?;
            Ok(Exit::of_run(&run))
        }
        None => {
            show_run(console, &run)?;
            Ok(Exit::Success)
        }
    }
}

pub(super) async fn release(
    server: &Server<'_>,
    args: ReleaseArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let client = server.client().await?;
    interrupt::before_change(if args.give_up {
        "the give-up"
    } else {
        "the release"
    })?;
    let run = if args.give_up {
        client.give_up_run(&args.run).await?
    } else {
        client.release_run(&args.run).await?
    };
    show_run(console, &run)?;
    Ok(Exit::Success)
}

pub(super) async fn transcript(
    server: &Server<'_>,
    args: TranscriptArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let client = server.client().await?;
    match args.event {
        Some(index) => {
            let entry = client.run_event(&args.run, index).await?;
            console.print_json(&entry)?;
        }
        None => {
            let transcript = client.run_transcript(&args.run).await?;
            if console.json() {
                console.print_json(&transcript)?;
            } else {
                console.text(&transcript.text);
            }
        }
    }
    Ok(Exit::Success)
}

pub(super) async fn list(
    server: &Server<'_>,
    args: RunsArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let filter = RunFilter {
        q: args.query,
        sort: args.sort.map(Into::into),
        status: args.status.map(Into::into),
        labels: args
            .labels
            .iter()
            .map(|pair| input::label_filter(pair))
            .collect::<Result<_, _>>()?,
        action: args.action,
        script: args.script,
        error: args.error,
        quarantined: args.quarantined.then_some(true),
        source: args.source,
        workflow: None,
        started_by: None,
        started_after: None,
        started_before: None,
        limit: args.limit,
        cursor: args.cursor,
    };
    let client = server.client().await?;
    let page = client.list_runs(&filter).await?;
    if console.json() {
        console.print_json(&page)?;
    } else {
        console.text(&render::runs(&page.runs));
        if let Some(cursor) = &page.next_cursor {
            console.note(format!("more runs: --cursor {cursor}"));
        }
    }
    Ok(Exit::Success)
}

pub(super) async fn export(
    server: &Server<'_>,
    args: ExportArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let client = server.client().await?;
    let export = match args.export_as {
        ExportFormat::Run => ExportAs::Run,
        ExportFormat::Cassette => ExportAs::Cassette,
    };
    let document = client.export_run(&args.run, export).await?;
    // Written in the stored format v1, byte for byte as a store writes it, and checked on the
    // way: what is written always reads back.
    let text = stored_form(export, &document)?;
    match &args.output {
        Some(path) => {
            std::fs::write(path, &text).map_err(|source| CliError::file(path, source))?;
            if !console.json() {
                console.note(format!("wrote {}", path.display()));
            }
        }
        None => console.text(&text),
    }
    Ok(Exit::Success)
}

/// An exported document in the stored format v1.
pub(super) fn stored_form(export: ExportAs, document: &Value) -> Result<String, CliError> {
    let json = document.to_string();
    let invalid = |error: rewind_format::FormatError| {
        CliError::Api(rewind_api::Error::Decode(format!(
            "the exported {} is not a document of the stored format v1: {error}",
            export.as_str()
        )))
    };
    Ok(match export {
        ExportAs::Run => rewind_format::encode_run_export(
            &rewind_format::decode_run_export(&json).map_err(invalid)?,
        ),
        ExportAs::Cassette => {
            rewind_format::encode_cassette(&rewind_format::decode_cassette(&json).map_err(invalid)?)
        }
    })
}

fn idempotency_key(key: Option<&str>) -> Result<Option<IdempotencyKey>, CliError> {
    key.map(|key| {
        IdempotencyKey::new(key).map_err(|error| CliError::usage(format!("--key {key:?}: {error}")))
    })
    .transpose()
}

/// Waits until the run ends, is quarantined, or the deadline passes; the first Ctrl-C stops
/// waiting. Returns the run as last read.
pub(super) async fn wait_for_end(
    client: &Client,
    run: Run,
    deadline: Instant,
) -> Result<Run, CliError> {
    if run.status.is_terminal() || run.quarantine.is_some() || Instant::now() >= deadline {
        return Ok(run);
    }
    Ok(held(client, &run.id, deadline).await?.unwrap_or(run))
}

/// The run, read with one request the server holds until the run ends or is quarantined, or
/// until `deadline` (`archi/07-code-architecture.md`, "Embedded server and CLI"): nothing is
/// asked again once the server has answered. A connection cut while the server held it (a
/// proxy) is sent again with the time left; one the server has not answered shortly after the
/// deadline is given up, and the run read as it is. `None` at the first Ctrl-C.
async fn held(client: &Client, run: &str, deadline: Instant) -> Result<Option<Run>, CliError> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let wait = (!left.is_zero()).then(|| Duration::from(left));
        let sent = Instant::now();
        let answer = tokio::time::timeout(left + ANSWER_GRACE, client.get_run(run, wait));
        match interrupt::abandonable(answer).await {
            None => return Ok(None),
            Some(Ok(Ok(read))) => return Ok(Some(read)),
            Some(Ok(Err(rewind_api::Error::Transport { .. }))) if sent.elapsed() >= CUT_AFTER => {}
            Some(Ok(Err(error))) => return Err(error.into()),
            // The server did not answer by the deadline: the run as it is now.
            Some(Err(_)) => return Ok(Some(client.get_run(run, None).await?)),
        }
    }
}

fn started(console: &mut Console, run: &Run) {
    if !console.json() && !run.status.is_terminal() {
        console.note(format!("started run {}", run.handle));
    }
}

fn show_run(console: &mut Console, run: &Run) -> Result<(), CliError> {
    if console.json() {
        console.print_json(run)
    } else {
        console.text(&render::run(run));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    const RUN: &str = r#"{"id":"r1","handle":"local/script/06bqx3k9f2mt","namespace":"local",
        "status":"completed","codeDigest":"aa",
        "actionSet":"bb","epoch":1,"mode":"live","labels":{},"traceId":"00","historyLen":3,
        "createdAt":"2026-11-09T10:00:00Z","updatedAt":"2026-11-09T10:00:01Z",
        "expiresAt":"2026-12-09T10:00:00Z"}"#;

    /// The request line of the next request on `socket`.
    async fn request_line(socket: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") && socket.read(&mut byte).await.unwrap_or(0) == 1 {
            head.push(byte[0]);
        }
        String::from_utf8_lossy(&head)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    }

    async fn answer(socket: &mut TcpStream) {
        let body = RUN.replace(['\n', ' '], "");
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    }

    /// A server that holds the first `cut` requests a while and then drops their connection,
    /// and answers the next one with the run; and the request lines it saw.
    async fn server(cut: usize) -> (Client, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for index in 0.. {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let line = request_line(&mut socket).await;
                log.lock().unwrap().push(line);
                if index < cut {
                    tokio::time::sleep(StdDuration::from_millis(1_100)).await;
                    drop(socket);
                } else {
                    answer(&mut socket).await;
                }
            }
        });
        let client = Client::remote(&format!("http://{address}"), None).unwrap();
        (client, seen)
    }

    fn wait_of(line: &str) -> u64 {
        let wait = line
            .split(['?', '&', ' '])
            .find_map(|part| part.strip_prefix("wait="))
            .unwrap_or_default();
        wait.parse::<Duration>().unwrap().as_millis()
    }

    #[tokio::test]
    async fn a_wait_is_one_request_with_the_whole_wait() {
        let (client, seen) = server(0).await;
        let deadline = Instant::now() + StdDuration::from_secs(3_600);
        let run = held(&client, "r1", deadline).await.unwrap().unwrap();
        assert_eq!(run.status, rewind_api::common::RunStatus::Completed);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].starts_with("GET /v1/runs/r1?wait="), "{seen:?}");
        assert!(wait_of(&seen[0]) > 3_590_000, "{seen:?}");
    }

    #[tokio::test]
    async fn a_cut_wait_is_sent_again_with_the_time_left() {
        let (client, seen) = server(1).await;
        let deadline = Instant::now() + StdDuration::from_secs(60);
        let run = held(&client, "r1", deadline).await.unwrap().unwrap();
        assert_eq!(run.id, "r1");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        let (first, second) = (wait_of(&seen[0]), wait_of(&seen[1]));
        assert!(first > 59_000, "{seen:?}");
        assert!(second < first - 1_000, "{seen:?}");
    }

    #[tokio::test]
    async fn a_run_that_ended_is_not_waited_for() {
        let (client, seen) = server(0).await;
        let run: Run = serde_json::from_str(RUN).unwrap();
        let deadline = Instant::now() + StdDuration::from_secs(60);
        assert_eq!(wait_for_end(&client, run, deadline).await.unwrap().id, "r1");
        assert!(seen.lock().unwrap().is_empty());
    }
}
