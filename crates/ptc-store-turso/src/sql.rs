//! Statements, transactions, and the conversions between domain values and columns.
//!
//! Times are milliseconds since the Unix epoch and durations milliseconds, in `INTEGER`
//! columns; a value past `i64::MAX` milliseconds (292 million years) is stored as `i64::MAX`.
//! Digests are lowercase hex and run ids hyphenated lowercase UUIDs: their text order is their
//! byte order, the order of the domain types.

use std::future::Future;
use std::time::Duration;

use ptc_core::{Digest, RunId, Timestamp};
use ptc_engine::ports::{TenantId, TenantScope};
use turso::{Connection, Row, Value};

use crate::Error;

pub(crate) type Params = Vec<Value>;

/// Runs a statement and returns every row. The rows are always read to the end: a statement
/// left half-read keeps the connection busy.
pub(crate) async fn query(
    connection: &Connection,
    sql: &str,
    params: Params,
) -> Result<Vec<Row>, Error> {
    let mut statement = connection.prepare_cached(sql).await?;
    let mut rows = statement.query(params).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(row);
    }
    Ok(out)
}

/// The first row of a statement, if any.
pub(crate) async fn query_row(
    connection: &Connection,
    sql: &str,
    params: Params,
) -> Result<Option<Row>, Error> {
    Ok(query(connection, sql, params).await?.into_iter().next())
}

/// Runs a statement whose text varies (lists of values): it is not kept in the statement cache.
pub(crate) async fn query_dynamic(
    connection: &Connection,
    sql: &str,
    params: Params,
) -> Result<Vec<Row>, Error> {
    let mut statement = connection.prepare(sql).await?;
    let mut rows = statement.query(params).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        out.push(row);
    }
    Ok(out)
}

/// Runs a statement that returns no rows, and returns how many rows it changed.
pub(crate) async fn execute(
    connection: &Connection,
    sql: &str,
    params: Params,
) -> Result<u64, Error> {
    let mut statement = connection.prepare_cached(sql).await?;
    Ok(statement.execute(params).await?)
}

/// [`execute`] for a statement whose text varies.
pub(crate) async fn execute_dynamic(
    connection: &Connection,
    sql: &str,
    params: Params,
) -> Result<u64, Error> {
    let mut statement = connection.prepare(sql).await?;
    Ok(statement.execute(params).await?)
}

/// Runs `body` in a write transaction: committed if it succeeds, rolled back if it fails. If
/// the caller drops the future half-way, the next call rolls the transaction back
/// (`TursoStore::connection`).
pub(crate) async fn transaction<T, E>(
    connection: &Connection,
    body: impl Future<Output = Result<T, E>>,
) -> Result<T, E>
where
    E: From<Error>,
{
    execute(connection, "BEGIN IMMEDIATE", Vec::new()).await?;
    match body.await {
        Ok(value) => {
            if let Err(err) = execute(connection, "COMMIT", Vec::new()).await {
                rollback(connection).await;
                return Err(err.into());
            }
            Ok(value)
        }
        Err(err) => {
            rollback(connection).await;
            Err(err)
        }
    }
}

async fn rollback(connection: &Connection) {
    if let Err(err) = execute(connection, "ROLLBACK", Vec::new()).await {
        // The next call rolls the transaction back before it starts.
        tracing::warn!(error = %err, "rolling back a failed transaction failed");
    }
}

/// `column IN (…)` or `column NOT IN (…)` for the tenants of a scope.
pub(crate) fn scope_clause(column: &str, scope: &TenantScope, params: &mut Vec<Value>) -> String {
    let (negated, tenants) = match scope {
        TenantScope::All => return "1 = 1".to_owned(),
        TenantScope::Only(tenants) => (false, tenants),
        TenantScope::Except(tenants) => (true, tenants),
    };
    if tenants.is_empty() {
        return if negated { "1 = 1" } else { "0 = 1" }.to_owned();
    }
    params.extend(tenants.iter().map(tenant));
    let not = if negated { "NOT " } else { "" };
    format!("{column} {not}IN ({})", placeholders(tenants.len()))
}

/// `?, ?, ?` for `count` values.
pub(crate) fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

// -------------------------------------------------------------------------------------------
// Values

pub(crate) fn int(value: u64) -> Value {
    Value::Integer(i64::try_from(value).unwrap_or(i64::MAX))
}

pub(crate) fn ts(at: Timestamp) -> Value {
    int(at.as_millis())
}

pub(crate) fn opt_ts(at: Option<Timestamp>) -> Value {
    at.map_or(Value::Null, ts)
}

pub(crate) fn ms(duration: Duration) -> Value {
    Value::Integer(duration_ms(duration))
}

pub(crate) fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

pub(crate) fn text(value: impl Into<String>) -> Value {
    Value::Text(value.into())
}

pub(crate) fn opt_text(value: Option<impl Into<String>>) -> Value {
    value.map_or(Value::Null, |value| Value::Text(value.into()))
}

pub(crate) fn digest(digest: Digest) -> Value {
    Value::Text(digest.to_hex())
}

pub(crate) fn run_id(id: RunId) -> Value {
    Value::Text(id.to_string())
}

pub(crate) fn tenant(tenant: &TenantId) -> Value {
    Value::Text(tenant.as_str().to_owned())
}

pub(crate) fn boolean(value: bool) -> Value {
    Value::Integer(i64::from(value))
}

/// `limit` for a `LIMIT` clause.
pub(crate) fn limit(limit: usize) -> Value {
    Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX))
}

/// Typed reads of a row's columns. A value of an unexpected type is a corrupt row.
pub(crate) trait Columns {
    fn column(&self, index: usize) -> Result<Value, Error>;

    fn opt_text(&self, index: usize) -> Result<Option<String>, Error> {
        match self.column(index)? {
            Value::Null => Ok(None),
            Value::Text(text) => Ok(Some(text)),
            other => Err(unexpected(index, "text", &other)),
        }
    }

    fn text(&self, index: usize) -> Result<String, Error> {
        self.opt_text(index)?.ok_or_else(|| null(index))
    }

    fn opt_int(&self, index: usize) -> Result<Option<i64>, Error> {
        match self.column(index)? {
            Value::Null => Ok(None),
            Value::Integer(value) => Ok(Some(value)),
            other => Err(unexpected(index, "an integer", &other)),
        }
    }

    fn int(&self, index: usize) -> Result<i64, Error> {
        self.opt_int(index)?.ok_or_else(|| null(index))
    }

    fn opt_u64(&self, index: usize) -> Result<Option<u64>, Error> {
        self.opt_int(index)?
            .map(|value| {
                u64::try_from(value).map_err(|_| Error::Corrupt(format!("negative value {value}")))
            })
            .transpose()
    }

    fn u64(&self, index: usize) -> Result<u64, Error> {
        self.opt_u64(index)?.ok_or_else(|| null(index))
    }

    fn u32(&self, index: usize) -> Result<u32, Error> {
        let value = self.u64(index)?;
        u32::try_from(value).map_err(|_| Error::Corrupt(format!("{value} is out of range")))
    }

    fn opt_u32(&self, index: usize) -> Result<Option<u32>, Error> {
        self.opt_u64(index)?
            .map(|value| {
                u32::try_from(value).map_err(|_| Error::Corrupt(format!("{value} is out of range")))
            })
            .transpose()
    }

    fn boolean(&self, index: usize) -> Result<bool, Error> {
        Ok(self.int(index)? != 0)
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
        parse_digest(&self.text(index)?)
    }

    fn opt_digest(&self, index: usize) -> Result<Option<Digest>, Error> {
        self.opt_text(index)?
            .map(|hex| parse_digest(&hex))
            .transpose()
    }

    fn run_id(&self, index: usize) -> Result<RunId, Error> {
        parse_run_id(&self.text(index)?)
    }

    fn opt_run_id(&self, index: usize) -> Result<Option<RunId>, Error> {
        self.opt_text(index)?
            .map(|id| parse_run_id(&id))
            .transpose()
    }

    fn tenant(&self, index: usize) -> Result<TenantId, Error> {
        let id = self.text(index)?;
        TenantId::new(id).map_err(|err| Error::Corrupt(err.to_string()))
    }

    fn blob(&self, index: usize) -> Result<Vec<u8>, Error> {
        match self.column(index)? {
            Value::Blob(bytes) => Ok(bytes),
            other => Err(unexpected(index, "a blob", &other)),
        }
    }
}

impl Columns for Row {
    fn column(&self, index: usize) -> Result<Value, Error> {
        Ok(self.get_value(index)?)
    }
}

pub(crate) fn parse_digest(hex: &str) -> Result<Digest, Error> {
    Digest::from_hex(hex).map_err(|err| Error::Corrupt(err.to_string()))
}

pub(crate) fn parse_run_id(id: &str) -> Result<RunId, Error> {
    id.parse::<RunId>()
        .map_err(|err| Error::Corrupt(err.to_string()))
}

fn null(index: usize) -> Error {
    Error::Corrupt(format!("column {index} is unexpectedly null"))
}

fn unexpected(index: usize, expected: &str, found: &Value) -> Error {
    Error::Corrupt(format!(
        "column {index}: expected {expected}, found {found:?}"
    ))
}
