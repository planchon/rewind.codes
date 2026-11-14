//! Grants given to people (`archi/06-storage-and-scale.md`, "What the caller may do"): a member's
//! first grant in an organization, the roles one may give, and the rule that no one gives or
//! takes away more than they hold.

use std::collections::BTreeSet;

use rewind_api::access::{DEFAULT_ROLE, Grant, OPERATOR, Permission};
use rewind_api::common::{Labels, timestamp};
use rewind_engine::ports::{
    self, AccessStore, IdentityProvider, InvitationState, MembershipStatus, OrganizationId,
    Principal, UserId,
};

use crate::app::App;
use crate::auth::{Caller, listed, role_permissions};
use crate::error::ApiError;
use crate::wiring::identity::Identity;

/// The role that no write may leave without a member holding it.
pub(crate) const OWNER: &str = "owner";

/// A grant as the API shows it.
pub(crate) fn grant_view(grant: &ports::Grant) -> Grant {
    Grant {
        role: grant.role.clone(),
        allowed_labels: Labels::from(grant.allowed_labels.clone()),
        granted_by: grant.granted_by.as_ref().map(ToString::to_string),
        granted_at: Some(timestamp(grant.granted_at)),
    }
}

/// The permissions of a role given to someone in the caller's tenant: a built-in role or a role
/// of the tenant. `operator` is no role of an organization (403); an unknown role is 404.
pub(crate) async fn givable_role(
    app: &App,
    caller: &Caller,
    role: &str,
) -> Result<BTreeSet<Permission>, ApiError> {
    if role == OPERATOR {
        return Err(ApiError::forbidden(
            "operator is the deployment's role, not a role of any organization: give a built-in \
             role (owner, harness, worker, reader) or a role of the organization",
        ));
    }
    role_permissions(&app.store, &caller.tenant, role)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no role {role:?} in this organization: GET /v1/roles lists the built-in roles \
                 and the organization's own"
            ))
        })
}

/// Refuses (403) unless the caller holds every permission of `role`, which it gives (`doing` is
/// "giving") or takes away ("taking away").
pub(crate) fn holds(
    caller: &Caller,
    role: &str,
    permissions: &BTreeSet<Permission>,
    doing: &str,
) -> Result<(), ApiError> {
    let lacking = caller.lacks(permissions);
    if lacking.is_empty() {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "{doing} the role {role} means holding {}, which the caller does not hold: no one gives \
         or takes away more than they hold",
        listed(&lacking)
    )))
}

/// The permissions of a principal's current grant, to take it away: those of its role, none for
/// a role that no longer exists.
pub(crate) async fn current_permissions(
    app: &App,
    caller: &Caller,
    grant: &ports::Grant,
) -> Result<BTreeSet<Permission>, ApiError> {
    Ok(role_permissions(&app.store, &caller.tenant, &grant.role)
        .await?
        .unwrap_or_default())
}

/// A member's first grant in an organization, written when a session is established there or
/// when they accept an invitation (`archi/08-decisions.md`: never deduced at request time). A
/// member who has a grant keeps it. Otherwise, if they are still a member: the pending grant of
/// the latest invitation of the organization they accepted, else the organization's default
/// role (`reader` unless changed), given by no one. Someone the organization removed gets
/// nothing, even with an access token issued before.
pub(crate) async fn first_grant(
    app: &App,
    identity: &Identity,
    user: &UserId,
    organization: &OrganizationId,
) -> Result<(), ApiError> {
    let tenant = organization
        .tenant()
        .map_err(|error| ApiError::invalid(error.to_string()))?;
    let principal = Principal::from(user);
    if app.store.grant(&tenant, &principal).await?.is_some() {
        return Ok(());
    }
    let member = identity
        .memberships_of_user(user)
        .await?
        .iter()
        .any(|membership| {
            membership.organization.id == *organization
                && membership.status == MembershipStatus::Active
        });
    if !member {
        return Ok(());
    }
    app.ensure_tenant(&tenant).await?;
    let mut accepted: Vec<_> = identity
        .invitations_of(organization)
        .await?
        .into_iter()
        .filter(|invitation| {
            invitation.state == InvitationState::Accepted
                && invitation.accepted_user.as_ref() == Some(user)
        })
        .collect();
    accepted.sort_by_key(|invitation| std::cmp::Reverse(invitation.created_at));
    for invitation in &accepted {
        let pending = Principal::from(&invitation.id);
        if app.store.grant(&tenant, &pending).await?.is_none() {
            continue;
        }
        match app.store.claim_grant(&tenant, &pending, &principal).await? {
            ports::GrantClaim::Claimed(_) | ports::GrantClaim::AlreadyGranted(_) => return Ok(()),
            ports::GrantClaim::NoGrant => {}
        }
    }
    let role = app
        .store
        .default_role(&tenant)
        .await?
        .unwrap_or_else(|| DEFAULT_ROLE.as_str().to_owned());
    app.store
        .put_grant(
            &ports::Grant {
                tenant,
                principal,
                role,
                allowed_labels: std::collections::BTreeMap::new(),
                granted_by: None,
                granted_at: app.now(),
            },
            None,
        )
        .await?;
    Ok(())
}
