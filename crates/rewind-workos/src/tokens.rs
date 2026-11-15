//! WorkOS's access tokens, checked locally (`archi/06-storage-and-scale.md`, "Who calls"): RS256
//! against the environment's JWKS, cached and fetched again for an unknown `kid`.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use rewind_core::Timestamp;
use rewind_engine::ports::{AccessClaims, IdentityError, OrganizationId, SessionId, UserId};
use serde::Deserialize;
use serde::de::IgnoredAny;
use workos::{Jwk, JwkSet, JwksHelper};

/// The issuer of WorkOS's access tokens, without a trailing slash (measured on Staging,
/// `archi/08-decisions.md`).
pub const ISSUER: &str = "https://api.workos.com";

/// The clock skew allowed on a token's expiry.
pub const LEEWAY: Duration = Duration::from_secs(30);

/// The least time between two fetches of the JWKS for a token whose key is unknown, so that
/// forged tokens cannot make every request call WorkOS. A key WorkOS rotates in is still found
/// within this time.
pub const JWKS_REFETCH_INTERVAL: Duration = Duration::from_secs(10);

/// The claims read from an access token. `role`, `roles` and `permissions` are not.
#[derive(Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    sid: Option<String>,
    #[serde(default)]
    org_id: Option<String>,
    exp: u64,
    #[serde(default)]
    client_id: Option<String>,
    /// Set on the other tokens WorkOS signs with the same key (agents, widgets).
    #[serde(default)]
    sub_type: Option<IgnoredAny>,
    #[serde(default)]
    sub_profile: Option<String>,
}

pub(crate) struct AccessTokens {
    client_id: String,
    jwks: JwksHelper,
    /// The last key set fetched, used while WorkOS cannot be reached.
    last_known: Mutex<Option<Arc<JwkSet>>>,
    /// When a token with an unknown key last made the adapter fetch the key set again.
    last_refetch: Mutex<Option<Instant>>,
}

impl AccessTokens {
    pub(crate) fn new(client: &workos::Client) -> Self {
        Self {
            client_id: client.client_id().to_owned(),
            jwks: client.jwks(),
            last_known: Mutex::new(None),
            last_refetch: Mutex::new(None),
        }
    }

    pub(crate) async fn verify(&self, token: &str) -> Result<AccessClaims, IdentityError> {
        let header = decode_header(token).map_err(|_| IdentityError::InvalidToken)?;
        if header.alg != Algorithm::RS256 {
            return Err(IdentityError::InvalidToken);
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty())
            .ok_or(IdentityError::InvalidToken)?;
        let keys = self.keys().await?;
        let key = match key(&keys, &kid)? {
            Some(key) => key,
            None => {
                if !self.may_refetch() {
                    return Err(IdentityError::InvalidToken);
                }
                let keys = self.jwks.refresh().await.map_err(jwks_unavailable)?;
                self.remember(&keys);
                key(&keys, &kid)?.ok_or(IdentityError::InvalidToken)?
            }
        };

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_required_spec_claims(&["exp", "iss", "sub"]);
        validation.set_issuer(&[ISSUER]);
        validation.leeway = LEEWAY.as_secs();
        validation.validate_exp = true;
        validation.validate_nbf = true;
        // WorkOS's tokens have no audience: the `client_id` claim says whose they are.
        validation.validate_aud = false;
        let claims = decode::<Claims>(token, &key, &validation)
            .map_err(|_| IdentityError::InvalidToken)?
            .claims;

        if claims.client_id.as_deref() != Some(self.client_id.as_str())
            || claims.sub.is_empty()
            || claims.sub_type.is_some()
            || claims
                .sub_profile
                .as_deref()
                .is_some_and(|profile| profile != "user")
        {
            return Err(IdentityError::InvalidToken);
        }
        let session = claims
            .sid
            .filter(|sid| !sid.is_empty())
            .ok_or(IdentityError::InvalidToken)?;
        Ok(AccessClaims {
            user: UserId::new(claims.sub),
            session: SessionId::new(session),
            organization: claims
                .org_id
                .filter(|org| !org.is_empty())
                .map(OrganizationId::new),
            expires_at: Timestamp::from_millis(claims.exp.saturating_mul(1000)),
        })
    }

    /// The cached key set, fetched when it is older than the helper's lifetime; the last one
    /// fetched while WorkOS cannot be reached.
    async fn keys(&self) -> Result<Arc<JwkSet>, IdentityError> {
        match self.jwks.fetch().await {
            Ok(keys) => {
                self.remember(&keys);
                Ok(keys)
            }
            Err(error) => self
                .last_known
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
                .ok_or_else(|| jwks_unavailable(error)),
        }
    }

    fn remember(&self, keys: &Arc<JwkSet>) {
        *self
            .last_known
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(keys.clone());
    }

    /// Whether an unknown key may fetch the key set again, and if so, records that it does.
    fn may_refetch(&self) -> bool {
        let mut last = self
            .last_refetch
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if last.is_some_and(|at| now.duration_since(at) < JWKS_REFETCH_INTERVAL) {
            return false;
        }
        *last = Some(now);
        true
    }
}

/// The key of `kid`, if the set has exactly one, usable for RS256 signatures.
fn key(keys: &JwkSet, kid: &str) -> Result<Option<DecodingKey>, IdentityError> {
    let mut matching = keys
        .keys
        .iter()
        .filter(|key| key.kid.as_deref() == Some(kid));
    let Some(key) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() || !usable(key) {
        return Err(IdentityError::InvalidToken);
    }
    let component = |name: &str| key.other.get(name).and_then(serde_json::Value::as_str);
    let (Some(modulus), Some(exponent)) = (component("n"), component("e")) else {
        return Err(IdentityError::InvalidToken);
    };
    DecodingKey::from_rsa_components(modulus, exponent)
        .map(Some)
        .map_err(|_| IdentityError::InvalidToken)
}

fn usable(key: &Jwk) -> bool {
    key.kty.as_deref() == Some("RSA")
        && key.alg.as_deref().is_none_or(|alg| alg == "RS256")
        && key.use_.as_deref().is_none_or(|usage| usage == "sig")
}

fn jwks_unavailable(error: workos::Error) -> IdentityError {
    let reason = match &error {
        workos::Error::Api(api) => format!("it answered {}", api.status),
        workos::Error::Network(_) => "it cannot be reached".to_owned(),
        workos::Error::Decode(_) => "its body cannot be read".to_owned(),
        other => other.to_string(),
    };
    IdentityError::Unavailable(format!("WorkOS's keys cannot be fetched: {reason}"))
}
