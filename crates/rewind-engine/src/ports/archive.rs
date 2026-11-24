//! The `RunArchive` port: the sealed journal of each ended attempt, and the erasure of a run's
//! objects (`archi/07-code-architecture.md`, "`RunArchive`"; `archi/06-storage-and-scale.md`,
//! "The archive and the blobs", "Retention and deletion").
//!
//! When an attempt ends, the sealer puts its journal in the archive, every payload up to 64 KiB
//! inlined as a value, then seals the attempt in its partition. A sealed journal never changes,
//! and every process caches it.

use std::future::Future;

use rewind_core::{ActionSet, Code, HistoryEntry, JournalDocument};

use crate::ports::partition::Attempt;
use crate::ports::tenancy::RunRef;

/// The journal of an ended attempt: what replays it on its own.
#[derive(Clone, Debug, PartialEq)]
pub struct SealedAttempt {
    pub run: RunRef,
    pub attempt: Attempt,
    /// The code the run started with.
    pub code: Code,
    /// The pinned action set the run started with.
    pub action_set: ActionSet,
    /// The attempt's history, every payload up to 64 KiB inlined as a value; larger ones stay
    /// digests, their values in the run's blobs.
    pub entries: Vec<HistoryEntry>,
}

impl SealedAttempt {
    /// The attempt as a journal document (`archi/02-journal.md`, "Stored format v1").
    pub fn document(&self) -> JournalDocument {
        JournalDocument {
            run: self.run.id,
            code: self.code.clone(),
            action_set: self.action_set.clone(),
            entries: self.entries.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveError {
    /// What the archive holds cannot be read back as a journal document.
    #[error("corrupt sealed journal: {0}")]
    Corrupt(String),
    /// The backend cannot be reached; the call may be tried again.
    #[error("the archive is unavailable: {0}")]
    Unavailable(String),
    #[error("archive backend error: {0}")]
    Backend(String),
}

/// The sealed journals of the ended attempts.
///
/// The contract, checked by the conformance suite (feature `conformance`, module
/// `conformance::archive`):
///
/// - `put` writes the journal document of the attempt. A second `put` of the same attempt
///   changes nothing: the first stays.
/// - `get` reads it back as it was put; `None` for an attempt never put, or of an erased run.
/// - `erase` deletes every attempt of the run and every blob of the run (its payloads above
///   64 KiB). Erasing a run with nothing archived is not an error.
/// - A run of one tenant is never visible to another.
pub trait RunArchive: Send + Sync {
    fn put(&self, sealed: &SealedAttempt) -> impl Future<Output = Result<(), ArchiveError>> + Send;

    fn get(
        &self,
        run: &RunRef,
        attempt: Attempt,
    ) -> impl Future<Output = Result<Option<SealedAttempt>, ArchiveError>> + Send;

    fn erase(&self, run: &RunRef) -> impl Future<Output = Result<(), ArchiveError>> + Send;
}
