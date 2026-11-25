//! Reading for the dashboard: a payload on demand (`archi/05-testing-and-observability.md`, "Read
//! model" and "Dashboard"). The lists and statistics drawn from the counters are in
//! [`super::stats`].

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use rewind_api::access::Permission;
use rewind_api::reading::PayloadQuery;
use rewind_core::{Digest, canonical_json};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::Query;

/// `GET /v1/payloads/{digest}?run=…`: a payload's value, read through the run that holds it.
pub(super) async fn payload(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(digest): Path<String>,
    Query(query): Query<PayloadQuery>,
) -> Result<Response, ApiError> {
    caller.require(Permission::RunsRead)?;
    let parsed = Digest::from_hex(&digest)
        .map_err(|_| ApiError::not_found(format!("no payload {digest} in this organization")))?;
    let run = &query.run;
    let view = app.run(&caller, run).await?;
    let value = app
        .views()
        .value(&view.head.run, parsed)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!("no payload {digest} in the history of run {run}"))
        })?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
        ],
        canonical_json(&value),
    )
        .into_response())
}
