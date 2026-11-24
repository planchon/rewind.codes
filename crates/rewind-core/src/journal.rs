//! A run's journal as one self-contained document (`archi/02-journal.md`, "Stored format v1"):
//! the run's id, its script and action set, and its history. It is what a journal export, the
//! archive of a finished run and the epoch corpus hold, so that each replays on its own: the
//! script reads `run.id`, and replay needs the exact script and action set the run started with.

use crate::action_set::ActionSet;
use crate::artifacts::Script;
use crate::digest::Digest;
use crate::event::HistoryEntry;
use crate::history::{HistoryError, validate_history};
use crate::ids::RunId;

/// A run's journal with everything needed to replay it.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalDocument {
    pub run: RunId,
    /// The script the run started with: its digest is `run_started.script`.
    pub script: Script,
    /// The pinned action set the run started with: its digest is `run_started.action_set`.
    pub action_set: ActionSet,
    pub entries: Vec<HistoryEntry>,
}

/// A journal document whose parts do not belong together.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    #[error("invalid history: {0}")]
    History(#[from] HistoryError),
    #[error("the script's digest is {actual}, but run_started records {recorded}")]
    ScriptMismatch { recorded: Digest, actual: Digest },
    #[error("the action set's digest is {actual}, but run_started records {recorded}")]
    ActionSetMismatch { recorded: Digest, actual: Digest },
}

impl JournalDocument {
    /// Checks that the history is well formed and that the script and the action set are the ones
    /// its `run_started` records.
    pub fn check(&self) -> Result<(), JournalError> {
        let started = validate_history(&self.entries)?;
        let script = self.script.digest();
        if script != started.script {
            return Err(JournalError::ScriptMismatch {
                recorded: started.script,
                actual: script,
            });
        }
        let action_set = self.action_set.digest();
        if action_set != started.action_set {
            return Err(JournalError::ActionSetMismatch {
                recorded: started.action_set,
                actual: action_set,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::event::{Event, Lifecycle, RunMode, RunStarted};
    use crate::ids::{Epoch, Seed};
    use crate::time::Timestamp;

    fn document(script: &Script, action_set: &ActionSet) -> JournalDocument {
        JournalDocument {
            run: RunId::from_u128(7),
            script: script.clone(),
            action_set: action_set.clone(),
            entries: vec![HistoryEntry {
                index: 0,
                recorded_at: Timestamp(1_000),
                event: Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
                    script: script.digest(),
                    action_set: action_set.digest(),
                    input: json!({}).into(),
                    seed: Seed::from_bytes([1; 32]),
                    epoch: Epoch(1),
                    expires_at: Timestamp(2_000),
                    mode: RunMode::Live,
                })),
            }],
        }
    }

    #[test]
    fn a_document_holds_the_script_and_action_set_its_run_started_with() {
        let script = Script::new("export default () => 1");
        let action_set = ActionSet::from_harness_actions(["a"]).unwrap();
        assert_eq!(document(&script, &action_set).check(), Ok(()));

        let mut other_script = document(&script, &action_set);
        other_script.script = Script::new("export default () => 2");
        assert!(matches!(
            other_script.check(),
            Err(JournalError::ScriptMismatch { .. })
        ));

        let mut other_action_set = document(&script, &action_set);
        other_action_set.action_set = ActionSet::empty();
        assert!(matches!(
            other_action_set.check(),
            Err(JournalError::ActionSetMismatch { .. })
        ));

        let mut empty = document(&script, &action_set);
        empty.entries.clear();
        assert_eq!(
            empty.check(),
            Err(JournalError::History(HistoryError::Empty))
        );
    }
}
