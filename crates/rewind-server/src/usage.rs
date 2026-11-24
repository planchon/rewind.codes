//! What the namespaces of a tenant use (`archi/06-storage-and-scale.md`, "Quotas"): the CPU time
//! of a month, the runs created in it, the runs active and the workflows kept now, from the
//! counters and gauges of the partitions that hold them (one request per partition), and the
//! quota alerts of the tenant's tenant actor.

use std::collections::BTreeMap;

use rewind_engine::ports::{Month, NamespaceName, QuotaAlert, Scope, TenantId, Usage};

use crate::app::App;
use crate::error::ApiError;

/// The use of every namespace of the scope over `month` that used something; with `days`, day
/// by day too.
pub(crate) async fn of_namespaces(
    app: &App,
    scope: &Scope,
    month: Month,
    days: bool,
) -> Result<BTreeMap<NamespaceName, Usage>, ApiError> {
    Ok(app
        .edge
        .usage(&mut app.gateway(), scope, month, days)
        .await?)
}

/// The use of a scope over `month`, its namespaces added up.
pub(crate) async fn of_scope(app: &App, scope: &Scope, month: Month) -> Result<Usage, ApiError> {
    let mut total = Usage::default();
    for used in of_namespaces(app, scope, month, false).await?.values() {
        total.cpu += used.cpu;
        total.runs_started += used.runs_started;
        total.active_runs += used.active_runs;
        total.workflows += used.workflows;
    }
    Ok(total)
}

/// The use of a namespace over `month`, with its CPU time and runs created day by day.
pub(crate) async fn by_day(
    app: &App,
    tenant: &TenantId,
    namespace: &NamespaceName,
    month: Month,
) -> Result<Usage, ApiError> {
    let scope = Scope::Namespace {
        tenant: tenant.clone(),
        namespace: namespace.clone(),
    };
    Ok(of_namespaces(app, &scope, month, true)
        .await?
        .remove(namespace)
        .unwrap_or_default())
}

/// The quota alerts of `month`, of one namespace or of all (the tenant's ceiling's included),
/// newest first.
pub(crate) async fn alerts(
    app: &App,
    tenant: &TenantId,
    namespace: Option<&NamespaceName>,
    month: Month,
) -> Result<Vec<QuotaAlert>, ApiError> {
    Ok(app
        .edge
        .quota_alerts(&mut app.gateway(), tenant, namespace, month)
        .await?)
}
