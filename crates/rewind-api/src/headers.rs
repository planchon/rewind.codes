//! The headers of the API.
//!
//! The webhook signature headers are in [`crate::webhooks`].

use std::fmt;

use crate::Error;

/// `Authorization: Bearer <token>` on every request to `/v1`, except over the embedded server's
/// socket: an API key `rewind_sk_…`, a member's access token from `/auth`, or an end user's
/// token, opaque (`rewind_nt_…`) or a JWT the customer signed with a key whose `kid` starts with
/// `rk_`. None to `POST /hooks/{token}`.
pub const AUTHORIZATION: &str = "Authorization";

/// The prefix of every API key.
pub const API_KEY_PREFIX: &str = "rewind_sk_";

/// The prefix of every end user's opaque token.
pub const TOKEN_PREFIX: &str = "rewind_nt_";

/// The prefix of the `kid` of every key a customer signs its end users' tokens with.
pub const TOKEN_KEY_PREFIX: &str = "rk_";

/// The namespace a key's or a member's request acts for (`archi/06-storage-and-scale.md`,
/// "Namespaces"): required by `POST /v1/runs`, `POST /v1/check` and `POST /v1/test`; on a read,
/// it narrows the answer to that namespace. A token's namespace is its own: the header may
/// repeat it, and naming another is `forbidden`.
pub const NAMESPACE: &str = "Rewind-Namespace";

/// The organization a person's connection (`rewind_ct_…`) acts in
/// (`archi/06-storage-and-scale.md`, "Connections"): required on the tenant's data with a
/// connection's token, `forbidden` for an organization the connection does not open. Other
/// callers may repeat their own tenant there; naming another is `forbidden`.
pub const ORGANIZATION: &str = "Rewind-Organization";

/// On `POST /v1/runs`, `POST /v1/runs/{id}/fork` and `POST /v1/runs/{id}/signals/{name}`
/// (`archi/06-storage-and-scale.md`, "Idempotency of the API").
pub const IDEMPOTENCY_KEY: &str = "Idempotency-Key";

/// On every response: the version of the server's `rewind` binary.
pub const REWIND_VERSION: &str = "Rewind-Version";

/// On `GET /v1/runs/{id}/events`: a reconnecting client resumes after this entry.
pub const LAST_EVENT_ID: &str = "Last-Event-ID";

/// The value of an `Idempotency-Key` header: 1 to 255 visible ASCII characters. The agent tools
/// use the model's tool call id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub const MAX_LEN: usize = 255;

    pub fn new(key: impl Into<String>) -> Result<Self, Error> {
        let key = key.into();
        if key.is_empty() || key.len() > Self::MAX_LEN {
            return Err(Error::invalid(
                "idempotency key",
                format!("{} characters, expected 1 to {}", key.len(), Self::MAX_LEN),
            ));
        }
        if let Some(bad) = key.chars().find(|c| !(' '..='~').contains(c)) {
            return Err(Error::invalid(
                "idempotency key",
                format!("{bad:?} is not a visible ASCII character"),
            ));
        }
        Ok(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for IdempotencyKey {
    type Err = Error;

    fn from_str(key: &str) -> Result<Self, Error> {
        Self::new(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_keys_are_short_visible_ascii() {
        assert!(IdempotencyKey::new("toolu_01A09q90qw90lq917835lq9").is_ok());
        assert!(IdempotencyKey::new("a b").is_ok());
        assert!(IdempotencyKey::new("").is_err());
        assert!(IdempotencyKey::new("x".repeat(256)).is_err());
        assert!(IdempotencyKey::new("line\nbreak").is_err());
        assert!(IdempotencyKey::new("café").is_err());
    }
}
