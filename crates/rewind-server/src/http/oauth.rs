//! The OAuth authorization server of people's connections (`archi/07-code-architecture.md`,
//! "MCP and OAuth"): discovery (RFC 9728, RFC 8414), the authorization request, registration
//! (RFC 7591), the token endpoint (RFC 6749 with PKCE) and revocation (RFC 7009). 404 on a
//! server without a public URL and a sign-in.
//!
//! The token, registration and revocation endpoints answer OAuth's JSON errors
//! ([`OAuthError`]), never cached; `/oauth/authorize` answers a client it cannot trust (unknown,
//! or a redirect URI it does not list) with a problem, and every other fault on the client's
//! redirect URI.

use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use rewind_api::connections::CONNECTION_PREFIX;
use rewind_api::headers::CONNECTION_TOKEN_PREFIX;
use rewind_api::oauth::{
    AuthorizationServerMetadata, AuthorizeQuery, CLIENT_SECRET_PREFIX, ClientMetadata, OAuthError,
    OFFLINE_ACCESS, ProtectedResourceMetadata, REFRESH_TOKEN_PREFIX, REGISTERED_CLIENT_PREFIX,
    RegisteredClient, RevokeRequest, TokenRequest, TokenResponse,
};
use rewind_engine::ports::{
    AccessStore, ClientAuthMethod, ClientKind, CodeRedemption, Connection, ConnectionTokenKind,
    IdentityError, IdentityProvider, MembershipStatus, NewConnection, NewToken, OAuthClient,
    OpenedOrganization, Rotation, StoreError,
};
use serde::de::DeserializeOwned;

use crate::app::App;
use crate::connections::{ACCESS_TOKEN_LIFETIME, REFRESH_TOKEN_LIFETIME, after, world_now};
use crate::error::ApiError;
use crate::extract::Query;
use crate::oauth::{
    ClientError, MAX_REGISTERED_CLIENTS, PendingRequest, REQUEST_LIFETIME, hash, name,
    pkce_matches, pkce_text, plain_id, random_text, redirect_matches, redirect_uris, secret, site,
    with_query,
};

/// The largest registration body read.
const REGISTRATION_LIMIT: usize = 64 * 1024;

// -------------------------------------------------------------------------------------------
// Answers

/// An answer never cached: it may carry tokens.
fn no_store(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

/// An OAuth error: its status, its code and a description written for the client's developer.
#[derive(Debug)]
pub(super) struct Refused {
    status: StatusCode,
    error: &'static str,
    description: String,
    /// The client authenticated with HTTP Basic: a 401 names the scheme.
    basic: bool,
}

impl Refused {
    fn new(status: StatusCode, error: &'static str, description: impl Into<String>) -> Self {
        Self {
            status,
            error,
            description: description.into(),
            basic: false,
        }
    }

    fn request(description: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", description)
    }

    fn grant(description: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_grant", description)
    }

    fn client(description: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "invalid_client", description)
    }
}

impl From<StoreError> for Refused {
    fn from(error: StoreError) -> Self {
        tracing::error!(%error, "the OAuth server's store failed");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "the store cannot be reached: retry later",
        )
    }
}

impl From<ApiError> for Refused {
    fn from(error: ApiError) -> Self {
        Self::new(
            StatusCode::from_u16(error.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            "server_error",
            error.0.detail,
        )
    }
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let body = OAuthError {
            error: self.error.to_owned(),
            error_description: Some(self.description),
        };
        let mut response = no_store((self.status, Json(body)).into_response());
        if self.basic && self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"rewind\""),
            );
        }
        response
    }
}

/// The 404 of a server without OAuth, as a problem; `None` with OAuth.
fn without_oauth(app: &App) -> Option<Response> {
    app.oauth().err().map(IntoResponse::into_response)
}

// -------------------------------------------------------------------------------------------
// Discovery

/// `GET /.well-known/oauth-protected-resource/mcp`, and `/.well-known/oauth-protected-resource`.
pub(super) async fn protected_resource(
    State(app): State<Arc<App>>,
) -> Result<Json<ProtectedResourceMetadata>, ApiError> {
    let (oauth, _) = app.oauth()?;
    Ok(Json(oauth.resource_metadata()))
}

/// `GET /.well-known/oauth-authorization-server`.
pub(super) async fn authorization_server(
    State(app): State<Arc<App>>,
) -> Result<Json<AuthorizationServerMetadata>, ApiError> {
    let (oauth, _) = app.oauth()?;
    Ok(Json(oauth.metadata()))
}

// -------------------------------------------------------------------------------------------
// The authorization request

/// `GET /oauth/authorize`: checks the request, then sends the browser to the consent page with
/// the request sealed for 10 minutes. An unknown client or a redirect URI it does not list is
/// answered here (400) and never redirected; any other fault goes back to the client.
pub(super) async fn authorize(
    State(app): State<Arc<App>>,
    Query(query): Query<AuthorizeQuery>,
) -> Result<Response, ApiError> {
    let (oauth, sign_in) = app.oauth()?;
    let Some(client_id) = query.client_id.as_deref().filter(|id| !id.is_empty()) else {
        return Err(ApiError::invalid(
            "client_id is required: the URL of the client's metadata document, or the rc_… \
             registration gave",
        ));
    };
    let client = oauth
        .client(&app.store, client_id)
        .await
        .map_err(|error| match error {
            ClientError::Unknown(reason) => ApiError::invalid(reason),
            ClientError::Store(error) => ApiError::from(error),
        })?;
    let Some(redirect_uri) = query.redirect_uri.as_deref().filter(|uri| !uri.is_empty()) else {
        return Err(ApiError::invalid(
            "redirect_uri is required: one of the client's redirect URIs",
        ));
    };
    if !client
        .redirect_uris
        .iter()
        .any(|listed| redirect_matches(listed, redirect_uri))
    {
        return Err(ApiError::invalid(format!(
            "redirect_uri {redirect_uri:?} is not one of the client's redirect URIs: the browser \
             is not sent there"
        )));
    }
    // From here, faults go back to the client, with its state.
    let state = query.state.clone().unwrap_or_default();
    let back = |error: &str, description: &str| -> Response {
        let mut params = vec![("error", error), ("error_description", description)];
        if !state.is_empty() {
            params.push(("state", state.as_str()));
        }
        found(&with_query(redirect_uri, &params))
    };
    match query.response_type.as_deref() {
        Some("code") => {}
        None => return Ok(back("invalid_request", "response_type is required: code")),
        Some(_) => {
            return Ok(back(
                "unsupported_response_type",
                "only the response_type code is supported",
            ));
        }
    }
    if query.code_challenge_method.as_deref() != Some("S256") {
        return Ok(back(
            "invalid_request",
            "PKCE is required, with code_challenge_method S256",
        ));
    }
    let Some(challenge) = query
        .code_challenge
        .as_deref()
        .filter(|challenge| pkce_text(challenge))
    else {
        return Ok(back(
            "invalid_request",
            "code_challenge is required: BASE64URL(SHA-256(code_verifier)), 43 characters",
        ));
    };
    if state.is_empty() {
        return Ok(back("invalid_request", "state is required"));
    }
    if let Some(resource) = &query.resource
        && resource.trim_end_matches('/') != oauth.resource
    {
        return Ok(back(
            "invalid_target",
            &format!("this server issues tokens for {} only", oauth.resource),
        ));
    }
    let request = PendingRequest {
        client_id: client.id,
        redirect_uri: redirect_uri.to_owned(),
        code_challenge: challenge.to_owned(),
        state: state.clone(),
        resource: oauth.resource.clone(),
        expires_at: after(world_now(), REQUEST_LIFETIME).as_millis(),
    };
    let sealed = request.seal(sign_in)?;
    let query =
        serde_html_form::to_string([("request", sealed.as_str())]).map_err(ApiError::internal)?;
    Ok(found(&format!(
        "{}/#/connect/authorize?{query}",
        oauth.dashboard
    )))
}

/// A redirect the browser follows: 302, never cached.
fn found(location: &str) -> Response {
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    no_store(response)
}

// -------------------------------------------------------------------------------------------
// Registration

/// `POST /oauth/register` (RFC 7591): a client with its redirect URIs, `rc_…`, and a secret
/// for a confidential one, stored hashed and shown once. Past 10 000 registered clients, 503.
pub(super) async fn register(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = without_oauth(&app) {
        return response;
    }
    match registered(&app, &headers, &body).await {
        Ok(client) => no_store((StatusCode::CREATED, Json(client)).into_response()),
        Err(refused) => refused.into_response(),
    }
}

async fn registered(
    app: &App,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<RegisteredClient, Refused> {
    let metadata = |description: String| {
        Refused::new(
            StatusCode::BAD_REQUEST,
            "invalid_client_metadata",
            description,
        )
    };
    if !media_type(headers, "application/json") {
        return Err(metadata(
            "the client's metadata is JSON, with Content-Type: application/json".into(),
        ));
    }
    if body.len() > REGISTRATION_LIMIT {
        return Err(metadata(
            "the client's metadata holds 64 KiB at most".into(),
        ));
    }
    let request: ClientMetadata = serde_json::from_slice(body)
        .map_err(|error| metadata(format!("the client's metadata cannot be read: {error}")))?;
    let redirect = redirect_uris(&request.redirect_uris)
        .map_err(|reason| Refused::new(StatusCode::BAD_REQUEST, "invalid_redirect_uri", reason))?;
    let method = match request.token_endpoint_auth_method.as_deref() {
        // RFC 7591's default.
        None => ClientAuthMethod::ClientSecretBasic,
        Some(text) => ClientAuthMethod::parse(text).ok_or_else(|| {
            metadata(format!(
                "token_endpoint_auth_method {text:?}: none, client_secret_post or \
                 client_secret_basic"
            ))
        })?,
    };
    let grant_types = request
        .grant_types
        .clone()
        .unwrap_or_else(|| vec!["authorization_code".into(), "refresh_token".into()]);
    if let Some(other) = grant_types
        .iter()
        .find(|grant| !["authorization_code", "refresh_token"].contains(&grant.as_str()))
    {
        return Err(metadata(format!(
            "grant_types: {other:?} is not supported: authorization_code and refresh_token"
        )));
    }
    let response_types = request
        .response_types
        .clone()
        .unwrap_or_else(|| vec!["code".into()]);
    if response_types.iter().any(|kind| kind != "code") {
        return Err(metadata("response_types: only code is supported".into()));
    }
    if app
        .store
        .count_oauth_clients(ClientKind::Registered)
        .await?
        >= MAX_REGISTERED_CLIENTS
    {
        return Err(Refused::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "too many clients are registered now: retry later",
        ));
    }
    let id = format!("{REGISTERED_CLIENT_PREFIX}{}", random_text(22)?);
    let shown = if method.confidential() {
        Some(secret(CLIENT_SECRET_PREFIX)?)
    } else {
        None
    };
    let now = world_now();
    let client = OAuthClient {
        id: id.clone(),
        kind: ClientKind::Registered,
        name: name(request.client_name.as_deref()).unwrap_or_else(|| "An MCP client".into()),
        uri: site(request.client_uri.as_deref()),
        logo_uri: site(request.logo_uri.as_deref()),
        redirect_uris: redirect,
        auth_method: method,
        secret_hash: shown.as_deref().map(hash),
        fetched_at: None,
        expires_at: None,
        created_at: now,
        last_used_at: None,
    };
    app.store.put_oauth_client(&client).await?;
    Ok(RegisteredClient {
        client_id: id,
        client_secret: shown,
        client_id_issued_at: now.as_millis() / 1_000,
        client_secret_expires_at: method.confidential().then_some(0),
        redirect_uris: client.redirect_uris,
        client_name: client.name,
        client_uri: client.uri,
        logo_uri: client.logo_uri,
        token_endpoint_auth_method: method.as_str().to_owned(),
        grant_types,
        response_types,
    })
}

// -------------------------------------------------------------------------------------------
// Client authentication, at the token and revocation endpoints

/// Whether the request's body is of the media type `expected`.
fn media_type(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case(expected)
        })
}

/// A form of the token or revocation endpoint.
fn form<T: DeserializeOwned>(headers: &HeaderMap, body: &[u8]) -> Result<T, Refused> {
    if !media_type(headers, "application/x-www-form-urlencoded") {
        return Err(Refused::request(
            "the body is a form: Content-Type: application/x-www-form-urlencoded",
        ));
    }
    serde_html_form::from_bytes(body)
        .map_err(|error| Refused::request(format!("the form cannot be read: {error}")))
}

/// The client of a request to the token or revocation endpoint, authenticated as it registered:
/// a public client by its `client_id`, a confidential one by its secret, in the form or in HTTP
/// Basic authentication. A metadata document's client is public.
#[derive(Debug)]
struct Authenticated {
    id: String,
    /// Its name, when Rewind keeps it.
    name: Option<String>,
}

async fn authenticate_client(
    app: &App,
    headers: &HeaderMap,
    form_id: Option<&str>,
    form_secret: Option<&str>,
) -> Result<Authenticated, Refused> {
    let basic = basic_credentials(headers)?;
    let with_basic = |mut refused: Refused| {
        refused.basic = basic.is_some();
        refused
    };
    let (id, secret, by_basic) = match &basic {
        Some((id, secret)) => {
            if form_secret.is_some() {
                return Err(Refused::request(
                    "the client authenticates one way: HTTP Basic or the form, not both",
                ));
            }
            if form_id.is_some_and(|form_id| form_id != id) {
                return Err(Refused::request(
                    "client_id in the form is not the client of HTTP Basic",
                ));
            }
            (id.clone(), Some(secret.clone()), true)
        }
        None => {
            let Some(id) = form_id.filter(|id| !id.is_empty()) else {
                return Err(Refused::client(
                    "the client is not identified: client_id in the form, or HTTP Basic",
                ));
            };
            (id.to_owned(), form_secret.map(ToOwned::to_owned), false)
        }
    };
    if id.contains("://") {
        // A metadata document's client: public, its codes bound by PKCE.
        if secret.is_some() {
            return Err(with_basic(Refused::client(
                "a client of a metadata document is public: it sends no secret",
            )));
        }
        if id.len() > 2_048 || id.chars().any(char::is_control) {
            return Err(with_basic(Refused::client(
                "this client_id is no metadata document's URL",
            )));
        }
        let name = app.store.oauth_client(&id).await?.map(|client| client.name);
        return Ok(Authenticated { id, name });
    }
    let found = if plain_id(&id) {
        app.store.oauth_client(&id).await?
    } else {
        None
    };
    let Some(client) = found else {
        return Err(with_basic(Refused::client(format!(
            "no client {id} is registered here: register again with POST /oauth/register"
        ))));
    };
    let refused = || {
        with_basic(Refused::client(format!(
            "the client {id} authenticates with {}",
            client.auth_method.as_str()
        )))
    };
    match client.auth_method {
        ClientAuthMethod::None if secret.is_some() => return Err(refused()),
        ClientAuthMethod::None => {}
        ClientAuthMethod::ClientSecretBasic | ClientAuthMethod::ClientSecretPost => {
            let expected = if client.auth_method == ClientAuthMethod::ClientSecretBasic {
                by_basic
            } else {
                !by_basic
            };
            let matches = match (&secret, client.secret_hash) {
                (Some(given), Some(stored)) => {
                    crate::oauth::same(hash(given).as_bytes(), stored.as_bytes())
                }
                _ => false,
            };
            if !expected || !matches {
                return Err(refused());
            }
        }
    }
    Ok(Authenticated {
        id,
        name: Some(client.name),
    })
}

/// The id and secret of HTTP Basic authentication, each form-urlencoded (RFC 6749, section
/// 2.3.1); `None` without `Authorization`.
fn basic_credentials(headers: &HeaderMap) -> Result<Option<(String, String)>, Refused> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let invalid = || {
        let mut refused = Refused::client("the Authorization header must be Basic <id:secret>");
        refused.basic = true;
        refused
    };
    let text = value.to_str().map_err(|_| invalid())?;
    let Some((scheme, encoded)) = text.split_once(' ') else {
        return Err(invalid());
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return Err(invalid());
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|_| invalid())?;
    let decoded = String::from_utf8(decoded).map_err(|_| invalid())?;
    let (id, secret) = decoded.split_once(':').ok_or_else(invalid)?;
    let unescape = |part: &str| -> Result<String, Refused> {
        let pairs: Vec<(String, String)> =
            serde_html_form::from_str(&format!("v={part}")).map_err(|_| invalid())?;
        Ok(pairs
            .into_iter()
            .next()
            .map(|(_, value)| value)
            .unwrap_or_default())
    };
    Ok(Some((unescape(id)?, unescape(secret)?)))
}

// -------------------------------------------------------------------------------------------
// Tokens

/// `POST /oauth/token`: `authorization_code` with PKCE creates the connection, its grants and
/// its tokens; `refresh_token` replaces both tokens. A code or refresh token unknown, expired,
/// used or another client's is `invalid_grant`; one used twice also revokes its connection.
pub(super) async fn token(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = without_oauth(&app) {
        return response;
    }
    match tokens(&app, &headers, &body).await {
        Ok(tokens) => no_store(Json(tokens).into_response()),
        Err(refused) => refused.into_response(),
    }
}

async fn tokens(app: &App, headers: &HeaderMap, body: &[u8]) -> Result<TokenResponse, Refused> {
    let (oauth, _) = app.oauth()?;
    let request: TokenRequest = form(headers, body)?;
    let client = authenticate_client(
        app,
        headers,
        request.client_id.as_deref(),
        request.client_secret.as_deref(),
    )
    .await?;
    if let Some(resource) = &request.resource
        && resource.trim_end_matches('/') != oauth.resource
    {
        return Err(Refused::new(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            format!("this server issues tokens for {} only", oauth.resource),
        ));
    }
    match request.grant_type.as_deref() {
        Some("authorization_code") => exchange_code(app, &client, &request).await,
        Some("refresh_token") => refresh(app, &client, &request).await,
        Some(other) => Err(Refused::new(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            format!("grant_type {other:?}: authorization_code or refresh_token"),
        )),
        None => Err(Refused::request(
            "grant_type is required: authorization_code or refresh_token",
        )),
    }
}

/// A new access token and refresh token, issued now.
fn new_tokens() -> Result<(String, NewToken, String, NewToken), Refused> {
    let now = world_now();
    let access = secret(CONNECTION_TOKEN_PREFIX)?;
    let refresh = secret(REFRESH_TOKEN_PREFIX)?;
    Ok((
        access.clone(),
        NewToken {
            hash: hash(&access),
            expires_at: after(now, ACCESS_TOKEN_LIFETIME),
        },
        refresh.clone(),
        NewToken {
            hash: hash(&refresh),
            expires_at: after(now, REFRESH_TOKEN_LIFETIME),
        },
    ))
}

fn answer(access: String, refresh: String) -> TokenResponse {
    TokenResponse {
        access_token: access,
        token_type: "Bearer".into(),
        expires_in: ACCESS_TOKEN_LIFETIME.as_secs(),
        refresh_token: refresh,
        scope: OFFLINE_ACCESS.into(),
    }
}

async fn exchange_code(
    app: &App,
    client: &Authenticated,
    request: &TokenRequest,
) -> Result<TokenResponse, Refused> {
    let Some(code) = request.code.as_deref().filter(|code| !code.is_empty()) else {
        return Err(Refused::request("code is required"));
    };
    let code_hash = hash(code);
    let Some(stored) = app.store.oauth_code(code_hash).await? else {
        return Err(Refused::grant("this code is unknown"));
    };
    let now = world_now();
    if stored.used_at.is_some() {
        // Exchanged twice: the connection it made is revoked.
        if let Some(id) = &stored.connection {
            app.store.revoke_connection(id, now).await?;
        }
        return Err(Refused::grant(
            "this code was already exchanged: the connection it made is revoked, and the \
             client connects again",
        ));
    }
    if stored.client_id != client.id {
        return Err(Refused::grant("this code was given to another client"));
    }
    if now >= stored.expires_at {
        return Err(Refused::grant(
            "this code expired (5 minutes): the client connects again",
        ));
    }
    if request.redirect_uri.as_deref() != Some(stored.redirect_uri.as_str()) {
        return Err(Refused::grant(
            "redirect_uri must be the one of the authorization request",
        ));
    }
    let Some(verifier) = request.code_verifier.as_deref() else {
        return Err(Refused::request("code_verifier is required (PKCE)"));
    };
    if !pkce_matches(&stored.code_challenge, verifier) {
        return Err(Refused::grant(
            "code_verifier does not answer the code_challenge of the authorization request",
        ));
    }
    let (access, access_token, refresh, refresh_token) = new_tokens()?;
    let organizations = stored
        .organizations
        .iter()
        .map(|(tenant, access)| OpenedOrganization {
            tenant: tenant.clone(),
            access: *access,
            allowed_labels: stored.labels.clone(),
        })
        .collect();
    let new = NewConnection {
        connection: Connection {
            id: format!("{CONNECTION_PREFIX}{}", random_text(22)?),
            user: stored.user.clone(),
            client_id: client.id.clone(),
            client_name: client.name.clone().unwrap_or_else(|| host_of(&client.id)),
            created_at: now,
            last_used_at: None,
            revoked_at: None,
            expires_at: refresh_token.expires_at,
        },
        organizations,
        access: access_token,
        refresh: refresh_token,
    };
    match app.store.redeem_oauth_code(code_hash, &new, now).await? {
        CodeRedemption::Redeemed(connection) => {
            tracing::info!(connection = %connection.id, client = %connection.client_id, "a connection was made");
            Ok(answer(access, refresh))
        }
        CodeRedemption::Unknown => Err(Refused::grant("this code is unknown")),
        CodeRedemption::Used => Err(Refused::grant(
            "this code was already exchanged: the connection it made is revoked, and the \
             client connects again",
        )),
    }
}

/// A client's id as a name: a metadata document's host.
fn host_of(id: &str) -> String {
    reqwest::Url::parse(id)
        .ok()
        .and_then(|url| url.host_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| id.to_owned())
}

async fn refresh(
    app: &App,
    client: &Authenticated,
    request: &TokenRequest,
) -> Result<TokenResponse, Refused> {
    let Some(given) = request
        .refresh_token
        .as_deref()
        .filter(|token| !token.is_empty())
    else {
        return Err(Refused::request("refresh_token is required"));
    };
    let token_hash = hash(given);
    let unknown = || Refused::grant("this refresh token is unknown, or its connection was revoked");
    let Some(stored) = app
        .store
        .connection_token(token_hash)
        .await?
        .filter(|stored| stored.kind == ConnectionTokenKind::Refresh)
    else {
        return Err(unknown());
    };
    let Some(connection) = app
        .store
        .connection(&stored.connection)
        .await?
        .filter(|connection| connection.revoked_at.is_none())
    else {
        return Err(unknown());
    };
    if connection.client_id != client.id {
        return Err(Refused::grant(
            "this refresh token was given to another client",
        ));
    }
    let now = world_now();
    let (access, access_token, refresh, refresh_token) = new_tokens()?;
    if stored.used_at.is_none() && now < stored.expires_at {
        leave_organizations_left(app, &connection).await?;
    }
    match app
        .store
        .rotate_refresh_token(token_hash, &access_token, &refresh_token, now)
        .await?
    {
        Rotation::Rotated(_) => Ok(answer(access, refresh)),
        Rotation::Unknown => Err(unknown()),
        Rotation::Expired => Err(Refused::grant(
            "this refresh token lapsed (30 days unused): the client connects again",
        )),
        Rotation::Used => Err(Refused::grant(
            "this refresh token was already used: its connection is revoked, and the client \
             connects again",
        )),
    }
}

/// Removes from the connection the organizations its person is no longer an active member of,
/// as the identity provider says now.
async fn leave_organizations_left(app: &App, connection: &Connection) -> Result<(), Refused> {
    let (_, sign_in) = app.oauth()?;
    let memberships = match sign_in.identity.memberships_of_user(&connection.user).await {
        Ok(memberships) => memberships,
        Err(IdentityError::NotFound(_)) => Vec::new(),
        Err(error) => {
            tracing::warn!(%error, "the person's memberships could not be checked at a refresh");
            return Err(Refused::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "temporarily_unavailable",
                "the person's memberships cannot be checked now: retry later",
            ));
        }
    };
    let active: HashSet<String> = memberships
        .into_iter()
        .filter(|membership| membership.status == MembershipStatus::Active)
        .map(|membership| membership.organization.id.as_str().to_owned())
        .collect();
    let principal = connection.principal();
    for grant in app.store.connection_grants(&connection.id).await? {
        if !active.contains(grant.tenant.as_str()) {
            app.store
                .delete_grant(&grant.tenant, &principal, None)
                .await?;
            tracing::info!(connection = %connection.id, tenant = %grant.tenant, "the person left the organization: the connection no longer opens it");
        }
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Revocation

/// `POST /oauth/revoke` (RFC 7009): an access token or a refresh token revokes its connection.
/// An unknown token is answered as revoked; another client's is refused.
pub(super) async fn revoke(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(response) = without_oauth(&app) {
        return response;
    }
    match revoked(&app, &headers, &body).await {
        Ok(()) => no_store(StatusCode::OK.into_response()),
        Err(refused) => refused.into_response(),
    }
}

async fn revoked(app: &App, headers: &HeaderMap, body: &[u8]) -> Result<(), Refused> {
    let request: RevokeRequest = form(headers, body)?;
    let client = authenticate_client(
        app,
        headers,
        request.client_id.as_deref(),
        request.client_secret.as_deref(),
    )
    .await?;
    let Some(token) = request.token.as_deref().filter(|token| !token.is_empty()) else {
        return Err(Refused::request("token is required"));
    };
    let Some(stored) = app.store.connection_token(hash(token)).await? else {
        return Ok(());
    };
    let Some(connection) = app.store.connection(&stored.connection).await? else {
        return Ok(());
    };
    if connection.client_id != client.id {
        return Err(Refused::new(
            StatusCode::BAD_REQUEST,
            "unauthorized_client",
            "this token was given to another client",
        ));
    }
    app.store
        .revoke_connection(&connection.id, world_now())
        .await?;
    Ok(())
}
