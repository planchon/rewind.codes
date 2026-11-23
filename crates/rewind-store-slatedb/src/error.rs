//! SlateDB's errors as the port's (`archi/07-code-architecture.md`, principle 8).

use rewind_core::PartitionId;
use rewind_engine::ports::PartitionError;
use slatedb::{CloseReason, ErrorKind};

/// What a store cannot be built from.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid partition store configuration: {0}")]
    Config(String),
}

/// A SlateDB error of a partition.
pub(crate) fn partition(partition: PartitionId, err: slatedb::Error) -> PartitionError {
    match err.kind() {
        ErrorKind::Closed(CloseReason::Fenced) => PartitionError::Fenced { partition },
        ErrorKind::Closed(reason) => PartitionError::Backend(format!(
            "partition {partition} is closed ({reason:?}): {err}"
        )),
        ErrorKind::Unavailable => PartitionError::Unavailable(err.to_string()),
        ErrorKind::Data => PartitionError::Corrupt(err.to_string()),
        _ => PartitionError::Backend(err.to_string()),
    }
}

/// A SlateDB error of an open. Two opens that create a partition's first manifest at once race
/// on it, and the one that loses finds it written: the other open is under way, which fences
/// this one.
pub(crate) fn opening(partition: PartitionId, err: slatedb::Error) -> PartitionError {
    if matches!(err.kind(), ErrorKind::Data) && err.to_string().contains("version already exists") {
        return PartitionError::Fenced { partition };
    }
    self::partition(partition, err)
}
