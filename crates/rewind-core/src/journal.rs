//! A run's journal as one self-contained document (`archi/02-journal.md`, "Stored format v1"):
//! the run's id, its script and toolset, and its history. It is what a journal export, the
//! archive of a finished run and the epoch corpus hold, so that each replays on its own: the
//! script reads `run.id`, and replay needs the exact script and toolset the run started with.

use crate::artifacts::Script;
use crate::digest::Digest;
use crate::event::HistoryEntry;
use crate::history::{HistoryError, validate_history};
use crate::ids::RunId;
use crate::toolset::Toolset;

/// A run's journal with everything needed to replay it.
#[derive(Clone, Debug, PartialEq)]
pub struct JournalDocument {
    pub run: RunId,
    /// The script the run started with: its digest is `run_started.script`.
    pub script: Script,
    /// The pinned toolset the run started with: its digest is `run_started.toolset`.
    pub toolset: Toolset,
    pub entries: Vec<HistoryEntry>,
}

/// A journal document whose parts do not belong together.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JournalError {
    #[error("invalid history: {0}")]
    History(#[from] HistoryError),
    #[error("the script's digest is {actual}, but run_started records {recorded}")]
    ScriptMismatch { recorded: Digest, actual: Digest },
    #[error("the toolset's digest is {actual}, but run_started records {recorded}")]
    ToolsetMismatch { recorded: Digest, actual: Digest },
}

impl JournalDocument {
    /// Checks that the history is well formed and that the script and the toolset are the ones
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
        let toolset = self.toolset.digest();
        if toolset != started.toolset {
            return Err(JournalError::ToolsetMismatch {
                recorded: started.toolset,
                actual: toolset,
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

    fn document(script: &Script, toolset: &Toolset) -> JournalDocument {
        JournalDocument {
            run: RunId::from_u128(7),
            script: script.clone(),
            toolset: toolset.clone(),
            entries: vec![HistoryEntry {
                index: 0,
                recorded_at: Timestamp(1_000),
                event: Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
                    script: script.digest(),
                    toolset: toolset.digest(),
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
    fn a_document_holds_the_script_and_toolset_its_run_started_with() {
        let script = Script::new("export default () => 1");
        let toolset = Toolset::from_host_tools(["a"]).unwrap();
        assert_eq!(document(&script, &toolset).check(), Ok(()));

        let mut other_script = document(&script, &toolset);
        other_script.script = Script::new("export default () => 2");
        assert!(matches!(
            other_script.check(),
            Err(JournalError::ScriptMismatch { .. })
        ));

        let mut other_toolset = document(&script, &toolset);
        other_toolset.toolset = Toolset::empty();
        assert!(matches!(
            other_toolset.check(),
            Err(JournalError::ToolsetMismatch { .. })
        ));

        let mut empty = document(&script, &toolset);
        empty.entries.clear();
        assert_eq!(
            empty.check(),
            Err(JournalError::History(HistoryError::Empty))
        );
    }
}
