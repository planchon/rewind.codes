//! The error type of `rewind-cli`, and the exit code of each error.

use std::path::{Path, PathBuf};

use rewind_api::problem::{Problem, ProblemType};
use rewind_api::scripts::Diagnostic;

use crate::exit::Exit;

/// Everything that stops a command before it has an outcome of its own.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The arguments, or a file they name, cannot be used.
    #[error("{0}")]
    Usage(String),
    /// A file cannot be read or written.
    #[error("{}: {source}", path.display())]
    File {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The server answered with an error, or could not be reached.
    #[error(transparent)]
    Api(#[from] rewind_api::Error),
    /// The script cannot start: it does not compile, or the start was refused before any run
    /// existed.
    #[error("{message}")]
    ScriptRefused {
        message: String,
        diagnostics: Vec<Diagnostic>,
    },
    /// The request was refused because of the state of things (a local command's equivalent of
    /// a 404 or a 409).
    #[error("{0}")]
    Refused(String),
    /// The embedded server could not be started or reached.
    #[error("{0}")]
    Server(String),
    /// A fault of the CLI or of an adapter it runs in process.
    #[error("{0}")]
    Internal(String),
    /// A first Ctrl-C came before a call that changes something: the command did not send it.
    #[error("interrupted: {0}")]
    Interrupted(String),
}

impl CliError {
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        CliError::Usage(message.into())
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        CliError::Internal(message.into())
    }

    pub(crate) fn file(path: &Path, source: std::io::Error) -> Self {
        CliError::File {
            path: path.to_owned(),
            source,
        }
    }

    /// The exit code of the error.
    pub fn exit(&self) -> Exit {
        match self {
            CliError::Usage(_) | CliError::File { .. } => Exit::Usage,
            CliError::ScriptRefused { .. } => Exit::ScriptRefused,
            CliError::Refused(_) => Exit::Refused,
            CliError::Server(_) | CliError::Internal(_) => Exit::Internal,
            CliError::Interrupted(_) => Exit::Interrupted,
            CliError::Api(error) => api_exit(error),
        }
    }

    /// The error as `application/problem+json`, as `--json` prints it.
    pub fn problem(&self) -> Problem {
        match self {
            CliError::Api(rewind_api::Error::Problem(problem)) => problem.as_ref().clone(),
            CliError::ScriptRefused {
                message,
                diagnostics,
            } => Problem::new(ProblemType::InvalidScript, message.clone())
                .with_diagnostics(diagnostics.clone()),
            other => {
                let kind = match other.exit() {
                    Exit::Usage => ProblemType::InvalidRequest,
                    Exit::Refused => ProblemType::NotFound,
                    Exit::Interrupted => ProblemType::Unavailable,
                    _ => ProblemType::Internal,
                };
                Problem::new(kind, other.to_string())
            }
        }
    }

    /// The diagnostics of a refused script, from the server or from the local checks.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        match self {
            CliError::Api(rewind_api::Error::Problem(problem)) => &problem.diagnostics,
            CliError::ScriptRefused { diagnostics, .. } => diagnostics,
            _ => &[],
        }
    }
}

/// The exit code of an error of the API: a problem the server answered with, by its kind.
fn api_exit(error: &rewind_api::Error) -> Exit {
    match error {
        rewind_api::Error::Problem(problem) => match problem.kind() {
            Some(
                ProblemType::NotFound
                | ProblemType::RunEnded
                | ProblemType::NotQuarantined
                | ProblemType::CallNotWaiting
                | ProblemType::RoleInUse
                | ProblemType::LastOwner
                | ProblemType::NamespaceDeleting
                | ProblemType::CallGone
                | ProblemType::QuotaExceeded
                | ProblemType::Unauthorized
                | ProblemType::Forbidden,
            ) => Exit::Refused,
            Some(
                ProblemType::InvalidRequest
                | ProblemType::InvalidQuery
                | ProblemType::UnsupportedMediaType
                | ProblemType::PayloadTooLarge
                | ProblemType::InvalidActionSet
                | ProblemType::InvalidWorkflow
                | ProblemType::InvalidInput
                | ProblemType::InvalidTest
                | ProblemType::InvalidSignal
                | ProblemType::InvalidTitle
                | ProblemType::SecretRequired,
            ) => Exit::Usage,
            Some(ProblemType::TestsFailed) => Exit::Failed,
            Some(ProblemType::InvalidScript) => Exit::ScriptRefused,
            Some(
                ProblemType::Internal
                | ProblemType::ActionSetUnavailable
                | ProblemType::IdentityUnavailable
                | ProblemType::Unavailable,
            ) => Exit::Internal,
            // A problem this build does not know: by its status.
            None => match problem.status {
                404 | 409 | 410 | 429 | 401 | 403 => Exit::Refused,
                400..=499 => Exit::Usage,
                _ => Exit::Internal,
            },
        },
        rewind_api::Error::Config(_) | rewind_api::Error::Invalid { .. } => Exit::Usage,
        rewind_api::Error::Transport { .. }
        | rewind_api::Error::Decode(_)
        | rewind_api::Error::Signature(_) => Exit::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(kind: ProblemType) -> CliError {
        CliError::Api(Problem::new(kind, "detail").into())
    }

    #[test]
    fn problems_exit_by_their_kind() {
        assert_eq!(problem(ProblemType::NotFound).exit(), Exit::Refused);
        assert_eq!(problem(ProblemType::RunEnded).exit(), Exit::Refused);
        assert_eq!(problem(ProblemType::NotQuarantined).exit(), Exit::Refused);
        assert_eq!(problem(ProblemType::QuotaExceeded).exit(), Exit::Refused);
        assert_eq!(problem(ProblemType::InvalidRequest).exit(), Exit::Usage);
        assert_eq!(
            problem(ProblemType::InvalidScript).exit(),
            Exit::ScriptRefused
        );
        assert_eq!(problem(ProblemType::Internal).exit(), Exit::Internal);
        assert_eq!(problem(ProblemType::Unavailable).exit(), Exit::Internal);
        let unknown = CliError::Api(
            Problem {
                r#type: "urn:rewind:problem:something-new".into(),
                title: "New".into(),
                status: 409,
                detail: "detail".into(),
                instance: None,
                diagnostics: Vec::new(),
                query: None,
                tests: Vec::new(),
                attachments: Vec::new(),
                quota: None,
            }
            .into(),
        );
        assert_eq!(unknown.exit(), Exit::Refused);
        let unreachable = CliError::Api(rewind_api::Error::Transport {
            url: "http://localhost/".into(),
            message: "connection refused".into(),
        });
        assert_eq!(unreachable.exit(), Exit::Internal);
    }

    #[test]
    fn local_errors_read_as_problems() {
        let error = CliError::usage("--input is not JSON");
        assert_eq!(error.exit(), Exit::Usage);
        assert!(error.problem().is(ProblemType::InvalidRequest));
        let refused = CliError::ScriptRefused {
            message: "the script does not compile".into(),
            diagnostics: Vec::new(),
        };
        assert!(refused.problem().is(ProblemType::InvalidScript));
        assert_eq!(refused.exit(), Exit::ScriptRefused);
    }
}
