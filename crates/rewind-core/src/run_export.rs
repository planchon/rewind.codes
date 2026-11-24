//! A run's history as one self-contained document (`archi/02-history.md`, "Stored format v1"):
//! the run's id, its code and action set, and its history. It is what a run export, the
//! archive of a finished run and the epoch corpus hold, so that each replays on its own: the
//! code reads `run.id`, and replay needs the exact code and action set the run started with.

use crate::action_set::ActionSet;
use crate::artifacts::Code;
use crate::digest::Digest;
use crate::event::HistoryEntry;
use crate::history::{HistoryError, validate_history};
use crate::ids::RunId;

/// A run's history with everything needed to replay it.
#[derive(Clone, Debug, PartialEq)]
pub struct RunExport {
    pub run: RunId,
    /// The code the run started with: its digest is `run_started.code`.
    pub code: Code,
    /// The pinned action set the run started with: its digest is `run_started.action_set`.
    pub action_set: ActionSet,
    pub entries: Vec<HistoryEntry>,
}

/// A run export whose parts do not belong together.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunExportError {
    #[error("invalid history: {0}")]
    History(#[from] HistoryError),
    #[error("the code's digest is {actual}, but run_started records {recorded}")]
    CodeMismatch { recorded: Digest, actual: Digest },
    #[error("the action set's digest is {actual}, but run_started records {recorded}")]
    ActionSetMismatch { recorded: Digest, actual: Digest },
}

impl RunExport {
    /// Checks that the history is well formed and that the code and the action set are the ones
    /// its `run_started` records.
    pub fn check(&self) -> Result<(), RunExportError> {
        let started = validate_history(&self.entries)?;
        let code = self.code.digest();
        if code != started.code {
            return Err(RunExportError::CodeMismatch {
                recorded: started.code,
                actual: code,
            });
        }
        let action_set = self.action_set.digest();
        if action_set != started.action_set {
            return Err(RunExportError::ActionSetMismatch {
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

    fn document(code: &Code, action_set: &ActionSet) -> RunExport {
        RunExport {
            run: RunId::from_u128(7),
            code: code.clone(),
            action_set: action_set.clone(),
            entries: vec![HistoryEntry {
                index: 0,
                recorded_at: Timestamp(1_000),
                event: Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
                    code: code.digest(),
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
        let code = Code::new("export default () => 1");
        let action_set = ActionSet::from_harness_actions(["a"]).unwrap();
        assert_eq!(document(&code, &action_set).check(), Ok(()));

        let mut other_code = document(&code, &action_set);
        other_code.code = Code::new("export default () => 2");
        assert!(matches!(
            other_code.check(),
            Err(RunExportError::CodeMismatch { .. })
        ));

        let mut other_action_set = document(&code, &action_set);
        other_action_set.action_set = ActionSet::empty();
        assert!(matches!(
            other_action_set.check(),
            Err(RunExportError::ActionSetMismatch { .. })
        ));

        let mut empty = document(&code, &action_set);
        empty.entries.clear();
        assert_eq!(
            empty.check(),
            Err(RunExportError::History(HistoryError::Empty))
        );
    }
}
