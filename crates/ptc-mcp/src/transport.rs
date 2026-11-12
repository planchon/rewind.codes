//! The HTTP client of a connection to a streamable HTTP server: where it may connect, and which
//! redirects it follows (`archi/04-tools.md`, "Toolsets and code generation").
//!
//! | [`McpEgress`] | Used by | Reaches |
//! | --- | --- | --- |
//! | open | The embedded server and the CLI, for one local developer, who may start stdio servers too | Any address |
//! | checked | A deployed server, whose tenants name the URLs | The addresses the `http` tool reaches ([`Egress`]: one resolution, private, loopback, link-local and metadata addresses refused, the connection to the checked address), and the internal hosts and networks of the operator's [`InternalAllowList`] |
//!
//! Each connection has a client of its own, checked when it opens and at each TCP connection
//! it makes. A connection that reached an address only the allow-list lets through is
//! internal: its failures never quote what the server answered over HTTP, as a tenant could
//! otherwise read an internal service through the error messages of a toolset that points at
//! it ([`crate::failure`]).
//!
//! Redirects are followed only within the server's origin, with 307 and 308, which keep the
//! message, up to [`MAX_REDIRECTS`]; the resolver checks the new connection as any other. A
//! redirect elsewhere is refused: the credentials are custom headers, which the client would
//! carry to the other origin, and a 301, 302 or 303 would turn the `POST` of a message into a
//! `GET` without it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ptc_core::ToolFailureKind;
use reqwest::StatusCode;
use url::Url;

use crate::failure::{Answers, Failure};
use crate::http::{Egress, InternalAllowList, NameResolver, Reach, Refused, SystemResolver};

/// Redirects followed at most within the server's origin, as the `http` tool.
pub(crate) const MAX_REDIRECTS: usize = 5;

/// How a refusal of the checks ends.
const REFUSAL: &str = "MCP servers are not reached there unless the operator allows it";

/// Where the MCP client connects over streamable HTTP: anywhere ([`McpEgress::open`]), or with
/// the egress checks of a deployment ([`McpEgress::checked`], the default). Stdio servers are
/// processes the dispatcher starts: whether a deployment accepts them is its own setting.
#[derive(Debug, Clone)]
pub struct McpEgress(Option<Egress>);

impl McpEgress {
    /// Any address: for the embedded server and the CLI, where one developer names the
    /// servers of their own machine and network.
    pub fn open() -> Self {
        Self(None)
    }

    /// The checks of a deployment, with the system's resolver: an MCP server is reached only at
    /// public addresses, or at the internal hosts and networks `internal` lets through.
    pub fn checked(internal: InternalAllowList) -> Self {
        Self::checked_with(Arc::new(SystemResolver), internal)
    }

    /// [`McpEgress::checked`], resolving names with `resolver`.
    pub fn checked_with(resolver: Arc<dyn NameResolver>, internal: InternalAllowList) -> Self {
        Self(Some(Egress::new(resolver, REFUSAL).allowing(internal)))
    }

    /// Whether any address is reached.
    pub fn is_open(&self) -> bool {
        self.0.is_none()
    }
}

impl Default for McpEgress {
    /// The checks, without internal hosts.
    fn default() -> Self {
        Self::checked(InternalAllowList::default())
    }
}

/// Whether a connection reached an internal server, an address only the operator's allow-list
/// lets through. Shared with the resolver of its client, which notes it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Internal(Arc<AtomicBool>);

impl Internal {
    /// Whether failures may quote what the server answered.
    pub(crate) fn answers(&self) -> Answers {
        if self.0.load(Ordering::SeqCst) {
            Answers::Withheld
        } else {
            Answers::Quoted
        }
    }
}

/// The client of one connection to the server `server` at `url`, which notes in `internal`
/// when it reaches an internal address. Refused when `url` is not an `http` or `https` URL, or
/// when its host is an address the egress refuses.
pub(crate) fn client(
    server: &str,
    url: &str,
    egress: &McpEgress,
    internal: &Internal,
) -> Result<reqwest::Client, Failure> {
    let refused = |reason: String| {
        Failure::permanent(
            ToolFailureKind::Unavailable,
            format!("{server} cannot be reached: {reason}"),
            None,
        )
    };
    let parsed =
        Url::parse(url).map_err(|error| refused(format!("{url:?} is not a URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(refused(format!(
            "{url:?} is not an http:// or https:// URL"
        )));
    }
    // Without an idle pool, as rmcp's own client: a response not read to its end would stall
    // the next request on the same connection.
    let builder = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(redirects());
    let builder = match &egress.0 {
        None => builder,
        Some(egress) => {
            if egress.judge_url(&parsed).map_err(refused)? == Reach::Internal {
                internal.0.store(true, Ordering::SeqCst);
            }
            egress.apply_noting(builder, Some(internal.0.clone()))
        }
    };
    builder.build().map_err(|error| {
        Failure::transient(
            ToolFailureKind::Unavailable,
            format!("the HTTP client of {server} cannot be built: {error}"),
        )
    })
}

/// Follows 307 and 308 within the server's origin, up to [`MAX_REDIRECTS`]; refuses the rest.
/// The reason never quotes where the server redirects: an internal server's answer is not.
fn redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let same_origin = attempt
            .previous()
            .first()
            .is_some_and(|first| first.origin() == attempt.url().origin());
        let status = attempt.status();
        if !same_origin {
            attempt.error(Refused(
                "it redirects to another origin: MCP requests go to the server's URL only".into(),
            ))
        } else if !matches!(
            status,
            StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
        ) {
            attempt.error(Refused(format!(
                "it redirects with HTTP {}, which drops the request's message: only 307 and 308 \
                 are followed",
                status.as_u16()
            )))
        } else if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error(Refused(format!(
                "it redirects more than {MAX_REDIRECTS} times"
            )))
        } else {
            attempt.follow()
        }
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// A server on 127.0.0.1 that answers every request with `status` and `location`.
    async fn redirecting(status: u16, location: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let _ = socket.read(&mut buffer).await;
                    let response = format!(
                        "HTTP/1.1 {status} Redirect\r\nlocation: {location}\r\n\
                         content-length: 0\r\n\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        port
    }

    /// Why the client refused to follow the redirect of a `POST` to the server on `port`.
    async fn refusal(port: u16) -> String {
        let url = format!("http://127.0.0.1:{port}/mcp");
        let client = client("fx", &url, &McpEgress::open(), &Internal::default()).unwrap();
        let error = client.post(url).body("{}").send().await.unwrap_err();
        let mut source: Option<&(dyn Error + 'static)> = Some(&error);
        while let Some(current) = source {
            if let Some(Refused(reason)) = current.downcast_ref::<Refused>() {
                return reason.clone();
            }
            source = current.source();
        }
        panic!("not refused: {error:?}");
    }

    #[tokio::test]
    async fn redirects_are_followed_only_with_307_and_308_within_the_origin() {
        let elsewhere = redirecting(307, "http://127.0.0.2:9/mcp").await;
        assert_eq!(
            refusal(elsewhere).await,
            "it redirects to another origin: MCP requests go to the server's URL only"
        );
        let moved = redirecting(301, "/mcp/").await;
        assert_eq!(
            refusal(moved).await,
            "it redirects with HTTP 301, which drops the request's message: only 307 and 308 are \
             followed"
        );
        let endless = redirecting(308, "/mcp").await;
        assert_eq!(refusal(endless).await, "it redirects more than 5 times");
    }

    #[test]
    fn only_http_urls_are_reached() {
        let refused = |url: &str| {
            client("fx", url, &McpEgress::default(), &Internal::default())
                .map(drop)
                .unwrap_err()
        };
        for url in ["ftp://mcp.example.com/", "not a url", "file:///etc/passwd"] {
            let Failure::Permanent(failure) = refused(url) else {
                panic!("{url} fails for good");
            };
            assert!(
                failure.message.starts_with("fx cannot be reached: "),
                "{failure:?}"
            );
        }
        let internal = Internal::default();
        let checked = McpEgress::checked(InternalAllowList::parse(&["10.0.0.0/8"]).unwrap());
        assert!(client("fx", "http://10.1.2.3/mcp", &checked, &internal).is_ok());
        assert_eq!(internal.answers(), Answers::Withheld);
        let public = Internal::default();
        assert!(client("fx", "https://mcp.example.com/mcp", &checked, &public).is_ok());
        assert_eq!(public.answers(), Answers::Quoted);
        assert!(McpEgress::open().is_open() && !McpEgress::default().is_open());
    }
}
