//! `rewind server migrate-runs`: moves the runs the schema migration set aside onto the run
//! plane, once (`archi/08-decisions.md`, "Taken for the object-storage rewrite": C2; "Taken
//! while writing the object-storage spec": the migration; `archi/07-code-architecture.md`,
//! "Embedded server and CLI").
//!
//! ```text
//! rewind server migrate-runs --store postgres://… --bucket s3://… [--blobs s3://…|<dir>] …
//! rewind server migrate-runs --embedded [--dir .rewind]
//! ```
//!
//! It takes the server's options. It opens the control plane (which sets the run tables aside,
//! migration 14 on Postgres, version 12 on Turso), places the namespaces in their partitions,
//! moves each run into its namespace's partition or into the archive ([`rewind_engine::migrate`]),
//! with its payloads above 64 KiB read from `--blobs` (the bucket by default), prints what it
//! moved and skipped, and drops the set-aside tables once every run moved. Run again, it skips
//! what it moved. Opening a partition fences its writer: it runs while no node runs.
//!
//! Embedded, it moves a `.rewind/` of the journal store into the run plane's layout:
//!
//! ```text
//! journal.db (+ -wal)      ──► control.db (+ -wal), its run tables set aside, then moved
//! blobs/tests/<tenant>/…   ──► tests/<tenant>/…
//! blobs/<tenant>/sha256/…  ──► blobs/<tenant>/<run>/sha256/… (each run's), then removed
//!                              partitions/0/, runs/<tenant>/<run>/1.json
//! ```
//!
//! under the directory's lock: a second command, or a server, waits for none and exits 75.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use rewind_core::RunId;
use rewind_engine::adapters::SystemClock;
use rewind_engine::migrate::{Migration, MovedAs, Report};
use rewind_engine::ports::Clock;
use rewind_engine::ports::legacy::{LegacyError, LegacyRun, LegacyRuns};
use rewind_store_postgres::PostgresLegacy;
use rewind_store_turso::TursoLegacy;

use crate::ServerArgs;
use crate::config::MasterKeyRing;
use crate::embedded::Directory;
use crate::serve::StartupError;
use crate::wiring::bucket::{self, BucketLocation, LegacyBlobStore, S3Options};
use crate::wiring::control::ControlPlane;

/// `rewind server migrate-runs`.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "rewind server migrate-runs",
    about = "Moves the runs of the journal store into their partitions and the archive, once: \
             ended runs sealed in the archive, live ones in their namespace's partition. Run it \
             while no node runs: opening a partition fences its writer. It skips what it moved, \
             so it can run again.",
    version
)]
pub struct MigrateArgs {
    /// Where the runs' payloads above 64 KiB and their archived journals are: the directory or
    /// `s3://<bucket>` the server kept its blobs in (with the `--s3-*` of the bucket). The
    /// bucket by default; `.rewind/blobs` embedded.
    #[arg(long, env = "REWIND_BLOBS")]
    pub blobs: Option<String>,

    #[command(flatten)]
    pub server: ServerArgs,
}

/// `rewind server migrate-runs …`: `args` are the arguments after `migrate-runs`. Exits 0 once
/// every run moved, 70 when one could not be moved or a store failed, 64 for a usage error, 75
/// when an embedded server holds the directory.
pub(crate) fn main(args: Vec<OsString>) -> ExitCode {
    let args = match MigrateArgs::try_parse_from(
        std::iter::once(OsString::from("rewind server migrate-runs")).chain(args),
    ) {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(if error.use_stderr() {
                crate::exit::USAGE
            } else {
                0
            });
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("rewind-migrate-runs")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rewind server migrate-runs: cannot start the async runtime: {error}");
            return ExitCode::from(crate::exit::FAILED);
        }
    };
    runtime.block_on(async move {
        let _telemetry = match crate::wiring::telemetry::install(&args.server) {
            Ok(guard) => guard,
            Err(error) => {
                eprintln!("rewind server migrate-runs: telemetry: {error}");
                return ExitCode::from(crate::exit::USAGE);
            }
        };
        match migrate_runs(args).await {
            Ok(report) => {
                print(&report);
                ExitCode::from(if report.failed.is_empty() {
                    crate::exit::STOPPED
                } else {
                    crate::exit::FAILED
                })
            }
            Err(error) => {
                eprintln!("rewind server migrate-runs: {error}");
                ExitCode::from(crate::exit_code(&error))
            }
        }
    })
}

/// What it moved, skipped and could not move, one line each, then the totals.
fn print(report: &Report) {
    for moved in &report.moved {
        let how = match moved.how {
            MovedAs::Ended => "ended, sealed in the archive",
            MovedAs::Live => "live",
        };
        println!(
            "moved {} ({how}) into partition {}",
            moved.run, moved.partition
        );
    }
    for run in &report.skipped {
        println!("skipped {run}: moved before");
    }
    for (run, why) in &report.failed {
        println!("not moved {run}: {why}");
    }
    println!(
        "{} run(s) moved, {} skipped, {} not moved; {}",
        report.moved.len(),
        report.skipped.len(),
        report.failed.len(),
        if report.dropped {
            "the set-aside tables are dropped"
        } else {
            "the set-aside tables are kept: run migrate-runs again once what stopped them is fixed"
        }
    );
}

/// Moves the runs of the store `args` name, or of the embedded directory; see the module.
pub async fn migrate_runs(args: MigrateArgs) -> Result<Report, StartupError> {
    let MigrateArgs { blobs, server } = args;
    if server.embedded {
        return embedded(server).await;
    }
    let s3 = S3Options {
        bucket: String::new(),
        endpoint: server.s3_endpoint.clone(),
        region: server.s3_region.clone(),
        allow_http: server.s3_allow_http,
        access_key_id: server.s3_access_key_id.clone(),
        secret_access_key: server.s3_secret_access_key.clone(),
    };
    let config = server.resolve()?;
    let keys = config
        .master_keys
        .clone()
        .ok_or_else(|| StartupError::Config("migrate-runs needs --master-key".into()))?;
    let legacy_blobs = match &blobs {
        Some(location) => BucketLocation::parse(location, s3)?,
        None => config.bucket.clone(),
    };
    let store = ControlPlane::open(&config.store, &keys).await?;
    let report = moved(&store, &config.bucket, &legacy_blobs, &config).await;
    store.close().await;
    report
}

/// The embedded directory: `journal.db` becomes `control.db`, the tests move under `tests/`,
/// the runs move, and the tenant-keyed blobs go once every run moved.
async fn embedded(server: ServerArgs) -> Result<Report, StartupError> {
    let dir = server.dir.clone();
    let directory = Directory::open(&dir)?;
    let io = |what: String, error: std::io::Error| StartupError::Io(format!("{what}: {error}"));
    let journal = dir.join("journal.db");
    let control = dir.join("control.db");
    if journal.exists() {
        if control.exists() {
            return Err(StartupError::Setup(format!(
                "{} holds both journal.db and control.db: keep the one to serve",
                dir.display()
            )));
        }
        for suffix in ["", "-wal", "-shm"] {
            let from = PathBuf::from(format!("{}{suffix}", journal.display()));
            let to = PathBuf::from(format!("{}{suffix}", control.display()));
            if from.exists() {
                std::fs::rename(&from, &to)
                    .map_err(|error| io(format!("cannot rename {}", from.display()), error))?;
            }
        }
    }
    move_tree(&dir.join("blobs").join("tests"), &dir.join("tests"))
        .map_err(|error| io(format!("cannot move the tests of {}", dir.display()), error))?;
    let config = server.resolve()?;
    let keys = MasterKeyRing::new(directory.master_key);
    let store = ControlPlane::open(&config.store, &keys).await?;
    let legacy_blobs = BucketLocation::Files(dir.join("blobs"));
    let report = moved(&store, &config.bucket, &legacy_blobs, &config).await;
    store.close().await;
    let report = report?;
    if report.dropped {
        rewind_blob::LegacyFiles::new(dir.join("blobs"))
            .remove()
            .map_err(|error| StartupError::Io(error.to_string()))?;
    }
    Ok(report)
}

/// Places the namespaces, then moves the runs.
async fn moved(
    store: &ControlPlane,
    bucket: &BucketLocation,
    legacy_blobs: &BucketLocation,
    config: &crate::config::Config,
) -> Result<Report, StartupError> {
    let legacy = Legacy::of(store)?;
    let placed = store
        .place_namespaces(config.partitions)
        .await
        .map_err(|error| StartupError::Setup(error.to_string()))?;
    if placed > 0 {
        tracing::info!(placed, "namespaces placed in their partitions");
    }
    let opened = bucket::open(bucket)?;
    let legacy_blobs = LegacyBlobStore::open(legacy_blobs)?;
    let catalog = store.catalog();
    Migration {
        catalog: &catalog,
        partitions: &opened.partitions,
        archive: &opened.archive,
        blobs: &opened.blobs,
        legacy: &legacy,
        legacy_blobs: &legacy_blobs,
        partition_count: config.partitions,
        node: config.node.clone(),
        now: SystemClock.now(),
    }
    .run()
    .await
    .map_err(|error| StartupError::Setup(error.to_string()))
}

/// Moves what `from` holds under `to`, file by file, keeping what `to` already holds; then
/// removes `from`. Nothing happens when `from` does not exist.
fn move_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(from) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    std::fs::create_dir_all(to)?;
    for entry in entries {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            move_tree(&entry.path(), &target)?;
        } else if !target.exists() {
            std::fs::rename(entry.path(), &target)?;
        }
    }
    std::fs::remove_dir_all(from)
}

/// The set-aside runs of the control plane's backend.
enum Legacy {
    Postgres(PostgresLegacy),
    Turso(TursoLegacy),
}

impl Legacy {
    fn of(store: &ControlPlane) -> Result<Self, StartupError> {
        match store {
            ControlPlane::Postgres(store) => Ok(Self::Postgres(store.legacy())),
            ControlPlane::Turso(store) => Ok(Self::Turso(store.legacy())),
            ControlPlane::Memory(_) => Err(StartupError::Config(
                "the in-memory control plane holds no runs to move".into(),
            )),
        }
    }
}

impl LegacyRuns for Legacy {
    async fn runs(
        &self,
        after: Option<RunId>,
        limit: usize,
    ) -> Result<Vec<LegacyRun>, LegacyError> {
        match self {
            Self::Postgres(legacy) => legacy.runs(after, limit).await,
            Self::Turso(legacy) => legacy.runs(after, limit).await,
        }
    }

    async fn drop_tables(&self) -> Result<(), LegacyError> {
        match self {
            Self::Postgres(legacy) => legacy.drop_tables().await,
            Self::Turso(legacy) => legacy.drop_tables().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_takes_the_servers_options_and_the_blobs() {
        let args = MigrateArgs::try_parse_from([
            "rewind server migrate-runs",
            "--store",
            "postgres://localhost/rewind",
            "--bucket",
            "s3://runs",
            "--blobs",
            "s3://blobs",
        ])
        .unwrap();
        assert_eq!(args.blobs.as_deref(), Some("s3://blobs"));
        assert_eq!(args.server.bucket.as_deref(), Some("s3://runs"));
        let embedded =
            MigrateArgs::try_parse_from(["rewind server migrate-runs", "--embedded"]).unwrap();
        assert!(embedded.server.embedded);
    }

    #[test]
    fn a_tree_moves_without_replacing_what_is_there() {
        let temp = tempfile::tempdir().unwrap();
        let (from, to) = (temp.path().join("blobs/tests"), temp.path().join("tests"));
        std::fs::create_dir_all(from.join("local/sha256/ab")).unwrap();
        std::fs::write(from.join("local/sha256/ab/abcd"), "moved").unwrap();
        std::fs::write(from.join("local/sha256/ab/kept"), "from").unwrap();
        std::fs::create_dir_all(to.join("local/sha256/ab")).unwrap();
        std::fs::write(to.join("local/sha256/ab/kept"), "to").unwrap();
        move_tree(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(
            std::fs::read_to_string(to.join("local/sha256/ab/abcd")).unwrap(),
            "moved"
        );
        assert_eq!(
            std::fs::read_to_string(to.join("local/sha256/ab/kept")).unwrap(),
            "to"
        );
        move_tree(&from, &to).unwrap();
    }
}
