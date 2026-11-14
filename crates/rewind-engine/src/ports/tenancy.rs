//! Tenants and the runs that belong to them (`archi/06-storage-and-scale.md`, "Tenancy, identity
//! and access"; "Retention and deletion").
//!
//! The tenant is run metadata, never part of the core: it lives here, with the ports. It is
//! part of every storage key, and every method of a store is scoped by it.

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use rewind_core::RunId;

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

/// Which tenants a worker serves. Workers run in pools per tenant when needed, and trusted
/// tenants share a pool (`archi/06-storage-and-scale.md`, "Sandboxed workers").
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum TenantScope {
    #[default]
    All,
    Only(BTreeSet<TenantId>),
    Except(BTreeSet<TenantId>),
}

impl TenantScope {
    pub fn contains(&self, tenant: &TenantId) -> bool {
        match self {
            TenantScope::All => true,
            TenantScope::Only(tenants) => tenants.contains(tenant),
            TenantScope::Except(tenants) => !tenants.contains(tenant),
        }
    }
}

/// A tenant's settings. A tenant without a record has no quota and the default retention: the
/// embedded server's tenant `local`, typically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tenant {
    pub id: TenantId,
    /// The most runs that may be active at once; starting one more is refused (429).
    pub active_run_quota: Option<u32>,
    pub retention: Retention,
}

impl Tenant {
    /// A tenant without quota, with the default retention.
    pub fn new(id: TenantId) -> Self {
        Self {
            id,
            active_run_quota: None,
            retention: Retention::default(),
        }
    }
}

/// How long the data of a finished run is kept (`archi/06-storage-and-scale.md`, "Retention
/// and deletion"). Active runs are never touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// A finished run's journal stays in the store this long after the run ended; then it is
    /// archived as a journal document. 30 days by default.
    pub journal: Duration,
    /// An archived journal is kept this long after it was archived; then the run is deleted.
    /// 1 year by default.
    pub archive: Duration,
    /// The read model keeps a run this long after it ended. 90 days by default.
    pub read_model: Duration,
    /// A payload no run references any more is deleted after this long. 24 hours by default.
    pub payload_grace: Duration,
}

impl Default for Retention {
    fn default() -> Self {
        const DAY: u64 = 86_400;
        Self {
            journal: Duration::from_secs(30 * DAY),
            archive: Duration::from_secs(365 * DAY),
            read_model: Duration::from_secs(90 * DAY),
            payload_grace: Duration::from_secs(DAY),
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

    #[test]
    fn scopes_select_tenants() {
        let acme = TenantId::new("acme").unwrap();
        let other = TenantId::new("other").unwrap();
        assert!(TenantScope::All.contains(&acme));
        let only = TenantScope::Only(BTreeSet::from([acme.clone()]));
        assert!(only.contains(&acme) && !only.contains(&other));
        let except = TenantScope::Except(BTreeSet::from([acme.clone()]));
        assert!(!except.contains(&acme) && except.contains(&other));
    }
}
