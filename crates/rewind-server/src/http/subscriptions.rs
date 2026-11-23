//! Subscriptions to lifecycle webhooks (`POST`, `GET`, `DELETE /v1/subscriptions`).

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rewind_api::access::Permission;
use rewind_api::common::Labels;
use rewind_api::webhooks::{
    CreateSubscription, Subscription as ApiSubscription, SubscriptionList, WebhookEventType,
    WebhookSecret,
};
use rewind_core::RunId;
use rewind_engine::adapters::SystemEntropy;
use rewind_engine::ports::{CatalogStore, Entropy, Secret, Subscription};
use rewind_engine::text::{refuse_nul, refuse_nul_in_labels};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::Body;

/// The prefix of a subscription's id, followed by a UUID v7: the id tells when the
/// subscription was created.
const PREFIX: &str = "sub_";

/// `POST /v1/subscriptions`.
pub(super) async fn create(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<CreateSubscription>,
) -> Result<Response, ApiError> {
    caller.require(Permission::SubscriptionsWrite)?;
    // The request is validated before the URL's address is resolved: a malformed request
    // is refused without any network access.
    refuse_nul("the url", &request.url)?;
    let labels = caller.within(request.labels.0)?;
    refuse_nul_in_labels(&labels)?;
    app.settings
        .webhook_egress
        .check_subscription(&request.url)
        .await
        .map_err(|reason| ApiError::invalid(format!("url {:?}: {reason}", request.url)))?;
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(ApiError::internal)?;
    let secret = WebhookSecret::from_bytes(key).map_err(ApiError::internal)?;
    let id = format!("{PREFIX}{}", SystemEntropy.run_id());
    let subscription = Subscription {
        tenant: caller.tenant.clone(),
        id,
        url: request.url,
        secret: Secret::new(secret.expose()),
        events: request.events.iter().map(|kind| port_type(*kind)).collect(),
        labels,
    };
    app.store.put_subscription(&subscription).await?;
    let mut view = view(&subscription);
    view.secret = Some(secret.expose());
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

/// `GET /v1/subscriptions`: without their secrets.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<SubscriptionList>, ApiError> {
    caller.require(Permission::SubscriptionsRead)?;
    let subscriptions = app
        .store
        .subscriptions(&caller.tenant)
        .await?
        .iter()
        .filter(|subscription| caller.sees(&subscription.labels))
        .map(view)
        .collect();
    Ok(Json(SubscriptionList { subscriptions }))
}

/// `DELETE /v1/subscriptions/{id}`.
pub(super) async fn delete(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::SubscriptionsWrite)?;
    let missing = || ApiError::not_found(format!("no subscription {id}"));
    let visible = app
        .store
        .subscriptions(&caller.tenant)
        .await?
        .iter()
        .any(|subscription| subscription.id == id && caller.sees(&subscription.labels));
    if !visible || !app.store.delete_subscription(&caller.tenant, &id).await? {
        return Err(missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

fn port_type(kind: WebhookEventType) -> rewind_engine::ports::WebhookEventType {
    use rewind_engine::ports::WebhookEventType as Port;
    match kind {
        WebhookEventType::RunCompleted => Port::RunCompleted,
        WebhookEventType::RunFailed => Port::RunFailed,
        WebhookEventType::RunCancelled => Port::RunCancelled,
        WebhookEventType::RunAborted => Port::RunAborted,
        WebhookEventType::RunSuspended => Port::RunSuspended,
        WebhookEventType::RunQuarantined => Port::RunQuarantined,
        WebhookEventType::ToolSchemaChanged => Port::ToolSchemaChanged,
        WebhookEventType::CredentialsExpired => Port::CredentialsExpired,
        WebhookEventType::QuotaAlert => Port::QuotaAlert,
    }
}

fn api_type(kind: rewind_engine::ports::WebhookEventType) -> WebhookEventType {
    WebhookEventType::ALL
        .into_iter()
        .find(|api| api.as_str() == kind.as_str())
        .unwrap_or(WebhookEventType::RunFailed)
}

fn view(subscription: &Subscription) -> ApiSubscription {
    let events: BTreeSet<WebhookEventType> = subscription
        .events
        .iter()
        .map(|kind| api_type(*kind))
        .collect();
    let created_at = subscription
        .id
        .strip_prefix(PREFIX)
        .and_then(|id| id.parse::<RunId>().ok())
        .and_then(|id| id.as_uuid().get_timestamp())
        .map_or(jiff::Timestamp::UNIX_EPOCH, |at| {
            let (seconds, nanos) = at.to_unix();
            jiff::Timestamp::new(
                i64::try_from(seconds).unwrap_or(0),
                i32::try_from(nanos).unwrap_or(0),
            )
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
        });
    ApiSubscription {
        id: subscription.id.clone(),
        url: subscription.url.clone(),
        events: events.into_iter().collect(),
        labels: Labels::from(subscription.labels.clone()),
        created_at,
        secret: None,
    }
}
