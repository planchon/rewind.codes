//! Retention and deletion (`archi/06-storage-and-scale.md`, "Retention and deletion").
//!
//! Active runs are never touched. Each tenant's settings apply, the defaults otherwise:
//!
//! | Job | What it does |
//! | --- | --- |
//! | archive | a finished run past its journal retention (30 days) gets its journal document in the `BlobStore`, and its history leaves the store |
//! | expiry | an archive past its retention (1 year) is deleted with its run |
//! | payload cleanup | a payload no run has referenced for its grace (24 h) is deleted, blob then row |
//! | read model | runs that ended before its retention (90 days) leave the read model |
//!
//! `DELETE /v1/runs/{id}` erases a run at any time: its journal, its archive, its read model
//! rows, its credentials and its payload references.

use std::collections::BTreeSet;
use std::time::Duration;

use ptc_core::{Digest, JournalDocument, Timestamp};

use crate::ports::{
    BlobError, BlobStore, JournalStore, ReadModel, ReadModelError, Retention, RunRef, SecretError,
    SecretOwner, SecretStore, StoreError, TenantScope,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RetentionError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error(transparent)]
    ReadModel(#[from] ReadModelError),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error("the script or the toolset of run {0} is missing: its journal cannot be archived")]
    Incomplete(RunRef),
}

/// What one round of the retention jobs did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Retained {
    pub archived: usize,
    pub deleted: usize,
    pub payloads_deleted: usize,
    pub purged_from_read_model: usize,
}

/// The retention jobs. They borrow the adapters they need; the archive is written in the stored
/// format, which the composition root passes as `encode` (`ptc_format::encode_journal`): the
/// engine does not know the stored format.
#[derive(Debug)]
pub struct Housekeeper<'a, J, B, R, S> {
    pub journal: &'a J,
    pub blobs: &'a B,
    pub read_model: &'a R,
    /// A deleted run's credentials are deleted with it.
    pub secrets: &'a S,
    /// Runs or payloads handled per call to the store.
    pub batch: usize,
}

impl<J, B, R, S> Housekeeper<'_, J, B, R, S>
where
    J: JournalStore,
    B: BlobStore,
    R: ReadModel,
    S: SecretStore,
{
    /// Every job, once.
    pub async fn run(
        &self,
        now: Timestamp,
        encode: impl Fn(&JournalDocument) -> String,
    ) -> Result<Retained, RetentionError> {
        Ok(Retained {
            archived: self.archive(now, encode).await?,
            deleted: self.delete_expired(now).await?,
            payloads_deleted: self.clean_payloads(now).await?,
            purged_from_read_model: self.purge_read_model(now).await?,
        })
    }

    /// Archives the finished runs past their journal retention: each journal document goes to
    /// the `BlobStore` under the SHA-256 of its bytes, then the run's history leaves the store.
    /// Its payloads stay where they are, referenced by the run: the document holds large
    /// payloads by digest, and is loaded on demand. Returns how many runs were archived.
    pub async fn archive(
        &self,
        now: Timestamp,
        encode: impl Fn(&JournalDocument) -> String,
    ) -> Result<usize, RetentionError> {
        let mut archived = 0;
        for run in self.journal.runs_to_archive(now, self.batch).await? {
            let Some(record) = self.journal.get_run(&run).await? else {
                continue;
            };
            let script = self.journal.get_script(&run.tenant, record.script).await?;
            let toolset = self
                .journal
                .get_toolset(&run.tenant, record.toolset)
                .await?;
            let (Some(script), Some(toolset)) = (script, toolset) else {
                return Err(RetentionError::Incomplete(run));
            };
            let entries = self.journal.load_history(&run, 0).await?;
            let document = JournalDocument {
                run: run.id,
                script: script.script,
                toolset: toolset.toolset,
                entries,
            };
            let bytes = encode(&document).into_bytes();
            let digest = Digest::of(&bytes);
            self.blobs.put(&run.tenant, digest, bytes).await?;
            if self.journal.archive_run(&run, digest, now).await? {
                archived += 1;
                tracing::debug!(%run, archive = %digest, "journal archived");
            }
        }
        Ok(archived)
    }

    /// The journal document of an archived run, as archived: `None` if the run is not archived.
    pub async fn load_archive(&self, run: &RunRef) -> Result<Option<Vec<u8>>, RetentionError> {
        let Some(archive) = self
            .journal
            .get_run(run)
            .await?
            .and_then(|record| record.archive)
        else {
            return Ok(None);
        };
        let bytes = self
            .blobs
            .get(&run.tenant, archive.document)
            .await?
            .ok_or(BlobError::Missing(archive.document))?;
        Ok(Some(bytes))
    }

    /// Deletes the runs whose archive is past its retention. Returns how many.
    pub async fn delete_expired(&self, now: Timestamp) -> Result<usize, RetentionError> {
        let mut deleted = 0;
        for run in self.journal.runs_to_delete(now, self.batch).await? {
            if self.delete_run(&run, now).await? {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Erases a run: its record, history and everything the store keeps for it, its archive,
    /// its read model rows, its credentials and its payload references. The payloads it leaves
    /// unreferenced are deleted after their grace. Returns whether the run existed.
    pub async fn delete_run(&self, run: &RunRef, now: Timestamp) -> Result<bool, RetentionError> {
        let archive = self
            .journal
            .get_run(run)
            .await?
            .and_then(|record| record.archive);
        let existed = self.journal.delete_run(run, now).await?;
        if let Some(archive) = archive {
            self.blobs.delete(&run.tenant, archive.document).await?;
        }
        self.read_model.delete_run(run).await?;
        self.secrets
            .delete(&run.tenant, &SecretOwner::Run(run.id))
            .await?;
        if existed {
            tracing::info!(%run, "run deleted");
        }
        Ok(existed)
    }

    /// Deletes the payloads no run has referenced for their tenant's grace: their deletion
    /// starts in the store, then their blobs are deleted, then their rows. A payload that a
    /// commit took back in the meantime keeps its row, which holds its value. Returns how many
    /// were deleted.
    ///
    /// A round that stops halfway leaves payloads whose deletion started: a later round starts
    /// it again after [`PAYLOAD_DELETION_TIMEOUT`](crate::ports::PAYLOAD_DELETION_TIMEOUT).
    pub async fn clean_payloads(&self, now: Timestamp) -> Result<usize, RetentionError> {
        let candidates = self.journal.unreferenced_payloads(now, self.batch).await?;
        if candidates.is_empty() {
            return Ok(0);
        }
        let forgotten = self.journal.forget_payloads(&candidates, now).await?;
        if forgotten.is_empty() {
            return Ok(0);
        }
        for (tenant, digest) in &forgotten {
            // A payload of the table tier has no blob: deleting a missing blob does nothing.
            self.blobs.delete(tenant, *digest).await?;
        }
        Ok(self.journal.drop_payloads(&forgotten, now).await?)
    }

    /// Removes from the read model the runs that ended before their tenant's read model
    /// retention. Returns how many.
    pub async fn purge_read_model(&self, now: Timestamp) -> Result<usize, RetentionError> {
        let before = |retention: Duration| {
            let ms = u64::try_from(retention.as_millis()).unwrap_or(u64::MAX);
            Timestamp(now.as_millis().saturating_sub(ms))
        };
        let tenants = self.journal.list_tenants().await?;
        let mut purged = 0;
        for tenant in &tenants {
            let scope = TenantScope::Only(BTreeSet::from([tenant.id.clone()]));
            purged += self
                .read_model
                .purge(before(tenant.retention.read_model), &scope)
                .await?;
        }
        let others = TenantScope::Except(tenants.into_iter().map(|tenant| tenant.id).collect());
        purged += self
            .read_model
            .purge(before(Retention::default().read_model), &others)
            .await?;
        Ok(purged)
    }
}
