//! The commands about runs, all through the API: `run`, `status`, `signal`, `cancel`, `fork`,
//! `trace`, `runs`, `release`, `export`.

use std::time::{Duration as StdDuration, Instant};

use ptc_api::Client;
use ptc_api::common::Duration;
use ptc_api::headers::IdempotencyKey;
use ptc_api::runs::{CancelRun, ForkRun, JournalExport, Run, RunFilter, StartRun, TraceFormat};
use serde_json::Value;

use super::Server;
use crate::args::{
    CancelArgs, ExportArgs, ExportFormat, ForkArgs, ReleaseArgs, RunArgs, RunsArgs, SignalArgs,
    StatusArgs, TraceArgs,
};
use crate::error::CliError;
use crate::exit::Exit;
use crate::input::{self, ToolsetRef};
use crate::interrupt;
use crate::output::Console;
use crate::render;

/// One request waits at most this long: proxies cut longer ones. A longer `--wait` is split.
const WAIT_CHUNK: StdDuration = StdDuration::from_secs(50);
/// A server that answers a wait early is asked again at most this often.
const WAIT_PACE: StdDuration = StdDuration::from_secs(1);

pub(super) async fn run(
    server: &Server<'_>,
    args: RunArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let source = input::read_text(&args.script)?;
    let mut start = StartRun::new(source);
    if let Some(text) = &args.input {
        start.input = input::json_value("--input", text)?;
    }
    start.labels = input::labels(&args.start.labels)?;
    start.ttl = args.start.ttl;
    if args.start.no_check {
        start.check = Some(false);
    }
    let key = idempotency_key(args.start.key.as_deref())?;
    let toolset = args.toolset.as_deref().map(ToolsetRef::parse).transpose()?;

    let client = server.client().await?;
    let deadline = Instant::now() + args.start.wait.get();
    start.toolset = match toolset {
        None => None,
        Some(ToolsetRef::Id(id)) => Some(id),
        // Content-addressed: registering the same file again gives the same id.
        Some(ToolsetRef::File(path)) => {
            let create = input::read_json(&path)?;
            Some(client.create_toolset(&create).await?.id)
        }
    };
    // Answered as soon as the run exists, so that its id is known at once; the wait follows.
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
    let mut fork = ForkRun::new(input::read_text(&args.script)?);
    fork.at = args.at;
    fork.input = args
        .input
        .as_deref()
        .map(|text| input::json_value("--input", text))
        .transpose()?;
    if !args.start.labels.is_empty() {
        fork.labels = Some(input::labels(&args.start.labels)?);
    }
    fork.ttl = args.start.ttl;
    if args.start.no_check {
        fork.check = Some(false);
    }
    let key = idempotency_key(args.start.key.as_deref())?;
    let client = server.client().await?;
    let deadline = Instant::now() + args.start.wait.get();
    // Answered as soon as the fork exists; the wait follows.
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
    let first = args.wait.map(first_wait).unwrap_or_default();
    let run = client.get_run(&args.run, first).await?;
    let run = wait_for_end(&client, run, deadline).await?;
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
    let request = CancelRun {
        reason: args.reason,
        wait: args.wait.map(first_wait).unwrap_or_default(),
    };
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
    let run = if args.give_up {
        client.give_up_run(&args.run).await?
    } else {
        client.release_run(&args.run).await?
    };
    show_run(console, &run)?;
    Ok(Exit::Success)
}

pub(super) async fn trace(
    server: &Server<'_>,
    args: TraceArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let client = server.client().await?;
    match args.entry {
        Some(index) => {
            let entry = client.run_entry(&args.run, index).await?;
            console.print_json(&entry)?;
        }
        None => {
            let trace = client.run_trace(&args.run, TraceFormat::Llm).await?;
            if console.json() {
                console.print_json(&trace)?;
            } else {
                console.text(&trace.text);
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
        status: args.status.map(Into::into),
        labels: args
            .labels
            .iter()
            .map(|pair| input::label_filter(pair))
            .collect::<Result<_, _>>()?,
        tool: args.tool,
        script: args.script,
        error: args.error,
        quarantined: args.quarantined.then_some(true),
        parent: args.parent,
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
        ExportFormat::Journal => JournalExport::Journal,
        ExportFormat::Cassette => JournalExport::Cassette,
    };
    let document = client.export_journal(&args.run, export).await?;
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
pub(super) fn stored_form(export: JournalExport, document: &Value) -> Result<String, CliError> {
    let json = document.to_string();
    let invalid = |error: ptc_format::FormatError| {
        CliError::Api(ptc_api::Error::Decode(format!(
            "the exported {} is not a document of the stored format v1: {error}",
            export.as_str()
        )))
    };
    Ok(match export {
        JournalExport::Journal => {
            ptc_format::encode_journal(&ptc_format::decode_journal(&json).map_err(invalid)?)
        }
        JournalExport::Cassette => {
            ptc_format::encode_cassette(&ptc_format::decode_cassette(&json).map_err(invalid)?)
        }
    })
}

fn idempotency_key(key: Option<&str>) -> Result<Option<IdempotencyKey>, CliError> {
    key.map(|key| {
        IdempotencyKey::new(key).map_err(|error| CliError::usage(format!("--key {key:?}: {error}")))
    })
    .transpose()
}

/// The wait of a first request: the first chunk of `--wait`.
fn first_wait(wait: Duration) -> Option<Duration> {
    let wait = wait.get().min(WAIT_CHUNK);
    (!wait.is_zero()).then(|| Duration::from(wait))
}

/// Waits until the run ends, is quarantined, or the deadline passes; the first Ctrl-C stops
/// waiting. Returns the run as last read.
pub(super) async fn wait_for_end(
    client: &Client,
    mut run: Run,
    deadline: Instant,
) -> Result<Run, CliError> {
    loop {
        if run.status.is_terminal() || run.quarantine.is_some() {
            return Ok(run);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(run);
        }
        let asked = Instant::now();
        let chunk = Duration::from(left.min(WAIT_CHUNK));
        match interrupt::abandonable(client.get_run(&run.id, Some(chunk))).await {
            Some(read) => run = read?,
            None => return Ok(run),
        }
        if run.status.is_terminal() || run.quarantine.is_some() {
            return Ok(run);
        }
        let pace = WAIT_PACE
            .saturating_sub(asked.elapsed())
            .min(deadline.saturating_duration_since(Instant::now()));
        if interrupt::abandonable(tokio::time::sleep(pace))
            .await
            .is_none()
        {
            return Ok(run);
        }
    }
}

fn started(console: &mut Console, run: &Run) {
    if !console.json() && !run.status.is_terminal() {
        console.note(format!("started run {}", run.id));
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
    use super::*;

    #[test]
    fn long_waits_are_split() {
        assert_eq!(first_wait(Duration::from_millis(0)), None);
        assert_eq!(
            first_wait(Duration::from_millis(30_000)),
            Some(Duration::from_millis(30_000))
        );
        assert_eq!(
            first_wait(Duration::from_millis(600_000)),
            Some(Duration::from_millis(50_000))
        );
    }
}
