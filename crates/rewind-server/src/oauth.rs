//! The OAuth authorization server of people's connections (`archi/07-code-architecture.md`,
//! "MCP and OAuth"), on the api role when the server has a public URL and a sign-in: what the
//! endpoints of `http::oauth` and the consent share.
//!
//! - **Clients.** A `client_id` that is an `https` URL with a path is a client's metadata
//!   document (CIMD): fetched with the egress rules of MCP servers (public addresses only, and
//!   the internal hosts the operator lets through, `--allow-internal-mcp`, which are the only
//!   ones a document may also be fetched from over `http`), without redirect, at most 5 KiB
//!   within 5 s, and kept as its `Cache-Control` says, 24 hours at most. Other clients register
//!   and get `rc_…`.
//! - **Redirect URIs.** `https`, `http` on a loopback address, or a private-use scheme whose URI
//!   holds a dot; never `javascript:`, `data:` nor `file:`, never a fragment. A redirect URI
//!   matches exactly, but a loopback one, compared without its port.
//! - **The request** travels sealed, with the seal of `/auth`'s cookies, from
//!   `/oauth/authorize` to the consent, for 10 minutes ([`PendingRequest`]).
//! - **Codes** are PKCE-bound (S256 only), live 5 minutes, and are stored hashed.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use reqwest::Url;
use rewind_api::oauth::{
    AuthorizationServerMetadata, ClientMetadata, OFFLINE_ACCESS, ProtectedResourceMetadata,
};
use rewind_api::routes;
use rewind_core::{Digest, Timestamp};
use rewind_engine::ports::{AccessStore, ClientAuthMethod, ClientKind, OAuthClient, StoreError};
use rewind_mcp::{Egress, InternalAllowList, SystemResolver};
use serde::{Deserialize, Serialize};

use crate::app::SignIn;
use crate::connections::{after, world_now};
use crate::error::ApiError;

/// The path of Rewind's MCP server: the resource the tokens are for.
pub(crate) const MCP_PATH: &str = "/mcp";

/// How long an authorization request may wait for the person's consent.
pub(crate) const REQUEST_LIFETIME: Duration = Duration::from_secs(600);

/// How long a code waits to be exchanged.
pub(crate) const CODE_LIFETIME: Duration = Duration::from_secs(300);

/// The most registered clients kept: past it, registering answers 503.
pub(crate) const MAX_REGISTERED_CLIENTS: u64 = 10_000;

/// The largest metadata document fetched.
const DOCUMENT_LIMIT: usize = 5 * 1024;

/// How long fetching a metadata document may take.
const DOCUMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest a metadata document is kept.
const DOCUMENT_KEPT_AT_MOST: Duration = Duration::from_secs(24 * 3_600);

/// How long a metadata document is kept when its answer says nothing of caching: the life of an
/// authorization request, so that one connection fetches it once.
const DOCUMENT_KEPT_BY_DEFAULT: Duration = REQUEST_LIFETIME;

/// The longest text kept of a client's name, site or logo.
const TEXT_LIMIT: usize = 2_048;

/// How a refusal of the egress checks ends.
const REFUSAL: &str =
    "client metadata documents are not fetched there unless the operator allows it";

/// The authorization server of this deployment.
#[derive(Debug)]
pub(crate) struct OAuthServer {
    /// `{public-url}`.
    pub(crate) issuer: String,
    /// `{public-url}/mcp`.
    pub(crate) resource: String,
    /// The dashboard's origin: that of `WORKOS_REDIRECT_URI`.
    pub(crate) dashboard: String,
    documents: reqwest::Client,
    egress: Egress,
    internal: Arc<InternalAllowList>,
}

/// Why a client is unknown: the reason, for the person or the client's developer.
#[derive(Debug)]
pub(crate) enum ClientError {
    Unknown(String),
    Store(StoreError),
}

impl From<StoreError> for ClientError {
    fn from(error: StoreError) -> Self {
        ClientError::Store(error)
    }
}

impl OAuthServer {
    /// The authorization server at `public_url`, whose consent is on the dashboard of
    /// `redirect_uri` (`WORKOS_REDIRECT_URI`), fetching metadata documents from public
    /// addresses and the hosts of `internal`.
    pub(crate) fn new(
        public_url: &str,
        redirect_uri: &str,
        internal: InternalAllowList,
    ) -> Result<Self, String> {
        let issuer = public_url.trim_end_matches('/').to_owned();
        let dashboard = Url::parse(redirect_uri)
            .map_err(|error| format!("WORKOS_REDIRECT_URI {redirect_uri:?}: {error}"))?
            .origin()
            .ascii_serialization();
        let egress = Egress::new(Arc::new(SystemResolver), REFUSAL).allowing(internal.clone());
        let documents = egress
            .apply(reqwest::Client::builder())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(DOCUMENT_TIMEOUT)
            .user_agent(concat!("Rewind/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("the client of metadata documents: {error}"))?;
        Ok(Self {
            resource: format!("{issuer}{MCP_PATH}"),
            issuer,
            dashboard,
            documents,
            egress,
            internal: Arc::new(internal),
        })
    }

    /// `/.well-known/oauth-protected-resource/mcp`.
    pub(crate) fn resource_metadata(&self) -> ProtectedResourceMetadata {
        ProtectedResourceMetadata {
            resource: self.resource.clone(),
            authorization_servers: vec![self.issuer.clone()],
            bearer_methods_supported: vec!["header".into()],
            scopes_supported: vec![OFFLINE_ACCESS.into()],
            resource_name: "Rewind".into(),
        }
    }

    /// `/.well-known/oauth-authorization-server`.
    pub(crate) fn metadata(&self) -> AuthorizationServerMetadata {
        let endpoint = |path: &str| format!("{}{path}", self.issuer);
        let methods = vec![
            ClientAuthMethod::None.as_str().to_owned(),
            ClientAuthMethod::ClientSecretPost.as_str().to_owned(),
            ClientAuthMethod::ClientSecretBasic.as_str().to_owned(),
        ];
        AuthorizationServerMetadata {
            issuer: self.issuer.clone(),
            authorization_endpoint: endpoint(routes::OAUTH_AUTHORIZE),
            token_endpoint: endpoint(routes::OAUTH_TOKEN),
            registration_endpoint: endpoint(routes::OAUTH_REGISTER),
            revocation_endpoint: endpoint(routes::OAUTH_REVOKE),
            response_types_supported: vec!["code".into()],
            grant_types_supported: vec!["authorization_code".into(), "refresh_token".into()],
            code_challenge_methods_supported: vec!["S256".into()],
            token_endpoint_auth_methods_supported: methods.clone(),
            revocation_endpoint_auth_methods_supported: methods,
            scopes_supported: vec![OFFLINE_ACCESS.into()],
            client_id_metadata_document_supported: true,
        }
    }

    /// The client `id`: a registered one, or a metadata document, kept or fetched now.
    pub(crate) async fn client(
        &self,
        store: &impl AccessStore,
        id: &str,
    ) -> Result<OAuthClient, ClientError> {
        if !id.contains("://") {
            let found = if plain_id(id) {
                store.oauth_client(id).await?
            } else {
                None
            };
            return found.ok_or_else(|| {
                ClientError::Unknown(format!(
                    "no client {id} is registered here: register with POST /oauth/register, or \
                     use the https URL of the client's metadata document as client_id"
                ))
            });
        }
        let url = self.document_url(id).map_err(ClientError::Unknown)?;
        let now = world_now();
        if let Some(kept) = store.oauth_client(id).await?
            && kept.kind == ClientKind::MetadataDocument
            && kept.expires_at.is_some_and(|until| now < until)
        {
            return Ok(kept);
        }
        let (document, kept_for) = self.fetch(&url).await.map_err(|reason| {
            ClientError::Unknown(format!(
                "the metadata document of the client {id} cannot be used: {reason}"
            ))
        })?;
        let client = metadata_client(id, &url, document, now, after(now, kept_for))
            .map_err(ClientError::Unknown)?;
        store.put_oauth_client(&client).await?;
        Ok(client)
    }

    /// A `client_id` that is a metadata document's URL: `https`, or `http` on a host the
    /// operator lets through; with a path; without fragment, credentials, or anything the URL's
    /// parsing would change.
    fn document_url(&self, id: &str) -> Result<Url, String> {
        let url = Url::parse(id).map_err(|error| format!("{id:?} is not a URL: {error}"))?;
        let host = url.host_str().unwrap_or_default();
        let scheme_allowed = match url.scheme() {
            "https" => true,
            "http" => self.internal.allows_host(host),
            _ => false,
        };
        if !scheme_allowed {
            return Err(format!(
                "a client_id URL is the https URL of the client's metadata document: {id:?} is not"
            ));
        }
        if url.path() == "/" || url.path().is_empty() {
            return Err(format!(
                "a client_id URL has a path, the metadata document's: {id:?} has none"
            ));
        }
        if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
            return Err(format!(
                "a client_id URL holds no fragment nor credentials: {id:?} does"
            ));
        }
        if url.as_str() != id {
            return Err(format!(
                "a client_id URL is written as it is fetched: {id:?} reads as {:?} (dot \
                 segments, case, escapes)",
                url.as_str()
            ));
        }
        Ok(url)
    }

    /// Fetches a metadata document: the egress checks, no redirect, 5 KiB within 5 s. The
    /// document, and how long it may be kept.
    async fn fetch(&self, url: &Url) -> Result<(ClientMetadata, Duration), String> {
        self.egress.check_url(url)?;
        let response = self
            .documents
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| {
                Egress::refusal_of(&error).unwrap_or_else(|| {
                    if error.is_timeout() {
                        "it did not answer within 5 s".to_owned()
                    } else {
                        "it cannot be reached".to_owned()
                    }
                })
            })?;
        let status = response.status();
        if status.is_redirection() {
            return Err(format!(
                "it answered HTTP {}, a redirect, and a metadata document is fetched without \
                 following one",
                status.as_u16()
            ));
        }
        if !status.is_success() {
            return Err(format!("it answered HTTP {}", status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > DOCUMENT_LIMIT as u64)
        {
            return Err("it is larger than 5 KiB".into());
        }
        let kept_for = freshness(response.headers());
        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "its answer could not be read within 5 s".to_owned())?
        {
            body.extend_from_slice(&chunk);
            if body.len() > DOCUMENT_LIMIT {
                return Err("it is larger than 5 KiB".into());
            }
        }
        let document: ClientMetadata = serde_json::from_slice(&body)
            .map_err(|error| format!("it is not a client's metadata in JSON: {error}"))?;
        Ok((document, kept_for))
    }
}

/// How long an answer may be kept, as its `Cache-Control` says: `max-age`, 24 hours at most;
/// nothing for `no-store` or `no-cache`; [`DOCUMENT_KEPT_BY_DEFAULT`] when it says nothing.
fn freshness(headers: &reqwest::header::HeaderMap) -> Duration {
    let mut kept = DOCUMENT_KEPT_BY_DEFAULT;
    for value in headers.get_all(reqwest::header::CACHE_CONTROL) {
        let Ok(text) = value.to_str() else { continue };
        for directive in text.split(',').map(str::trim) {
            let lower = directive.to_ascii_lowercase();
            if lower == "no-store" || lower == "no-cache" {
                return Duration::ZERO;
            }
            if let Some(seconds) = lower.strip_prefix("max-age=")
                && let Ok(seconds) = seconds.trim_matches('"').parse::<u64>()
            {
                kept = Duration::from_secs(seconds);
            }
        }
    }
    kept.min(DOCUMENT_KEPT_AT_MOST)
}

/// A metadata document as a client: it names itself, lists valid redirect URIs, and
/// authenticates with `none`.
fn metadata_client(
    id: &str,
    url: &Url,
    document: ClientMetadata,
    now: Timestamp,
    expires_at: Timestamp,
) -> Result<OAuthClient, String> {
    if document.client_id.as_deref() != Some(id) {
        return Err(format!(
            "its client_id must be its own URL, {id:?}, and it is {:?}",
            document.client_id.unwrap_or_default()
        ));
    }
    if document.client_secret.is_some() {
        return Err("it holds a client_secret, which a public document never does".into());
    }
    match document.token_endpoint_auth_method.as_deref() {
        None | Some("none") => {}
        Some(other) => {
            return Err(format!(
                "its token_endpoint_auth_method is {other:?}: a metadata document's client \
                 authenticates with none here"
            ));
        }
    }
    let redirect_uris = redirect_uris(&document.redirect_uris)?;
    Ok(OAuthClient {
        id: id.to_owned(),
        kind: ClientKind::MetadataDocument,
        name: name(document.client_name.as_deref())
            .unwrap_or_else(|| url.host_str().unwrap_or(id).to_owned()),
        uri: site(document.client_uri.as_deref()),
        logo_uri: site(document.logo_uri.as_deref()),
        redirect_uris,
        auth_method: ClientAuthMethod::None,
        secret_hash: None,
        fetched_at: Some(now),
        expires_at: Some(expires_at),
        created_at: now,
        last_used_at: None,
    })
}

/// A client's name: trimmed, without control characters, 200 characters at most; `None` when
/// blank.
pub(crate) fn name(text: Option<&str>) -> Option<String> {
    let cleaned: String = text?
        .chars()
        .filter(|character| !character.is_control())
        .take(200)
        .collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_owned())
}

/// A client's site or logo: an `https` URL, else nothing.
pub(crate) fn site(text: Option<&str>) -> Option<String> {
    let text = text?.trim();
    (text.len() <= TEXT_LIMIT
        && Url::parse(text).is_ok_and(|url| url.scheme() == "https" && url.host().is_some()))
    .then(|| text.to_owned())
}

/// The redirect URIs of a client: at least one, at most 20, each allowed.
pub(crate) fn redirect_uris(uris: &[String]) -> Result<Vec<String>, String> {
    if uris.is_empty() {
        return Err("redirect_uris: a client lists at least one redirect URI".into());
    }
    if uris.len() > 20 {
        return Err("redirect_uris: a client lists at most 20 redirect URIs".into());
    }
    for uri in uris {
        redirect_uri_allowed(uri).map_err(|reason| format!("redirect_uris: {reason}"))?;
    }
    Ok(uris.to_vec())
}

/// Whether a redirect URI may be listed: `https`, `http` on a loopback address, or a
/// private-use scheme whose URI holds a dot (`com.example.app:/callback`,
/// `cursor://anysphere.cursor-mcp/oauth/callback`); never `javascript:`, `data:`, `file:`, nor
/// a fragment.
pub(crate) fn redirect_uri_allowed(uri: &str) -> Result<(), String> {
    if uri.len() > TEXT_LIMIT {
        return Err("a redirect URI holds 2 048 characters at most".into());
    }
    let url = Url::parse(uri).map_err(|error| format!("{uri:?} is not a URI: {error}"))?;
    if url.fragment().is_some() {
        return Err(format!(
            "{uri:?} holds a fragment, which no redirect URI does"
        ));
    }
    match url.scheme() {
        "https" if url.host().is_some() => Ok(()),
        "http" if loopback_url(&url) => Ok(()),
        "http" => Err(format!(
            "{uri:?} is http on a host that is not a loopback address: use https"
        )),
        "javascript" | "data" | "file" | "vbscript" | "about" | "blob" | "https" => {
            Err(format!("{uri:?} is not a redirect URI a client may list"))
        }
        scheme => {
            let dotted =
                scheme.contains('.') || url.host_str().is_some_and(|host| host.contains('.'));
            if dotted {
                Ok(())
            } else {
                Err(format!(
                    "{uri:?} is a private-use scheme without a dot: use a reverse domain name \
                     (com.example.app:/callback) or a host holding one"
                ))
            }
        }
    }
}

/// Whether a URL's host is a loopback address (or `localhost`).
fn loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Whether a redirect URI returns to a program on the person's own machine.
pub(crate) fn loopback(uri: &str) -> bool {
    Url::parse(uri).is_ok_and(|url| url.scheme() == "http" && loopback_url(&url))
}

/// Whether `given` is the listed redirect URI `listed`: exactly, or for a loopback one,
/// without its port (RFC 8252, section 7.3).
pub(crate) fn redirect_matches(listed: &str, given: &str) -> bool {
    if listed == given {
        return true;
    }
    let (Ok(mut listed), Ok(mut given)) = (Url::parse(listed), Url::parse(given)) else {
        return false;
    };
    if !(listed.scheme() == "http" && loopback_url(&listed) && loopback_url(&given)) {
        return false;
    }
    let portless = listed.set_port(None).is_ok() && given.set_port(None).is_ok();
    portless && listed == given
}

/// The host the browser returns to, as the consent shows it: a URL's host, or a private-use
/// scheme's.
pub(crate) fn redirect_host(uri: &str) -> String {
    match Url::parse(uri) {
        Ok(url) => url
            .host_str()
            .filter(|host| !host.is_empty())
            .map_or_else(|| format!("{}:", url.scheme()), ToOwned::to_owned),
        Err(_) => uri.to_owned(),
    }
}

/// `uri` with `params` added to its query.
pub(crate) fn with_query(uri: &str, params: &[(&str, &str)]) -> String {
    let encoded = serde_html_form::to_string(params).unwrap_or_default();
    if encoded.is_empty() {
        return uri.to_owned();
    }
    let separator = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{separator}{encoded}")
}

/// Whether a PKCE challenge or verifier is well formed: 43 to 128 characters of `A-Z`, `a-z`,
/// `0-9`, `-`, `.`, `_` and `~` (RFC 7636).
pub(crate) fn pkce_text(text: &str) -> bool {
    (43..=128).contains(&text.len())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
}

/// Whether `verifier` answers the S256 `challenge`.
pub(crate) fn pkce_matches(challenge: &str, verifier: &str) -> bool {
    use sha2::Digest as _;
    if !pkce_text(verifier) {
        return false;
    }
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    let computed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    same(computed.as_bytes(), challenge.as_bytes())
}

/// Compares two secrets' bytes in a time that does not depend on where they differ.
pub(crate) fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |differ, (x, y)| differ | (x ^ y)) == 0
}

/// A secret: `prefix` and 256 bits from the operating system's CSPRNG, in base 62.
pub(crate) fn secret(prefix: &str) -> Result<String, ApiError> {
    Ok(format!("{prefix}{}", random_text(43)?))
}

/// `length` characters of base 62 from the operating system's CSPRNG.
pub(crate) fn random_text(length: usize) -> Result<String, ApiError> {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut bytes = vec![0u8; length];
    getrandom::fill(&mut bytes).map_err(ApiError::internal)?;
    // The bias of `% 62` on a byte is below 2 %.
    Ok(bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect())
}

/// Whether an id from a request may be an id Rewind gave (`rc_…`, `cn_…`): 1 to 128 ASCII
/// letters, digits, `-` and `_`. Any other is unknown, and never reaches a store.
pub(crate) fn plain_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The SHA-256 of a secret: how the store finds it.
pub(crate) fn hash(secret: &str) -> Digest {
    Digest::of(secret.as_bytes())
}

// -------------------------------------------------------------------------------------------
// The request, sealed from `/oauth/authorize` to the consent

/// An authorization request checked by `/oauth/authorize`, waiting for the person's consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingRequest {
    pub(crate) client_id: String,
    pub(crate) redirect_uri: String,
    pub(crate) code_challenge: String,
    pub(crate) state: String,
    pub(crate) resource: String,
    /// Milliseconds since the epoch, in the world's time.
    pub(crate) expires_at: u64,
}

impl PendingRequest {
    pub(crate) fn expires_at(&self) -> Timestamp {
        Timestamp::from_millis(self.expires_at)
    }

    /// The request sealed, as the consent page's URL carries it.
    pub(crate) fn seal(&self, sign_in: &SignIn) -> Result<String, ApiError> {
        sign_in.seal.seal(self).map_err(ApiError::internal)
    }

    /// The request a consent page sent back: one this server sealed, not lapsed.
    pub(crate) fn unseal(sign_in: &SignIn, sealed: &str) -> Result<Self, ApiError> {
        let request: Self = sign_in.seal.unseal(sealed.trim()).map_err(|_| {
            ApiError::invalid(
                "request: this authorization request cannot be read: start connecting again \
                 from the client",
            )
        })?;
        if world_now() >= request.expires_at() {
            return Err(ApiError::invalid(
                "request: this authorization request lapsed (10 minutes): start connecting \
                 again from the client",
            ));
        }
        Ok(request)
    }

    /// Where the browser returns with `params`, and the request's state.
    pub(crate) fn answer(&self, params: &[(&str, &str)]) -> String {
        let mut all: Vec<(&str, &str)> = params.to_vec();
        all.push(("state", self.state.as_str()));
        with_query(&self.redirect_uri, &all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uris_follow_the_rules() {
        for allowed in [
            "https://claude.ai/api/mcp/auth_callback",
            "http://localhost:33418/callback",
            "http://127.0.0.1/callback",
            "http://[::1]:8080/cb",
            "cursor://anysphere.cursor-mcp/oauth/callback",
            "com.example.app:/callback",
        ] {
            assert!(redirect_uri_allowed(allowed).is_ok(), "{allowed}");
        }
        for refused in [
            "http://example.com/callback",
            "https://claude.ai/cb#fragment",
            "javascript:alert(1)",
            "data:text/html,hi",
            "file:///etc/passwd",
            "myapp://callback",
            "not a uri",
        ] {
            assert!(redirect_uri_allowed(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_loopback_redirect_uri_matches_on_any_port() {
        assert!(redirect_matches(
            "https://claude.ai/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callback"
        ));
        assert!(!redirect_matches(
            "https://claude.ai/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callback/"
        ));
        assert!(redirect_matches(
            "http://localhost/callback",
            "http://localhost:51234/callback"
        ));
        assert!(redirect_matches(
            "http://127.0.0.1:1/callback",
            "http://127.0.0.1:51234/callback"
        ));
        assert!(!redirect_matches(
            "http://localhost/callback",
            "http://localhost:51234/other"
        ));
        assert!(!redirect_matches(
            "https://claude.ai/cb",
            "https://claude.ai:8443/cb"
        ));
    }

    #[test]
    fn ids_from_requests_are_plain() {
        assert!(plain_id("rc_0123abcXYZ"));
        assert!(plain_id("cn_a-b_c"));
        assert!(!plain_id(""));
        assert!(!plain_id("rc_\u{0}"));
        assert!(!plain_id("rc 1"));
        assert!(!plain_id(&"x".repeat(129)));
    }

    #[test]
    fn pkce_is_s256() {
        // RFC 7636, appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(pkce_matches(challenge, verifier));
        assert!(!pkce_matches(challenge, &verifier.replace('d', "e")));
        assert!(!pkce_matches(challenge, "short"));
        assert!(pkce_text(challenge));
        assert!(!pkce_text(
            "plain text with spaces that is long enough to pass"
        ));
    }

    #[test]
    fn caching_says_how_long_a_document_is_kept() {
        let headers = |value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::CACHE_CONTROL,
                reqwest::header::HeaderValue::from_str(value)
                    .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("no-store")),
            );
            headers
        };
        assert_eq!(
            freshness(&headers("public, max-age=3600")),
            Duration::from_secs(3_600)
        );
        assert_eq!(freshness(&headers("max-age=999999")), DOCUMENT_KEPT_AT_MOST);
        assert_eq!(freshness(&headers("no-store")), Duration::ZERO);
        assert_eq!(freshness(&headers("no-cache, max-age=60")), Duration::ZERO);
        assert_eq!(
            freshness(&reqwest::header::HeaderMap::new()),
            DOCUMENT_KEPT_BY_DEFAULT
        );
    }

    #[test]
    fn answers_keep_the_redirect_uri_s_query() {
        assert_eq!(
            with_query(
                "https://a.example/cb?x=1",
                &[("code", "c d"), ("state", "s")]
            ),
            "https://a.example/cb?x=1&code=c+d&state=s"
        );
        assert_eq!(
            with_query("http://127.0.0.1:9/cb", &[("error", "access_denied")]),
            "http://127.0.0.1:9/cb?error=access_denied"
        );
        assert_eq!(
            redirect_host("cursor://anysphere.cursor-mcp/cb"),
            "anysphere.cursor-mcp"
        );
        assert_eq!(
            redirect_host("com.example.app:/callback"),
            "com.example.app:"
        );
    }
}
