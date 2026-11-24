//! Access (`archi/06-storage-and-scale.md`, "What the caller may do"): the catalog of
//! permissions, the built-in roles, the roles and grants of a tenant, and API keys.
//!
//! A principal (a member, an API key, an invitation) holds a grant; the grant gives a role and
//! optionally allowed labels; the role is a set of [`Permission`]s. Every endpoint requires one
//! permission of the catalog.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::common::{Labels, Timestamp};

// -------------------------------------------------------------------------------------------
// The catalog

/// A kind of action. Every endpoint requires one; a role is a set of them.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
pub enum Permission {
    #[serde(rename = "runs:read")]
    RunsRead,
    #[serde(rename = "runs:start")]
    RunsStart,
    #[serde(rename = "runs:control")]
    RunsControl,
    #[serde(rename = "runs:quarantine")]
    RunsQuarantine,
    #[serde(rename = "runs:delete")]
    RunsDelete,
    #[serde(rename = "actions:read")]
    ActionsRead,
    #[serde(rename = "actions:write")]
    ActionsWrite,
    #[serde(rename = "scripts:write")]
    ScriptsWrite,
    #[serde(rename = "secrets:read")]
    SecretsRead,
    #[serde(rename = "secrets:write")]
    SecretsWrite,
    #[serde(rename = "secrets:use")]
    SecretsUse,
    #[serde(rename = "calls:work")]
    CallsWork,
    #[serde(rename = "calls:approve")]
    CallsApprove,
    #[serde(rename = "subscriptions:read")]
    SubscriptionsRead,
    #[serde(rename = "subscriptions:write")]
    SubscriptionsWrite,
    #[serde(rename = "members:read")]
    MembersRead,
    #[serde(rename = "members:invite")]
    MembersInvite,
    #[serde(rename = "members:manage")]
    MembersManage,
    #[serde(rename = "roles:read")]
    RolesRead,
    #[serde(rename = "roles:write")]
    RolesWrite,
    #[serde(rename = "api_keys:read")]
    ApiKeysRead,
    #[serde(rename = "api_keys:write")]
    ApiKeysWrite,
    #[serde(rename = "organization:write")]
    OrganizationWrite,
    #[serde(rename = "namespaces:read")]
    NamespacesRead,
    #[serde(rename = "namespaces:write")]
    NamespacesWrite,
    #[serde(rename = "tokens:write")]
    TokensWrite,
}

impl Permission {
    /// The catalog, in the order of `archi/06-storage-and-scale.md`.
    pub const ALL: [Permission; 26] = [
        Permission::RunsRead,
        Permission::RunsStart,
        Permission::RunsControl,
        Permission::RunsQuarantine,
        Permission::RunsDelete,
        Permission::ActionsRead,
        Permission::ActionsWrite,
        Permission::ScriptsWrite,
        Permission::SecretsRead,
        Permission::SecretsWrite,
        Permission::SecretsUse,
        Permission::CallsWork,
        Permission::CallsApprove,
        Permission::SubscriptionsRead,
        Permission::SubscriptionsWrite,
        Permission::MembersRead,
        Permission::MembersInvite,
        Permission::MembersManage,
        Permission::RolesRead,
        Permission::RolesWrite,
        Permission::ApiKeysRead,
        Permission::ApiKeysWrite,
        Permission::OrganizationWrite,
        Permission::NamespacesRead,
        Permission::NamespacesWrite,
        Permission::TokensWrite,
    ];

    /// The permissions an end user's token may hold: those that act within one namespace
    /// (`archi/06-storage-and-scale.md`, "What the caller may do").
    pub const TOKEN: [Permission; 11] = [
        Permission::RunsRead,
        Permission::RunsStart,
        Permission::RunsControl,
        Permission::RunsQuarantine,
        Permission::RunsDelete,
        Permission::ActionsRead,
        Permission::SecretsRead,
        Permission::SecretsWrite,
        Permission::SecretsUse,
        Permission::CallsWork,
        Permission::CallsApprove,
    ];

    /// The permissions of a token minted or signed without `permissions`.
    pub const TOKEN_DEFAULT: [Permission; 4] = [
        Permission::RunsRead,
        Permission::RunsStart,
        Permission::RunsControl,
        Permission::ActionsRead,
    ];

    /// Whether an end user's token may hold this permission.
    pub fn in_token(self) -> bool {
        Self::TOKEN.contains(&self)
    }

    /// As the API and the store write it: `runs:read`.
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::RunsRead => "runs:read",
            Permission::RunsStart => "runs:start",
            Permission::RunsControl => "runs:control",
            Permission::RunsQuarantine => "runs:quarantine",
            Permission::RunsDelete => "runs:delete",
            Permission::ActionsRead => "actions:read",
            Permission::ActionsWrite => "actions:write",
            Permission::ScriptsWrite => "scripts:write",
            Permission::SecretsRead => "secrets:read",
            Permission::SecretsWrite => "secrets:write",
            Permission::SecretsUse => "secrets:use",
            Permission::CallsWork => "calls:work",
            Permission::CallsApprove => "calls:approve",
            Permission::SubscriptionsRead => "subscriptions:read",
            Permission::SubscriptionsWrite => "subscriptions:write",
            Permission::MembersRead => "members:read",
            Permission::MembersInvite => "members:invite",
            Permission::MembersManage => "members:manage",
            Permission::RolesRead => "roles:read",
            Permission::RolesWrite => "roles:write",
            Permission::ApiKeysRead => "api_keys:read",
            Permission::ApiKeysWrite => "api_keys:write",
            Permission::OrganizationWrite => "organization:write",
            Permission::NamespacesRead => "namespaces:read",
            Permission::NamespacesWrite => "namespaces:write",
            Permission::TokensWrite => "tokens:write",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|permission| permission.as_str() == text)
    }

    /// What the permission lets the caller do.
    pub fn description(self) -> &'static str {
        match self {
            Permission::RunsRead => {
                "Read runs, their statistics and failure groups, a run's state, transcript, events, \
                 event stream and history, the workflows, the action statistics and payloads."
            }
            Permission::RunsStart => {
                "Start and fork runs, and check and test workflows (`/v1/check`, `/v1/test`)."
            }
            Permission::RunsControl => "Send signals to runs and cancel them.",
            Permission::RunsQuarantine => "Release a quarantined run, or give it up.",
            Permission::RunsDelete => "Erase a run, its payloads included.",
            Permission::ActionsRead => {
                "List action sets and workflows, read their declarations and search their actions."
            }
            Permission::ActionsWrite => "Register an action set, and save a workflow.",
            Permission::ScriptsWrite => {
                "Give a code a title and a description for the organization, or remove them."
            }
            Permission::SecretsRead => {
                "List the secrets of the organization and of the namespaces, their metadata and \
                 usage, never a value; read the signing keys' ids."
            }
            Permission::SecretsWrite => "Put and delete secrets.",
            Permission::SecretsUse => "Attach an organization's secret to a run or a workflow.",
            Permission::CallsWork => {
                "Hold a call stream of harness calls, and defer, complete or fail them."
            }
            Permission::CallsApprove => "Approve or reject a call that waits for the harness.",
            Permission::SubscriptionsRead => "List the webhooks.",
            Permission::SubscriptionsWrite => "Create and delete webhooks.",
            Permission::MembersRead => "List the members, their grants and the invitations.",
            Permission::MembersInvite => "Invite people, and resend and revoke invitations.",
            Permission::MembersManage => "Change a member's grant, and remove a member.",
            Permission::RolesRead => "List the roles and their permissions.",
            Permission::RolesWrite => "Create, change and delete the organization's own roles.",
            Permission::ApiKeysRead => "List the tenant's API keys.",
            Permission::ApiKeysWrite => "Create and revoke API keys.",
            Permission::OrganizationWrite => "Rename the organization and set its default role.",
            Permission::NamespacesRead => {
                "List the namespaces (the end users), their limits, use, alerts and tokens in use."
            }
            Permission::NamespacesWrite => {
                "Set the default limits of namespaces and a namespace's exceptions, and erase a \
                 namespace."
            }
            Permission::TokensWrite => {
                "Mint and revoke end users' tokens, and register and delete the keys that sign them."
            }
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission {0:?}: GET /v1/permissions lists the catalog")]
pub struct UnknownPermission(pub String);

impl FromStr for Permission {
    type Err = UnknownPermission;

    fn from_str(text: &str) -> Result<Self, UnknownPermission> {
        Self::parse(text).ok_or_else(|| UnknownPermission(text.to_owned()))
    }
}

// -------------------------------------------------------------------------------------------
// Roles

/// The four roles every organization has. They never change, and are not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuiltInRole {
    /// Every permission: the creator of an organization.
    Owner,
    /// Everything about runs, action sets, calls and subscriptions, and reading the members and
    /// roles: a harness and its agent tools.
    Harness,
    /// Only the harness calls: a harness's SDK worker.
    Worker,
    /// Only reading.
    Reader,
}

/// The role of the deployment's operator: every permission in its own tenant, the keys of every
/// tenant, and the stop of the embedded server. It is not a role of any organization.
pub const OPERATOR: &str = "operator";

/// The role a member gets at their first session in an organization, unless the organization
/// set another.
pub const DEFAULT_ROLE: BuiltInRole = BuiltInRole::Reader;

const HARNESS: [Permission; 20] = [
    Permission::RunsRead,
    Permission::RunsStart,
    Permission::RunsControl,
    Permission::RunsQuarantine,
    Permission::RunsDelete,
    Permission::ActionsRead,
    Permission::ActionsWrite,
    Permission::ScriptsWrite,
    Permission::SecretsRead,
    Permission::SecretsWrite,
    Permission::SecretsUse,
    Permission::CallsWork,
    Permission::CallsApprove,
    Permission::SubscriptionsRead,
    Permission::SubscriptionsWrite,
    Permission::MembersRead,
    Permission::RolesRead,
    Permission::NamespacesRead,
    Permission::NamespacesWrite,
    Permission::TokensWrite,
];

const WORKER: [Permission; 1] = [Permission::CallsWork];

const READER: [Permission; 7] = [
    Permission::RunsRead,
    Permission::ActionsRead,
    Permission::SecretsRead,
    Permission::SubscriptionsRead,
    Permission::MembersRead,
    Permission::RolesRead,
    Permission::NamespacesRead,
];

impl BuiltInRole {
    pub const ALL: [BuiltInRole; 4] = [
        BuiltInRole::Owner,
        BuiltInRole::Harness,
        BuiltInRole::Worker,
        BuiltInRole::Reader,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BuiltInRole::Owner => "owner",
            BuiltInRole::Harness => "harness",
            BuiltInRole::Worker => "worker",
            BuiltInRole::Reader => "reader",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.as_str() == name)
    }

    pub fn description(self) -> &'static str {
        match self {
            BuiltInRole::Owner => "Every permission: administers the organization.",
            BuiltInRole::Harness => {
                "Runs, action sets, secrets, calls and subscriptions, and reading the members \
                 and roles: a harness and its agent tools."
            }
            BuiltInRole::Worker => "Only the harness calls: a harness's SDK worker.",
            BuiltInRole::Reader => {
                "Reading runs, action sets, subscriptions, members and roles: a dashboard."
            }
        }
    }

    /// The role's permissions, in the catalog's order.
    pub fn permissions(self) -> &'static [Permission] {
        match self {
            BuiltInRole::Owner => &Permission::ALL,
            BuiltInRole::Harness => &HARNESS,
            BuiltInRole::Worker => &WORKER,
            BuiltInRole::Reader => &READER,
        }
    }
}

impl fmt::Display for BuiltInRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The permissions of a role that is not stored: a built-in role's, or every permission for
/// `operator`. `None` for any other name, a role of a tenant.
pub fn fixed_permissions(role: &str) -> Option<&'static [Permission]> {
    if role == OPERATOR {
        return Some(&Permission::ALL);
    }
    BuiltInRole::parse(role).map(BuiltInRole::permissions)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidRoleName {
    #[error("invalid role name {0:?}: expected 1 to 64 lowercase ASCII letters, digits, - and _")]
    Malformed(String),
    #[error(
        "{0:?} is a built-in role or the operator's: an organization's own role needs another name"
    )]
    Reserved(String),
}

/// Checks the name of an organization's own role: 1 to 64 lowercase ASCII letters, digits, `-`
/// and `_`, and neither a built-in role nor `operator`.
pub fn validate_role_name(name: &str) -> Result<(), InvalidRoleName> {
    let well_formed = (1..=64).contains(&name.len())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        });
    if !well_formed {
        return Err(InvalidRoleName::Malformed(name.to_owned()));
    }
    if name == OPERATOR || BuiltInRole::parse(name).is_some() {
        return Err(InvalidRoleName::Reserved(name.to_owned()));
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// DTOs

/// A permission of the catalog, with what it lets the caller do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionInfo {
    pub permission: Permission,
    pub description: String,
}

/// `GET /v1/permissions`: the catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionList {
    pub permissions: Vec<PermissionInfo>,
}

impl PermissionList {
    /// The whole catalog, with its descriptions.
    pub fn catalog() -> Self {
        Self {
            permissions: Permission::ALL
                .into_iter()
                .map(|permission| PermissionInfo {
                    permission,
                    description: permission.description().to_owned(),
                })
                .collect(),
        }
    }
}

/// A role: one of the four built-in roles, or the organization's own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Role {
    pub name: String,
    pub description: String,
    pub permissions: Vec<Permission>,
    /// A built-in role, which never changes.
    pub built_in: bool,
    /// Absent for a built-in role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<Timestamp>,
    /// Absent for a built-in role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<Timestamp>,
}

impl Role {
    /// A built-in role, as `GET /v1/roles` lists it.
    pub fn built_in(role: BuiltInRole) -> Self {
        Self {
            name: role.as_str().to_owned(),
            description: role.description().to_owned(),
            permissions: role.permissions().to_vec(),
            built_in: true,
            created_at: None,
            updated_at: None,
        }
    }
}

/// `GET /v1/roles`: the built-in roles, then the organization's own, by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoleList {
    pub roles: Vec<Role>,
}

/// `PUT /v1/roles/{name}`: creates or changes a role of the organization. Writing a role needs
/// every permission it gives. A change applies at once to every principal holding the role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PutRole {
    #[serde(default)]
    pub description: String,
    pub permissions: Vec<Permission>,
}

/// What a principal holds in a tenant: a role, and optionally allowed labels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    /// A built-in role, an organization's own, or `operator`. A grant whose role no longer
    /// exists gives no permission.
    pub role: String,
    /// The principal only sees the runs carrying all these labels, and cannot administer.
    #[serde(default)]
    pub allowed_labels: Labels,
    /// The principal who gave it: `user:<id>` or `key:<id>`. Absent for the grants the server
    /// gives (an organization's creator, a member's default role, the operator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<String>,
    /// Absent for the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<Timestamp>,
}

/// `PUT /v1/organization/members/{user}/grant`: the member's role and allowed labels. Giving a
/// role needs every permission of it, and of the member's current role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetGrant {
    pub role: String,
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub allowed_labels: Labels,
}

// -------------------------------------------------------------------------------------------
// API keys

/// `POST /v1/api-keys`: a key `rewind_sk_…`, with its grant. Keys are stored hashed: the key
/// itself is only in the answer. Creating a key needs every permission of its role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateApiKey {
    /// Another tenant than the caller's: only the operator names one, and the tenant is
    /// created with its first key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// What the key is for.
    pub name: String,
    /// A built-in role, a role of the tenant, or `operator` (only from the operator).
    pub role: String,
    /// The key only sees runs carrying all these labels: an agent's actions see only their own
    /// conversation's runs.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub allowed_labels: Labels,
}

/// An API key, without its secret but in the answer to its creation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiKey {
    /// For `DELETE /v1/api-keys/{id}`: the hex of the key's SHA-256. Not a secret.
    pub id: String,
    pub tenant: String,
    pub name: String,
    /// The role of the key's grant.
    pub role: String,
    /// The allowed labels of the key's grant.
    pub allowed_labels: Labels,
    /// The principal who created it: `user:<id>` or `key:<id>`. Absent for the operator's key
    /// given at start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<Timestamp>,
    /// The key, `rewind_sk_…`, to send as `Authorization: Bearer rewind_sk_…`. Only in the answer
    /// to `POST /v1/api-keys`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// `GET /v1/api-keys`: the tenant's keys, revoked ones included, oldest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyList {
    pub keys: Vec<ApiKey>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::*;

    #[test]
    fn the_catalog_has_twenty_six_permissions_written_once() {
        let names: BTreeSet<&str> = Permission::ALL.iter().map(|p| p.as_str()).collect();
        assert_eq!(names.len(), 26);
        for permission in Permission::ALL {
            assert_eq!(Permission::parse(permission.as_str()), Some(permission));
            assert_eq!(
                serde_json::to_value(permission).unwrap(),
                json!(permission.as_str())
            );
            assert!(permission.description().ends_with('.'), "{permission}");
        }
        assert_eq!(
            "api_keys:write".parse::<Permission>(),
            Ok(Permission::ApiKeysWrite)
        );
        assert!("runs:everything".parse::<Permission>().is_err());
        assert_eq!(PermissionList::catalog().permissions.len(), 26);
        assert!(
            "credentials:write".parse::<Permission>().is_err(),
            "replaced by the secrets' permissions"
        );
    }

    fn names(permissions: &[Permission]) -> Vec<&'static str> {
        permissions.iter().map(|p| p.as_str()).collect()
    }

    /// `archi/06-storage-and-scale.md`, "What the caller may do".
    #[test]
    fn the_built_in_roles_hold_the_permissions_of_the_table() {
        assert_eq!(BuiltInRole::Owner.permissions(), Permission::ALL);
        assert_eq!(
            names(BuiltInRole::Harness.permissions()),
            [
                "runs:read",
                "runs:start",
                "runs:control",
                "runs:quarantine",
                "runs:delete",
                "actions:read",
                "actions:write",
                "scripts:write",
                "secrets:read",
                "secrets:write",
                "secrets:use",
                "calls:work",
                "calls:approve",
                "subscriptions:read",
                "subscriptions:write",
                "members:read",
                "roles:read",
                "namespaces:read",
                "namespaces:write",
                "tokens:write",
            ]
        );
        assert_eq!(names(BuiltInRole::Worker.permissions()), ["calls:work"]);
        assert_eq!(
            names(BuiltInRole::Reader.permissions()),
            [
                "runs:read",
                "actions:read",
                "secrets:read",
                "subscriptions:read",
                "members:read",
                "roles:read",
                "namespaces:read",
            ]
        );
        assert_eq!(fixed_permissions(OPERATOR), Some(&Permission::ALL[..]));
        assert_eq!(fixed_permissions("reader"), Some(&READER[..]));
        assert_eq!(fixed_permissions("triage"), None);
        assert_eq!(DEFAULT_ROLE.as_str(), "reader");
    }

    /// What the key roles allowed before roles became data: `reader` the reading endpoints of
    /// its tenant, `harness` everything of its tenant but the administration, `worker` the
    /// harness calls, the administration only `admin`, now `operator`. The permissions of
    /// namespaces came after: `harness` manages its end users, `reader` reads them; and so did
    /// `scripts:write`, which `harness` holds with `actions:write`.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Former {
        Read,
        Write,
        Tasks,
        Admin,
    }

    fn former(permission: Permission) -> Former {
        match permission {
            Permission::RunsRead
            | Permission::ActionsRead
            | Permission::SecretsRead
            | Permission::SubscriptionsRead
            | Permission::MembersRead
            | Permission::RolesRead
            | Permission::NamespacesRead => Former::Read,
            Permission::RunsStart
            | Permission::RunsControl
            | Permission::RunsQuarantine
            | Permission::RunsDelete
            | Permission::ActionsWrite
            | Permission::ScriptsWrite
            | Permission::SecretsWrite
            | Permission::SecretsUse
            | Permission::CallsApprove
            | Permission::SubscriptionsWrite
            | Permission::NamespacesWrite
            | Permission::TokensWrite => Former::Write,
            Permission::CallsWork => Former::Tasks,
            Permission::MembersInvite
            | Permission::MembersManage
            | Permission::RolesWrite
            | Permission::ApiKeysRead
            | Permission::ApiKeysWrite
            | Permission::OrganizationWrite => Former::Admin,
        }
    }

    #[test]
    fn harness_worker_and_reader_allow_what_the_key_roles_allowed() {
        let allowed = |role: BuiltInRole, access: Former| match role {
            BuiltInRole::Harness => matches!(access, Former::Read | Former::Write | Former::Tasks),
            BuiltInRole::Worker => access == Former::Tasks,
            BuiltInRole::Reader => access == Former::Read,
            BuiltInRole::Owner => true,
        };
        for role in [
            BuiltInRole::Harness,
            BuiltInRole::Worker,
            BuiltInRole::Reader,
        ] {
            for permission in Permission::ALL {
                assert_eq!(
                    role.permissions().contains(&permission),
                    allowed(role, former(permission)),
                    "{role} and {permission}"
                );
            }
        }
    }

    #[test]
    fn a_token_holds_only_permissions_that_act_within_a_namespace() {
        assert_eq!(
            names(&Permission::TOKEN),
            [
                "runs:read",
                "runs:start",
                "runs:control",
                "runs:quarantine",
                "runs:delete",
                "actions:read",
                "secrets:read",
                "secrets:write",
                "secrets:use",
                "calls:work",
                "calls:approve",
            ]
        );
        assert!(Permission::TOKEN_DEFAULT.iter().all(|p| p.in_token()));
        for never in [
            Permission::ActionsWrite,
            Permission::ScriptsWrite,
            Permission::SubscriptionsWrite,
            Permission::NamespacesRead,
            Permission::TokensWrite,
            Permission::MembersInvite,
        ] {
            assert!(!never.in_token(), "{never}");
        }
    }

    #[test]
    fn role_names_are_short_lowercase_and_not_reserved() {
        for valid in ["triage", "on-call_2", "a", &"x".repeat(64)] {
            assert_eq!(validate_role_name(valid), Ok(()), "{valid}");
        }
        for malformed in ["", "Triage", "on call", "é", "a:b", &"x".repeat(65)] {
            assert_eq!(
                validate_role_name(malformed),
                Err(InvalidRoleName::Malformed(malformed.to_owned()))
            );
        }
        for reserved in ["owner", "harness", "worker", "reader", "operator"] {
            assert_eq!(
                validate_role_name(reserved),
                Err(InvalidRoleName::Reserved(reserved.to_owned()))
            );
        }
    }

    #[test]
    fn a_key_is_written_in_camel_case() {
        let created: CreateApiKey = serde_json::from_value(json!({
            "name": "triage agent",
            "role": "harness",
            "allowedLabels": { "conversation": "c-1" }
        }))
        .unwrap();
        assert_eq!(created.tenant, None);
        assert_eq!(created.allowed_labels.len(), 1);
        assert!(
            serde_json::from_value::<CreateApiKey>(json!({ "role": "harness" })).is_err(),
            "a key has a name"
        );
        let built_in = serde_json::to_value(Role::built_in(BuiltInRole::Worker)).unwrap();
        assert_eq!(
            built_in,
            json!({
                "name": "worker",
                "description": BuiltInRole::Worker.description(),
                "permissions": ["calls:work"],
                "builtIn": true
            })
        );
    }
}
