//! How requests are read: JSON bodies, query strings with repeated keys, `Idempotency-Key`.
//! Whatever cannot be read is a problem (`invalid-request`, `unsupported-media-type`,
//! `payload-too-large`) with a detail that says what to fix.

use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::header;
use axum::http::request::Parts;
use ptc_api::ProblemType;
use ptc_api::headers::{IDEMPOTENCY_KEY, IdempotencyKey};
use serde::de::DeserializeOwned;

use crate::error::ApiError;

/// A JSON body, required.
#[derive(Debug)]
pub(crate) struct Body<T>(pub(crate) T);

/// A JSON body that may be absent: its default then.
#[derive(Debug)]
pub(crate) struct OptionalBody<T>(pub(crate) T);

/// The query string, read as `serde_html_form` does: a list repeats its key.
#[derive(Debug)]
pub(crate) struct Query<T>(pub(crate) T);

/// The `Idempotency-Key` header, if any.
#[derive(Debug)]
pub(crate) struct Idempotency(pub(crate) Option<String>);

async fn bytes<S: Send + Sync>(request: Request, state: &S) -> Result<(bool, Bytes), ApiError> {
    let json = match request.headers().get(header::CONTENT_TYPE) {
        None => true,
        Some(value) => value.to_str().is_ok_and(|value| {
            let media = value.split(';').next().unwrap_or_default().trim();
            media.eq_ignore_ascii_case("application/json") || media.ends_with("+json")
        }),
    };
    let bytes = Bytes::from_request(request, state)
        .await
        .map_err(|rejection| {
            if rejection.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(
                    ProblemType::PayloadTooLarge,
                    "the body is larger than this server accepts",
                )
            } else {
                ApiError::invalid(format!("the body cannot be read: {rejection}"))
            }
        })?;
    Ok((json, bytes))
}

fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes)
        .map_err(|error| ApiError::invalid(format!("the body is not valid: {error}")))
}

fn not_json() -> ApiError {
    ApiError::new(
        ProblemType::UnsupportedMediaType,
        "the body must be JSON (Content-Type: application/json)",
    )
}

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        let (json, bytes) = bytes(request, state).await?;
        if !json {
            return Err(not_json());
        }
        if bytes.is_empty() {
            return Err(ApiError::invalid(
                "the body is empty: a JSON object is expected",
            ));
        }
        parse(&bytes).map(Body)
    }
}

impl<S: Send + Sync, T: DeserializeOwned + Default> FromRequest<S> for OptionalBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, ApiError> {
        let (json, bytes) = bytes(request, state).await?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(OptionalBody(T::default()));
        }
        if !json {
            return Err(not_json());
        }
        parse(&bytes).map(OptionalBody)
    }
}

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Query<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        let query = parts.uri.query().unwrap_or_default();
        serde_html_form::from_str(query)
            .map(Query)
            .map_err(|error| ApiError::invalid(format!("the query string is not valid: {error}")))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Idempotency {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        let Some(value) = parts.headers.get(IDEMPOTENCY_KEY) else {
            return Ok(Idempotency(None));
        };
        let text = value
            .to_str()
            .map_err(|_| ApiError::invalid("Idempotency-Key: visible ASCII characters only"))?;
        let key = IdempotencyKey::new(text)
            .map_err(|error| ApiError::invalid(format!("Idempotency-Key: {error}")))?;
        Ok(Idempotency(Some(key.as_str().to_owned())))
    }
}
