//! How WorkOS's errors become the port's, and the sign-in steps WorkOS answers as errors.
//!
//! Every code this adapter reads is in this module. A message WorkOS writes may repeat what it
//! was sent (a code, a pending token, an invitation token): each call names the secrets it
//! sent, and they are replaced before a message leaves the adapter.

use rewind_engine::ports::{
    Authentication, IdentityError, Organization, OrganizationId, PendingToken,
};
use serde::Deserialize;
use workos::Error;
use workos::transport::{TransportError, TransportErrorKind};

use crate::convert;

/// The codes of a sign-up WorkOS refuses because sign-ups are closed (its waitlist) and the
/// person has neither an account nor an invitation.
///
/// WorkOS does not document them for a sign-in through its API. `sign_up_not_allowed` is the
/// error its SDKs model (`SignUpNotAllowedOAuthErrorBody`); it is to be confirmed once the
/// Waitlist is enabled on WorkOS Staging (`archi/08-decisions.md`).
pub const SIGN_UP_CLOSED: &[&str] = &["sign_up_not_allowed"];

/// The codes of a refresh token that is unknown, or whose session ended or was revoked.
pub(crate) const INVALID_REFRESH_TOKEN: &[&str] = &[
    "invalid_grant",
    "invalid_refresh_token",
    "session_expired",
    "session_revoked",
    "user_session_ended",
];

/// The code of a pending token (an email to verify, an organization to choose) that is unknown
/// or expired.
pub(crate) const INVALID_PENDING_TOKEN: &[&str] = &["invalid_pending_authentication_token"];

/// WorkOS asks the user to choose one of their organizations.
const ORGANIZATION_SELECTION_REQUIRED: &str = "organization_selection_required";

/// WorkOS sent a code to the user's email, to verify it.
const EMAIL_VERIFICATION_REQUIRED: &str = "email_verification_required";

/// What a message says in place of a secret.
const REDACTED: &str = "<redacted>";

/// A sign-in that WorkOS did not end with a session.
pub(crate) struct SignIn<'a> {
    /// The email the person signs in with, when the call names one.
    pub(crate) email: Option<&'a str>,
    /// The codes that mean the token the call sent is invalid, expired or revoked.
    pub(crate) invalid_token: &'a [&'a str],
    /// What the call sent that a message must not repeat.
    pub(crate) secrets: &'a [&'a str],
}

/// The body of an error that is a sign-in step.
#[derive(Deserialize)]
struct StepBody {
    #[serde(default)]
    pending_authentication_token: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    user: Option<serde_json::Value>,
    #[serde(default)]
    organizations: Option<Vec<StepOrganization>>,
}

#[derive(Deserialize)]
struct StepOrganization {
    id: String,
    name: String,
}

/// The body of an error about a missing object.
#[derive(Deserialize)]
struct NotFoundBody {
    #[serde(default)]
    entity_id: Option<String>,
}

/// The answer of a sign-in that failed: the next step WorkOS asks for, a closed sign-up, or an
/// error.
pub(crate) fn sign_in_failure(
    error: Error,
    sign_in: &SignIn<'_>,
) -> Result<Authentication, IdentityError> {
    let Error::Api(api) = &error else {
        return Err(refusal(error, sign_in.secrets));
    };
    let code = api.code.as_deref().unwrap_or_default();
    if code == ORGANIZATION_SELECTION_REQUIRED || code == EMAIL_VERIFICATION_REQUIRED {
        let body: StepBody =
            serde_json::from_slice(&api.raw_body).map_err(|_| unreadable_step(code, "its body"))?;
        let pending = body
            .pending_authentication_token
            .filter(|token| !token.is_empty())
            .map(PendingToken::new)
            .ok_or_else(|| unreadable_step(code, "its pending token"))?;
        if code == EMAIL_VERIFICATION_REQUIRED {
            let email = body
                .email
                .or_else(|| sign_in.email.map(normalize))
                .ok_or_else(|| unreadable_step(code, "its email"))?;
            return Ok(Authentication::EmailVerificationRequired { pending, email });
        }
        let user = body
            .user
            .and_then(|user| serde_json::from_value::<workos::User>(user).ok())
            .ok_or_else(|| unreadable_step(code, "its user"))?;
        let organizations = body
            .organizations
            .ok_or_else(|| unreadable_step(code, "its organizations"))?
            .into_iter()
            .map(|organization| Organization {
                id: OrganizationId::new(organization.id),
                name: organization.name,
            })
            .collect();
        return Ok(Authentication::OrganizationSelectionRequired {
            pending,
            user: convert::user(&user),
            organizations,
        });
    }
    if is_sign_up_closed(&error) {
        let email = serde_json::from_slice::<StepBody>(&api.raw_body)
            .ok()
            .and_then(|body| body.email)
            .or_else(|| sign_in.email.map(normalize));
        return Ok(Authentication::SignUpClosed { email });
    }
    if (400..500).contains(&api.status) && sign_in.invalid_token.contains(&code) {
        return Err(IdentityError::InvalidToken);
    }
    // A sign-in names no object but its tokens: what WorkOS does not find (an invitation's
    // token, an organization) is a refusal, as an unknown code is.
    Err(refusal(error, sign_in.secrets))
}

/// Whether WorkOS refused a sign-up because sign-ups are closed.
pub(crate) fn is_sign_up_closed(error: &Error) -> bool {
    error
        .code()
        .is_some_and(|code| SIGN_UP_CLOSED.contains(&code))
}

/// The error of a call that is not a sign-in: `NotFound` for a missing object (`what` names it
/// when WorkOS does not), `Refused` for another refusal, `Unavailable` when WorkOS cannot be
/// reached or fails.
pub(crate) fn failure(
    error: Error,
    what: impl FnOnce() -> String,
    secrets: &[&str],
) -> IdentityError {
    if let Error::Api(api) = &error
        && api.status == 404
    {
        let entity = serde_json::from_slice::<NotFoundBody>(&api.raw_body)
            .ok()
            .and_then(|body| body.entity_id)
            .filter(|id| !id.is_empty())
            .map(|id| describe(&redact(&id, secrets)));
        return IdentityError::NotFound(entity.unwrap_or_else(what));
    }
    refusal(error, secrets)
}

/// `Refused` for a request WorkOS refused, `Unavailable` otherwise.
pub(crate) fn refusal(error: Error, secrets: &[&str]) -> IdentityError {
    match error {
        Error::Api(api) => match api.status {
            // The deployment's API key is refused, or WorkOS asks to slow down: nothing the
            // request itself can change.
            401 => IdentityError::Unavailable("WorkOS refused the API key".into()),
            429 => IdentityError::Unavailable("WorkOS limits the rate of requests".into()),
            400..=499 => IdentityError::Refused {
                code: api
                    .code
                    .clone()
                    .unwrap_or_else(|| format!("http_{}", api.status)),
                message: redact(&api.message, secrets),
            },
            status => IdentityError::Unavailable(format!(
                "WorkOS answered {status}: {}",
                redact(&api.message, secrets)
            )),
        },
        Error::Network(transport) => IdentityError::Unavailable(unreachable(&transport)),
        Error::Decode(decode) => IdentityError::Unavailable(unreadable(&decode)),
        other => IdentityError::Unavailable(format!("WorkOS: {other}")),
    }
}

/// Why WorkOS cannot be reached. The transport's own message names the URL, which is left out.
fn unreachable(error: &TransportError) -> String {
    let kind = match error.kind {
        TransportErrorKind::Connect => "the connection failed",
        TransportErrorKind::Timeout => "the request timed out",
        TransportErrorKind::Other => "the request failed",
    };
    let mut causes = Vec::new();
    let mut cause = error.source.source();
    while let Some(current) = cause {
        causes.push(current.to_string());
        cause = current.source();
    }
    if causes.is_empty() {
        format!("WorkOS cannot be reached: {kind}")
    } else {
        format!("WorkOS cannot be reached: {kind} ({})", causes.join(": "))
    }
}

/// A body the SDK cannot read. Only a missing field is named: other messages may quote a value.
fn unreadable(error: &serde_json::Error) -> String {
    let message = error.to_string();
    if message.starts_with("missing field") {
        format!("WorkOS answered a body the adapter cannot read: {message}")
    } else {
        format!(
            "WorkOS answered a body the adapter cannot read (line {}, column {})",
            error.line(),
            error.column()
        )
    }
}

fn unreadable_step(code: &str, missing: &str) -> IdentityError {
    IdentityError::Unavailable(format!("WorkOS answered {code} without {missing}"))
}

/// `message` without the secrets a call sent.
pub(crate) fn redact(message: &str, secrets: &[&str]) -> String {
    secrets
        .iter()
        .filter(|secret| !secret.is_empty())
        .fold(message.to_owned(), |message, secret| {
            message.replace(secret, REDACTED)
        })
}

/// An object of WorkOS by its id: `organization org_01…`.
fn describe(id: &str) -> String {
    let noun = [
        ("org_", "organization"),
        ("user_", "user"),
        ("om_", "membership"),
        ("invitation_", "invitation"),
        ("session_", "session"),
    ]
    .iter()
    .find(|(prefix, _)| id.starts_with(prefix))
    .map_or("object", |(_, noun)| noun);
    format!("{noun} {id}")
}

/// An email as WorkOS keeps it, and as the in-memory provider compares it.
pub(crate) fn normalize(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_and_ids_named() {
        assert_eq!(
            redact(
                "The user for pending authentication abc123 does not belong",
                &["", "abc123"]
            ),
            "The user for pending authentication <redacted> does not belong"
        );
        assert_eq!(describe("org_01J"), "organization org_01J");
        assert_eq!(describe("om_01J"), "membership om_01J");
        assert_eq!(describe("weird"), "object weird");
        assert_eq!(normalize(" Ada@Example.NET "), "ada@example.net");
    }
}
