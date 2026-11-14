//! The organization (`archi/06-storage-and-scale.md`, "Tenancy, identity and access"): who
//! calls, the organizations, their members and invitations.
//!
//! A tenant is an organization of the identity provider, with the same id. The provider holds
//! the users, organizations, memberships and invitations; what a member or an invitation may do
//! is its [`Grant`], kept by Rewind.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::access::{Grant, Permission};
use crate::common::{Labels, Timestamp};
use crate::namespaces::NamespaceName;

/// A person, as the identity provider knows them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct User {
    /// `user_01J…`.
    pub id: String,
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture_url: Option<String>,
}

/// An organization: a tenant, with the same id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Organization {
    /// `org_01J…`, also the tenant's id.
    pub id: String,
    pub name: String,
}

/// The organization of the caller's session, with its settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrganizationSettings {
    pub id: String,
    pub name: String,
    /// The role a member gets at their first session, without an invitation's grant.
    pub default_role: String,
}

/// Who calls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    /// An API key.
    Key {
        /// The key's id (`DELETE /v1/api-keys/{id}`).
        id: String,
    },
    /// A member, signed in on the dashboard.
    User { user: User },
    /// An end user's token: it reaches its namespace only, with its own permissions.
    Token {
        /// `opaque` (`rewind_nt_…`) or `signed` (a JWT the customer signed).
        token: TokenKind,
        /// For an opaque token: its id (`DELETE /v1/tokens/{id}`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// For a signed token: the key that signed it, and its `jti` if it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        jti: Option<String>,
        namespace: NamespaceName,
        #[serde(default, skip_serializing_if = "Labels::is_empty")]
        labels: Labels,
        expires_at: Timestamp,
    },
}

/// The form of an end user's token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Opaque,
    Signed,
}

/// `GET /v1/whoami`: who calls, and what they may do. Over the embedded server's socket, which
/// takes no token: the operator of the tenant `local`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// The tenant the caller acts in. Absent for a member signed in without an organization,
    /// who reaches only the onboarding endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Absent over the embedded server's socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<Principal>,
    /// The caller's grant in the tenant. Absent for a member without one, who gets 403 on the
    /// tenant's data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<Grant>,
    /// What the grant's role allows, in the catalog's order.
    pub permissions: Vec<Permission>,
    /// The deployment's operator: every permission in its tenant, and the keys of every tenant.
    pub operator: bool,
    /// For a member: the organization of the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<OrganizationSettings>,
    /// For a member: every organization they belong to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub organizations: Vec<Organization>,
}

/// `POST /v1/organizations`: a new organization, whose creator is its `owner`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateOrganization {
    pub name: String,
}

/// `PATCH /v1/organization`: what is given changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateOrganization {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A built-in role or a role of the organization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_role: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MembershipStatus {
    Active,
    /// Waits for the user to accept an invitation.
    Pending,
    Inactive,
}

/// A member of the organization, with their grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub user: User,
    pub status: MembershipStatus,
    /// Absent for a member without a grant: one who has not had a session in the organization
    /// yet, or whose grant was removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<Grant>,
}

/// `GET /v1/organization/members`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemberList {
    pub members: Vec<Member>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InvitationState {
    Pending,
    Accepted,
    Expired,
    Revoked,
}

/// An invitation sent by email, with the grant its invitee gets on accepting it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Invitation {
    /// `invitation_01J…`.
    pub id: String,
    pub email: String,
    pub state: InvitationState,
    /// The organization it invites to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<Organization>,
    /// The id of the member who invited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inviter: Option<String>,
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
    /// The pending grant, which becomes the invitee's when they accept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<Grant>,
}

/// `GET /v1/invitations` (sent to the caller) and `GET /v1/organization/invitations`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InvitationList {
    pub invitations: Vec<Invitation>,
}

/// `POST /v1/organization/invitations`: invites an email with a role and allowed labels.
/// Inviting needs every permission of the role given.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateInvitation {
    pub email: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub allowed_labels: Labels,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn whoami_names_the_principal_by_kind() {
        let member = Identity {
            tenant: Some("org_01JABC".into()),
            principal: Some(Principal::User {
                user: User {
                    id: "user_01JABC".into(),
                    email: "ada@example.net".into(),
                    first_name: Some("Ada".into()),
                    last_name: None,
                    picture_url: None,
                },
            }),
            grant: Some(Grant {
                role: "owner".into(),
                allowed_labels: Labels::new(),
                granted_by: None,
                granted_at: None,
            }),
            permissions: vec![Permission::RunsRead],
            operator: false,
            organization: Some(OrganizationSettings {
                id: "org_01JABC".into(),
                name: "Acme".into(),
                default_role: "reader".into(),
            }),
            organizations: vec![Organization {
                id: "org_01JABC".into(),
                name: "Acme".into(),
            }],
        };
        let value = serde_json::to_value(&member).unwrap();
        assert_eq!(value["principal"]["kind"], "user");
        assert_eq!(value["principal"]["user"]["firstName"], "Ada");
        assert_eq!(
            value["grant"],
            json!({ "role": "owner", "allowedLabels": {} })
        );
        assert_eq!(value["organization"]["defaultRole"], "reader");
        assert_eq!(serde_json::from_value::<Identity>(value).unwrap(), member);

        let key: Principal =
            serde_json::from_value(json!({ "kind": "key", "id": "ab12" })).unwrap();
        assert_eq!(key, Principal::Key { id: "ab12".into() });
    }
}
