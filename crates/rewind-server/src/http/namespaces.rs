//! Namespaces: the end users of the organization, their limits, their use and their alerts,
//! and the operator's ceiling of a tenant (`archi/06-storage-and-scale.md`,
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
    DayUsage, Namespace as ApiNamespace, NamespaceDefaults, NamespaceDetail, NamespaceDisplayName,
    NamespaceList, NamespaceListQuery, NamespaceNames, NamespaceOrder as ApiOrder, NamespacePatch,
    NamespaceQuery, PutCeiling, PutNamespace, PutNamespaceDefaults, QuotaAlertList,
    QuotaAlertQuery,
};
use rewind_engine::ports::catalog::NamespaceFilter;
use rewind_engine::ports::{
    AccessStore, CatalogStore, DeleteNamespace, DisplayName, Month, Namespace, NamespaceName,
    NamespaceWrite, QuotaKind, Scope, Tenant, TenantId, Usage, limits,
};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::{Body, OptionalBody, Query};
use crate::http::refuse_nul_in_filter;
use crate::views::{self, engine_quotas, quotas};

/// Namespaces a page of `GET /v1/namespaces` holds by default, and at most.
const PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;

/// Namespaces read from the catalog at once, to order them by their use.
const CATALOG_PAGE: usize = 1_000;

/// How `GET /v1/namespaces` orders them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamespaceOrder {
    Cpu,
    Runs,
    Workflows,
    Name,
}

/// A namespace's name from a path: 400 when it is not one.
pub(crate) fn name(text: &str) -> Result<NamespaceName, ApiError> {
    NamespaceName::new(text).map_err(|_| {
        ApiError::invalid(format!(
            "{text:?} is not a namespace: expected 1 to 128 ASCII letters, digits, -, _, ., : \
             and @"
        ))
    })
}

/// A display name as a request gives it: `None` to remove it, 400 when it is not one.
fn display_name(text: &str) -> Result<Option<DisplayName>, ApiError> {
    DisplayName::parse(text).map_err(|_| {
        ApiError::invalid(format!(
            "displayName {text:?}: expected at most {} characters once trimmed, none of them a \
             control character",
            DisplayName::MAX_CHARS
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
        "this namespace is being erased: it takes no run, no token and no secret any more",
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
        display_name: namespace
            .display_name
            .as_ref()
            .map(|display| display.as_str().to_owned()),
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
    let usage = crate::usage::by_day(app, &namespace.tenant, &namespace.name, month).await?;
    let alerts = crate::usage::alerts(app, &namespace.tenant, Some(&namespace.name), month).await?;
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
        NamespaceOrder::Workflows => usage.workflows,
        NamespaceOrder::Name => 0,
    }
}

/// A cursor of `GET /v1/namespaces`: `{key}-{name}`, the last namespace of the page.
fn encode_cursor(key: u64, name: &NamespaceName) -> String {
    format!("{key}-{name}")
}

fn decode_cursor(text: &str) -> Result<(u64, NamespaceName), ApiError> {
    let invalid = || ApiError::invalid(format!("after {text:?} is not a cursor of this listing"));
    let (key, name) = text.split_once('-').ok_or_else(invalid)?;
    Ok((
        key.parse().map_err(|_| invalid())?,
        NamespaceName::new(name).map_err(|_| invalid())?,
    ))
}

/// Whether a namespace uses at least `percent` % of one of its limits.
fn above(usage: &Usage, limits: &rewind_engine::ports::Quotas, percent: u32) -> bool {
    [
        QuotaKind::CpuPerMonth,
        QuotaKind::ActiveRuns,
        QuotaKind::RunsPerMonth,
        QuotaKind::Workflows,
    ]
    .into_iter()
    .any(|quota| match (usage.of(quota), limits.limit(quota)) {
        (Some(used), Some(limit)) => {
            used.saturating_mul(100) >= limit.saturating_mul(u64::from(percent))
        }
        _ => false,
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
        ApiOrder::Workflows => NamespaceOrder::Workflows,
        ApiOrder::Name => NamespaceOrder::Name,
    };
    let limit = usize::try_from(query.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    let after = query.after.as_deref().map(decode_cursor).transpose()?;
    let bound = caller.namespace.is_some();
    // Every namespace the filter takes, from the catalog by name, then their use of the month
    // from the counters, ordered by it.
    let mut matching = Vec::new();
    let mut from = None;
    loop {
        let page = app
            .catalog()
            .list_namespaces(&NamespaceFilter {
                tenant: caller.tenant.clone(),
                // Bound to one namespace: that one alone, on the first page only.
                name: caller.namespace.clone(),
                prefix: query.prefix.clone(),
                after: from.clone(),
                limit: CATALOG_PAGE,
            })
            .await?;
        let full = page.namespaces.len() == CATALOG_PAGE;
        from = page.namespaces.last().map(|last| last.name.clone());
        matching.extend(page.namespaces);
        if !full || bound {
            break;
        }
    }
    let tenant = tenant(&app, &caller.tenant).await?;
    let scope = match &caller.namespace {
        Some(namespace) => Scope::Namespace {
            tenant: caller.tenant.clone(),
            namespace: namespace.clone(),
        },
        None => Scope::Tenant(caller.tenant.clone()),
    };
    let used = crate::usage::of_namespaces(&app, &scope, month, false).await?;
    let mut listed: Vec<(u64, Namespace, rewind_engine::ports::Quotas, Usage)> = matching
        .into_iter()
        .filter_map(|namespace| {
            let usage = used.get(&namespace.name).cloned().unwrap_or_default();
            let limits = limits(&tenant, Some(&namespace));
            if above_percent.is_some_and(|percent| !above(&usage, &limits, percent)) {
                return None;
            }
            Some((order_key(order, &usage), namespace, limits, usage))
        })
        .collect();
    listed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    let total = listed.len() as u64;
    if bound && query.after.is_some() {
        listed.clear();
    }
    if let Some((key, name)) = &after
        && !bound
    {
        // Largest use first, then by name: what comes after the cursor.
        listed.retain(|(at, namespace, ..)| *at < *key || (*at == *key && namespace.name > *name));
    }
    let next = (listed.len() > limit)
        .then(|| &listed[limit - 1])
        .map(|(key, namespace, ..)| encode_cursor(*key, &namespace.name));
    listed.truncate(limit);
    Ok(Json(NamespaceList {
        month: month.to_string(),
        namespaces: listed
            .iter()
            .map(|(_, namespace, limits, usage)| view(namespace, limits, usage))
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
        .put_namespace(
            &caller.tenant,
            &name,
            Some(&exceptions),
            app.config.partitions,
            app.now(),
        )
        .await?;
    let NamespaceWrite::Written(namespace) = written else {
        return Err(deleting());
    };
    Ok(Json(detail(&app, &namespace, Month::of(app.now())).await?))
}

/// `PATCH /v1/namespaces/{name}`: changes what the merge patch holds of an existing namespace,
/// its display name. `PUT` never touches it.
pub(super) async fn patch(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(text): Path<String>,
    Body(patch): Body<NamespacePatch>,
) -> Result<Json<NamespaceDetail>, ApiError> {
    caller.require(Permission::NamespacesWrite)?;
    let name = name(&text)?;
    reach(&caller, &name)?;
    let written = match patch.display_name {
        Some(given) => {
            let display = given.as_deref().map(display_name).transpose()?.flatten();
            app.store
                .set_namespace_display_name(&caller.tenant, &name, display.as_ref())
                .await?
        }
        // Nothing to change: the namespace as it is, refused as a change would be.
        None => app
            .store
            .get_namespace(&caller.tenant, &name)
            .await?
            .map(|namespace| match namespace.deleting_since {
                Some(_) => NamespaceWrite::Deleting,
                None => NamespaceWrite::Written(namespace),
            }),
    };
    match written {
        None => Err(no_namespace(&name)),
        Some(NamespaceWrite::Deleting) => Err(deleting()),
        Some(NamespaceWrite::Written(namespace)) => {
            Ok(Json(detail(&app, &namespace, Month::of(app.now())).await?))
        }
    }
}

/// `DELETE /v1/namespaces/{name}`: erases the end user. The namespace takes no run and no token
/// from now on, and its tokens are revoked. The change reaches the owner of its partition, which
/// cancels its active runs through the normal path and erases its ended runs; then the control
/// plane erases its tests, its secrets and the namespace.
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
        .catalog()
        .delete_namespace(&caller.tenant, &name, now)
        .await?
        == DeleteNamespace::NotFound
    {
        return Err(no_namespace(&name));
    }
    app.store.revoke_tokens(&caller.tenant, &name, now).await?;
    app.catalog()
        .revoke_namespace_tokens(&caller.tenant, &name, now)
        .await?;
    Ok(StatusCode::ACCEPTED)
}

/// The defaults, the ceiling and the tenant's use of `month`.
async fn defaults_of(
    app: &App,
    id: &TenantId,
    month: Month,
) -> Result<NamespaceDefaults, ApiError> {
    let tenant = tenant(app, id).await?;
    let usage = crate::usage::of_scope(app, &Scope::Tenant(id.clone()), month).await?;
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

/// `GET /v1/organization/namespace-names`: the namespaces that have a display name, by name; a
/// request bound to a namespace reads its own alone.
pub(super) async fn names(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<NamespaceNames>, ApiError> {
    caller.require(Permission::NamespacesRead)?;
    let names = app.store.namespace_display_names(&caller.tenant).await?;
    Ok(Json(NamespaceNames {
        namespaces: names
            .into_iter()
            .filter(|(name, _)| caller.reaches(name))
            .map(|(name, display_name)| NamespaceDisplayName {
                name: views::namespace(&name),
                display_name: display_name.as_str().to_owned(),
            })
            .collect(),
    }))
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
    let alerts = crate::usage::alerts(&app, &caller.tenant, namespace.as_ref(), month).await?;
    Ok(Json(QuotaAlertList {
        month: month.to_string(),
        alerts: alerts.iter().map(views::quota_alert).collect(),
    }))
}

/// `PUT /v1/organizations/{organization}/quotas`: the operator replaces a tenant's ceiling.
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
