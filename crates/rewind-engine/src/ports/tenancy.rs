//! Tenants and the runs that belong to them (`archi/06-storage-and-scale.md`, "Tenancy, identity
//! and access"; "Retention and deletion").
//!
//! The tenant is run metadata, never part of the core: it lives here, with the ports. It is
//! part of every storage key, and every method of a store is scoped by it.

use std::fmt;
use std::time::Duration;

use rewind_core::RunId;

use super::namespaces::Quotas;

/// The tenant a run, a script, a toolset or a payload belongs to: 1 to 64 ASCII letters,
/// digits, `-` and `_`. It appears in storage keys and blob paths (`{tenant}/sha256/…`), hence
/// the restricted alphabet.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid tenant {0:?}: expected 1 to 64 ASCII letters, digits, - and _")]
pub struct InvalidTenant(pub String);

impl TenantId {
    /// The one implicit tenant of the embedded server (`archi/07-code-architecture.md`,
    /// "Embedded server and CLI").
    pub const LOCAL: &'static str = "local";

    pub fn new(id: impl Into<String>) -> Result<Self, InvalidTenant> {
        let id = id.into();
        let valid = (1..=64).contains(&id.len())
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        if valid {
            Ok(Self(id))
        } else {
            Err(InvalidTenant(id))
        }
    }

    /// The tenant `local`.
    pub fn local() -> Self {
        Self(Self::LOCAL.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A run, with the tenant it belongs to. Run ids are globally unique (UUID v7), but every
/// lookup is scoped by the tenant: a run of one tenant is never visible to another.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunRef {
    pub tenant: TenantId,
    pub id: RunId,
}

impl RunRef {
    pub fn new(tenant: TenantId, id: RunId) -> Self {
        Self { tenant, id }
    }
}

impl fmt::Display for RunRef {
    /// `{tenant}/{run_id}`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.tenant, self.id)
    }
}

/// A tenant's settings. A tenant without a record has no quota and the default retention: the
/// embedded server's tenant `local`, typically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tenant {
    pub id: TenantId,
    /// The ceiling, set by the operator: the customer's offer, on the tenant's totals across its
    /// namespaces (`archi/06-storage-and-scale.md`, "Quotas").
    pub quotas: Quotas,
    /// The limits of every namespace without an exception, set by the customer.
    pub namespace_defaults: Quotas,
    pub retention: Retention,
}

impl Tenant {
    /// A tenant without quota, with the default retention.
    pub fn new(id: TenantId) -> Self {
        Self {
            id,
            quotas: Quotas::default(),
            namespace_defaults: Quotas::default(),
            retention: Retention::default(),
        }
    }
}

/// How long an ended run is kept (`archi/06-storage-and-scale.md`, "Retention and deletion"):
/// its summary, its sealed journals and its payloads, from its end; then a timer erases it.
/// Active runs are never touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// 1 year by default.
    pub runs: Duration,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            runs: Duration::from_secs(365 * 86_400),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_ids_are_safe_in_paths() {
        assert_eq!(
            TenantId::new("acme-prod_2").unwrap().as_str(),
            "acme-prod_2"
        );
        for invalid in ["", "../etc", "a/b", "é", &"x".repeat(65)] {
            assert_eq!(
                TenantId::new(invalid),
                Err(InvalidTenant(invalid.to_owned()))
            );
        }
        assert_eq!(TenantId::local().to_string(), "local");
    }
}
