//! Organizations, members, invitations and roles end to end (`archi/06-storage-and-scale.md`,
//! "What the caller may do"; `archi/07-code-architecture.md`, groups Organization and Access),
//! members signed in over `/auth` with the engine's fake of WorkOS: first grants (an
//! invitation's, the default role), no grant no access, the last owner, roles in use, no one
//! giving more than they hold, grants limited to labels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use rewind_api::access::{Permission, PutRole, SetGrant};
use rewind_api::auth::AuthState;
use rewind_api::common::Labels;
use rewind_api::organization::{
    CreateInvitation, CreateOrganization, InvitationState, UpdateOrganization,
};
use rewind_api::runs::RunFilter;
use rewind_api::{Client, Problem, ProblemType};
use rewind_engine::adapters::MemoryIdentity;
use rewind_engine::ports::{IdentityProvider, InvitationId};
use support::{Browser, SkewedClock, TestServer, identity, token};

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

fn kind<T: std::fmt::Debug>(result: Result<T, rewind_api::Error>) -> ProblemType {
    problem(result.unwrap_err()).kind().unwrap()
}

/// A server with a sign-in, and an organization whose owner signed in.
struct Organization {
    server: TestServer,
    identity: MemoryIdentity,
    #[allow(dead_code)]
    clock: Arc<SkewedClock>,
    id: String,
    owner: Client,
    owner_id: String,
}

/// A person the operator approved: they sign in by code.
async fn signed_in(
    server: &TestServer,
    identity: &MemoryIdentity,
    email: &str,
) -> (Browser, AuthState) {
    identity.add_user(email, true);
    let mut browser = Browser::new(&server.url);
    let state = browser.sign_in(identity, email, None).await.unwrap();
    (browser, state)
}

fn client(server: &TestServer, state: &AuthState) -> Client {
    server.client(Some(&token(state).0))
}

fn user_id(state: &AuthState) -> String {
    match state {
        AuthState::Authenticated { person, .. } => person.id.clone(),
        other => panic!("not signed in: {other:?}"),
    }
}

impl Organization {
    /// Ada signs in and creates Acme, whose owner she is.
    async fn new() -> Self {
        let (identity, clock) = identity();
        identity.set_sign_ups_closed(true);
        let server = TestServer::signed_in(&identity, &[]).await;
        let (mut browser, state) = signed_in(&server, &identity, "ada@example.net").await;
        let acme = client(&server, &state)
            .create_organization(&CreateOrganization {
                name: "Acme".into(),
            })
            .await
            .unwrap();
        let state = browser.switch(&acme.id).await.unwrap();
        Self {
            owner: client(&server, &state),
            owner_id: user_id(&state),
            server,
            identity,
            clock,
            id: acme.id,
        }
    }

    /// Someone the organization added directly (in WorkOS), signed in on it.
    async fn member(&self, email: &str) -> (Client, String) {
        let (client, user, _) = self.member_in(email).await;
        (client, user)
    }

    /// The same, with the member's browser.
    async fn member_in(&self, email: &str) -> (Client, String, Browser) {
        let user = self.identity.add_user(email, true);
        self.identity
            .add_member(
                &rewind_engine::ports::OrganizationId::new(&self.id),
                &user.id,
            )
            .await
            .unwrap();
        let mut browser = Browser::new(&self.server.url);
        let state = browser.sign_in(&self.identity, email, None).await.unwrap();
        assert_eq!(token(&state).1.as_deref(), Some(self.id.as_str()));
        (client(&self.server, &state), user.id.to_string(), browser)
    }

    async fn grant(&self, user: &str, role: &str, labels: &[(&str, &str)]) {
        self.owner
            .set_member_grant(
                user,
                &SetGrant {
                    role: role.into(),
                    allowed_labels: labels.iter().copied().collect::<Labels>(),
                },
            )
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invitee_signs_in_with_the_invitation_and_gets_its_grant() {
    let acme = Organization::new().await;
    let invitation = acme
        .owner
        .invite(&CreateInvitation {
            email: "grace@example.net".into(),
            role: "harness".into(),
            allowed_labels: Labels::new(),
        })
        .await
        .unwrap();
    assert_eq!(invitation.state, InvitationState::Pending);
    assert_eq!(invitation.inviter.as_deref(), Some(acme.owner_id.as_str()));
    assert_eq!(invitation.organization.as_ref().unwrap().name, "Acme");
    let pending = invitation.grant.clone().unwrap();
    assert_eq!(pending.role, "harness");
    assert_eq!(
        pending.granted_by,
        Some(format!("member:{}", acme.owner_id)),
        "the inviter gave it"
    );
    let listed = acme.owner.list_invitations().await.unwrap().invitations;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].grant, Some(pending.clone()));

    // Sign-ups are closed; the invitation's token lets Grace in, on Acme.
    let invitation_token = acme
        .identity
        .invitation_token(&InvitationId::new(&invitation.id))
        .unwrap();
    let mut browser = Browser::new(&acme.server.url);
    let state = browser
        .sign_in(&acme.identity, "grace@example.net", Some(&invitation_token))
        .await
        .unwrap();
    assert_eq!(token(&state).1.as_deref(), Some(acme.id.as_str()));
    let grace = client(&acme.server, &state);
    let me = grace.whoami().await.unwrap();
    assert_eq!(me.grant.as_ref(), Some(&pending), "the invitation's grant");
    assert_eq!(
        me.permissions,
        rewind_api::access::BuiltInRole::Harness.permissions()
    );
    grace.list_runs(&RunFilter::default()).await.unwrap();

    let members = acme.owner.list_members().await.unwrap().members;
    let roles: Vec<(&str, &str)> = members
        .iter()
        .map(|member| {
            (
                member.person.email.as_str(),
                member.grant.as_ref().unwrap().role.as_str(),
            )
        })
        .collect();
    assert_eq!(
        roles,
        [
            ("ada@example.net", "owner"),
            ("grace@example.net", "harness")
        ]
    );
    let listed = acme.owner.list_invitations().await.unwrap().invitations;
    assert_eq!(listed[0].state, InvitationState::Accepted);
    assert_eq!(listed[0].grant, None, "claimed");
    acme.server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invitation_is_accepted_resent_and_revoked_through_the_api() {
    let acme = Organization::new().await;
    let (mut browser, state) = signed_in(&acme.server, &acme.identity, "bob@example.net").await;
    let bob = client(&acme.server, &state);
    let invite = |email: &str, role: &str| CreateInvitation {
        email: email.into(),
        role: role.into(),
        allowed_labels: Labels::new(),
    };
    let to_bob = acme
        .owner
        .invite(&invite("Bob@Example.net", "reader"))
        .await
        .unwrap();
    let to_eve = acme
        .owner
        .invite(&invite("eve@example.net", "harness"))
        .await
        .unwrap();

    let mine = bob.my_invitations().await.unwrap().invitations;
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].id, to_bob.id);
    assert_eq!(mine[0].organization.as_ref().unwrap().name, "Acme");
    assert_eq!(
        kind(bob.accept_invitation(&to_eve.id).await),
        ProblemType::NotFound
    );
    let accepted = bob.accept_invitation(&to_bob.id).await.unwrap();
    assert_eq!(accepted.state, InvitationState::Accepted);
    assert_eq!(accepted.grant.as_ref().unwrap().role, "reader");
    assert_eq!(
        kind(bob.accept_invitation(&to_bob.id).await),
        ProblemType::NotFound
    );
    let state = browser.switch(&acme.id).await.unwrap();
    let me = client(&acme.server, &state).whoami().await.unwrap();
    let grant = me.grant.unwrap();
    assert_eq!(grant.role, "reader");
    assert_eq!(grant.granted_by, Some(format!("member:{}", acme.owner_id)));

    let resent = acme.owner.resend_invitation(&to_eve.id).await.unwrap();
    assert_eq!(resent.grant.unwrap().role, "harness");
    acme.owner.revoke_invitation(&to_eve.id).await.unwrap();
    assert_eq!(
        kind(acme.owner.revoke_invitation(&to_eve.id).await),
        ProblemType::NotFound
    );
    let eve = acme
        .owner
        .list_invitations()
        .await
        .unwrap()
        .invitations
        .into_iter()
        .find(|invitation| invitation.id == to_eve.id)
        .unwrap();
    assert_eq!(eve.state, InvitationState::Revoked);
    assert_eq!(eve.grant, None, "its pending grant is deleted");
    assert_eq!(
        kind(acme.owner.invite(&invite("x@example.net", "nobody")).await),
        ProblemType::NotFound
    );
    assert_eq!(
        kind(
            acme.owner
                .invite(&invite("x@example.net", "operator"))
                .await
        ),
        ProblemType::Forbidden
    );
    acme.server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_added_directly_gets_the_default_role_and_the_organization_changes() {
    let acme = Organization::new().await;
    let settings = acme
        .owner
        .update_organization(&UpdateOrganization {
            name: Some("Acme Corp".into()),
            default_role: Some("harness".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        (settings.name.as_str(), settings.default_role.as_str()),
        ("Acme Corp", "harness")
    );
    for refused in ["operator", "nobody"] {
        let update = UpdateOrganization {
            name: None,
            default_role: Some(refused.into()),
        };
        assert_eq!(
            kind(acme.owner.update_organization(&update).await),
            ProblemType::InvalidRequest,
            "{refused}"
        );
    }
    let (carol, _) = acme.member("carol@example.net").await;
    let me = carol.whoami().await.unwrap();
    let grant = me.grant.unwrap();
    assert_eq!((grant.role.as_str(), grant.granted_by), ("harness", None));
    assert_eq!(me.organization.unwrap().name, "Acme Corp");
    acme.server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_member_cuts_their_access_at_the_next_request() {
    let acme = Organization::new().await;
    let (grace, grace_id, mut browser) = acme.member_in("grace@example.net").await;
    grace.list_runs(&RunFilter::default()).await.unwrap();
    acme.owner.remove_member(&grace_id).await.unwrap();
    let refused = problem(grace.list_runs(&RunFilter::default()).await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::Forbidden));
    assert!(refused.detail.contains("no grant"), "{}", refused.detail);
    let me = grace.whoami().await.unwrap();
    assert_eq!(me.grant, None, "whoami still answers");
    assert!(me.organizations.is_empty());
    // Her session, whose token is still for Acme, gets no grant back: she is no
    // member any more.
    let state = browser.session().await.unwrap();
    assert_eq!(token(&state).1.as_deref(), Some(acme.id.as_str()));
    assert_eq!(grace.whoami().await.unwrap().grant, None);
    let members = acme.owner.list_members().await.unwrap().members;
    assert_eq!(members.len(), 1);
    assert_eq!(
        kind(acme.owner.remove_member(&grace_id).await),
        ProblemType::NotFound
    );
    acme.server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_organization_keeps_an_owner() {
    let acme = Organization::new().await;
    let (_grace, grace_id) = acme.member("grace@example.net").await;
    let demote = SetGrant {
        role: "reader".into(),
        allowed_labels: Labels::new(),
    };
    assert_eq!(
        kind(acme.owner.set_member_grant(&acme.owner_id, &demote).await),
        ProblemType::LastOwner
    );
    assert_eq!(
        kind(acme.owner.remove_member(&acme.owner_id).await),
        ProblemType::LastOwner
    );
    acme.grant(&grace_id, "owner", &[]).await;
    acme.owner
        .set_member_grant(&acme.owner_id, &demote)
        .await
        .unwrap();
    acme.server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_role_in_use_stays() {
    let acme = Organization::new().await;
    let (grace, grace_id) = acme.member("grace@example.net").await;
    let triage = PutRole {
        description: "Reads and controls runs.".into(),
        permissions: vec![Permission::RunsRead, Permission::RunsControl],
    };
    let created = acme.owner.put_role("triage", &triage).await.unwrap();
    assert!(!created.built_in);
    let changed = acme
        .owner
        .put_role(
            "triage",
            &PutRole {
                permissions: vec![Permission::RunsControl, Permission::RunsRead],
                ..triage.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(changed.created_at, created.created_at);
    let names: Vec<String> = acme
        .owner
        .list_roles()
        .await
        .unwrap()
        .roles
        .into_iter()
        .map(|role| role.name)
        .collect();
    assert_eq!(names, ["owner", "harness", "worker", "reader", "triage"]);
    for invalid in ["Triage", "owner", "operator"] {
        assert_eq!(
            kind(acme.owner.put_role(invalid, &triage).await),
            ProblemType::InvalidRequest,
            "{invalid}"
        );
    }

    acme.grant(&grace_id, "triage", &[]).await;
    assert_eq!(
        grace.whoami().await.unwrap().permissions,
        [Permission::RunsRead, Permission::RunsControl]
    );
    assert_eq!(
        kind(acme.owner.delete_role("triage").await),
        ProblemType::RoleInUse
    );
    acme.grant(&grace_id, "reader", &[]).await;
    acme.owner
        .update_organization(&UpdateOrganization {
            name: None,
            default_role: Some("triage".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        kind(acme.owner.delete_role("triage").await),
        ProblemType::RoleInUse,
        "the default role"
    );
    acme.owner
        .update_organization(&UpdateOrganization {
            name: None,
            default_role: Some("reader".into()),
        })
        .await
        .unwrap();
    acme.owner.delete_role("triage").await.unwrap();
    assert_eq!(
        kind(acme.owner.delete_role("triage").await),
        ProblemType::NotFound
    );
    assert_eq!(
        kind(acme.owner.delete_role("owner").await),
        ProblemType::InvalidRequest
    );
    acme.server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_one_gives_or_takes_away_more_than_they_hold() {
    let acme = Organization::new().await;
    // A reader who also invites and manages the members.
    let manager = PutRole {
        description: "Manages the members.".into(),
        permissions: vec![
            Permission::RunsRead,
            Permission::ActionsRead,
            Permission::SecretsRead,
            Permission::SubscriptionsRead,
            Permission::MembersRead,
            Permission::MembersInvite,
            Permission::MembersManage,
            Permission::RolesRead,
            Permission::NamespacesRead,
        ],
    };
    acme.owner.put_role("manager", &manager).await.unwrap();
    let (grace, grace_id) = acme.member("grace@example.net").await;
    let (_bob, bob_id) = acme.member("bob@example.net").await;
    acme.grant(&grace_id, "manager", &[]).await;

    let give = |role: &str| SetGrant {
        role: role.into(),
        allowed_labels: Labels::new(),
    };
    let refused = problem(
        grace
            .set_member_grant(&bob_id, &give("owner"))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::Forbidden));
    assert!(
        refused.detail.contains("`runs:start`"),
        "{}",
        refused.detail
    );
    assert_eq!(
        kind(
            grace
                .set_member_grant(&acme.owner_id, &give("manager"))
                .await
        ),
        ProblemType::Forbidden,
        "taking owner away"
    );
    assert_eq!(
        kind(grace.remove_member(&acme.owner_id).await),
        ProblemType::Forbidden
    );
    let invite = |role: &str| CreateInvitation {
        email: "eve@example.net".into(),
        role: role.into(),
        allowed_labels: Labels::new(),
    };
    assert_eq!(
        kind(grace.invite(&invite("harness")).await),
        ProblemType::Forbidden
    );
    grace.invite(&invite("reader")).await.unwrap();
    grace
        .set_member_grant(&bob_id, &give("manager"))
        .await
        .unwrap();
    assert_eq!(
        kind(grace.put_role("manager", &manager).await),
        ProblemType::Forbidden,
        "roles:write"
    );
    assert_eq!(
        kind(
            grace
                .update_organization(&UpdateOrganization {
                    name: Some("Mine".into()),
                    default_role: None,
                })
                .await
        ),
        ProblemType::Forbidden
    );
    acme.server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_limited_to_labels_does_not_administer() {
    let acme = Organization::new().await;
    let (grace, grace_id) = acme.member("grace@example.net").await;
    acme.grant(&grace_id, "owner", &[("team", "support")]).await;
    let me = grace.whoami().await.unwrap();
    let administration = [
        Permission::MembersInvite,
        Permission::MembersManage,
        Permission::RolesWrite,
        Permission::ApiKeysRead,
        Permission::ApiKeysWrite,
        Permission::OrganizationWrite,
    ];
    assert_eq!(
        me.permissions,
        Permission::ALL
            .into_iter()
            .filter(|permission| !administration.contains(permission))
            .collect::<Vec<_>>(),
        "what the grant may use"
    );
    grace.list_members().await.unwrap();
    let refused = problem(
        grace
            .update_organization(&UpdateOrganization {
                name: Some("Mine".into()),
                default_role: None,
            })
            .await
            .unwrap_err(),
    );
    assert!(
        refused.detail.contains("limited to labels"),
        "{}",
        refused.detail
    );
    assert_eq!(
        kind(
            grace
                .invite(&CreateInvitation {
                    email: "eve@example.net".into(),
                    role: "reader".into(),
                    allowed_labels: Labels::new(),
                })
                .await
        ),
        ProblemType::Forbidden
    );
    assert_eq!(kind(grace.list_api_keys().await), ProblemType::Forbidden);
    acme.server.stop().await;
}
