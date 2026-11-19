//! Rewind's OAuth authorization server, for the MCP clients people connect
//! (`archi/07-code-architecture.md`, "MCP and OAuth"): its discovery documents (RFC 9728, RFC
//! 8414), the authorization request, client registration (RFC 7591) and client metadata
//! documents, the token endpoint (RFC 6749, PKCE of RFC 7636) and revocation (RFC 7009).
//!
//! These are OAuth's wire formats, not the API's: fields are snake_case, as the RFCs name them,
//! the token and revocation endpoints take `application/x-www-form-urlencoded`, and their errors
//! are [`OAuthError`] (`application/json`), not problems.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// The prefix of every client registered with `POST /oauth/register`.
pub const REGISTERED_CLIENT_PREFIX: &str = "rc_";

/// The prefix of a registered client's secret, shown once.
pub const CLIENT_SECRET_PREFIX: &str = "rewind_cs_";

/// The prefix of every connection's refresh token.
pub const REFRESH_TOKEN_PREFIX: &str = "rewind_rt_";

/// The scope Rewind lists: rights come from the consent, not from scopes, and an unknown scope
/// is ignored.
pub const OFFLINE_ACCESS: &str = "offline_access";

/// `GET /.well-known/oauth-protected-resource/mcp` (RFC 9728): where to get tokens for `/mcp`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProtectedResourceMetadata {
    /// `{public-url}/mcp`.
    pub resource: String,
    /// `[{public-url}]`.
    pub authorization_servers: Vec<String>,
    pub bearer_methods_supported: Vec<String>,
    pub scopes_supported: Vec<String>,
    pub resource_name: String,
}

/// `GET /.well-known/oauth-authorization-server` (RFC 8414).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthorizationServerMetadata {
    /// `{public-url}`.
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub registration_endpoint: String,
    pub revocation_endpoint: String,
    pub response_types_supported: Vec<String>,
    pub grant_types_supported: Vec<String>,
    pub code_challenge_methods_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    pub revocation_endpoint_auth_methods_supported: Vec<String>,
    pub scopes_supported: Vec<String>,
    /// A `client_id` may be the `https` URL of the client's metadata document.
    pub client_id_metadata_document_supported: bool,
}

/// `GET /oauth/authorize`: the authorization request. Every parameter is read as text, so that
/// a missing one is answered as OAuth says rather than refused before.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AuthorizeQuery {
    /// `code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_type: Option<String>,
    /// The URL of the client's metadata document, or `rc_…`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// One of the client's redirect URIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    /// The PKCE challenge: BASE64URL(SHA-256(code_verifier)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge: Option<String>,
    /// `S256`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_challenge_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Ignored: rights come from the consent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// `{public-url}/mcp` when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

/// A client's metadata: the body of `POST /oauth/register` (RFC 7591), and a client's metadata
/// document, whose `client_id` is its own URL. Other fields are ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ClientMetadata {
    /// A metadata document's own URL; none at registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Each `https`, `http` on a loopback address, or a private-use scheme holding a dot
    /// (`cursor://…`).
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
    /// `none`, `client_secret_post` or `client_secret_basic` (RFC 7591's default) at
    /// registration; `none` in a metadata document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_types: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_types: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Never in a metadata document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

/// `POST /oauth/register`'s answer (RFC 7591): the client, with its secret for a confidential
/// one, shown only here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RegisteredClient {
    /// `rc_…`.
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// Seconds since the Unix epoch.
    pub client_id_issued_at: u64,
    /// 0: the secret does not expire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<u64>,
    pub redirect_uris: Vec<String>,
    pub client_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
    pub token_endpoint_auth_method: String,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
}

/// `POST /oauth/token`, a form: `authorization_code` (with `code`, `redirect_uri`,
/// `code_verifier`) or `refresh_token` (with `refresh_token`). The client's id and secret come
/// in the form or in HTTP Basic authentication.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TokenRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_verifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// `{public-url}/mcp` when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// Ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// `POST /oauth/token`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TokenResponse {
    /// `rewind_ct_…`: the Bearer of `/mcp` and `/v1`.
    pub access_token: String,
    /// `Bearer`.
    pub token_type: String,
    /// 3600.
    pub expires_in: u64,
    /// `rewind_rt_…`: used once, it lapses after 30 days unused.
    pub refresh_token: String,
    pub scope: String,
}

/// `POST /oauth/revoke` (RFC 7009), a form.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RevokeRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// `access_token` or `refresh_token`; either revokes the connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

/// An OAuth error (RFC 6749, section 5.2; RFC 7591, section 3.2.2), answered as
/// `application/json` with `Cache-Control: no-store`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OAuthError {
    /// `invalid_request`, `invalid_client`, `invalid_grant`, `unauthorized_client`,
    /// `unsupported_grant_type`, `invalid_target`, `invalid_redirect_uri`,
    /// `invalid_client_metadata`, `temporarily_unavailable`, `unsupported_token_type`.
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}
