//! `BlobStore` on local files: the embedded server's `.rewind/`.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rewind_core::Digest;
use rewind_engine::ports::{BlobError, BlobOwner, BlobStore, RunRef, TenantId};

use crate::{Error, verify};

/// The directory of staging files, next to `blobs` and `tests`. Its name starts with a dot, so
/// it cannot collide with them.
const STAGING: &str = ".staging";

/// A staging file older than this was left by a crash: no put takes that long.
const STALE_STAGING: Duration = Duration::from_secs(60 * 60);

/// `BlobStore` on local files.
///
/// A blob lives under the keys of the bucket (`archi/06-storage-and-scale.md`, "The
/// archive and the blobs"), as paths under the root, the first two hex digits a directory of
/// their own (`archi/07-code-architecture.md`, "Embedded server and CLI"): a run's at
/// `{root}/blobs/{tenant}/{run}/sha256/{ab}/{hex digest}`, a tenant's test at
/// `{root}/tests/{tenant}/sha256/{ab}/{hex digest}`. Tenant ids are case-sensitive and file systems
/// may not be (macOS, Windows), so a capital letter of a tenant id is written `+` and the
/// letter in lowercase: `Acme` lives in `+acme`, apart from `acme`. A put writes a staging file
/// in `{root}/.staging`, syncs it, then renames it to its path without replacing an existing
/// blob, and syncs the directory: a blob is either absent or complete, and stays on disk once
/// `put` returns. A copy is a hard link, made without replacing an existing blob. `open`
/// removes the staging files a crash left behind.
///
/// File operations run on Tokio's blocking threads, so the store must be used inside a Tokio
/// runtime.
#[derive(Debug, Clone)]
pub struct FileBlobs {
    root: PathBuf,
}

impl FileBlobs {
    /// Opens the blob directory `root`, creating it if needed. Blocks while it removes the
    /// staging files older than an hour.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        let staging = root.join(STAGING);
        let failed = |source| Error::Directory {
            path: root.clone(),
            source,
        };
        fs::create_dir_all(&staging).map_err(failed)?;
        remove_stale_staging(&staging, SystemTime::now()).map_err(failed)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path of an owner's blob: `{root}/blobs/{tenant}/{run}/sha256/{ab}/{abcdef…}` or
    /// `{root}/tests/{tenant}/sha256/{ab}/{abcdef…}`, the tenant's capital letters escaped.
    pub fn path(&self, owner: &BlobOwner, digest: Digest) -> PathBuf {
        let directory = match owner {
            BlobOwner::Run(run) => self.run_directory(run),
            BlobOwner::Tests(tenant) => self.root.join("tests").join(tenant_directory(tenant)),
        };
        let hex = digest.to_hex();
        directory.join("sha256").join(&hex[..2]).join(hex)
    }

    /// The directory of a run's blobs: `{root}/blobs/{tenant}/{run}`.
    fn run_directory(&self, run: &RunRef) -> PathBuf {
        self.root
            .join("blobs")
            .join(tenant_directory(&run.tenant))
            .join(run.id.to_string())
    }

    pub(crate) fn staging(&self) -> PathBuf {
        self.root.join(STAGING)
    }
}

impl BlobStore for FileBlobs {
    async fn put(
        &self,
        owner: &BlobOwner,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        let path = self.path(owner, digest);
        let staging = self.staging();
        blocking(move || {
            verify(digest, &bytes, "bytes")?;
            write(&staging, &path, &bytes).map_err(|error| failed("write", &path, &error))
        })
        .await
    }

    async fn get(&self, owner: &BlobOwner, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        let path = self.path(owner, digest);
        blocking(move || read(digest, &path)).await
    }

    async fn copy(
        &self,
        from: &BlobOwner,
        to: &BlobOwner,
        digest: Digest,
    ) -> Result<(), BlobError> {
        let (source, target) = (self.path(from, digest), self.path(to, digest));
        blocking(move || match link(&source, &target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound && !source.exists() => {
                Err(BlobError::Missing(digest))
            }
            Err(error) => Err(failed("copy", &target, &error)),
        })
        .await
    }

    async fn delete_run(&self, run: &RunRef) -> Result<(), BlobError> {
        let directory = self.run_directory(run);
        blocking(move || {
            remove_directory(&directory).map_err(|error| failed("delete", &directory, &error))
        })
        .await
    }

    async fn delete(&self, owner: &BlobOwner, digest: Digest) -> Result<bool, BlobError> {
        let path = self.path(owner, digest);
        blocking(move || remove(&path).map_err(|error| failed("delete", &path, &error))).await
    }
}

/// Reads a blob and checks its digest.
pub(crate) fn read(digest: Digest, path: &Path) -> Result<Option<Vec<u8>>, BlobError> {
    match fs::read(path) {
        Ok(bytes) => {
            verify(digest, &bytes, "stored bytes")?;
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(failed("read", path, &error)),
    }
}

/// Links a blob to a second path, durably, unless a blob is already there: a blob is immutable,
/// so both paths name the same bytes for good.
fn link(source: &Path, target: &Path) -> io::Result<()> {
    if target.try_exists()? {
        return Ok(());
    }
    let dir = parent(target)?;
    create_dirs(dir)?;
    match fs::hard_link(source, target) {
        Ok(()) => sync_dir(dir),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// Removes a directory and everything under it, durably; a missing directory is not an error.
pub(crate) fn remove_directory(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => sync_dir(parent(dir)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// The directory of a tenant: its id, each capital letter written `+` and the letter in
/// lowercase. Two tenants never share a directory, even on a file system that ignores case: the
/// name holds no capital letter, and a tenant id holds no `+`.
pub(crate) fn tenant_directory(tenant: &TenantId) -> String {
    let mut directory = String::with_capacity(tenant.as_str().len());
    for letter in tenant.as_str().chars() {
        if letter.is_ascii_uppercase() {
            directory.push('+');
            directory.push(letter.to_ascii_lowercase());
        } else {
            directory.push(letter);
        }
    }
    directory
}

/// Writes a file atomically and durably, unless it is already there.
pub(crate) fn write(staging: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    if path.try_exists()? {
        // Blobs are immutable: a second put changes nothing.
        return Ok(());
    }
    let dir = parent(path)?;
    create_dirs(dir)?;
    let mut staged = tempfile::Builder::new()
        .prefix("blob-")
        .tempfile_in(staging)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(path) {
        Ok(_) => {}
        // A concurrent put of the same digest won: the bytes are the same. Dropping the error
        // removes the staging file.
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error.error),
    }
    sync_dir(dir)
}

/// Writes `bytes` at `path` in place of what it holds, through a staging file: what
/// `rename-data` does to a sealed attempt, the one object written twice.
pub(crate) fn replace(staging: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = parent(path)?;
    create_dirs(dir)?;
    let mut staged = tempfile::Builder::new()
        .prefix("blob-")
        .tempfile_in(staging)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|error| error.error)?;
    sync_dir(dir)
}

/// Removes a blob; returns whether it existed.
fn remove(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => {
            // Durable, so that a cleaned-up blob does not come back after a crash, when the
            // store has already forgotten it.
            sync_dir(parent(path)?)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Creates a directory and its missing parents, syncing each parent that got a new event.
fn create_dirs(dir: &Path) -> io::Result<()> {
    if dir.try_exists()? {
        return Ok(());
    }
    let parent = parent(dir)?;
    create_dirs(parent)?;
    match fs::create_dir(dir) {
        Ok(()) => sync_dir(parent),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| io::Error::other(format!("{} has no parent", path.display())))
}

/// Makes the events of a directory durable (a created, renamed or removed file).
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Removes the staging files not modified for [`STALE_STAGING`].
fn remove_stale_staging(staging: &Path, now: SystemTime) -> io::Result<()> {
    for entry in fs::read_dir(staging)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let age = now.duration_since(metadata.modified()?).unwrap_or_default();
        if metadata.is_file() && age >= STALE_STAGING {
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn failed(action: &str, path: &Path, error: &io::Error) -> BlobError {
    BlobError::Backend(format!("cannot {action} {}: {error}", path.display()))
}

/// Runs file I/O on Tokio's blocking threads.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, BlobError> + Send + 'static,
) -> Result<T, BlobError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| BlobError::Backend(format!("the blob file task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_directories_hold_no_capital_letter() {
        let directory = |id: &str| tenant_directory(&TenantId::new(id).expect("tenant"));
        assert_eq!(directory("local"), "local");
        assert_eq!(directory("acme-2_x"), "acme-2_x");
        assert_eq!(directory("Acme"), "+acme");
        assert_eq!(directory("ACME"), "+a+c+m+e");
        assert_ne!(directory("Acme"), directory("acme"));
    }

    #[test]
    fn stale_staging_files_are_removed_and_fresh_ones_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale = dir.path().join("blob-stale");
        let fresh = dir.path().join("blob-fresh");
        fs::write(&stale, b"left by a crash").expect("write");
        fs::write(&fresh, b"a put in progress").expect("write");
        let now = SystemTime::now();
        File::options()
            .write(true)
            .open(&stale)
            .expect("open")
            .set_modified(now - STALE_STAGING - Duration::from_secs(1))
            .expect("set_modified");

        remove_stale_staging(dir.path(), now).expect("remove");

        assert!(!stale.exists());
        assert!(fresh.exists());
    }
}
