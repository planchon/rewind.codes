//! The exit codes of `rewind` (`archi/07-code-architecture.md`, "Embedded server and CLI").

use rewind_api::common::RunStatus;
use rewind_api::runs::Run;

/// How a command ended. A command that waits for a run exits with what became of the run; the
/// others exit with [`Exit::Success`] when they did what they were asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The run completed, or the command succeeded.
    Success,
    /// The run failed. Also: `rewind test` found an expectation that does not hold, `rewind replay`
    /// found that the code does not reproduce the history.
    Failed,
    /// The run was cancelled.
    Cancelled,
    /// The run was aborted by the host.
    Aborted,
    /// The run is still waiting: `--wait` elapsed, or it waits for the outside world.
    Waiting,
    /// The run is quarantined.
    Quarantined,
    /// Refused: unknown run, a signal or a cancellation to a run that has ended, a run that is
    /// not quarantined.
    Refused,
    /// Usage error: arguments, or a file the command cannot read.
    Usage,
    /// The code was refused at start, or `rewind check` found errors.
    CodeRefused,
    /// Internal error: the server is unreachable or failed.
    Internal,
    /// Interrupted by a second Ctrl-C, or by a first one before a call that changes something
    /// was sent.
    Interrupted,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Exit::Success => 0,
            Exit::Failed => 1,
            Exit::Cancelled => 2,
            Exit::Aborted => 3,
            Exit::Waiting => 4,
            Exit::Quarantined => 5,
            Exit::Refused => 6,
            Exit::Usage => 64,
            Exit::CodeRefused => 65,
            Exit::Internal => 70,
            Exit::Interrupted => 130,
        }
    }

    /// What became of a run: its status, or its quarantine, which is not a status.
    pub fn of_run(run: &Run) -> Exit {
        if run.quarantine.is_some() && !run.status.is_ended() {
            return Exit::Quarantined;
        }
        Exit::of_status(run.status)
    }

    pub fn of_status(status: RunStatus) -> Exit {
        match status {
            RunStatus::Completed => Exit::Success,
            RunStatus::Failed => Exit::Failed,
            RunStatus::Cancelled => Exit::Cancelled,
            RunStatus::Aborted => Exit::Aborted,
            RunStatus::Queued | RunStatus::Running | RunStatus::Waiting => Exit::Waiting,
        }
    }
}

impl From<Exit> for std::process::ExitCode {
    fn from(exit: Exit) -> Self {
        std::process::ExitCode::from(exit.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_outcome_has_its_own_code() {
        let all = [
            Exit::Success,
            Exit::Failed,
            Exit::Cancelled,
            Exit::Aborted,
            Exit::Waiting,
            Exit::Quarantined,
            Exit::Refused,
            Exit::Usage,
            Exit::CodeRefused,
            Exit::Internal,
            Exit::Interrupted,
        ];
        let codes: Vec<u8> = all.iter().map(|exit| exit.code()).collect();
        assert_eq!(codes, [0, 1, 2, 3, 4, 5, 6, 64, 65, 70, 130]);
    }

    #[test]
    fn a_run_exits_with_its_status() {
        assert_eq!(Exit::of_status(RunStatus::Completed), Exit::Success);
        assert_eq!(Exit::of_status(RunStatus::Failed), Exit::Failed);
        assert_eq!(Exit::of_status(RunStatus::Cancelled), Exit::Cancelled);
        assert_eq!(Exit::of_status(RunStatus::Aborted), Exit::Aborted);
        assert_eq!(Exit::of_status(RunStatus::Waiting), Exit::Waiting);
        assert_eq!(Exit::of_status(RunStatus::Queued), Exit::Waiting);
    }
}
