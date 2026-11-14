//! The dashboard's sign-in, `/auth` (`archi/07-code-architecture.md`, "Sign-in (`/auth`)"), on
//! the api role when WorkOS is configured; 404 otherwise.
//!
//! For a browser only: these routes read and write the sealed cookies [`SESSION_COOKIE`] (the
//! session's tokens or the pending step, `SameSite=Strict`) and [`OAUTH_COOKIE`] (a GitHub
//! sign-in in progress, `SameSite=Lax`, 5 minutes), both `HttpOnly`, `Secure`, `Path=/auth`. A
//! `POST` takes `Content-Type: application/json` only (415), which a form of another site cannot
//! send without a preflight. Every answer but the redirects, 202 and 204 is an [`AuthState`];
//! the access token is in the body only, the refresh token never leaves the cookie.
//!
//! A session established on an organization writes the member's first grant there
//! ([`first_grant`]).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::auth::{
    AuthConfig, AuthState, Callback, EmailVerification, GithubSignIn, MagicAuthRequest,
    MagicAuthVerification, OAUTH_COOKIE, OrganizationSelection, SESSION_COOKIE, SessionRequest,
    SignInMethod, WaitlistRequest,
};
use rewind_api::common::timestamp;
use rewind_api::organization::{Organization, User};
use rewind_core::Timestamp;
use rewind_engine::adapters::SystemClock;
use rewind_engine::ports::{
    self, Authentication, Client, Clock, IdentityError, IdentityProvider, MembershipStatus,
    OrganizationId, PendingToken, Provider, SessionId, UserId,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::app::{App, SignIn};
use crate::error::ApiError;
use crate::extract::Query;
use crate::grants::first_grant;

/// How long the session cookie lasts in the browser: WorkOS's longest session. The session
/// ends sooner when WorkOS ends it: its refresh token then fails, and the cookie is cleared.
const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 86_400);

/// How long a GitHub sign-in may take, from `GET /auth/github` to the callback.
const OAUTH_LIFETIME: Duration = Duration::from_secs(300);

/// An access token with less left than this is refreshed.
const REFRESH_BEFORE: Duration = Duration::from_secs(60);

/// The longest user agent passed on to the identity provider.
const USER_AGENT_LIMIT: usize = 512;

/// Where the browser goes after the callback: the dashboard.
const DASHBOARD: &str = "/#/";

/// Real time: tokens expire in the world's time, even on a server in virtual time.
fn now() -> Timestamp {
    SystemClock.now()
}

// -------------------------------------------------------------------------------------------
// The cookies

/// What [`SESSION_COOKIE`] holds, sealed: the session's tokens, or the step the identity
/// provider asks for with its pending token.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
enum Stored {
    Authenticated {
        access_token: String,
        /// Kept here only: never given to the browser's scripts.
        refresh_token: String,
        session: String,
        user: User,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        organization_id: Option<String>,
        /// Milliseconds since the epoch.
        expires_at: u64,
    },
    OrganizationSelection {
        pending: String,
        user: User,
        organizations: Vec<Organization>,
    },
    EmailVerification {
        pending: String,
        email: String,
    },
    SignUpClosed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
    },
}

/// What [`OAUTH_COOKIE`] holds, sealed, during a GitHub sign-in.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OAuth {
    state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    invitation_token: Option<String>,
    /// Milliseconds since the epoch: the cookie's own `Max-Age` is the browser's to keep.
    expires_at: u64,
}

fn user_view(user: &ports::User) -> User {
    User {
        id: user.id.as_str().to_owned(),
        email: user.email.clone(),
        first_name: user.first_name.clone(),
        last_name: user.last_name.clone(),
        picture_url: user.picture_url.clone(),
    }
}

impl From<Authentication> for Stored {
    fn from(authentication: Authentication) -> Self {
        match authentication {
            Authentication::Authenticated(session) => Stored::Authenticated {
                access_token: session.tokens.access_token,
                refresh_token: session.tokens.refresh_token,
                session: session.claims.session.as_str().to_owned(),
                user: user_view(&session.user),
                organization_id: session
                    .organization
                    .or(session.claims.organization)
                    .map(|organization| organization.as_str().to_owned()),
                expires_at: session.claims.expires_at.as_millis(),
            },
            Authentication::OrganizationSelectionRequired {
                pending,
                user,
                organizations,
            } => Stored::OrganizationSelection {
                pending: pending.expose().to_owned(),
                user: user_view(&user),
                organizations: organizations
                    .iter()
                    .map(|organization| Organization {
                        id: organization.id.as_str().to_owned(),
                        name: organization.name.clone(),
                    })
                    .collect(),
            },
            Authentication::EmailVerificationRequired { pending, email } => {
                Stored::EmailVerification {
                    pending: pending.expose().to_owned(),
                    email,
                }
            }
            Authentication::SignUpClosed { email } => Stored::SignUpClosed { email },
        }
    }
}

impl Stored {
    /// Where the sign-in stands, as the dashboard reads it.
    fn state(&self) -> AuthState {
        match self {
            Stored::Authenticated {
                access_token,
                user,
                organization_id,
                expires_at,
                ..
            } => AuthState::Authenticated {
                access_token: access_token.clone(),
                expires_at: timestamp(Timestamp::from_millis(*expires_at)),
                user: user.clone(),
                organization_id: organization_id.clone(),
            },
            Stored::OrganizationSelection {
                user,
                organizations,
                ..
            } => AuthState::OrganizationSelectionRequired {
                user: user.clone(),
                organizations: organizations.clone(),
            },
            Stored::EmailVerification { email, .. } => AuthState::EmailVerificationRequired {
                email: email.clone(),
            },
            Stored::SignUpClosed { email } => AuthState::SignUpClosed {
                email: email.clone(),
            },
        }
    }

    /// The signed-in user and their organization, when signed in on one.
    fn member(&self) -> Option<(UserId, OrganizationId)> {
        match self {
            Stored::Authenticated {
                user,
                organization_id: Some(organization),
                ..
            } => Some((
                UserId::new(user.id.clone()),
                OrganizationId::new(organization.clone()),
            )),
            _ => None,
        }
    }
}

/// The value of the cookie `name` in the request.
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.trim())
}

#[derive(Clone, Copy)]
enum SameSite {
    /// The session: never sent by a request another site starts.
    Strict,
    /// The GitHub sign-in in progress: sent on the navigation back from GitHub and WorkOS.
    Lax,
}

fn set_cookie(name: &str, value: &str, max_age: Duration, same_site: SameSite) -> HeaderValue {
    let same_site = match same_site {
        SameSite::Strict => "Strict",
        SameSite::Lax => "Lax",
    };
    let text = format!(
        "{name}={value}; Path=/auth; Max-Age={}; HttpOnly; Secure; SameSite={same_site}",
        max_age.as_secs()
    );
    // A sealed value is base64: always a valid header value.
    HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn clear_cookie(name: &str, same_site: SameSite) -> HeaderValue {
    set_cookie(name, "", Duration::ZERO, same_site)
}

fn clear_session() -> HeaderValue {
    clear_cookie(SESSION_COOKIE, SameSite::Strict)
}

/// An answer of `/auth`: never cached, since it may carry an access token.
fn respond(status: StatusCode, body: Option<&AuthState>, cookies: &[HeaderValue]) -> Response {
    let mut response = match body {
        Some(state) => (status, Json(state)).into_response(),
        None => status.into_response(),
    };
    no_store(&mut response);
    for cookie in cookies {
        response
            .headers_mut()
            .append(header::SET_COOKIE, cookie.clone());
    }
    response
}

fn no_store(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

/// The session as the browser sent it; `None` without a cookie. A cookie that does not unseal
/// (forged, or sealed with a former password) is refused and cleared.
fn session(sign_in: &SignIn, headers: &HeaderMap) -> Result<Option<Stored>, Refusal> {
    let Some(sealed) = cookie(headers, SESSION_COOKIE).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    sign_in
        .seal
        .unseal(sealed)
        .map(Some)
        .map_err(|_| Refusal::signed_out("the session cannot be read: sign in again"))
}

/// The session, or 401 without one.
fn required_session(sign_in: &SignIn, headers: &HeaderMap) -> Result<Stored, Refusal> {
    session(sign_in, headers)?.ok_or_else(|| {
        Refusal::from(ApiError::new(
            ProblemType::Unauthorized,
            "no session: sign in with GET /auth/github or POST /auth/magic-auth",
        ))
    })
}

/// The answer to a step: the new state, kept in the session cookie.
fn answer(sign_in: &SignIn, stored: &Stored) -> Result<Response, Refusal> {
    let sealed = sign_in.seal.seal(stored).map_err(ApiError::internal)?;
    Ok(respond(
        StatusCode::OK,
        Some(&stored.state()),
        &[set_cookie(
            SESSION_COOKIE,
            &sealed,
            SESSION_LIFETIME,
            SameSite::Strict,
        )],
    ))
}

/// The end of a sign-in step: the member's first grant when signed in on an organization, then
/// the answer. A grant that cannot be written now is written by the next `POST /auth/session`.
async fn finish(app: &App, sign_in: &SignIn, stored: Stored) -> Result<Response, Refusal> {
    if let Some((user, organization)) = stored.member()
        && let Err(error) = first_grant(app, &sign_in.identity, &user, &organization).await
    {
        tracing::warn!(%organization, detail = %error.0.detail, "the member's first grant was not written");
    }
    answer(sign_in, &stored)
}

// -------------------------------------------------------------------------------------------
// Errors

/// An error of `/auth`: a problem, which may also clear the session cookie.
#[derive(Debug)]
pub(super) struct Refusal {
    error: ApiError,
    clear: bool,
}

impl Refusal {
    /// 401, and the session cookie cleared: the session is over.
    fn signed_out(detail: &str) -> Self {
        Self {
            error: ApiError::new(ProblemType::Unauthorized, detail),
            clear: true,
        }
    }
}

impl From<ApiError> for Refusal {
    fn from(error: ApiError) -> Self {
        Self {
            error,
            clear: false,
        }
    }
}

impl From<IdentityError> for Refusal {
    fn from(error: IdentityError) -> Self {
        match error {
            IdentityError::InvalidToken => {
                Self::signed_out("the sign-in or the session expired: sign in again")
            }
            other => ApiError::from(other).into(),
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let mut response = self.error.into_response();
        no_store(&mut response);
        if self.clear {
            response
                .headers_mut()
                .append(header::SET_COOKIE, clear_session());
        }
        response
    }
}

// -------------------------------------------------------------------------------------------
// Requests

/// A `POST`'s JSON body: `Content-Type: application/json` is required (415 otherwise), so that
/// a form of another site cannot send it.
#[derive(Debug)]
pub(super) struct JsonBody<T>(T);

/// The same, for a body that may be empty: its default then.
#[derive(Debug)]
pub(super) struct OptionalJsonBody<T>(T);

async fn json_bytes<S: Send + Sync>(request: Request, state: &S) -> Result<Bytes, ApiError> {
    let json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let media = value.split(';').next().unwrap_or_default().trim();
            media.eq_ignore_ascii_case("application/json")
        });
    if !json {
        return Err(ApiError::new(
            ProblemType::UnsupportedMediaType,
            "a POST to /auth takes a JSON body with `Content-Type: application/json`",
        ));
    }
    Bytes::from_request(request, state)
        .await
        .map_err(|rejection| ApiError::invalid(format!("the body cannot be read: {rejection}")))
}

fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(bytes)
        .map_err(|error| ApiError::invalid(format!("the body is not valid: {error}")))
}

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = Refusal;

    async fn from_request(request: Request, state: &S) -> Result<Self, Refusal> {
        let bytes = json_bytes(request, state).await?;
        Ok(JsonBody(parse(&bytes)?))
    }
}

impl<S: Send + Sync, T: DeserializeOwned + Default> FromRequest<S> for OptionalJsonBody<T> {
    type Rejection = Refusal;

    async fn from_request(request: Request, state: &S) -> Result<Self, Refusal> {
        let bytes = json_bytes(request, state).await?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(OptionalJsonBody(T::default()));
        }
        Ok(OptionalJsonBody(parse(&bytes)?))
    }
}

/// The browser behind a request, as the identity provider records it on the session: its user
/// agent, and its address when it is a public one. The address is the last of
/// `X-Forwarded-For`, which the deployment's edge writes (Caddy ignores the one a client sends,
/// `deploy/`), else the connection's peer. An address of a private, shared or local network is
/// a proxy of the deployment, not a browser: it is not passed on.
#[derive(Debug)]
pub(super) struct Browser(Client);

impl<S: Send + Sync> FromRequestParts<S> for Browser {
    type Rejection = Refusal;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Refusal> {
        let user_agent = parts
            .headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(|agent| agent.chars().take(USER_AGENT_LIMIT).collect::<String>())
            .filter(|agent| !agent.trim().is_empty());
        let forwarded = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .next_back()
            .and_then(|address| address.trim().parse::<IpAddr>().ok());
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(address)| address.ip());
        let ip_address = forwarded
            .or(peer)
            .filter(|address| public(*address))
            .map(|address| address.to_string());
        Ok(Browser(Client {
            ip_address,
            user_agent,
        }))
    }
}

/// Whether an address is of the public internet: not private, shared (carrier-grade NAT),
/// loopback, link-local, unique-local, documentation or unspecified.
fn public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || first == 0x2001 && v6.segments()[1] == 0x0db8)
        }
    }
}

/// An email as a person typed it, trimmed; 400 when blank.
fn email(text: &str) -> Result<&str, ApiError> {
    let email = text.trim();
    if email.is_empty() || email.contains('\0') {
        return Err(ApiError::invalid("email: an email address is required"));
    }
    Ok(email)
}

fn invitation_token(token: Option<&String>) -> Option<&str> {
    token
        .map(|token| token.trim())
        .filter(|token| !token.is_empty())
}

/// 256 random bits in hex: the `state` of a GitHub sign-in.
fn random_state() -> Result<String, ApiError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(ApiError::internal)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Compares two texts in a time that does not depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |differ, (x, y)| differ | (x ^ y))
            == 0
}

// -------------------------------------------------------------------------------------------
// Routes

/// `GET /auth/config`: the sign-in methods; 404 without WorkOS, and the dashboard asks for an
/// API key.
pub(super) async fn config(State(app): State<Arc<App>>) -> Result<Response, Refusal> {
    app.sign_in()?;
    let mut response = Json(AuthConfig {
        methods: vec![SignInMethod::Github, SignInMethod::MagicAuth],
        // Sign-ups are closed: an unknown person joins the waitlist (06).
        waitlist: true,
    })
    .into_response();
    no_store(&mut response);
    Ok(response)
}

/// `GET /auth/github`: to WorkOS for GitHub, with a random `state` and the page's invitation
/// token kept in [`OAUTH_COOKIE`].
pub(super) async fn github(
    State(app): State<Arc<App>>,
    Query(query): Query<GithubSignIn>,
) -> Result<Response, Refusal> {
    let sign_in = app.sign_in()?;
    let state = random_state()?;
    let url =
        sign_in
            .identity
            .authorization_url(Provider::GitHub, &sign_in.redirect_uri, &state)?;
    let location = HeaderValue::from_str(&url).map_err(ApiError::internal)?;
    let oauth = OAuth {
        state,
        invitation_token: invitation_token(query.invitation_token.as_ref()).map(str::to_owned),
        expires_at: now().saturating_add(OAUTH_LIFETIME).as_millis(),
    };
    let sealed = sign_in.seal.seal(&oauth).map_err(ApiError::internal)?;
    let mut response = respond(
        StatusCode::FOUND,
        None,
        &[set_cookie(
            OAUTH_COOKIE,
            &sealed,
            OAUTH_LIFETIME,
            SameSite::Lax,
        )],
    );
    response.headers_mut().insert(header::LOCATION, location);
    Ok(response)
}

/// Why a callback did not sign in, for the dashboard: a code that says nothing secret.
#[derive(Debug, Clone, Copy)]
enum CallbackFailure {
    /// No sign-in in progress in this browser, or another one's `state`.
    StateMismatch,
    /// GitHub or WorkOS answered with an error, or without a code.
    ProviderError,
    /// The code could not be exchanged.
    SignInFailed,
}

impl CallbackFailure {
    fn code(self) -> &'static str {
        match self {
            CallbackFailure::StateMismatch => "state_mismatch",
            CallbackFailure::ProviderError => "provider_error",
            CallbackFailure::SignInFailed => "sign_in_failed",
        }
    }
}

/// `GET /auth/callback`: checks `state`, exchanges the code, keeps the outcome in the session
/// cookie, and sends the browser to the dashboard: `/#/`, or `/?auth_error=<code>#/` when the
/// sign-in did not go through. A failure leaves the session cookie as it was, so a forged
/// callback cannot sign anyone in or out; the GitHub sign-in in progress is over either way.
pub(super) async fn callback(
    State(app): State<Arc<App>>,
    Browser(client): Browser,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Result<Response, Refusal> {
    let sign_in = app.sign_in()?;
    let oauth: Option<OAuth> = cookie(&headers, OAUTH_COOKIE)
        .and_then(|sealed| sign_in.seal.unseal(sealed).ok())
        .filter(|oauth: &OAuth| oauth.expires_at > now().as_millis());
    let mut cookies = vec![clear_cookie(OAUTH_COOKIE, SameSite::Lax)];
    let outcome = match (&oauth, query.state.as_deref()) {
        (Some(oauth), Some(state)) if same(&oauth.state, state) => {
            match (&query.error, &query.code) {
                (Some(error), _) => {
                    tracing::info!(%error, "the identity provider refused a GitHub sign-in");
                    Err(CallbackFailure::ProviderError)
                }
                (None, None) => Err(CallbackFailure::ProviderError),
                (None, Some(code)) => match sign_in
                    .identity
                    .authenticate_code(code, oauth.invitation_token.as_deref(), &client)
                    .await
                {
                    Ok(authentication) => Ok(Stored::from(authentication)),
                    Err(error) => {
                        tracing::info!(%error, "a GitHub sign-in failed");
                        Err(CallbackFailure::SignInFailed)
                    }
                },
            }
        }
        _ => Err(CallbackFailure::StateMismatch),
    };
    let location = match outcome {
        Ok(stored) => {
            if let Some((user, organization)) = stored.member()
                && let Err(error) = first_grant(&app, &sign_in.identity, &user, &organization).await
            {
                tracing::warn!(%organization, detail = %error.0.detail, "the member's first grant was not written");
            }
            let sealed = sign_in.seal.seal(&stored).map_err(ApiError::internal)?;
            cookies.push(set_cookie(
                SESSION_COOKIE,
                &sealed,
                SESSION_LIFETIME,
                SameSite::Strict,
            ));
            DASHBOARD.to_owned()
        }
        Err(failure) => format!("/?auth_error={}#/", failure.code()),
    };
    let mut response = respond(StatusCode::FOUND, None, &cookies);
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(&location).map_err(ApiError::internal)?,
    );
    Ok(response)
}

/// `POST /auth/magic-auth`: sends a 6-digit code; the same answer whether the email has an
/// account or not.
pub(super) async fn send_magic_code(
    State(app): State<Arc<App>>,
    JsonBody(request): JsonBody<MagicAuthRequest>,
) -> Result<Response, Refusal> {
    let sign_in = app.sign_in()?;
    sign_in
        .identity
        .send_magic_code(
            email(&request.email)?,
            invitation_token(request.invitation_token.as_ref()),
        )
        .await?;
    Ok(respond(StatusCode::ACCEPTED, None, &[]))
}

/// `POST /auth/magic-auth/verify`: signs in with the code.
pub(super) async fn verify_magic_code(
    State(app): State<Arc<App>>,
    Browser(client): Browser,
    JsonBody(request): JsonBody<MagicAuthVerification>,
) -> Result<Response, Refusal> {
    let sign_in = app.sign_in()?;
    let authentication = sign_in
        .identity
        .authenticate_magic_code(
            email(&request.email)?,
            request.code.trim(),
            invitation_token(request.invitation_token.as_ref()),
            &client,
        )
        .await?;
    finish(&app, sign_in, Stored::from(authentication)).await
}

/// `POST /auth/email-verification`: the code sent after `email_verification_required`.
pub(super) async fn verify_email(
    State(app): State<Arc<App>>,
    Browser(client): Browser,
    headers: HeaderMap,
    JsonBody(request): JsonBody<EmailVerification>,
) -> Result<Response, Refusal> {
    let sign_in = app.sign_in()?;
    let Stored::EmailVerification { pending, .. } = required_session(sign_in, &headers)? else {
        return Err(ApiError::invalid(
            "no email waits to be verified in this sign-in: POST /auth/session says where it \
             stands",
        )
        .into());
    };
    let authentication = sign_in
        .identity
        .verify_email(&PendingToken::new(pending), request.code.trim(), &client)
        .await?;
    finish(&app, sign_in, Stored::from(authentication)).await
}

/// `POST /auth/organization`: the organization chosen after `organization_selection_required`,
/// one of those the step offered (403 otherwise).
pub(super) async fn select_organization(
    State(app): State<Arc<App>>,
    Browser(client): Browser,
    headers: HeaderMap,
    JsonBody(request): JsonBody<OrganizationSelection>,
) -> Result<Response, Refusal> {
    let sign_in = app.sign_in()?;
    let Stored::OrganizationSelection {
        pending,
        organizations,
        ..
    } = required_session(sign_in, &headers)?
    else {
        return Err(ApiError::invalid(
            "no organization is asked for in this sign-in: POST /auth/session says where it \
             stands, and switches organization with organizationId",
        )
        .into());
    };
    let chosen = request.organization_id.trim();
    if !organizations
        .iter()
        .any(|organization| organization.id == chosen)
    {
        return Err(not_a_member(chosen).into());
    }
    let authentication = sign_in
        .identity
        .select_organization(
            &PendingToken::new(pending),
            &OrganizationId::new(chosen),
            &client,
        )
        .await?;
    finish(&app, sign_in, Stored::from(authentication)).await
}

fn not_a_member(organization: &str) -> ApiError {
    ApiError::forbidden(format!(
        "the user is not a member of the organization {organization:?}: choose one of theirs \
         (GET /v1/whoami lists them)"
    ))
}

/// `POST /auth/session`: where the sign-in stands. Signed in: the access token, refreshed when
/// less than a minute is left, or for `organizationId` (a switch, for a member of it). A session
/// that cannot be refreshed any more is over: 401, and the cookie cleared. Writes the member's
/// first grant in the session's organization.
pub(super) async fn session_state(
    State(app): State<Arc<App>>,
    Browser(client): Browser,
    headers: HeaderMap,
    OptionalJsonBody(request): OptionalJsonBody<SessionRequest>,
) -> Result<Response, Refusal> {
    let sign_in = app.sign_in()?;
    let stored = required_session(sign_in, &headers)?;
    let Stored::Authenticated {
        refresh_token,
        user,
        expires_at,
        ..
    } = &stored
    else {
        return answer(sign_in, &stored);
    };
    let switch = request
        .organization_id
        .as_deref()
        .map(str::trim)
        .filter(|organization| !organization.is_empty())
        .map(OrganizationId::new);
    let left = Duration::from_millis(expires_at.saturating_sub(now().as_millis()));
    let stored = if let Some(organization) = &switch {
        let user = UserId::new(user.id.clone());
        let member = sign_in
            .identity
            .memberships_of_user(&user)
            .await?
            .iter()
            .any(|membership| {
                membership.organization.id == *organization
                    && membership.status == MembershipStatus::Active
            });
        if !member {
            return Err(not_a_member(organization.as_str()).into());
        }
        match sign_in
            .identity
            .refresh(refresh_token, Some(organization), &client)
            .await
        {
            Ok(authentication) => Stored::from(authentication),
            Err(IdentityError::Refused { .. }) => {
                return Err(not_a_member(organization.as_str()).into());
            }
            Err(error) => return Err(ended(error)),
        }
    } else if left < REFRESH_BEFORE {
        match sign_in.identity.refresh(refresh_token, None, &client).await {
            Ok(authentication) => Stored::from(authentication),
            Err(error) => return Err(ended(error)),
        }
    } else {
        stored
    };
    if let Some((user, organization)) = stored.member() {
        first_grant(&app, &sign_in.identity, &user, &organization).await?;
    }
    answer(sign_in, &stored)
}

/// A refresh that failed: the session is over (401, the cookie cleared) unless the provider is
/// unavailable.
fn ended(error: IdentityError) -> Refusal {
    match error {
        IdentityError::Unavailable(_) => error.into(),
        _ => Refusal::signed_out("the session ended: sign in again"),
    }
}

/// `POST /auth/waitlist`: puts an email on the waitlist.
pub(super) async fn waitlist(
    State(app): State<Arc<App>>,
    JsonBody(request): JsonBody<WaitlistRequest>,
) -> Result<Response, Refusal> {
    let sign_in = app.sign_in()?;
    sign_in
        .identity
        .join_waitlist(email(&request.email)?)
        .await?;
    Ok(respond(StatusCode::ACCEPTED, None, &[]))
}

/// `POST /auth/logout`: revokes the session in WorkOS and clears the cookie. The cookie is
/// cleared even when WorkOS cannot be reached (502): the browser is signed out, and the refresh
/// token, which only the cookie held, is gone with it.
pub(super) async fn logout(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    OptionalJsonBody(_body): OptionalJsonBody<serde_json::Value>,
) -> Result<Response, Refusal> {
    let sign_in = app.sign_in()?;
    // A cookie that does not unseal holds no session to revoke.
    if let Ok(Some(Stored::Authenticated { session, .. })) = session(sign_in, &headers) {
        match sign_in
            .identity
            .revoke_session(&SessionId::new(session))
            .await
        {
            Ok(()) | Err(IdentityError::NotFound(_) | IdentityError::InvalidToken) => {}
            Err(error) => {
                return Err(Refusal {
                    error: error.into(),
                    clear: true,
                });
            }
        }
    }
    Ok(respond(StatusCode::NO_CONTENT, None, &[clear_session()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses_are_passed_on() {
        for private in [
            "10.0.0.1",
            "172.16.4.2",
            "192.168.1.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
            "fd12::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!public(private.parse().unwrap()), "{private}");
        }
        for open in [
            "8.8.8.8",
            "100.128.0.1",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
        ] {
            assert!(public(open.parse().unwrap()), "{open}");
        }
    }

    #[test]
    fn cookies_are_read_by_name_and_set_with_their_attributes() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; rewind_session=abc=; other=1"),
        );
        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("abc="));
        assert_eq!(cookie(&headers, OAUTH_COOKIE), None);
        let set = set_cookie(SESSION_COOKIE, "v", SESSION_LIFETIME, SameSite::Strict);
        assert_eq!(
            set.to_str().unwrap(),
            "rewind_session=v; Path=/auth; Max-Age=2592000; HttpOnly; Secure; SameSite=Strict"
        );
        assert!(
            clear_cookie(OAUTH_COOKIE, SameSite::Lax)
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
