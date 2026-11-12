//! The client commands of the `ptc` binary (`archi/07-code-architecture.md`, "Embedded server
//! and CLI"). The binary hands every command but `ptc server` to [`main`].
//!
//! No command is implemented in this build yet: each answers with the usage error code (64) and
//! says so.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::process::ExitCode;

/// The exit code of a usage error (`archi/07-code-architecture.md`, "Exit codes").
pub const USAGE: u8 = 64;

/// `ptc <command> …`: `args` are the arguments after `ptc`.
pub fn main(args: Vec<OsString>) -> ExitCode {
    let command = args
        .first()
        .map(|command| command.to_string_lossy().into_owned());
    match command.as_deref() {
        None | Some("-h" | "--help" | "help") => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Some("-V" | "--version") => {
            println!("ptc {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some(command) => {
            eprintln!(
                "ptc {command}: this command is not available in this build of ptc. \
                 Available: ptc server (see `ptc server --help`)."
            );
            ExitCode::from(USAGE)
        }
    }
}

fn usage() -> &'static str {
    "ptc: programmatic tool calling, durable and replayable.\n\n\
     Usage: ptc <command> [options]\n\n\
     Commands:\n  \
     server   Runs the server in the foreground (`ptc server --help`)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_command_is_a_usage_error() {
        assert_eq!(main(vec!["run".into()]), ExitCode::from(USAGE));
        assert_eq!(main(Vec::new()), ExitCode::SUCCESS);
    }
}
