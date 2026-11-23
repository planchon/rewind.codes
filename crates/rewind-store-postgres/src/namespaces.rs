//! The rows of tenants and namespaces the catalog reads and writes (`crate::catalog`): a
//! tenant's retention, its ceiling and its namespaces' defaults, the quotas of either as six
//! columns, the namespaces' display names and those being erased
//! (`archi/06-storage-and-scale.md`, "Quotas", "Namespaces").

use rewind_engine::ports::{DisplayName, NamespaceName, Quotas, Retention, Tenant, TenantId};
use sqlx::PgConnection;
use sqlx::postgres::PgRow;

use crate::Error;
use crate::sql::{Columns, int, limit, ms, tenant};

/// The six columns of a [`Quotas`], in this order, prefixed (`default_` for the defaults).
pub(crate) fn quota_columns(prefix: &str) -> String {
    [
        "cpu_per_month_ms",
        "active_runs",
        "runs_per_month",
        "scripts",
        "max_ttl_ms",
        "max_script_lines",
    ]
    .map(|column| format!("{prefix}{column}"))
    .join(", ")
}

pub(crate) fn quotas_from_row(row: &PgRow, first: usize) -> Result<Quotas, Error> {
    Ok(Quotas {
        cpu_per_month: row.opt_duration(first)?,
        active_runs: row.opt_u32(first + 1)?,
        runs_per_month: row.opt_u64(first + 2)?,
        scripts: row.opt_u64(first + 3)?,
        max_ttl: row.opt_duration(first + 4)?,
        max_script_lines: row.opt_u32(first + 5)?,
    })
}

/// The values of the six columns of a [`Quotas`].
pub(crate) fn quota_values(quotas: &Quotas) -> [Option<i64>; 6] {
    [
        quotas.cpu_per_month.map(ms),
        quotas.active_runs.map(i64::from),
        quotas.runs_per_month.map(int),
        quotas.scripts.map(int),
        quotas.max_ttl.map(ms),
        quotas.max_script_lines.map(i64::from),
    ]
}

pub(crate) fn name_of(text: String) -> Result<NamespaceName, Error> {
    NamespaceName::new(text.clone())
        .map_err(|_| Error::Corrupt(format!("invalid namespace {text:?}")))
}

pub(crate) fn display_name_of(text: String) -> Result<DisplayName, Error> {
    DisplayName::new(text.clone())
        .map_err(|_| Error::Corrupt(format!("invalid display name {text:?}")))
}

// -------------------------------------------------------------------------------------------
// Tenants

/// The columns of a [`Tenant`], read by [`tenant_from_row`]. The ceiling's `active_runs` is the
/// column of version 1, `active_run_quota`.
fn tenant_columns() -> String {
    format!(
        "id, retention_ms, cpu_per_month_ms, active_run_quota, runs_per_month, scripts, \
         max_ttl_ms, max_script_lines, {}",
        quota_columns("default_")
    )
}

fn tenant_from_row(row: &PgRow) -> Result<Tenant, Error> {
    Ok(Tenant {
        id: row.tenant(0)?,
        retention: Retention {
            runs: row.duration(1)?,
        },
        quotas: quotas_from_row(row, 2)?,
        namespace_defaults: quotas_from_row(row, 8)?,
    })
}

pub(crate) async fn put_tenant(conn: &mut PgConnection, record: &Tenant) -> Result<(), Error> {
    let columns = tenant_columns();
    let updates = columns
        .split(", ")
        .skip(1)
        .map(|column| format!("{column} = EXCLUDED.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=14)
        .map(|n| format!("${n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO tenants ({columns}) VALUES ({placeholders}) \
         ON CONFLICT (id) DO UPDATE SET {updates}"
    );
    let mut query = sqlx::query(&sql)
        .bind(tenant(&record.id))
        .bind(ms(record.retention.runs));
    for value in quota_values(&record.quotas)
        .into_iter()
        .chain(quota_values(&record.namespace_defaults))
    {
        query = query.bind(value);
    }
    query.execute(&mut *conn).await?;
    Ok(())
}

pub(crate) async fn get_tenant(
    conn: &mut PgConnection,
    id: &TenantId,
) -> Result<Option<Tenant>, Error> {
    let row = sqlx::query(&format!(
        "SELECT {} FROM tenants WHERE id = $1",
        tenant_columns()
    ))
    .bind(tenant(id))
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| tenant_from_row(&row)).transpose()
}

pub(crate) async fn list_tenants(conn: &mut PgConnection) -> Result<Vec<Tenant>, Error> {
    let rows = sqlx::query(&format!(
        "SELECT {} FROM tenants ORDER BY id",
        tenant_columns()
    ))
    .fetch_all(&mut *conn)
    .await?;
    rows.iter().map(tenant_from_row).collect()
}

// -------------------------------------------------------------------------------------------
// Namespaces

pub(crate) async fn display_names(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
) -> Result<Vec<(NamespaceName, DisplayName)>, Error> {
    let rows = sqlx::query(
        "SELECT name, display_name FROM namespaces \
         WHERE tenant = $1 AND display_name IS NOT NULL ORDER BY name",
    )
    .bind(tenant(tenant_id))
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| Ok((name_of(row.text(0)?)?, display_name_of(row.text(1)?)?)))
        .collect()
}

pub(crate) async fn namespaces_to_erase(
    conn: &mut PgConnection,
    max: usize,
) -> Result<Vec<(TenantId, NamespaceName)>, Error> {
    let rows = sqlx::query(
        "SELECT tenant, name FROM namespaces WHERE deleting_since IS NOT NULL \
         ORDER BY deleting_since, tenant, name LIMIT $1",
    )
    .bind(limit(max))
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| Ok((row.tenant(0)?, name_of(row.text(1)?)?)))
        .collect()
}
