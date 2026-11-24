//! An OpenAPI document read from its URL, once, at registration (`archi/04-tools.md`, "API
//! actions"): through the egress checks of the deployment (`rewind-outbound`), at most
//! [`MAX_DOCUMENT_BYTES`], within [`FETCH_TIMEOUT`]. No secret goes with it, so a redirect
//! is followed wherever it leads, each new host checked again, up to 5 times. What an internal
//! server answers is never quoted: an error gives the HTTP status, not the body.

use std::time::Duration;

use rewind_outbound::{ActionEgress, Egress, Redirects};
use serde_json::Value;
use url::Url;

/// The largest document read: 2 MiB.
pub const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;

/// How long reading a document may take.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// A document read from its URL.
#[derive(Debug, Clone, PartialEq)]
pub struct Fetched {
    pub document: Value,
    /// Where it was read, after redirects: a relative server URL is resolved against it.
    pub url: Url,
    /// It came from an address only the operator's allow-list lets through: what it says is
    /// not quoted in errors.
    pub internal: bool,
}

/// Why a document could not be read: written for the harness developer, never quoting what an
/// internal server answered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FetchError(pub String);

/// Reads the JSON document at `url`, where `egress` lets it go.
pub async fn fetch(url: &str, egress: &ActionEgress) -> Result<Fetched, FetchError> {
    let fail = |reason: String| FetchError(format!("the OpenAPI document {url} {reason}"));
    let parsed = Url::parse(url).map_err(|error| fail(format!("is not a URL: {error}")))?;
    let builder = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .connect_timeout(FETCH_TIMEOUT)
        .user_agent(concat!("Rewind/", env!("CARGO_PKG_VERSION")));
    let (client, reached) = egress
        .client(&parsed, Redirects::Checked, builder)
        .map_err(|reason| fail(format!("cannot be read: {reason}")))?;
    let mut response = client
        .get(parsed)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| {
            fail(match Egress::refusal_of(&error) {
                Some(reason) => format!("cannot be read: {reason}"),
                None if error.is_timeout() => {
                    format!("did not answer within {} s", FETCH_TIMEOUT.as_secs())
                }
                None => format!("cannot be reached: {}", without_url(&error)),
            })
        })?;
    let internal = reached.internal();
    let status = response.status();
    if !status.is_success() {
        return Err(fail(format!("answered HTTP {}", status.as_u16())));
    }
    let final_url = response.url().clone();
    let too_large = || fail(format!("is larger than {} MiB", MAX_DOCUMENT_BYTES >> 20));
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DOCUMENT_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        fail(format!(
            "could not be read to its end: {}",
            without_url(&error)
        ))
    })? {
        if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    let document = serde_json::from_slice(&body).map_err(|error| {
        fail(if internal {
            "is not JSON; the answer of an internal server is not quoted".to_owned()
        } else {
            format!("is not JSON: {error}")
        })
    })?;
    Ok(Fetched {
        document,
        url: final_url,
        internal,
    })
}

/// A client's error without the URL reqwest adds, which the message already names.
fn without_url(error: &reqwest::Error) -> String {
    let mut parts = Vec::new();
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(current) = source {
        parts.push(current.to_string());
        source = current.source();
    }
    if parts.is_empty() {
        error.to_string()
    } else {
        parts.join(": ")
    }
}
