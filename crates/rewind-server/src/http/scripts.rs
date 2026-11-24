//! Scripts: type-checking (`POST /v1/check`). Tests (`POST /v1/test`) are in [`super::testing`].

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use rewind_api::access::Permission;
use rewind_api::scripts::{CheckRequest, CheckResult, Severity};
use rewind_core::Script;
use rewind_engine::StartError;
use rewind_engine::ports::{CatalogStore, Quotas, ScriptExecutor, Tenant, TypeChecker, limits};

use crate::app::App;
use crate::auth::Caller;
use crate::error::{ApiError, diagnostic, invalid_script_diagnostic};
use crate::extract::Body;
use crate::http::action_sets;

/// The limits a script checked or tested in the request's namespace passes, as a start would:
/// its namespace's (its exceptions over the tenant's defaults) and the tenant's ceiling. The
/// header `Rewind-Namespace` is required with a key or a member (400).
pub(super) async fn bounds(app: &App, caller: &Caller) -> Result<Quotas, ApiError> {
    let namespace = caller.namespace_required("check or test a script")?;
    let tenant = app
        .store
        .get_tenant(&caller.tenant)
        .await?
        .unwrap_or_else(|| Tenant::new(caller.tenant.clone()));
    let found = app.store.get_namespace(&caller.tenant, &namespace).await?;
    Ok(limits(&tenant, found.as_ref()).tightest(&tenant.quotas))
}

/// Refuses (422 `invalid-script`) a script longer than `max_script_lines`, as a start does.
pub(super) fn check_lines(script: &Script, bounds: &Quotas) -> Result<(), ApiError> {
    let Some(max) = bounds.max_script_lines else {
        return Ok(());
    };
    let lines = script.source().lines().count() as u64;
    if lines > u64::from(max) {
        return Err(StartError::ScriptTooLong {
            lines,
            max: u64::from(max),
        }
        .into());
    }
    Ok(())
}

/// `POST /v1/check`: the checks at start, then tsgo and the checker's rules.
pub(super) async fn check(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<CheckRequest>,
) -> Result<Json<CheckResult>, ApiError> {
    caller.require(Permission::RunsStart)?;
    let bounds = bounds(&app, &caller).await?;
    let stored = match &request.action_set {
        Some(id) => action_sets::stored(&app, &caller.tenant, id).await?,
        None => app.default_action_set(&caller.tenant).await?,
    };
    let script = Script::new(request.script);
    check_lines(&script, &bounds)?;
    if let Err(invalid) = app.plane.executor.compile(&script, &stored.action_set) {
        return Ok(Json(CheckResult {
            ok: false,
            diagnostics: vec![invalid_script_diagnostic(&invalid)],
        }));
    }
    let diagnostics: Vec<_> = app
        .checker
        .check(&script, &stored.declarations)
        .await?
        .iter()
        .map(diagnostic)
        .collect();
    Ok(Json(CheckResult {
        ok: !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error),
        diagnostics,
    }))
}
