//! The HTTP API `/v1` (`archi/07-code-architecture.md`, "HTTP API `/v1`"), the dashboard's
//! sign-in `/auth`, the inbound webhooks and the dashboard. The paths are `rewind_api::routes`;
//! the DTOs are `rewind-api`'s.

mod admin;
mod changes;
mod connections;
mod dashboard;
mod events;
mod hooks;
mod namespaces;
mod oauth;
mod organization;
mod reading;
mod roles;
pub(crate) mod runs;
mod scripts;
mod search;
mod secrets;
mod sign_in;
mod stats;
mod subscriptions;
mod tasks;
mod testing;
mod tokens;
mod toolsets;
mod workflows;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Extension, Router};
use rewind_api::headers::REWIND_VERSION;
use rewind_api::{ProblemType, routes};

use crate::app::App;
use crate::auth::{Caller, authenticate};
use crate::error::{ApiError, FreesAt};

/// The largest request body: payloads are bounded at 8 MiB of canonical JSON, and JSON as sent
/// may be larger.
pub(crate) const BODY_LIMIT: usize = 16 * 1024 * 1024;

/// The routes of one listener. Over the embedded server's socket, no key is read: the caller is
/// the operator of the tenant `local`. Over TCP, every request to `/v1` carries a key or, with
/// WorkOS on the api role, a member's access token.
pub(crate) fn router(app: Arc<App>, socket: bool, mcp: Option<Router>) -> Router {
    let api = Router::new()
        .route(routes::TOOLSETS, post(toolsets::create).get(toolsets::list))
        .route(routes::TOOLSET_DECLARATIONS, get(toolsets::declarations))
        .route(routes::TOOLSET_SEARCH, get(toolsets::search))
        .route(
            routes::WORKFLOWS,
            post(workflows::create).get(workflows::list),
        )
        .route(routes::WORKFLOW, get(workflows::versions))
        .route(routes::WORKFLOW_RECORD, post(testing::record))
        .route(routes::TEST_DOCUMENT, get(testing::get))
        .route(routes::CHECK, post(scripts::check))
        .route(routes::TEST, post(testing::test))
        .route(routes::SCRIPTS, get(stats::scripts))
        .route(routes::SCRIPT, get(reading::script))
        .route(routes::RUNS, post(runs::start).get(runs::list))
        .route(routes::RUN_STATS, get(stats::runs))
        .route(routes::RUN_FAILURES, get(stats::failures))
        .route(routes::RUN, get(runs::get).delete(runs::erase))
        .route(routes::RUN_TRACE, get(runs::trace))
        .route(routes::RUN_ENTRY, get(runs::entry))
        .route(routes::RUN_EVENTS, get(events::stream))
        .route(routes::CHANGES, get(changes::stream))
        .route(routes::RUN_SIGNAL, post(runs::signal))
        .route(routes::RUN_CANCEL, post(runs::cancel))
        .route(routes::RUN_FORK, post(runs::fork))
        .route(routes::RUN_JOURNAL, get(runs::journal))
        .route(routes::RUN_RELEASE, post(runs::release))
        .route(routes::RUN_GIVE_UP, post(runs::give_up))
        .route(routes::TASK_STREAM, get(tasks::stream))
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
        .route(routes::TOOL_STATS, get(stats::tools))
        .route(routes::PAYLOAD, get(reading::payload))
        .route(
            routes::API_KEYS,
            post(admin::create_key).get(admin::list_keys),
        )
        .route(routes::API_KEY, delete(admin::revoke_key))
        .route(routes::WHOAMI, get(organization::whoami))
        .route(
            routes::ORGANIZATIONS,
            post(organization::create_organization),
        )
        .route(
            routes::ORGANIZATION,
            patch(organization::update_organization),
        )
        .route(routes::INVITATIONS, get(organization::my_invitations))
        .route(
            routes::INVITATION_ACCEPT,
            post(organization::accept_invitation),
        )
        .route(routes::MEMBERS, get(organization::members))
        .route(routes::MEMBER, delete(organization::remove_member))
        .route(routes::MEMBER_GRANT, put(organization::set_member_grant))
        .route(
            routes::ORGANIZATION_INVITATIONS,
            get(organization::invitations).post(organization::invite),
        )
        .route(
            routes::ORGANIZATION_INVITATION,
            delete(organization::revoke_invitation),
        )
        .route(
            routes::ORGANIZATION_INVITATION_RESEND,
            post(organization::resend_invitation),
        )
        .route(routes::NAMESPACES, get(namespaces::list))
        .route(
            routes::NAMESPACE,
            get(namespaces::get)
                .put(namespaces::put)
                .patch(namespaces::patch)
                .delete(namespaces::delete),
        )
        .route(
            routes::NAMESPACE_DEFAULTS,
            get(namespaces::defaults).put(namespaces::put_defaults),
        )
        .route(routes::NAMESPACE_NAMES, get(namespaces::names))
        .route(routes::QUOTA_ALERTS, get(namespaces::alerts))
        .route(routes::ORGANIZATION_QUOTAS, put(namespaces::put_ceiling))
        .route(routes::SECRETS, get(secrets::list))
        .route(routes::SECRET, put(secrets::put).delete(secrets::delete))
        .route(routes::SECRET_USAGE, get(secrets::usage))
        .route(routes::NAMESPACE_SECRETS, get(secrets::list_of_namespace))
        .route(
            routes::NAMESPACE_SECRET,
            put(secrets::put_of_namespace).delete(secrets::delete_of_namespace),
        )
        .route(
            routes::NAMESPACE_SECRET_USAGE,
            get(secrets::usage_of_namespace),
        )
        .route(
            routes::SIGNING_KEYS,
            get(secrets::signing_keys).post(secrets::rotate_signing_key),
        )
        .route(
            routes::NAMESPACE_TOKENS,
            post(tokens::create).delete(tokens::revoke_namespace),
        )
        .route(routes::TOKEN, delete(tokens::revoke))
        .route(
            routes::TOKEN_KEYS,
            get(tokens::list_keys).post(tokens::create_key),
        )
        .route(routes::TOKEN_KEY, delete(tokens::delete_key))
        .route(routes::CONNECTIONS, get(connections::list))
        .route(
            routes::CONNECTION,
            patch(connections::update).delete(connections::revoke),
        )
        .route(
            routes::ORGANIZATION_CONNECTIONS,
            get(connections::organization_list),
        )
        .route(
            routes::ORGANIZATION_CONNECTION,
            delete(connections::organization_remove),
        )
        .route(
            routes::OAUTH_CONSENT,
            get(connections::consent).post(connections::answer),
        )
        .route(routes::ROLES, get(roles::list))
        .route(routes::ROLE, put(roles::put).delete(roles::delete))
        .route(routes::PERMISSIONS, get(admin::permissions))
        .route(routes::SERVER, get(admin::server))
        .route(routes::SERVER_STOP, post(admin::stop))
        .with_state(app.clone());
    let api = if socket {
        api.layer(Extension(Caller::local()))
    } else {
        api.layer(from_fn_with_state(app.clone(), authenticate))
    };
    // The sign-in reads its own cookies, never a Bearer token. Without WorkOS every route of it
    // answers 404 before anything reads the request, whatever its body.
    let sign_in = Router::new()
        .route(routes::AUTH_CONFIG, get(sign_in::config))
        .route(routes::AUTH_GITHUB, get(sign_in::github))
        .route(routes::AUTH_CALLBACK, get(sign_in::callback))
        .route(routes::AUTH_MAGIC_AUTH, post(sign_in::send_magic_code))
        .route(
            routes::AUTH_MAGIC_AUTH_VERIFY,
            post(sign_in::verify_magic_code),
        )
        .route(routes::AUTH_EMAIL_VERIFICATION, post(sign_in::verify_email))
        .route(
            routes::AUTH_ORGANIZATION,
            post(sign_in::select_organization),
        )
        .route(routes::AUTH_SESSION, post(sign_in::session_state))
        .route(routes::AUTH_WAITLIST, post(sign_in::waitlist))
        .route(routes::AUTH_LOGOUT, post(sign_in::logout))
        .route_layer(from_fn_with_state(app.clone(), with_sign_in))
        .with_state(app.clone());
    // The OAuth authorization server of people's connections: no Bearer, 404 without OAuth.
    let oauth = Router::new()
        .route(routes::OAUTH_AUTHORIZE, get(oauth::authorize))
        .route(routes::OAUTH_REGISTER, post(oauth::register))
        .route(routes::OAUTH_TOKEN, post(oauth::token))
        .route(routes::OAUTH_REVOKE, post(oauth::revoke))
        .route(routes::PROTECTED_RESOURCE, get(oauth::protected_resource))
        .route(
            routes::PROTECTED_RESOURCE_MCP,
            get(oauth::protected_resource),
        )
        .route(
            routes::AUTHORIZATION_SERVER,
            get(oauth::authorization_server),
        )
        .with_state(app.clone());
    // The organizations' public keys, which the servers that check signed calls fetch without
    // authentication (`archi/04-tools.md`, "Signed calls").
    let public = Router::new()
        .route(routes::HOOK, post(hooks::post))
        .route(routes::JWKS, get(secrets::jwks))
        .fallback(dashboard::serve)
        .with_state(app.clone())
        .merge(sign_in)
        .merge(oauth)
        .merge(mcp.unwrap_or_default());
    api.merge(public)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(from_fn(refuse_oversized))
        .layer(from_fn_with_state(app, every_response))
}

/// A body whose `Content-Length` is above the limit: 413 `payload-too-large` at once, before
/// any handler reads it. The body is never polled, so a client that sent
/// `Expect: 100-continue` gets the refusal instead of `100 Continue`, and uploads nothing.
async fn refuse_oversized(request: Request, next: Next) -> Response {
    let length = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    match length {
        Some(length) if length > BODY_LIMIT as u64 => ApiError::new(
            ProblemType::PayloadTooLarge,
            format!(
                "the body is {length} bytes, above this server's limit of {} MiB: send less, and \
                 put large values in fewer, smaller requests",
                BODY_LIMIT / (1024 * 1024)
            ),
        )
        .into_response(),
        _ => next.run(request).await,
    }
}

/// The routes of `/auth` on a server without WorkOS: 404 `not-found`, before a body or a cookie
/// is read (`archi/08-decisions.md`: without WorkOS, `/auth/*` answers 404).
async fn with_sign_in(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    match app.sign_in() {
        Ok(_) => next.run(request).await,
        Err(error) => error.into_response(),
    }
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

/// Every response carries `Rewind-Version`, and a monthly quota's refusal `Retry-After`: the
/// seconds left in the month, by the server's clock. Every request pushes back the embedded
/// server's idle stop.
async fn every_response(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let busy = app.idle.busy();
    let mut response = next.run(request).await;
    drop(busy);
    response.headers_mut().insert(
        REWIND_VERSION,
        HeaderValue::from_static(app.settings.version),
    );
    if let Some(FreesAt(at)) = response.extensions().get::<FreesAt>().copied() {
        let seconds = at
            .as_millis()
            .saturating_sub(app.now().as_millis())
            .div_ceil(1000);
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(seconds.max(1)));
    }
    response
}
