//! The embedded server of a state directory (`archi/07-code-architecture.md`, "Embedded server
//! and CLI"): the CLI never opens `journal.db`; it is a client of `ptc server --embedded`, which
//! it starts on demand as a detached process.
//!
//! ```text
//! .ptc/server.sock answers GET /v1/server ──► same version ─────────────► use it
//!                    │                        └ other version ─► POST /v1/server/stop,
//!                    │                                           wait for the lock, start one
//!                    └ no answer ─► .ptc/server.lock free ─► start `ptc server --embedded`
//!                                   └ held: a server is starting ─► wait for the socket
//! ```
//!
//! A server holds `server.lock` for its whole life, so a free lock means no server runs: a
//! socket file left by a killed server is stale. Two commands may start a server at the same
//! time: the second server fails to take the lock and exits, and both commands use the first.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ptc_api::Client;
use ptc_api::admin::ServerInfo;
use rustix::fs::FlockOperation;

/// The version a server must have for this CLI to use it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a starting server may take to answer on its socket.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a server asked to stop may take to release its lock.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the version handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);
/// The embedded server the CLI starts stops after 10 minutes without requests and without
/// work in progress.
const IDLE_TIMEOUT: &str = "10m";
/// How many servers one command starts at most before it reports why they did not start.
const MAX_SPAWNS: usize = 3;
/// The end of the server's log quoted when it does not start.
const LOG_TAIL: u64 = 4096;

/// Finds or starts the embedded server of a state directory.
#[derive(Debug, Clone)]
pub struct Launcher {
    dir: PathBuf,
    program: PathBuf,
    program_args: Vec<String>,
    version: String,
}

/// Why the embedded server cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("the state directory {}: {source}", dir.display())]
    Dir {
        dir: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot start the embedded server ({}): {source}", program.display())]
    Spawn {
        program: PathBuf,
        source: std::io::Error,
    },
    #[error("the embedded server of {} did not start: {reason}{log}", dir.display())]
    NotStarted {
        dir: PathBuf,
        reason: String,
        log: String,
    },
    #[error(
        "the embedded server of {} (version {version}) did not stop when asked: stop it, then try again",
        dir.display()
    )]
    NotStopped { dir: PathBuf, version: String },
    #[error(transparent)]
    Client(#[from] ptc_api::Error),
}

impl Launcher {
    /// The embedded server of `dir` (`.ptc`), started by running this executable as
    /// `ptc server --embedded --dir <dir> --idle-timeout 10m`.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, LaunchError> {
        let program = std::env::current_exe().map_err(|source| LaunchError::Spawn {
            program: PathBuf::from("ptc"),
            source,
        })?;
        Ok(Self::with_program(dir, program))
    }

    /// Starts the server with another program, which takes the arguments `server --embedded
    /// --dir <dir> --idle-timeout 10m`.
    pub fn with_program(dir: impl Into<PathBuf>, program: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        Self {
            dir,
            program: program.into(),
            program_args: Vec::new(),
            version: VERSION.to_owned(),
        }
    }

    /// Arguments given to the program before `server --embedded …`.
    pub fn program_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.program_args = args.into_iter().map(Into::into).collect();
        self
    }

    /// The version the server must have; this CLI's by default.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.join("server.sock")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("server.lock")
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("server.log")
    }

    /// A client of the running server of this directory, of this CLI's version: the one that
    /// runs, or a new one.
    pub async fn connect(&self) -> Result<Client, LaunchError> {
        self.create_dir()?;
        // A server of another version is stopped, then replaced; a race with another command
        // starting one at the same time is settled by the lock. Three rounds settle any of them.
        let mut last_version = None;
        for _ in 0..3 {
            if let Some((client, info)) = self.handshake().await {
                if info.version == self.version {
                    return Ok(client);
                }
                last_version = Some(info.version.clone());
                self.stop(&client, &info).await?;
            }
            let child = if self.lock_is_free()? {
                Some(self.spawn()?)
            } else {
                None
            };
            match self.wait_for_server(child).await? {
                Some(client) => return Ok(client),
                None => continue,
            }
        }
        Err(LaunchError::NotStopped {
            dir: self.dir.clone(),
            version: last_version.unwrap_or_default(),
        })
    }

    /// Whether no server holds the lock: no server runs in this directory.
    pub fn lock_is_free(&self) -> Result<bool, LaunchError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.lock_path())
            .map_err(|source| self.dir_error(source))?;
        match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
            // Taken, so free; closing the file releases it at once.
            Ok(()) => Ok(true),
            Err(rustix::io::Errno::WOULDBLOCK) => Ok(false),
            Err(errno) => Err(self.dir_error(errno.into())),
        }
    }

    fn create_dir(&self) -> Result<(), LaunchError> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|source| self.dir_error(source))
    }

    fn dir_error(&self, source: std::io::Error) -> LaunchError {
        LaunchError::Dir {
            dir: self.dir.clone(),
            source,
        }
    }

    /// The server answering on the socket, and what it is.
    async fn handshake(&self) -> Option<(Client, ServerInfo)> {
        let socket = self.socket();
        if !socket.exists() {
            return None;
        }
        let client = Client::embedded(socket).ok()?;
        let info = tokio::time::timeout(HANDSHAKE_TIMEOUT, client.server_info())
            .await
            .ok()?
            .ok()?;
        Some((client, info))
    }

    /// Asks a server of another version to stop, and waits until it released its lock.
    async fn stop(&self, client: &Client, info: &ServerInfo) -> Result<(), LaunchError> {
        // It may stop before it answers.
        let _ = client.stop_server().await;
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            if self.lock_is_free()? {
                return Ok(());
            }
            tokio::time::sleep(POLL).await;
        }
        Err(LaunchError::NotStopped {
            dir: self.dir.clone(),
            version: info.version.clone(),
        })
    }

    /// Starts `ptc server --embedded --dir <dir>`, detached: in a process group of its own, so
    /// that a Ctrl-C in the terminal does not reach it, its output appended to `server.log`.
    fn spawn(&self) -> Result<Child, LaunchError> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.log_path())
            .map_err(|source| self.dir_error(source))?;
        let stderr = log.try_clone().map_err(|source| self.dir_error(source))?;
        Command::new(&self.program)
            .args(&self.program_args)
            .args(["server", "--embedded", "--dir"])
            .arg(&self.dir)
            // Unlike a server run in the foreground, the CLI's stops when unused.
            .args(["--idle-timeout", IDLE_TIMEOUT])
            // The runtime refuses to start unless TZ=UTC.
            .env("TZ", "UTC")
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(stderr)
            .process_group(0)
            .spawn()
            .map_err(|source| LaunchError::Spawn {
                program: self.program.clone(),
                source,
            })
    }

    /// Waits until a server answers on the socket. `None` when the one that answers is of
    /// another version (another command started it): the caller settles it.
    async fn wait_for_server(
        &self,
        mut child: Option<Child>,
    ) -> Result<Option<Client>, LaunchError> {
        let deadline = Instant::now() + START_TIMEOUT;
        let mut spawned = usize::from(child.is_some());
        let mut exited = None;
        loop {
            if let Some((client, info)) = self.handshake().await {
                return Ok((info.version == self.version).then_some(client));
            }
            if let Some(started) = child.as_mut()
                && let Ok(Some(status)) = started.try_wait()
            {
                // The server we started exited: another one took the lock first (it then
                // answers soon), or it failed.
                child = None;
                exited = Some(status);
            }
            // While our own server starts, the lock is left alone: checking it could make the
            // server miss it.
            if child.is_none() && self.lock_is_free()? {
                // Nobody holds the lock and nothing answers: start a server, a few times at
                // most (a check of another command can make a starting server miss the lock).
                if spawned == MAX_SPAWNS {
                    let reason = match exited {
                        Some(status) => format!("it exited ({status})"),
                        None => "no server took the lock".to_owned(),
                    };
                    return Err(self.not_started(reason));
                }
                child = Some(self.spawn()?);
                spawned += 1;
            }
            if Instant::now() >= deadline {
                return Err(self.not_started(format!(
                    "it did not answer on {} within {}s",
                    self.socket().display(),
                    START_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn not_started(&self, reason: String) -> LaunchError {
        let log = log_tail(&self.log_path())
            .filter(|tail| !tail.trim().is_empty())
            .map(|tail| format!("\n{}:\n{}", self.log_path().display(), tail.trim_end()))
            .unwrap_or_default();
        LaunchError::NotStarted {
            dir: self.dir.clone(),
            reason,
            log,
        }
    }
}

/// The last lines of the server's log.
fn log_tail(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(LOG_TAIL)))
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // Start at a line boundary when the tail cut one.
    Some(match (length > LOG_TAIL, text.find('\n')) {
        (true, Some(newline)) => text[newline + 1..].to_owned(),
        _ => text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_held_by_another_open_file_is_not_free() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = Launcher::with_program(dir.path(), "/nonexistent");
        launcher.create_dir().unwrap();
        assert!(launcher.lock_is_free().unwrap());
        assert!(
            launcher.lock_is_free().unwrap(),
            "checking does not keep it"
        );

        let held = File::options()
            .read(true)
            .write(true)
            .open(launcher.lock_path())
            .unwrap();
        rustix::fs::flock(&held, FlockOperation::NonBlockingLockExclusive).unwrap();
        assert!(!launcher.lock_is_free().unwrap());
        drop(held);
        assert!(launcher.lock_is_free().unwrap());
    }

    #[test]
    fn the_log_tail_starts_at_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.log");
        assert_eq!(log_tail(&path), None);
        std::fs::write(&path, "short\n").unwrap();
        assert_eq!(log_tail(&path).unwrap(), "short\n");
        let long: String = (0..2000).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&path, &long).unwrap();
        let tail = log_tail(&path).unwrap();
        assert!(tail.starts_with("line "));
        assert!(tail.ends_with("line 1999\n"));
        assert!(tail.len() <= LOG_TAIL as usize);
    }

    #[tokio::test]
    async fn a_program_that_cannot_start_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = Launcher::with_program(dir.path().join(".ptc"), "/nonexistent/ptc");
        let error = launcher.connect().await.unwrap_err();
        assert!(matches!(error, LaunchError::Spawn { .. }), "{error}");
    }
}
