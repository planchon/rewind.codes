//! `IdentityProvider` on WorkOS (`archi/07-code-architecture.md`, "The other ports"), through
//! `workos`, its official Rust SDK.
//!
//! - Sign-in through AuthKit's API on Rewind's own pages: GitHub (an authorization URL, then the
//!   code of its redirect) and a 6-digit code sent by email (Magic Auth). Each sign-in passes
//!   the browser's IP address and user agent, and the invitation's token when there is one
//!   (with the code, never when the code is sent: WorkOS would accept the invitation then). The
//!   steps WorkOS asks for (an organization to choose, an email to verify) and a sign-up refused
//!   because sign-ups are closed ([`SIGN_UP_CLOSED`]) are answers, not errors. The module
//!   `errors` keeps every code of WorkOS the adapter reads.
//! - Access tokens are checked locally: RS256 against the environment's JWKS, cached and
//!   fetched again for an unknown key (at most every [`JWKS_REFETCH_INTERVAL`]), the issuer
//!   [`ISSUER`], the deployment's `client_id`, the expiry with [`LEEWAY`]. Their role and
//!   permission claims are never read.
//! - Users, organizations, memberships (created without a role) and invitations (sent without
//!   a role), the waitlist. Lists are in creation order. Memberships are the active ones: the
//!   pending membership WorkOS creates with an invitation is the invitation.
//! - The dashboard's cookies are sealed with the `workos` crate's `seal` ([`CookieSeal`]), so
//!   that the server depends on one adapter of WorkOS.
//!
//! No token, code or key appears in an error or a `Debug` output.

#![forbid(unsafe_code)]

mod convert;
mod errors;
mod seal;
mod tokens;

use std::fmt;
use std::sync::Arc;

use futures_util::TryStreamExt as _;
use rewind_engine::ports::{
    AccessClaims, Authentication, Client, IdentityError, IdentityProvider, Invitation,
    InvitationId, Membership, MembershipId, Organization, OrganizationId, PendingToken, Provider,
    Session, SessionId, Tokens, User, UserId,
};
use workos::SecretString;
use workos::organization_membership::{
    CreateOrganizationMembershipParams, CreateOrganizationMembershipParamsBody,
    ListOrganizationMembershipsParams,
};
use workos::organizations::{CreateOrganizationParams, UpdateOrganizationParams};
use workos::user_management::{
    AuthenticateWithCodeParams, AuthenticateWithEmailVerificationParams,
    AuthenticateWithMagicAuthParams, AuthenticateWithOrganizationSelectionParams,
    AuthenticateWithRefreshTokenParams, CreateMagicAuthParams, CreateWaitlistEntryParams,
    GetAuthorizationUrlParams, ListInvitationsParams, ResendInvitationParams, RevokeSessionParams,
    SendInvitationParams, UserManagementApi,
};

pub use errors::SIGN_UP_CLOSED;
pub use seal::{CookieSeal, SealError};
pub use tokens::{ISSUER, JWKS_REFETCH_INTERVAL, LEEWAY};

use errors::{
    INVALID_PENDING_TOKEN, INVALID_REFRESH_TOKEN, SignIn, failure, refusal, sign_in_failure,
};

/// WorkOS's API, where the deployment's environment lives.
pub const DEFAULT_BASE_URL: &str = workos::DEFAULT_BASE_URL;

/// The waitlist people join: WorkOS's name for the environment's own.
const WAITLIST: &str = "default";

/// The most objects a list asks WorkOS for at once.
const PAGE_SIZE: i64 = 100;

/// A WorkOS environment cannot be configured with these values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("the WorkOS client id is empty")]
    EmptyClientId,
    #[error("the WorkOS API key is empty")]
    EmptyApiKey,
    #[error("the WorkOS API key is not a valid HTTP header value")]
    InvalidApiKey,
    #[error("the WorkOS base URL must start with http:// or https://")]
    InvalidBaseUrl,
    #[error("the cookie password must be 32 bytes in hexadecimal: 64 characters 0-9 and a-f")]
    InvalidCookiePassword,
}

/// `IdentityProvider` on a WorkOS environment. Clones share the client and the key cache.
#[derive(Clone)]
pub struct WorkOs {
    client: workos::Client,
    tokens: Arc<tokens::AccessTokens>,
}

impl fmt::Debug for WorkOs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkOs")
            .field("client_id", &self.client.client_id())
            .field("base_url", &self.client.base_url())
            .finish_non_exhaustive()
    }
}

impl WorkOs {
    /// The environment of `client_id` (`client_…`), called with its secret `api_key`
    /// (`sk_…`).
    pub fn new(client_id: &str, api_key: &str) -> Result<Self, ConfigError> {
        Self::with_base_url(client_id, api_key, DEFAULT_BASE_URL)
    }

    /// The same, with WorkOS's API at `base_url`: a fake WorkOS in tests. The JWKS is read
    /// there too; the tokens' issuer stays [`ISSUER`].
    pub fn with_base_url(
        client_id: &str,
        api_key: &str,
        base_url: &str,
    ) -> Result<Self, ConfigError> {
        if client_id.trim().is_empty() {
            return Err(ConfigError::EmptyClientId);
        }
        if api_key.trim().is_empty() {
            return Err(ConfigError::EmptyApiKey);
        }
        let base_url = base_url.trim_end_matches('/');
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err(ConfigError::InvalidBaseUrl);
        }
        let client = workos::Client::builder()
            .client_id(client_id)
            .api_key(api_key)
            .base_url(base_url)
            .try_build()
            .map_err(|_| ConfigError::InvalidApiKey)?;
        let tokens = Arc::new(tokens::AccessTokens::new(&client));
        Ok(Self { client, tokens })
    }

    fn users(&self) -> UserManagementApi<'_> {
        self.client.user_management()
    }

    /// A session WorkOS opened, with the claims of its access token.
    async fn signed_in(
        &self,
        response: workos::AuthenticateResponse,
    ) -> Result<Authentication, IdentityError> {
        let claims = self.tokens.verify(response.access_token.expose()).await?;
        let organization = response
            .organization_id
            .map(OrganizationId::new)
            .or_else(|| claims.organization.clone());
        Ok(Authentication::Authenticated(Session {
            user: convert::user(&response.user),
            organization,
            tokens: Tokens {
                access_token: response.access_token.into_inner(),
                refresh_token: response.refresh_token.into_inner(),
            },
            claims,
        }))
    }

    /// The end of a sign-in: a session, or what WorkOS answered instead.
    async fn finish(
        &self,
        answer: Result<workos::AuthenticateResponse, workos::Error>,
        sign_in: SignIn<'_>,
    ) -> Result<Authentication, IdentityError> {
        match answer {
            Ok(response) => self.signed_in(response).await,
            Err(error) => sign_in_failure(error, &sign_in),
        }
    }

    async fn memberships(
        &self,
        params: ListOrganizationMembershipsParams,
    ) -> Result<Vec<Membership>, IdentityError> {
        let memberships: Vec<workos::UserOrganizationMembership> = self
            .client
            .organization_membership()
            .list_organization_memberships_auto_paging(params)
            .try_collect()
            .await
            .map_err(|error| failure(error, || "memberships".into(), &[]))?;
        let mut converted = Vec::with_capacity(memberships.len());
        for membership in &memberships {
            converted.push(
                self.membership(MembershipFields {
                    id: &membership.id,
                    user: &membership.user,
                    organization_id: &membership.organization_id,
                    organization_name: membership.organization_name.as_deref(),
                    status: membership.status.as_str(),
                })
                .await?,
            );
        }
        Ok(converted)
    }

    async fn membership(&self, fields: MembershipFields<'_>) -> Result<Membership, IdentityError> {
        let organization = match fields.organization_name {
            Some(name) => Organization {
                id: OrganizationId::new(fields.organization_id),
                name: name.to_owned(),
            },
            None => {
                let id = OrganizationId::new(fields.organization_id);
                self.organization(&id)
                    .await?
                    .ok_or_else(|| IdentityError::NotFound(format!("organization {id}")))?
            }
        };
        Ok(Membership {
            id: MembershipId::new(fields.id),
            user: convert::user(fields.user),
            organization,
            status: convert::membership_status(fields.status)?,
        })
    }

    async fn invitations(
        &self,
        params: ListInvitationsParams,
    ) -> Result<Vec<Invitation>, IdentityError> {
        let invitations: Vec<workos::UserInvite> = self
            .users()
            .list_invitations_auto_paging(params)
            .try_collect()
            .await
            .map_err(|error| failure(error, || "invitations".into(), &[]))?;
        invitations
            .iter()
            .map(|invitation| convert::invitation(convert::invitation_fields!(invitation)))
            .collect()
    }
}

/// The fields of a membership, which the SDK models twice.
struct MembershipFields<'a> {
    id: &'a str,
    user: &'a workos::User,
    organization_id: &'a str,
    organization_name: Option<&'a str>,
    status: &'a str,
}

fn list_memberships() -> ListOrganizationMembershipsParams {
    ListOrganizationMembershipsParams {
        limit: Some(PAGE_SIZE),
        order: Some(workos::PaginationOrder::Asc),
        statuses: Some(vec![
            workos::UserManagementOrganizationMembershipStatuses::Active,
        ]),
        ..ListOrganizationMembershipsParams::default()
    }
}

fn list_invitations() -> ListInvitationsParams {
    ListInvitationsParams {
        limit: Some(PAGE_SIZE),
        order: Some(workos::PaginationOrder::Asc),
        ..ListInvitationsParams::default()
    }
}

fn secret(value: Option<&str>) -> Option<SecretString> {
    value.map(SecretString::from)
}

/// `None` for an id that cannot name an object of WorkOS: nothing to ask it.
fn object_id(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty() && value != "." && value != "..").then_some(value)
}

fn not_found(what: impl fmt::Display) -> IdentityError {
    IdentityError::NotFound(what.to_string())
}

impl IdentityProvider for WorkOs {
    fn authorization_url(
        &self,
        provider: Provider,
        redirect_uri: &str,
        state: &str,
    ) -> Result<String, IdentityError> {
        let provider = match provider {
            Provider::GitHub => workos::UserManagementAuthenticationProvider::GitHubOAuth,
        };
        let mut params = GetAuthorizationUrlParams::new(redirect_uri);
        params.provider = Some(provider);
        params.state = Some(state.to_owned());
        self.users()
            .get_authorization_url(params)
            .map_err(|error| IdentityError::Unavailable(error.to_string()))
    }

    async fn authenticate_code(
        &self,
        code: &str,
        invitation_token: Option<&str>,
        client: &Client,
    ) -> Result<Authentication, IdentityError> {
        let mut params = AuthenticateWithCodeParams::new(code);
        params.invitation_token = secret(invitation_token);
        params.ip_address = client.ip_address.clone();
        params.user_agent = client.user_agent.clone();
        let answer = self.users().authenticate_with_code(params).await;
        let secrets = [code, invitation_token.unwrap_or_default()];
        self.finish(
            answer,
            SignIn {
                email: None,
                invalid_token: &[],
                secrets: &secrets,
            },
        )
        .await
    }

    /// The invitation's token is not sent here, only with the code
    /// ([`Self::authenticate_magic_code`]): WorkOS accepts an invitation as soon as a Magic Auth
    /// is created with its token, before anyone has proved the email is theirs, and then refuses
    /// the authentication that carries it (`invitation_invalid`, measured on Staging).
    async fn send_magic_code(
        &self,
        email: &str,
        _invitation_token: Option<&str>,
    ) -> Result<(), IdentityError> {
        let body = workos::CreateMagicCodeAndReturn {
            email: email.trim().to_owned(),
            invitation_token: None,
            ip_address: None,
            user_agent: None,
            radar_auth_attempt_id: None,
            signals_id: None,
            locale: None,
        };
        // WorkOS answers with the code it emails: it is dropped.
        match self
            .users()
            .create_magic_auth(CreateMagicAuthParams::new(body))
            .await
        {
            Ok(_) => Ok(()),
            // The same answer whether the email has an account or not: the closed sign-up is
            // answered when the code is checked.
            Err(error) if errors::is_sign_up_closed(&error) => Ok(()),
            Err(error) => Err(refusal(error, &[])),
        }
    }

    async fn authenticate_magic_code(
        &self,
        email: &str,
        code: &str,
        invitation_token: Option<&str>,
        client: &Client,
    ) -> Result<Authentication, IdentityError> {
        let mut params = AuthenticateWithMagicAuthParams::new(code, email.trim());
        params.invitation_token = secret(invitation_token);
        params.ip_address = client.ip_address.clone();
        params.user_agent = client.user_agent.clone();
        let answer = self.users().authenticate_with_magic_auth(params).await;
        let secrets = [code, invitation_token.unwrap_or_default()];
        self.finish(
            answer,
            SignIn {
                email: Some(email),
                invalid_token: &[],
                secrets: &secrets,
            },
        )
        .await
    }

    async fn verify_email(
        &self,
        pending: &PendingToken,
        code: &str,
        client: &Client,
    ) -> Result<Authentication, IdentityError> {
        let mut params = AuthenticateWithEmailVerificationParams::new(code, pending.expose());
        params.ip_address = client.ip_address.clone();
        params.user_agent = client.user_agent.clone();
        let answer = self
            .users()
            .authenticate_with_email_verification(params)
            .await;
        let secrets = [code, pending.expose()];
        self.finish(
            answer,
            SignIn {
                email: None,
                invalid_token: INVALID_PENDING_TOKEN,
                secrets: &secrets,
            },
        )
        .await
    }

    async fn select_organization(
        &self,
        pending: &PendingToken,
        organization: &OrganizationId,
        client: &Client,
    ) -> Result<Authentication, IdentityError> {
        let mut params = AuthenticateWithOrganizationSelectionParams::new(
            pending.expose(),
            organization.0.clone(),
        );
        params.ip_address = client.ip_address.clone();
        params.user_agent = client.user_agent.clone();
        let answer = self
            .users()
            .authenticate_with_organization_selection(params)
            .await;
        let secrets = [pending.expose()];
        self.finish(
            answer,
            SignIn {
                email: None,
                invalid_token: INVALID_PENDING_TOKEN,
                secrets: &secrets,
            },
        )
        .await
    }

    async fn refresh(
        &self,
        refresh_token: &str,
        organization: Option<&OrganizationId>,
        client: &Client,
    ) -> Result<Authentication, IdentityError> {
        let mut params = AuthenticateWithRefreshTokenParams::new(refresh_token);
        params.organization_id = organization.map(|organization| organization.0.clone());
        params.ip_address = client.ip_address.clone();
        params.user_agent = client.user_agent.clone();
        let answer = self.users().authenticate_with_refresh_token(params).await;
        let secrets = [refresh_token];
        self.finish(
            answer,
            SignIn {
                email: None,
                invalid_token: INVALID_REFRESH_TOKEN,
                secrets: &secrets,
            },
        )
        .await
    }

    async fn verify_access_token(&self, token: &str) -> Result<AccessClaims, IdentityError> {
        self.tokens.verify(token).await
    }

    async fn revoke_session(&self, session: &SessionId) -> Result<(), IdentityError> {
        let Some(id) = object_id(session.as_str()) else {
            return Err(not_found(format_args!("session {session}")));
        };
        let params = RevokeSessionParams::new(workos::RevokeSession {
            session_id: id.to_owned(),
        });
        self.users()
            .revoke_session(params)
            .await
            .map_err(|error| failure(error, || format!("session {session}"), &[]))
    }

    async fn join_waitlist(&self, email: &str) -> Result<(), IdentityError> {
        let params = CreateWaitlistEntryParams::new(workos::CreateWaitlistEntry {
            email: email.trim().to_owned(),
            additional_fields: None,
            // WorkOS sends it only when the environment's confirmation email is enabled.
            send_confirmation_email: Some(true),
        });
        self.users()
            .create_waitlist_entry(WAITLIST, params)
            .await
            .map(|_| ())
            .map_err(|error| failure(error, || "the waitlist".into(), &[]))
    }

    async fn create_organization(&self, name: &str) -> Result<Organization, IdentityError> {
        #[allow(deprecated)] // `domains`, which is left out.
        let body = workos::OrganizationInput {
            name: name.to_owned(),
            allow_profiles_outside_organization: None,
            domains: None,
            domain_data: None,
            metadata: None,
            external_id: None,
        };
        self.client
            .organizations()
            .create_organization(CreateOrganizationParams::new(body))
            .await
            .map(|organization| convert::organization(&organization))
            .map_err(|error| failure(error, || "organization".into(), &[]))
    }

    async fn rename_organization(
        &self,
        organization: &OrganizationId,
        name: &str,
    ) -> Result<Organization, IdentityError> {
        let Some(id) = object_id(organization.as_str()) else {
            return Err(not_found(format_args!("organization {organization}")));
        };
        #[allow(deprecated)] // `domains`, which is left out.
        let body = workos::UpdateOrganization {
            name: Some(name.to_owned()),
            allow_profiles_outside_organization: None,
            domains: None,
            domain_data: None,
            stripe_customer_id: None,
            metadata: None,
            external_id: None,
        };
        self.client
            .organizations()
            .update_organization(id, UpdateOrganizationParams::new(body))
            .await
            .map(|organization| convert::organization(&organization))
            .map_err(|error| failure(error, || format!("organization {organization}"), &[]))
    }

    async fn delete_organization(
        &self,
        organization: &OrganizationId,
    ) -> Result<(), IdentityError> {
        let Some(id) = object_id(organization.as_str()) else {
            return Err(not_found(format_args!("organization {organization}")));
        };
        self.client
            .organizations()
            .delete_organization(id)
            .await
            .map_err(|error| failure(error, || format!("organization {organization}"), &[]))
    }

    async fn organization(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<Organization>, IdentityError> {
        let Some(id) = object_id(organization.as_str()) else {
            return Ok(None);
        };
        match self.client.organizations().get_organization(id).await {
            Ok(found) => Ok(Some(convert::organization(&found))),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(failure(
                error,
                || format!("organization {organization}"),
                &[],
            )),
        }
    }

    async fn user(&self, user: &UserId) -> Result<Option<User>, IdentityError> {
        let Some(id) = object_id(user.as_str()) else {
            return Ok(None);
        };
        match self.users().get_user(id).await {
            Ok(found) => Ok(Some(convert::user(&found))),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(failure(error, || format!("user {user}"), &[])),
        }
    }

    async fn memberships_of_user(&self, user: &UserId) -> Result<Vec<Membership>, IdentityError> {
        let Some(id) = object_id(user.as_str()) else {
            return Ok(Vec::new());
        };
        self.memberships(ListOrganizationMembershipsParams {
            user_id: Some(id.to_owned()),
            ..list_memberships()
        })
        .await
    }

    async fn members(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<Membership>, IdentityError> {
        let Some(id) = object_id(organization.as_str()) else {
            return Err(not_found(format_args!("organization {organization}")));
        };
        self.memberships(ListOrganizationMembershipsParams {
            organization_id: Some(id.to_owned()),
            ..list_memberships()
        })
        .await
    }

    async fn add_member(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Membership, IdentityError> {
        let (Some(organization_id), Some(user_id)) =
            (object_id(organization.as_str()), object_id(user.as_str()))
        else {
            return Err(not_found(format_args!(
                "user {user} or organization {organization}"
            )));
        };
        let body = CreateOrganizationMembershipParamsBody::new(user_id, organization_id);
        let membership = self
            .client
            .organization_membership()
            .create_organization_membership(CreateOrganizationMembershipParams::new(body))
            .await
            .map_err(|error| {
                failure(
                    error,
                    || format!("user {user} or organization {organization}"),
                    &[],
                )
            })?;
        self.membership(MembershipFields {
            id: &membership.id,
            user: &membership.user,
            organization_id: &membership.organization_id,
            organization_name: membership.organization_name.as_deref(),
            status: membership.status.as_str(),
        })
        .await
    }

    async fn remove_member(&self, membership: &MembershipId) -> Result<(), IdentityError> {
        let Some(id) = object_id(membership.as_str()) else {
            return Err(not_found(format_args!("membership {membership}")));
        };
        self.client
            .organization_membership()
            .delete_organization_membership(id)
            .await
            .map_err(|error| failure(error, || format!("membership {membership}"), &[]))
    }

    async fn invite(
        &self,
        organization: Option<&OrganizationId>,
        email: &str,
        inviter: Option<&UserId>,
    ) -> Result<Invitation, IdentityError> {
        let body = workos::CreateUserInviteOptions {
            email: email.trim().to_owned(),
            organization_id: organization.map(|organization| organization.0.clone()),
            role_slug: None,
            expires_in_days: None,
            inviter_user_id: inviter.map(|inviter| inviter.0.clone()),
            locale: None,
        };
        let invitation = self
            .users()
            .send_invitation(SendInvitationParams::new(body))
            .await
            .map_err(|error| {
                failure(
                    error,
                    || match organization {
                        Some(organization) => format!("organization {organization}"),
                        None => "the invited user".into(),
                    },
                    &[],
                )
            })?;
        convert::invitation(convert::invitation_fields!(&invitation))
    }

    async fn invitations_of(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<Invitation>, IdentityError> {
        let Some(id) = object_id(organization.as_str()) else {
            return Ok(Vec::new());
        };
        self.invitations(ListInvitationsParams {
            organization_id: Some(id.to_owned()),
            ..list_invitations()
        })
        .await
    }

    async fn invitations_for(&self, email: &str) -> Result<Vec<Invitation>, IdentityError> {
        let email = errors::normalize(email);
        if email.is_empty() {
            return Ok(Vec::new());
        }
        self.invitations(ListInvitationsParams {
            email: Some(email),
            ..list_invitations()
        })
        .await
    }

    async fn invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Option<Invitation>, IdentityError> {
        let Some(id) = object_id(invitation.as_str()) else {
            return Ok(None);
        };
        match self.users().get_invitation(id).await {
            Ok(found) => convert::invitation(convert::invitation_fields!(&found)).map(Some),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(failure(error, || format!("invitation {invitation}"), &[])),
        }
    }

    async fn accept_invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Invitation, IdentityError> {
        let Some(id) = object_id(invitation.as_str()) else {
            return Err(not_found(format_args!("invitation {invitation}")));
        };
        let accepted = self
            .users()
            .accept_invitation(id)
            .await
            .map_err(|error| failure(error, || format!("invitation {invitation}"), &[]))?;
        convert::invitation(convert::invitation_fields!(&accepted))
    }

    async fn resend_invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Invitation, IdentityError> {
        let Some(id) = object_id(invitation.as_str()) else {
            return Err(not_found(format_args!("invitation {invitation}")));
        };
        let params = ResendInvitationParams::new(workos::ResendUserInviteOptions { locale: None });
        let resent = self
            .users()
            .resend_invitation(id, params)
            .await
            .map_err(|error| failure(error, || format!("invitation {invitation}"), &[]))?;
        convert::invitation(convert::invitation_fields!(&resent))
    }

    async fn revoke_invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Invitation, IdentityError> {
        let Some(id) = object_id(invitation.as_str()) else {
            return Err(not_found(format_args!("invitation {invitation}")));
        };
        let revoked = self
            .users()
            .revoke_invitation(id)
            .await
            .map_err(|error| failure(error, || format!("invitation {invitation}"), &[]))?;
        convert::invitation(convert::invitation_fields!(&revoked))
    }
}
