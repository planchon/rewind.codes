//! Replays a history of the corpus by hand:
//!
//! ```text
//! cargo run -p rewind-e2e -- replay epoch-1/blue/fan_out [--code other.ts]
//! ```
//!
//! The committed `history.v1.json` is decoded and replayed with the code it holds (or
//! another one, to check that it is compatible with the history), on a fresh V8 executor.

use std::process::ExitCode;

use rewind_core::Code;
use rewind_e2e::{Case, Replayed};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (name, code) = match args.as_slice() {
        [command, name] if command == "replay" => (name, None),
        [command, name, flag, code] if command == "replay" && flag == "--code" => {
            (name, Some(code))
        }
        _ => {
            eprintln!("usage: rewind-e2e replay <epoch-N/blue|red/case> [--code <file.ts>]");
            return ExitCode::from(2);
        }
    };
    match replay(name, code.map(String::as_str)).await {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

async fn replay(name: &str, code: Option<&str>) -> Result<String, String> {
    let case = Case::load(name)?;
    let export = std::fs::read_to_string(case.history_path()).map_err(|err| err.to_string())?;
    let export = rewind_format::decode_run_export(&export).map_err(|err| err.to_string())?;
    let code = match code {
        Some(path) => {
            Code::new(std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?)
        }
        None => export.code.clone(),
    };
    match rewind_e2e::replay_export(&case, &export, &code).await {
        Replayed::Matches(end) => Ok(format!(
            "{name}: the code replays the {} entries of the history ({end:?})",
            export.entries.len()
        )),
        Replayed::Fails(message) => Err(format!("{name}: {message}")),
    }
}
