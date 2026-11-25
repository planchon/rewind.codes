//! The production store of Rewind on Postgres (`archi/06-storage-and-scale.md`, "Backends";
//! `archi/07-code-architecture.md`, "Ports").
//!
//! The control plane: [`PostgresCatalog`] (`CatalogStore`, and `CatalogMigration` for
//! `rename-data`; [`PostgresStore::catalog`]) and [`PostgresCluster`] (`Cluster`: the nodes,
//! their session advisory locks and the ring they watch, the partition map, the changes of the
//! control plane through `LISTEN`), with access and secrets below. Every write of the catalog, of access and of secrets sends what it changed with
//! `NOTIFY` in its transaction; the payloads name what changed, never a value
//! (`crate::changes`). A new version of a workflow locks the workflow's row: two versions never
//! take the same number.
//!
//! [`PostgresStore`] implements, on the same database:
//!
//! - `AccessStore`: API keys, the tenants' own roles, grants and default roles, end users'
//!   tokens and token keys, the connections of Rewind's MCP server and their OAuth clients and
//!   codes. The writes of a tenant's grants and roles take a transaction-scoped advisory lock on
//!   the tenant: the kept role's last holder and a role in use are decided against concurrent
//!   writers;
//! - `SecretStore`: named secrets of the organization and of each namespace, their reads per
//!   day, and each tenant's Ed25519 signing keys; values and private keys encrypted with a data
//!   key per tenant, itself encrypted by one of the deployment's [`MasterKeys`], whose id it
//!   records.
//!
//! The nodes of a deployment and its edges share one database. Each port method is one short
//! transaction at `READ COMMITTED`, and the rows it competes for are locked in a fixed order; a
//! deadlock or a serialization failure, should one happen anyway, is retried.
//!
//! [`PostgresLegacy`] ([`PostgresStore::legacy`]) reads the runs migration 15 set aside in the
//! schema `legacy`, for `rewind server migrate-runs`, and drops them once they moved.
//!
//! The schema is versioned: its migrations are embedded in the binary and applied when the store
//! connects, under an advisory lock so that servers starting together migrate once; a database
//! migrated by a newer build is refused ([`Error::NewerSchema`]). A store connected as it is
//! ([`PostgresStore::connect_as_is`]) leaves the schema as it found it until it is asked to
//! migrate: `rewind server rename-data` checks the deployment first.

#![forbid(unsafe_code)]

mod access;
mod catalog;
mod changes;
mod cluster;
mod codec;
mod connections;
mod legacy;
mod namespaces;
mod schema;
mod secrets;
mod sql;

use std::str::FromStr;
use std::time::Duration;

use sqlx::ConnectOptions as _;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub use catalog::PostgresCatalog;
pub use cluster::{PostgresChanges, PostgresCluster, PostgresMembership};
pub use legacy::PostgresLegacy;
pub use schema::SCHEMA_VERSION;
pub use secrets::{MasterKey, MasterKeys};

/// The errors of this crate. The port methods convert them into the port's own error type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The database was migrated by a newer build: this build does not know its schema.
    #[error(
        "the database has schema version {found}, newer than this build's {supported}: \
         upgrade Rewind to open it"
    )]
    NewerSchema { found: i64, supported: i64 },
    /// The connection string is not one Postgres accepts.
    #[error("invalid Postgres connection string: {0}")]
    InvalidUrl(String),
    /// A migration could not be applied, or an applied one was changed since.
    #[error("migrating the schema failed: {0}")]
    Migration(String),
    /// The operating system's random generator failed: no key, nonce or token can be drawn.
    #[error("the operating system's random generator failed: {0}")]
    Random(String),
    /// A stored value this build cannot read.
    #[error("corrupt stored value: {0}")]
    Corrupt(String),
    /// An error of Postgres or of the connection to it, with its SQLSTATE code when Postgres
    /// gave one.
    #[error("Postgres error: {message}")]
    Database {
        code: Option<String>,
        message: String,
    },
    /// Postgres cannot be reached: the connection failed or was lost, or the pool had no
    /// connection to give in time. The call may be tried again.
    #[error("Postgres cannot be reached: {0}")]
    Unreachable(String),
}

impl From<sqlx::Error> for Error {
    fn from(err: sqlx::Error) -> Self {
        if matches!(
            err,
            sqlx::Error::Io(_)
                | sqlx::Error::Tls(_)
                | sqlx::Error::PoolTimedOut
                | sqlx::Error::PoolClosed
                | sqlx::Error::WorkerCrashed
        ) {
            return Error::Unreachable(err.to_string());
        }
        let code = err
            .as_database_error()
            .and_then(|db| db.code())
            .map(|code| code.into_owned());
        Error::Database {
            code,
            message: err.to_string(),
        }
    }
}

impl Error {
    /// A deadlock or a serialization failure: the transaction can be run again.
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(self, Error::Database { code: Some(code), .. } if code == "40P01" || code == "40001")
    }

    /// A unique constraint refused a row.
    pub(crate) fn is_unique_violation(&self) -> bool {
        matches!(self, Error::Database { code: Some(code), .. } if code == "23505")
    }
}

/// How to reach the database, and how many connections a store keeps.
#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// A Postgres connection string: `postgres://user:password@host:5432/database?sslmode=…`.
    pub url: String,
    /// The most connections the store's pool opens. 16 by default.
    pub max_connections: u32,
    /// How long a call waits for a free connection before it fails. 30 s by default.
    pub acquire_timeout: Duration,
}

impl Config {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            max_connections: 16,
            acquire_timeout: Duration::from_secs(30),
        }
    }
}

impl std::fmt::Debug for Config {
    /// The connection string may hold a password: it is not printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("url", &"<redacted>")
            .field("max_connections", &self.max_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .finish()
    }
}

/// The control plane of a production deployment on one Postgres database: `AccessStore` and
/// `SecretStore`, its catalog ([`PostgresStore::catalog`]) and its cluster
/// ([`PostgresCluster::open`]). Cloning it is cheap, and the clones share its pool of
/// connections.
#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
    master_keys: MasterKeys,
}

impl std::fmt::Debug for PostgresStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStore").finish_non_exhaustive()
    }
}

impl PostgresStore {
    /// Connects to the database and brings its schema to [`SCHEMA_VERSION`]. A database with a
    /// newer schema is refused. `master_keys` is the current master key, which encrypts, and the
    /// previous ones, which only decrypt (a [`MasterKey`] alone has none).
    pub async fn connect(
        config: &Config,
        master_keys: impl Into<MasterKeys>,
    ) -> Result<Self, Error> {
        let store = Self::connect_as_is(config, master_keys).await?;
        store.migrate_to(SCHEMA_VERSION).await?;
        Ok(store)
    }

    /// Connects to the database and leaves its schema as it is, whatever its version: connecting
    /// writes nothing. What reads any schema works on it ([`schema_version`](Self::schema_version),
    /// [`PostgresCluster::alive`], [`PostgresLegacy::holds_runs`]: the checks `rewind server
    /// rename-data` makes before it migrates); everything else expects [`SCHEMA_VERSION`], which
    /// [`migrate_to`](Self::migrate_to) brings.
    pub async fn connect_as_is(
        config: &Config,
        master_keys: impl Into<MasterKeys>,
    ) -> Result<Self, Error> {
        let master_keys = master_keys.into();
        let options = PgConnectOptions::from_str(&config.url)
            .map_err(|err| Error::InvalidUrl(err.to_string()))?
            // Statements are the store's business; slow ones are still reported.
            .disable_statement_logging()
            .application_name("rewind");
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections.max(1))
            .acquire_timeout(config.acquire_timeout)
            .connect_with(options)
            .await?;
        Ok(Self { pool, master_keys })
    }

    /// Brings the schema to `version`, at most [`SCHEMA_VERSION`], applying the migrations it
    /// lacks in order; a database whose schema is newer than this build's is refused. A test
    /// builds the schema an older build left with a `version` below this build's.
    pub async fn migrate_to(&self, version: i64) -> Result<(), Error> {
        schema::migrate(&self.pool, version).await
    }

    /// The schema version of the database, 0 for a database never migrated: always
    /// [`SCHEMA_VERSION`] once [`connect`](Self::connect) returned.
    pub async fn schema_version(&self) -> Result<i64, Error> {
        Ok(schema::applied_version(&self.pool).await?.unwrap_or(0))
    }

    /// Closes the pool's connections, waiting for the calls in progress to end.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
