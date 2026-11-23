//! The client commands of the `rewind` binary (`archi/07-code-architecture.md`, "Embedded server
//! and CLI").
//!
//! The CLI is a client of the API `/v1`: every command about runs goes through `rewind-api`'s
//! client, to the remote server of `--url` (`REWIND_URL`, with `REWIND_API_KEY`), or to the embedded
//! server of the state directory (`.rewind/`, or `--dir`), which [`embedded::Launcher`] starts on
//! demand as `rewind server --embedded`. The CLI never opens what `.rewind/` stores.
//!
//! Three commands run in this process, without a server, wiring the adapters themselves:
//! `rewind check` (the runtime's checks at start, tsgo and the checker's rules), `rewind test` (a
//! scenario in mock mode, with in-memory adapters and virtual time) and `rewind replay` of an
//! exported journal (a run's journal is also replayed here, once the server exported it).
//!
//! Every command takes `--json`, and exits with one code per outcome ([`Exit`]). A first Ctrl-C
//! lets the command finish the call in flight; a second one exits at once with 130.
//!
//! The `rewind` binary hands `rewind server …` to `rewind-server` and every other command to [`main`],
//! once it runs with `TZ=UTC`.

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

/// `rewind <command> …`, but `rewind server`: `args` are the arguments after `rewind`. The `rewind` binary
/// calls it once it runs with `TZ=UTC`, which the sandbox requires.
///
/// The command runs on a Tokio runtime of its own, with the `tracing` subscriber of `rewind-otel`
/// (logs on stderr at `warn`, or as `RUST_LOG` says; service telemetry over OTLP when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set) and Ctrl-C handled.
pub fn main(args: Vec<OsString>) -> ExitCode {
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
    let (exit, telemetry) = runtime.block_on(async {
        let telemetry = telemetry();
        interrupt::install();
        let mut console = Console::stdio();
        let exit = run(args, &mut console).await;
        (exit, telemetry)
    });
    // Exports what is buffered, on the runtime still alive.
    drop(telemetry);
    exit.into()
}

/// Runs one command line, writing to `console`: `args` are the arguments after `rewind`.
pub async fn run<I, T>(args: I, console: &mut Console) -> Exit
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let argv = std::iter::once(OsString::from("rewind")).chain(args.into_iter().map(Into::into));
    let cli = match args::Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => {
            let exit = match error.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => Exit::Success,
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

/// The process's `tracing` subscriber. A CLI is quiet: warnings and errors only, unless
/// `RUST_LOG` says otherwise.
fn telemetry() -> Option<rewind_otel::TelemetryGuard> {
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
    .then(rewind_otel::OtlpConfig::default);
    let config = rewind_otel::SubscriberConfig {
        filter: Some(filter),
        stderr: true,
        otlp,
        service: rewind_otel::ServiceConfig {
            version: Some(embedded::VERSION.to_owned()),
            ..rewind_otel::ServiceConfig::default()
        },
        ..rewind_otel::SubscriberConfig::default()
    };
    match rewind_otel::install(&config) {
        Ok(guard) => Some(guard),
        Err(error) => {
            eprintln!("warning: telemetry is off: {error}");
            None
        }
    }
}
