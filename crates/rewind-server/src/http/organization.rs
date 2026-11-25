//! The organization (`archi/07-code-architecture.md`, "HTTP API `/v1`", group Organization): who
//! calls, the organizations, their members and invitations. WorkOS holds the organizations,
//! users, memberships and invitations; Rewind the grants. A tenant is an organization, with the
//! same id.
//!
//! The onboarding endpoints (`whoami`, creating an organization, the invitations sent to the
//! caller and their acceptance) take any member, even one without organization ([`Who`]).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::access::{DEFAULT_ROLE, OPERATOR, Permission, SetGrant};
use rewind_api::common::timestamp;
use rewind_api::organization::{
    CreateInvitation, CreateOrganization, Identity, Invitation, InvitationList, InvitationState,
    Member, MemberList, MembershipStatus, Organization, OrganizationSettings, Person,
    Principal as CallerPrincipal, TokenKind, UpdateOrganization,
};
use rewind_engine::ports::{
    self, AccessStore, GrantClaim, GrantDeletion, GrantWrite, IdentityProvider, InvitationId,
    OrganizationId, Principal, TenantId, UserId,
};
use rewind_engine::text::{refuse_nul, refuse_nul_in_labels};

use crate::app::{App, SignIn};
use crate::auth::{Caller, Member as SignedIn, TokenForm, Who, role_permissions};
use crate::error::ApiError;
use crate::extract::Body;
use crate::grants::{OWNER, current_permissions, first_grant, givable_role, grant_view, holds};

// -------------------------------------------------------------------------------------------
// Views

fn user_view(user: &ports::User) -> Person {
    Person {
        id: user.id.as_str().to_owned(),
        email: user.email.clone(),
        first_name: user.first_name.clone(),
        last_name: user.last_name.clone(),
        picture_url: user.picture_url.clone(),
    }
}

fn organization_view(organization: &ports::Organization) -> Organization {
    Organization {
        id: organization.id.as_str().to_owned(),
        name: organization.name.clone(),
    }
}

fn membership_status(status: ports::MembershipStatus) -> MembershipStatus {
    match status {
        ports::MembershipStatus::Active => MembershipStatus::Active,
        ports::MembershipStatus::Pending => MembershipStatus::Pending,
        ports::MembershipStatus::Inactive => MembershipStatus::Inactive,
    }
}

fn invitation_state(state: ports::InvitationState) -> InvitationState {
    match state {
        ports::InvitationState::Pending => InvitationState::Pending,
        ports::InvitationState::Accepted => InvitationState::Accepted,
        ports::InvitationState::Expired => InvitationState::Expired,
        ports::InvitationState::Revoked => InvitationState::Revoked,
    }
}

fn invitation_view(
    invitation: &ports::Invitation,
    organization: Option<Organization>,
    grant: Option<&ports::Grant>,
) -> Invitation {
    Invitation {
        id: invitation.id.as_str().to_owned(),
        email: invitation.email.clone(),
        state: invitation_state(invitation.state),
        organization,
        inviter: invitation
            .inviter
            .as_ref()
            .map(|user| user.as_str().to_owned()),
        expires_at: timestamp(invitation.expires_at),
        created_at: timestamp(invitation.created_at),
        grant: grant.map(grant_view),
    }
}

/// An organization as the views name it: its name from the identity provider, its id when it
/// has none there any more.
async fn named(sign_in: &SignIn, organization: &OrganizationId) -> Result<Organization, ApiError> {
    Ok(match sign_in.identity.organization(organization).await? {
        Some(found) => organization_view(&found),
        None => Organization {
            id: organization.as_str().to_owned(),
            name: organization.as_str().to_owned(),
        },
    })
}

/// The caller's tenant, as an organization of the identity provider.
fn tenant_state_of(caller: &Caller) -> OrganizationId {
    OrganizationId::new(caller.tenant.as_str())
}

/// The member behind an onboarding request: keys and the socket's operator are refused.
fn signed_in(who: &Who, what: &str) -> Result<SignedIn, ApiError> {
    who.member.clone().ok_or_else(|| {
        ApiError::forbidden(format!(
            "only a member signed in on the dashboard may {what}: an API key belongs to one \
             organization, and has no email nor organizations"
        ))
    })
}

/// The member's user, as the identity provider knows them.
async fn user_of(sign_in: &SignIn, user: &UserId) -> Result<ports::User, ApiError> {
    sign_in.identity.user(user).await?.ok_or_else(|| {
        ApiError::new(
            ProblemType::Unauthorized,
            "the user of this access token no longer exists: sign in again",
        )
    })
}

fn same_email(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn last_owner() -> ApiError {
    ApiError::new(
        ProblemType::LastOwner,
        "this would leave the organization without a member holding the role owner: give owner \
         to another member first",
    )
}

/// The tenant's default role: the one it set, else `reader`.
async fn default_role(app: &App, tenant: &TenantId) -> Result<String, ApiError> {
    Ok(app
        .store
        .default_role(tenant)
        .await?
        .unwrap_or_else(|| DEFAULT_ROLE.as_str().to_owned()))
}

// -------------------------------------------------------------------------------------------
// Who calls

/// `GET /v1/whoami`: any caller, a member without organization too. A grant limited to labels
/// lists the permissions it may use: without the administration.
pub(super) async fn whoami(
    State(app): State<Arc<App>>,
    who: Who,
) -> Result<Json<Identity>, ApiError> {
    let mut identity = Identity {
        organization_id: None,
        principal: None,
        grant: None,
        permissions: Vec::new(),
        operator: false,
        organization: None,
        organizations: Vec::new(),
    };
    if let Some(connection) = &who.connection {
        identity.principal = Some(super::connections::principal(&app, connection).await?);
    }
    if let Some(caller) = &who.caller {
        identity.organization_id = Some(caller.tenant.as_str().to_owned());
        identity.permissions = caller.effective_permissions();
        identity.operator = caller.operator();
        identity.grant = caller.role.clone().map(|role| rewind_api::access::Grant {
            role,
            allowed_labels: caller.labels.clone().into(),
            granted_by: caller.granted_by.as_ref().map(ToString::to_string),
            granted_at: caller.granted_at.map(timestamp),
        });
        if let Some(Principal::Key(hash)) = &caller.principal
            && caller.connection.is_none()
        {
            identity.principal = Some(CallerPrincipal::Key { id: hash.to_hex() });
        }
        if let (Some(token), Some(namespace)) = (&caller.token, &caller.namespace) {
            let (kind, id, kid, jti) = match &token.form {
                TokenForm::Opaque { hash } => (TokenKind::Opaque, Some(hash.to_hex()), None, None),
                TokenForm::Signed { kid, jti, .. } => {
                    (TokenKind::Signed, None, Some(kid.clone()), jti.clone())
                }
            };
            identity.principal = Some(CallerPrincipal::Token {
                token: kind,
                id,
                kid,
                jti,
                namespace: crate::views::namespace(namespace),
                labels: caller.labels.clone().into(),
                expires_at: timestamp(token.expires_at),
            });
        }
    }
    if let Some(member) = &who.member {
        let sign_in = app.sign_in()?;
        let user = user_of(sign_in, &member.user).await?;
        identity.principal = Some(CallerPrincipal::Member {
            person: user_view(&user),
        });
        if let Some(organization) = &member.organization {
            let named = named(sign_in, organization).await?;
            let tenant = organization
                .tenant()
                .map_err(|error| ApiError::invalid(error.to_string()))?;
            identity.organization = Some(OrganizationSettings {
                id: named.id,
                name: named.name,
                default_role: default_role(&app, &tenant).await?,
            });
        }
        identity.organizations = sign_in
            .identity
            .memberships_of_user(&member.user)
            .await?
            .iter()
            .filter(|membership| membership.status == ports::MembershipStatus::Active)
            .map(|membership| organization_view(&membership.organization))
            .collect();
    }
    Ok(Json(identity))
}

// -------------------------------------------------------------------------------------------
// Organizations

/// `POST /v1/organizations`: a member, even without organization. The organization in WorkOS
/// with the caller's membership, then its tenant and the caller's `owner` grant; if a step
/// fails, the organization is deleted in WorkOS: no organization without an owner.
pub(super) async fn create_organization(
    State(app): State<Arc<App>>,
    who: Who,
    Body(request): Body<CreateOrganization>,
) -> Result<Response, ApiError> {
    let member = signed_in(&who, "create an organization")?;
    let sign_in = app.sign_in()?;
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::invalid("name: an organization needs a name"));
    }
    refuse_nul("the name", name)?;
    let organization = sign_in.identity.create_organization(name).await?;
    let owned = async {
        sign_in
            .identity
            .add_member(&organization.id, &member.user)
            .await?;
        let tenant = organization
            .id
            .tenant()
            .map_err(|error| ApiError::internal(format!("the organization's id: {error}")))?;
        app.ensure_tenant(&tenant).await?;
        app.store
            .put_grant(
                &ports::Grant {
                    tenant,
                    principal: Principal::from(&member.user),
                    role: OWNER.to_owned(),
                    allowed_labels: BTreeMap::new(),
                    granted_by: None,
                    granted_at: app.now(),
                },
                None,
            )
            .await?;
        Ok::<(), ApiError>(())
    };
    if let Err(error) = owned.await {
        if let Err(deleting) = sign_in.identity.delete_organization(&organization.id).await {
            tracing::error!(organization = %organization.id, error = %deleting, "an organization without an owner was not deleted");
        }
        return Err(error);
    }
    Ok((StatusCode::CREATED, Json(organization_view(&organization))).into_response())
}

/// `PATCH /v1/organization`: its name (in WorkOS) and its default role, a role of the
/// organization or a built-in one that the caller holds entirely.
pub(super) async fn update_organization(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<UpdateOrganization>,
) -> Result<Json<OrganizationSettings>, ApiError> {
    caller.require(Permission::OrganizationWrite)?;
    let sign_in = app.sign_in()?;
    let organization = tenant_state_of(&caller);
    let name = match request.name.as_deref().map(str::trim) {
        Some("") => return Err(ApiError::invalid("name: an organization needs a name")),
        Some(name) => {
            refuse_nul("the name", name)?;
            Some(name)
        }
        None => None,
    };
    if let Some(role) = &request.default_role {
        if role == OPERATOR {
            return Err(ApiError::invalid(
                "defaultRole: operator is the deployment's role, not a role of an organization",
            ));
        }
        let Some(permissions) = role_permissions(&app.store, &caller.tenant, role).await? else {
            return Err(ApiError::invalid(format!(
                "defaultRole: no role {role:?} in this organization: GET /v1/roles lists them"
            )));
        };
        holds(&caller, role, &permissions, "making the default role")?;
    }
    let named = match name {
        Some(name) => organization_view(
            &sign_in
                .identity
                .rename_organization(&organization, name)
                .await?,
        ),
        None => {
            let Some(found) = sign_in.identity.organization(&organization).await? else {
                return Err(ApiError::not_found(format!(
                    "{} is not an organization of the identity provider: only those have a name \
                     and a default role",
                    caller.tenant
                )));
            };
            organization_view(&found)
        }
    };
    if let Some(role) = &request.default_role {
        app.store.set_default_role(&caller.tenant, role).await?;
    }
    Ok(Json(OrganizationSettings {
        id: named.id,
        name: named.name,
        default_role: default_role(&app, &caller.tenant).await?,
    }))
}

// -------------------------------------------------------------------------------------------
// The invitations sent to the caller

/// `GET /v1/invitations`: the pending invitations to an organization sent to the caller's
/// email, each with its organization and pending grant.
pub(super) async fn my_invitations(
    State(app): State<Arc<App>>,
    who: Who,
) -> Result<Json<InvitationList>, ApiError> {
    let member = signed_in(&who, "list the invitations sent to them")?;
    let sign_in = app.sign_in()?;
    let user = user_of(sign_in, &member.user).await?;
    let mut invitations = Vec::new();
    for invitation in sign_in.identity.invitations_for(&user.email).await? {
        let Some(organization) = &invitation.organization else {
            continue;
        };
        if invitation.state != ports::InvitationState::Pending {
            continue;
        }
        let grant = match organization.tenant() {
            Ok(tenant) => {
                app.store
                    .grant(&tenant, &Principal::from(&invitation.id))
                    .await?
            }
            Err(_) => None,
        };
        let named = named(sign_in, organization).await?;
        invitations.push(invitation_view(&invitation, Some(named), grant.as_ref()));
    }
    Ok(Json(InvitationList { invitations }))
}

/// `POST /v1/invitations/{id}/accept`: a pending invitation to an organization, sent to the
/// caller's email. The caller joins the organization and gets the invitation's pending grant
/// (or keeps the grant they have there; without either, the organization's default role).
pub(super) async fn accept_invitation(
    State(app): State<Arc<App>>,
    who: Who,
    Path(id): Path<String>,
) -> Result<Json<Invitation>, ApiError> {
    let member = signed_in(&who, "accept an invitation")?;
    let sign_in = app.sign_in()?;
    let missing = || {
        ApiError::not_found(format!(
            "no pending invitation {id} to an organization for this user's email: GET \
             /v1/invitations lists theirs"
        ))
    };
    let invitation_id = InvitationId::new(id.trim());
    let user = user_of(sign_in, &member.user).await?;
    let invitation = sign_in
        .identity
        .invitation(&invitation_id)
        .await?
        .filter(|invitation| invitation.state == ports::InvitationState::Pending)
        .filter(|invitation| same_email(&invitation.email, &user.email))
        .ok_or_else(missing)?;
    let organization = invitation.organization.clone().ok_or_else(missing)?;
    let tenant = organization.tenant().map_err(|_| missing())?;
    let accepted = sign_in.identity.accept_invitation(&invitation_id).await?;
    app.ensure_tenant(&tenant).await?;
    let principal = Principal::from(&member.user);
    let claim = app
        .store
        .claim_grant(&tenant, &Principal::from(&invitation_id), &principal)
        .await?;
    if claim == GrantClaim::NoGrant {
        first_grant(&app, &sign_in.identity, &member.user, &organization).await?;
    }
    let grant = app.store.grant(&tenant, &principal).await?;
    let named = named(sign_in, &organization).await?;
    Ok(Json(invitation_view(
        &accepted,
        Some(named),
        grant.as_ref(),
    )))
}

// -------------------------------------------------------------------------------------------
// Members

/// The organization's membership of `user`, or 404.
async fn membership(
    sign_in: &SignIn,
    caller: &Caller,
    user: &str,
) -> Result<ports::Membership, ApiError> {
    sign_in
        .identity
        .members(&tenant_state_of(caller))
        .await?
        .into_iter()
        .find(|membership| membership.user.id.as_str() == user)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no member {user} in this organization: GET /v1/organization/members lists them"
            ))
        })
}

/// `GET /v1/organization/members`: the memberships in WorkOS, each with its grant.
pub(super) async fn members(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<MemberList>, ApiError> {
    caller.require(Permission::MembersRead)?;
    let sign_in = app.sign_in()?;
    let memberships = sign_in.identity.members(&tenant_state_of(&caller)).await?;
    let grants: HashMap<Principal, ports::Grant> = app
        .store
        .list_grants(&caller.tenant)
        .await?
        .into_iter()
        .map(|grant| (grant.principal.clone(), grant))
        .collect();
    let members = memberships
        .iter()
        .map(|membership| Member {
            person: user_view(&membership.user),
            status: membership_status(membership.status),
            grant: grants
                .get(&Principal::from(&membership.user.id))
                .map(grant_view),
        })
        .collect();
    Ok(Json(MemberList { members }))
}

/// `PUT /v1/organization/members/{user}/grant`: the member's role and allowed labels. The caller
/// holds every permission of the role given and of the member's current one; the last owner
/// keeps `owner` (409).
pub(super) async fn set_member_grant(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(user): Path<String>,
    Body(request): Body<SetGrant>,
) -> Result<Json<rewind_api::access::Grant>, ApiError> {
    caller.require(Permission::MembersManage)?;
    let sign_in = app.sign_in()?;
    let membership = membership(sign_in, &caller, &user).await?;
    let permissions = givable_role(&app, &caller, &request.role).await?;
    holds(&caller, &request.role, &permissions, "giving")?;
    refuse_nul_in_labels(&request.allowed_labels.0)?;
    let principal = Principal::from(&membership.user.id);
    if let Some(current) = app.store.grant(&caller.tenant, &principal).await? {
        let taken = current_permissions(&app, &caller, &current).await?;
        holds(&caller, &current.role, &taken, "taking away")?;
    }
    let grant = ports::Grant {
        tenant: caller.tenant.clone(),
        principal,
        role: request.role,
        allowed_labels: request.allowed_labels.0,
        granted_by: caller.principal.clone(),
        granted_at: app.now(),
    };
    match app.store.put_grant(&grant, Some(OWNER)).await? {
        GrantWrite::Written => Ok(Json(grant_view(&grant))),
        GrantWrite::LastHolder => Err(last_owner()),
    }
}

/// `DELETE /v1/organization/members/{user}`: deletes the member's grant, which cuts their
/// access at their next request, and their connections' grants here, then their membership in
/// WorkOS. The caller holds every
/// permission of the member's role; the last owner stays (409).
pub(super) async fn remove_member(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(user): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::MembersManage)?;
    let sign_in = app.sign_in()?;
    let membership = membership(sign_in, &caller, &user).await?;
    let principal = Principal::from(&membership.user.id);
    if let Some(current) = app.store.grant(&caller.tenant, &principal).await? {
        let taken = current_permissions(&app, &caller, &current).await?;
        holds(&caller, &current.role, &taken, "taking away")?;
    }
    match app
        .store
        .delete_grant(&caller.tenant, &principal, Some(OWNER))
        .await?
    {
        GrantDeletion::Deleted | GrantDeletion::NotFound => {}
        GrantDeletion::LastHolder => return Err(last_owner()),
    }
    super::connections::close_for(&app, &caller.tenant, &membership.user.id).await?;
    sign_in.identity.remove_member(&membership.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------------------------------------
// The organization's invitations

/// An invitation of the caller's organization, or 404.
async fn invitation_of(
    sign_in: &SignIn,
    caller: &Caller,
    id: &str,
) -> Result<ports::Invitation, ApiError> {
    let organization = tenant_state_of(caller);
    sign_in
        .identity
        .invitation(&InvitationId::new(id.trim()))
        .await?
        .filter(|invitation| invitation.organization.as_ref() == Some(&organization))
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no invitation {id} in this organization: GET /v1/organization/invitations \
                 lists them"
            ))
        })
}

/// `GET /v1/organization/invitations`: every invitation of the organization, each with its
/// pending grant.
pub(super) async fn invitations(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<InvitationList>, ApiError> {
    caller.require(Permission::MembersRead)?;
    let sign_in = app.sign_in()?;
    let organization = tenant_state_of(&caller);
    let found = sign_in.identity.invitations_of(&organization).await?;
    let grants: HashMap<Principal, ports::Grant> = app
        .store
        .list_grants(&caller.tenant)
        .await?
        .into_iter()
        .filter(|grant| matches!(grant.principal, Principal::Invitation(_)))
        .map(|grant| (grant.principal.clone(), grant))
        .collect();
    let named = named(sign_in, &organization).await?;
    let invitations = found
        .iter()
        .map(|invitation| {
            invitation_view(
                invitation,
                Some(named.clone()),
                grants.get(&Principal::from(&invitation.id)),
            )
        })
        .collect();
    Ok(Json(InvitationList { invitations }))
}

/// `POST /v1/organization/invitations`: the invitation in WorkOS, with the caller as its inviter
/// when a member, and its pending grant (`invitation:<id>`). The caller holds every permission
/// of the role given. A grant that cannot be written revokes the invitation.
pub(super) async fn invite(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<CreateInvitation>,
) -> Result<Response, ApiError> {
    caller.require(Permission::MembersInvite)?;
    let sign_in = app.sign_in()?;
    let email = request.email.trim();
    if email.is_empty() {
        return Err(ApiError::invalid("email: an invitation needs an email"));
    }
    refuse_nul("the email", email)?;
    let permissions = givable_role(&app, &caller, &request.role).await?;
    holds(&caller, &request.role, &permissions, "giving")?;
    refuse_nul_in_labels(&request.allowed_labels.0)?;
    let organization = tenant_state_of(&caller);
    let inviter = match &caller.principal {
        Some(Principal::Member(user)) => Some(UserId::new(user.clone())),
        _ => None,
    };
    let invitation = sign_in
        .identity
        .invite(Some(&organization), email, inviter.as_ref())
        .await?;
    let grant = ports::Grant {
        tenant: caller.tenant.clone(),
        principal: Principal::from(&invitation.id),
        role: request.role,
        allowed_labels: request.allowed_labels.0,
        granted_by: caller.principal.clone(),
        granted_at: app.now(),
    };
    if let Err(error) = app.store.put_grant(&grant, None).await {
        // An invitation without its grant would let its invitee in with the default role.
        if let Err(revoking) = sign_in.identity.revoke_invitation(&invitation.id).await {
            tracing::error!(invitation = %invitation.id, error = %revoking, "an invitation without its grant was not revoked");
        }
        return Err(error.into());
    }
    let named = named(sign_in, &organization).await?;
    let view = invitation_view(&invitation, Some(named), Some(&grant));
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

/// `POST /v1/organization/invitations/{id}/resend`: sends a pending invitation again.
pub(super) async fn resend_invitation(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Invitation>, ApiError> {
    caller.require(Permission::MembersInvite)?;
    let sign_in = app.sign_in()?;
    let invitation = invitation_of(sign_in, &caller, &id).await?;
    let resent = sign_in.identity.resend_invitation(&invitation.id).await?;
    let grant = app
        .store
        .grant(&caller.tenant, &Principal::from(&invitation.id))
        .await?;
    let named = named(sign_in, &tenant_state_of(&caller)).await?;
    Ok(Json(invitation_view(&resent, Some(named), grant.as_ref())))
}

/// `DELETE /v1/organization/invitations/{id}`: revokes a pending invitation and deletes its
/// pending grant, whose role's permissions the caller holds.
pub(super) async fn revoke_invitation(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::MembersInvite)?;
    let sign_in = app.sign_in()?;
    let invitation = invitation_of(sign_in, &caller, &id).await?;
    if invitation.state != ports::InvitationState::Pending {
        return Err(ApiError::not_found(format!(
            "the invitation {id} is not pending: only a pending invitation is revoked"
        )));
    }
    let principal = Principal::from(&invitation.id);
    if let Some(pending) = app.store.grant(&caller.tenant, &principal).await? {
        let taken = current_permissions(&app, &caller, &pending).await?;
        holds(&caller, &pending.role, &taken, "taking away")?;
    }
    sign_in.identity.revoke_invitation(&invitation.id).await?;
    app.store
        .delete_grant(&caller.tenant, &principal, None)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
