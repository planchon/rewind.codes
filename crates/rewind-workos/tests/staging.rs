//! `WorkOs` against WorkOS Staging, when `WORKOS_API_KEY` and `WORKOS_CLIENT_ID` are set; skipped,
//! and says so, otherwise.
//!
//! A code by email through the API, organizations and memberships, a refresh onto an
//! organization, the choice of an organization, invitations with and without an organization
//! (one accepted by a code with its token), the waitlist and signing out. Addresses are `@example.net` only: WorkOS's demo organization
//! holds `example.com`, which it leaves alone. Everything the test creates is removed at the
//! end, even when it fails: users, organizations (with their memberships and invitations) and
//! waitlist entries; an invitation without an organization, which WorkOS cannot delete, is
//! revoked.

// The scenario runs in a task of its own, outside the test function, so that clean-up follows
// a failure.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::TryStreamExt as _;
use rewind_engine::ports::{
    Authentication, Client, IdentityError, IdentityProvider, InvitationId, InvitationState,
    MembershipId, MembershipStatus, OrganizationId, PendingToken, Session, SessionId, UserId,
};
use rewind_workos::WorkOs;
use workos::user_management::{CreateMagicAuthParams, ListUsersParams, ListWaitlistEntriesParams};

/// An id of the right shape that WorkOS never gave.
const UNKNOWN: &str = "01JZZZZZZZZZZZZZZZZZZZZZZZ";

struct Staging {
    identity: WorkOs,
    sdk: workos::Client,
    created: Mutex<Created>,
}

/// What the test created, to remove.
#[derive(Default)]
struct Created {
    users: Vec<String>,
    organizations: Vec<String>,
    invitations: Vec<String>,
    waitlist: Vec<String>,
}

impl Staging {
    fn from_env() -> Option<Self> {
        let client_id = std::env::var("WORKOS_CLIENT_ID")
            .ok()
            .filter(|v| !v.is_empty());
        let api_key = std::env::var("WORKOS_API_KEY")
            .ok()
            .filter(|v| !v.is_empty());
        let (Some(client_id), Some(api_key)) = (client_id, api_key) else {
            eprintln!("skipped: WORKOS_CLIENT_ID and WORKOS_API_KEY are not set");
            return None;
        };
        let identity = WorkOs::new(&client_id, &api_key).expect("configured");
        let sdk = workos::Client::builder()
            .client_id(client_id)
            .api_key(api_key)
            .try_build()
            .expect("an SDK client");
        Some(Self {
            identity,
            sdk,
            created: Mutex::new(Created::default()),
        })
    }

    fn record(&self, add: impl FnOnce(&mut Created)) {
        add(&mut self.created.lock().unwrap_or_else(PoisonError::into_inner));
    }

    /// The WorkOS user of `email`, recorded for removal.
    async fn user_by_email(&self, email: &str) -> Option<UserId> {
        let users = self
            .sdk
            .user_management()
            .list_users(ListUsersParams {
                email: Some(email.to_owned()),
                ..ListUsersParams::default()
            })
            .await
            .expect("users listed");
        let user = users.data.first()?.id.clone();
        self.record(|created| created.users.push(user.clone()));
        Some(UserId::new(user))
    }

    /// A Magic Auth code for `email`, as the SDK's Create Magic Auth answers it.
    async fn magic_code(&self, email: &str) -> String {
        let body = workos::CreateMagicCodeAndReturn {
            email: email.to_owned(),
            invitation_token: None,
            ip_address: None,
            user_agent: None,
            radar_auth_attempt_id: None,
            signals_id: None,
            locale: None,
        };
        let created = self
            .sdk
            .user_management()
            .create_magic_auth(CreateMagicAuthParams::new(body))
            .await
            .expect("a Magic Auth created");
        let code = created.code.expose().to_owned();
        assert!(
            code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit()),
            "Create Magic Auth answers the 6-digit code"
        );
        code
    }

    async fn organization(&self, name: &str) -> OrganizationId {
        let organization = self
            .identity
            .create_organization(name)
            .await
            .expect("an organization");
        self.record(|created| created.organizations.push(organization.id.0.clone()));
        assert_eq!(organization.name, name);
        organization.id
    }

    /// Removes what the test created; the failures, if any.
    async fn clean_up(&self) -> Vec<String> {
        let created =
            std::mem::take(&mut *self.created.lock().unwrap_or_else(PoisonError::into_inner));
        let users = self.sdk.user_management();
        let mut failures = Vec::new();
        for invitation in &created.invitations {
            match users.get_invitation(invitation).await {
                Ok(found) if found.state.as_str() == "pending" => {
                    if let Err(error) = users.revoke_invitation(invitation).await {
                        failures.push(format!("revoke {invitation}: {error}"));
                    }
                }
                Ok(_) => {}
                Err(error) if error.is_not_found() => {}
                Err(error) => failures.push(format!("read {invitation}: {error}")),
            }
        }
        for organization in &created.organizations {
            match self
                .sdk
                .organizations()
                .delete_organization(organization)
                .await
            {
                Ok(()) => {}
                Err(error) if error.is_not_found() => {}
                Err(error) => failures.push(format!("delete {organization}: {error}")),
            }
        }
        for user in &created.users {
            match users.delete_user(user).await {
                Ok(()) => {}
                Err(error) if error.is_not_found() => {}
                Err(error) => failures.push(format!("delete {user}: {error}")),
            }
        }
        if !created.waitlist.is_empty() {
            match users.list_waitlists().await {
                Ok(waitlists) => {
                    for waitlist in waitlists.data {
                        for email in &created.waitlist {
                            let entries: Result<Vec<workos::WaitlistEntry>, _> = users
                                .list_waitlist_entries_auto_paging(
                                    waitlist.id.clone(),
                                    ListWaitlistEntriesParams {
                                        email: Some(email.clone()),
                                        ..ListWaitlistEntriesParams::default()
                                    },
                                )
                                .try_collect()
                                .await;
                            match entries {
                                Ok(entries) => {
                                    for entry in
                                        entries.iter().filter(|entry| entry.email == *email)
                                    {
                                        if let Err(error) =
                                            users.delete_waitlist_entry(&entry.id).await
                                        {
                                            failures.push(format!("delete {}: {error}", entry.id));
                                        }
                                    }
                                }
                                Err(error) => failures.push(format!("list the waitlist: {error}")),
                            }
                        }
                    }
                }
                Err(error) => failures.push(format!("list the waitlists: {error}")),
            }
        }
        failures
    }
}

fn session(authentication: Authentication) -> Session {
    match authentication {
        Authentication::Authenticated(session) => session,
        other => panic!("not signed in: {other:?}"),
    }
}

fn refused(result: Result<impl std::fmt::Debug, IdentityError>) -> String {
    match result {
        Err(IdentityError::Refused { code, .. }) => code,
        other => panic!("not refused: {other:?}"),
    }
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_millis(),
    )
    .expect("in range")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewind_works_with_workos_staging() {
    let Some(staging) = Staging::from_env() else {
        return;
    };
    let staging = Arc::new(staging);
    let outcome = tokio::spawn(scenario(staging.clone())).await;
    let failures = staging.clean_up().await;
    if let Err(error) = outcome {
        std::panic::resume_unwind(error.into_panic());
    }
    assert!(failures.is_empty(), "clean-up failed: {failures:#?}");
}

async fn scenario(staging: Arc<Staging>) {
    let identity = &staging.identity;
    let tag = format!("{:x}{:x}", now_ms(), std::process::id());
    let browser = Client {
        ip_address: Some("203.0.113.7".into()),
        user_agent: Some("rewind-workos staging test".into()),
    };
    let ada_email = format!("rewind-test-{tag}@example.net");

    // A code by email: Magic Auth creates the user at once.
    identity.send_magic_code(&ada_email, None).await.unwrap();
    let ada = staging
        .user_by_email(&ada_email)
        .await
        .expect("Magic Auth creates the user");
    assert_eq!(
        refused(
            identity
                .authenticate_magic_code(&ada_email, "000000", None, &browser)
                .await
        ),
        "invalid_one_time_code"
    );
    let code = staging.magic_code(&ada_email).await;
    let first = session(
        identity
            .authenticate_magic_code(&ada_email, &code, None, &browser)
            .await
            .unwrap(),
    );
    assert_eq!(first.user.id, ada);
    assert_eq!(first.user.email, ada_email);
    assert!(first.user.email_verified, "the code proves the email");
    assert_eq!(first.organization, None);
    assert_eq!(first.claims.user, ada);
    assert_eq!(first.claims.organization, None);
    assert_eq!(
        identity
            .verify_access_token(&first.tokens.access_token)
            .await
            .unwrap(),
        first.claims
    );
    let lifetime = first.claims.expires_at.as_millis().saturating_sub(now_ms());
    assert!(
        (240_000..=310_000).contains(&lifetime),
        "access tokens live 300 s: {lifetime} ms left"
    );

    // Organizations and memberships.
    let acme = staging
        .organization(&format!("rewind-test-acme-{tag}"))
        .await;
    let beta = staging
        .organization(&format!("rewind-test-beta-{tag}"))
        .await;
    let other = staging
        .organization(&format!("rewind-test-other-{tag}"))
        .await;
    let renamed = format!("rewind-test-acme2-{tag}");
    assert_eq!(
        identity
            .rename_organization(&acme, &renamed)
            .await
            .unwrap()
            .name,
        renamed
    );
    assert_eq!(
        identity.organization(&acme).await.unwrap().unwrap().name,
        renamed
    );
    let in_acme = identity.add_member(&acme, &ada).await.unwrap();
    assert_eq!(in_acme.status, MembershipStatus::Active);
    assert_eq!(in_acme.organization.name, renamed);
    assert_eq!(in_acme.user.id, ada);
    let in_beta = identity.add_member(&beta, &ada).await.unwrap();
    let mut of_ada: Vec<OrganizationId> = identity
        .memberships_of_user(&ada)
        .await
        .unwrap()
        .into_iter()
        .map(|membership| membership.organization.id)
        .collect();
    of_ada.sort();
    let mut expected = vec![acme.clone(), beta.clone()];
    expected.sort();
    assert_eq!(of_ada, expected);
    let members: Vec<UserId> = identity
        .members(&acme)
        .await
        .unwrap()
        .into_iter()
        .map(|membership| membership.user.id)
        .collect();
    assert_eq!(members, std::slice::from_ref(&ada));

    // A refresh onto an organization: the token gets `org_id`; refresh tokens do not rotate.
    let switched = session(
        identity
            .refresh(&first.tokens.refresh_token, Some(&acme), &browser)
            .await
            .unwrap(),
    );
    assert_eq!(switched.organization, Some(acme.clone()));
    assert_eq!(switched.claims.organization, Some(acme.clone()));
    assert_eq!(switched.claims.session, first.claims.session);
    assert_eq!(switched.tokens.refresh_token, first.tokens.refresh_token);
    assert_eq!(
        identity
            .verify_access_token(&switched.tokens.access_token)
            .await
            .unwrap()
            .organization,
        Some(acme.clone())
    );
    refused(
        identity
            .refresh(&first.tokens.refresh_token, Some(&other), &browser)
            .await,
    );
    assert_eq!(
        identity
            .refresh("not-a-refresh-token", None, &browser)
            .await,
        Err(IdentityError::InvalidToken)
    );

    // Two organizations: WorkOS asks for one.
    let code = staging.magic_code(&ada_email).await;
    let Authentication::OrganizationSelectionRequired {
        pending,
        user,
        organizations,
    } = identity
        .authenticate_magic_code(&ada_email, &code, None, &browser)
        .await
        .unwrap()
    else {
        panic!("an organization is asked for");
    };
    assert_eq!(user.id, ada);
    let mut offered: Vec<OrganizationId> = organizations
        .into_iter()
        .map(|organization| organization.id)
        .collect();
    offered.sort();
    assert_eq!(offered, expected);
    refused(
        identity
            .select_organization(&pending, &other, &browser)
            .await,
    );
    let chosen = session(
        identity
            .select_organization(&pending, &beta, &browser)
            .await
            .unwrap(),
    );
    assert_eq!(chosen.claims.organization, Some(beta.clone()));
    assert_eq!(
        identity
            .select_organization(&PendingToken::new("not-a-pending-token"), &beta, &browser)
            .await,
        Err(IdentityError::InvalidToken)
    );

    // An invitation to an organization creates its user at once; the invited person becomes a
    // member when it is accepted.
    let bob_email = format!("rewind-test-bob-{tag}@example.net");
    let invitation = identity
        .invite(Some(&acme), &bob_email, Some(&ada))
        .await
        .unwrap();
    staging.record(|created| created.invitations.push(invitation.id.0.clone()));
    assert_eq!(invitation.state, InvitationState::Pending);
    assert_eq!(invitation.organization, Some(acme.clone()));
    assert_eq!(invitation.inviter, Some(ada.clone()));
    assert_eq!(invitation.email, bob_email);
    let week = invitation
        .expires_at
        .saturating_since(invitation.created_at);
    assert!(
        (6 * 86_400..=8 * 86_400).contains(&week.as_secs()),
        "invitations last 7 days: {week:?}"
    );
    let bob = staging
        .user_by_email(&bob_email)
        .await
        .expect("an invitation creates its user");
    assert!(
        !identity
            .members(&acme)
            .await
            .unwrap()
            .iter()
            .any(|membership| membership.user.id == bob),
        "invited, not a member yet"
    );
    assert!(
        identity
            .invitations_of(&acme)
            .await
            .unwrap()
            .iter()
            .any(|of| of.id == invitation.id)
    );
    assert!(
        identity
            .invitations_for(&bob_email)
            .await
            .unwrap()
            .iter()
            .any(|of| of.id == invitation.id)
    );
    assert_eq!(
        identity.invitation(&invitation.id).await.unwrap(),
        Some(invitation.clone())
    );
    assert_eq!(
        identity
            .resend_invitation(&invitation.id)
            .await
            .unwrap()
            .state,
        InvitationState::Pending
    );
    let accepted = identity.accept_invitation(&invitation.id).await.unwrap();
    assert_eq!(accepted.state, InvitationState::Accepted);
    assert_eq!(accepted.accepted_user, Some(bob.clone()));
    assert!(
        identity
            .members(&acme)
            .await
            .unwrap()
            .iter()
            .any(|membership| membership.user.id == bob)
    );
    refused(identity.revoke_invitation(&invitation.id).await);

    // An invitation accepted by signing in with its token and a code, as the dashboard does:
    // sending the code accepts nothing (WorkOS would, were the token sent then); the code does.
    let dora_email = format!("rewind-test-dora-{tag}@example.net");
    let to_dora = identity
        .invite(Some(&acme), &dora_email, Some(&ada))
        .await
        .unwrap();
    staging.record(|created| created.invitations.push(to_dora.id.0.clone()));
    let dora = staging
        .user_by_email(&dora_email)
        .await
        .expect("an invitation creates its user");
    let token = staging
        .sdk
        .user_management()
        .get_invitation(&to_dora.id.0)
        .await
        .expect("the invitation")
        .token
        .expose()
        .to_owned();
    identity
        .send_magic_code(&dora_email, Some(&token))
        .await
        .unwrap();
    assert_eq!(
        identity
            .invitation(&to_dora.id)
            .await
            .unwrap()
            .map(|of| of.state),
        Some(InvitationState::Pending),
        "sending the code accepts nothing"
    );
    let code = staging.magic_code(&dora_email).await;
    let joined = session(
        identity
            .authenticate_magic_code(&dora_email, &code, Some(&token), &browser)
            .await
            .unwrap(),
    );
    assert_eq!(joined.user.id, dora);
    assert_eq!(joined.organization, Some(acme.clone()));
    let accepted = identity.invitation(&to_dora.id).await.unwrap().unwrap();
    assert_eq!(accepted.state, InvitationState::Accepted);
    assert_eq!(accepted.accepted_user, Some(dora.clone()));
    // Spent, its token is refused.
    let code = staging.magic_code(&dora_email).await;
    assert_eq!(
        refused(
            identity
                .authenticate_magic_code(&dora_email, &code, Some(&token), &browser)
                .await
        ),
        "invitation_invalid"
    );

    // An invitation to the deployment, as the operator's approval sends, accepted by a code
    // with its token as the dashboard does: WorkOS refuses the token of an invitation without
    // organization, which the adapter therefore does not send.
    let carol_email = format!("rewind-test-carol-{tag}@example.net");
    let to_deployment = identity.invite(None, &carol_email, None).await.unwrap();
    staging.record(|created| created.invitations.push(to_deployment.id.0.clone()));
    assert_eq!(to_deployment.organization, None);
    assert_eq!(to_deployment.inviter, None);
    let carol = staging
        .user_by_email(&carol_email)
        .await
        .expect("an invitation creates its user");
    let token = staging
        .sdk
        .user_management()
        .get_invitation(&to_deployment.id.0)
        .await
        .expect("the invitation")
        .token
        .expose()
        .to_owned();
    identity
        .send_magic_code(&carol_email, Some(&token))
        .await
        .unwrap();
    let code = staging.magic_code(&carol_email).await;
    let approved = session(
        identity
            .authenticate_magic_code(&carol_email, &code, Some(&token), &browser)
            .await
            .unwrap(),
    );
    assert_eq!(approved.user.id, carol);
    assert_eq!(approved.organization, None);
    let accepted = identity
        .invitation(&to_deployment.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(accepted.state, InvitationState::Accepted);
    // Another one, revoked.
    let erin_email = format!("rewind-test-erin-{tag}@example.net");
    let to_erin = identity.invite(None, &erin_email, None).await.unwrap();
    staging.record(|created| created.invitations.push(to_erin.id.0.clone()));
    staging
        .user_by_email(&erin_email)
        .await
        .expect("an invitation creates its user");
    assert_eq!(
        identity.revoke_invitation(&to_erin.id).await.unwrap().state,
        InvitationState::Revoked
    );

    // Unknown ids.
    assert_eq!(
        identity
            .organization(&OrganizationId::new(format!("org_{UNKNOWN}")))
            .await,
        Ok(None)
    );
    assert_eq!(
        identity.user(&UserId::new(format!("user_{UNKNOWN}"))).await,
        Ok(None)
    );
    assert_eq!(
        identity
            .invitation(&InvitationId::new(format!("invitation_{UNKNOWN}")))
            .await,
        Ok(None)
    );
    assert!(matches!(
        identity
            .remove_member(&MembershipId::new(format!("om_{UNKNOWN}")))
            .await,
        Err(IdentityError::NotFound(_))
    ));

    // The waitlist.
    let dave_email = format!("rewind-test-dave-{tag}@example.net");
    staging.record(|created| created.waitlist.push(dave_email.clone()));
    identity.join_waitlist(&dave_email).await.unwrap();
    // Ada has a user: WorkOS refuses her entry (`user_already_exists`), and the answer is the same.
    identity.join_waitlist(&ada_email).await.unwrap();

    // Signing out: the session's refresh token stops working.
    identity.remove_member(&in_beta.id).await.unwrap();
    identity
        .revoke_session(&first.claims.session)
        .await
        .unwrap();
    assert_eq!(
        identity
            .refresh(&first.tokens.refresh_token, None, &browser)
            .await,
        Err(IdentityError::InvalidToken)
    );
    assert!(matches!(
        identity
            .revoke_session(&SessionId::new(format!("session_{UNKNOWN}")))
            .await,
        Err(IdentityError::NotFound(_))
    ));
    identity.delete_organization(&other).await.unwrap();
}
