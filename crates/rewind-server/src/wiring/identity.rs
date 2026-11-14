//! The identity provider: who someone is (`archi/06-storage-and-scale.md`, "Who calls"), for
//! the dashboard's sign-in and members' access tokens. Wired on the api role when the four
//! `WORKOS_*` are given:
//!
//! | Configuration | Adapter |
//! | --- | --- |
//! | `WORKOS_*` | `rewind-workos` (WorkOS) |
//! | the server's tests ([`crate::start_with_identity`]) | the engine's `MemoryIdentity` |
//!
//! Each method is handed to the adapter as it is: the enum only picks it.

use rewind_engine::adapters::MemoryIdentity;
use rewind_engine::ports::{
    AccessClaims, Authentication, Client, IdentityError, IdentityProvider, Invitation,
    InvitationId, Membership, MembershipId, Organization, OrganizationId, PendingToken, Provider,
    SessionId, User, UserId,
};
use rewind_workos::WorkOs;

/// The identity provider of this server.
#[derive(Debug, Clone)]
pub(crate) enum Identity {
    WorkOs(WorkOs),
    Memory(MemoryIdentity),
}

macro_rules! each {
    ($self:ident, $provider:ident => $call:expr) => {
        match $self {
            Identity::WorkOs($provider) => $call,
            Identity::Memory($provider) => $call,
        }
    };
}

impl IdentityProvider for Identity {
    fn authorization_url(
        &self,
        provider: Provider,
        redirect_uri: &str,
        state: &str,
    ) -> Result<String, IdentityError> {
        each!(self, identity => identity.authorization_url(provider, redirect_uri, state))
    }

    async fn authenticate_code(
        &self,
        code: &str,
        invitation_token: Option<&str>,
        client: &Client,
    ) -> Result<Authentication, IdentityError> {
        each!(self, identity => identity.authenticate_code(code, invitation_token, client).await)
    }

    async fn send_magic_code(
        &self,
        email: &str,
        invitation_token: Option<&str>,
    ) -> Result<(), IdentityError> {
        each!(self, identity => identity.send_magic_code(email, invitation_token).await)
    }

    async fn authenticate_magic_code(
        &self,
        email: &str,
        code: &str,
        invitation_token: Option<&str>,
        client: &Client,
    ) -> Result<Authentication, IdentityError> {
        each!(self, identity => {
            identity
                .authenticate_magic_code(email, code, invitation_token, client)
                .await
        })
    }

    async fn verify_email(
        &self,
        pending: &PendingToken,
        code: &str,
        client: &Client,
    ) -> Result<Authentication, IdentityError> {
        each!(self, identity => identity.verify_email(pending, code, client).await)
    }

    async fn select_organization(
        &self,
        pending: &PendingToken,
        organization: &OrganizationId,
        client: &Client,
    ) -> Result<Authentication, IdentityError> {
        each!(self, identity => identity.select_organization(pending, organization, client).await)
    }

    async fn refresh(
        &self,
        refresh_token: &str,
        organization: Option<&OrganizationId>,
        client: &Client,
    ) -> Result<Authentication, IdentityError> {
        each!(self, identity => identity.refresh(refresh_token, organization, client).await)
    }

    async fn verify_access_token(&self, token: &str) -> Result<AccessClaims, IdentityError> {
        each!(self, identity => identity.verify_access_token(token).await)
    }

    async fn revoke_session(&self, session: &SessionId) -> Result<(), IdentityError> {
        each!(self, identity => identity.revoke_session(session).await)
    }

    async fn join_waitlist(&self, email: &str) -> Result<(), IdentityError> {
        each!(self, identity => identity.join_waitlist(email).await)
    }

    async fn create_organization(&self, name: &str) -> Result<Organization, IdentityError> {
        each!(self, identity => identity.create_organization(name).await)
    }

    async fn rename_organization(
        &self,
        organization: &OrganizationId,
        name: &str,
    ) -> Result<Organization, IdentityError> {
        each!(self, identity => identity.rename_organization(organization, name).await)
    }

    async fn delete_organization(
        &self,
        organization: &OrganizationId,
    ) -> Result<(), IdentityError> {
        each!(self, identity => identity.delete_organization(organization).await)
    }

    async fn organization(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<Organization>, IdentityError> {
        each!(self, identity => identity.organization(organization).await)
    }

    async fn user(&self, user: &UserId) -> Result<Option<User>, IdentityError> {
        each!(self, identity => identity.user(user).await)
    }

    async fn memberships_of_user(&self, user: &UserId) -> Result<Vec<Membership>, IdentityError> {
        each!(self, identity => identity.memberships_of_user(user).await)
    }

    async fn members(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<Membership>, IdentityError> {
        each!(self, identity => identity.members(organization).await)
    }

    async fn add_member(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Membership, IdentityError> {
        each!(self, identity => identity.add_member(organization, user).await)
    }

    async fn remove_member(&self, membership: &MembershipId) -> Result<(), IdentityError> {
        each!(self, identity => identity.remove_member(membership).await)
    }

    async fn invite(
        &self,
        organization: Option<&OrganizationId>,
        email: &str,
        inviter: Option<&UserId>,
    ) -> Result<Invitation, IdentityError> {
        each!(self, identity => identity.invite(organization, email, inviter).await)
    }

    async fn invitations_of(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<Invitation>, IdentityError> {
        each!(self, identity => identity.invitations_of(organization).await)
    }

    async fn invitations_for(&self, email: &str) -> Result<Vec<Invitation>, IdentityError> {
        each!(self, identity => identity.invitations_for(email).await)
    }

    async fn invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Option<Invitation>, IdentityError> {
        each!(self, identity => identity.invitation(invitation).await)
    }

    async fn accept_invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Invitation, IdentityError> {
        each!(self, identity => identity.accept_invitation(invitation).await)
    }

    async fn resend_invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Invitation, IdentityError> {
        each!(self, identity => identity.resend_invitation(invitation).await)
    }

    async fn revoke_invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Invitation, IdentityError> {
        each!(self, identity => identity.revoke_invitation(invitation).await)
    }
}
