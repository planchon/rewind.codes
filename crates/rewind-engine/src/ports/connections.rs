//! A person's connections, and the state of the OAuth authorization server they are made
//! through (`archi/06-storage-and-scale.md`, "Connections"; `archi/07-code-architecture.md`,
//! "MCP and OAuth"). The [`AccessStore`](super::AccessStore) keeps them, outside every tenant:
//! a connection is a person's, and only its grants, one per organization it opens, are a
//! tenant's.
//!
//! - An [`OAuthClient`] is a client a person may connect: a client's metadata document, cached
//!   under its URL, or a client registered with Rewind (`rc_…`).
//! - An [`OAuthCode`] is what a consent gives: the request and the choices, until the client
//!   exchanges it, once.
//! - A [`Connection`] is what the exchange creates: one per person and client, holding a grant
//!   (principal `connection:cn_…`) in each organization it opens.
//! - A [`ConnectionToken`] is an access token (`rewind_ct_…`) or a refresh token
//!   (`rewind_rt_…`) of a connection, stored hashed. A refresh token is used once.

use std::collections::BTreeMap;
use std::fmt;

use rewind_core::{Digest, Timestamp};

use super::access::Principal;
use super::identity::UserId;
use super::tenancy::TenantId;

/// How a client made itself known.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientKind {
    /// A client's metadata document (CIMD): its id is the `https` URL of the document, which
    /// Rewind fetches and caches.
    MetadataDocument,
    /// A client registered with `POST /oauth/register` (RFC 7591): its id is `rc_…`.
    Registered,
}

impl ClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientKind::MetadataDocument => "metadata_document",
            ClientKind::Registered => "registered",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "metadata_document" => Some(ClientKind::MetadataDocument),
            "registered" => Some(ClientKind::Registered),
            _ => None,
        }
    }
}

/// How a client authenticates at the token endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientAuthMethod {
    /// A public client: its `client_id` only, its codes bound by PKCE.
    None,
    /// Its secret in the form's `client_secret`.
    ClientSecretPost,
    /// Its secret in HTTP Basic authentication.
    ClientSecretBasic,
}

impl ClientAuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientAuthMethod::None => "none",
            ClientAuthMethod::ClientSecretPost => "client_secret_post",
            ClientAuthMethod::ClientSecretBasic => "client_secret_basic",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "none" => Some(ClientAuthMethod::None),
            "client_secret_post" => Some(ClientAuthMethod::ClientSecretPost),
            "client_secret_basic" => Some(ClientAuthMethod::ClientSecretBasic),
            _ => None,
        }
    }

    /// Whether the client holds a secret.
    pub fn confidential(self) -> bool {
        self != ClientAuthMethod::None
    }
}

/// A client a person may connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthClient {
    /// The URL of its metadata document, or `rc_…` for a registered client.
    pub id: String,
    pub kind: ClientKind,
    /// The name the consent shows.
    pub name: String,
    /// Its site.
    pub uri: Option<String>,
    pub logo_uri: Option<String>,
    /// Where the browser may return with a code.
    pub redirect_uris: Vec<String>,
    pub auth_method: ClientAuthMethod,
    /// The SHA-256 of a confidential client's secret, which is never stored.
    pub secret_hash: Option<Digest>,
    /// For a metadata document: when it was fetched, and until when the copy may serve.
    pub fetched_at: Option<Timestamp>,
    pub expires_at: Option<Timestamp>,
    pub created_at: Timestamp,
    /// When a connection was last made or renewed through it.
    pub last_used_at: Option<Timestamp>,
}

/// What a connection may do in an organization it opens: `read` (`runs:read`, `actions:read`,
/// `namespaces:read`, `secrets:read`) or `run` (`runs:start`, `runs:control` and `secrets:use`
/// too).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Access {
    Read,
    Run,
}

impl Access {
    pub const ALL: [Access; 2] = [Access::Read, Access::Run];

    pub fn as_str(self) -> &'static str {
        match self {
            Access::Read => "read",
            Access::Run => "run",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|access| access.as_str() == text)
    }

    /// The built-in role of the connection's grant: `reader` for `read`, `harness` for `run`.
    pub fn role(self) -> &'static str {
        match self {
            Access::Read => "reader",
            Access::Run => "harness",
        }
    }

    /// The access a grant's role stands for; `None` for a role no access gives.
    pub fn of_role(role: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|access| access.role() == role)
    }
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An authorization code, stored hashed with the request it answers and the person's choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthCode {
    /// The SHA-256 of the code.
    pub hash: Digest,
    pub client_id: String,
    /// The person who consented.
    pub user: UserId,
    pub redirect_uri: String,
    /// The PKCE challenge (S256).
    pub code_challenge: String,
    /// The resource the tokens are for: `{public-url}/mcp`.
    pub resource: String,
    /// The organizations the person ticked, each with its access.
    pub organizations: BTreeMap<TenantId, Access>,
    /// The labels of the connection's grants.
    pub labels: BTreeMap<String, String>,
    pub expires_at: Timestamp,
    /// When it was exchanged: a second exchange revokes the connection it made.
    pub used_at: Option<Timestamp>,
    /// The connection its exchange made or renewed.
    pub connection: Option<String>,
}

/// A person's connection: their key for one client, outside every organization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// `cn_…`.
    pub id: String,
    /// The person.
    pub user: UserId,
    pub client_id: String,
    /// The client's name when the connection was made: shown after the client is forgotten.
    pub client_name: String,
    pub created_at: Timestamp,
    /// When an access token of it was last used, to the minute.
    pub last_used_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
    /// When its latest refresh token lapses: the nodes delete it 30 days later.
    pub expires_at: Timestamp,
}

impl Connection {
    /// The principal of its grants: `connection:cn_…`.
    pub fn principal(&self) -> Principal {
        Principal::Connection(self.id.clone())
    }
}

/// An access token or a refresh token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConnectionTokenKind {
    Access,
    Refresh,
}

impl ConnectionTokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectionTokenKind::Access => "access",
            ConnectionTokenKind::Refresh => "refresh",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "access" => Some(ConnectionTokenKind::Access),
            "refresh" => Some(ConnectionTokenKind::Refresh),
            _ => None,
        }
    }
}

/// A token of a connection, as the store keeps it: hashed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionToken {
    /// The SHA-256 of the token.
    pub hash: Digest,
    pub connection: String,
    pub kind: ConnectionTokenKind,
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
    /// For a refresh token: when it was exchanged. A second use revokes its connection.
    pub used_at: Option<Timestamp>,
}

/// A token about to be issued: its hash and its expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NewToken {
    pub hash: Digest,
    pub expires_at: Timestamp,
}

/// An organization a connection opens: the grant written there for the connection, given by
/// its person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedOrganization {
    pub tenant: TenantId,
    pub access: Access,
    pub allowed_labels: BTreeMap<String, String>,
}

/// What exchanging a code writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewConnection {
    /// The connection to create. When its person already has a live connection of the same
    /// client, that one is kept instead (its id, creation and last use): one connection per
    /// person and client.
    pub connection: Connection,
    /// The organizations it opens, replacing those the kept connection opened.
    pub organizations: Vec<OpenedOrganization>,
    pub access: NewToken,
    pub refresh: NewToken,
}

/// What [`AccessStore::redeem_oauth_code`](super::AccessStore::redeem_oauth_code) did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeRedemption {
    /// The code is now used; the connection (created or kept) holds its grants and the new
    /// tokens.
    Redeemed(Connection),
    /// No such code.
    Unknown,
    /// The code was exchanged before: nothing was written, and the connection it made is now
    /// revoked.
    Used,
}

/// What [`AccessStore::rotate_refresh_token`](super::AccessStore::rotate_refresh_token) did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rotation {
    /// The refresh token is now used, and its connection holds the new tokens.
    Rotated(Connection),
    /// No such refresh token, or its connection was revoked.
    Unknown,
    /// The refresh token lapsed.
    Expired,
    /// The refresh token was used before: nothing was written, and its connection is now
    /// revoked.
    Used,
}
