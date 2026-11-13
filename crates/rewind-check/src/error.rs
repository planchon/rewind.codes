use std::io;
use std::path::PathBuf;
use std::time::Duration;

use rewind_engine::ports::CheckerError;

/// Why a check could not be made. A script with type errors is not one: its diagnostics are
/// the check's result.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// tsgo could not be extracted to the cache directory.
    #[error("cannot extract tsgo into {}: {source}", dir.display())]
    Install { dir: PathBuf, source: io::Error },
    /// The check's files could not be written.
    #[error("cannot write the files of the check: {0}")]
    Files(io::Error),
    #[error("cannot start tsgo: {0}")]
    Start(io::Error),
    #[error("tsgo did not finish within {0:?}")]
    Timeout(Duration),
    /// tsgo ended without the diagnostics it prints, or with diagnostics about its
    /// configuration.
    #[error("tsgo failed ({status}): {output}")]
    Failed { status: String, output: String },
    /// The toolset's declarations or the sandbox lib do not type-check by themselves: a fault
    /// of the platform, not of the script.
    #[error("the declarations do not type-check: {0}")]
    Declarations(String),
}

impl From<CheckError> for CheckerError {
    fn from(error: CheckError) -> Self {
        CheckerError::Internal(error.to_string())
    }
}
