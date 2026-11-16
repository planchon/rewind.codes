//! The `IdentityProvider` port: who someone is (`archi/06-storage-and-scale.md`, "Who calls";
//! `archi/07-code-architecture.md`, "The other ports" and "Sign-in (`/auth`)").
//!
//! Sign-in, sessions and access tokens, users, organizations, memberships, invitations and the
//! waitlist. Never what someone may do: memberships and invitations carry no role, and an
//! access token's claims are its user, session, organization and expiry, nothing else. What a
//! principal may do is the [`AccessStore`](super::AccessStore)'s.
//!
//! Tokens are opaque to Rewind but for the access token's claims, read by
//! [`IdentityProvider::verify_access_token`].

use std::fmt;
use std::future::Future;

use rewind_core::Timestamp;

use super::access::Principal;
use super::tenancy::{InvalidTenant, TenantId};

macro_rules! identifier {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

identifier!(
    /// A user of the identity provider (`user_01J…`).
    UserId
);
identifier!(
    /// An organization of the identity provider (`org_01J…`): the id of its tenant.
    OrganizationId
);
identifier!(
    /// A sign-in session: what signing out revokes.
    SessionId
);
identifier!(
    /// An invitation to an organization, or to the deployment (`invitation_01J…`).
    InvitationId
);
identifier!(
    /// A user's membership of an organization (`om_01J…`).
    MembershipId
);

impl OrganizationId {
    /// The organization's tenant, which has the same id.
    pub fn tenant(&self) -> Result<TenantId, InvalidTenant> {
        TenantId::new(self.0.clone())
    }
}

impl From<&UserId> for Principal {
    fn from(user: &UserId) -> Self {
        Principal::User(user.0.clone())
    }
}

impl From<&InvitationId> for Principal {
    fn from(invitation: &InvitationId) -> Self {
        Principal::Invitation(invitation.0.clone())
    }
}

/// A person, as the identity provider knows them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub picture_url: Option<String>,
    pub email_verified: bool,
}

/// An organization: a tenant of Rewind, with the same id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Organization {
    pub id: OrganizationId,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MembershipStatus {
    Active,
    /// Waits for the user to accept an invitation.
    Pending,
    Inactive,
}

/// A user's membership of an organization. It carries no role: the member's grant does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Membership {
    pub id: MembershipId,
    pub user: User,
    pub organization: Organization,
    pub status: MembershipStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InvitationState {
    Pending,
    Accepted,
    Expired,
    Revoked,
}

/// An invitation sent by email. It carries no role: its pending grant does
/// ([`Principal::Invitation`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invitation {
    pub id: InvitationId,
    pub email: String,
    /// `None` for an invitation to the deployment, sent when the operator approves someone on
    /// the waitlist.
    pub organization: Option<OrganizationId>,
    pub state: InvitationState,
    /// The member who invited; `None` when the operator did.
    pub inviter: Option<UserId>,
    /// The user who accepted it, whose email may differ from the invited one (GitHub).
    pub accepted_user: Option<UserId>,
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
}

/// A session's tokens. `Debug` redacts them.
#[derive(Clone, PartialEq, Eq)]
pub struct Tokens {
    /// A JWT the dashboard sends to `/v1` as a Bearer token.
    pub access_token: String,
    /// Kept by the server in the sealed session cookie, never given to the browser.
    pub refresh_token: String,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tokens { access_token: <redacted>, refresh_token: <redacted> }")
    }
}

/// What an access token says, once its signature, issuer, audience and expiry are checked. The
/// token's role and permission claims are never read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessClaims {
    pub user: UserId,
    pub session: SessionId,
    /// The tenant; `None` for a member signed in without an organization, who reaches only the
    /// onboarding endpoints.
    pub organization: Option<OrganizationId>,
    pub expires_at: Timestamp,
}

/// A signed-in user's session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub user: User,
    pub organization: Option<OrganizationId>,
    pub tokens: Tokens,
    pub claims: AccessClaims,
}

/// The token of a sign-in step the provider asks for (an email to verify, an organization to
/// choose), given back with the step's answer. `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingToken(String);

impl PendingToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token, for the sealed cookie that keeps it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PendingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PendingToken(<redacted>)")
    }
}

/// Where a sign-in ends: signed in, or the next step the provider asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Authentication {
    Authenticated(Session),
    /// The user belongs to several organizations and named none.
    OrganizationSelectionRequired {
        pending: PendingToken,
        user: User,
        organizations: Vec<Organization>,
    },
    /// The provider sent a code to the email, to verify it.
    EmailVerificationRequired {
        pending: PendingToken,
        email: String,
    },
    /// Sign-ups are closed and this person has no account nor invitation: they may join the
    /// waitlist.
    SignUpClosed {
        email: Option<String>,
    },
}

/// What became of a code asked for by email.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MagicCode {
    /// Sent: the person checks it with `authenticate_magic_code`.
    Sent,
    /// None sent: sign-ups are closed and the email has no user. The person may join the
    /// waitlist.
    SignUpClosed,
}

/// The browser behind a sign-in, as the provider records it on the session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Client {
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
}

/// The identity providers people sign in with, besides a code sent by email.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    GitHub,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// An access, refresh or pending token that is malformed, expired or revoked.
    #[error("the token is invalid or expired")]
    InvalidToken,
    /// The provider refused the request: a wrong or expired code, an unknown invitation, an
    /// organization the user is not a member of. `message` says why.
    #[error("the identity provider refused the request ({code}): {message}")]
    Refused { code: String, message: String },
    /// No such user, organization, membership, invitation or session.
    #[error("{0} not found")]
    NotFound(String),
    /// The provider cannot be reached, or answered with an error of its own.
    #[error("the identity provider is unavailable: {0}")]
    Unavailable(String),
}

/// Who someone is (`archi/07-code-architecture.md`, "The other ports").
///
/// The sign-in methods each answer an [`Authentication`]: signed in, or the next step. A
/// sign-in with an invitation's token accepts the invitation for the user who signs in, and
/// lets them in even when sign-ups are closed. Gets answer `None` for an unknown id; the other
/// methods answer `NotFound`.
pub trait IdentityProvider: Send + Sync {
    // ---------------------------------------------------------------------------------------
    // Sign-in and sessions

    /// Where to send the browser to sign in with `provider`; the provider then redirects to
    /// `redirect_uri` with a code and `state`.
    fn authorization_url(
        &self,
        provider: Provider,
        redirect_uri: &str,
        state: &str,
    ) -> Result<String, IdentityError>;

    /// Exchanges the code of the provider's redirect.
    fn authenticate_code(
        &self,
        code: &str,
        invitation_token: Option<&str>,
        client: &Client,
    ) -> impl Future<Output = Result<Authentication, IdentityError>> + Send;

    /// Sends a 6-digit code to the email. When sign-ups are closed, an email without a user
    /// gets none: an invitation creates its invitee's user, so only someone neither signed up
    /// nor invited is answered [`MagicCode::SignUpClosed`].
    fn send_magic_code(
        &self,
        email: &str,
        invitation_token: Option<&str>,
    ) -> impl Future<Output = Result<MagicCode, IdentityError>> + Send;

    fn authenticate_magic_code(
        &self,
        email: &str,
        code: &str,
        invitation_token: Option<&str>,
        client: &Client,
    ) -> impl Future<Output = Result<Authentication, IdentityError>> + Send;

    /// Answers `EmailVerificationRequired` with the code the provider sent.
    fn verify_email(
        &self,
        pending: &PendingToken,
        code: &str,
        client: &Client,
    ) -> impl Future<Output = Result<Authentication, IdentityError>> + Send;

    /// Answers `OrganizationSelectionRequired`.
    fn select_organization(
        &self,
        pending: &PendingToken,
        organization: &OrganizationId,
        client: &Client,
    ) -> impl Future<Output = Result<Authentication, IdentityError>> + Send;

    /// A new access token for the session; with `organization`, for that organization (a
    /// switch), which the user must be a member of.
    fn refresh(
        &self,
        refresh_token: &str,
        organization: Option<&OrganizationId>,
        client: &Client,
    ) -> impl Future<Output = Result<Authentication, IdentityError>> + Send;

    /// Checks an access token and reads its claims.
    fn verify_access_token(
        &self,
        token: &str,
    ) -> impl Future<Output = Result<AccessClaims, IdentityError>> + Send;

    /// Signs the session out: its refresh token stops working.
    fn revoke_session(
        &self,
        session: &SessionId,
    ) -> impl Future<Output = Result<(), IdentityError>> + Send;

    fn join_waitlist(&self, email: &str) -> impl Future<Output = Result<(), IdentityError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Organizations and users

    fn create_organization(
        &self,
        name: &str,
    ) -> impl Future<Output = Result<Organization, IdentityError>> + Send;

    fn rename_organization(
        &self,
        organization: &OrganizationId,
        name: &str,
    ) -> impl Future<Output = Result<Organization, IdentityError>> + Send;

    /// Deletes the organization with its memberships and invitations.
    fn delete_organization(
        &self,
        organization: &OrganizationId,
    ) -> impl Future<Output = Result<(), IdentityError>> + Send;

    fn organization(
        &self,
        organization: &OrganizationId,
    ) -> impl Future<Output = Result<Option<Organization>, IdentityError>> + Send;

    fn user(
        &self,
        user: &UserId,
    ) -> impl Future<Output = Result<Option<User>, IdentityError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Memberships

    fn memberships_of_user(
        &self,
        user: &UserId,
    ) -> impl Future<Output = Result<Vec<Membership>, IdentityError>> + Send;

    fn members(
        &self,
        organization: &OrganizationId,
    ) -> impl Future<Output = Result<Vec<Membership>, IdentityError>> + Send;

    /// An active membership of the user in the organization.
    fn add_member(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> impl Future<Output = Result<Membership, IdentityError>> + Send;

    fn remove_member(
        &self,
        membership: &MembershipId,
    ) -> impl Future<Output = Result<(), IdentityError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Invitations

    /// Sends an invitation by email: to an organization, or to the deployment (`None`). The
    /// provider creates the invited user at once.
    fn invite(
        &self,
        organization: Option<&OrganizationId>,
        email: &str,
        inviter: Option<&UserId>,
    ) -> impl Future<Output = Result<Invitation, IdentityError>> + Send;

    /// The organization's invitations, in every state.
    fn invitations_of(
        &self,
        organization: &OrganizationId,
    ) -> impl Future<Output = Result<Vec<Invitation>, IdentityError>> + Send;

    /// The invitations sent to the email, in every state.
    fn invitations_for(
        &self,
        email: &str,
    ) -> impl Future<Output = Result<Vec<Invitation>, IdentityError>> + Send;

    fn invitation(
        &self,
        invitation: &InvitationId,
    ) -> impl Future<Output = Result<Option<Invitation>, IdentityError>> + Send;

    /// Accepts a pending invitation for the user of its email, who becomes a member of its
    /// organization.
    fn accept_invitation(
        &self,
        invitation: &InvitationId,
    ) -> impl Future<Output = Result<Invitation, IdentityError>> + Send;

    /// Sends a pending invitation's email again. Its expiry does not change (WorkOS keeps it);
    /// an expired invitation is refused, and the caller invites the email again.
    fn resend_invitation(
        &self,
        invitation: &InvitationId,
    ) -> impl Future<Output = Result<Invitation, IdentityError>> + Send;

    fn revoke_invitation(
        &self,
        invitation: &InvitationId,
    ) -> impl Future<Output = Result<Invitation, IdentityError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_never_printed() {
        let tokens = Tokens {
            access_token: "eyJhbGciOi.secret".into(),
            refresh_token: "refresh-secret".into(),
        };
        let pending = PendingToken::new("pending-secret");
        let printed = format!("{tokens:?} {pending:?}");
        assert!(!printed.contains("secret"), "{printed}");
        assert_eq!(pending.expose(), "pending-secret");
    }

    #[test]
    fn an_organization_is_a_tenant_and_users_are_principals() {
        let organization = OrganizationId::new("org_01JABC");
        assert_eq!(organization.tenant().unwrap().as_str(), "org_01JABC");
        assert_eq!(
            Principal::from(&UserId::new("user_1")).to_string(),
            "user:user_1"
        );
        assert_eq!(
            Principal::from(&InvitationId::new("invitation_1")).to_string(),
            "invitation:invitation_1"
        );
    }
}
