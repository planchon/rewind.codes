//! `BlobStore` on local files: the embedded server's `.rewind/blobs/`.

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rewind_core::Digest;
use rewind_engine::ports::{BlobError, BlobStore, TenantId};

use crate::{Error, verify};

/// The directory of staging files, next to the tenants' directories. A tenant id never starts
/// with a dot, so it cannot collide with a tenant.
const STAGING: &str = ".staging";

/// A staging file older than this was left by a crash: no put takes that long.
const STALE_STAGING: Duration = Duration::from_secs(60 * 60);

/// `BlobStore` on local files.
///
/// A blob lives at `{root}/{tenant}/sha256/{first two hex digits}/{hex digest}`; the embedded
/// server's root is `.rewind/blobs`. Tenant ids are case-sensitive and file systems may not be
/// (macOS, Windows), so a capital letter of a tenant id is written `+` and the letter in
/// lowercase: `Acme` lives in `+acme`, apart from `acme`. A put writes a staging file in `{root}/.staging`, syncs it,
/// then renames it to its path without replacing an existing blob, and syncs the directory: a
/// blob is either absent or complete, and stays on disk once `put` returns. `open` removes the
/// staging files a crash left behind.
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

    /// The path of a tenant's blob: `{root}/{tenant}/sha256/{ab}/{abcdef…}`, the tenant's
    /// capital letters escaped.
    pub fn path(&self, tenant: &TenantId, digest: Digest) -> PathBuf {
        let hex = digest.to_hex();
        self.root
            .join(tenant_directory(tenant))
            .join("sha256")
            .join(&hex[..2])
            .join(hex)
    }

    fn staging(&self) -> PathBuf {
        self.root.join(STAGING)
    }
}

impl BlobStore for FileBlobs {
    async fn put(
        &self,
        tenant: &TenantId,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        let path = self.path(tenant, digest);
        let staging = self.staging();
        blocking(move || {
            verify(digest, &bytes, "bytes")?;
            write(&staging, &path, &bytes).map_err(|error| failed("write", &path, &error))
        })
        .await
    }

    async fn get(&self, tenant: &TenantId, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        let path = self.path(tenant, digest);
        blocking(move || match fs::read(&path) {
            Ok(bytes) => {
                verify(digest, &bytes, "stored bytes")?;
                Ok(Some(bytes))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(failed("read", &path, &error)),
        })
        .await
    }

    async fn delete(&self, tenant: &TenantId, digest: Digest) -> Result<bool, BlobError> {
        let path = self.path(tenant, digest);
        blocking(move || remove(&path).map_err(|error| failed("delete", &path, &error))).await
    }
}

/// The directory of a tenant: its id, each capital letter written `+` and the letter in
/// lowercase. Two tenants never share a directory, even on a file system that ignores case: the
/// name holds no capital letter, and a tenant id holds no `+`.
fn tenant_directory(tenant: &TenantId) -> String {
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

/// Writes a blob atomically and durably, unless it is already there.
fn write(staging: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
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

/// Creates a directory and its missing parents, syncing each parent that got a new entry.
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

/// Makes the entries of a directory durable (a created, renamed or removed file).
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
async fn blocking<T: Send + 'static>(
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
