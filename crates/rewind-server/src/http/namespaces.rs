//! Namespaces: the end users of the organization, their limits, their use and their alerts,
//! their credentials, and the operator's ceiling of a tenant (`archi/06-storage-and-scale.md`,
//! "Namespaces", "Quotas"; `archi/07-code-architecture.md`, "HTTP API `/v1`").
//!
//! A request bound to a namespace (`Rewind-Namespace`) reads that namespace only: another one in
//! a path is missing (404), and the lists hold it alone.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use rewind_api::ProblemType;
use rewind_api::access::Permission;
use rewind_api::common::timestamp;
use rewind_api::namespaces::{
    DayUsage, Namespace as ApiNamespace, NamespaceDefaults, NamespaceDetail, NamespaceList,
    NamespaceListQuery, NamespaceOrder as ApiOrder, NamespaceQuery, PutCeiling, PutNamespace,
    PutNamespaceDefaults, QuotaAlertList, QuotaAlertQuery,
};
use rewind_api::toolsets::Credentials;
use rewind_core::RunStatus;
use rewind_engine::ports::{
    AccessStore, DeleteNamespace, JournalStore, Month, Namespace, NamespaceCursor, NamespaceFilter,
    NamespaceName, NamespaceOrder, NamespaceUsage, NamespaceWrite, QuotaKind, RunCursor, RunFilter,
    Tenant, TenantId, Usage, UsageOf, limits,
};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::{Body, OptionalBody, Query};
use crate::http::refuse_nul_in_filter;
use crate::http::toolsets::{parse_digest, secrets};
use crate::views::{self, engine_quotas, quotas};

/// Namespaces a page of `GET /v1/namespaces` holds by default, and at most.
const PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;

/// The runs cancelled at once when a namespace is erased.
const CANCEL_BATCH: usize = 100;

/// A namespace's name from a path: 400 when it is not one.
pub(crate) fn name(text: &str) -> Result<NamespaceName, ApiError> {
    NamespaceName::new(text).map_err(|_| {
        ApiError::invalid(format!(
            "{text:?} is not a namespace: expected 1 to 128 ASCII letters, digits, -, _, ., : \
             and @"
        ))
    })
}

fn no_namespace(name: &NamespaceName) -> ApiError {
    ApiError::not_found(format!(
        "no namespace {name}: a namespace is created at its first use (a run, a token) or with \
         PUT /v1/namespaces/{{name}}"
    ))
}

/// Refuses (404) a namespace outside the one the request is bound to.
pub(crate) fn reach(caller: &Caller, name: &NamespaceName) -> Result<(), ApiError> {
    if caller.reaches(name) {
        Ok(())
    } else {
        Err(no_namespace(name))
    }
}

/// The 409 of a namespace being erased.
pub(crate) fn deleting() -> ApiError {
    ApiError::new(
        ProblemType::NamespaceDeleting,
        "this namespace is being erased: it takes no run, no token and no credentials any more",
    )
}

/// The month of `text` (`2026-11`), the current one by default.
fn month(text: Option<&str>, app: &App) -> Result<Month, ApiError> {
    match text {
        None => Ok(Month::of(app.now())),
        Some(text) => Month::parse(text).ok_or_else(|| {
            ApiError::invalid(format!("month {text:?}: expected a month such as 2026-11"))
        }),
    }
}

/// The tenant's record; without one, a tenant without limits.
async fn tenant(app: &App, id: &TenantId) -> Result<Tenant, ApiError> {
    Ok(app
        .store
        .get_tenant(id)
        .await?
        .unwrap_or_else(|| Tenant::new(id.clone())))
}

fn view(
    namespace: &Namespace,
    limits: &rewind_engine::ports::Quotas,
    usage: &Usage,
) -> ApiNamespace {
    ApiNamespace {
        name: views::namespace(&namespace.name),
        limits: quotas(limits),
        exceptions: quotas(&namespace.exceptions),
        usage: views::usage(usage),
        created_at: timestamp(namespace.created_at),
        last_active_at: timestamp(namespace.last_active_at),
        deleting_since: namespace.deleting_since.map(timestamp),
    }
}

/// A namespace with its use day by day over `month`, its alerts and its live tokens.
async fn detail(
    app: &App,
    namespace: &Namespace,
    month: Month,
) -> Result<NamespaceDetail, ApiError> {
    let tenant = tenant(app, &namespace.tenant).await?;
    let usage = app
        .store
        .namespace_usage(
            &namespace.tenant,
            &UsageOf::Namespace(namespace.name.clone()),
            month,
        )
        .await?;
    let alerts = app
        .store
        .quota_alerts(&namespace.tenant, Some(&namespace.name), month)
        .await?;
    let tokens = app
        .store
        .count_tokens(&namespace.tenant, &namespace.name, app.now())
        .await?;
    Ok(NamespaceDetail {
        namespace: view(namespace, &limits(&tenant, Some(namespace)), &usage),
        month: month.to_string(),
        days: usage
            .days
            .iter()
            .map(|(day, used)| DayUsage {
                day: day.date(),
                cpu: used.cpu.into(),
                runs_started: used.runs_started,
            })
            .collect(),
        alerts: alerts.iter().map(views::quota_alert).collect(),
        tokens,
        tokens_revoked_before: namespace.tokens_revoked_before.map(timestamp),
    })
}

/// The key of a namespace in an order: its use of the month, largest first; 0 by name.
fn order_key(order: NamespaceOrder, usage: &Usage) -> u64 {
    match order {
        NamespaceOrder::Cpu => usage.of(QuotaKind::CpuPerMonth).unwrap_or(0),
        NamespaceOrder::Runs => usage.runs_started,
        NamespaceOrder::Scripts => usage.scripts,
        NamespaceOrder::Name => 0,
    }
}

/// A cursor of `GET /v1/namespaces`: `{key}-{name}`, the last namespace of the page.
fn encode_cursor(order: NamespaceOrder, last: &NamespaceUsage) -> String {
    format!("{}-{}", order_key(order, &last.usage), last.namespace.name)
}

fn decode_cursor(text: &str) -> Result<NamespaceCursor, ApiError> {
    let invalid = || ApiError::invalid(format!("after {text:?} is not a cursor of this listing"));
    let (key, name) = text.split_once('-').ok_or_else(invalid)?;
    Ok(NamespaceCursor {
        key: key.parse().map_err(|_| invalid())?,
        name: NamespaceName::new(name).map_err(|_| invalid())?,
    })
}

/// `GET /v1/namespaces`: a page of namespaces, each with its limits and its use of the month.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<NamespaceListQuery>,
) -> Result<Json<NamespaceList>, ApiError> {
    caller.require(Permission::NamespacesRead)?;
    let month = month(query.month.as_deref(), &app)?;
    if let Some(prefix) = &query.prefix {
        refuse_nul_in_filter("prefix", prefix)?;
    }
    let above_percent = match query.above {
        None => None,
        Some(share) if share.is_finite() && (0.0..=100.0).contains(&share) => {
            Some((share * 100.0).round() as u32)
        }
        Some(share) => {
            return Err(ApiError::invalid(format!(
                "above {share}: expected a share of a limit, such as 0.8 for 80 %"
            )));
        }
    };
    let order = match query.order.unwrap_or_default() {
        ApiOrder::Cpu => NamespaceOrder::Cpu,
        ApiOrder::Runs => NamespaceOrder::Runs,
        ApiOrder::Scripts => NamespaceOrder::Scripts,
        ApiOrder::Name => NamespaceOrder::Name,
    };
    let limit = usize::try_from(query.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    let mut filter = NamespaceFilter {
        tenant: caller.tenant.clone(),
        prefix: query.prefix.clone(),
        above_percent,
        order,
        month,
        after: query.after.as_deref().map(decode_cursor).transpose()?,
        limit,
    };
    if let Some(bound) = &caller.namespace {
        // Bound to one namespace: that one alone, which sorts first by name among those it
        // prefixes.
        if query
            .prefix
            .as_deref()
            .is_some_and(|prefix| !bound.as_str().starts_with(prefix))
        {
            return Ok(Json(NamespaceList {
                month: month.to_string(),
                namespaces: Vec::new(),
                total: 0,
                next: None,
            }));
        }
        filter.prefix = Some(bound.as_str().to_owned());
        filter.order = NamespaceOrder::Name;
        filter.after = None;
        filter.limit = 1;
    }
    let page = app.store.list_namespaces(&filter).await?;
    let (found, total) = match &caller.namespace {
        // Its own namespace, on the first page only; the store's total counts those its name
        // prefixes.
        Some(bound) => {
            let found: Vec<_> = page
                .namespaces
                .into_iter()
                .filter(|listed| listed.namespace.name == *bound)
                .collect();
            let total = u64::try_from(found.len()).unwrap_or(0);
            let found = if query.after.is_some() { Vec::new() } else { found };
            (found, total)
        }
        None => (page.namespaces, page.total),
    };
    let next = (caller.namespace.is_none() && found.len() == limit)
        .then(|| found.last())
        .flatten()
        .map(|last| encode_cursor(order, last));
    Ok(Json(NamespaceList {
        month: month.to_string(),
        namespaces: found
            .iter()
            .map(|listed| view(&listed.namespace, &listed.limits, &listed.usage))
            .collect(),
        total,
        next,
    }))
}

/// `GET /v1/namespaces/{name}`.
pub(super) async fn get(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(text): Path<String>,
    Query(query): Query<NamespaceQuery>,
) -> Result<Json<NamespaceDetail>, ApiError> {
    caller.require(Permission::NamespacesRead)?;
    let name = name(&text)?;
    reach(&caller, &name)?;
    let month = month(query.month.as_deref(), &app)?;
    let namespace = app
        .store
        .get_namespace(&caller.tenant, &name)
        .await?
        .ok_or_else(|| no_namespace(&name))?;
    Ok(Json(detail(&app, &namespace, month).await?))
}

/// `PUT /v1/namespaces/{name}`: creates the namespace, or replaces its exceptions.
pub(super) async fn put(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(text): Path<String>,
    OptionalBody(request): OptionalBody<PutNamespace>,
) -> Result<Json<NamespaceDetail>, ApiError> {
    caller.require(Permission::NamespacesWrite)?;
    let name = name(&text)?;
    reach(&caller, &name)?;
    let exceptions = engine_quotas(&request.exceptions);
    let written = app
        .store
        .put_namespace(&caller.tenant, &name, Some(&exceptions), app.now())
        .await?;
    let NamespaceWrite::Written(namespace) = written else {
        return Err(deleting());
    };
    Ok(Json(detail(&app, &namespace, Month::of(app.now())).await?))
}

/// `DELETE /v1/namespaces/{name}`: erases the end user. The namespace takes no run and no token
/// from now on, its tokens are revoked and its active runs cancelled through the normal path;
/// the worker role erases its runs, credentials and use in the background.
pub(super) async fn delete(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(text): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::NamespacesWrite)?;
    let name = name(&text)?;
    reach(&caller, &name)?;
    let now = app.now();
    if app
        .store
        .delete_namespace(&caller.tenant, &name, now)
        .await?
        == DeleteNamespace::NotFound
    {
        return Err(no_namespace(&name));
    }
    app.store.revoke_tokens(&caller.tenant, &name, now).await?;
    app.store
        .revoke_namespace_tokens(&caller.tenant, &name, now)
        .await?;
    for status in [RunStatus::Pending, RunStatus::Active] {
        let mut filter = RunFilter::new(caller.tenant.clone());
        filter.namespace = Some(name.clone());
        filter.status = Some(status);
        filter.limit = Some(CANCEL_BATCH);
        loop {
            let runs = app.store.list_runs(&filter).await?;
            for record in &runs {
                app.engine
                    .cancel(
                        &record.run,
                        Some("the end user's namespace is being erased".to_owned()),
                    )
                    .await?;
            }
            match runs.last() {
                Some(last) if runs.len() == CANCEL_BATCH => {
                    filter.after = Some(RunCursor::from(last));
                }
                _ => break,
            }
        }
    }
    app.wake.bump();
    Ok(StatusCode::ACCEPTED)
}

/// `PUT /v1/namespaces/{name}/toolsets/{id}/credentials`: an end user's credentials for a
/// toolset. The calls of the namespace's runs parked by `credentials.expired` are ready again.
pub(super) async fn credentials(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path((text, id)): Path<(String, String)>,
    Body(credentials): Body<Credentials>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::CredentialsWrite)?;
    if caller.scoped() {
        return Err(ApiError::forbidden(format!(
            "a namespace's credentials serve every run of the end user: {} is limited to labels, \
             and sets the credentials of its own runs (PUT /v1/runs/{{id}}/credentials)",
            caller.who()
        )));
    }
    let name = name(&text)?;
    reach(&caller, &name)?;
    let toolset = parse_digest(&id)?;
    if app
        .store
        .get_namespace(&caller.tenant, &name)
        .await?
        .is_some_and(|namespace| namespace.deleting_since.is_some())
    {
        return Err(deleting());
    }
    app.engine
        .set_namespace_credentials(&caller.tenant, &name, toolset, &secrets(credentials))
        .await?;
    app.record_parked_tasks(&caller.tenant, Some(&name), toolset)
        .await;
    app.wake.bump();
    Ok(StatusCode::NO_CONTENT)
}

/// The defaults, the ceiling and the tenant's use of `month`.
async fn defaults_of(
    app: &App,
    id: &TenantId,
    month: Month,
) -> Result<NamespaceDefaults, ApiError> {
    let tenant = tenant(app, id).await?;
    let usage = app
        .store
        .namespace_usage(id, &UsageOf::Tenant, month)
        .await?;
    Ok(NamespaceDefaults {
        defaults: quotas(&tenant.namespace_defaults),
        ceiling: quotas(&tenant.quotas),
        month: month.to_string(),
        usage: views::usage(&usage),
    })
}

/// `GET /v1/organization/namespace-defaults`.
pub(super) async fn defaults(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<NamespaceQuery>,
) -> Result<Json<NamespaceDefaults>, ApiError> {
    caller.require(Permission::NamespacesRead)?;
    let month = month(query.month.as_deref(), &app)?;
    Ok(Json(defaults_of(&app, &caller.tenant, month).await?))
}

/// `PUT /v1/organization/namespace-defaults`: replaces the limits of every namespace without an
/// exception.
pub(super) async fn put_defaults(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<PutNamespaceDefaults>,
) -> Result<Json<NamespaceDefaults>, ApiError> {
    caller.require(Permission::NamespacesWrite)?;
    app.ensure_tenant(&caller.tenant).await?;
    let mut record = tenant(&app, &caller.tenant).await?;
    record.namespace_defaults = engine_quotas(&request.defaults);
    app.store.put_tenant(&record).await?;
    Ok(Json(
        defaults_of(&app, &caller.tenant, Month::of(app.now())).await?,
    ))
}

/// `GET /v1/organization/quota-alerts`: the alerts of a month, newest first.
pub(super) async fn alerts(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<QuotaAlertQuery>,
) -> Result<Json<QuotaAlertList>, ApiError> {
    caller.require(Permission::NamespacesRead)?;
    let month = month(query.month.as_deref(), &app)?;
    let asked = query.namespace.as_ref().map(views::engine_namespace);
    let namespace = match (&caller.namespace, asked) {
        (Some(bound), Some(asked)) if *bound != asked => {
            return Ok(Json(QuotaAlertList {
                month: month.to_string(),
                alerts: Vec::new(),
            }));
        }
        (Some(bound), _) => Some(bound.clone()),
        (None, asked) => asked,
    };
    let alerts = app
        .store
        .quota_alerts(&caller.tenant, namespace.as_ref(), month)
        .await?;
    Ok(Json(QuotaAlertList {
        month: month.to_string(),
        alerts: alerts.iter().map(views::quota_alert).collect(),
    }))
}

/// `PUT /v1/organizations/{tenant}/quotas`: the operator replaces a tenant's ceiling.
pub(super) async fn put_ceiling(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(text): Path<String>,
    Body(request): Body<PutCeiling>,
) -> Result<StatusCode, ApiError> {
    caller.require_operator("set an organization's ceiling")?;
    let missing = || {
        ApiError::not_found(format!(
            "no tenant {text}: a tenant has a record from its first key, its organization's \
             creation, or its first member's request"
        ))
    };
    let id = TenantId::new(text.clone()).map_err(|_| missing())?;
    let mut record = app.store.get_tenant(&id).await?.ok_or_else(missing)?;
    record.quotas = engine_quotas(&request.ceiling);
    app.store.put_tenant(&record).await?;
    Ok(StatusCode::NO_CONTENT)
}
