//! WorkOS's objects as the port's. Roles are never read.

use rewind_core::Timestamp;
use rewind_engine::ports::{
    IdentityError, Invitation, InvitationId, InvitationState, MembershipStatus, Organization,
    OrganizationId, User, UserId,
};

pub(crate) fn user(user: &workos::User) -> User {
    User {
        id: UserId::new(user.id.clone()),
        email: user.email.clone(),
        first_name: user.first_name.clone(),
        last_name: user.last_name.clone(),
        picture_url: user.profile_picture_url.clone(),
        email_verified: user.email_verified,
    }
}

pub(crate) fn organization(organization: &workos::Organization) -> Organization {
    Organization {
        id: OrganizationId::new(organization.id.clone()),
        name: organization.name.clone(),
    }
}

pub(crate) fn membership_status(status: &str) -> Result<MembershipStatus, IdentityError> {
    match status {
        "active" => Ok(MembershipStatus::Active),
        "pending" => Ok(MembershipStatus::Pending),
        "inactive" => Ok(MembershipStatus::Inactive),
        other => Err(IdentityError::Unavailable(format!(
            "WorkOS answered an unknown membership status {other:?}"
        ))),
    }
}

/// The fields of an invitation, which the SDK models twice (`Invitation`, `UserInvite`).
pub(crate) struct InvitationFields<'a> {
    pub(crate) id: &'a str,
    pub(crate) email: &'a str,
    pub(crate) state: &'a str,
    pub(crate) organization_id: Option<&'a str>,
    pub(crate) inviter_user_id: Option<&'a str>,
    pub(crate) accepted_user_id: Option<&'a str>,
    pub(crate) expires_at: &'a str,
    pub(crate) created_at: &'a str,
}

/// The fields of either model of an invitation. Its token and role are left out.
macro_rules! invitation_fields {
    ($invitation:expr) => {{
        let invitation = $invitation;
        $crate::convert::InvitationFields {
            id: &invitation.id,
            email: &invitation.email,
            state: invitation.state.as_str(),
            organization_id: invitation.organization_id.as_deref(),
            inviter_user_id: invitation.inviter_user_id.as_deref(),
            accepted_user_id: invitation.accepted_user_id.as_deref(),
            expires_at: &invitation.expires_at,
            created_at: &invitation.created_at,
        }
    }};
}
pub(crate) use invitation_fields;

pub(crate) fn invitation(fields: InvitationFields<'_>) -> Result<Invitation, IdentityError> {
    let state = match fields.state {
        "pending" => InvitationState::Pending,
        "accepted" => InvitationState::Accepted,
        "expired" => InvitationState::Expired,
        "revoked" => InvitationState::Revoked,
        other => {
            return Err(IdentityError::Unavailable(format!(
                "WorkOS answered an unknown invitation state {other:?}"
            )));
        }
    };
    Ok(Invitation {
        id: InvitationId::new(fields.id),
        email: fields.email.to_owned(),
        organization: fields.organization_id.map(OrganizationId::new),
        state,
        inviter: fields.inviter_user_id.map(UserId::new),
        accepted_user: fields.accepted_user_id.map(UserId::new),
        expires_at: timestamp(fields.expires_at)?,
        created_at: timestamp(fields.created_at)?,
    })
}

/// An ISO 8601 timestamp of WorkOS (`2026-11-14T17:30:39.577Z`).
pub(crate) fn timestamp(text: &str) -> Result<Timestamp, IdentityError> {
    let parsed: jiff::Timestamp = text.parse().map_err(|_| {
        IdentityError::Unavailable(format!("WorkOS answered an invalid timestamp {text:?}"))
    })?;
    Ok(Timestamp::from_millis(
        u64::try_from(parsed.as_millisecond()).unwrap_or(0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_read_to_the_millisecond() {
        assert_eq!(
            timestamp("2026-11-14T17:30:39.577Z").unwrap(),
            Timestamp::from_millis(1_794_677_439_577)
        );
        assert!(timestamp("yesterday").is_err());
    }
}
