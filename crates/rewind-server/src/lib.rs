//! `rewind server`: a node of Rewind's run plane and its HTTP edge, in one process
//! (`archi/07-code-architecture.md`, "Crates", "Inside a node").
//!
//! - The node: the owners of the partitions it holds (on SlateDB), the organization actors they
//!   host, its executors (V8) and its call executor, driven by `rewind-engine`'s runtime; its
//!   membership of the cluster on the control plane (Postgres, or Turso for a single node);
//!   `--executors-only` and `--pool`.
//! - The edge: the HTTP API `/v1` of `rewind-api` on the engine's `Edge`, with axum: API keys
//!   hashed in the control plane and members' access tokens, grants, roles and label scopes,
//!   organizations, members and invitations, tenants and their quotas, `Idempotency-Key`,
//!   problem+json errors; the event stream of a run, the change stream and the task stream
//!   (server-sent events pushed by the owners); held waits; inbound webhooks that become
//!   signals; `/mcp` and the OAuth authorization server of people's connections.
//! - The node link over HTTP/2 (`link`): its DTOs, the `NodeLink` that sends them to another
//!   node, and the listener that serves a node's side, authenticated by the cluster's secret.
//! - The dashboard's sign-in `/auth`, through WorkOS (`rewind-workos`), with its sealed
//!   cookies; a member's first grant in an organization.
//! - Outbound lifecycle webhooks, signed following Standard Webhooks, never to the private
//!   network of a deployed server: the sender of the owners' outboxes.
//! - The embedded server of a `.rewind/` directory: one node, one partition, its lock, its unix
//!   socket, its idle stop (`embedded`).
//! - The control plane's daily jobs.
//! - `rewind server migrate-runs`: the runs of the journal store moved onto the run plane, once
//!   (`migrate`).
//! - The dashboard's build, embedded in the binary and served under `/`.
//!
//! The adapters are wired by configuration, each in its own module of `wiring`. The `rewind`
//! binary hands `rewind server …` to [`main`].

#![forbid(unsafe_code)]

mod app;
mod auth;
mod config;
mod connections;
mod embedded;
mod error;
mod extract;
mod follow;
mod grants;
mod http;
mod idle;
pub mod link;
mod mcp;
mod migrate;
mod oauth;
mod serve;
mod streams;
mod titles;
mod tokens;
mod usage;
mod views;
mod wiring;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;

pub use config::{ConfigError, OtlpArg, ServerArgs};
pub use connections::{McpCaller, McpOrganization, McpPrincipal};
pub use migrate::{MigrateArgs, migrate_runs};
pub use serve::{
    Rewrapped, Running, StartupError, VERSION, rewrap_data_keys, start, start_in_memory,
    start_with_identity,
};

/// Exit codes of `rewind server`.
pub mod exit {
    /// The server stopped: a signal, `POST /v1/server/stop`, or the idle timeout.
    pub const STOPPED: u8 = 0;
    /// The command line is invalid.
    pub const USAGE: u8 = 64;
    /// The server cannot start: a store, a port, a file, a data key held under a master key it
    /// was not given. `--rewrap-data-keys` or `migrate-runs` that failed.
    pub const FAILED: u8 = 70;
    /// Another embedded server holds the directory's lock.
    pub const LOCKED: u8 = 75;
}

/// The exit code of a server that could not start.
pub fn exit_code(error: &StartupError) -> u8 {
    match error {
        StartupError::Config(_) => exit::USAGE,
        StartupError::Locked(_) => exit::LOCKED,
        StartupError::Io(_) | StartupError::Setup(_) => exit::FAILED,
    }
}

/// `rewind server …`: `args` are the arguments after `server`. Runs until a signal (SIGINT,
/// SIGTERM), `POST /v1/server/stop` or the idle timeout stops the server. With
/// `--rewrap-data-keys`, rewraps the data keys, prints how many it changed, and exits;
/// `rewind server migrate-runs …` moves the runs of the journal store onto the run plane, prints
/// what it moved, and exits ([`migrate_runs`]).
pub fn main(args: Vec<OsString>) -> ExitCode {
    if args.first().is_some_and(|first| first == "migrate-runs") {
        return migrate::main(args.into_iter().skip(1).collect());
    }
    let args = match ServerArgs::try_parse_from(
        std::iter::once(OsString::from("rewind server")).chain(args),
    ) {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(if error.use_stderr() { exit::USAGE } else { 0 });
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("rewind-server")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rewind server: cannot start the async runtime: {error}");
            return ExitCode::from(exit::FAILED);
        }
    };
    runtime.block_on(async move {
        let _telemetry = match wiring::telemetry::install(&args) {
            Ok(guard) => guard,
            Err(error) => {
                eprintln!("rewind server: telemetry: {error}");
                return ExitCode::from(exit::USAGE);
            }
        };
        if args.rewrap_data_keys {
            return match rewrap_data_keys(args).await {
                Ok(rewrapped) => {
                    println!(
                        "{} data key(s) encrypted again with the master key {}",
                        rewrapped.changed, rewrapped.master_key
                    );
                    ExitCode::from(exit::STOPPED)
                }
                Err(error) => {
                    eprintln!("rewind server: {error}");
                    ExitCode::from(exit_code(&error))
                }
            };
        }
        let notify_ready = args.notify_ready;
        let running = match start(args).await {
            Ok(running) => running,
            Err(error) => {
                eprintln!("rewind server: {error}");
                return ExitCode::from(exit_code(&error));
            }
        };
        if notify_ready && let Err(error) = embedded::notify_ready() {
            eprintln!("rewind server: cannot write `ready` on standard output: {error}");
            running.stop().await;
            return ExitCode::from(exit::FAILED);
        }
        tokio::select! {
            () = running.stopped() => {}
            () = signal() => tracing::info!("stopping: signal"),
        }
        running.stop().await;
        ExitCode::from(exit::STOPPED)
    })
}

/// SIGINT or SIGTERM.
async fn signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = interrupt => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = interrupt.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = interrupt.await;
    }
}
