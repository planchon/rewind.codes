//! The production store of Rewind on Postgres (`archi/06-storage-and-scale.md`, "Backends";
//! `archi/07-code-architecture.md`, "Ports").
//!
//! [`PostgresStore`] implements four ports of `rewind-engine` on one database:
//!
//! - `JournalStore`: scripts, toolsets, runs, histories, inboxes, timers, tool tasks, leases
//!   and fencing, quarantine, the feed, webhook tokens and subscriptions, tenants, namespaces
//!   with their quotas, use and alerts, payload rows and references, retention. The starts of a
//!   tenant and the writes of its use take a transaction-scoped advisory lock on the tenant: two
//!   starts never take the same last place of a quota;
//! - `AccessStore`: API keys, the tenants' own roles, grants and default roles, end users'
//!   tokens and token keys. The writes of a tenant's grants and roles take a transaction-scoped
//!   advisory lock on the tenant: the kept role's last holder and a role in use are decided
//!   against concurrent writers;
//! - `ReadModel`: the projections behind the dashboard's searches, statistics and failure
//!   groups: run summaries with their labels, waits, search text and quarantine, tool calls with
//!   their tasks' states;
//! - `SecretStore`: credentials per toolset, per namespace and toolset, and per run, encrypted
//!   with a data key per tenant, itself encrypted by the deployment's [`MasterKey`].
//!
//! Many workers, schedulers and projectors share one database. Each port method is one short
//! transaction at `READ COMMITTED`, and the rows they compete for are taken with
//! `FOR UPDATE SKIP LOCKED` (runnable runs, due tool tasks, feed items, quota alerts,
//! quarantined runs) or
//! locked in a fixed order: a run's row first, then the rows it owns, payload rows by digest.
//! A deadlock or a serialization failure, should one happen anyway, is retried.
//!
//! History and inbox rows hold the `rewind-format` v1 JSON of their event. The schema is versioned:
//! its migrations are embedded in the binary and applied when the store connects, under an
//! advisory lock so that servers starting together migrate once; a database migrated by a newer
//! build is refused ([`Error::NewerSchema`]).

#![forbid(unsafe_code)]

mod access;
mod codec;

mod journal;
mod namespaces;
mod read_model;
mod schema;
mod secrets;
mod sql;

use std::str::FromStr;
use std::time::Duration;

use sqlx::ConnectOptions as _;
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub use schema::SCHEMA_VERSION;
pub use secrets::MasterKey;

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
}

impl From<sqlx::Error> for Error {
    fn from(err: sqlx::Error) -> Self {
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

/// The store of a production deployment: `JournalStore`, `AccessStore`, `ReadModel` and
/// `SecretStore` on one Postgres database. Cloning it is cheap, and the clones share its pool of connections.
#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
    master_key: MasterKey,
}

impl std::fmt::Debug for PostgresStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStore").finish_non_exhaustive()
    }
}

impl PostgresStore {
    /// Connects to the database and brings its schema to [`SCHEMA_VERSION`]. A database with a
    /// newer schema is refused.
    pub async fn connect(config: &Config, master_key: MasterKey) -> Result<Self, Error> {
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
        schema::migrate(&pool).await?;
        Ok(Self { pool, master_key })
    }

    /// The schema version of the database: always [`SCHEMA_VERSION`] once connected.
    pub async fn schema_version(&self) -> Result<i64, Error> {
        Ok(schema::applied_version(&self.pool).await?.unwrap_or(0))
    }

    /// Closes the pool's connections, waiting for the calls in progress to end.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
