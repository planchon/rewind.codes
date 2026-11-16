use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rewind_core::Timestamp;

use super::random_token;
use crate::ports::{
    AccessClaims, Authentication, Client, Clock, IdentityError, IdentityProvider, Invitation,
    InvitationId, InvitationState, MagicCode, Membership, MembershipId, MembershipStatus,
    Organization, OrganizationId, PendingToken, Provider, Session, SessionId, Tokens, User, UserId,
};

/// How long an access token lives: WorkOS's setting, measured on its staging environment
/// (`archi/08-decisions.md`).
pub const ACCESS_TOKEN_LIFETIME: Duration = Duration::from_secs(300);

/// How long an invitation waits to be accepted.
pub const INVITATION_LIFETIME: Duration = Duration::from_secs(7 * 86_400);

/// `IdentityProvider` in memory: a deterministic fake of WorkOS for the server's tests. Clones
/// share their state, so a test keeps a handle on what the server it built uses.
///
/// - Users, organizations, memberships and invitations; an invitation creates its user at once,
///   as WorkOS does.
/// - Codes sent by email (sign-in, email verification), which [`last_code`](Self::last_code)
///   reads; GitHub's redirect is [`github_code`](Self::github_code).
/// - Sign-ups open by default; [`set_sign_ups_closed`](Self::set_sign_ups_closed) closes them:
///   an unknown person gets `SignUpClosed`, an invited one signs up.
/// - `OrganizationSelectionRequired` when a user with several active memberships names none.
/// - Opaque random tokens. An access token lives [`ACCESS_TOKEN_LIFETIME`] by the clock given
///   and stays valid until then, as a JWT checked locally does; a revoked session refuses its
///   refresh token. Refresh tokens do not rotate, as on WorkOS.
#[derive(Clone)]
pub struct MemoryIdentity {
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<State>>,
}

impl fmt::Debug for MemoryIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryIdentity").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct State {
    sign_ups_closed: bool,
    next_id: u64,
    users: BTreeMap<UserId, User>,
    organizations: BTreeMap<OrganizationId, Organization>,
    memberships: BTreeMap<MembershipId, StoredMembership>,
    invitations: BTreeMap<InvitationId, StoredInvitation>,
    /// The last code sent to each email.
    codes: BTreeMap<String, String>,
    /// GitHub's codes: the email it vouches for, and whether GitHub verified it.
    github: BTreeMap<String, (String, bool)>,
    pending: BTreeMap<String, Step>,
    sessions: BTreeMap<SessionId, StoredSession>,
    refresh_tokens: BTreeMap<String, SessionId>,
    access_tokens: BTreeMap<String, AccessClaims>,
    waitlist: Vec<String>,
}

struct StoredMembership {
    user: UserId,
    organization: OrganizationId,
    status: MembershipStatus,
}

struct StoredInvitation {
    invitation: Invitation,
    token: String,
}

struct StoredSession {
    user: UserId,
    organization: Option<OrganizationId>,
    revoked: bool,
}

/// A sign-in step waiting for the user.
#[derive(Clone)]
enum Step {
    VerifyEmail {
        user: UserId,
        invitation: Option<InvitationId>,
    },
    SelectOrganization {
        user: UserId,
    },
}

fn refused(code: &str, message: impl Into<String>) -> IdentityError {
    IdentityError::Refused {
        code: code.to_owned(),
        message: message.into(),
    }
}

fn not_found(what: impl fmt::Display) -> IdentityError {
    IdentityError::NotFound(what.to_string())
}

fn normalize(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

impl State {
    fn next(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}_{:06}", self.next_id)
    }

    /// A 6-digit code, sent to `email`.
    fn send_code(&mut self, email: &str) {
        self.next_id += 1;
        let code = format!("{:06}", 100_000 + self.next_id % 900_000);
        self.codes.insert(email.to_owned(), code);
    }

    fn take_code(&mut self, email: &str, code: &str) -> Result<(), IdentityError> {
        if self.codes.get(email).is_some_and(|sent| sent == code) {
            self.codes.remove(email);
            Ok(())
        } else {
            Err(refused("invalid_code", "the code is wrong or expired"))
        }
    }

    fn user_by_email(&self, email: &str) -> Option<User> {
        self.users
            .values()
            .find(|user| user.email == email)
            .cloned()
    }

    fn create_user(&mut self, email: &str, email_verified: bool) -> User {
        let user = User {
            id: UserId::new(self.next("user")),
            email: email.to_owned(),
            first_name: None,
            last_name: None,
            picture_url: None,
            email_verified,
        };
        self.users.insert(user.id.clone(), user.clone());
        user
    }

    fn user(&self, id: &UserId) -> Result<User, IdentityError> {
        self.users
            .get(id)
            .cloned()
            .ok_or_else(|| not_found(format!("user {id}")))
    }

    fn verify(&mut self, id: &UserId) {
        if let Some(user) = self.users.get_mut(id) {
            user.email_verified = true;
        }
    }

    fn invitation(&self, id: &InvitationId, now: Timestamp) -> Option<Invitation> {
        let mut invitation = self.invitations.get(id)?.invitation.clone();
        if invitation.state == InvitationState::Pending && invitation.expires_at <= now {
            invitation.state = InvitationState::Expired;
        }
        Some(invitation)
    }

    /// The pending invitation an invitation token designates.
    fn invitation_by_token(
        &self,
        token: &str,
        now: Timestamp,
    ) -> Result<InvitationId, IdentityError> {
        self.invitations
            .iter()
            .find(|(_, stored)| stored.token == token)
            .and_then(|(id, _)| self.invitation(id, now))
            .filter(|invitation| invitation.state == InvitationState::Pending)
            .map(|invitation| invitation.id)
            .ok_or_else(|| {
                refused(
                    "invalid_invitation",
                    "the invitation is unknown or not pending",
                )
            })
    }

    fn accept(
        &mut self,
        id: &InvitationId,
        user: &UserId,
        now: Timestamp,
    ) -> Result<Invitation, IdentityError> {
        let current = self
            .invitation(id, now)
            .ok_or_else(|| not_found(format!("invitation {id}")))?;
        if current.state != InvitationState::Pending {
            return Err(refused(
                "invitation_not_pending",
                format!("the invitation {id} is not pending"),
            ));
        }
        if let Some(organization) = &current.organization {
            self.join(organization, user);
        }
        let stored = self
            .invitations
            .get_mut(id)
            .ok_or_else(|| not_found(format!("invitation {id}")))?;
        stored.invitation.state = InvitationState::Accepted;
        stored.invitation.accepted_user = Some(user.clone());
        Ok(stored.invitation.clone())
    }

    /// An active membership of the user in the organization.
    fn join(&mut self, organization: &OrganizationId, user: &UserId) -> MembershipId {
        let existing = self.memberships.iter_mut().find(|(_, membership)| {
            membership.user == *user && membership.organization == *organization
        });
        if let Some((id, membership)) = existing {
            membership.status = MembershipStatus::Active;
            return id.clone();
        }
        let id = MembershipId::new(self.next("om"));
        self.memberships.insert(
            id.clone(),
            StoredMembership {
                user: user.clone(),
                organization: organization.clone(),
                status: MembershipStatus::Active,
            },
        );
        id
    }

    fn membership(&self, id: &MembershipId) -> Result<Membership, IdentityError> {
        let stored = self
            .memberships
            .get(id)
            .ok_or_else(|| not_found(format!("membership {id}")))?;
        Ok(Membership {
            id: id.clone(),
            user: self.user(&stored.user)?,
            organization: self
                .organizations
                .get(&stored.organization)
                .cloned()
                .ok_or_else(|| not_found(format!("organization {}", stored.organization)))?,
            status: stored.status,
        })
    }

    fn memberships_where(
        &self,
        keep: impl Fn(&StoredMembership) -> bool,
    ) -> Result<Vec<Membership>, IdentityError> {
        self.memberships
            .iter()
            .filter(|(_, membership)| keep(membership))
            .map(|(id, _)| self.membership(id))
            .collect()
    }

    fn active_organizations(&self, user: &UserId) -> Vec<Organization> {
        self.memberships
            .values()
            .filter(|membership| {
                membership.user == *user && membership.status == MembershipStatus::Active
            })
            .filter_map(|membership| self.organizations.get(&membership.organization).cloned())
            .collect()
    }

    fn is_member(&self, user: &UserId, organization: &OrganizationId) -> bool {
        self.active_organizations(user)
            .iter()
            .any(|of| of.id == *organization)
    }

    /// The end of every sign-in: the email verified, the invitation accepted, the organization
    /// chosen, then a session.
    fn sign_in(
        &mut self,
        user: &UserId,
        invitation: Option<InvitationId>,
        organization: Option<OrganizationId>,
        now: Timestamp,
    ) -> Result<Authentication, IdentityError> {
        let found = self.user(user)?;
        if !found.email_verified {
            self.send_code(&found.email);
            let pending = random_token("pending_");
            self.pending.insert(
                pending.clone(),
                Step::VerifyEmail {
                    user: user.clone(),
                    invitation,
                },
            );
            return Ok(Authentication::EmailVerificationRequired {
                pending: PendingToken::new(pending),
                email: found.email,
            });
        }
        let mut organization = organization;
        if let Some(invitation) = invitation {
            let accepted = self.accept(&invitation, user, now)?;
            organization = organization.or(accepted.organization);
        }
        let organization = match organization {
            Some(organization) if !self.is_member(user, &organization) => {
                return Err(refused(
                    "not_a_member",
                    format!("the user is not a member of {organization}"),
                ));
            }
            Some(organization) => Some(organization),
            None => {
                let organizations = self.active_organizations(user);
                match organizations.as_slice() {
                    [] => None,
                    [only] => Some(only.id.clone()),
                    _ => {
                        let pending = random_token("pending_");
                        self.pending.insert(
                            pending.clone(),
                            Step::SelectOrganization { user: user.clone() },
                        );
                        return Ok(Authentication::OrganizationSelectionRequired {
                            pending: PendingToken::new(pending),
                            user: found,
                            organizations,
                        });
                    }
                }
            }
        };
        let session = SessionId::new(self.next("session"));
        let refresh_token = random_token("refresh_");
        self.sessions.insert(
            session.clone(),
            StoredSession {
                user: user.clone(),
                organization: organization.clone(),
                revoked: false,
            },
        );
        self.refresh_tokens
            .insert(refresh_token.clone(), session.clone());
        Ok(self.session(found, session, refresh_token, organization, now))
    }

    /// A new access token for a session.
    fn session(
        &mut self,
        user: User,
        session: SessionId,
        refresh_token: String,
        organization: Option<OrganizationId>,
        now: Timestamp,
    ) -> Authentication {
        let access_token = random_token("access_");
        let claims = AccessClaims {
            user: user.id.clone(),
            session,
            organization: organization.clone(),
            expires_at: now.saturating_add(ACCESS_TOKEN_LIFETIME),
        };
        self.access_tokens
            .insert(access_token.clone(), claims.clone());
        Authentication::Authenticated(Session {
            user,
            organization,
            tokens: Tokens {
                access_token,
                refresh_token,
            },
            claims,
        })
    }

    /// The user signing in by email or GitHub: known, invited, or new when sign-ups are open.
    fn signing_up(
        &mut self,
        email: &str,
        invitation: Option<&InvitationId>,
        email_verified: bool,
    ) -> Option<User> {
        if let Some(user) = self.user_by_email(email) {
            return Some(user);
        }
        if self.sign_ups_closed && invitation.is_none() {
            return None;
        }
        Some(self.create_user(email, email_verified))
    }
}

impl MemoryIdentity {
    /// A fake provider whose tokens expire by `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Closes or opens sign-ups. Open by default.
    pub fn set_sign_ups_closed(&self, closed: bool) {
        self.state().sign_ups_closed = closed;
    }

    /// A user, as if they had signed up before.
    pub fn add_user(&self, email: &str, email_verified: bool) -> User {
        self.state().create_user(&normalize(email), email_verified)
    }

    /// The last code sent to `email` and not used yet: to sign in, or to verify the email.
    pub fn last_code(&self, email: &str) -> Option<String> {
        self.state().codes.get(&normalize(email)).cloned()
    }

    /// The code GitHub's redirect carries when someone signs in with GitHub as `email`, which
    /// GitHub has verified or not. It is exchanged once.
    pub fn github_code(&self, email: &str, verified: bool) -> String {
        let code = random_token("github_");
        self.state()
            .github
            .insert(code.clone(), (normalize(email), verified));
        code
    }

    /// The token of an invitation's email, which signs the invited person in.
    pub fn invitation_token(&self, invitation: &InvitationId) -> Option<String> {
        self.state()
            .invitations
            .get(invitation)
            .map(|stored| stored.token.clone())
    }

    /// The emails on the waitlist, in the order they joined.
    pub fn waitlist(&self) -> Vec<String> {
        self.state().waitlist.clone()
    }

    /// For the test helpers: a poisoned lock still holds the state.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, IdentityError> {
        self.state.lock().map_err(|_| {
            IdentityError::Unavailable("the memory identity provider's lock is poisoned".into())
        })
    }
}

/// Percent-encodes a query value: everything but RFC 3986's unreserved characters.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

impl IdentityProvider for MemoryIdentity {
    fn authorization_url(
        &self,
        provider: Provider,
        redirect_uri: &str,
        state: &str,
    ) -> Result<String, IdentityError> {
        let provider = match provider {
            Provider::GitHub => "GitHubOAuth",
        };
        Ok(format!(
            "https://identity.invalid/authorize?provider={provider}&redirect_uri={}&state={}",
            encode(redirect_uri),
            encode(state)
        ))
    }

    async fn authenticate_code(
        &self,
        code: &str,
        invitation_token: Option<&str>,
        _client: &Client,
    ) -> Result<Authentication, IdentityError> {
        let now = self.clock.now();
        let mut state = self.lock()?;
        let (email, verified) = state
            .github
            .remove(code)
            .ok_or_else(|| refused("invalid_code", "the code is unknown or used"))?;
        let invitation = invitation_token
            .map(|token| state.invitation_by_token(token, now))
            .transpose()?;
        let Some(user) = state.signing_up(&email, invitation.as_ref(), verified) else {
            return Ok(Authentication::SignUpClosed { email: Some(email) });
        };
        if verified {
            state.verify(&user.id);
        }
        state.sign_in(&user.id, invitation, None, now)
    }

    /// As WorkOS: with sign-ups closed, no code for an email without a user (an invitation
    /// created its invitee's).
    async fn send_magic_code(
        &self,
        email: &str,
        _invitation_token: Option<&str>,
    ) -> Result<MagicCode, IdentityError> {
        let email = normalize(email);
        let mut state = self.lock()?;
        if state.sign_ups_closed && state.user_by_email(&email).is_none() {
            return Ok(MagicCode::SignUpClosed);
        }
        state.send_code(&email);
        Ok(MagicCode::Sent)
    }

    async fn authenticate_magic_code(
        &self,
        email: &str,
        code: &str,
        invitation_token: Option<&str>,
        _client: &Client,
    ) -> Result<Authentication, IdentityError> {
        let now = self.clock.now();
        let email = normalize(email);
        let mut state = self.lock()?;
        state.take_code(&email, code)?;
        let invitation = invitation_token
            .map(|token| state.invitation_by_token(token, now))
            .transpose()?;
        let Some(user) = state.signing_up(&email, invitation.as_ref(), true) else {
            return Ok(Authentication::SignUpClosed { email: Some(email) });
        };
        // The code proves the email.
        state.verify(&user.id);
        state.sign_in(&user.id, invitation, None, now)
    }

    async fn verify_email(
        &self,
        pending: &PendingToken,
        code: &str,
        _client: &Client,
    ) -> Result<Authentication, IdentityError> {
        let now = self.clock.now();
        let mut state = self.lock()?;
        let Some(Step::VerifyEmail { user, invitation }) =
            state.pending.get(pending.expose()).cloned()
        else {
            return Err(IdentityError::InvalidToken);
        };
        let email = state.user(&user)?.email;
        state.take_code(&email, code)?;
        state.pending.remove(pending.expose());
        state.verify(&user);
        state.sign_in(&user, invitation, None, now)
    }

    async fn select_organization(
        &self,
        pending: &PendingToken,
        organization: &OrganizationId,
        _client: &Client,
    ) -> Result<Authentication, IdentityError> {
        let now = self.clock.now();
        let mut state = self.lock()?;
        let Some(Step::SelectOrganization { user }) = state.pending.get(pending.expose()).cloned()
        else {
            return Err(IdentityError::InvalidToken);
        };
        let signed_in = state.sign_in(&user, None, Some(organization.clone()), now)?;
        state.pending.remove(pending.expose());
        Ok(signed_in)
    }

    async fn refresh(
        &self,
        refresh_token: &str,
        organization: Option<&OrganizationId>,
        _client: &Client,
    ) -> Result<Authentication, IdentityError> {
        let now = self.clock.now();
        let mut state = self.lock()?;
        let session = state
            .refresh_tokens
            .get(refresh_token)
            .cloned()
            .ok_or(IdentityError::InvalidToken)?;
        let (user, current) = match state.sessions.get(&session) {
            Some(stored) if !stored.revoked => (stored.user.clone(), stored.organization.clone()),
            _ => return Err(IdentityError::InvalidToken),
        };
        let organization = organization.cloned().or(current);
        if let Some(organization) = &organization
            && !state.is_member(&user, organization)
        {
            return Err(refused(
                "not_a_member",
                format!("the user is not a member of {organization}"),
            ));
        }
        if let Some(stored) = state.sessions.get_mut(&session) {
            stored.organization = organization.clone();
        }
        let user = state.user(&user)?;
        Ok(state.session(user, session, refresh_token.to_owned(), organization, now))
    }

    async fn verify_access_token(&self, token: &str) -> Result<AccessClaims, IdentityError> {
        let now = self.clock.now();
        self.lock()?
            .access_tokens
            .get(token)
            .filter(|claims| claims.expires_at > now)
            .cloned()
            .ok_or(IdentityError::InvalidToken)
    }

    async fn revoke_session(&self, session: &SessionId) -> Result<(), IdentityError> {
        let mut state = self.lock()?;
        let stored = state
            .sessions
            .get_mut(session)
            .ok_or_else(|| not_found(format!("session {session}")))?;
        stored.revoked = true;
        Ok(())
    }

    async fn join_waitlist(&self, email: &str) -> Result<(), IdentityError> {
        let email = normalize(email);
        let mut state = self.lock()?;
        if !state.waitlist.contains(&email) {
            state.waitlist.push(email);
        }
        Ok(())
    }

    async fn create_organization(&self, name: &str) -> Result<Organization, IdentityError> {
        let mut state = self.lock()?;
        let organization = Organization {
            id: OrganizationId::new(state.next("org")),
            name: name.to_owned(),
        };
        state
            .organizations
            .insert(organization.id.clone(), organization.clone());
        Ok(organization)
    }

    async fn rename_organization(
        &self,
        organization: &OrganizationId,
        name: &str,
    ) -> Result<Organization, IdentityError> {
        let mut state = self.lock()?;
        let found = state
            .organizations
            .get_mut(organization)
            .ok_or_else(|| not_found(format!("organization {organization}")))?;
        found.name = name.to_owned();
        Ok(found.clone())
    }

    async fn delete_organization(
        &self,
        organization: &OrganizationId,
    ) -> Result<(), IdentityError> {
        let mut state = self.lock()?;
        if state.organizations.remove(organization).is_none() {
            return Err(not_found(format!("organization {organization}")));
        }
        state
            .memberships
            .retain(|_, membership| membership.organization != *organization);
        state
            .invitations
            .retain(|_, stored| stored.invitation.organization.as_ref() != Some(organization));
        Ok(())
    }

    async fn organization(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<Organization>, IdentityError> {
        Ok(self.lock()?.organizations.get(organization).cloned())
    }

    async fn user(&self, user: &UserId) -> Result<Option<User>, IdentityError> {
        Ok(self.lock()?.users.get(user).cloned())
    }

    async fn memberships_of_user(&self, user: &UserId) -> Result<Vec<Membership>, IdentityError> {
        self.lock()?
            .memberships_where(|membership| membership.user == *user)
    }

    async fn members(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<Membership>, IdentityError> {
        self.lock()?
            .memberships_where(|membership| membership.organization == *organization)
    }

    async fn add_member(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Membership, IdentityError> {
        let mut state = self.lock()?;
        state.user(user)?;
        if !state.organizations.contains_key(organization) {
            return Err(not_found(format!("organization {organization}")));
        }
        let id = state.join(organization, user);
        state.membership(&id)
    }

    async fn remove_member(&self, membership: &MembershipId) -> Result<(), IdentityError> {
        self.lock()?
            .memberships
            .remove(membership)
            .map(|_| ())
            .ok_or_else(|| not_found(format!("membership {membership}")))
    }

    async fn invite(
        &self,
        organization: Option<&OrganizationId>,
        email: &str,
        inviter: Option<&UserId>,
    ) -> Result<Invitation, IdentityError> {
        let now = self.clock.now();
        let email = normalize(email);
        let mut state = self.lock()?;
        if let Some(organization) = organization
            && !state.organizations.contains_key(organization)
        {
            return Err(not_found(format!("organization {organization}")));
        }
        let pending = state.invitations.keys().any(|id| {
            state.invitation(id, now).is_some_and(|invitation| {
                invitation.state == InvitationState::Pending
                    && invitation.email == email
                    && invitation.organization.as_ref() == organization
            })
        });
        if pending {
            return Err(refused(
                "invitation_exists",
                format!("{email} already has a pending invitation"),
            ));
        }
        let user = match state.user_by_email(&email) {
            Some(user) => user,
            None => state.create_user(&email, false),
        };
        if let Some(organization) = organization
            && state.is_member(&user.id, organization)
        {
            return Err(refused(
                "already_a_member",
                format!("{email} is already a member of {organization}"),
            ));
        }
        let invitation = Invitation {
            id: InvitationId::new(state.next("invitation")),
            email,
            organization: organization.cloned(),
            state: InvitationState::Pending,
            inviter: inviter.cloned(),
            accepted_user: None,
            expires_at: now.saturating_add(INVITATION_LIFETIME),
            created_at: now,
        };
        state.invitations.insert(
            invitation.id.clone(),
            StoredInvitation {
                invitation: invitation.clone(),
                token: random_token("invitation_token_"),
            },
        );
        Ok(invitation)
    }

    async fn invitations_of(
        &self,
        organization: &OrganizationId,
    ) -> Result<Vec<Invitation>, IdentityError> {
        let now = self.clock.now();
        let state = self.lock()?;
        Ok(state
            .invitations
            .keys()
            .filter_map(|id| state.invitation(id, now))
            .filter(|invitation| invitation.organization.as_ref() == Some(organization))
            .collect())
    }

    async fn invitations_for(&self, email: &str) -> Result<Vec<Invitation>, IdentityError> {
        let now = self.clock.now();
        let email = normalize(email);
        let state = self.lock()?;
        Ok(state
            .invitations
            .keys()
            .filter_map(|id| state.invitation(id, now))
            .filter(|invitation| invitation.email == email)
            .collect())
    }

    async fn invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Option<Invitation>, IdentityError> {
        Ok(self.lock()?.invitation(invitation, self.clock.now()))
    }

    async fn accept_invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Invitation, IdentityError> {
        let now = self.clock.now();
        let mut state = self.lock()?;
        let email = state
            .invitations
            .get(invitation)
            .map(|stored| stored.invitation.email.clone())
            .ok_or_else(|| not_found(format!("invitation {invitation}")))?;
        let user = match state.user_by_email(&email) {
            Some(user) => user,
            None => state.create_user(&email, false),
        };
        state.accept(invitation, &user.id, now)
    }

    async fn resend_invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Invitation, IdentityError> {
        let now = self.clock.now();
        let state = self.lock()?;
        let current = state
            .invitation(invitation, now)
            .ok_or_else(|| not_found(format!("invitation {invitation}")))?;
        if current.state != InvitationState::Pending {
            return Err(refused(
                "invitation_not_pending",
                format!("the invitation {invitation} expired, or was accepted or revoked"),
            ));
        }
        Ok(current)
    }

    async fn revoke_invitation(
        &self,
        invitation: &InvitationId,
    ) -> Result<Invitation, IdentityError> {
        let now = self.clock.now();
        let mut state = self.lock()?;
        let current = state
            .invitation(invitation, now)
            .ok_or_else(|| not_found(format!("invitation {invitation}")))?;
        if current.state != InvitationState::Pending {
            return Err(refused(
                "invitation_not_pending",
                format!("the invitation {invitation} is not pending"),
            ));
        }
        let stored = state
            .invitations
            .get_mut(invitation)
            .ok_or_else(|| not_found(format!("invitation {invitation}")))?;
        stored.invitation.state = InvitationState::Revoked;
        Ok(stored.invitation.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::VirtualClock;
    use crate::ports::VirtualTime;

    const T0: Timestamp = Timestamp(1_767_225_600_000);

    fn provider() -> (MemoryIdentity, Arc<VirtualClock>) {
        let clock = Arc::new(VirtualClock::new(T0));
        (MemoryIdentity::new(clock.clone()), clock)
    }

    fn session(authentication: Authentication) -> Session {
        match authentication {
            Authentication::Authenticated(session) => session,
            other => panic!("not signed in: {other:?}"),
        }
    }

    async fn magic(
        identity: &MemoryIdentity,
        email: &str,
        invitation: Option<&str>,
    ) -> Authentication {
        assert_eq!(
            identity.send_magic_code(email, invitation).await.unwrap(),
            MagicCode::Sent
        );
        let code = identity.last_code(email).unwrap();
        identity
            .authenticate_magic_code(email, &code, invitation, &Client::default())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_code_by_email_signs_in_and_its_token_expires() {
        let (identity, clock) = provider();
        let signed_in = session(magic(&identity, "Ada@Example.net", None).await);
        assert_eq!(signed_in.user.email, "ada@example.net");
        assert_eq!(signed_in.organization, None);
        let claims = identity
            .verify_access_token(&signed_in.tokens.access_token)
            .await
            .unwrap();
        assert_eq!(claims, signed_in.claims);
        assert!(identity.last_code("ada@example.net").is_none(), "used once");
        assert!(matches!(
            identity
                .authenticate_magic_code("ada@example.net", "000000", None, &Client::default())
                .await,
            Err(IdentityError::Refused { .. })
        ));
        clock.advance_to(T0.saturating_add(ACCESS_TOKEN_LIFETIME));
        assert_eq!(
            identity
                .verify_access_token(&signed_in.tokens.access_token)
                .await,
            Err(IdentityError::InvalidToken)
        );
    }

    #[tokio::test]
    async fn closed_sign_ups_let_only_the_invited_in() {
        let (identity, _) = provider();
        identity.set_sign_ups_closed(true);
        // No code for an email without a user.
        assert_eq!(
            identity
                .send_magic_code("Eve@example.net", None)
                .await
                .unwrap(),
            MagicCode::SignUpClosed
        );
        assert_eq!(identity.last_code("eve@example.net"), None);
        // GitHub's sign-up is refused when its code is exchanged.
        let code = identity.github_code("eve@example.net", true);
        assert_eq!(
            identity
                .authenticate_code(&code, None, &Client::default())
                .await
                .unwrap(),
            Authentication::SignUpClosed {
                email: Some("eve@example.net".into())
            }
        );
        identity.join_waitlist("eve@example.net").await.unwrap();
        identity.join_waitlist("EVE@example.net").await.unwrap();
        assert_eq!(identity.waitlist(), ["eve@example.net"]);

        let acme = identity.create_organization("Acme").await.unwrap();
        let invitation = identity
            .invite(Some(&acme.id), "eve@example.net", None)
            .await
            .unwrap();
        let token = identity.invitation_token(&invitation.id).unwrap();
        // The invitation created her user: a code is sent.
        assert_eq!(
            identity
                .send_magic_code("eve@example.net", None)
                .await
                .unwrap(),
            MagicCode::Sent
        );
        // Through GitHub, with another email than the invited one.
        let code = identity.github_code("eve@github.example.net", true);
        let signed_in = session(
            identity
                .authenticate_code(&code, Some(&token), &Client::default())
                .await
                .unwrap(),
        );
        assert_eq!(signed_in.organization, Some(acme.id.clone()));
        let accepted = identity.invitation(&invitation.id).await.unwrap().unwrap();
        assert_eq!(accepted.state, InvitationState::Accepted);
        assert_eq!(accepted.accepted_user, Some(signed_in.user.id.clone()));
        assert_eq!(identity.members(&acme.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn several_organizations_ask_for_one_and_refresh_switches() {
        let (identity, _) = provider();
        let user = identity.add_user("ada@example.net", true);
        let acme = identity.create_organization("Acme").await.unwrap();
        let beta = identity.create_organization("Beta").await.unwrap();
        let other = identity.create_organization("Other").await.unwrap();
        identity.add_member(&acme.id, &user.id).await.unwrap();
        identity.add_member(&beta.id, &user.id).await.unwrap();
        let Authentication::OrganizationSelectionRequired {
            pending,
            organizations,
            ..
        } = magic(&identity, "ada@example.net", None).await
        else {
            panic!("an organization is asked for");
        };
        assert_eq!(organizations, [acme.clone(), beta.clone()]);
        assert!(matches!(
            identity
                .select_organization(&pending, &other.id, &Client::default())
                .await,
            Err(IdentityError::Refused { .. })
        ));
        let signed_in = session(
            identity
                .select_organization(&pending, &beta.id, &Client::default())
                .await
                .unwrap(),
        );
        assert_eq!(signed_in.claims.organization, Some(beta.id.clone()));

        let refresh = &signed_in.tokens.refresh_token;
        let switched = session(
            identity
                .refresh(refresh, Some(&acme.id), &Client::default())
                .await
                .unwrap(),
        );
        assert_eq!(switched.organization, Some(acme.id.clone()));
        assert_eq!(&switched.tokens.refresh_token, refresh);
        assert!(matches!(
            identity
                .refresh(refresh, Some(&other.id), &Client::default())
                .await,
            Err(IdentityError::Refused { .. })
        ));
        identity
            .revoke_session(&switched.claims.session)
            .await
            .unwrap();
        assert_eq!(
            identity.refresh(refresh, None, &Client::default()).await,
            Err(IdentityError::InvalidToken)
        );
    }

    #[tokio::test]
    async fn an_unverified_email_is_verified_with_a_code() {
        let (identity, _) = provider();
        let code = identity.github_code("ada@example.net", false);
        let Authentication::EmailVerificationRequired { pending, email } = identity
            .authenticate_code(&code, None, &Client::default())
            .await
            .unwrap()
        else {
            panic!("the email is to be verified");
        };
        let sent = identity.last_code(&email).unwrap();
        let signed_in = session(
            identity
                .verify_email(&pending, &sent, &Client::default())
                .await
                .unwrap(),
        );
        assert!(signed_in.user.email_verified);
    }

    #[tokio::test]
    async fn invitations_expire_and_are_resent_or_revoked() {
        let (identity, clock) = provider();
        let acme = identity.create_organization("Acme").await.unwrap();
        let invitation = identity
            .invite(Some(&acme.id), "bob@example.net", None)
            .await
            .unwrap();
        assert_eq!(
            identity.invitations_for("Bob@example.net").await.unwrap(),
            std::slice::from_ref(&invitation)
        );
        assert!(
            identity.members(&acme.id).await.unwrap().is_empty(),
            "invited, not a member yet"
        );
        clock.advance_to(invitation.expires_at);
        assert_eq!(
            identity
                .invitation(&invitation.id)
                .await
                .unwrap()
                .unwrap()
                .state,
            InvitationState::Expired
        );
        assert!(identity.accept_invitation(&invitation.id).await.is_err());
        assert!(
            identity.resend_invitation(&invitation.id).await.is_err(),
            "an expired invitation is invited again, not resent"
        );
        let other = identity
            .invite(Some(&acme.id), "carol@example.net", None)
            .await
            .unwrap();
        let resent = identity.resend_invitation(&other.id).await.unwrap();
        assert_eq!(resent, other, "resending keeps the expiry");
        let revoked = identity.revoke_invitation(&other.id).await.unwrap();
        assert_eq!(revoked.state, InvitationState::Revoked);
        assert!(identity.accept_invitation(&other.id).await.is_err());
    }
}
