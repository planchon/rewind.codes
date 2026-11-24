//! Inbound webhooks: `POST /hooks/{token}`, at the unguessable URL `webhook(name)` returned to a
//! code. Whatever is posted becomes a signal on the run (`archi/01-product.md`, "Signals").
//! No API key: the token is the credential, and it names its run: any process routes it to the
//! run's owner, which resolves it.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rewind_api::headers::IdempotencyKey;
use rewind_api::webhooks::DELIVERY_ID_HEADERS;
use rewind_engine::edge::EdgeError;
use rewind_engine::ports::Enqueued;
use rewind_engine::ports::link::Refusal;
use serde_json::Value;

use crate::app::App;
use crate::error::ApiError;
use crate::http::runs::check_payload_size;

/// `POST /hooks/{token}`: a JSON body is the payload, another body a string, an empty one
/// `null`. The first of `Idempotency-Key`, `webhook-id` and `X-GitHub-Delivery` is the
/// signal's idempotency key.
pub(super) async fn post(
    State(app): State<Arc<App>>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let missing = || ApiError::not_found("no webhook at this URL: its run may have ended");
    // No token holds a NUL character, and Postgres refuses it in a query.
    if token.contains('\0') {
        return Err(missing());
    }
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("json"));
    let payload = if body.iter().all(u8::is_ascii_whitespace) {
        Value::Null
    } else if json {
        serde_json::from_slice(&body)
            .map_err(|error| ApiError::invalid(format!("the body is not valid JSON: {error}")))?
    } else {
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
    };
    check_payload_size(&app, &payload)?;
    let key = DELIVERY_ID_HEADERS.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| IdempotencyKey::new(value).ok())
            .map(|key| key.as_str().to_owned())
    });
    match app
        .edge
        .webhook(&mut app.gateway(), &token, payload, key)
        .await
    {
        Ok(Enqueued::RunEnded) | Err(EdgeError::Refused(Refusal::RunEnded(_))) => {
            Err(ApiError::new(
                rewind_api::ProblemType::RunEnded,
                "the run this webhook belongs to has ended",
            ))
        }
        Ok(Enqueued::Accepted | Enqueued::Duplicate) => Ok(StatusCode::ACCEPTED.into_response()),
        Err(EdgeError::Refused(Refusal::RunNotFound(_) | Refusal::TaskGone)) => Err(missing()),
        Err(error) => Err(error.into()),
    }
}
