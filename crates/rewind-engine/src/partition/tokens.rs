//! The tokens of call tasks and webhooks (`archi/08-decisions.md`, "Taken while writing the
//! object-storage spec": a task's token and a webhook token name their run).
//!
//! A token is `rewind_tk_{run}_{secret}` (a task's hand-out) or `rewind_wh_{run}_{secret}` (a
//! webhook), the secret 256 bits from the operating system's CSPRNG in hex: any process routes
//! it to the owner of its run from the run's id alone, and the owner checks it against the
//! SHA-256 it keeps.

use rewind_core::{Digest, RunId};

use crate::ports::TaskToken;

const TASK: &str = "rewind_tk_";
const WEBHOOK: &str = "rewind_wh_";

fn secret() -> String {
    let mut bytes = [0u8; 32];
    // Without a CSPRNG, tokens would be guessable, and a token is a capability.
    getrandom::fill(&mut bytes).expect("the operating system's random generator is available");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A new token for a hand-out of a task of `run`.
pub(crate) fn task_token(run: RunId) -> TaskToken {
    TaskToken(format!("{TASK}{run}_{}", secret()))
}

/// A new webhook token of `run`.
pub(crate) fn webhook_token(run: RunId) -> String {
    format!("{WEBHOOK}{run}_{}", secret())
}

/// The run a token names, if it is a token of the run plane.
pub(crate) fn run_of(token: &str) -> Option<RunId> {
    let rest = token
        .strip_prefix(TASK)
        .or_else(|| token.strip_prefix(WEBHOOK))?;
    let (run, _) = rest.split_once('_')?;
    run.parse().ok()
}

/// What the owner keeps of a token: its SHA-256.
pub(crate) fn hash(token: &str) -> Digest {
    Digest::of(token.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_names_its_run_and_is_unguessable() {
        let run = RunId::from_u128(0x1234);
        let token = task_token(run);
        assert_eq!(run_of(&token.0), Some(run));
        assert_ne!(token, task_token(run));
        let webhook = webhook_token(run);
        assert_eq!(run_of(&webhook), Some(run));
        assert_eq!(run_of("whk_8f3a"), None);
        assert_eq!(run_of("rewind_tk_nope_00"), None);
    }
}
