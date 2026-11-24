//! The HTTP client of a connection to a streamable HTTP server: where it may connect, which
//! redirects it follows, and how each of its requests proves who calls (`archi/04-tools.md`,
//! "Where MCP servers and APIs may be" and "Signed calls").
//!
//! Each connection has a client of its own, from `rewind-outbound`: checked when it opens and at
//! each TCP connection it makes, where [`McpConfig::egress`](crate::McpConfig) lets it go. A
//! connection that reached an address only the operator's allow-list lets through is internal:
//! its failures never quote what the server answered over HTTP ([`crate::failure`]).
//!
//! | The server's `auth` | Each HTTP request carries | Redirects |
//! | --- | --- | --- |
//! | none | Nothing | 307 and 308 within the server's origin, up to 5 |
//! | a secret in a header | The header, as its `format` and `encoding` say | The same: the header never leaves the origin |
//! | `rewind` | A JWT of its own ([`SigningClient`]): the call's claims on the `tools/call` of a call, the reduced set on the others (`initialize`, listings, notifications, the stream, the session's end) | None: a JWT is bound to its URL; a redirect is refused, naming why |

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use futures_util::stream::BoxStream;
use reqwest::header::{HeaderName, HeaderValue};
use rewind_core::ActionFailureKind;
use rewind_outbound::{
    ActionEgress, CallIdentity, Reached, Redirects, SignedRequest, Signer, SigningError,
};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use serde_json::Value;
use sse_stream::Sse;
use url::Url;

use crate::IDEMPOTENCY_KEY_META;
use crate::failure::{Answers, Failure};

/// Whether a connection reached an internal server: its failures then quote nothing it
/// answered.
#[derive(Debug, Clone, Default)]
pub(crate) struct Internal(Option<Reached>);

impl Internal {
    /// Whether failures may quote what the server answered.
    pub(crate) fn answers(&self) -> Answers {
        if self.0.as_ref().is_some_and(Reached::internal) {
            Answers::Withheld
        } else {
            Answers::Quoted
        }
    }
}

/// The client of one connection to the server `server` at `url`. Refused when `url` is not an
/// `http` or `https` URL, or when its host is an address the egress refuses.
pub(crate) fn client(
    server: &str,
    url: &str,
    egress: &ActionEgress,
    redirects: Redirects,
) -> Result<(reqwest::Client, Internal), Failure> {
    let refused = |reason: String| {
        Failure::permanent(
            ActionFailureKind::Unavailable,
            format!("{server} cannot be reached: {reason}"),
            None,
        )
    };
    let parsed =
        Url::parse(url).map_err(|error| refused(format!("{url:?} is not a URL: {error}")))?;
    // Without an idle pool, as rmcp's own client: a response not read to its end would stall
    // the next request on the same connection.
    let builder = reqwest::Client::builder().pool_max_idle_per_host(0);
    let (client, reached) = egress
        .client(&parsed, redirects, builder)
        .map_err(refused)?;
    Ok((client, Internal(Some(reached))))
}

/// Signs the requests of one session, as the tenant: those of a call with its claims, the others
/// with the reduced set. A call registers its claims while it is in flight
/// ([`SessionSigner::register`]), and its `tools/call` request is recognized by the idempotency
/// key in its `_meta`.
#[derive(Debug)]
pub(crate) struct SessionSigner {
    signer: Signer,
    issuer: String,
    /// `aud`: the server's URL.
    audience: String,
    /// The calls in flight on the session, by idempotency key.
    calls: Mutex<HashMap<String, CallIdentity>>,
}

impl SessionSigner {
    pub(crate) fn new(signer: Signer, issuer: String, audience: String) -> Self {
        Self {
            signer,
            issuer,
            audience,
            calls: Mutex::new(HashMap::new()),
        }
    }

    /// Signs the session's `tools/call` with the idempotency key `key` with `identity` until the
    /// guard is dropped.
    pub(crate) fn register(self: &Arc<Self>, key: String, identity: CallIdentity) -> CallGuard {
        let jti = identity.jti.clone();
        self.lock().insert(key.clone(), identity);
        CallGuard {
            signer: self.clone(),
            key,
            jti,
        }
    }

    /// The JWT of a request of the session.
    fn token(&self, method: &str, uri: &str, body: Option<&[u8]>) -> Result<String, SigningError> {
        let url = Url::parse(uri)
            .map_err(|error| SigningError(format!("{uri:?} is not a URL: {error}")))?;
        let call = body
            .and_then(call_key)
            .and_then(|key| self.lock().get(&key).cloned());
        self.signer.sign(
            &self.issuer,
            call.as_ref(),
            &SignedRequest {
                audience: &self.audience,
                method,
                url: &url,
                body,
            },
            SystemTime::now(),
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CallIdentity>> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Keeps a call's claims on its session while it is in flight.
#[derive(Debug)]
pub(crate) struct CallGuard {
    signer: Arc<SessionSigner>,
    key: String,
    jti: String,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        let mut calls = self.signer.lock();
        // Another delivery of the same call may have registered since: only this one goes.
        if calls
            .get(&self.key)
            .is_some_and(|identity| identity.jti == self.jti)
        {
            calls.remove(&self.key);
        }
    }
}

/// The idempotency key of a `tools/call` request's `_meta`, if the message is one.
fn call_key(body: &[u8]) -> Option<String> {
    let message: Value = serde_json::from_slice(body).ok()?;
    if message.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    message
        .pointer("/params/_meta")?
        .get(IDEMPOTENCY_KEY_META)?
        .as_str()
        .map(str::to_owned)
}

/// The client of a signed session: rmcp's reqwest client, each request with a JWT of its own in
/// `Authorization: Bearer`. The body it signs is the one reqwest sends: `serde_json::to_vec` of
/// the message, as `RequestBuilder::json` writes it.
#[derive(Debug, Clone)]
pub(crate) struct SigningClient {
    pub(crate) inner: reqwest::Client,
    pub(crate) signer: Arc<SessionSigner>,
}

/// A failure to sign, as the transport's error: its message never holds the key.
fn unsigned(error: SigningError) -> StreamableHttpError<reqwest::Error> {
    StreamableHttpError::Io(std::io::Error::other(error))
}

impl StreamableHttpClient for SigningClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let body = serde_json::to_vec(&message)?;
        let token = self
            .signer
            .token("POST", &uri, Some(&body))
            .map_err(unsigned)?;
        self.inner
            .post_message(uri, message, session_id, Some(token), custom_headers)
            .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let body = serde_json::to_vec(&message)?;
        let token = self
            .signer
            .token("POST", &uri, Some(&body))
            .map_err(unsigned)?;
        self.inner
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                Some(token),
                custom_headers,
                max_sse_event_size,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let token = self.signer.token("DELETE", &uri, None).map_err(unsigned)?;
        self.inner
            .delete_session(uri, session_id, Some(token), custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let token = self.signer.token("GET", &uri, None).map_err(unsigned)?;
        self.inner
            .get_stream(uri, session_id, last_event_id, Some(token), custom_headers)
            .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let token = self.signer.token("GET", &uri, None).map_err(unsigned)?;
        self.inner
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                Some(token),
                custom_headers,
                max_sse_event_size,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_call_is_recognized_by_its_idempotency_key() {
        let call = json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "lookup", "_meta": { IDEMPOTENCY_KEY_META: "run/3" } }
        });
        assert_eq!(
            call_key(&serde_json::to_vec(&call).unwrap()),
            Some("run/3".into())
        );
        let listing = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" });
        assert_eq!(call_key(&serde_json::to_vec(&listing).unwrap()), None);
        assert_eq!(call_key(b"not json"), None);
    }

    #[test]
    fn only_http_urls_are_reached() {
        for url in ["ftp://mcp.example.com/", "not a url", "file:///etc/passwd"] {
            let Err(Failure::Permanent(failure)) =
                client("fx", url, &ActionEgress::default(), Redirects::WithinOrigin)
            else {
                panic!("{url} fails for good");
            };
            assert!(
                failure.message.starts_with("fx cannot be reached: "),
                "{failure:?}"
            );
        }
        let checked = ActionEgress::checked(
            rewind_outbound::InternalAllowList::parse(&["10.0.0.0/8"]).unwrap(),
        );
        let (_, internal) = client(
            "fx",
            "http://10.1.2.3/mcp",
            &checked,
            Redirects::WithinOrigin,
        )
        .unwrap();
        assert_eq!(internal.answers(), Answers::Withheld);
        let (_, public) = client(
            "fx",
            "https://mcp.example.com/mcp",
            &checked,
            Redirects::WithinOrigin,
        )
        .unwrap();
        assert_eq!(public.answers(), Answers::Quoted);
    }
}
