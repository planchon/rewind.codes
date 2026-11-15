//! The schema, versioned: migrations are embedded in the binary and applied by sqlx's migrator
//! when the store connects, each in its own transaction, under an advisory lock so that servers
//! starting together migrate once. The migrator records each migration with its checksum in
//! `_sqlx_migrations`, and refuses a migration changed since it was applied.
//!
//! A database whose latest migration is above [`SCHEMA_VERSION`] was migrated by a newer build,
//! and is refused rather than misread.

use std::future::Future;
use std::pin::Pin;

use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migration, MigrationSource, MigrationType, Migrator};

use crate::Error;

/// The version of the schema this build writes: its latest migration.
pub const SCHEMA_VERSION: i64 = 4;

/// Migration `n` brings the schema to version `n`. A migration, once released, never changes: a
/// change of the schema is a new one.
const MIGRATIONS: [(i64, &str, &str); SCHEMA_VERSION as usize] = [
    (1, "schema", include_str!("../migrations/0001_schema.sql")),
    (2, "access", include_str!("../migrations/0002_access.sql")),
    (
        3,
        "workflows",
        include_str!("../migrations/0003_workflows.sql"),
    ),
    (
        4,
        "run workflows",
        include_str!("../migrations/0004_run_workflows.sql"),
    ),
];

/// The migrations embedded in this build.
#[derive(Debug)]
struct Embedded;

impl<'s> MigrationSource<'s> for Embedded {
    fn resolve(
        self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<Migration>, Box<dyn std::error::Error + Send + Sync>>>
                + Send
                + 's,
        >,
    > {
        Box::pin(async {
            Ok(MIGRATIONS
                .iter()
                .map(|(version, description, sql)| {
                    Migration::new(
                        *version,
                        (*description).into(),
                        MigrationType::Simple,
                        (*sql).into(),
                        false,
                    )
                })
                .collect())
        })
    }
}

/// Brings the database's schema to [`SCHEMA_VERSION`], or refuses a newer one.
pub(crate) async fn migrate(pool: &PgPool) -> Result<(), Error> {
    let newer = |found| Error::NewerSchema {
        found,
        supported: SCHEMA_VERSION,
    };
    if let Some(found) = applied_version(pool).await?
        && found > SCHEMA_VERSION
    {
        return Err(newer(found));
    }
    let migrator = Migrator::new(Embedded)
        .await
        .map_err(|err| Error::Migration(err.to_string()))?;
    migrator.run(pool).await.map_err(|err| match err {
        // Another server migrated the database further while this one was starting.
        MigrateError::VersionMissing(found) => newer(found),
        other => Error::Migration(other.to_string()),
    })?;
    tracing::debug!(
        version = SCHEMA_VERSION,
        "the Postgres schema is up to date"
    );
    Ok(())
}

/// The latest migration applied to the database; `None` for a database never migrated.
pub(crate) async fn applied_version(pool: &PgPool) -> Result<Option<i64>, Error> {
    let migrated: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(pool)
        .await?;
    if !migrated {
        return Ok(None);
    }
    let version: Option<i64> =
        sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
            .fetch_one(pool)
            .await?;
    Ok(version)
}
