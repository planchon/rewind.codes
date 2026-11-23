//! The embedded server of a `.rewind/` directory (`archi/07-code-architecture.md`, "Embedded
//! server and CLI"):
//!
//! ```text
//! .rewind/
//!   server.lock    held by the server: a second server fails to take it and exits; the lock
//!                  goes with the process, `kill -9` included
//!   server.sock    the API /v1, mode 0600, no key, tenant `local`
//!   control.db     Turso: the control plane; `meta` keeps the schema version and the virtual time
//!   master.key     the master key of the secret store, mode 0600, made at the first start
//!   partitions/0/  SlateDB: the one partition's write-ahead log, tables and manifest
//!   runs/          the sealed attempts, by tenant and run
//!   blobs/         payloads above 64 KiB, under their run
//!   tests/         test documents, by tenant
//! ```
//!
//! One node owns the one partition, with the node link inside the process.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use fs4::fs_std::FileExt;

use crate::config::parse_master_key;
use crate::serve::StartupError;

/// The directory of an embedded server, locked for as long as this value lives.
#[derive(Debug)]
pub(crate) struct Directory {
    pub(crate) socket: PathBuf,
    pub(crate) master_key: [u8; 32],
    /// Holds the exclusive lock on `server.lock`.
    _lock: File,
}

impl Directory {
    /// Creates the directory if needed (mode 0700), takes its lock, and reads or makes its
    /// master key. A stale socket left by a server that died is removed: the lock proves no
    /// server listens on it.
    pub(crate) fn open(dir: &Path) -> Result<Self, StartupError> {
        let io = |what: &str, error: std::io::Error| {
            StartupError::Io(format!("{what} {}: {error}", dir.display()))
        };
        std::fs::create_dir_all(dir).map_err(|error| io("cannot create", error))?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("server.lock"))
            .map_err(|error| io("cannot open the lock of", error))?;
        match FileExt::try_lock_exclusive(&lock) {
            Ok(true) => {}
            Ok(false) => return Err(StartupError::Locked(dir.to_owned())),
            Err(error) => return Err(io("cannot lock", error)),
        }
        let socket = dir.join("server.sock");
        // `sockaddr_un` holds 104 bytes on macOS, 108 on Linux, the final NUL included.
        if socket.as_os_str().len() > 103 {
            return Err(StartupError::Config(format!(
                "{} is too long for a unix socket ({} bytes, 103 at most): use a shorter --dir",
                socket.display(),
                socket.as_os_str().len()
            )));
        }
        match std::fs::remove_file(&socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io("cannot remove the stale socket of", error)),
        }
        let master_key = master_key(&dir.join("master.key"))?;
        Ok(Self {
            socket,
            master_key,
            _lock: lock,
        })
    }
}

/// Tells the command that started this server that it listens (`--notify-ready`): `ready` and a
/// newline on standard output, the pipe the CLI gave it, then standard output goes where standard
/// error goes, so that nothing written later reaches a pipe nobody reads any more
/// (`archi/07-code-architecture.md`, "Embedded server and CLI").
pub(crate) fn notify_ready() -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(b"ready\n")?;
    stdout.flush()?;
    rustix::stdio::dup2_stdout(std::io::stderr()).map_err(std::io::Error::from)
}

/// The master key kept in `path`, made at the first start.
fn master_key(path: &Path) -> Result<[u8; 32], StartupError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            parse_master_key(&text).map_err(|error| StartupError::Config(error.to_string()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut key = [0u8; 32];
            getrandom::fill(&mut key).map_err(|error| StartupError::Io(error.to_string()))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|error| StartupError::Io(format!("{}: {error}", path.display())))?;
            writeln!(file, "{}", BASE64.encode(key))
                .and_then(|()| file.sync_all())
                .map_err(|error| StartupError::Io(format!("{}: {error}", path.display())))?;
            Ok(key)
        }
        Err(error) => Err(StartupError::Io(format!("{}: {error}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_is_held_by_one_server() {
        let temp = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let dir = temp.path().join(".rewind");
        let first = Directory::open(&dir).unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            Directory::open(&dir),
            Err(StartupError::Locked(_))
        ));
        let key = first.master_key;
        drop(first);
        let again = Directory::open(&dir).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(again.master_key, key, "the master key is kept");
        let mode = std::fs::metadata(dir.join("master.key"))
            .map(|meta| meta.permissions().mode() & 0o777)
            .unwrap_or_default();
        assert_eq!(mode, 0o600);
    }
}
