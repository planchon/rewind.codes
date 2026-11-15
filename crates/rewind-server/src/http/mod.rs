//! The HTTP API `/v1` (`archi/07-code-architecture.md`, "HTTP API `/v1`"), the inbound
//! webhooks and the dashboard. The paths are `rewind_api::routes`; the DTOs are `rewind-api`'s.

mod admin;

pub(crate) use admin::ensure_tenant;
mod dashboard;
mod events;
mod hooks;
mod reading;
mod runs;
mod scripts;
mod search;
mod subscriptions;
mod tasks;
mod toolsets;
mod workflows;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::HeaderValue;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::{Extension, Router};
use rewind_api::headers::REWIND_VERSION;
use rewind_api::routes;

use crate::app::App;
use crate::auth::{Caller, authenticate};
use crate::error::ApiError;

/// The largest request body: payloads are bounded at 8 MiB of canonical JSON, and JSON as sent
/// may be larger.
const BODY_LIMIT: usize = 16 * 1024 * 1024;

/// The routes of one listener. Over the embedded server's socket, no key is read: the caller is
/// the tenant `local`. Over TCP, every request to `/v1` carries a key.
pub(crate) fn router(app: Arc<App>, socket: bool) -> Router {
    let api = Router::new()
        .route(routes::TOOLSETS, post(toolsets::create).get(toolsets::list))
        .route(routes::TOOLSET_DECLARATIONS, get(toolsets::declarations))
        .route(routes::TOOLSET_SEARCH, get(toolsets::search))
        .route(routes::TOOLSET_CREDENTIALS, put(toolsets::credentials))
        .route(
            routes::WORKFLOWS,
            post(workflows::create).get(workflows::list),
        )
        .route(routes::WORKFLOW, get(workflows::versions))
        .route(routes::CHECK, post(scripts::check))
        .route(routes::TEST, post(scripts::test))
        .route(routes::SCRIPTS, get(reading::scripts))
        .route(routes::SCRIPT, get(reading::script))
        .route(routes::RUNS, post(runs::start).get(runs::list))
        .route(routes::RUN_STATS, get(runs::stats))
        .route(routes::RUN_FAILURES, get(runs::failures))
        .route(routes::RUN, get(runs::get).delete(runs::erase))
        .route(routes::RUN_TRACE, get(runs::trace))
        .route(routes::RUN_ENTRY, get(runs::entry))
        .route(routes::RUN_EVENTS, get(events::stream))
        .route(routes::RUN_SIGNAL, post(runs::signal))
        .route(routes::RUN_CANCEL, post(runs::cancel))
        .route(routes::RUN_FORK, post(runs::fork))
        .route(routes::RUN_JOURNAL, get(runs::journal))
        .route(routes::RUN_RELEASE, post(runs::release))
        .route(routes::RUN_GIVE_UP, post(runs::give_up))
        .route(routes::RUN_CREDENTIALS, put(runs::credentials))
        .route(routes::TASKS, get(tasks::claim))
        .route(routes::TASK_HEARTBEAT, post(tasks::heartbeat))
        .route(routes::TASK_DEFER, post(tasks::defer))
        .route(routes::TASK_COMPLETE, post(tasks::complete))
        .route(routes::TASK_FAIL, post(tasks::fail))
        .route(routes::TASK_APPROVE, post(tasks::approve))
        .route(routes::TASK_REJECT, post(tasks::reject))
        .route(
            routes::SUBSCRIPTIONS,
            post(subscriptions::create).get(subscriptions::list),
        )
        .route(routes::SUBSCRIPTION, delete(subscriptions::delete))
        .route(routes::TOOL_STATS, get(reading::tool_stats))
        .route(routes::PAYLOAD, get(reading::payload))
        .route(routes::API_KEYS, post(admin::create_key))
        .route(routes::API_KEY, delete(admin::revoke_key))
        .route(routes::WHOAMI, get(admin::whoami))
        .route(routes::SERVER, get(admin::server))
        .route(routes::SERVER_STOP, post(admin::stop))
        .with_state(app.clone());
    let api = if socket {
        api.layer(Extension(Caller::local()))
    } else {
        api.layer(from_fn_with_state(app.clone(), authenticate))
    };
    let public = Router::new()
        .route(routes::HOOK, post(hooks::post))
        .fallback(dashboard::serve)
        .with_state(app.clone());
    api.merge(public)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(from_fn_with_state(app, every_response))
}

/// Refuses a filter holding a NUL character (U+0000): no text the stores keep holds one, so the
/// filter could match nothing, and Postgres refuses the character in a query.
pub(crate) fn refuse_nul_in_filter(what: &str, text: &str) -> Result<(), ApiError> {
    if text.contains('\0') {
        return Err(ApiError::invalid(format!(
            "{what} {text:?} contains a NUL character (U+0000), which no stored text holds: \
             remove it"
        )));
    }
    Ok(())
}

/// The core status of an API status.
pub(crate) fn runs_status(status: rewind_api::common::RunStatus) -> rewind_core::RunStatus {
    use rewind_api::common::RunStatus as Api;
    match status {
        Api::Pending => rewind_core::RunStatus::Pending,
        Api::Active => rewind_core::RunStatus::Active,
        Api::Completed => rewind_core::RunStatus::Completed,
        Api::Failed => rewind_core::RunStatus::Failed,
        Api::Cancelled => rewind_core::RunStatus::Cancelled,
        Api::Aborted => rewind_core::RunStatus::Aborted,
    }
}

/// Every response carries `Rewind-Version`; every request counts as activity for the embedded
/// server's idle stop.
async fn every_response(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let busy = app.activity.busy();
    let mut response = next.run(request).await;
    drop(busy);
    response.headers_mut().insert(
        REWIND_VERSION,
        HeaderValue::from_static(app.settings.version),
    );
    response
}
