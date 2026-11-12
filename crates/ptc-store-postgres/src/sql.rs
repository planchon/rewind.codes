//! Conversions between domain values and columns, typed reads of rows, and the retry of
//! transactions.
//!
//! Times are milliseconds since the Unix epoch and durations milliseconds, in `bigint` columns;
//! a value past `i64::MAX` milliseconds (292 million years) is stored as `i64::MAX`. Digests are
//! their 32 bytes and run ids `uuid`: their order in Postgres is their byte order, the order of
//! the domain types.

use std::future::Future;
use std::time::Duration;

use ptc_core::{Digest, RunId, Timestamp};
use ptc_engine::ports::{TenantId, TenantScope};
use sqlx::Row as _;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::Error;

/// How many times a transaction that hit a deadlock or a serialization failure is run again.
const RETRIES: u32 = 8;

/// What a step of a port method fails with: the port's own error, or the database.
#[derive(Debug)]
pub(crate) enum Failure<E> {
    Port(E),
    Db(Error),
}

impl<E> From<Error> for Failure<E> {
    fn from(err: Error) -> Self {
        Failure::Db(err)
    }
}

impl<E> From<sqlx::Error> for Failure<E> {
    fn from(err: sqlx::Error) -> Self {
        Failure::Db(err.into())
    }
}

/// Runs `attempt` until it does not fail with a deadlock or a serialization failure, at most
/// [`RETRIES`] more times. Each attempt is a whole transaction: a failed one wrote nothing.
pub(crate) async fn retrying<T, E, F, Fut>(mut attempt: F) -> Result<T, Failure<E>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Failure<E>>>,
{
    let mut retries = 0;
    loop {
        match attempt().await {
            Err(Failure::Db(err)) if err.is_retryable() && retries < RETRIES => {
                retries += 1;
                tracing::debug!(error = %err, retries, "retrying a transaction");
            }
            outcome => return outcome,
        }
    }
}

/// `AND column = ANY($n)` or `AND NOT (column = ANY($n))` for the tenants of a scope, with the
/// tenants to bind as `$n`; nothing for every tenant.
pub(crate) fn scope_clause(
    column: &str,
    scope: &TenantScope,
    n: usize,
) -> (String, Option<Vec<String>>) {
    let (negated, tenants) = match scope {
        TenantScope::All => return (String::new(), None),
        TenantScope::Only(tenants) => (false, tenants),
        TenantScope::Except(tenants) => (true, tenants),
    };
    let tenants: Vec<String> = tenants.iter().map(|t| t.as_str().to_owned()).collect();
    let clause = if negated {
        format!(" AND NOT ({column} = ANY(${n}))")
    } else {
        format!(" AND {column} = ANY(${n})")
    };
    (clause, Some(tenants))
}

// -------------------------------------------------------------------------------------------
// Values

/// A count or index as stored.
pub(crate) fn int(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// A time as stored.
pub(crate) fn ts(at: Timestamp) -> i64 {
    int(at.as_millis())
}

pub(crate) fn opt_ts(at: Option<Timestamp>) -> Option<i64> {
    at.map(ts)
}

/// A duration as stored.
pub(crate) fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

pub(crate) fn bytes(digest: Digest) -> Vec<u8> {
    digest.as_bytes().to_vec()
}

pub(crate) fn uuid(id: RunId) -> Uuid {
    id.as_uuid()
}

pub(crate) fn tenant(tenant: &TenantId) -> &str {
    tenant.as_str()
}

/// `limit` for a `LIMIT` clause.
pub(crate) fn limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

pub(crate) fn parse_digest(bytes: &[u8]) -> Result<Digest, Error> {
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| Error::Corrupt(format!("a digest of {} bytes", bytes.len())))?;
    Ok(Digest::from_bytes(bytes))
}

fn unsigned(value: i64) -> Result<u64, Error> {
    u64::try_from(value).map_err(|_| Error::Corrupt(format!("negative value {value}")))
}

fn narrow(value: u64) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| Error::Corrupt(format!("{value} is out of range")))
}

/// Typed reads of a row's columns. A value of an unexpected type is a corrupt row.
pub(crate) trait Columns {
    fn opt_i64(&self, index: usize) -> Result<Option<i64>, Error>;
    fn opt_text(&self, index: usize) -> Result<Option<String>, Error>;
    fn opt_bytes(&self, index: usize) -> Result<Option<Vec<u8>>, Error>;
    fn opt_uuid(&self, index: usize) -> Result<Option<Uuid>, Error>;
    fn opt_bool(&self, index: usize) -> Result<Option<bool>, Error>;

    fn i64(&self, index: usize) -> Result<i64, Error> {
        self.opt_i64(index)?.ok_or_else(|| null(index))
    }

    fn text(&self, index: usize) -> Result<String, Error> {
        self.opt_text(index)?.ok_or_else(|| null(index))
    }

    fn bytes(&self, index: usize) -> Result<Vec<u8>, Error> {
        self.opt_bytes(index)?.ok_or_else(|| null(index))
    }

    fn boolean(&self, index: usize) -> Result<bool, Error> {
        self.opt_bool(index)?.ok_or_else(|| null(index))
    }

    fn opt_u64(&self, index: usize) -> Result<Option<u64>, Error> {
        self.opt_i64(index)?.map(unsigned).transpose()
    }

    fn u64(&self, index: usize) -> Result<u64, Error> {
        unsigned(self.i64(index)?)
    }

    fn u32(&self, index: usize) -> Result<u32, Error> {
        narrow(self.u64(index)?)
    }

    fn opt_u32(&self, index: usize) -> Result<Option<u32>, Error> {
        self.opt_u64(index)?.map(narrow).transpose()
    }

    fn ts(&self, index: usize) -> Result<Timestamp, Error> {
        self.u64(index).map(Timestamp)
    }

    fn opt_ts(&self, index: usize) -> Result<Option<Timestamp>, Error> {
        Ok(self.opt_u64(index)?.map(Timestamp))
    }

    fn duration(&self, index: usize) -> Result<Duration, Error> {
        self.u64(index).map(Duration::from_millis)
    }

    fn opt_duration(&self, index: usize) -> Result<Option<Duration>, Error> {
        Ok(self.opt_u64(index)?.map(Duration::from_millis))
    }

    fn digest(&self, index: usize) -> Result<Digest, Error> {
        parse_digest(&self.bytes(index)?)
    }

    fn opt_digest(&self, index: usize) -> Result<Option<Digest>, Error> {
        self.opt_bytes(index)?
            .map(|bytes| parse_digest(&bytes))
            .transpose()
    }

    fn run_id(&self, index: usize) -> Result<RunId, Error> {
        self.opt_run_id(index)?.ok_or_else(|| null(index))
    }

    fn opt_run_id(&self, index: usize) -> Result<Option<RunId>, Error> {
        Ok(self.opt_uuid(index)?.map(RunId::from_uuid))
    }

    fn tenant(&self, index: usize) -> Result<TenantId, Error> {
        TenantId::new(self.text(index)?).map_err(|err| Error::Corrupt(err.to_string()))
    }
}

impl Columns for PgRow {
    fn opt_i64(&self, index: usize) -> Result<Option<i64>, Error> {
        Ok(self.try_get(index)?)
    }

    fn opt_text(&self, index: usize) -> Result<Option<String>, Error> {
        Ok(self.try_get(index)?)
    }

    fn opt_bytes(&self, index: usize) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.try_get(index)?)
    }

    fn opt_uuid(&self, index: usize) -> Result<Option<Uuid>, Error> {
        Ok(self.try_get(index)?)
    }

    fn opt_bool(&self, index: usize) -> Result<Option<bool>, Error> {
        Ok(self.try_get(index)?)
    }
}

fn null(index: usize) -> Error {
    Error::Corrupt(format!("column {index} is unexpectedly null"))
}
