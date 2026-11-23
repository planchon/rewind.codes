//! Where the partitions live, and how SlateDB is run (`archi/06-storage-and-scale.md`,
//! "Backends"; `archi/08-decisions.md`, C10).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::{fmt, fs};

use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::memory::InMemory;
use slatedb::config::{
    CompactionWorkerOptions, CompactorOptions, CompressionCodec, GarbageCollectorDirectoryOptions,
    GarbageCollectorOptions, Settings,
};

use crate::Error;

/// Where the partitions live: `partitions/{id}/` under the bucket's root.
#[derive(Clone)]
pub enum Bucket {
    /// R2 or any S3-compatible service.
    S3(S3Bucket),
    /// Local files under a directory: the embedded server's `.rewind/`.
    Local(PathBuf),
    /// In memory, for tests: shared by the clones of the store.
    Memory,
    /// An object store built by the caller.
    Store(Arc<dyn ObjectStore>),
}

impl fmt::Debug for Bucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Bucket::S3(bucket) => f.debug_tuple("S3").field(bucket).finish(),
            Bucket::Local(path) => f.debug_tuple("Local").field(path).finish(),
            Bucket::Memory => f.write_str("Memory"),
            Bucket::Store(store) => f.debug_tuple("Store").field(&store.to_string()).finish(),
        }
    }
}

/// An S3 bucket. Without an access key, what is left out comes from the standard AWS
/// environment (`AWS_REGION`, `AWS_ENDPOINT`, `AWS_ACCESS_KEY_ID`…).
#[derive(Clone, Default)]
pub struct S3Bucket {
    pub bucket: String,
    /// `auto` for R2.
    pub region: Option<String>,
    /// The endpoint of an S3-compatible service (R2, MinIO…); requests use path-style URLs.
    pub endpoint: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    /// Allows an `http://` endpoint, for a service on a trusted network.
    pub allow_http: bool,
    /// A prefix of every key in the bucket, `partitions/` going under it.
    pub root: Option<String>,
}

impl fmt::Debug for S3Bucket {
    /// Leaves the secrets out.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted = |secret: &Option<String>| secret.as_ref().map(|_| "…");
        f.debug_struct("S3Bucket")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &redacted(&self.secret_access_key))
            .field("session_token", &redacted(&self.session_token))
            .field("allow_http", &self.allow_http)
            .field("root", &self.root)
            .finish()
    }
}

impl Bucket {
    /// The object store and the root of the partitions in it.
    pub(crate) fn open(&self) -> Result<(Arc<dyn ObjectStore>, String), Error> {
        Ok(match self {
            Bucket::S3(bucket) => {
                let mut builder = if bucket.access_key_id.is_some() {
                    AmazonS3Builder::new()
                } else {
                    AmazonS3Builder::from_env()
                };
                builder = builder
                    .with_bucket_name(&bucket.bucket)
                    .with_allow_http(bucket.allow_http);
                if let Some(region) = &bucket.region {
                    builder = builder.with_region(region);
                }
                if let Some(endpoint) = &bucket.endpoint {
                    builder = builder.with_endpoint(endpoint);
                }
                if let Some(key) = &bucket.access_key_id {
                    builder = builder.with_access_key_id(key);
                }
                if let Some(secret) = &bucket.secret_access_key {
                    builder = builder.with_secret_access_key(secret);
                }
                if let Some(token) = &bucket.session_token {
                    builder = builder.with_token(token);
                }
                let store = builder
                    .build()
                    .map_err(|err| Error::Config(err.to_string()))?;
                let root = bucket
                    .root
                    .as_deref()
                    .map(|root| format!("{}/", root.trim_matches('/')))
                    .unwrap_or_default();
                (Arc::new(store), root)
            }
            Bucket::Local(directory) => {
                fs::create_dir_all(directory).map_err(|err| {
                    Error::Config(format!("cannot create {}: {err}", directory.display()))
                })?;
                let store = LocalFileSystem::new_with_prefix(directory)
                    .map_err(|err| Error::Config(err.to_string()))?;
                (Arc::new(store), String::new())
            }
            Bucket::Memory => (Arc::new(InMemory::new()), String::new()),
            Bucket::Store(store) => (Arc::clone(store), String::new()),
        })
    }
}

/// SlateDB's timers: see [`Options`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timers {
    /// How often a writer reads the manifest. Shorter than the compactor's checkpoint lifetime
    /// (15 min).
    pub manifest_poll: Duration,
    /// How often the compactor reads the manifest and schedules compactions.
    pub compactor_poll: Duration,
    /// How often the compactor publishes the compactions that finished, while any runs.
    pub compactor_commit: Duration,
    /// How often the compaction worker looks for a compaction to run.
    pub worker_poll: Duration,
    /// How often the collector runs each of its tasks.
    pub collector: Duration,
    /// Batches after which the memtable is written as a level-0 table even below 64 MiB: what a
    /// new owner replays at most from the WAL when it takes the partition over. SlateDB refuses
    /// less than 4096.
    pub wal_flushes_per_table: u64,
}

impl Default for Timers {
    fn default() -> Self {
        Self {
            manifest_poll: Duration::from_secs(300),
            compactor_poll: Duration::from_secs(60),
            compactor_commit: Duration::from_secs(5),
            worker_poll: Duration::from_secs(60),
            collector: Duration::from_secs(600),
            wal_flushes_per_table: 4_096,
        }
    }
}

/// How the store runs SlateDB.
///
/// SlateDB has background tasks that wake on timers. They are the only timers of the run plane
/// besides the owner's timer wheel (`archi/07-code-architecture.md`, principle 11), so each is
/// set as long as SlateDB stays correct with it, and every one is listed here:
///
/// | Timer | Here | What it does when it fires |
/// | --- | --- | --- |
/// | WAL flush (`flush_interval`) | none | Nothing: the store flushes the WAL itself, once per batch, and the commit returns once it is durable |
/// | The writer's manifest poll (`manifest_poll_interval`) | [`Timers::manifest_poll`], 5 min | Reads the manifest to see what the compactor published. A writer must see a compaction before the compactor's checkpoint protecting its inputs expires (15 min): its reads would otherwise reach objects the collector deleted. Fencing never depends on it: a fenced writer finds out at its next write |
/// | The compactor's poll (`compactor_options.poll_interval`) | [`Timers::compactor_poll`], 1 min | Reads the manifest and `.compactions`, and schedules a compaction when level 0 has enough tables. Writes stall at 8 level-0 tables, one written per 64 MiB or per [`Timers::wal_flushes_per_table`] batches |
/// | The compactor's commit (`commit_compacted_interval`) | [`Timers::compactor_commit`], 5 s | Publishes finished compactions; reads nothing while none runs |
/// | The compactor's statistics | 10 s, fixed in SlateDB | Logs its counters; reads nothing |
/// | The compaction worker's poll (`compactions_poll_interval`) | [`Timers::worker_poll`], 1 min | Reads `.compactions` for a compaction to run |
/// | The compaction worker's heartbeat (`heartbeat_interval`) | 10 s, SlateDB's | Writes the progress of the compactions it runs; reads nothing while it runs none |
/// | The collector (`garbage_collector_options`) | [`Timers::collector`], 10 min each | Lists and deletes the manifests, WAL objects, compacted tables and `.compactions` older than 5 min that nothing references. Fence objects are never deleted (SlateDB warns it can lose data), and the detach task is off: no partition is a clone |
///
/// SlateDB's own retries of an object-store request after an error back off on timers too
/// (allowed: a backoff after an error). Its block cache is shared by every partition the store
/// opens, each under its own id.
///
/// The idle requests these timers make were measured by `examples/bench.rs`.
#[derive(Clone, Debug)]
pub struct Options {
    pub timers: Timers,
    /// The bytes of data blocks the block cache keeps, across the partitions of the store.
    pub block_cache_bytes: u64,
    /// The bytes of indexes, filters and statistics it keeps.
    pub meta_cache_bytes: u64,
    /// Runs the compactor and its worker in the writer's process: a partition's owner compacts
    /// it (C10). Off only for tests that look at level 0.
    pub compactor: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timers: Timers::default(),
            block_cache_bytes: 256 * 1024 * 1024,
            meta_cache_bytes: 64 * 1024 * 1024,
            compactor: true,
        }
    }
}

impl Options {
    /// SlateDB's settings for a partition's writer.
    pub(crate) fn settings(&self) -> Settings {
        let timers = self.timers;
        let collect = Some(GarbageCollectorDirectoryOptions {
            interval: Some(timers.collector),
            ..GarbageCollectorDirectoryOptions::default()
        });
        Settings {
            // The committer flushes the WAL after each batch: no timer.
            flush_interval: None,
            manifest_poll_interval: timers.manifest_poll,
            max_wal_flushes_before_l0_flush: timers.wal_flushes_per_table,
            compression_codec: Some(CompressionCodec::Lz4),
            compactor_options: self.compactor.then(|| CompactorOptions {
                poll_interval: timers.compactor_poll,
                commit_compacted_interval: timers.compactor_commit,
                worker: Some(CompactionWorkerOptions {
                    compactions_poll_interval: timers.worker_poll,
                    compression_codec: Some(CompressionCodec::Lz4),
                    ..CompactionWorkerOptions::default()
                }),
                ..CompactorOptions::default()
            }),
            garbage_collector_options: Some(GarbageCollectorOptions {
                manifest_options: collect,
                wal_options: collect,
                wal_fence_options: None,
                compacted_options: collect,
                compactions_options: collect,
                detach_options: None,
                ..GarbageCollectorOptions::default()
            }),
            ..Settings::default()
        }
    }
}
