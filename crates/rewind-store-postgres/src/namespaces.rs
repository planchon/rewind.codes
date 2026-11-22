//! Namespaces on Postgres: the tenant's ceiling and its namespaces' defaults, namespaces with
//! their exceptions, their use per day, the runs kept per script, and quota alerts
//! (`archi/06-storage-and-scale.md`, "Quotas"). The in-memory `MemoryJournal` of
//! `rewind-engine` is the reference of every behavior here.
//!
//! The writes that count against a quota take a transaction-scoped advisory lock on the tenant
//! first ([`lock_tenant`]): `create_run` and `record_usage` of one tenant run one after the
//! other, so two starts never take the same last place and every crossing of a threshold is
//! seen once. `create_run` then locks its namespace's row, which `delete_namespace` and
//! `drop_namespace` take too. The tenant's own rows of `usage` and `quota_alerts` have the
//! namespace [`TENANT`].

use std::collections::BTreeSet;
use std::time::Duration;

use rewind_core::{Digest, PartitionId, Timestamp};
use rewind_engine::ports::{
    CreateRun, Day, DayUsage, DeleteNamespace, DisplayName, DropNamespace, Month, Namespace,
    NamespaceFilter, NamespaceName, NamespaceOrder, NamespacePage, NamespaceUsage, NamespaceWrite,
    NewRun, QuotaAlert, QuotaAlertKey, QuotaExceeded, QuotaKind, QuotaScope, Quotas, Retention,
    Tenant, TenantId, Threshold, Usage, UsageDelta, UsageOf,
};
use sqlx::PgConnection;
use sqlx::postgres::PgRow;

use crate::Error;
use crate::sql::{Columns, bytes, int, limit, ms, tenant, ts};

/// The namespace of the tenant's own rows of use and alerts.
pub(crate) const TENANT: &str = "*";

/// The terminal statuses, for `status NOT IN …`.
macro_rules! terminal {
    () => {
        "('completed', 'failed', 'cancelled', 'aborted')"
    };
}

/// Takes the advisory lock of the tenant's quotas for the transaction. Parts are separated by
/// U+001F, which Postgres text can hold (unlike NUL) and tenant ids do not.
pub(crate) async fn lock_tenant(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
) -> Result<(), Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("rewind quotas\u{1f}{tenant_id}"))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The six columns of a [`Quotas`], in this order, prefixed (`default_` for the defaults).
fn quota_columns(prefix: &str) -> String {
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

fn quotas_from_row(row: &PgRow, first: usize) -> Result<Quotas, Error> {
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
fn quota_values(quotas: &Quotas) -> [Option<i64>; 6] {
    [
        quotas.cpu_per_month.map(ms),
        quotas.active_runs.map(i64::from),
        quotas.runs_per_month.map(int),
        quotas.scripts.map(int),
        quotas.max_ttl.map(ms),
        quotas.max_script_lines.map(i64::from),
    ]
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn name_of(text: String) -> Result<NamespaceName, Error> {
    NamespaceName::new(text.clone())
        .map_err(|_| Error::Corrupt(format!("invalid namespace {text:?}")))
}

fn display_name_of(text: String) -> Result<DisplayName, Error> {
    DisplayName::new(text.clone())
        .map_err(|_| Error::Corrupt(format!("invalid display name {text:?}")))
}

// -------------------------------------------------------------------------------------------
// Tenants

/// The columns of a [`Tenant`], read by [`tenant_from_row`]. The ceiling's `active_runs` is the
/// column of version 1, `active_run_quota`.
fn tenant_columns() -> String {
    format!(
        "id, journal_retention_ms, archive_retention_ms, read_model_retention_ms, \
         payload_grace_ms, cpu_per_month_ms, active_run_quota, runs_per_month, scripts, \
         max_ttl_ms, max_script_lines, {}",
        quota_columns("default_")
    )
}

fn tenant_from_row(row: &PgRow) -> Result<Tenant, Error> {
    Ok(Tenant {
        id: row.tenant(0)?,
        retention: Retention {
            journal: row.duration(1)?,
            archive: row.duration(2)?,
            read_model: row.duration(3)?,
            payload_grace: row.duration(4)?,
        },
        quotas: quotas_from_row(row, 5)?,
        namespace_defaults: quotas_from_row(row, 11)?,
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
    let placeholders = (1..=17)
        .map(|n| format!("${n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO tenants ({columns}) VALUES ({placeholders}) \
         ON CONFLICT (id) DO UPDATE SET {updates}"
    );
    let mut query = sqlx::query(&sql)
        .bind(tenant(&record.id))
        .bind(ms(record.retention.journal))
        .bind(ms(record.retention.archive))
        .bind(ms(record.retention.read_model))
        .bind(ms(record.retention.payload_grace));
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

/// The tenant's record, or the defaults of a tenant without one.
async fn tenant_or_default(conn: &mut PgConnection, id: &TenantId) -> Result<Tenant, Error> {
    Ok(get_tenant(conn, id)
        .await?
        .unwrap_or_else(|| Tenant::new(id.clone())))
}

// -------------------------------------------------------------------------------------------
// Namespaces

fn namespace_columns() -> String {
    format!(
        "tenant, name, {}, tokens_revoked_before, created_at, last_active_at, deleting_since, \
         display_name",
        quota_columns("")
    )
}

fn namespace_from_row(row: &PgRow) -> Result<Namespace, Error> {
    Ok(Namespace {
        tenant: row.tenant(0)?,
        name: name_of(row.text(1)?)?,
        // The journal store keeps every run of the tenant in one store: partition 0.
        partition: PartitionId::new(0),
        display_name: row.opt_text(12)?.map(display_name_of).transpose()?,
        exceptions: quotas_from_row(row, 2)?,
        tokens_revoked_before: row.opt_ts(8)?,
        created_at: row.ts(9)?,
        last_active_at: row.ts(10)?,
        deleting_since: row.opt_ts(11)?,
    })
}

pub(crate) async fn get_namespace(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
) -> Result<Option<Namespace>, Error> {
    let row = sqlx::query(&format!(
        "SELECT {} FROM namespaces WHERE tenant = $1 AND name = $2",
        namespace_columns()
    ))
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| namespace_from_row(&row)).transpose()
}

/// The namespace, created at `now` without exceptions if it does not exist, its row locked for
/// the transaction.
async fn ensure_namespace(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    now: Timestamp,
) -> Result<Namespace, Error> {
    sqlx::query(
        "INSERT INTO namespaces (tenant, name, created_at, last_active_at) VALUES ($1, $2, $3, $3) \
         ON CONFLICT (tenant, name) DO NOTHING",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    let row = sqlx::query(&format!(
        "SELECT {} FROM namespaces WHERE tenant = $1 AND name = $2 FOR UPDATE",
        namespace_columns()
    ))
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        Some(row) => namespace_from_row(&row),
        // Dropped by another transaction between the two statements: a retry creates it.
        None => Err(Error::Database {
            message: format!("namespace {name} was dropped while it was created"),
            code: Some("40001".into()),
        }),
    }
}

pub(crate) async fn put_namespace(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    exceptions: Option<&Quotas>,
    now: Timestamp,
) -> Result<NamespaceWrite, Error> {
    let namespace = ensure_namespace(conn, tenant_id, name, now).await?;
    if namespace.deleting_since.is_some() {
        return Ok(NamespaceWrite::Deleting);
    }
    let Some(exceptions) = exceptions else {
        return Ok(NamespaceWrite::Written(namespace));
    };
    let [cpu, active, runs, scripts, ttl, lines] = quota_values(exceptions);
    sqlx::query(
        "UPDATE namespaces SET cpu_per_month_ms = $3, active_runs = $4, runs_per_month = $5, \
         scripts = $6, max_ttl_ms = $7, max_script_lines = $8 WHERE tenant = $1 AND name = $2",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .bind(cpu)
    .bind(active)
    .bind(runs)
    .bind(scripts)
    .bind(ttl)
    .bind(lines)
    .execute(&mut *conn)
    .await?;
    Ok(NamespaceWrite::Written(Namespace {
        exceptions: exceptions.clone(),
        ..namespace
    }))
}

pub(crate) async fn set_display_name(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    display_name: Option<&DisplayName>,
) -> Result<Option<NamespaceWrite>, Error> {
    let row = sqlx::query(&format!(
        "SELECT {} FROM namespaces WHERE tenant = $1 AND name = $2 FOR UPDATE",
        namespace_columns()
    ))
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let namespace = namespace_from_row(&row)?;
    if namespace.deleting_since.is_some() {
        return Ok(Some(NamespaceWrite::Deleting));
    }
    sqlx::query(
        "UPDATE namespaces SET display_name = $3, display_key = $4 WHERE tenant = $1 AND name = $2",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .bind(display_name.map(DisplayName::as_str))
    .bind(display_name.map(DisplayName::key))
    .execute(&mut *conn)
    .await?;
    Ok(Some(NamespaceWrite::Written(Namespace {
        display_name: display_name.cloned(),
        ..namespace
    })))
}

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

pub(crate) async fn revoke_namespace_tokens(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    now: Timestamp,
) -> Result<bool, Error> {
    let changed = sqlx::query(
        "UPDATE namespaces SET tokens_revoked_before = $3 WHERE tenant = $1 AND name = $2",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok(changed.rows_affected() > 0)
}

pub(crate) async fn delete_namespace(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    now: Timestamp,
) -> Result<DeleteNamespace, Error> {
    let changed = sqlx::query(
        "UPDATE namespaces SET deleting_since = COALESCE(deleting_since, $3) \
         WHERE tenant = $1 AND name = $2",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok(if changed.rows_affected() > 0 {
        DeleteNamespace::Started
    } else {
        DeleteNamespace::NotFound
    })
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

pub(crate) async fn drop_namespace(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
) -> Result<DropNamespace, Error> {
    let marked: Option<i64> = sqlx::query_scalar(
        "SELECT deleting_since FROM namespaces WHERE tenant = $1 AND name = $2 \
         AND deleting_since IS NOT NULL FOR UPDATE",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    if marked.is_none() {
        return Ok(DropNamespace::NotFound);
    }
    let runs_left: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM runs WHERE tenant = $1 AND namespace = $2)",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .fetch_one(&mut *conn)
    .await?;
    if runs_left {
        return Ok(DropNamespace::RunsLeft);
    }
    sqlx::query(
        "WITH usage_gone AS (DELETE FROM usage WHERE tenant = $1 AND namespace = $2), \
         alerts_gone AS (DELETE FROM quota_alerts WHERE tenant = $1 AND namespace = $2), \
         scripts_gone AS (DELETE FROM namespace_scripts WHERE tenant = $1 AND namespace = $2) \
         DELETE FROM namespaces WHERE tenant = $1 AND name = $2",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .execute(&mut *conn)
    .await?;
    Ok(DropNamespace::Dropped)
}

pub(crate) async fn namespace_has_script(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    script: Digest,
) -> Result<bool, Error> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM namespace_scripts \
         WHERE tenant = $1 AND namespace = $2 AND digest = $3)",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .bind(bytes(script))
    .fetch_one(&mut *conn)
    .await?)
}

// -------------------------------------------------------------------------------------------
// Use

/// The days of a month, as stored: `[first, end)`.
fn days_of(month: Month) -> (i64, i64) {
    (
        i64::from(month.first_day().0),
        i64::from(month.next().first_day().0),
    )
}

/// CPU time (milliseconds) and runs created in a month, of a namespace or of the tenant
/// ([`TENANT`]).
async fn month_usage(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    of: &str,
    month: Month,
) -> Result<(u64, u64), Error> {
    let (from, to) = days_of(month);
    let row = sqlx::query(
        "SELECT COALESCE(sum(cpu_ms), 0)::bigint, COALESCE(sum(runs_started), 0)::bigint \
         FROM usage WHERE tenant = $1 AND namespace = $2 AND day >= $3 AND day < $4",
    )
    .bind(tenant(tenant_id))
    .bind(of)
    .bind(from)
    .bind(to)
    .fetch_one(&mut *conn)
    .await?;
    Ok((row.u64(0)?, row.u64(1)?))
}

/// The active runs of a namespace, or of the tenant.
async fn active_runs(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    namespace: Option<&NamespaceName>,
) -> Result<u64, Error> {
    let count: i64 = sqlx::query_scalar(concat!(
        "SELECT count(*) FROM runs WHERE tenant = $1 AND ($2::text IS NULL OR namespace = $2) \
         AND status NOT IN ",
        terminal!()
    ))
    .bind(tenant(tenant_id))
    .bind(namespace.map(NamespaceName::as_str))
    .fetch_one(&mut *conn)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

/// The distinct scripts with a run kept: of a namespace, or summed over the tenant's.
async fn scripts(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    namespace: Option<&NamespaceName>,
) -> Result<u64, Error> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM namespace_scripts \
         WHERE tenant = $1 AND ($2::text IS NULL OR namespace = $2)",
    )
    .bind(tenant(tenant_id))
    .bind(namespace.map(NamespaceName::as_str))
    .fetch_one(&mut *conn)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

pub(crate) async fn namespace_usage(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    of: &UsageOf,
    month: Month,
) -> Result<Usage, Error> {
    let namespace = match of {
        UsageOf::Namespace(name) => Some(name),
        UsageOf::Tenant => None,
    };
    let row_of = namespace.map_or(TENANT, NamespaceName::as_str);
    let (from, to) = days_of(month);
    let rows = sqlx::query(
        "SELECT day, cpu_ms, runs_started FROM usage \
         WHERE tenant = $1 AND namespace = $2 AND day >= $3 AND day < $4 ORDER BY day",
    )
    .bind(tenant(tenant_id))
    .bind(row_of)
    .bind(from)
    .bind(to)
    .fetch_all(&mut *conn)
    .await?;
    let mut usage = Usage {
        active_runs: active_runs(conn, tenant_id, namespace).await?,
        scripts: scripts(conn, tenant_id, namespace).await?,
        ..Usage::default()
    };
    for row in &rows {
        let day = Day(row.u32(0)?);
        let used = DayUsage {
            cpu: row.duration(1)?,
            runs_started: row.u64(2)?,
        };
        usage.cpu += used.cpu;
        usage.runs_started += used.runs_started;
        usage.days.push((day, used));
    }
    Ok(usage)
}

/// Adds to a day's use.
async fn add_usage(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    of: &str,
    day: Day,
    cpu_ms: u64,
    runs: u64,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO usage (tenant, namespace, day, cpu_ms, runs_started) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (tenant, namespace, day) DO UPDATE SET \
         cpu_ms = usage.cpu_ms + EXCLUDED.cpu_ms, \
         runs_started = usage.runs_started + EXCLUDED.runs_started",
    )
    .bind(tenant(tenant_id))
    .bind(of)
    .bind(i64::from(day.0))
    .bind(int(cpu_ms))
    .bind(int(runs))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn touch(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE namespaces SET last_active_at = greatest(last_active_at, $3) \
         WHERE tenant = $1 AND name = $2",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub(crate) async fn record_usage(
    conn: &mut PgConnection,
    deltas: &[UsageDelta],
    now: Timestamp,
) -> Result<Vec<QuotaAlert>, Error> {
    // The tenants' locks in a fixed order: two workers writing for the same tenants never wait
    // for each other in a cycle.
    let tenants: BTreeSet<&TenantId> = deltas.iter().map(|delta| &delta.tenant).collect();
    for tenant_id in &tenants {
        lock_tenant(conn, tenant_id).await?;
    }
    let mut raised = Vec::new();
    for delta in deltas {
        let month = Month::of_day(delta.day);
        let tenant_id = &delta.tenant;
        let record = tenant_or_default(conn, tenant_id).await?;
        let added = millis(delta.cpu);
        if let Some(namespace) = get_namespace(conn, tenant_id, &delta.namespace).await? {
            let limit = namespace
                .exceptions
                .over(&record.namespace_defaults)
                .limit(QuotaKind::CpuPerMonth);
            let name = delta.namespace.as_str();
            let (before, _) = month_usage(conn, tenant_id, name, month).await?;
            add_usage(conn, tenant_id, name, delta.day, added, 0).await?;
            touch(conn, tenant_id, &delta.namespace, now).await?;
            let crossing = Crossing {
                tenant: tenant_id,
                namespace: Some(&delta.namespace),
                quota: QuotaKind::CpuPerMonth,
                month,
                before,
                after: before.saturating_add(added),
                limit,
            };
            raised.extend(crossing.raise(conn, now).await?);
        }
        let (before, _) = month_usage(conn, tenant_id, TENANT, month).await?;
        add_usage(conn, tenant_id, TENANT, delta.day, added, 0).await?;
        let crossing = Crossing {
            tenant: tenant_id,
            namespace: None,
            quota: QuotaKind::CpuPerMonth,
            month,
            before,
            after: before.saturating_add(added),
            limit: record.quotas.limit(QuotaKind::CpuPerMonth),
        };
        raised.extend(crossing.raise(conn, now).await?);
    }
    Ok(raised)
}

// -------------------------------------------------------------------------------------------
// Creating a run: the quotas, then the counts

/// Under the transaction of `create_run`, with the tenant's lock, once the idempotency key is
/// known to be new: creates the namespace if needed and locks it, and refuses the run if the
/// namespace is being erased or a quota is reached (recording the alert of a first refusal of
/// `active_runs` in the month). `Ok`: the run may be created, then [`count_run`].
pub(crate) async fn admit_run(
    conn: &mut PgConnection,
    run: &NewRun,
) -> Result<Result<Admitted, CreateRun>, Error> {
    let tenant_id = &run.run.tenant;
    let name = &run.namespace;
    let now = run.created_at;
    let namespace = ensure_namespace(conn, tenant_id, name, now).await?;
    if namespace.deleting_since.is_some() {
        return Ok(Err(CreateRun::NamespaceDeleting));
    }
    let record = tenant_or_default(conn, tenant_id).await?;
    let limits = namespace.exceptions.over(&record.namespace_defaults);
    let ceiling = record.quotas;
    let month = Month::of(now);
    let new_script = !namespace_has_script(conn, tenant_id, name, run.started.script).await?;
    let (_, runs_of_namespace) = month_usage(conn, tenant_id, name.as_str(), month).await?;
    let (_, runs_of_tenant) = month_usage(conn, tenant_id, TENANT, month).await?;
    let scripts_of_namespace = scripts(conn, tenant_id, Some(name)).await?;
    let scripts_of_tenant = scripts(conn, tenant_id, None).await?;
    let checks = [
        (
            QuotaKind::ActiveRuns,
            QuotaScope::Namespace,
            limits.active_runs.map(u64::from),
        ),
        (
            QuotaKind::ActiveRuns,
            QuotaScope::Tenant,
            ceiling.active_runs.map(u64::from),
        ),
        (
            QuotaKind::RunsPerMonth,
            QuotaScope::Namespace,
            limits.runs_per_month,
        ),
        (
            QuotaKind::RunsPerMonth,
            QuotaScope::Tenant,
            ceiling.runs_per_month,
        ),
        (
            QuotaKind::Scripts,
            QuotaScope::Namespace,
            limits.scripts.filter(|_| new_script),
        ),
        (
            QuotaKind::Scripts,
            QuotaScope::Tenant,
            ceiling.scripts.filter(|_| new_script),
        ),
    ];
    for (quota, scope, limit) in checks {
        let Some(limit) = limit else { continue };
        let of = (scope == QuotaScope::Namespace).then_some(name);
        let used = match quota {
            QuotaKind::ActiveRuns => active_runs(conn, tenant_id, of).await?,
            QuotaKind::RunsPerMonth if of.is_some() => runs_of_namespace,
            QuotaKind::RunsPerMonth => runs_of_tenant,
            _ if of.is_some() => scripts_of_namespace,
            _ => scripts_of_tenant,
        };
        if used < limit {
            continue;
        }
        if quota == QuotaKind::ActiveRuns {
            let key = QuotaAlertKey {
                tenant: tenant_id.clone(),
                namespace: of.cloned(),
                quota,
                period: month,
                threshold: Threshold::Reached,
            };
            raise(conn, &key, used, limit, now).await?;
        }
        return Ok(Err(CreateRun::QuotaExceeded(QuotaExceeded {
            quota,
            scope,
            limit,
            used,
            resets_at: (quota == QuotaKind::RunsPerMonth).then(|| month.end()),
        })));
    }
    Ok(Ok(Admitted {
        limits,
        ceiling,
        month,
        new_script,
        runs: (runs_of_namespace, runs_of_tenant),
        scripts: (scripts_of_namespace, scripts_of_tenant),
    }))
}

/// What [`admit_run`] read, for [`count_run`].
#[derive(Debug)]
pub(crate) struct Admitted {
    limits: Quotas,
    ceiling: Quotas,
    month: Month,
    new_script: bool,
    /// Runs created in the month before this one: of the namespace, of the tenant.
    runs: (u64, u64),
    /// Distinct scripts before this run: of the namespace, of the tenant.
    scripts: (u64, u64),
}

/// Counts a created run: in the day's `runs_started` of its namespace and of its tenant, in its
/// script's row of the namespace; sets the namespace's `last_active_at`, and records the alerts
/// of `runs_per_month` and `scripts` the run crosses.
pub(crate) async fn count_run(
    conn: &mut PgConnection,
    run: &NewRun,
    admitted: Admitted,
) -> Result<(), Error> {
    let tenant_id = &run.run.tenant;
    let name = &run.namespace;
    let now = run.created_at;
    let day = Day::of(now);
    add_usage(conn, tenant_id, name.as_str(), day, 0, 1).await?;
    add_usage(conn, tenant_id, TENANT, day, 0, 1).await?;
    sqlx::query(
        "INSERT INTO namespace_scripts (tenant, namespace, digest, runs, first_run_at) \
         VALUES ($1, $2, $3, 1, $4) ON CONFLICT (tenant, namespace, digest) \
         DO UPDATE SET runs = namespace_scripts.runs + 1",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .bind(bytes(run.started.script))
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    touch(conn, tenant_id, name, now).await?;
    let Admitted {
        limits,
        ceiling,
        month,
        new_script,
        runs,
        scripts,
    } = admitted;
    let mut crossings = vec![
        Crossing {
            tenant: tenant_id,
            namespace: Some(name),
            quota: QuotaKind::RunsPerMonth,
            month,
            before: runs.0,
            after: runs.0 + 1,
            limit: limits.runs_per_month,
        },
        Crossing {
            tenant: tenant_id,
            namespace: None,
            quota: QuotaKind::RunsPerMonth,
            month,
            before: runs.1,
            after: runs.1 + 1,
            limit: ceiling.runs_per_month,
        },
    ];
    if new_script {
        crossings.extend([
            Crossing {
                tenant: tenant_id,
                namespace: Some(name),
                quota: QuotaKind::Scripts,
                month,
                before: scripts.0,
                after: scripts.0 + 1,
                limit: limits.scripts,
            },
            Crossing {
                tenant: tenant_id,
                namespace: None,
                quota: QuotaKind::Scripts,
                month,
                before: scripts.1,
                after: scripts.1 + 1,
                limit: ceiling.scripts,
            },
        ]);
    }
    for crossing in crossings {
        crossing.raise(conn, now).await?;
    }
    Ok(())
}

/// Removes an erased run from its script's count in its namespace; the count at zero is removed.
pub(crate) async fn uncount_run(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    namespace: &str,
    script: &[u8],
) -> Result<(), Error> {
    let left: Option<i64> = sqlx::query_scalar(
        "UPDATE namespace_scripts SET runs = runs - 1 \
         WHERE tenant = $1 AND namespace = $2 AND digest = $3 RETURNING runs",
    )
    .bind(tenant(tenant_id))
    .bind(namespace)
    .bind(script)
    .fetch_optional(&mut *conn)
    .await?;
    if left.is_some_and(|runs| runs <= 0) {
        sqlx::query(
            "DELETE FROM namespace_scripts WHERE tenant = $1 AND namespace = $2 AND digest = $3",
        )
        .bind(tenant(tenant_id))
        .bind(namespace)
        .bind(script)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Alerts

/// A use that went from `before` to `after` against a limit.
struct Crossing<'a> {
    tenant: &'a TenantId,
    namespace: Option<&'a NamespaceName>,
    quota: QuotaKind,
    month: Month,
    before: u64,
    after: u64,
    limit: Option<u64>,
}

impl Crossing<'_> {
    /// Records the alerts of the thresholds crossed, once each.
    async fn raise(
        self,
        conn: &mut PgConnection,
        now: Timestamp,
    ) -> Result<Vec<QuotaAlert>, Error> {
        let Some(limit) = self.limit else {
            return Ok(Vec::new());
        };
        let mut raised = Vec::new();
        for threshold in Threshold::crossed(self.before, self.after, limit) {
            let key = QuotaAlertKey {
                tenant: self.tenant.clone(),
                namespace: self.namespace.cloned(),
                quota: self.quota,
                period: self.month,
                threshold,
            };
            raised.extend(raise(conn, &key, self.after, limit, now).await?);
        }
        Ok(raised)
    }
}

/// An alert's key as stored: tenant, namespace (or `*`), quota, period, threshold.
fn key_columns(key: &QuotaAlertKey) -> (&str, &str, &'static str, String, i64) {
    (
        tenant(&key.tenant),
        key.namespace.as_ref().map_or(TENANT, NamespaceName::as_str),
        key.quota.as_str(),
        key.period.to_string(),
        i64::from(key.threshold.percent()),
    )
}

/// Records an alert unless one with its key exists. Returns it when recorded.
async fn raise(
    conn: &mut PgConnection,
    key: &QuotaAlertKey,
    used: u64,
    limit: u64,
    now: Timestamp,
) -> Result<Option<QuotaAlert>, Error> {
    let (tenant_id, namespace, quota, period, threshold) = key_columns(key);
    let inserted = sqlx::query(
        "INSERT INTO quota_alerts (tenant, namespace, quota, period, threshold, used, \
         quota_limit, raised_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
    )
    .bind(tenant_id)
    .bind(namespace)
    .bind(quota)
    .bind(period)
    .bind(threshold)
    .bind(int(used))
    .bind(int(limit))
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok((inserted.rows_affected() > 0).then(|| QuotaAlert {
        key: key.clone(),
        used,
        limit,
        raised_at: now,
        sent_at: None,
    }))
}

const ALERT_COLUMNS: &str =
    "tenant, namespace, quota, period, threshold, used, quota_limit, raised_at, sent_at";

fn alert_from_row(row: &PgRow) -> Result<QuotaAlert, Error> {
    let namespace = row.text(1)?;
    let quota = row.text(2)?;
    let period = row.text(3)?;
    let threshold = row.i64(4)?;
    Ok(QuotaAlert {
        key: QuotaAlertKey {
            tenant: row.tenant(0)?,
            namespace: (namespace != TENANT)
                .then(|| name_of(namespace))
                .transpose()?,
            quota: QuotaKind::parse(&quota)
                .ok_or_else(|| Error::Corrupt(format!("unknown quota {quota:?}")))?,
            period: Month::parse(&period)
                .ok_or_else(|| Error::Corrupt(format!("invalid month {period:?}")))?,
            threshold: match threshold {
                80 => Threshold::Near,
                100 => Threshold::Reached,
                other => return Err(Error::Corrupt(format!("unknown threshold {other}"))),
            },
        },
        used: row.u64(5)?,
        limit: row.u64(6)?,
        raised_at: row.ts(7)?,
        sent_at: row.opt_ts(8)?,
    })
}

pub(crate) async fn quota_alerts(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    namespace: Option<&NamespaceName>,
    month: Month,
) -> Result<Vec<QuotaAlert>, Error> {
    let rows = sqlx::query(&format!(
        "SELECT {ALERT_COLUMNS} FROM quota_alerts WHERE tenant = $1 AND period = $2 \
         AND ($3::text IS NULL OR namespace = $3)"
    ))
    .bind(tenant(tenant_id))
    .bind(month.to_string())
    .bind(namespace.map(NamespaceName::as_str))
    .fetch_all(&mut *conn)
    .await?;
    let mut alerts = rows
        .iter()
        .map(alert_from_row)
        .collect::<Result<Vec<_>, Error>>()?;
    alerts.sort_by(|a, b| {
        b.raised_at
            .cmp(&a.raised_at)
            .then_with(|| a.key.cmp(&b.key))
    });
    Ok(alerts)
}

/// Claims up to `max` alerts not sent and not claimed (or whose claim expired), the oldest
/// first, rows locked with `SKIP LOCKED`: two workers never claim the same alert. Alerts raised
/// at the same time come in the order of their keys.
pub(crate) async fn claim_quota_alerts(
    conn: &mut PgConnection,
    owner: &str,
    now: Timestamp,
    lease_duration: Duration,
    max: usize,
) -> Result<Vec<QuotaAlert>, Error> {
    let rows = sqlx::query(&format!(
        "UPDATE quota_alerts a SET lease_owner = $1, lease_until = $2 FROM ( \
             SELECT tenant, namespace, quota, period, threshold FROM quota_alerts \
             WHERE sent_at IS NULL AND (lease_until IS NULL OR lease_until <= $3) \
             ORDER BY raised_at, tenant, (namespace <> '*'), namespace, \
                 array_position(ARRAY['cpu_per_month', 'active_runs', 'runs_per_month', \
                     'scripts', 'max_ttl', 'max_script_lines'], quota), period, threshold \
             LIMIT $4 FOR UPDATE SKIP LOCKED) c \
         WHERE a.tenant = c.tenant AND a.namespace = c.namespace AND a.quota = c.quota \
         AND a.period = c.period AND a.threshold = c.threshold \
         RETURNING {}",
        ALERT_COLUMNS
            .split(", ")
            .map(|column| format!("a.{column}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .bind(owner)
    .bind(ts(now.saturating_add(lease_duration)))
    .bind(ts(now))
    .bind(limit(max))
    .fetch_all(&mut *conn)
    .await?;
    let mut claimed = rows
        .iter()
        .map(alert_from_row)
        .collect::<Result<Vec<_>, Error>>()?;
    claimed.sort_by(|a, b| (a.raised_at, &a.key).cmp(&(b.raised_at, &b.key)));
    Ok(claimed)
}

pub(crate) async fn ack_quota_alerts(
    conn: &mut PgConnection,
    keys: &[QuotaAlertKey],
    now: Timestamp,
) -> Result<(), Error> {
    let mut keys: Vec<_> = keys.iter().map(key_columns).collect();
    // Rows locked in the order of their keys.
    keys.sort();
    keys.dedup();
    let mut tenants = Vec::new();
    let mut namespaces = Vec::new();
    let mut quotas = Vec::new();
    let mut periods = Vec::new();
    let mut thresholds = Vec::new();
    for (tenant_id, namespace, quota, period, threshold) in keys {
        tenants.push(tenant_id.to_owned());
        namespaces.push(namespace.to_owned());
        quotas.push(quota.to_owned());
        periods.push(period);
        thresholds.push(threshold);
    }
    sqlx::query(
        "UPDATE quota_alerts a SET sent_at = COALESCE(a.sent_at, $6) \
         FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::bigint[]) \
             AS k(tenant, namespace, quota, period, threshold) \
         WHERE a.tenant = k.tenant AND a.namespace = k.namespace AND a.quota = k.quota \
         AND a.period = k.period AND a.threshold = k.threshold",
    )
    .bind(tenants)
    .bind(namespaces)
    .bind(quotas)
    .bind(periods)
    .bind(thresholds)
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Listing

/// The tenant's namespaces with their limits and their use of the month, filtered, ordered and
/// paged by Postgres.
pub(crate) async fn list_namespaces(
    conn: &mut PgConnection,
    filter: &NamespaceFilter,
) -> Result<NamespacePage, Error> {
    let (from, to) = days_of(filter.month);
    let order = match filter.order {
        NamespaceOrder::Cpu => "cpu",
        NamespaceOrder::Runs => "runs",
        NamespaceOrder::Scripts => "scripts",
        NamespaceOrder::Name => "name",
    };
    let limits = [
        ("cpu_per_month_ms", "l_cpu"),
        ("active_runs", "l_active"),
        ("runs_per_month", "l_runs"),
        ("scripts", "l_scripts"),
        ("max_ttl_ms", "l_ttl"),
        ("max_script_lines", "l_lines"),
    ]
    .map(|(column, alias)| format!("COALESCE(n.{column}, t.default_{column}) AS {alias}"))
    .join(", ");
    let exceptions = quota_columns("n.");
    let sql = format!(
        "WITH t AS (SELECT {defaults} FROM tenants WHERE id = $1), \
         u AS (SELECT namespace, sum(cpu_ms)::bigint AS cpu, sum(runs_started)::bigint AS runs \
               FROM usage WHERE tenant = $1 AND day >= $2 AND day < $3 GROUP BY namespace), \
         a AS (SELECT namespace, count(*) AS active FROM runs \
               WHERE tenant = $1 AND status NOT IN {terminal} GROUP BY namespace), \
         s AS (SELECT namespace, count(*) AS kept FROM namespace_scripts \
               WHERE tenant = $1 GROUP BY namespace), \
         j AS (SELECT n.tenant, n.name, {exceptions}, n.tokens_revoked_before, n.created_at, \
                      n.last_active_at, n.deleting_since, n.display_name, {limits}, \
                      COALESCE(u.cpu, 0) AS cpu, COALESCE(u.runs, 0) AS runs, \
                      COALESCE(a.active, 0) AS active, COALESCE(s.kept, 0) AS kept \
               FROM namespaces n LEFT JOIN t ON true \
               LEFT JOIN u ON u.namespace = n.name LEFT JOIN a ON a.namespace = n.name \
               LEFT JOIN s ON s.namespace = n.name \
               WHERE n.tenant = $1 AND ($10::text IS NULL OR n.name = $10) \
               AND ($4::text IS NULL OR starts_with(n.name, $4) \
                    OR starts_with(n.display_key, $11))), \
         k AS (SELECT j.*, CASE $5 WHEN 'cpu' THEN cpu WHEN 'runs' THEN runs \
                                   WHEN 'scripts' THEN kept ELSE 0 END AS key FROM j), \
         f AS (SELECT * FROM k \
               WHERE $6::numeric IS NULL \
               OR (l_cpu > 0 AND cpu::numeric * 100 >= l_cpu::numeric * $6) \
               OR (l_active > 0 AND active::numeric * 100 >= l_active::numeric * $6) \
               OR (l_runs > 0 AND runs::numeric * 100 >= l_runs::numeric * $6) \
               OR (l_scripts > 0 AND kept::numeric * 100 >= l_scripts::numeric * $6)), \
         c AS (SELECT count(*)::bigint AS total FROM f) \
         SELECT p.tenant, p.name, {plain}, p.tokens_revoked_before, p.created_at, \
                p.last_active_at, p.deleting_since, p.display_name, p.l_cpu, p.l_active, p.l_runs, p.l_scripts, \
                p.l_ttl, p.l_lines, p.cpu, p.runs, p.active, p.kept, p.key, c.total \
         FROM c LEFT JOIN LATERAL ( \
             SELECT * FROM f \
             WHERE $7::bigint IS NULL OR key < $7 OR (key = $7 AND name > $8) \
             ORDER BY key DESC, name LIMIT $9) p ON true \
         ORDER BY p.key DESC, p.name",
        defaults = quota_columns("default_"),
        terminal = terminal!(),
        plain = quota_columns("p."),
    );
    let rows = sqlx::query(&sql)
        .bind(tenant(&filter.tenant))
        .bind(from)
        .bind(to)
        .bind(filter.prefix.as_deref())
        .bind(order)
        .bind(filter.above_percent.map(i64::from))
        .bind(filter.after.as_ref().map(|after| int(after.key)))
        .bind(filter.after.as_ref().map(|after| after.name.as_str()))
        .bind(limit(filter.limit))
        .bind(filter.name.as_ref().map(NamespaceName::as_str))
        .bind(filter.prefix.as_deref().map(DisplayName::fold))
        .fetch_all(&mut *conn)
        .await?;
    // One row with the total alone when the page is empty: its namespace columns are null.
    let total = match rows.first() {
        Some(row) => row.u64(24)?,
        None => 0,
    };
    let mut namespaces = Vec::with_capacity(rows.len());
    for row in &rows {
        if row.opt_text(1)?.is_none() {
            continue;
        }
        namespaces.push(NamespaceUsage {
            namespace: namespace_from_row(row)?,
            limits: quotas_from_row(row, 13)?,
            usage: Usage {
                cpu: row.duration(19)?,
                runs_started: row.u64(20)?,
                active_runs: row.u64(21)?,
                scripts: row.u64(22)?,
                days: Vec::new(),
            },
        });
    }
    Ok(NamespacePage { namespaces, total })
}
