//! The built-in `http` tool (`archi/04-tools.md`, "The http tool and egress"): the only way a
//! script reaches the network, through `fetch`. Requests leave from the dispatcher, never from
//! the isolate, and each goes through these steps, in order:
//!
//! 1. The host must be in the toolset's allow-list.
//! 2. The name is resolved once.
//! 3. Private, loopback, link-local and cloud-metadata addresses are refused.
//! 4. The connection goes to the address that was checked.
//! 5. Each redirect is checked again from step 1, up to 5 redirects.
//! 6. The response is bounded to 5 MB and 30 s.
//!
//! A refused request fails with an `error`, for good. What `fetch` sees as a network error is
//! a failure too: before anything was sent (no connection), it is transient, and retried under
//! the `retry` policy; once the request may have reached the server (a timeout, a connection
//! cut while reading), it is transient only for an idempotent method (`GET`, `HEAD`,
//! `OPTIONS`, `PUT`, `DELETE`), and fails otherwise, as the request may have taken effect. A
//! response with any status, 404 and 500 included, is a result, as `fetch` resolves with it.
//!
//! In production, egress also goes through an egress proxy: a rule of the network the workers
//! run in, not a setting of this client.

mod egress;
mod wire;

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{self, HeaderName};
use reqwest::{Method, StatusCode};
use rewind_core::{Builtin, Tool, ToolFailureKind, ToolRoute};
use rewind_engine::ports::{CallAuth, Dispatched, ToolCall, ToolDispatcher};
use url::Url;

pub use egress::{Egress, InternalAllowList, NameResolver, Resolving, SystemResolver};
pub use wire::{http_input_schema, http_output_schema};

use crate::McpError;
use crate::failure::{Failure, human};
use egress::{AllowList, CheckedResolver, HTTP_TOOL, Policy, Rules};
pub(crate) use egress::{Reach, Refused};
use wire::Request;

/// The bounds of the `http` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpConfig {
    /// Redirects followed at most.
    pub max_redirects: usize,
    /// Bytes of a response body at most.
    pub max_response_bytes: usize,
    /// How long a request may take, redirects included. The tool's execution timeout bounds it
    /// too.
    pub timeout: Duration,
    /// Sent when the script sets no `User-Agent`: some APIs refuse requests without one.
    pub user_agent: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            max_redirects: 5,
            max_response_bytes: 5 * 1024 * 1024,
            timeout: Duration::from_secs(30),
            user_agent: format!("Rewind/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// The `http` tool as a toolset pins it: the built-in with its allow-list and its schemas.
pub fn http_tool(allowed_hosts: Vec<String>) -> Tool {
    let mut tool = Tool::new(
        "http",
        ToolRoute::Builtin(Builtin::Http { allowed_hosts }),
        http_input_schema(),
    );
    tool.description = Some(
        "HTTP requests, sent by Rewind to the hosts of the toolset's allow-list; scripts call it \
         through fetch."
            .to_owned(),
    );
    tool.output_schema = Some(http_output_schema());
    tool
}

/// Answers the built-in tools: the `builtins` route of the engine's `RoutingDispatcher`. It
/// sends the requests of `http`; `webhook` is answered by the engine, which holds the webhook
/// tokens, and fails here. Cheap to clone: clones share their connections.
#[derive(Debug, Clone)]
pub struct HttpTool {
    client: reqwest::Client,
    config: Arc<HttpConfig>,
    policy: Policy,
}

impl HttpTool {
    /// The tool, resolving names with the system's resolver.
    pub fn new(config: HttpConfig) -> Result<Self, McpError> {
        Self::with_resolver(config, Arc::new(SystemResolver))
    }

    /// The tool, resolving names with `resolver`. Its answers are checked like the system's.
    pub fn with_resolver(
        config: HttpConfig,
        resolver: Arc<dyn NameResolver>,
    ) -> Result<Self, McpError> {
        Self::build(config, resolver, egress::forbidden)
    }

    fn build(
        config: HttpConfig,
        resolver: Arc<dyn NameResolver>,
        policy: Policy,
    ) -> Result<Self, McpError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Arc::new(CheckedResolver {
                resolver,
                rules: Rules::strict(policy, HTTP_TOOL),
                reached_internal: None,
            }))
            .user_agent(config.user_agent.as_str())
            .connect_timeout(config.timeout)
            .build()
            .map_err(|error| McpError::Client {
                message: format!("the client of the http tool cannot be built: {error}"),
            })?;
        Ok(Self {
            client,
            config: Arc::new(config),
            policy,
        })
    }

    async fn fetch(&self, allowed: &AllowList, mut request: Request) -> Dispatched {
        let mut url = match Url::parse(&request.url) {
            Ok(url) => url,
            Err(error) => {
                return refused(format!("{:?} is not a valid URL: {error}", request.url));
            }
        };
        let mut redirected = false;
        let mut redirects = 0;
        loop {
            if let Err(Refused(reason)) = egress::check(&url, allowed, self.policy) {
                return refused(reason);
            }
            let mut builder = self.client.request(request.method.clone(), url.clone());
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = &request.body {
                builder = builder.body(body.clone());
            }
            let mut response = match builder.send().await {
                Ok(response) => response,
                Err(error) => return failed(&request.method, &url, &error),
            };

            if let Some(location) = location(&response, &url) {
                let next = match location {
                    Ok(next) => next,
                    Err(reason) => return refused(reason),
                };
                if redirects == self.config.max_redirects {
                    return refused(format!(
                        "{} {url} redirects more than {} times",
                        request.method, self.config.max_redirects
                    ));
                }
                follow(&mut request, response.status(), &url, &next);
                url = next;
                redirected = true;
                redirects += 1;
                continue;
            }

            let max = self.config.max_response_bytes;
            let too_large = || {
                refused(format!(
                    "the response of {} {url} is larger than {} bytes",
                    request.method, max
                ))
            };
            if response
                .content_length()
                .is_some_and(|length| length > max as u64)
            {
                return too_large();
            }
            let mut body = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        if body.len() + chunk.len() > max {
                            return too_large();
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(error) => return failed(&request.method, &url, &error),
                }
            }
            tracing::debug!(
                host = url.host_str().unwrap_or_default(),
                status = response.status().as_u16(),
                redirected,
                "http tool request"
            );
            return Dispatched::Result(wire::response(
                response.status(),
                &url,
                redirected,
                response.headers(),
                &body,
            ));
        }
    }
}

impl ToolDispatcher for HttpTool {
    async fn dispatch(&self, call: ToolCall, _auth: CallAuth) -> Dispatched {
        match &call.tool.route {
            ToolRoute::Builtin(Builtin::Http { allowed_hosts }) => {
                let request = match wire::request(&call.args) {
                    Ok(request) => request,
                    Err(reason) => return refused(reason),
                };
                let allowed = AllowList::new(allowed_hosts);
                let limit = call
                    .tool
                    .policy
                    .execution_timeout
                    .map_or(self.config.timeout, |timeout| {
                        timeout.min(self.config.timeout)
                    });
                let method = request.method.clone();
                let url = request.url.clone();
                match tokio::time::timeout(limit, self.fetch(&allowed, request)).await {
                    Ok(dispatched) => dispatched,
                    Err(_) => {
                        let message =
                            format!("{method} {url} did not answer within {}", human(limit));
                        if idempotent(&method) {
                            Failure::transient(ToolFailureKind::Timeout, message)
                        } else {
                            Failure::permanent(ToolFailureKind::Timeout, message, None)
                        }
                        .into_dispatched()
                    }
                }
            }
            ToolRoute::Builtin(Builtin::Webhook) => Failure::permanent(
                ToolFailureKind::Unavailable,
                "webhook is answered by the engine, which holds the webhook tokens",
                None,
            )
            .into_dispatched(),
            _ => Failure::permanent(
                ToolFailureKind::Unavailable,
                format!("{} is not a built-in tool", call.tool.name),
                None,
            )
            .into_dispatched(),
        }
    }
}

/// A request the egress rules refuse: it fails for good.
fn refused(reason: String) -> Dispatched {
    Failure::permanent(ToolFailureKind::Error, reason, None).into_dispatched()
}

/// A request that failed without a response.
fn failed(method: &Method, url: &Url, error: &reqwest::Error) -> Dispatched {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if let Some(Refused(reason)) = current.downcast_ref::<Refused>() {
            return refused(reason.clone());
        }
        source = current.source();
    }
    let detail = crate::failure::chain(error);
    if error.is_connect() {
        // Nothing was sent.
        return Failure::transient(
            ToolFailureKind::Unavailable,
            format!("{method} {url} failed: no connection: {detail}"),
        )
        .into_dispatched();
    }
    let kind = if error.is_timeout() {
        ToolFailureKind::Timeout
    } else {
        ToolFailureKind::Unavailable
    };
    if idempotent(method) {
        Failure::transient(kind, format!("{method} {url} failed: {detail}")).into_dispatched()
    } else {
        Failure::permanent(
            kind,
            format!("{method} {url} failed, and may have reached the server: {detail}"),
            None,
        )
        .into_dispatched()
    }
}

/// Methods that can be sent twice with the effect of once (RFC 9110).
fn idempotent(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::OPTIONS | Method::PUT | Method::DELETE
    )
}

/// Where a redirect leads, if the response is one with a `Location`.
fn location(response: &reqwest::Response, url: &Url) -> Option<Result<Url, String>> {
    if !matches!(
        response.status(),
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    ) {
        return None;
    }
    let location = response.headers().get(header::LOCATION)?;
    let next = location
        .to_str()
        .map_err(|_| format!("{url} redirects to an invalid location"))
        .and_then(|location| {
            url.join(location)
                .map_err(|error| format!("{url} redirects to {location:?}, not a URL: {error}"))
        });
    Some(next)
}

/// The request to send after a redirect, as `fetch` makes it: `303`, and `301` or `302` after
/// a `POST`, continue with a `GET` without a body; across origins, credentials are dropped.
fn follow(request: &mut Request, status: StatusCode, from: &Url, to: &Url) {
    let to_get = status == StatusCode::SEE_OTHER && request.method != Method::HEAD
        || matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND)
            && request.method == Method::POST;
    if to_get {
        request.method = Method::GET;
        request.body = None;
        request.headers.retain(|(name, _)| {
            !matches!(
                name.as_str(),
                "content-type" | "content-encoding" | "content-language" | "content-location"
            )
        });
    }
    if from.origin() != to.origin() {
        let credentials: [HeaderName; 2] = [header::AUTHORIZATION, header::COOKIE];
        request
            .headers
            .retain(|(name, _)| !credentials.contains(name));
    }
    request.url = to.to_string();
}

#[cfg(test)]
mod tests;
