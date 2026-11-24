//! `RunArchive` on local files and on S3, and the local cache in front of either
//! (`archi/07-code-architecture.md`, "`RunArchive`"; `archi/06-storage-and-scale.md`, "The
//! archive and the blobs", "Retention and deletion").
//!
//! A sealed attempt is the object `runs/{tenant}/{run}/{attempt}.json`: its run export v1
//! (`rewind_format::encode_run_export`), every payload up to 64 KiB inlined as a value, larger ones
//! digests whose values are the run's blobs. It is written once: a second put of the attempt
//! leaves the first. Erasing a run deletes its sealed attempts, then its blobs, the prefix
//! `blobs/{tenant}/{run}/`; an erasure stopped half-way is done again whole.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt as _;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore as _, ObjectStoreExt as _, PutMode};
use quick_cache::sync::Cache;
use quick_cache::{OptionsBuilder, Weighter};
use rewind_engine::ports::{
    ArchiveError, Attempt, BlobError, BlobStore, RunArchive, RunRef, SealedAttempt,
};

use crate::files::{self, tenant_directory};
use crate::{FileBlobs, S3Blobs};

/// The run export of a sealed attempt, as stored.
fn encode(sealed: &SealedAttempt) -> Vec<u8> {
    rewind_format::encode_run_export(&sealed.document()).into_bytes()
}

/// A sealed attempt read back: the document must be the run's.
fn decode(run: &RunRef, attempt: Attempt, bytes: &[u8]) -> Result<SealedAttempt, ArchiveError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|err| ArchiveError::Corrupt(format!("{run} attempt {attempt}: {err}")))?;
    let document = rewind_format::decode_run_export(text)
        .map_err(|err| ArchiveError::Corrupt(format!("{run} attempt {attempt}: {err}")))?;
    if document.run != run.id {
        return Err(ArchiveError::Corrupt(format!(
            "{run} attempt {attempt} holds the history of run {}",
            document.run
        )));
    }
    Ok(SealedAttempt {
        run: run.clone(),
        attempt,
        code: document.code,
        action_set: document.action_set,
        events: document.events,
    })
}

fn blobs_failed(error: BlobError) -> ArchiveError {
    ArchiveError::Backend(format!("cannot erase the run's blobs: {error}"))
}

// -------------------------------------------------------------------------------------------
// Files

/// `RunArchive` on local files, beside the blobs of a [`FileBlobs`] (`.rewind/`): a sealed
/// attempt is `{root}/runs/{tenant}/{run}/{attempt}.json`, the tenant's capital letters
/// escaped as the blobs' are. A put writes a staging file, syncs it, and renames it without
/// replacing an attempt already there. File operations run on Tokio's blocking threads.
#[derive(Debug, Clone)]
pub struct FileArchive {
    blobs: FileBlobs,
}

impl FileArchive {
    /// The archive of the blob directory `blobs`, whose run blobs it erases with each run.
    pub fn new(blobs: FileBlobs) -> Self {
        Self { blobs }
    }

    pub fn blobs(&self) -> &FileBlobs {
        &self.blobs
    }

    /// The path of a sealed attempt: `{root}/runs/{tenant}/{run}/{attempt}.json`.
    pub fn path(&self, run: &RunRef, attempt: Attempt) -> PathBuf {
        self.run_directory(run).join(format!("{attempt}.json"))
    }

    fn run_directory(&self, run: &RunRef) -> PathBuf {
        self.blobs
            .root()
            .join("runs")
            .join(tenant_directory(&run.tenant))
            .join(run.id.to_string())
    }
}

fn file_failed(action: &str, path: &Path, error: &io::Error) -> ArchiveError {
    ArchiveError::Backend(format!("cannot {action} {}: {error}", path.display()))
}

/// Runs file I/O on Tokio's blocking threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ArchiveError> + Send + 'static,
) -> Result<T, ArchiveError> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        ArchiveError::Backend(format!("the archive's file task failed: {error}"))
    })?
}

impl RunArchive for FileArchive {
    async fn put(&self, sealed: &SealedAttempt) -> Result<(), ArchiveError> {
        let path = self.path(&sealed.run, sealed.attempt);
        let staging = self.blobs.staging();
        let bytes = encode(sealed);
        blocking(move || {
            files::write(&staging, &path, &bytes)
                .map_err(|error| file_failed("write", &path, &error))
        })
        .await
    }

    async fn get(
        &self,
        run: &RunRef,
        attempt: Attempt,
    ) -> Result<Option<SealedAttempt>, ArchiveError> {
        let path = self.path(run, attempt);
        let bytes = blocking(move || match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(file_failed("read", &path, &error)),
        })
        .await?;
        bytes.map(|bytes| decode(run, attempt, &bytes)).transpose()
    }

    async fn erase(&self, run: &RunRef) -> Result<(), ArchiveError> {
        let directory = self.run_directory(run);
        blocking(move || {
            files::remove_directory(&directory)
                .map_err(|error| file_failed("delete", &directory, &error))
        })
        .await?;
        self.blobs.delete_run(run).await.map_err(blobs_failed)
    }
}

// -------------------------------------------------------------------------------------------
// S3

/// `RunArchive` on S3, in the bucket of an [`S3Blobs`]: a sealed attempt is the object
/// `runs/{tenant}/{run}/{attempt}.json`. A put is conditional (`If-None-Match: *`): a second
/// put of an attempt never rewrites it. Erasing a run lists and deletes its prefix
/// `runs/{tenant}/{run}/`, then the prefix of its blobs.
#[derive(Debug, Clone)]
pub struct S3Archive {
    blobs: S3Blobs,
}

impl S3Archive {
    /// The archive of the bucket of `blobs`, whose run blobs it erases with each run.
    pub fn new(blobs: S3Blobs) -> Self {
        Self { blobs }
    }

    pub fn blobs(&self) -> &S3Blobs {
        &self.blobs
    }

    /// The key of a sealed attempt: `runs/{tenant}/{run}/{attempt}.json`.
    pub fn key(run: &RunRef, attempt: Attempt) -> String {
        format!("{}/{attempt}.json", Self::run_prefix(run))
    }

    /// The prefix of a run's sealed attempts: `runs/{tenant}/{run}`.
    pub fn run_prefix(run: &RunRef) -> String {
        format!("runs/{}/{}", run.tenant, run.id)
    }
}

fn s3_failed(action: &str, location: &ObjectPath, error: &object_store::Error) -> ArchiveError {
    ArchiveError::Backend(format!("cannot {action} s3 object {location}: {error}"))
}

impl RunArchive for S3Archive {
    async fn put(&self, sealed: &SealedAttempt) -> Result<(), ArchiveError> {
        // Tenant ids, run ids and attempts hold no character a path would encode.
        let location = ObjectPath::from(Self::key(&sealed.run, sealed.attempt));
        match self
            .blobs
            .store()
            .put_opts(&location, encode(sealed).into(), PutMode::Create.into())
            .await
        {
            // A sealed attempt is immutable: a second put changes nothing.
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(error) => Err(s3_failed("write", &location, &error)),
        }
    }

    async fn get(
        &self,
        run: &RunRef,
        attempt: Attempt,
    ) -> Result<Option<SealedAttempt>, ArchiveError> {
        let location = ObjectPath::from(Self::key(run, attempt));
        let object = match self.blobs.store().get(&location).await {
            Ok(object) => object,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(s3_failed("read", &location, &error)),
        };
        let bytes = object
            .bytes()
            .await
            .map_err(|error| s3_failed("read", &location, &error))?;
        decode(run, attempt, &bytes).map(Some)
    }

    async fn erase(&self, run: &RunRef) -> Result<(), ArchiveError> {
        let prefix = ObjectPath::from(Self::run_prefix(run));
        let store = self.blobs.store();
        let mut listed = store.list(Some(&prefix));
        while let Some(object) = listed.next().await {
            let object = object.map_err(|error| s3_failed("list", &prefix, &error))?;
            match store.delete(&object.location).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(error) => return Err(s3_failed("delete", &object.location, &error)),
            }
        }
        self.blobs.delete_run(run).await.map_err(blobs_failed)
    }
}

// -------------------------------------------------------------------------------------------
// The local cache

/// The sealed attempts of a run that the cache holds, with what each weighs.
type Attempts = BTreeMap<Attempt, (Arc<SealedAttempt>, u64)>;

/// What an entry costs beyond its documents: its key and the cache's bookkeeping.
const ENTRY_OVERHEAD: u64 = 256;

/// The cache is split into this many independently locked shards. A run whose attempts weigh
/// more than a shard, an eighth of the capacity, is not cached.
const SHARDS: usize = 8;

/// The size the cache expects of an average run's sealed attempts, to size its tables.
const TYPICAL_RUN: u64 = 64 * 1024;

/// The default capacity of a [`CachedArchive`]: 256 MiB.
pub const DEFAULT_ARCHIVE_CACHE_CAPACITY: u64 = 256 * 1024 * 1024;

/// A `RunArchive` with a local cache in front of it: what every process puts in front of the
/// archive, since a sealed attempt never changes.
///
/// The cache is in memory, bounded in bytes (the size of each attempt's run export), and
/// evicts the runs read least recently and least often (CLOCK-Pro, from `quick_cache`). A get
/// that misses reads the inner archive and keeps the attempt; a put only writes to the inner
/// archive, since the attempt the archive keeps is the first put, which may be another
/// process's; an erasure drops the run's attempts, then erases the run from the inner archive.
/// Another process's cache may still hold a run this one erased: an erased run is read no more,
/// since its head is gone from its partition.
pub struct CachedArchive<A> {
    inner: A,
    cache: Cache<RunRef, Arc<Attempts>, AttemptsWeight>,
    capacity: u64,
}

impl<A> CachedArchive<A> {
    /// A cache of at most `capacity` bytes in front of `inner`; see
    /// [`DEFAULT_ARCHIVE_CACHE_CAPACITY`].
    pub fn new(inner: A, capacity: u64) -> Self {
        // At least 32 entries per shard, or quick_cache merges shards.
        let entries = usize::try_from(capacity / TYPICAL_RUN)
            .unwrap_or(usize::MAX)
            .max(32 * SHARDS);
        let options = OptionsBuilder::new()
            .shards(SHARDS)
            .estimated_items_capacity(entries)
            .weight_capacity(capacity)
            .build()
            .expect("both capacities are set");
        Self {
            inner,
            cache: Cache::with_options(
                options,
                AttemptsWeight,
                Default::default(),
                Default::default(),
            ),
            capacity,
        }
    }

    pub fn inner(&self) -> &A {
        &self.inner
    }

    /// The bound of the cache, in bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The bytes the cache holds, the entries' overhead included.
    pub fn weight(&self) -> u64 {
        self.cache.weight()
    }
}

impl<A> fmt::Debug for CachedArchive<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedArchive")
            .field("capacity", &self.capacity)
            .field("weight", &self.cache.weight())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
struct AttemptsWeight;

impl Weighter<RunRef, Arc<Attempts>> for AttemptsWeight {
    fn weight(&self, _run: &RunRef, attempts: &Arc<Attempts>) -> u64 {
        attempts
            .values()
            .map(|(_, weight)| *weight)
            .sum::<u64>()
            .saturating_add(ENTRY_OVERHEAD)
    }
}

impl<A: RunArchive> RunArchive for CachedArchive<A> {
    async fn put(&self, sealed: &SealedAttempt) -> Result<(), ArchiveError> {
        self.inner.put(sealed).await
    }

    async fn get(
        &self,
        run: &RunRef,
        attempt: Attempt,
    ) -> Result<Option<SealedAttempt>, ArchiveError> {
        if let Some((sealed, _)) = self
            .cache
            .get(run)
            .and_then(|attempts| attempts.get(&attempt).cloned())
        {
            return Ok(Some(SealedAttempt::clone(&sealed)));
        }
        let Some(sealed) = self.inner.get(run, attempt).await? else {
            return Ok(None);
        };
        let weight = u64::try_from(encode(&sealed).len()).unwrap_or(u64::MAX);
        let mut attempts = self
            .cache
            .get(run)
            .map(|attempts| Attempts::clone(&attempts))
            .unwrap_or_default();
        attempts.insert(attempt, (Arc::new(sealed.clone()), weight));
        self.cache.insert(run.clone(), Arc::new(attempts));
        Ok(Some(sealed))
    }

    async fn erase(&self, run: &RunRef) -> Result<(), ArchiveError> {
        self.cache.remove(run);
        self.inner.erase(run).await
    }
}
