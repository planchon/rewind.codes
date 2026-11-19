//! `rewind server`: the engine host of Rewind (`archi/07-code-architecture.md`, "Crates").
//!
//! - The HTTP API `/v1` of `rewind-api`, with axum: API keys hashed in the store and members'
//!   access tokens, grants, roles and label scopes, organizations, members and invitations,
//!   tenants and their quotas, `Idempotency-Key`, problem+json errors; the event stream
//!   of a run (server-sent events); host-tool tasks long-polled by the harness's worker; inbound
//!   webhooks that become signals; outbound lifecycle webhooks signed following Standard
//!   Webhooks, sent concurrently and bounded per subscription (`deliveries`), never to the
//!   private network of a deployed server.
//! - The dashboard's sign-in `/auth` on the api role, through WorkOS (`rewind-workos`), with its
//!   sealed cookies; a member's first grant in an organization.
//! - The worker role: activations with the sticky cache, tool dispatch, the scheduler of timers
//!   and deadlines, the projector, the retention jobs (`worker`).
//! - The embedded server of a `.rewind/` directory: its lock, its unix socket, its idle stop
//!   (`embedded`).
//! - The dashboard's build, embedded in the binary and served under `/`.
//!
//! The adapters are wired by configuration, each in its own module of `wiring`. The `rewind`
//! binary hands `rewind server …` to [`main`].

#![forbid(unsafe_code)]

mod app;
mod auth;
mod config;
mod connections;
mod deliveries;
mod embedded;
mod error;
mod extract;
mod grants;
mod http;
mod mcp;
mod oauth;
mod serve;
mod tokens;
mod views;
mod wake;
mod wiring;
mod worker;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;

pub use config::{ConfigError, OtlpArg, RoleArg, ServerArgs};
pub use connections::{McpCaller, McpOrganization, McpPrincipal};
pub use serve::{Running, StartupError, VERSION, start, start_in_memory, start_with_identity};

/// Exit codes of `rewind server`.
pub mod exit {
    /// The server stopped: a signal, `POST /v1/server/stop`, or the idle timeout.
    pub const STOPPED: u8 = 0;
    /// The command line is invalid.
    pub const USAGE: u8 = 64;
    /// The server cannot start: a store, a port, a file.
    pub const FAILED: u8 = 70;
    /// Another embedded server holds the directory's lock.
    pub const LOCKED: u8 = 75;
}

/// `rewind server …`: `args` are the arguments after `server`. Runs until a signal (SIGINT,
/// SIGTERM), `POST /v1/server/stop` or the idle timeout stops the server.
pub fn main(args: Vec<OsString>) -> ExitCode {
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
        let running = match start(args).await {
            Ok(running) => running,
            Err(error) => {
                eprintln!("rewind server: {error}");
                return ExitCode::from(match error {
                    StartupError::Config(_) => exit::USAGE,
                    StartupError::Locked(_) => exit::LOCKED,
                    StartupError::Io(_) | StartupError::Setup(_) => exit::FAILED,
                });
            }
        };
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
