//! Authentication and scopes (`archi/06-storage-and-scale.md`, "Tenancy and authentication").
//!
//! Every request to `/v1` carries an API key as a Bearer token, `ptc_sk_…`. The store keeps
//! the SHA-256 of keys, never a key. A key has a tenant, which scopes everything it sees, a
//! role, which says which endpoints it may call, and optionally allowed labels: it then only
//! sees the runs carrying them.
//!
//! | Role | May call |
//! | --- | --- |
//! | `admin` | everything, and the API keys of every tenant |
//! | `harness` | everything of its tenant but the API keys |
//! | `worker` | the host-tool task endpoints |
//! | `reader` | the `GET` endpoints of its tenant |
//!
//! The embedded server's socket takes no key: its one tenant is `local`, with the `admin` role.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::header;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ptc_api::ProblemType;
use ptc_api::admin::Role;
use ptc_api::headers::API_KEY_PREFIX;
use ptc_core::Digest;
use ptc_engine::ports::{JournalStore, TenantId};

use crate::app::App;
use crate::error::ApiError;

/// Who calls: the key's tenant, role and allowed labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Caller {
    pub(crate) tenant: TenantId,
    pub(crate) role: Role,
    /// Not empty: the caller only sees the runs carrying all of these labels.
    pub(crate) labels: BTreeMap<String, String>,
    /// The key's id, the hex of its hash; `None` over the embedded server's socket.
    pub(crate) key_id: Option<String>,
}

/// What an endpoint requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Reading the tenant's data: `admin`, `harness`, `reader`.
    Read,
    /// Changing the tenant's data: `admin`, `harness`.
    Write,
    /// The host-tool task endpoints: `admin`, `harness`, `worker`.
    Tasks,
    /// API keys: `admin`.
    Admin,
}

impl Caller {
    /// The one caller of the embedded server's socket.
    pub(crate) fn local() -> Self {
        Self {
            tenant: TenantId::local(),
            role: Role::Admin,
            labels: BTreeMap::new(),
            key_id: None,
        }
    }

    /// Refuses the request (403) unless the role allows it.
    pub(crate) fn allow(&self, access: Access) -> Result<(), ApiError> {
        let allowed = match access {
            Access::Read => matches!(self.role, Role::Admin | Role::Harness | Role::Reader),
            Access::Write => matches!(self.role, Role::Admin | Role::Harness),
            Access::Tasks => matches!(self.role, Role::Admin | Role::Harness | Role::Worker),
            Access::Admin => self.role == Role::Admin,
        };
        if allowed {
            return Ok(());
        }
        let needs = match access {
            Access::Read => "a key with the role reader, harness or admin",
            Access::Write => "a key with the role harness or admin",
            Access::Tasks => "a key with the role worker, harness or admin",
            Access::Admin => "a key with the role admin",
        };
        Err(ApiError::forbidden(format!(
            "this key has the role {}: this request needs {needs}",
            self.role.as_str()
        )))
    }

    /// Whether the caller sees a run carrying `labels`.
    pub(crate) fn sees(&self, labels: &BTreeMap<String, String>) -> bool {
        self.labels
            .iter()
            .all(|(key, value)| labels.get(key) == Some(value))
    }

    /// Whether the key is limited to labels.
    pub(crate) fn scoped(&self) -> bool {
        !self.labels.is_empty()
    }

    /// The labels of something the caller creates (a run, a subscription): `labels`, with the
    /// key's allowed labels added. A label that contradicts the key's is refused.
    pub(crate) fn within(
        &self,
        mut labels: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, ApiError> {
        for (key, value) in &self.labels {
            match labels.get(key) {
                Some(given) if given != value => {
                    return Err(ApiError::forbidden(format!(
                        "this key only sees runs labelled {key}:{value}, not {key}:{given}"
                    )));
                }
                Some(_) => {}
                None => {
                    labels.insert(key.clone(), value.clone());
                }
            }
        }
        Ok(labels)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        parts
            .extensions
            .get::<Caller>()
            .cloned()
            .ok_or_else(|| ApiError::new(ProblemType::Unauthorized, "no API key"))
    }
}

/// The SHA-256 of a key: how the store finds it.
pub(crate) fn key_hash(key: &str) -> Digest {
    Digest::of(key.as_bytes())
}

/// A new key: `ptc_sk_` and 256 bits from the operating system's CSPRNG, in base 62.
pub(crate) fn new_key() -> Result<String, ApiError> {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut bytes = [0u8; 43];
    getrandom::fill(&mut bytes).map_err(ApiError::internal)?;
    // 43 characters of base 62 hold 256 bits; the bias of `% 62` on a byte is below 2 %.
    let key: String = bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect();
    Ok(format!("{API_KEY_PREFIX}{key}"))
}

/// The middleware of the TCP listener: reads the Bearer key and puts its [`Caller`] in the
/// request's extensions, or answers 401.
pub(crate) async fn authenticate(
    State(app): State<Arc<App>>,
    mut request: Request,
    next: Next,
) -> Response {
    match caller(&app, request.headers()).await {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}

async fn caller(app: &App, headers: &axum::http::HeaderMap) -> Result<Caller, ApiError> {
    let unauthorized = |detail: &str| ApiError::new(ProblemType::Unauthorized, detail);
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err(unauthorized(
            "no API key: send it as `Authorization: Bearer ptc_sk_…`",
        ));
    };
    let key = value
        .to_str()
        .ok()
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|key| key.starts_with(API_KEY_PREFIX))
        .ok_or_else(|| unauthorized("the Authorization header must be `Bearer ptc_sk_…`"))?;
    let hash = key_hash(key);
    let stored = app.store.find_api_key(hash).await?;
    let Some(stored) = stored.filter(|stored| stored.revoked_at.is_none()) else {
        return Err(unauthorized("this API key is unknown or revoked"));
    };
    let role = Role::parse(&stored.role).ok_or_else(|| {
        ApiError::internal(format!("the key {} has an unknown role", hash.short()))
    })?;
    Ok(Caller {
        tenant: stored.tenant,
        role,
        labels: stored.allowed_labels,
        key_id: Some(hash.to_hex()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_long_and_prefixed() {
        let key = new_key().unwrap_or_default();
        assert!(key.starts_with("ptc_sk_"));
        assert_eq!(key.len(), 7 + 43);
        assert_ne!(new_key().unwrap_or_default(), key);
    }

    #[test]
    fn roles_allow_their_endpoints() {
        let reader = Caller {
            role: Role::Reader,
            ..Caller::local()
        };
        assert!(reader.allow(Access::Read).is_ok());
        assert!(reader.allow(Access::Write).is_err());
        assert!(reader.allow(Access::Tasks).is_err());
        let worker = Caller {
            role: Role::Worker,
            ..Caller::local()
        };
        assert!(worker.allow(Access::Tasks).is_ok());
        assert!(worker.allow(Access::Read).is_err());
        assert!(Caller::local().allow(Access::Admin).is_ok());
    }

    #[test]
    fn labels_scope_what_a_key_creates() {
        let scoped = Caller {
            labels: BTreeMap::from([("conversation".into(), "c-1".into())]),
            ..Caller::local()
        };
        let labels = scoped
            .within(BTreeMap::from([("agent".into(), "triage".into())]))
            .unwrap_or_default();
        assert_eq!(labels.len(), 2);
        assert!(scoped.sees(&labels));
        assert!(
            scoped
                .within(BTreeMap::from([("conversation".into(), "c-2".into())]))
                .is_err()
        );
        assert!(!scoped.sees(&BTreeMap::new()));
    }
}
