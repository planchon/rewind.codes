//! The `rewind` binary (`archi/07-code-architecture.md`, "Crates"): `rewind server …` goes to
//! `rewind-server`, every other command to `rewind-cli`. Nothing else happens here.
//!
//! The sandbox requires `TZ=UTC` (`archi/01-product.md`, "What exists in the sandbox"): a
//! process started with another time zone starts itself again with `TZ=UTC`, before any thread
//! exists.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    if std::env::var_os("TZ").as_deref() != Some("UTC".as_ref()) {
        return restart_in_utc();
    }
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|command| command == "server") {
        args.remove(0);
        rewind_server::main(args)
    } else {
        rewind_cli::main(args)
    }
}

/// Replaces this process by itself with `TZ=UTC`.
#[cfg(unix)]
fn restart_in_utc() -> ExitCode {
    use std::os::unix::process::CommandExt;

    let error = match std::env::current_exe() {
        Ok(exe) => std::process::Command::new(exe)
            .args(std::env::args_os().skip(1))
            .env("TZ", "UTC")
            .exec(),
        Err(error) => error,
    };
    eprintln!("rewind: cannot start itself again with TZ=UTC: {error}");
    ExitCode::from(70)
}

#[cfg(not(unix))]
fn restart_in_utc() -> ExitCode {
    eprintln!("rewind: set TZ=UTC: the sandbox requires it");
    ExitCode::from(64)
}
