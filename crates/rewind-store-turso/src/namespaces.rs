//! The rows of tenants and namespaces the catalog reads and writes (`crate::catalog`): a
//! tenant's retention, its ceiling and its namespaces' defaults, the quotas of either as six
//! columns, the namespaces' display names and those being erased
//! (`archi/06-storage-and-scale.md`, "Quotas", "Namespaces").

use rewind_engine::ports::{DisplayName, NamespaceName, Quotas, Retention, Tenant, TenantId};
use turso::{Connection, Row, Value};

use crate::Error;
use crate::sql::{self, Columns, int, limit, ms, tenant};

/// The six columns of a [`Quotas`], in this order, prefixed (`default_` for the defaults).
pub(crate) fn quota_columns(prefix: &str) -> String {
    [
        "cpu_per_month_ms",
        "active_runs",
        "runs_per_month",
        "workflows",
        "max_ttl_ms",
        "max_code_lines",
    ]
    .map(|column| format!("{prefix}{column}"))
    .join(", ")
}

pub(crate) fn quotas_from_row(row: &Row, first: usize) -> Result<Quotas, Error> {
    Ok(Quotas {
        cpu_per_month: row.opt_duration(first)?,
        active_runs: row.opt_u32(first + 1)?,
        runs_per_month: row.opt_u64(first + 2)?,
        workflows: row.opt_u64(first + 3)?,
        max_ttl: row.opt_duration(first + 4)?,
        max_code_lines: row.opt_u32(first + 5)?,
    })
}

pub(crate) fn quota_values(quotas: &Quotas) -> [Value; 6] {
    let count = |value: Option<u64>| value.map_or(Value::Null, int);
    [
        quotas.cpu_per_month.map_or(Value::Null, ms),
        count(quotas.active_runs.map(u64::from)),
        count(quotas.runs_per_month),
        count(quotas.workflows),
        quotas.max_ttl.map_or(Value::Null, ms),
        count(quotas.max_code_lines.map(u64::from)),
    ]
}

// -------------------------------------------------------------------------------------------
// Tenants

/// The columns of a [`Tenant`], read by [`tenant_from_row`]. The ceiling's `active_runs` is the
/// column of version 1, `active_run_quota`.
fn tenant_columns() -> String {
    format!(
        "id, retention_ms, cpu_per_month_ms, active_run_quota, runs_per_month, workflows, \
         max_ttl_ms, max_code_lines, {}",
        quota_columns("default_")
    )
}

fn tenant_from_row(row: &Row) -> Result<Tenant, Error> {
    Ok(Tenant {
        id: row.tenant(0)?,
        retention: Retention {
            runs: row.duration(1)?,
        },
        quotas: quotas_from_row(row, 2)?,
        namespace_defaults: quotas_from_row(row, 8)?,
    })
}

pub(crate) async fn put_tenant(connection: &Connection, record: &Tenant) -> Result<(), Error> {
    let mut params = vec![tenant(&record.id), ms(record.retention.runs)];
    params.extend(quota_values(&record.quotas));
    params.extend(quota_values(&record.namespace_defaults));
    sql::execute(
        connection,
        &format!(
            "INSERT OR REPLACE INTO tenants ({}) VALUES ({})",
            tenant_columns(),
            sql::placeholders(14)
        ),
        params,
    )
    .await?;
    Ok(())
}

pub(crate) async fn get_tenant(
    connection: &Connection,
    id: &TenantId,
) -> Result<Option<Tenant>, Error> {
    let row = sql::query_row(
        connection,
        &format!("SELECT {} FROM tenants WHERE id = ?", tenant_columns()),
        vec![tenant(id)],
    )
    .await?;
    row.map(|row| tenant_from_row(&row)).transpose()
}

pub(crate) async fn list_tenants(connection: &Connection) -> Result<Vec<Tenant>, Error> {
    let rows = sql::query(
        connection,
        &format!("SELECT {} FROM tenants ORDER BY id", tenant_columns()),
        Vec::new(),
    )
    .await?;
    rows.iter().map(tenant_from_row).collect()
}

// -------------------------------------------------------------------------------------------
// Namespaces

pub(crate) fn display_name_of(text: String) -> Result<DisplayName, Error> {
    DisplayName::new(text.clone())
        .map_err(|_| Error::Corrupt(format!("invalid display name {text:?}")))
}

pub(crate) async fn display_names(
    connection: &Connection,
    tenant_id: &TenantId,
) -> Result<Vec<(NamespaceName, DisplayName)>, Error> {
    sql::query(
        connection,
        "SELECT name, display_name FROM namespaces \
         WHERE tenant = ? AND display_name IS NOT NULL ORDER BY name",
        vec![tenant(tenant_id)],
    )
    .await?
    .iter()
    .map(|row| {
        let name = row.text(0)?;
        Ok((
            NamespaceName::new(name.clone())
                .map_err(|_| Error::Corrupt(format!("invalid namespace {name:?}")))?,
            display_name_of(row.text(1)?)?,
        ))
    })
    .collect()
}

pub(crate) async fn namespaces_to_erase(
    connection: &Connection,
    max: usize,
) -> Result<Vec<(TenantId, NamespaceName)>, Error> {
    let rows = sql::query(
        connection,
        "SELECT tenant, name FROM namespaces WHERE deleting_since IS NOT NULL \
         ORDER BY deleting_since, tenant, name LIMIT ?",
        vec![limit(max)],
    )
    .await?;
    rows.iter()
        .map(|row| {
            let name = row.text(1)?;
            Ok((
                row.tenant(0)?,
                NamespaceName::new(name.clone())
                    .map_err(|_| Error::Corrupt(format!("invalid namespace {name:?}")))?,
            ))
        })
        .collect()
}
