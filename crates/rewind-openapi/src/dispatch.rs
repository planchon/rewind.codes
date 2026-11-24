//! `ApiDispatcher`, the `ActionDispatcher` of the `api` route (`archi/04-tools.md`, "API actions").
//!
//! A call:
//!
//! 1. The request is built from the args ([`crate::request`]), with `traceparent` and
//!    `tracestate`, `Idempotency-Key: {run_id}/{seq}` when the operation declares it, and the
//!    authentication the engine resolved: the secret used in its header, or a JWT signed
//!    with the tenant's key for this request (`archi/04-tools.md`, "Signed calls").
//! 2. It goes through the egress checks of `rewind-outbound`: where [`ApiConfig::egress`] lets
//!    it go, a redirect followed only within the API's origin with a 307 or a 308, up to 5
//!    times, each request signed again for its URL.
//! 3. The answer decides the outcome:
//!
//! | Answer | Outcome |
//! | --- | --- |
//! | `2xx` | `Result`: the JSON body, `null` for an empty one, `{ contentType, text }` for another type (`{ contentType, base64 }` when it is not UTF-8); larger than 5 MB, `Failed` |
//! | `401` | `Park(credentials_refused)` |
//! | `408`, `429`, `5xx`, a failed connection, no answer within the execution timeout | `Transient`, after its `Retry-After` when it ends within the execution timeout; a longer one is in the failure's `data` (`retryAfter`, in seconds) |
//! | An address the egress checks refuse, a redirect elsewhere | `Failed` (`unavailable`), for good |
//! | Another status | `Failed`: the status, and the body's `title` and `detail` when it is `application/problem+json`, else its first 2 KB |
//!
//! What an API reached at an address only the operator's allow-list lets through answers is
//! never quoted: its failures hold the status alone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use rewind_core::{ActionFailure, ActionFailureKind, ActionRoute, Api, ApiOperation, HttpMethod};
use rewind_engine::ports::{
    ActionDispatcher, Call, CallAuth, CallClaims, Dispatched, ParkKind, ParkReason,
};
use rewind_outbound::{
    ActionEgress, CallIdentity, Egress, MAX_REDIRECTS, Placed, Reached, Redirects, SignedRequest,
    Signer, place,
};
use serde_json::{Value, json};
use url::Url;

use crate::read::is_json;
use crate::request::{self, Built};

/// The `data` key of a transient failure's `Retry-After`, in seconds, when the dispatcher could
/// not wait it out within the execution timeout: the engine does not retry before.
pub use rewind_engine::ports::RETRY_AFTER_DATA;

/// Bytes of a failed answer's body quoted in its failure.
const QUOTED_BODY: usize = 2048;

/// How [`ApiDispatcher`] sends its requests.
#[derive(Debug, Clone)]
pub struct ApiConfig {
    /// Where APIs may be: with the egress checks of a deployment by default, anywhere for the
    /// embedded server.
    pub egress: ActionEgress,
    /// Bytes of a successful answer at most: 5 MB.
    pub max_response_bytes: usize,
    /// How long a connection may take to open.
    pub connect_timeout: Duration,
    /// The longest `Retry-After` the dispatcher waits out itself, within the execution timeout.
    pub max_retry_wait: Duration,
    /// The clients kept, one per origin of each tenant; beyond, they are made again.
    pub max_clients: usize,
    pub user_agent: String,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            egress: ActionEgress::default(),
            max_response_bytes: 5 * 1024 * 1024,
            connect_timeout: Duration::from_secs(30),
            max_retry_wait: Duration::from_secs(60),
            max_clients: 256,
            user_agent: format!("Rewind/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// Calls the operations of APIs: the `api` route of the engine's `RoutingDispatcher`. Cheap to
/// clone: clones share their clients.
#[derive(Debug, Clone, Default)]
pub struct ApiDispatcher {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    config: ApiConfig,
    /// By tenant and origin: the client, and whether it reached an internal address.
    clients: Mutex<HashMap<(String, String), (reqwest::Client, Reached)>>,
}

impl ApiDispatcher {
    pub fn new(config: ApiConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                clients: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The client of the API's origin for the tenant.
    fn client(&self, tenant: &str, url: &Url) -> Result<(reqwest::Client, Reached), String> {
        let key = (tenant.to_owned(), url.origin().ascii_serialization());
        let mut clients = self
            .inner
            .clients
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let config = &self.inner.config;
        let builder = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .user_agent(config.user_agent.as_str());
        let client = config.egress.client(url, Redirects::Manual, builder)?;
        if clients.len() >= config.max_clients {
            clients.clear();
        }
        clients.insert(key, client.clone());
        Ok(client)
    }

    async fn attempt(&self, call: &Call, auth: CallAuth, deadline: Option<Instant>) -> Dispatched {
        let action = call.action.name.as_str();
        let (api, operation) = match (&call.api, &call.action.operation, &call.action.route) {
            (Some(api), Some(operation), ActionRoute::Api { api: name }) if api.name == *name => {
                (api, operation)
            }
            (_, _, ActionRoute::Api { api: name }) => {
                return failed(
                    ActionFailureKind::Unavailable,
                    format!("the action set does not say how to call the API {name} for {action}"),
                    None,
                );
            }
            _ => {
                return failed(
                    ActionFailureKind::Unavailable,
                    format!("{action} is not an API action"),
                    None,
                );
            }
        };
        let mut built = match request::build(api, operation, &call.args) {
            Ok(built) => built,
            Err(reason) => {
                return failed(
                    ActionFailureKind::Error,
                    format!("the arguments of {action}: {reason}"),
                    None,
                );
            }
        };
        if let Some(trace) = &call.trace_context {
            add(&mut built.headers, "traceparent", &trace.traceparent);
            if let Some(state) = &trace.tracestate {
                add(&mut built.headers, "tracestate", state);
            }
        }
        if operation.idempotency_key {
            add(&mut built.headers, "idempotency-key", &call.idempotency_key);
        }
        if built.body.is_some() {
            add(&mut built.headers, "content-type", "application/json");
        }
        add(
            &mut built.headers,
            "accept",
            "application/json, application/problem+json;q=0.9, */*;q=0.8",
        );
        let signing = match auth {
            CallAuth::None => None,
            CallAuth::Secret { value, placement } => match place(&placement, value.expose()) {
                Ok(Placed::Header { name, value }) => {
                    built.headers.push((name, value));
                    None
                }
                Ok(Placed::Env { .. }) => {
                    return failed(
                        ActionFailureKind::Unavailable,
                        format!("{}: an API takes its secret in a header", api.name),
                        None,
                    );
                }
                Err(error) => {
                    return failed(
                        ActionFailureKind::Unavailable,
                        format!("{}: {error}", api.name),
                        None,
                    );
                }
            },
            CallAuth::Signed { key, claims } => {
                match Signer::new(&key.public.kid, key.private.expose()) {
                    Ok(signer) => Some((signer, claims)),
                    Err(error) => {
                        return failed(
                            ActionFailureKind::Unavailable,
                            format!("{}: {error}", api.name),
                            None,
                        );
                    }
                }
            }
        };
        let (client, reached) = match self.client(call.run.tenant.as_str(), &built.url) {
            Ok(client) => client,
            Err(reason) => {
                return failed(
                    ActionFailureKind::Unavailable,
                    format!("{} cannot be reached: {reason}", api.name),
                    None,
                );
            }
        };
        let sent = Sending {
            api,
            operation,
            action,
            client: &client,
            signing: signing.as_ref(),
        };
        let response = match sent.send(built).await {
            Ok(response) => response,
            Err(dispatched) => return dispatched,
        };
        let answered = Answered {
            api: &api.name,
            action,
            internal: reached.internal(),
            max_bytes: self.inner.config.max_response_bytes,
        };
        let dispatched = answered.outcome(response).await;
        self.honor_retry_after(dispatched, deadline).await
    }

    /// Waits out a transient failure's `Retry-After` when it ends within the execution timeout
    /// and [`ApiConfig::max_retry_wait`]; leaves it in the failure's data otherwise.
    async fn honor_retry_after(
        &self,
        dispatched: Dispatched,
        deadline: Option<Instant>,
    ) -> Dispatched {
        let Dispatched::Transient(mut failure) = dispatched else {
            return dispatched;
        };
        let Some(wait) = failure
            .data
            .as_ref()
            .and_then(|data| data.get(RETRY_AFTER_DATA))
            .and_then(Value::as_u64)
            .map(Duration::from_secs)
        else {
            return Dispatched::Transient(failure);
        };
        let fits = deadline
            .is_none_or(|deadline| Instant::now() + wait + Duration::from_secs(1) < deadline);
        if fits && wait <= self.inner.config.max_retry_wait {
            tokio::time::sleep(wait).await;
            if let Some(Value::Object(data)) = &mut failure.data {
                data.remove(RETRY_AFTER_DATA);
            }
        }
        Dispatched::Transient(failure)
    }
}

impl ActionDispatcher for ApiDispatcher {
    async fn dispatch(&self, call: Call, auth: CallAuth) -> Dispatched {
        let timeout = call.action.policy.execution_timeout;
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let attempt = self.attempt(&call, auth, deadline);
        let dispatched = match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, attempt).await {
                Ok(dispatched) => dispatched,
                Err(_) => Dispatched::Transient(failure(
                    ActionFailureKind::Timeout,
                    format!(
                        "{} did not answer within {}",
                        call.action.name,
                        human(timeout)
                    ),
                    None,
                )),
            },
            None => attempt.await,
        };
        tracing::debug!(
            run = %call.run,
            seq = call.seq.0,
            action = %call.action.name,
            attempt = call.delivery,
            outcome = match &dispatched {
                Dispatched::Result(_) => "result",
                Dispatched::Failed(_) => "failed",
                Dispatched::Transient(_) => "transient",
                Dispatched::Deferred => "deferred",
                Dispatched::Park(_) => "parked",
            },
            "API call"
        );
        dispatched
    }
}

/// One call's requests: the first, and those of the redirects followed.
struct Sending<'a> {
    api: &'a Api,
    operation: &'a ApiOperation,
    action: &'a str,
    client: &'a reqwest::Client,
    signing: Option<&'a (Signer, CallClaims)>,
}

impl Sending<'_> {
    /// Sends the request, following 307 and 308 within the API's origin, each request signed for
    /// its URL. The response, or the outcome of a request that got none.
    async fn send(&self, built: Built) -> Result<reqwest::Response, Dispatched> {
        let method = method(self.operation.method);
        let origin = built.url.origin();
        let mut url = built.url;
        for _ in 0..=MAX_REDIRECTS {
            let mut request = self.client.request(method.clone(), url.clone());
            for (name, value) in &built.headers {
                request = request.header(name, value);
            }
            if let Some((signer, claims)) = self.signing {
                let token = signer
                    .sign(
                        &claims.issuer,
                        Some(&identity(claims)),
                        &SignedRequest {
                            audience: &self.api.base_url,
                            method: method.as_str(),
                            url: &url,
                            body: built.body.as_deref(),
                        },
                        SystemTime::now(),
                    )
                    .map_err(|error| {
                        failed(
                            ActionFailureKind::Unavailable,
                            format!("{}: {error}", self.api.name),
                            None,
                        )
                    })?;
                let mut value =
                    HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
                        failed(
                            ActionFailureKind::Unavailable,
                            format!("{}: the signature is not a header value", self.api.name),
                            None,
                        )
                    })?;
                value.set_sensitive(true);
                request = request.header(header::AUTHORIZATION, value);
            }
            if let Some(body) = &built.body {
                request = request.body(body.clone());
            }
            let response = request.send().await.map_err(|error| self.unsent(&error))?;
            let status = response.status();
            if !matches!(
                status,
                StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
            ) {
                return Ok(response);
            }
            let next = response
                .headers()
                .get(header::LOCATION)
                .and_then(|location| location.to_str().ok())
                .and_then(|location| url.join(location).ok());
            match next {
                Some(next) if next.origin() == origin => url = next,
                _ => {
                    return Err(failed(
                        ActionFailureKind::Unavailable,
                        format!(
                            "{} cannot be called: it redirects {} to another origin, and its \
                             calls go to its own URL only",
                            self.api.name, self.action
                        ),
                        Some(json!({ "status": status.as_u16() })),
                    ));
                }
            }
        }
        Err(failed(
            ActionFailureKind::Unavailable,
            format!(
                "{} cannot be called: it redirects {} more than {MAX_REDIRECTS} times",
                self.api.name, self.action
            ),
            None,
        ))
    }

    /// A request that got no answer: refused by the egress checks, for good; otherwise
    /// transient.
    fn unsent(&self, error: &reqwest::Error) -> Dispatched {
        if let Some(reason) = Egress::refusal_of(error) {
            return failed(
                ActionFailureKind::Unavailable,
                format!("{} cannot be reached: {reason}", self.api.name),
                None,
            );
        }
        let kind = if error.is_timeout() {
            ActionFailureKind::Timeout
        } else {
            ActionFailureKind::Unavailable
        };
        let what = if error.is_connect() {
            "could not be reached"
        } else {
            "did not answer"
        };
        Dispatched::Transient(failure(
            kind,
            format!(
                "{} {what} for {}: {}",
                self.api.name,
                self.action,
                chain(error)
            ),
            None,
        ))
    }
}

/// The claims of the request of a call.
fn identity(claims: &CallClaims) -> CallIdentity {
    CallIdentity {
        subject: claims.subject.as_str().to_owned(),
        labels: claims.labels.clone(),
        run: claims.run.to_string(),
        action: claims.action.clone(),
        idempotency_key: claims.idempotency_key.clone(),
        jti: claims.jti(),
    }
}

/// How an answer becomes an outcome.
struct Answered<'a> {
    api: &'a str,
    action: &'a str,
    /// The API was reached at an internal address: its answers are not quoted.
    internal: bool,
    max_bytes: usize,
}

impl Answered<'_> {
    async fn outcome(&self, response: reqwest::Response) -> Dispatched {
        let status = response.status();
        let headers = response.headers().clone();
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if status.is_success() {
            return match self.read(response, self.max_bytes, false).await {
                Ok(Some(body)) => Dispatched::Result(result(&body, content_type.as_deref())),
                Ok(None) => failed(
                    ActionFailureKind::Error,
                    format!(
                        "the answer of {} is larger than {} bytes",
                        self.action, self.max_bytes
                    ),
                    Some(json!({ "status": status.as_u16() })),
                ),
                Err(dispatched) => dispatched,
            };
        }
        let code = status.as_u16();
        // What the failure may quote of the body: nothing for an internal API.
        let body = if self.internal {
            Vec::new()
        } else {
            match self.read(response, QUOTED_BODY, true).await {
                Ok(Some(body)) => body,
                Ok(None) | Err(_) => Vec::new(),
            }
        };
        if status == StatusCode::UNAUTHORIZED {
            let challenge = headers
                .get(header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .filter(|_| !self.internal)
                .map(|challenge| format!(": {}", cut(challenge)))
                .unwrap_or_default();
            return Dispatched::Park(ParkReason {
                kind: ParkKind::CredentialsRefused,
                message: format!(
                    "{} refused the credentials (HTTP 401) of {}{challenge}",
                    self.api, self.action
                ),
            });
        }
        let (detail, mut data) = self.detail(code, &body, content_type.as_deref());
        if matches!(code, 408 | 429) || status.is_server_error() {
            if let Some(seconds) = retry_after(&headers)
                && let Value::Object(fields) = &mut data
            {
                fields.insert(RETRY_AFTER_DATA.into(), json!(seconds));
            }
            return Dispatched::Transient(failure(
                ActionFailureKind::Unavailable,
                format!(
                    "{} answered HTTP {code} to {}{detail}",
                    self.api, self.action
                ),
                Some(data),
            ));
        }
        failed(
            ActionFailureKind::Error,
            format!(
                "{} answered HTTP {code} to {}{detail}",
                self.api, self.action
            ),
            Some(data),
        )
    }

    /// The body, read to its end within `max` bytes: `None` when it is larger, unless `start`
    /// asks for its first `max` bytes only (what a failure quotes).
    async fn read(
        &self,
        mut response: reqwest::Response,
        max: usize,
        start: bool,
    ) -> Result<Option<Vec<u8>>, Dispatched> {
        if !start
            && response
                .content_length()
                .is_some_and(|length| length > max as u64)
        {
            return Ok(None);
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > max {
                        if start {
                            let room = max - body.len();
                            body.extend_from_slice(&chunk[..room]);
                            return Ok(Some(body));
                        }
                        return Ok(None);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(Some(body)),
                Err(error) => {
                    return Err(Dispatched::Transient(failure(
                        ActionFailureKind::Unavailable,
                        format!(
                            "the answer of {} to {} was cut: {}",
                            self.api,
                            self.action,
                            chain(&error)
                        ),
                        None,
                    )));
                }
            }
        }
    }

    /// What follows the status in a failure's message, and its data: the problem's `title` and
    /// `detail`, else the body's first 2 KB.
    fn detail(&self, code: u16, body: &[u8], content_type: Option<&str>) -> (String, Value) {
        let mut data = json!({ "status": code });
        if self.internal {
            return (
                "; the answer of an internal server is not quoted".into(),
                data,
            );
        }
        let essence = content_type
            .and_then(|media| media.split(';').next())
            .map(|essence| essence.trim().to_ascii_lowercase());
        if essence.as_deref() == Some("application/problem+json")
            && let Ok(problem) = serde_json::from_slice::<Value>(body)
        {
            let mut parts = Vec::new();
            for key in ["title", "detail"] {
                if let Some(text) = problem.get(key).and_then(Value::as_str) {
                    data[key] = json!(cut(text));
                    parts.push(cut(text));
                }
            }
            if !parts.is_empty() {
                return (format!(": {}", parts.join(": ")), data);
            }
        }
        let text = String::from_utf8_lossy(body);
        let text = text.trim();
        if text.is_empty() {
            return (String::new(), data);
        }
        data["body"] = json!(text);
        (format!(": {}", cut(text)), data)
    }
}

/// A successful answer's body as the code receives it.
fn result(body: &[u8], content_type: Option<&str>) -> Value {
    if body.is_empty() {
        return Value::Null;
    }
    let content_type = content_type.unwrap_or("application/octet-stream");
    if is_json(content_type)
        && let Ok(value) = serde_json::from_slice(body)
    {
        return value;
    }
    match std::str::from_utf8(body) {
        Ok(text) => json!({ "contentType": content_type, "text": text }),
        Err(_) => json!({ "contentType": content_type, "base64": STANDARD.encode(body) }),
    }
}

/// `Retry-After` in seconds: a number of seconds, or an HTTP date from now.
fn retry_after(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    let date = httpdate::parse_http_date(value).ok()?;
    Some(date.duration_since(SystemTime::now()).map_or(0, |wait| {
        wait.as_secs() + u64::from(wait.subsec_nanos() > 0)
    }))
}

fn method(method: HttpMethod) -> Method {
    match method {
        HttpMethod::Get => Method::GET,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Post => Method::POST,
        HttpMethod::Delete => Method::DELETE,
        HttpMethod::Options => Method::OPTIONS,
        HttpMethod::Head => Method::HEAD,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Trace => Method::TRACE,
    }
}

fn add(headers: &mut Vec<(HeaderName, HeaderValue)>, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.push((HeaderName::from_static(name), value));
    }
}

fn failure(kind: ActionFailureKind, message: String, data: Option<Value>) -> ActionFailure {
    ActionFailure {
        kind,
        message,
        data,
    }
}

fn failed(kind: ActionFailureKind, message: String, data: Option<Value>) -> Dispatched {
    Dispatched::Failed(failure(kind, message, data))
}

/// At most 2,000 characters of what a server said.
fn cut(text: &str) -> String {
    if text.chars().count() <= 2000 {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(2000).collect();
    cut.push('…');
    cut
}

/// The error and its sources, as one line.
fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(current) = source {
        let text = current.to_string();
        if !parts.iter().any(|part| part.contains(&text)) {
            parts.push(text);
        }
        source = current.source();
    }
    parts.join(": ")
}

/// `30s`, `5m`, `1500ms`.
fn human(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis >= 60_000 && millis.is_multiple_of(60_000) {
        format!("{}m", millis / 60_000)
    } else if millis.is_multiple_of(1000) {
        format!("{}s", millis / 1000)
    } else {
        format!("{millis}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_become_results() {
        assert_eq!(result(b"", Some("application/json")), Value::Null);
        assert_eq!(
            result(b"{\"a\":1}", Some("application/json; charset=utf-8")),
            json!({ "a": 1 })
        );
        assert_eq!(result(b"[1]", Some("application/vnd.api+json")), json!([1]));
        assert_eq!(
            result(b"hello", Some("text/plain")),
            json!({ "contentType": "text/plain", "text": "hello" })
        );
        assert_eq!(
            result(&[0xff, 0x00], Some("image/png")),
            json!({ "contentType": "image/png", "base64": "/wA=" })
        );
        assert_eq!(
            result(b"{not json", Some("application/json")),
            json!({ "contentType": "application/json", "text": "{not json" })
        );
    }

    #[test]
    fn retry_after_is_read_in_seconds_or_as_a_date() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(retry_after(&headers), Some(7));
        let later = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120));
        headers.insert(header::RETRY_AFTER, HeaderValue::from_str(&later).unwrap());
        let seconds = retry_after(&headers).unwrap();
        assert!((118..=121).contains(&seconds), "{seconds}");
        headers.insert(header::RETRY_AFTER, HeaderValue::from_static("soon"));
        assert_eq!(retry_after(&headers), None);
    }
}
