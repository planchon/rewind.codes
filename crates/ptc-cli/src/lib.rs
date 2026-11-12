//! The client commands of the `ptc` binary (`archi/07-code-architecture.md`, "Embedded server
//! and CLI").
//!
//! The CLI is a client of the API `/v1`: every command about runs goes through `ptc-api`'s
//! client, to the remote server of `--url` (`PTC_URL`, with `PTC_API_KEY`), or to the embedded
//! server of the state directory (`.ptc/`, or `--dir`), which [`embedded::Launcher`] starts on
//! demand as `ptc server --embedded`. The CLI never opens `journal.db`.
//!
//! Three commands run in this process, without a server, wiring the adapters themselves:
//! `ptc check` (the runtime's checks at start, tsgo and the checker's rules), `ptc test` (a
//! scenario in mock mode, with in-memory adapters and virtual time) and `ptc replay` of an
//! exported journal (a run's journal is also replayed here, once the server exported it).
//!
//! Every command takes `--json`, and exits with one code per outcome ([`Exit`]). A first Ctrl-C
//! lets the command finish the call in flight; a second one exits at once with 130.
//!
//! The `ptc` binary hands `ptc server …` to `ptc-server` and every other command to [`main`].

#![forbid(unsafe_code)]

mod args;
mod commands;
pub mod embedded;
mod error;
mod exit;
mod input;
mod interrupt;
mod output;
mod render;
mod toolset;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;
use clap::error::ErrorKind;

pub use error::CliError;
pub use exit::Exit;
pub use output::{Captured, Console};

/// The `ptc` command line, but `ptc server`. `args` starts with the program's name, as
/// `std::env::args_os()` does.
///
/// The runtime needs `TZ=UTC` from the start of the process: without it, the process runs
/// itself again with it. Then the command runs on a Tokio runtime of its own, with the
/// `tracing` subscriber of `ptc-otel` (logs on stderr at `warn`, or as `RUST_LOG` says; service
/// telemetry over OTLP when `OTEL_EXPORTER_OTLP_ENDPOINT` is set) and Ctrl-C handled.
pub fn main<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if let Err(error) = ensure_utc(&args) {
        eprintln!("error: cannot run with TZ=UTC: {error}");
        return Exit::Internal.into();
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: cannot start the async runtime: {error}");
            return Exit::Internal.into();
        }
    };
    let exit = runtime.block_on(async {
        let telemetry = telemetry();
        interrupt::install();
        let mut console = Console::stdio();
        let exit = run(args, &mut console).await;
        (exit, telemetry)
    });
    let (exit, telemetry) = exit;
    // Exports what is buffered, on the runtime still alive.
    drop(telemetry);
    exit.into()
}

/// Runs one command line, writing to `console`. `args` starts with the program's name.
pub async fn run<I, T>(args: I, console: &mut Console) -> Exit
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match args::Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let exit = match error.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => Exit::Success,
                ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => Exit::Usage,
                _ => Exit::Usage,
            };
            let text = error.render().to_string();
            if exit == Exit::Success {
                console.text(&text);
            } else {
                console.note(text.trim_end());
            }
            console.flush();
            return exit;
        }
    };
    console.set_json(cli.global.json);
    let exit = match commands::execute(cli.command, &cli.global, console).await {
        Ok(exit) => exit,
        Err(error) => {
            console.error(&error);
            error.exit()
        }
    };
    console.flush();
    exit
}

/// Runs the process again with `TZ=UTC` unless it has it: the sandbox refuses another time
/// zone, and only the start of a process can set it safely.
fn ensure_utc(args: &[OsString]) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;

    if std::env::var_os("TZ").is_some_and(|tz| tz == "UTC") {
        return Ok(());
    }
    let program = std::env::current_exe()?;
    // Only returns if the process could not be replaced.
    Err(std::process::Command::new(program)
        .args(args.iter().skip(1))
        .env("TZ", "UTC")
        .exec())
}

/// The process's `tracing` subscriber. A CLI is quiet: warnings and errors only, unless
/// `RUST_LOG` says otherwise.
fn telemetry() -> Option<ptc_otel::TelemetryGuard> {
    let filter = std::env::var("RUST_LOG")
        .ok()
        .filter(|directives| !directives.trim().is_empty())
        .unwrap_or_else(|| "warn".into());
    let otlp = [
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some())
    .then(ptc_otel::OtlpConfig::default);
    let config = ptc_otel::SubscriberConfig {
        filter: Some(filter),
        stderr: true,
        otlp,
        service: ptc_otel::ServiceConfig {
            version: Some(embedded::VERSION.to_owned()),
            ..ptc_otel::ServiceConfig::default()
        },
        ..ptc_otel::SubscriberConfig::default()
    };
    match ptc_otel::install(&config) {
        Ok(guard) => Some(guard),
        Err(error) => {
            eprintln!("warning: telemetry is off: {error}");
            None
        }
    }
}
