//! The dashboard's sign-in, `/auth` (`archi/07-code-architecture.md`, "Sign-in (`/auth`)").
//!
//! For a browser only: these routes read and write the sealed cookie [`SESSION_COOKIE`], which
//! `/v1` never reads. A `POST` takes a JSON body only. Each step answers an [`AuthState`]: signed
//! in with an access token for `/v1`, or the next step.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::common::Timestamp;
use crate::organization::{Organization, Person};

/// The session: the access token, the refresh token and the pending token of a step, sealed.
pub const SESSION_COOKIE: &str = "rewind_session";

/// The `state` and invitation token of a GitHub sign-in in progress, for 5 minutes.
pub const OAUTH_COOKIE: &str = "rewind_oauth";

/// The ways to sign in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SignInMethod {
    /// `GET /auth/github`.
    Github,
    /// A 6-digit code sent by email: `POST /auth/magic-auth`.
    MagicAuth,
}

/// `GET /auth/config`: how to sign in on this server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfig {
    pub methods: Vec<SignInMethod>,
    /// Sign-ups are closed: someone without an account nor an invitation joins the waitlist
    /// (`POST /auth/waitlist`).
    pub waitlist: bool,
}

/// `GET /auth/github`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct GithubSignIn {
    /// The token of the invitation the page was opened with: signing in accepts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation_token: Option<String>,
}

/// `GET /auth/callback`: the provider's redirect, with OAuth's parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct Callback {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Must match the one kept in the cookie `rewind_oauth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// The provider's error, when the sign-in did not go through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

/// `POST /auth/magic-auth`: sends a 6-digit code to the email.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MagicAuthRequest {
    pub email: String,
    /// The token of the invitation the page was opened with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation_token: Option<String>,
}

/// `POST /auth/magic-auth/verify`: the code received by email.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MagicAuthVerification {
    pub email: String,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation_token: Option<String>,
}

/// `POST /auth/email-verification`: the code sent to verify the email, after
/// `email_verification_required`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmailVerification {
    pub code: String,
}

/// `POST /auth/organization`: the organization chosen, after
/// `organization_selection_required`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationSelection {
    pub organization_id: String,
}

/// `POST /auth/session`: the current state; with `organizationId`, a switch to that
/// organization.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
}

/// `POST /auth/waitlist`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WaitlistRequest {
    pub email: String,
}

/// Where a sign-in stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthState {
    /// Signed in: send the access token to `/v1` as a Bearer token, and keep it in memory only.
    #[serde(rename_all = "camelCase")]
    Authenticated {
        access_token: String,
        /// Ask `POST /auth/session` again before then: it refreshes a token with less than a
        /// minute left.
        expires_at: Timestamp,
        person: Person,
        /// The organization of the session, the one `/v1` acts in. Absent without one:
        /// create one, or accept an invitation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        organization_id: Option<String>,
    },
    /// The person belongs to several organizations: choose one with `POST /auth/organization`.
    #[serde(rename_all = "camelCase")]
    OrganizationSelectionRequired {
        person: Person,
        organizations: Vec<Organization>,
    },
    /// A code was sent to the email: give it to `POST /auth/email-verification`.
    #[serde(rename_all = "camelCase")]
    EmailVerificationRequired { email: String },
    /// Sign-ups are closed and this person has no account nor invitation: they may join the
    /// waitlist.
    #[serde(rename_all = "camelCase")]
    SignUpClosed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn states_are_tagged_by_status() {
        let signed_in = AuthState::Authenticated {
            access_token: "eyJ…".into(),
            expires_at: Timestamp::from_second(1_767_225_900).unwrap(),
            person: Person {
                id: "user_01JABC".into(),
                email: "ada@example.net".into(),
                first_name: None,
                last_name: None,
                picture_url: None,
            },
            organization_id: Some("org_01JABC".into()),
        };
        assert_eq!(
            serde_json::to_value(&signed_in).unwrap(),
            json!({
                "status": "authenticated",
                "accessToken": "eyJ…",
                "expiresAt": "2026-01-01T00:05:00Z",
                "person": { "id": "user_01JABC", "email": "ada@example.net" },
                "organizationId": "org_01JABC"
            })
        );
        assert_eq!(
            serde_json::to_value(AuthState::SignUpClosed { email: None }).unwrap(),
            json!({ "status": "sign_up_closed" })
        );
        let selection: AuthState = serde_json::from_value(json!({
            "status": "organization_selection_required",
            "person": { "id": "user_1", "email": "ada@example.net" },
            "organizations": [{ "id": "org_1", "name": "Acme" }]
        }))
        .unwrap();
        let AuthState::OrganizationSelectionRequired { organizations, .. } = selection else {
            panic!("an organization is asked for");
        };
        assert_eq!(organizations.len(), 1);
    }
}
