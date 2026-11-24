//! The ports of `rewind server migrate-runs` in memory: the engine's tests of the migration.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rewind_core::{Digest, HistoryEntry, RunId};

use crate::ports::TenantId;
use crate::ports::legacy::{LegacyBlobs, LegacyError, LegacyRun, LegacyRuns};

/// Set-aside runs and their tenant-keyed blobs, held in memory.
#[derive(Debug, Default)]
pub struct MemoryLegacy {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    runs: BTreeMap<RunId, LegacyRun>,
    payloads: BTreeMap<(TenantId, Digest), Vec<u8>>,
    exports: BTreeMap<(TenantId, Digest), Vec<HistoryEntry>>,
    dropped: bool,
}

impl MemoryLegacy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a run to the set-aside tables.
    pub fn put_run(&self, run: LegacyRun) {
        self.lock().runs.insert(run.run.id, run);
    }

    /// Adds a payload value of the tenant: its canonical JSON.
    pub fn put_payload(&self, tenant: &TenantId, digest: Digest, bytes: Vec<u8>) {
        self.lock().payloads.insert((tenant.clone(), digest), bytes);
    }

    /// Adds an archived run export of the tenant, by its digest.
    pub fn put_export(&self, tenant: &TenantId, digest: Digest, entries: Vec<HistoryEntry>) {
        self.lock()
            .exports
            .insert((tenant.clone(), digest), entries);
    }

    /// Whether the tables were dropped.
    pub fn is_dropped(&self) -> bool {
        self.lock().dropped
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl LegacyRuns for MemoryLegacy {
    async fn runs(
        &self,
        after: Option<RunId>,
        limit: usize,
    ) -> Result<Vec<LegacyRun>, LegacyError> {
        let state = self.lock();
        if state.dropped {
            return Ok(Vec::new());
        }
        Ok(state
            .runs
            .values()
            .filter(|run| after.is_none_or(|after| run.run.id > after))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn drop_tables(&self) -> Result<(), LegacyError> {
        let mut state = self.lock();
        state.runs.clear();
        state.dropped = true;
        Ok(())
    }
}

impl LegacyBlobs for MemoryLegacy {
    async fn payload(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Vec<u8>>, LegacyError> {
        Ok(self.lock().payloads.get(&(tenant.clone(), digest)).cloned())
    }

    async fn export(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Vec<HistoryEntry>>, LegacyError> {
        Ok(self.lock().exports.get(&(tenant.clone(), digest)).cloned())
    }
}
