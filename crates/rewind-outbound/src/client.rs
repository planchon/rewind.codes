//! The HTTP client of a tenant's servers: where it may connect, and which redirects it follows
//! (`archi/04-tools.md`, "Where MCP servers and APIs may be").
//!
//! | [`ToolEgress`] | Used by | Reaches |
//! | --- | --- | --- |
//! | open | The embedded server and the CLI, for one local developer, who may start stdio servers too | Any address |
//! | checked | A deployed server, whose tenants name the URLs | The addresses the egress checks let through ([`Egress`]), and the internal hosts and networks of the operator's [`InternalAllowList`] (`--allow-internal-tools`) |
//!
//! Each client is checked when it is built and at each TCP connection it makes. A client that
//! reached an address only the allow-list lets through notes it ([`Reached`]): its caller then
//! never quotes what the server answered, as a tenant could otherwise read an internal service
//! through the error messages of a toolset that points at it.
//!
//! | [`Redirects`] | For | Follows |
//! | --- | --- | --- |
//! | `WithinOrigin` | MCP servers with a secret or none, which carry no proof bound to the URL | 307 and 308, which keep the request, within the server's origin, up to [`MAX_REDIRECTS`]; the resolver checks the new connection. Another redirect is refused: a secret is a header, which the client would carry elsewhere, and a 301, 302 or 303 turns the `POST` of a message into a `GET` without it |
//! | `Signed` | MCP servers whose requests are signed | None: a signature binds the request to its URL (`htu`), which a redirect would change; it is refused, naming why |
//! | `Manual` | The API dispatcher, which follows them itself to sign each request again | None: the response is answered as is |
//! | `Checked` | Documents read without a secret (OpenAPI documents) | Any, up to [`MAX_REDIRECTS`], each new host checked again |
//!
//! A refusal's reason never quotes where the server redirects: an internal server's answer is
//! not quoted.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use reqwest::StatusCode;
use url::Url;

use crate::egress::{Egress, InternalAllowList, NameResolver, Reach, Refused, SystemResolver};

/// Redirects followed at most, as the `http` tool.
pub const MAX_REDIRECTS: usize = 5;

/// How a refusal of the checks ends.
const REFUSAL: &str = "MCP servers and APIs are not reached there unless the operator allows it";

/// Where a tenant's MCP servers and APIs may be: anywhere ([`ToolEgress::open`]), or with the
/// egress checks of a deployment ([`ToolEgress::checked`], the default). Stdio servers are
/// processes the dispatcher starts: whether a deployment accepts them is its own setting.
#[derive(Debug, Clone)]
pub struct ToolEgress(Option<Egress>);

impl ToolEgress {
    /// Any address: for the embedded server and the CLI, where one developer names the
    /// servers of their own machine and network.
    pub fn open() -> Self {
        Self(None)
    }

    /// The checks of a deployment, with the system's resolver: a server is reached only at
    /// public addresses, or at the internal hosts and networks `internal` lets through.
    pub fn checked(internal: InternalAllowList) -> Self {
        Self::checked_with(Arc::new(SystemResolver), internal)
    }

    /// [`ToolEgress::checked`], resolving names with `resolver`.
    pub fn checked_with(resolver: Arc<dyn NameResolver>, internal: InternalAllowList) -> Self {
        Self(Some(Egress::new(resolver, REFUSAL).allowing(internal)))
    }

    /// The checks of `egress`, whatever its policy: for tests.
    pub fn with(egress: Egress) -> Self {
        Self(Some(egress))
    }

    /// Whether any address is reached.
    pub fn is_open(&self) -> bool {
        self.0.is_none()
    }

    /// A client for the server at `url`, from `builder` (timeouts, user agent), which notes in
    /// the answered [`Reached`] when it reaches an internal address. Refused, with the reason,
    /// when `url` is not an `http` or `https` URL or when its host is an address the checks
    /// refuse.
    pub fn client(
        &self,
        url: &Url,
        redirects: Redirects,
        builder: reqwest::ClientBuilder,
    ) -> Result<(reqwest::Client, Reached), String> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("{url} is not an http:// or https:// URL"));
        }
        let reached = Reached::default();
        let builder = builder.redirect(redirects.policy(self.0.clone(), reached.clone()));
        let builder = match &self.0 {
            None => builder,
            Some(egress) => {
                if egress.judge_url(url)? == Reach::Internal {
                    reached.note();
                }
                egress.apply_noting(builder, Some(reached.0.clone()))
            }
        };
        let client = builder
            .build()
            .map_err(|error| format!("its HTTP client cannot be built: {error}"))?;
        Ok((client, reached))
    }
}

impl Default for ToolEgress {
    /// The checks, without internal hosts.
    fn default() -> Self {
        Self::checked(InternalAllowList::default())
    }
}

/// Whether a client reached an internal server, an address only the operator's allow-list lets
/// through. Shared with the resolver of its client, which notes it.
#[derive(Debug, Clone, Default)]
pub struct Reached(Arc<AtomicBool>);

impl Reached {
    /// Whether an answer of the server may be quoted to the tenant: not once the client reached
    /// an internal address.
    pub fn internal(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub(crate) fn note(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Which redirects a client follows (the module's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Redirects {
    WithinOrigin,
    Signed,
    Manual,
    Checked,
}

impl Redirects {
    fn policy(self, egress: Option<Egress>, reached: Reached) -> reqwest::redirect::Policy {
        match self {
            Redirects::Manual => reqwest::redirect::Policy::none(),
            Redirects::Signed => reqwest::redirect::Policy::custom(|attempt| {
                attempt.error(Refused(
                    "it redirects, and a signed request is bound to its URL: register the URL it \
                     answers at"
                        .into(),
                ))
            }),
            Redirects::WithinOrigin => reqwest::redirect::Policy::custom(|attempt| {
                let same_origin = attempt
                    .previous()
                    .first()
                    .is_some_and(|first| first.origin() == attempt.url().origin());
                let status = attempt.status();
                if !same_origin {
                    attempt.error(Refused(
                        "it redirects to another origin: requests go to the server's own URL only"
                            .into(),
                    ))
                } else if !matches!(
                    status,
                    StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
                ) {
                    attempt.error(Refused(format!(
                        "it redirects with HTTP {}, which drops the request's body: only 307 and \
                         308 are followed",
                        status.as_u16()
                    )))
                } else if attempt.previous().len() > MAX_REDIRECTS {
                    attempt.error(Refused(format!(
                        "it redirects more than {MAX_REDIRECTS} times"
                    )))
                } else {
                    attempt.follow()
                }
            }),
            Redirects::Checked => reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > MAX_REDIRECTS {
                    return attempt.error(Refused(format!(
                        "it redirects more than {MAX_REDIRECTS} times"
                    )));
                }
                if !matches!(attempt.url().scheme(), "http" | "https") {
                    return attempt.error(Refused(
                        "it redirects to a URL that is not http:// or https://".into(),
                    ));
                }
                // A name is checked by the resolver; an address given in the location, here.
                if let Some(egress) = &egress {
                    match egress.judge_url(attempt.url()) {
                        Ok(Reach::Internal) => reached.note(),
                        Ok(Reach::Public) => {}
                        Err(reason) => return attempt.error(Refused(reason)),
                    }
                }
                attempt.follow()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::net::IpAddr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::egress::{Resolving, forbidden};

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
    async fn refusal(egress: &ToolEgress, redirects: Redirects, port: u16) -> String {
        let url = Url::parse(&format!("http://127.0.0.1:{port}/mcp")).unwrap();
        let (client, _) = egress
            .client(&url, redirects, reqwest::Client::builder())
            .unwrap();
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
        let open = ToolEgress::open();
        let elsewhere = redirecting(307, "http://127.0.0.2:9/mcp").await;
        assert_eq!(
            refusal(&open, Redirects::WithinOrigin, elsewhere).await,
            "it redirects to another origin: requests go to the server's own URL only"
        );
        let moved = redirecting(301, "/mcp/").await;
        assert_eq!(
            refusal(&open, Redirects::WithinOrigin, moved).await,
            "it redirects with HTTP 301, which drops the request's body: only 307 and 308 are \
             followed"
        );
        let endless = redirecting(308, "/mcp").await;
        assert_eq!(
            refusal(&open, Redirects::WithinOrigin, endless).await,
            "it redirects more than 5 times"
        );
        assert_eq!(
            refusal(&open, Redirects::Signed, endless).await,
            "it redirects, and a signed request is bound to its URL: register the URL it answers \
             at"
        );
        assert_eq!(
            refusal(&open, Redirects::Checked, endless).await,
            "it redirects more than 5 times"
        );
    }

    /// Only 127.0.0.1 is public.
    fn test_policy(address: IpAddr) -> Option<&'static str> {
        (address != IpAddr::from([127, 0, 0, 1])).then(|| forbidden(address).unwrap_or("test"))
    }

    #[derive(Debug)]
    struct Local;

    impl NameResolver for Local {
        fn resolve(&self, _host: &str) -> Resolving {
            Box::pin(async { Ok(vec![IpAddr::from([127, 0, 0, 1])]) })
        }
    }

    #[tokio::test]
    async fn a_checked_redirect_to_an_internal_address_is_refused() {
        let checked = ToolEgress::with(Egress::with_policy(Arc::new(Local), test_policy, "no"));
        let metadata = redirecting(302, "http://169.254.169.254/latest").await;
        assert_eq!(
            refusal(&checked, Redirects::Checked, metadata).await,
            "169.254.169.254 is a link-local (cloud metadata) address: no"
        );
        let manual = redirecting(302, "http://169.254.169.254/latest").await;
        let url = Url::parse(&format!("http://127.0.0.1:{manual}/x")).unwrap();
        let (client, reached) = checked
            .client(&url, Redirects::Manual, reqwest::Client::builder())
            .unwrap();
        let response = client.get(url).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        assert!(!reached.internal());
    }

    #[test]
    fn only_http_urls_are_reached_and_internal_ones_are_noted() {
        let url = |text: &str| Url::parse(text).unwrap();
        let refused = |egress: &ToolEgress, text: &str| {
            egress
                .client(
                    &url(text),
                    Redirects::WithinOrigin,
                    reqwest::Client::builder(),
                )
                .map(drop)
                .unwrap_err()
        };
        let checked = ToolEgress::checked(InternalAllowList::parse(&["10.0.0.0/8"]).unwrap());
        for text in ["ftp://mcp.example.com/", "file:///etc/passwd"] {
            assert!(
                refused(&checked, text).contains("is not an http:// or https:// URL"),
                "{text}"
            );
        }
        assert_eq!(
            refused(&checked, "http://169.254.169.254/latest"),
            "169.254.169.254 is a link-local (cloud metadata) address: MCP servers and APIs are \
             not reached there unless the operator allows it"
        );
        let builder = reqwest::Client::builder;
        let (_, internal) = checked
            .client(
                &url("http://10.1.2.3/mcp"),
                Redirects::WithinOrigin,
                builder(),
            )
            .unwrap();
        assert!(internal.internal());
        let (_, public) = checked
            .client(
                &url("https://mcp.example.com/mcp"),
                Redirects::WithinOrigin,
                builder(),
            )
            .unwrap();
        assert!(!public.internal());
        assert!(ToolEgress::open().is_open() && !ToolEgress::default().is_open());
    }
}
