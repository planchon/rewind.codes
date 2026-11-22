use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::adapters::MemoryBlobs;
use crate::ports::{ArchiveError, Attempt, RunArchive, RunRef, SealedAttempt};

type Sealed = BTreeMap<(RunRef, Attempt), SealedAttempt>;

/// `RunArchive` in memory: the sealed attempts, and the blobs of the runs it erases, in the
/// [`MemoryBlobs`] it shares with the engine. It passes the conformance suite: it is the
/// reference implementation of the contract. Its clones share their attempts.
#[derive(Debug, Default, Clone)]
pub struct MemoryArchive {
    sealed: Arc<Mutex<Sealed>>,
    blobs: MemoryBlobs,
}

impl MemoryArchive {
    /// An archive with blobs of its own.
    pub fn new() -> Self {
        Self::default()
    }

    /// An archive whose `erase` deletes the runs' blobs from `blobs`, a clone of the blob store
    /// the engine writes to.
    pub fn with_blobs(blobs: MemoryBlobs) -> Self {
        Self {
            sealed: Arc::default(),
            blobs,
        }
    }

    /// Its blob store.
    pub fn blobs(&self) -> &MemoryBlobs {
        &self.blobs
    }

    /// Number of attempts sealed, across runs.
    pub fn len(&self) -> usize {
        self.lock().map_or(0, |sealed| sealed.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> Result<MutexGuard<'_, Sealed>, ArchiveError> {
        self.sealed
            .lock()
            .map_err(|_| ArchiveError::Backend("the memory archive's lock is poisoned".into()))
    }
}

impl RunArchive for MemoryArchive {
    async fn put(&self, sealed: &SealedAttempt) -> Result<(), ArchiveError> {
        self.lock()?
            .entry((sealed.run.clone(), sealed.attempt))
            .or_insert_with(|| sealed.clone());
        Ok(())
    }

    async fn get(
        &self,
        run: &RunRef,
        attempt: Attempt,
    ) -> Result<Option<SealedAttempt>, ArchiveError> {
        Ok(self.lock()?.get(&(run.clone(), attempt)).cloned())
    }

    async fn erase(&self, run: &RunRef) -> Result<(), ArchiveError> {
        self.blobs
            .remove_run(run)
            .map_err(|error| ArchiveError::Backend(error.to_string()))?;
        self.lock()?.retain(|(of, _), _| of != run);
        Ok(())
    }
}
