//! The embedded store of Rewind on Turso Database (`archi/06-storage-and-scale.md`, "Backends";
//! `archi/07-code-architecture.md`, "Embedded server and CLI").
//!
//! The embedded control plane: [`TursoCatalog`] (`CatalogStore`, [`TursoStore::catalog`]) and
//! [`TursoCluster`] (`Cluster`: its members live in the process, its partition map in the
//! database), with access and secrets below. Every write of the catalog, of access and of
//! secrets sends what it changed once it committed, through a broadcast inside the process
//! ([`TursoStore::changes`]).
//!
//! [`TursoStore`] implements four ports of `rewind-engine` on one database file, `journal.db`:
//!
//! - `JournalStore`: scripts and their titles, toolsets, workflow versions and their numbers,
//!   runs with their handles and their watchers, histories, inboxes, timers, tool tasks, leases
//!   and fencing, quarantine, the feed, webhook tokens and subscriptions, tenants, namespaces
//!   with their quotas, use and alerts, payload rows and references, test rows and the suites of
//!   workflow versions, retention;
//! - `AccessStore`: API keys, the tenants' own roles, grants and default roles, end users'
//!   tokens and token keys;
//! - `ReadModel`: the projections behind the dashboard and the query language
//!   (`run_summaries` with each run's snapshot, `run_labels`, `run_waits`, `run_texts`,
//!   `tool_calls`), with the task states and quarantine recorded by whoever changes them
//!   (`tool_calls.task_*`, `run_quarantine`);
//! - `SecretStore`: named secrets of the organization and of each namespace, their reads per
//!   day, and each tenant's Ed25519 signing keys; values and private keys encrypted with a data
//!   key per tenant, itself encrypted by one of the deployment's [`MasterKeys`], whose id it
//!   records.
//!
//! History and inbox rows hold the `rewind-format` v1 JSON of their event. The `meta` table holds
//! the schema version and the server's settings, such as virtual time.
//!
//! Only the embedded server opens the database (07): Turso stays in its single-process mode.
//! Inside that process every call goes through one connection, one call at a time, so each
//! port method runs alone, as in the in-memory reference adapters. Writes are transactions:
//! a call that fails or is dropped half-way writes nothing.
//!
//! The schema is versioned. Its migrations are embedded in the binary and applied when the
//! database is opened; a database written by a newer build is refused
//! ([`Error::NewerSchema`]).

#![forbid(unsafe_code)]

mod access;
mod catalog;
mod cluster;
mod codec;
mod connections;
mod journal;
mod namespaces;
mod read_model;
mod schema;
mod secrets;
mod sql;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rewind_core::Timestamp;
use rewind_engine::adapters::ControlPlaneChanges;
use rewind_engine::ports::ControlPlaneChange;
use tokio::sync::{Mutex, MutexGuard};
use turso::Connection;

pub use catalog::TursoCatalog;
pub use cluster::{TursoCluster, TursoMembership};
pub use schema::SCHEMA_VERSION;
pub use secrets::{MasterKey, MasterKeys};

/// The errors of this crate. The port methods convert them into the port's own error type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The database file was written by a newer build: this build does not know its schema.
    #[error(
        "the database has schema version {found}, newer than this build's {supported}: \
         upgrade Rewind to open it"
    )]
    NewerSchema { found: u32, supported: u32 },
    /// The path of the database file is not valid UTF-8, which Turso requires.
    #[error("the database path {0:?} is not valid UTF-8")]
    InvalidPath(PathBuf),
    /// A setting name reserved by the store.
    #[error("the setting {0:?} is reserved by the store")]
    ReservedSetting(String),
    /// The operating system's random generator failed: no key or nonce can be drawn.
    #[error("the operating system's random generator failed: {0}")]
    Random(String),
    /// A stored value this build cannot read.
    #[error("corrupt stored value: {0}")]
    Corrupt(String),
    /// An error of Turso Database.
    #[error("Turso database error: {0}")]
    Database(String),
}

impl From<turso::Error> for Error {
    fn from(err: turso::Error) -> Self {
        Error::Database(err.to_string())
    }
}

/// The store of an embedded server: `JournalStore`, `AccessStore`, `ReadModel` and `SecretStore`
/// on one Turso database. Cloning it is cheap, and the clones share the database.
#[derive(Clone)]
pub struct TursoStore {
    inner: Arc<Inner>,
}

struct Inner {
    /// Kept open for as long as the store lives.
    _database: turso::Database,
    /// The one connection. Every call holds it for its whole duration: calls never interleave
    /// inside a transaction.
    connection: Mutex<Connection>,
    master_keys: MasterKeys,
    /// Where every write of the control plane sends what it changed.
    changes: ControlPlaneChanges,
}

impl std::fmt::Debug for TursoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TursoStore").finish_non_exhaustive()
    }
}

/// How long a statement waits for a lock held by another connection before it fails. Only the
/// embedded server opens the database, so this only covers a second process opening it by
/// mistake.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The `meta` key of the virtual time setting.
const VIRTUAL_TIME: &str = "virtual_time";

impl TursoStore {
    /// Opens the database at `path`, creating it if needed, and brings its schema to
    /// [`SCHEMA_VERSION`]. A database with a newer schema is refused. `master_keys` is the
    /// current master key, which encrypts, and the previous ones, which only decrypt (a
    /// [`MasterKey`] alone has none).
    pub async fn open(
        path: impl AsRef<Path>,
        master_keys: impl Into<MasterKeys>,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        let utf8 = path
            .to_str()
            .ok_or_else(|| Error::InvalidPath(path.to_owned()))?;
        let database = turso::Builder::new_local(utf8).build().await?;
        let connection = database.connect()?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        schema::migrate(&connection).await?;
        Ok(Self {
            inner: Arc::new(Inner {
                _database: database,
                connection: Mutex::new(connection),
                master_keys: master_keys.into(),
                changes: ControlPlaneChanges::new(),
            }),
        })
    }

    /// The schema version of the open database: always [`SCHEMA_VERSION`] once opened.
    pub async fn schema_version(&self) -> Result<u32, Error> {
        let connection = self.connection().await?;
        schema::version(&connection).await
    }

    /// A setting of the server, kept in the `meta` table.
    pub async fn setting(&self, name: &str) -> Result<Option<String>, Error> {
        schema::check_setting_name(name)?;
        let connection = self.connection().await?;
        schema::meta(&connection, name).await
    }

    /// Sets or, with `None`, removes a setting of the server.
    pub async fn set_setting(&self, name: &str, value: Option<&str>) -> Result<(), Error> {
        schema::check_setting_name(name)?;
        let connection = self.connection().await?;
        schema::set_meta(&connection, name, value).await
    }

    /// The server's virtual time, when it runs in virtual time: where its clock stood when it
    /// last saved it.
    pub async fn virtual_time(&self) -> Result<Option<Timestamp>, Error> {
        match self.setting(VIRTUAL_TIME).await? {
            None => Ok(None),
            Some(value) => value
                .parse()
                .map(|ms| Some(Timestamp(ms)))
                .map_err(|_| Error::Corrupt(format!("virtual time {value:?}"))),
        }
    }

    /// Saves the server's virtual time, or removes it with `None`: the server then runs in real
    /// time.
    pub async fn set_virtual_time(&self, at: Option<Timestamp>) -> Result<(), Error> {
        let value = at.map(|at| at.as_millis().to_string());
        self.set_setting(VIRTUAL_TIME, value.as_deref()).await
    }

    /// The changes of the control plane this store sends: the catalog's, access's and secrets',
    /// and the partition map's of its [`TursoCluster`]. Every member of that cluster streams
    /// them.
    pub fn changes(&self) -> &ControlPlaneChanges {
        &self.inner.changes
    }

    /// Sends what a write changed, once it committed. The caller still holds the connection,
    /// so changes leave in the order their writes committed.
    fn send(&self, changes: impl IntoIterator<Item = ControlPlaneChange>) {
        for change in changes {
            self.inner.changes.send(change);
        }
    }

    /// The connection, for one call. A transaction left open by a call that was dropped
    /// half-way is rolled back first: it never committed.
    async fn connection(&self) -> Result<MutexGuard<'_, Connection>, Error> {
        let connection = self.inner.connection.lock().await;
        if !connection.is_autocommit()? {
            sql::execute(&connection, "ROLLBACK", Vec::new()).await?;
        }
        Ok(connection)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_transaction_left_open_by_a_dropped_call_is_rolled_back() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let store = TursoStore::open(
            dir.path().join("journal.db"),
            MasterKey::from_bytes([1; 32]),
        )
        .await
        .expect("open");
        {
            // A call dropped between its writes and its commit leaves this behind.
            let connection = store.inner.connection.lock().await;
            sql::execute(&connection, "BEGIN IMMEDIATE", Vec::new())
                .await
                .expect("begin");
            sql::execute(
                &connection,
                "INSERT INTO meta (key, value) VALUES ('half', 'written')",
                Vec::new(),
            )
            .await
            .expect("insert");
        }
        assert_eq!(store.setting("half").await, Ok(None), "never committed");
        store
            .set_setting("next", Some("call"))
            .await
            .expect("the next call works");
        assert_eq!(store.setting("next").await, Ok(Some("call".to_owned())));
    }
}
