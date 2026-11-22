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
//! | erasure | a namespace marked for erasure (`DELETE /v1/namespaces/{name}`) has its active runs cancelled, then its runs erased, then its secrets and their reads, its use, alerts and itself |
//!
//! `DELETE /v1/runs/{id}` erases a run at any time: its journal, its archive, its read model
//! rows, its payload references and its count in its namespace's scripts.
//!
//! Erasing a namespace (`archi/06-storage-and-scale.md`, "Retention and deletion"):
//!
//! ```text
//! for (tenant, namespace) in namespaces_to_erase:        # marked: it takes no run any more
//!     for each run of the namespace:
//!         ended or quarantined → erased as DELETE /v1/runs/{id} erases it
//!         active               → cancel_requested, once (an ordinary cancellation, its grace
//!                                included): erased by the next round once it has ended
//!     no run left → its secrets, then drop_namespace: its use, alerts and script counts
//! ```
//!
//! The tenant's own use (`*`) keeps what the namespace consumed.

use std::collections::BTreeSet;
use std::time::Duration;

use rewind_core::{CancelReason, Completion, Digest, JournalDocument, Timestamp};

use crate::ports::{
    BlobError, BlobStore, DropNamespace, Enqueued, JournalStore, NamespaceName, ReadModel,
    ReadModelError, Retention, RunCursor, RunFilter, RunRef, SecretError, SecretStore, StoreError,
    TenantId, TenantScope,
};

/// The idempotency key of the cancellation an erasure sends to an active run: one per run,
/// whatever the rounds.
pub const ERASURE_CANCELLATION_KEY: &str = "rewind:namespace-erasure";

/// The message of that cancellation, which the script sees.
const ERASURE_CANCELLATION: &str = "its namespace is being erased";

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
    /// Tests no version referenced past their grace.
    pub tests_deleted: usize,
    pub purged_from_read_model: usize,
    /// Runs of namespaces being erased: erased, and cancelled.
    pub erased_runs: usize,
    pub cancelled_runs: usize,
    /// Namespaces erased.
    pub erased_namespaces: usize,
}

/// What one round of [`Housekeeper::erase_namespaces`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Erased {
    /// Runs erased.
    pub runs: usize,
    /// Active runs a cancellation was sent to.
    pub cancelled: usize,
    /// Namespaces erased, with their use, alerts and secrets.
    pub namespaces: usize,
    /// Tests recorded from their runs, erased.
    pub tests: usize,
}

/// The retention jobs. They borrow the adapters they need; the archive is written in the stored
/// format, which the composition root passes as `encode` (`rewind_format::encode_journal`): the
/// engine does not know the stored format.
#[derive(Debug)]
pub struct Housekeeper<'a, J, B, R, S> {
    pub journal: &'a J,
    pub blobs: &'a B,
    pub read_model: &'a R,
    /// An erased namespace's secrets are deleted with it.
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
        let erased = self.erase_namespaces(now).await?;
        Ok(Retained {
            archived: self.archive(now, encode).await?,
            deleted: self.delete_expired(now).await?,
            payloads_deleted: self.clean_payloads(now).await?,
            tests_deleted: self.clean_tests(now).await?,
            purged_from_read_model: self.purge_read_model(now).await?,
            erased_runs: erased.runs,
            cancelled_runs: erased.cancelled,
            erased_namespaces: erased.namespaces,
        })
    }

    /// Goes on erasing the namespaces marked for erasure, up to `batch` of them: cancels their
    /// active runs, erases their other runs, and once a namespace has no run left, erases its
    /// secrets, then its use, alerts and script counts with it. A run cancelled here is
    /// erased by a following round, once it has ended.
    pub async fn erase_namespaces(&self, now: Timestamp) -> Result<Erased, RetentionError> {
        let mut erased = Erased::default();
        for (tenant, namespace) in self.journal.namespaces_to_erase(self.batch).await? {
            let left = self
                .erase_runs(&tenant, &namespace, now, &mut erased)
                .await?;
            if left > 0 {
                continue;
            }
            erased.tests += self.erase_tests(&tenant, &namespace).await?;
            self.secrets.delete_namespace(&tenant, &namespace).await?;
            match self.journal.drop_namespace(&tenant, &namespace).await? {
                DropNamespace::Dropped => {
                    erased.namespaces += 1;
                    tracing::info!(%tenant, %namespace, "namespace erased");
                }
                // A run was created while it was being marked: the next round erases it.
                DropNamespace::RunsLeft | DropNamespace::NotFound => {}
            }
        }
        Ok(erased)
    }

    /// Erases the runs of a namespace being erased that have ended or are quarantined, and
    /// cancels the others. Returns how many runs are left.
    async fn erase_runs(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        now: Timestamp,
        erased: &mut Erased,
    ) -> Result<usize, RetentionError> {
        let mut left = 0;
        let mut after: Option<RunCursor> = None;
        loop {
            let filter = RunFilter {
                namespace: Some(namespace.clone()),
                after,
                limit: Some(self.batch.max(1)),
                ..RunFilter::new(tenant.clone())
            };
            let page = self.journal.list_runs(&filter).await?;
            let Some(last) = page.last() else {
                return Ok(left);
            };
            after = Some(RunCursor::from(last));
            for record in &page {
                // A quarantined run takes no cancellation while it stays quarantined: the
                // erasure of its end user does not wait for an operator.
                if record.status.is_terminal() || record.quarantine.is_some() {
                    if self.delete_run(&record.run, now).await? {
                        erased.runs += 1;
                    }
                    continue;
                }
                let cancel = Completion::CancelRequested {
                    reason: CancelReason::Requested(Some(ERASURE_CANCELLATION.to_owned())),
                };
                let key = Some(ERASURE_CANCELLATION_KEY.to_owned());
                match self.journal.enqueue(&record.run, cancel, key, now).await {
                    Ok(Enqueued::Accepted) => {
                        left += 1;
                        erased.cancelled += 1;
                        tracing::info!(run = %record.run, "run cancelled: its namespace is being erased");
                    }
                    // Cancelled by an earlier round, or ended since it was listed: erased by a
                    // following round.
                    Ok(Enqueued::Duplicate | Enqueued::RunEnded) => left += 1,
                    // Erased since it was listed.
                    Err(StoreError::RunNotFound(_)) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
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
    /// its read model rows and its payload references. The payloads it leaves
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

    /// Deletes the tests no version references, stored more than [`TEST_GRACE`] ago: marks
    /// their rows, deletes their blobs, then drops the rows still marked. Returns how many.
    ///
    /// [`TEST_GRACE`]: crate::ports::TEST_GRACE
    pub async fn clean_tests(&self, now: Timestamp) -> Result<usize, RetentionError> {
        let marked = self.journal.unreferenced_tests(now, self.batch).await?;
        if marked.is_empty() {
            return Ok(0);
        }
        for (tenant, digest) in &marked {
            self.blobs.delete(tenant, *digest).await?;
        }
        Ok(self.journal.drop_tests(&marked, false).await?)
    }

    /// Erases the tests recorded from the runs of a namespace being erased, whatever version
    /// references them: the end user's data goes with them. Returns how many.
    async fn erase_tests(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
    ) -> Result<usize, RetentionError> {
        let mut erased = 0;
        loop {
            let digests = self
                .journal
                .namespace_tests(tenant, namespace, self.batch.max(1))
                .await?;
            if digests.is_empty() {
                return Ok(erased);
            }
            for digest in &digests {
                self.blobs.delete(tenant, *digest).await?;
            }
            let keys: Vec<(TenantId, Digest)> = digests
                .into_iter()
                .map(|digest| (tenant.clone(), digest))
                .collect();
            erased += self.journal.drop_tests(&keys, true).await?;
        }
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
