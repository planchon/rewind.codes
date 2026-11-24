//! A person's connections (`archi/06-storage-and-scale.md`, "Connections"): the key an MCP
//! client, Claude above all, holds for a person, outside every organization, opening the
//! organizations the person ticked, each with an [`Access`]; and the consent that makes one
//! (`archi/07-code-architecture.md`, "MCP and OAuth").
//!
//! A connection's request names the organization it acts in with the header
//! `Rewind-Organization` ([`crate::headers::ORGANIZATION`]). In that organization it uses the
//! permissions of its access that the person's own grant gives at that moment, and sees the
//! runs carrying the labels of both grants. It never administers anything.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::access::Permission;
use crate::common::{Labels, Timestamp};

/// The prefix of every connection's id.
pub const CONNECTION_PREFIX: &str = "cn_";

/// What a connection may do in an organization it opens.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// `runs:read`, `actions:read`, `namespaces:read`, `secrets:read`: follow runs, read
    /// action sets, namespaces and the names of secrets.
    Read,
    /// `read`, and `runs:start`, `runs:control` and `secrets:use`: check, test and start
    /// scripts with the secrets they attach, signal and cancel runs.
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

    /// The permissions this access gives, at most: a connection never holds others.
    pub fn permissions(self) -> &'static [Permission] {
        match self {
            Access::Read => &READ,
            Access::Run => &RUN,
        }
    }
}

const READ: [Permission; 4] = [
    Permission::RunsRead,
    Permission::ActionsRead,
    Permission::NamespacesRead,
    Permission::SecretsRead,
];

const RUN: [Permission; 7] = [
    Permission::RunsRead,
    Permission::RunsStart,
    Permission::RunsControl,
    Permission::ActionsRead,
    Permission::NamespacesRead,
    Permission::SecretsRead,
    Permission::SecretsUse,
];

/// How a client made itself known to Rewind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// Its `client_id` is the `https` URL of its metadata document, which Rewind fetches:
    /// Claude's.
    MetadataDocument,
    /// It registered with `POST /oauth/register`, and its `client_id` is `rc_…`.
    Registered,
}

/// The person of a connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionPerson {
    /// `user_…`.
    pub id: String,
    /// Unknown on a server without the identity provider.
    pub email: Option<String>,
    pub name: Option<String>,
}

/// The client a connection was made through.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionClient {
    /// The URL of its metadata document, or `rc_…`.
    pub id: String,
    pub name: String,
    /// Its site.
    pub uri: Option<String>,
    pub kind: ClientKind,
}

/// An organization a connection opens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionOrganization {
    /// `org_…`, the tenant.
    pub id: String,
    /// Its name; unknown on a server without the identity provider.
    pub name: Option<String>,
    pub access: Access,
    /// The labels of the connection's grant there: what it starts carries them, and it sees
    /// only the runs carrying them.
    pub allowed_labels: Labels,
}

/// A person's connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    /// `cn_…`.
    pub id: String,
    pub person: ConnectionPerson,
    pub client: ConnectionClient,
    /// The organizations it opens, by id. `GET /v1/organization/connections` gives the
    /// caller's organization only.
    pub organizations: Vec<ConnectionOrganization>,
    pub created_at: Timestamp,
    /// When an access token of it was last used, to the minute.
    pub last_used_at: Option<Timestamp>,
}

/// `GET /v1/connections` (the caller's) and `GET /v1/organization/connections` (those that open
/// the organization).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionList {
    pub connections: Vec<Connection>,
}

/// An organization to open, with its access and labels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenOrganization {
    /// `org_…`.
    pub id: String,
    pub access: Access,
    #[serde(default)]
    pub allowed_labels: Labels,
}

/// `PATCH /v1/connections/{id}`: the organizations the connection opens from now on, each where
/// the person holds a grant giving its access. An organization left out is closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateConnection {
    pub organizations: Vec<OpenOrganization>,
}

/// `GET /v1/oauth/consent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConsentQuery {
    /// The sealed authorization request, as `GET /oauth/authorize` put it in the consent
    /// page's URL.
    pub request: String,
}

/// The client asking for a connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConsentClient {
    pub id: String,
    pub name: String,
    pub uri: Option<String>,
    pub logo_uri: Option<String>,
    pub kind: ClientKind,
}

/// One of the person's organizations, as the consent offers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConsentOrganization {
    pub id: String,
    pub name: Option<String>,
    /// The role of the person's grant there; none without a grant.
    pub role: Option<String>,
    /// The accesses the person may give there: those whose permissions their grant holds.
    /// Empty: they cannot open it.
    pub accesses: Vec<Access>,
    /// The organization of the caller's access token: ticked in advance.
    pub current: bool,
}

/// `GET /v1/oauth/consent`: what an authorization request asks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Consent {
    pub client: ConsentClient,
    /// The host the browser returns to with the code.
    pub redirect_host: String,
    /// The client returns to a loopback address: a program on the person's machine, which the
    /// page warns about.
    pub loopback: bool,
    /// The person's organizations.
    pub organizations: Vec<ConsentOrganization>,
    /// The labels proposed for the connection's grants: what it starts carries them.
    pub allowed_labels: Labels,
    /// When the request lapses: the client starts again after.
    pub expires_at: Timestamp,
}

/// The person's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConsentDecision {
    Allow,
    Deny,
}

/// An organization ticked at consent, with its access.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsentChoice {
    pub id: String,
    pub access: Access,
}

/// `POST /v1/oauth/consent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsentAnswer {
    /// The request, as `GET /v1/oauth/consent` read it.
    pub request: String,
    pub decision: ConsentDecision,
    /// For `allow`: at least one organization, each where the person's grant gives its access.
    #[serde(default)]
    pub organizations: Vec<ConsentChoice>,
    /// The labels of the connection's grants.
    #[serde(default)]
    pub allowed_labels: Labels,
}

/// Where the consent page sends the browser: the client's redirect URI, with the code and the
/// state, or with `error=access_denied`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConsentRedirect {
    pub redirect_to: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_connection_names_its_unknowns_null() {
        let connection = Connection {
            id: "cn_1".into(),
            person: ConnectionPerson {
                id: "user_1".into(),
                email: None,
                name: None,
            },
            client: ConnectionClient {
                id: "rc_1".into(),
                name: "Cursor".into(),
                uri: None,
                kind: ClientKind::Registered,
            },
            organizations: vec![ConnectionOrganization {
                id: "org_1".into(),
                name: None,
                access: Access::Run,
                allowed_labels: Labels::new(),
            }],
            created_at: Timestamp::UNIX_EPOCH,
            last_used_at: None,
        };
        let value = serde_json::to_value(&connection).unwrap();
        assert_eq!(
            value,
            json!({
                "id": "cn_1",
                "person": { "id": "user_1", "email": null, "name": null },
                "client": { "id": "rc_1", "name": "Cursor", "uri": null, "kind": "registered" },
                "organizations": [
                    { "id": "org_1", "name": null, "access": "run", "allowedLabels": {} }
                ],
                "createdAt": "1970-01-01T00:00:00Z",
                "lastUsedAt": null
            })
        );
        assert_eq!(
            serde_json::from_value::<Connection>(value).unwrap(),
            connection
        );
        assert_eq!(
            Access::Run.permissions().len(),
            Access::Read.permissions().len() + 3
        );
    }
}
