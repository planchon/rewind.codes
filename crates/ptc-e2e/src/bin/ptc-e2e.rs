//! Replays a journal of the corpus by hand:
//!
//! ```text
//! cargo run -p ptc-e2e -- replay epoch-1/blue/fan_out [--script other.ts]
//! ```
//!
//! The committed `journal.v1.json` is decoded and replayed with the script it holds (or
//! another one, to check that it is compatible with the journal), on a fresh V8 executor.

use std::process::ExitCode;

use ptc_core::Script;
use ptc_e2e::{Case, Replayed};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (name, script) = match args.as_slice() {
        [command, name] if command == "replay" => (name, None),
        [command, name, flag, script] if command == "replay" && flag == "--script" => {
            (name, Some(script))
        }
        _ => {
            eprintln!("usage: ptc-e2e replay <epoch-N/blue|red/case> [--script <file.ts>]");
            return ExitCode::from(2);
        }
    };
    match replay(name, script.map(String::as_str)).await {
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

async fn replay(name: &str, script: Option<&str>) -> Result<String, String> {
    let case = Case::load(name)?;
    let journal = std::fs::read_to_string(case.journal_path()).map_err(|err| err.to_string())?;
    let journal = ptc_format::decode_journal(&journal).map_err(|err| err.to_string())?;
    let script = match script {
        Some(path) => {
            Script::new(std::fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?)
        }
        None => journal.script.clone(),
    };
    match ptc_e2e::replay_journal(&case, &journal, &script).await {
        Replayed::Matches(end) => Ok(format!(
            "{name}: the script replays the {} entries of the journal ({end:?})",
            journal.entries.len()
        )),
        Replayed::Fails(message) => Err(format!("{name}: {message}")),
    }
}
