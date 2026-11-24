//! `rewind-load`: the load generator of Rewind (`deploy/README.md`, "Load tests").
//!
//! ```text
//! rewind-load run --url http://localhost:8080 --admin-key rewind_sk_… [--runs 500] [--concurrency 100]
//! rewind-load mcp --listen 127.0.0.1:9100
//! ```
//!
//! `run` drives a deployment through its HTTP API: it registers an action set whose MCP server is
//! its own fake one (served by this process, which the deployment must reach) and whose host
//! action it answers itself, starts many runs at once, signals them, waits for every one to end,
//! then checks the invariants and prints throughput, latencies and errors. It exits with 0 when
//! every run completed as expected, no invariant broke and the thresholds given were met, 1
//! otherwise, and 3 when the load could not be set up.

#![forbid(unsafe_code)]

mod error;
mod load;
mod mcp;
mod report;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use rewind_api::Client;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::mcp::FakeServer;

#[derive(Debug, Parser)]
#[command(name = "rewind-load", version, about = "Load generator of Rewind")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Drives a deployment with many concurrent runs, then checks them and reports.
    Run(Box<RunArgs>),
    /// Serves only the fake MCP server, until interrupted.
    Mcp(McpArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    /// The deployment's API.
    #[arg(long, env = "REWIND_URL", default_value = "http://localhost:8080")]
    url: String,

    /// An API key with the role `operator`: the load makes its own keys with it.
    #[arg(long, env = "REWIND_ADMIN_KEY", hide_env_values = true)]
    admin_key: String,

    /// The tenant of the load's runs.
    #[arg(long, default_value = "load")]
    tenant: String,

    /// Runs to start.
    #[arg(long, default_value_t = 500)]
    runs: usize,

    /// Runs in flight at once.
    #[arg(long, default_value_t = 100)]
    concurrency: usize,

    /// Workers answering the host tool, each on a task stream of 16 slots.
    #[arg(long, default_value_t = 8)]
    host_workers: usize,

    /// The durable sleep of each run.
    #[arg(long, default_value = "1s")]
    sleep: String,

    /// A padding of this many bytes in each run's input, sent to and returned by `fake.echo`:
    /// above 1 KiB the payload is stored apart from its event, above 64 KiB in S3.
    #[arg(long, default_value_t = 0)]
    payload_bytes: usize,

    /// Where the fake MCP server listens.
    #[arg(long, env = "REWIND_LOAD_MCP_LISTEN", default_value = "127.0.0.1:9100")]
    mcp_listen: SocketAddr,

    /// The URL the deployment reaches the fake MCP server at; `http://<listen>/mcp` by default.
    #[arg(long, env = "REWIND_LOAD_MCP_URL")]
    mcp_url: Option<String>,

    /// The whole load gives up after this long.
    #[arg(long, default_value = "15m", value_parser = parse_duration)]
    timeout: Duration,

    /// Also writes the report as JSON to this file.
    #[arg(long)]
    json: Option<PathBuf>,

    /// Fail below this throughput, in runs per second.
    #[arg(long)]
    min_runs_per_s: Option<f64>,

    /// Fail when the 99th percentile of the end-to-end latency is above this.
    #[arg(long, value_parser = parse_duration)]
    max_p99_end_to_end: Option<Duration>,

    /// Fail when the 99th percentile of the activation latency is above this.
    #[arg(long, value_parser = parse_duration)]
    max_p99_activation: Option<Duration>,
}

#[derive(Debug, Args)]
struct McpArgs {
    #[arg(long, env = "REWIND_LOAD_MCP_LISTEN", default_value = "127.0.0.1:9100")]
    listen: SocketAddr,
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    text.parse::<rewind_api::common::Duration>()
        .map(Into::into)
        .map_err(|error| error.to_string())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rewind-load: cannot start: {error}");
            return ExitCode::from(3);
        }
    };
    let result = runtime.block_on(async {
        match cli.command {
            Command::Run(args) => run(*args).await,
            Command::Mcp(args) => mcp(args).await,
        }
    });
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("rewind-load: {error}");
            ExitCode::from(3)
        }
    }
}

/// Stops at the first Ctrl-C or SIGTERM.
fn interrupt() -> CancellationToken {
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let Ok(mut term) = signal(SignalKind::terminate()) else {
                return;
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        cancel.cancel();
    });
    token
}

async fn run(args: RunArgs) -> Result<bool, Error> {
    let mcp_url = args
        .mcp_url
        .clone()
        .unwrap_or_else(|| format!("http://{}/mcp", args.mcp_listen));
    let parsed = url::Url::parse(&mcp_url)
        .map_err(|error| Error::new(format!("--mcp-url {mcp_url:?}: {error}")))?;
    let stop = interrupt();
    let server = FakeServer::default();
    let serving = CancellationToken::new();
    let address = mcp::serve(server.clone(), args.mcp_listen, &parsed, serving.clone()).await?;
    eprintln!("rewind-load: fake MCP server on {address}, reached at {mcp_url}");

    let admin = Client::remote(&args.url, Some(&args.admin_key))?;
    let settings = load::Settings {
        runs: args.runs,
        concurrency: args.concurrency,
        host_workers: args.host_workers,
        sleep: args.sleep.clone(),
        payload_bytes: args.payload_bytes,
        mcp_url,
        tenant: args.tenant.clone(),
        timeout: args.timeout,
        thresholds: load::Thresholds {
            min_runs_per_s: args.min_runs_per_s,
            max_p99_end_to_end: args.max_p99_end_to_end,
            max_p99_activation: args.max_p99_activation,
        },
    };
    eprintln!(
        "rewind-load: {} runs, {} at once, against {}",
        settings.runs, settings.concurrency, args.url
    );
    let report = load::run(&admin, &args.url, &server, &settings, stop).await;
    serving.cancel();
    let report = report?;
    println!("{report}");
    if let Some(path) = &args.json {
        let json =
            serde_json::to_string_pretty(&report).map_err(|error| Error::new(error.to_string()))?;
        std::fs::write(path, json)
            .map_err(|error| Error::new(format!("cannot write {}: {error}", path.display())))?;
    }
    Ok(report.passed())
}

async fn mcp(args: McpArgs) -> Result<bool, Error> {
    let url = url::Url::parse(&format!("http://{}/mcp", args.listen))
        .map_err(|error| Error::new(error.to_string()))?;
    let stop = interrupt();
    let address = mcp::serve(FakeServer::default(), args.listen, &url, stop.clone()).await?;
    eprintln!("rewind-load: fake MCP server on http://{address}/mcp");
    stop.cancelled().await;
    Ok(true)
}
