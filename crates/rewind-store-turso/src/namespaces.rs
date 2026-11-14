//! Namespaces on Turso: the tenant's ceiling and its namespaces' defaults, namespaces with their
//! exceptions, their use per day, the runs kept per script, and quota alerts
//! (`archi/06-storage-and-scale.md`, "Quotas"). The in-memory `MemoryJournal` of
//! `rewind-engine` is the reference of every behavior here.
//!
//! Every call holds the one connection, and the writes run in a transaction: `create_run`'s
//! checks and counts are atomic, which is the lock of the namespace and of the tenant the
//! contract asks for. The tenant's own rows of `usage` and `quota_alerts` have the namespace
//! [`TENANT`].

use std::cmp::Reverse;
use std::collections::HashMap;
use std::time::Duration;

use rewind_core::{Digest, Timestamp};
use rewind_engine::ports::{
    CreateRun, Day, DayUsage, DeleteNamespace, DropNamespace, Month, Namespace, NamespaceFilter,
    NamespaceName, NamespaceOrder, NamespaceUsage, NamespaceWrite, NewRun, QuotaAlert,
    QuotaAlertKey, QuotaExceeded, QuotaKind, QuotaScope, Quotas, Retention, Tenant, TenantId,
    Threshold, Usage, UsageDelta, UsageOf,
};
use turso::{Connection, Row, Value};

use crate::Error;
use crate::sql::{self, Columns, digest, int, limit, ms, tenant, text, ts};

/// The namespace of the tenant's own rows of use and alerts.
pub(crate) const TENANT: &str = "*";

/// The terminal statuses, for `status NOT IN …`.
const TERMINAL: &str = "('completed', 'failed', 'cancelled', 'aborted')";

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

fn quotas_from_row(row: &Row, first: usize) -> Result<Quotas, Error> {
    Ok(Quotas {
        cpu_per_month: row.opt_duration(first)?,
        active_runs: row.opt_u32(first + 1)?,
        runs_per_month: row.opt_u64(first + 2)?,
        scripts: row.opt_u64(first + 3)?,
        max_ttl: row.opt_duration(first + 4)?,
        max_script_lines: row.opt_u32(first + 5)?,
    })
}

fn quota_values(quotas: &Quotas) -> [Value; 6] {
    let count = |value: Option<u64>| value.map_or(Value::Null, int);
    [
        quotas.cpu_per_month.map_or(Value::Null, ms),
        count(quotas.active_runs.map(u64::from)),
        count(quotas.runs_per_month),
        count(quotas.scripts),
        quotas.max_ttl.map_or(Value::Null, ms),
        count(quotas.max_script_lines.map(u64::from)),
    ]
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

fn tenant_from_row(row: &Row) -> Result<Tenant, Error> {
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

pub(crate) async fn put_tenant(connection: &Connection, record: &Tenant) -> Result<(), Error> {
    let mut params = vec![
        tenant(&record.id),
        ms(record.retention.journal),
        ms(record.retention.archive),
        ms(record.retention.read_model),
        ms(record.retention.payload_grace),
    ];
    params.extend(quota_values(&record.quotas));
    params.extend(quota_values(&record.namespace_defaults));
    sql::execute(
        connection,
        &format!(
            "INSERT OR REPLACE INTO tenants ({}) VALUES ({})",
            tenant_columns(),
            sql::placeholders(17)
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

/// The tenant's record, or the defaults of a tenant without one.
async fn tenant_or_default(connection: &Connection, id: &TenantId) -> Result<Tenant, Error> {
    Ok(get_tenant(connection, id)
        .await?
        .unwrap_or_else(|| Tenant::new(id.clone())))
}

// -------------------------------------------------------------------------------------------
// Namespaces

fn namespace_columns() -> String {
    format!(
        "tenant, name, {}, tokens_revoked_before, created_at, last_active_at, deleting_since",
        quota_columns("")
    )
}

fn namespace_from_row(row: &Row) -> Result<Namespace, Error> {
    let name = row.text(1)?;
    Ok(Namespace {
        tenant: row.tenant(0)?,
        name: NamespaceName::new(name.clone())
            .map_err(|_| Error::Corrupt(format!("invalid namespace {name:?}")))?,
        exceptions: quotas_from_row(row, 2)?,
        tokens_revoked_before: row.opt_ts(8)?,
        created_at: row.ts(9)?,
        last_active_at: row.ts(10)?,
        deleting_since: row.opt_ts(11)?,
    })
}

pub(crate) async fn get_namespace(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
) -> Result<Option<Namespace>, Error> {
    let row = sql::query_row(
        connection,
        &format!(
            "SELECT {} FROM namespaces WHERE tenant = ? AND name = ?",
            namespace_columns()
        ),
        vec![tenant(tenant_id), text(name.as_str())],
    )
    .await?;
    row.map(|row| namespace_from_row(&row)).transpose()
}

/// The namespace, created at `now` without exceptions if it does not exist.
async fn ensure_namespace(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    now: Timestamp,
) -> Result<Namespace, Error> {
    sql::execute(
        connection,
        "INSERT OR IGNORE INTO namespaces (tenant, name, created_at, last_active_at) \
         VALUES (?, ?, ?, ?)",
        vec![tenant(tenant_id), text(name.as_str()), ts(now), ts(now)],
    )
    .await?;
    get_namespace(connection, tenant_id, name)
        .await?
        .ok_or_else(|| Error::Corrupt(format!("namespace {name} vanished")))
}

pub(crate) async fn put_namespace(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    exceptions: Option<&Quotas>,
    now: Timestamp,
) -> Result<NamespaceWrite, Error> {
    let namespace = ensure_namespace(connection, tenant_id, name, now).await?;
    if namespace.deleting_since.is_some() {
        return Ok(NamespaceWrite::Deleting);
    }
    let Some(exceptions) = exceptions else {
        return Ok(NamespaceWrite::Written(namespace));
    };
    let mut params = quota_values(exceptions).to_vec();
    params.extend([tenant(tenant_id), text(name.as_str())]);
    sql::execute(
        connection,
        "UPDATE namespaces SET cpu_per_month_ms = ?, active_runs = ?, runs_per_month = ?, \
         scripts = ?, max_ttl_ms = ?, max_script_lines = ? WHERE tenant = ? AND name = ?",
        params,
    )
    .await?;
    Ok(NamespaceWrite::Written(Namespace {
        exceptions: exceptions.clone(),
        ..namespace
    }))
}

pub(crate) async fn revoke_namespace_tokens(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    now: Timestamp,
) -> Result<bool, Error> {
    let changed = sql::execute(
        connection,
        "UPDATE namespaces SET tokens_revoked_before = ? WHERE tenant = ? AND name = ?",
        vec![ts(now), tenant(tenant_id), text(name.as_str())],
    )
    .await?;
    Ok(changed > 0)
}

pub(crate) async fn delete_namespace(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    now: Timestamp,
) -> Result<DeleteNamespace, Error> {
    let changed = sql::execute(
        connection,
        "UPDATE namespaces SET deleting_since = COALESCE(deleting_since, ?) \
         WHERE tenant = ? AND name = ?",
        vec![ts(now), tenant(tenant_id), text(name.as_str())],
    )
    .await?;
    Ok(if changed > 0 {
        DeleteNamespace::Started
    } else {
        DeleteNamespace::NotFound
    })
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

pub(crate) async fn drop_namespace(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
) -> Result<DropNamespace, Error> {
    let marked = sql::query_row(
        connection,
        "SELECT 1 FROM namespaces WHERE tenant = ? AND name = ? AND deleting_since IS NOT NULL",
        vec![tenant(tenant_id), text(name.as_str())],
    )
    .await?;
    if marked.is_none() {
        return Ok(DropNamespace::NotFound);
    }
    let runs_left = sql::query_row(
        connection,
        "SELECT 1 FROM runs WHERE tenant = ? AND namespace = ? LIMIT 1",
        vec![tenant(tenant_id), text(name.as_str())],
    )
    .await?;
    if runs_left.is_some() {
        return Ok(DropNamespace::RunsLeft);
    }
    for statement in [
        "DELETE FROM usage WHERE tenant = ? AND namespace = ?",
        "DELETE FROM quota_alerts WHERE tenant = ? AND namespace = ?",
        "DELETE FROM namespace_scripts WHERE tenant = ? AND namespace = ?",
        "DELETE FROM namespaces WHERE tenant = ? AND name = ?",
    ] {
        sql::execute(
            connection,
            statement,
            vec![tenant(tenant_id), text(name.as_str())],
        )
        .await?;
    }
    Ok(DropNamespace::Dropped)
}

pub(crate) async fn namespace_has_script(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    script: Digest,
) -> Result<bool, Error> {
    Ok(sql::query_row(
        connection,
        "SELECT 1 FROM namespace_scripts WHERE tenant = ? AND namespace = ? AND digest = ?",
        vec![tenant(tenant_id), text(name.as_str()), digest(script)],
    )
    .await?
    .is_some())
}

// -------------------------------------------------------------------------------------------
// Use

/// The days of a month, as stored: `[first, end)`.
fn days_of(month: Month) -> (Value, Value) {
    (
        int(u64::from(month.first_day().0)),
        int(u64::from(month.next().first_day().0)),
    )
}

/// CPU time (milliseconds) and runs created in a month, of a namespace or of the tenant
/// ([`TENANT`]).
async fn month_usage(
    connection: &Connection,
    tenant_id: &TenantId,
    of: &str,
    month: Month,
) -> Result<(u64, u64), Error> {
    let (from, to) = days_of(month);
    let row = sql::query_row(
        connection,
        "SELECT COALESCE(SUM(cpu_ms), 0), COALESCE(SUM(runs_started), 0) FROM usage \
         WHERE tenant = ? AND namespace = ? AND day >= ? AND day < ?",
        vec![tenant(tenant_id), text(of), from, to],
    )
    .await?;
    match row {
        Some(row) => Ok((row.u64(0)?, row.u64(1)?)),
        None => Ok((0, 0)),
    }
}

/// The active runs of a namespace, or of the tenant.
async fn active_runs(
    connection: &Connection,
    tenant_id: &TenantId,
    namespace: Option<&NamespaceName>,
) -> Result<u64, Error> {
    let mut query =
        format!("SELECT COUNT(*) FROM runs WHERE tenant = ? AND status NOT IN {TERMINAL}");
    let mut params = vec![tenant(tenant_id)];
    if let Some(name) = namespace {
        query.push_str(" AND namespace = ?");
        params.push(text(name.as_str()));
    }
    count(connection, &query, params).await
}

/// The distinct scripts with a run kept: of a namespace, or summed over the tenant's.
async fn scripts(
    connection: &Connection,
    tenant_id: &TenantId,
    namespace: Option<&NamespaceName>,
) -> Result<u64, Error> {
    let mut query = "SELECT COUNT(*) FROM namespace_scripts WHERE tenant = ?".to_owned();
    let mut params = vec![tenant(tenant_id)];
    if let Some(name) = namespace {
        query.push_str(" AND namespace = ?");
        params.push(text(name.as_str()));
    }
    count(connection, &query, params).await
}

async fn count(connection: &Connection, query: &str, params: Vec<Value>) -> Result<u64, Error> {
    Ok(sql::query_dynamic(connection, query, params)
        .await?
        .first()
        .map(|row| row.u64(0))
        .transpose()?
        .unwrap_or(0))
}

pub(crate) async fn namespace_usage(
    connection: &Connection,
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
    let rows = sql::query(
        connection,
        "SELECT day, cpu_ms, runs_started FROM usage \
         WHERE tenant = ? AND namespace = ? AND day >= ? AND day < ? ORDER BY day",
        vec![tenant(tenant_id), text(row_of), from, to],
    )
    .await?;
    let mut usage = Usage {
        active_runs: active_runs(connection, tenant_id, namespace).await?,
        scripts: scripts(connection, tenant_id, namespace).await?,
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
    connection: &Connection,
    tenant_id: &TenantId,
    of: &str,
    day: Day,
    cpu_ms: u64,
    runs: u64,
) -> Result<(), Error> {
    sql::execute(
        connection,
        "INSERT INTO usage (tenant, namespace, day, cpu_ms, runs_started) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (tenant, namespace, day) DO UPDATE SET \
         cpu_ms = cpu_ms + excluded.cpu_ms, runs_started = runs_started + excluded.runs_started",
        vec![
            tenant(tenant_id),
            text(of),
            int(u64::from(day.0)),
            int(cpu_ms),
            int(runs),
        ],
    )
    .await?;
    Ok(())
}

async fn touch(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    now: Timestamp,
) -> Result<(), Error> {
    sql::execute(
        connection,
        "UPDATE namespaces SET last_active_at = MAX(last_active_at, ?) \
         WHERE tenant = ? AND name = ?",
        vec![ts(now), tenant(tenant_id), text(name.as_str())],
    )
    .await?;
    Ok(())
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(crate) async fn record_usage(
    connection: &Connection,
    deltas: &[UsageDelta],
    now: Timestamp,
) -> Result<Vec<QuotaAlert>, Error> {
    let mut raised = Vec::new();
    let mut tenants: HashMap<TenantId, Tenant> = HashMap::new();
    for delta in deltas {
        let month = Month::of_day(delta.day);
        let tenant_id = &delta.tenant;
        if !tenants.contains_key(tenant_id) {
            let record = tenant_or_default(connection, tenant_id).await?;
            tenants.insert(tenant_id.clone(), record);
        }
        let record = &tenants[tenant_id];
        let added = millis(delta.cpu);
        if let Some(namespace) = get_namespace(connection, tenant_id, &delta.namespace).await? {
            let limit = namespace
                .exceptions
                .over(&record.namespace_defaults)
                .limit(QuotaKind::CpuPerMonth);
            let name = delta.namespace.as_str();
            let (before, _) = month_usage(connection, tenant_id, name, month).await?;
            add_usage(connection, tenant_id, name, delta.day, added, 0).await?;
            touch(connection, tenant_id, &delta.namespace, now).await?;
            let crossing = Crossing {
                tenant: tenant_id,
                namespace: Some(&delta.namespace),
                quota: QuotaKind::CpuPerMonth,
                month,
                before,
                after: before.saturating_add(added),
                limit,
            };
            raised.extend(crossing.raise(connection, now).await?);
        }
        let (before, _) = month_usage(connection, tenant_id, TENANT, month).await?;
        add_usage(connection, tenant_id, TENANT, delta.day, added, 0).await?;
        let crossing = Crossing {
            tenant: tenant_id,
            namespace: None,
            quota: QuotaKind::CpuPerMonth,
            month,
            before,
            after: before.saturating_add(added),
            limit: record.quotas.limit(QuotaKind::CpuPerMonth),
        };
        raised.extend(crossing.raise(connection, now).await?);
    }
    Ok(raised)
}

// -------------------------------------------------------------------------------------------
// Creating a run: the quotas, then the counts

/// Under the transaction of `create_run`, once the idempotency key is known to be new: creates
/// the namespace if needed, and refuses the run if the namespace is being erased or a quota is
/// reached (recording the alert of a first refusal of `active_runs` in the month). `None`: the
/// run may be created, then [`count_run`].
pub(crate) async fn admit_run(
    connection: &Connection,
    run: &NewRun,
) -> Result<Result<Admitted, CreateRun>, Error> {
    let tenant_id = &run.run.tenant;
    let name = &run.namespace;
    let now = run.created_at;
    let namespace = ensure_namespace(connection, tenant_id, name, now).await?;
    if namespace.deleting_since.is_some() {
        return Ok(Err(CreateRun::NamespaceDeleting));
    }
    let record = tenant_or_default(connection, tenant_id).await?;
    let limits = namespace.exceptions.over(&record.namespace_defaults);
    let ceiling = record.quotas;
    let month = Month::of(now);
    let new_script = !namespace_has_script(connection, tenant_id, name, run.started.script).await?;
    let (_, runs_of_namespace) = month_usage(connection, tenant_id, name.as_str(), month).await?;
    let (_, runs_of_tenant) = month_usage(connection, tenant_id, TENANT, month).await?;
    let scripts_of_namespace = scripts(connection, tenant_id, Some(name)).await?;
    let scripts_of_tenant = scripts(connection, tenant_id, None).await?;
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
            QuotaKind::ActiveRuns => active_runs(connection, tenant_id, of).await?,
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
            raise(connection, &key, used, limit, now).await?;
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
    connection: &Connection,
    run: &NewRun,
    admitted: Admitted,
) -> Result<(), Error> {
    let tenant_id = &run.run.tenant;
    let name = &run.namespace;
    let now = run.created_at;
    let day = Day::of(now);
    add_usage(connection, tenant_id, name.as_str(), day, 0, 1).await?;
    add_usage(connection, tenant_id, TENANT, day, 0, 1).await?;
    sql::execute(
        connection,
        "INSERT INTO namespace_scripts (tenant, namespace, digest, runs, first_run_at) \
         VALUES (?, ?, ?, 1, ?) \
         ON CONFLICT (tenant, namespace, digest) DO UPDATE SET runs = runs + 1",
        vec![
            tenant(tenant_id),
            text(name.as_str()),
            digest(run.started.script),
            ts(now),
        ],
    )
    .await?;
    touch(connection, tenant_id, name, now).await?;
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
        crossing.raise(connection, now).await?;
    }
    Ok(())
}

/// Removes an erased run from its script's count in its namespace; the count at zero is removed.
pub(crate) async fn uncount_run(
    connection: &Connection,
    tenant_id: &TenantId,
    namespace: &str,
    script: &str,
) -> Result<(), Error> {
    let params = || vec![tenant(tenant_id), text(namespace), text(script)];
    sql::execute(
        connection,
        "UPDATE namespace_scripts SET runs = runs - 1 \
         WHERE tenant = ? AND namespace = ? AND digest = ?",
        params(),
    )
    .await?;
    sql::execute(
        connection,
        "DELETE FROM namespace_scripts \
         WHERE tenant = ? AND namespace = ? AND digest = ? AND runs <= 0",
        params(),
    )
    .await?;
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
        connection: &Connection,
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
            raised.extend(raise(connection, &key, self.after, limit, now).await?);
        }
        Ok(raised)
    }
}

fn key_values(key: &QuotaAlertKey) -> Vec<Value> {
    vec![
        tenant(&key.tenant),
        text(key.namespace.as_ref().map_or(TENANT, NamespaceName::as_str)),
        text(key.quota.as_str()),
        text(key.period.to_string()),
        int(u64::from(key.threshold.percent())),
    ]
}

/// Records an alert unless one with its key exists. Returns it when recorded.
async fn raise(
    connection: &Connection,
    key: &QuotaAlertKey,
    used: u64,
    limit: u64,
    now: Timestamp,
) -> Result<Option<QuotaAlert>, Error> {
    let mut params = key_values(key);
    params.extend([int(used), int(limit), ts(now)]);
    let inserted = sql::execute(
        connection,
        "INSERT OR IGNORE INTO quota_alerts (tenant, namespace, quota, period, threshold, used, \
         quota_limit, raised_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params,
    )
    .await?;
    Ok((inserted > 0).then(|| QuotaAlert {
        key: key.clone(),
        used,
        limit,
        raised_at: now,
        sent_at: None,
    }))
}

const ALERT_COLUMNS: &str =
    "tenant, namespace, quota, period, threshold, used, quota_limit, raised_at, sent_at";

fn alert_from_row(row: &Row) -> Result<QuotaAlert, Error> {
    let namespace = row.text(1)?;
    let quota = row.text(2)?;
    let period = row.text(3)?;
    let threshold = row.u64(4)?;
    Ok(QuotaAlert {
        key: QuotaAlertKey {
            tenant: row.tenant(0)?,
            namespace: (namespace != TENANT)
                .then(|| NamespaceName::new(namespace.clone()))
                .transpose()
                .map_err(|_| Error::Corrupt(format!("invalid namespace {namespace:?}")))?,
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
    connection: &Connection,
    tenant_id: &TenantId,
    namespace: Option<&NamespaceName>,
    month: Month,
) -> Result<Vec<QuotaAlert>, Error> {
    let mut query =
        format!("SELECT {ALERT_COLUMNS} FROM quota_alerts WHERE tenant = ? AND period = ?");
    let mut params = vec![tenant(tenant_id), text(month.to_string())];
    if let Some(name) = namespace {
        query.push_str(" AND namespace = ?");
        params.push(text(name.as_str()));
    }
    let mut alerts = sql::query_dynamic(connection, &query, params)
        .await?
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

pub(crate) async fn claim_quota_alerts(
    connection: &Connection,
    owner: &str,
    now: Timestamp,
    lease_duration: Duration,
    max: usize,
) -> Result<Vec<QuotaAlert>, Error> {
    let rows = sql::query(
        connection,
        &format!(
            "SELECT {ALERT_COLUMNS} FROM quota_alerts WHERE sent_at IS NULL \
             AND (lease_until IS NULL OR lease_until <= ?)"
        ),
        vec![ts(now)],
    )
    .await?;
    let mut open = rows
        .iter()
        .map(alert_from_row)
        .collect::<Result<Vec<_>, Error>>()?;
    open.sort_by(|a, b| (a.raised_at, &a.key).cmp(&(b.raised_at, &b.key)));
    open.truncate(max);
    let lease_until = now.saturating_add(lease_duration);
    for alert in &open {
        let mut params = vec![text(owner), ts(lease_until)];
        params.extend(key_values(&alert.key));
        sql::execute(
            connection,
            "UPDATE quota_alerts SET lease_owner = ?, lease_until = ? WHERE tenant = ? \
             AND namespace = ? AND quota = ? AND period = ? AND threshold = ?",
            params,
        )
        .await?;
    }
    Ok(open)
}

pub(crate) async fn ack_quota_alerts(
    connection: &Connection,
    keys: &[QuotaAlertKey],
    now: Timestamp,
) -> Result<(), Error> {
    for key in keys {
        let mut params = vec![ts(now)];
        params.extend(key_values(key));
        sql::execute(
            connection,
            "UPDATE quota_alerts SET sent_at = COALESCE(sent_at, ?) WHERE tenant = ? \
             AND namespace = ? AND quota = ? AND period = ? AND threshold = ?",
            params,
        )
        .await?;
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Listing

/// The tenant's namespaces with their limits and their use of the month, ordered and paged in
/// memory: an embedded store keeps few of them.
pub(crate) async fn list_namespaces(
    connection: &Connection,
    filter: &NamespaceFilter,
) -> Result<Vec<NamespaceUsage>, Error> {
    let record = tenant_or_default(connection, &filter.tenant).await?;
    let rows = sql::query(
        connection,
        &format!(
            "SELECT {} FROM namespaces WHERE tenant = ? ORDER BY name",
            namespace_columns()
        ),
        vec![tenant(&filter.tenant)],
    )
    .await?;
    let (from, to) = days_of(filter.month);
    let used: HashMap<String, (u64, u64)> = grouped(
        connection,
        "SELECT namespace, SUM(cpu_ms), SUM(runs_started) FROM usage \
         WHERE tenant = ? AND day >= ? AND day < ? GROUP BY namespace",
        vec![tenant(&filter.tenant), from, to],
    )
    .await?;
    let active: HashMap<String, (u64, u64)> = grouped(
        connection,
        &format!(
            "SELECT namespace, COUNT(*), 0 FROM runs WHERE tenant = ? AND status NOT IN \
             {TERMINAL} GROUP BY namespace"
        ),
        vec![tenant(&filter.tenant)],
    )
    .await?;
    let kept: HashMap<String, (u64, u64)> = grouped(
        connection,
        "SELECT namespace, COUNT(*), 0 FROM namespace_scripts WHERE tenant = ? GROUP BY namespace",
        vec![tenant(&filter.tenant)],
    )
    .await?;
    let key = |usage: &Usage| match filter.order {
        NamespaceOrder::Cpu => millis(usage.cpu),
        NamespaceOrder::Runs => usage.runs_started,
        NamespaceOrder::Scripts => usage.scripts,
        NamespaceOrder::Name => 0,
    };
    let mut listed = Vec::new();
    for row in &rows {
        let namespace = namespace_from_row(row)?;
        let name = namespace.name.as_str();
        if filter
            .prefix
            .as_deref()
            .is_some_and(|prefix| !name.starts_with(prefix))
        {
            continue;
        }
        let (cpu, runs_started) = used.get(name).copied().unwrap_or_default();
        let usage = Usage {
            cpu: Duration::from_millis(cpu),
            runs_started,
            active_runs: active.get(name).map_or(0, |(count, _)| *count),
            scripts: kept.get(name).map_or(0, |(count, _)| *count),
            days: Vec::new(),
        };
        let limits = namespace.exceptions.over(&record.namespace_defaults);
        if let Some(percent) = filter.above_percent
            && !above(&limits, &usage, percent)
        {
            continue;
        }
        listed.push((
            key(&usage),
            NamespaceUsage {
                namespace,
                limits,
                usage,
            },
        ));
    }
    listed.sort_by(|(a_key, a), (b_key, b)| {
        (Reverse(*a_key), &a.namespace.name).cmp(&(Reverse(*b_key), &b.namespace.name))
    });
    Ok(listed
        .into_iter()
        .filter(|(key, listed)| {
            filter.after.as_ref().is_none_or(|after| {
                *key < after.key || (*key == after.key && listed.namespace.name > after.name)
            })
        })
        .take(filter.limit)
        .map(|(_, listed)| listed)
        .collect())
}

/// Whether a use reaches `percent` of one of its counted limits.
fn above(limits: &Quotas, usage: &Usage, percent: u32) -> bool {
    [
        QuotaKind::CpuPerMonth,
        QuotaKind::ActiveRuns,
        QuotaKind::RunsPerMonth,
        QuotaKind::Scripts,
    ]
    .into_iter()
    .any(|quota| match (limits.limit(quota), usage.of(quota)) {
        (Some(limit), Some(used)) if limit > 0 => {
            u128::from(used) * 100 >= u128::from(limit) * u128::from(percent)
        }
        _ => false,
    })
}

/// Two counts per namespace.
async fn grouped(
    connection: &Connection,
    query: &str,
    params: Vec<Value>,
) -> Result<HashMap<String, (u64, u64)>, Error> {
    sql::query_dynamic(connection, query, params)
        .await?
        .iter()
        .map(|row| Ok((row.text(0)?, (row.u64(1)?, row.u64(2)?))))
        .collect()
}
