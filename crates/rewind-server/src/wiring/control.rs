//! The control plane (`archi/06-storage-and-scale.md`, "Backends", "The control plane"): the
//! catalog, access, secrets and the cluster on one backend, chosen by the configuration:
//!
//! | `--store` | Backend |
//! | --- | --- |
//! | `turso:<path>` | `rewind-store-turso`: the embedded server, a single node |
//! | `postgres://…` | `rewind-store-postgres`: production, several nodes on one database |
//!
//! The server's tests also run it on the engine's in-memory adapters ([`MemoryControlPlane`]),
//! through `rewind_server::start_in_memory`: the reference implementations of the ports'
//! contracts, sharing one broadcast of the control plane's changes.
//!
//! [`ControlPlane`] is `AccessStore` and `SecretStore`; [`Catalog`] is its `CatalogStore`,
//! [`ServerCluster`] its `Cluster`. Each method is handed to the backend as it is: the enums only
//! pick it.

#![allow(clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use rewind_core::{ActionSet, Code, Digest, PartitionId, RunId, Timestamp, Workflow};
use rewind_engine::adapters::{
    ControlPlaneChanges, MemoryAccess, MemoryCatalog, MemoryCluster, MemoryMembership,
    MemorySecrets,
};
use rewind_engine::ports::catalog::{NamespaceFilter, NamespacePage};
use rewind_engine::ports::cluster::Membership;
use rewind_engine::ports::{
    AccessStore, ActionDeclarations, ApiKey, CatalogError, CatalogStore, ClientKind, Cluster,
    ClusterError, CodeRedemption, CompiledCode, Connection, ConnectionToken, ControlPlaneChange,
    Day, DeleteNamespace, Departed, DisplayName, DropNamespace, Grant, GrantClaim, GrantDeletion,
    GrantWrite, MasterKeyId, Moves, Namespace, NamespaceName, NamespaceWrite, NewConnection,
    NewSecret, NewToken, NodeId, NodeInfo, OAuthClient, OAuthCode, OpenedOrganization,
    PartitionMap, Principal, PublicKey, PutSecret, Quotas, Role, RoleDeletion, Rotation, RunRef,
    ScriptTitle, Secret, SecretError, SecretId, SecretMeta, SecretNamespace, SecretReader,
    SecretStore, SigningKey, StoreError, StoredActionSet, StoredCode, StoredWorkflow, Subscription,
    Tenant, TenantId, TestRecord, Token, TokenKey, UserId, WorkflowFilter,
};
use rewind_store_postgres::{PostgresCatalog, PostgresCluster, PostgresMembership, PostgresStore};
use rewind_store_turso::{TursoCatalog, TursoCluster, TursoMembership, TursoStore};

use crate::config::{ConfigError, MasterKeyRing};

/// Where the control plane lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreLocation {
    /// A Turso database file.
    Turso(PathBuf),
    /// A Postgres connection string.
    Postgres(String),
    /// The engine's in-memory adapters, for the server's tests: never read from `--store`.
    Memory,
}

impl StoreLocation {
    /// Reads `--store`: `turso:<path>`, or `postgres://…`.
    pub(crate) fn parse(text: &str) -> Result<Self, ConfigError> {
        if text.starts_with("postgres://") || text.starts_with("postgresql://") {
            return Ok(Self::Postgres(text.to_owned()));
        }
        if let Some(path) = text.strip_prefix("turso:") {
            if path.is_empty() {
                return Err(ConfigError::new(
                    "--store turso: needs a path: turso:<path>",
                ));
            }
            return Ok(Self::Turso(PathBuf::from(path)));
        }
        Err(ConfigError::new(
            "--store: expected turso:<path> or postgres://… (the value is not printed: it may \
             hold a password)",
        ))
    }
}

/// The control plane of this server: access and secrets, and the catalog and cluster it gives.
#[derive(Debug, Clone)]
pub(crate) enum ControlPlane {
    Turso(TursoStore),
    Postgres(PostgresStore),
    Memory(Arc<MemoryControlPlane>),
}

/// The engine's in-memory adapters of the control plane, one broadcast of changes between them,
/// and the virtual time a server on them keeps while it runs.
#[derive(Debug)]
pub(crate) struct MemoryControlPlane {
    changes: ControlPlaneChanges,
    catalog: Arc<MemoryCatalog>,
    access: MemoryAccess,
    secrets: MemorySecrets,
    cluster: Mutex<Option<MemoryCluster>>,
    virtual_time: Mutex<Option<Timestamp>>,
}

impl Default for MemoryControlPlane {
    fn default() -> Self {
        let changes = ControlPlaneChanges::new();
        Self {
            catalog: Arc::new(MemoryCatalog::with_changes(changes.clone())),
            access: MemoryAccess::with_changes(changes.clone()),
            secrets: MemorySecrets::with_changes(changes.clone()),
            cluster: Mutex::new(None),
            virtual_time: Mutex::new(None),
            changes,
        }
    }
}

impl ControlPlane {
    /// Opens the control plane at `location`, whose secrets are encrypted under `master_keys`.
    pub(crate) async fn open(
        location: &StoreLocation,
        master_keys: &MasterKeyRing,
    ) -> Result<Self, ConfigError> {
        match location {
            StoreLocation::Turso(path) => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        ConfigError::new(format!("cannot create {}: {error}", parent.display()))
                    })?;
                }
                use rewind_store_turso::{MasterKey, MasterKeys};
                let keys = MasterKeys::new(MasterKey::from_bytes(master_keys.current))
                    .with_previous(
                        master_keys
                            .previous
                            .iter()
                            .copied()
                            .map(MasterKey::from_bytes),
                    );
                let store = TursoStore::open(path, keys).await.map_err(|error| {
                    ConfigError::new(format!("cannot open {}: {error}", path.display()))
                })?;
                Ok(Self::Turso(store))
            }
            StoreLocation::Postgres(url) => {
                use rewind_store_postgres::{MasterKey, MasterKeys};
                let keys = MasterKeys::new(MasterKey::from_bytes(master_keys.current))
                    .with_previous(
                        master_keys
                            .previous
                            .iter()
                            .copied()
                            .map(MasterKey::from_bytes),
                    );
                let store = PostgresStore::connect(&rewind_store_postgres::Config::new(url), keys)
                    .await
                    .map_err(|error| ConfigError::new(format!("Postgres: {error}")))?;
                Ok(Self::Postgres(store))
            }
            StoreLocation::Memory => Ok(Self::Memory(Arc::new(MemoryControlPlane::default()))),
        }
    }

    /// The catalog on the same backend.
    pub(crate) fn catalog(&self) -> Catalog {
        match self {
            ControlPlane::Turso(store) => Catalog::Turso(store.catalog()),
            ControlPlane::Postgres(store) => Catalog::Postgres(store.catalog()),
            ControlPlane::Memory(memory) => Catalog::Memory(memory.catalog.clone()),
        }
    }

    /// Sets the partition of every namespace created without one, before the catalog serves
    /// (`archi/08-decisions.md`, "Taken while building the control plane of the run plane").
    pub(crate) async fn place_namespaces(
        &self,
        partitions: NonZeroU16,
    ) -> Result<u64, CatalogError> {
        match self {
            ControlPlane::Turso(store) => store.catalog().place_namespaces(partitions).await,
            ControlPlane::Postgres(store) => store.catalog().place_namespaces(partitions).await,
            ControlPlane::Memory(_) => Ok(0),
        }
    }

    /// The cluster of a deployment of `partitions` partitions on the same backend; on Postgres,
    /// the nodes' liveness locks are taken in `lock_database`.
    pub(crate) async fn cluster(
        &self,
        partitions: NonZeroU16,
        lock_database: &str,
    ) -> Result<ServerCluster, ClusterError> {
        match self {
            ControlPlane::Turso(store) => Ok(ServerCluster::Turso(
                TursoCluster::open(store, partitions).await?,
            )),
            ControlPlane::Postgres(store) => Ok(ServerCluster::Postgres(
                PostgresCluster::open_in(store, partitions, lock_database).await?,
            )),
            ControlPlane::Memory(memory) => {
                let mut cluster = memory
                    .cluster
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let cluster = cluster.get_or_insert_with(|| {
                    MemoryCluster::with_changes(partitions.get(), memory.changes.clone())
                });
                Ok(ServerCluster::Memory(cluster.clone()))
            }
        }
    }

    /// Closes the store's connections.
    pub(crate) async fn close(&self) {
        if let ControlPlane::Postgres(store) = self {
            store.close().await;
        }
    }

    /// The virtual time saved by a server that runs in virtual time; only Turso keeps it, a
    /// shared Postgres database always runs in real time.
    pub(crate) async fn virtual_time(&self) -> Result<Option<Timestamp>, StoreError> {
        match self {
            ControlPlane::Turso(store) => store
                .virtual_time()
                .await
                .map_err(|error| StoreError::Backend(error.to_string())),
            ControlPlane::Postgres(_) => Ok(None),
            ControlPlane::Memory(store) => Ok(*store
                .virtual_time
                .lock()
                .unwrap_or_else(PoisonError::into_inner)),
        }
    }

    pub(crate) async fn set_virtual_time(&self, at: Option<Timestamp>) -> Result<(), StoreError> {
        match (self, at) {
            (ControlPlane::Turso(store), at) => store
                .set_virtual_time(at)
                .await
                .map_err(|error| StoreError::Backend(error.to_string())),
            (ControlPlane::Postgres(_), None) => Ok(()),
            (ControlPlane::Postgres(_), Some(_)) => Err(StoreError::Backend(
                "a server on Postgres runs in real time: virtual time is for Turso".into(),
            )),
            (ControlPlane::Memory(store), at) => {
                *store
                    .virtual_time
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = at;
                Ok(())
            }
        }
    }
}

/// Hands a call of one port to the backend: Turso and Postgres implement access and secrets on
/// one type, the in-memory control plane has one adapter per port (`$port`).
macro_rules! each {
    ($port:ident, $self:ident, $store:ident => $call:expr) => {
        match $self {
            ControlPlane::Turso($store) => $call,
            ControlPlane::Postgres($store) => $call,
            ControlPlane::Memory(memory) => {
                let $store = &memory.$port;
                $call
            }
        }
    };
}

macro_rules! access {
    ($self:ident, $store:ident => $call:expr) => {
        each!(access, $self, $store => $call)
    };
}

macro_rules! secrets {
    ($self:ident, $store:ident => $call:expr) => {
        each!(secrets, $self, $store => $call)
    };
}

impl AccessStore for ControlPlane {
    async fn put_key(&self, key: &ApiKey) -> Result<(), StoreError> {
        access!(self, store => store.put_key(key).await)
    }

    async fn find_key(&self, hash: Digest) -> Result<Option<ApiKey>, StoreError> {
        access!(self, store => store.find_key(hash).await)
    }

    async fn list_keys(&self, tenant: &TenantId) -> Result<Vec<ApiKey>, StoreError> {
        access!(self, store => store.list_keys(tenant).await)
    }

    async fn revoke_key(&self, hash: Digest, now: Timestamp) -> Result<bool, StoreError> {
        access!(self, store => store.revoke_key(hash, now).await)
    }

    async fn put_token(&self, token: &Token) -> Result<(), StoreError> {
        access!(self, store => store.put_token(token).await)
    }

    async fn find_token(&self, hash: Digest) -> Result<Option<Token>, StoreError> {
        access!(self, store => store.find_token(hash).await)
    }

    async fn revoke_token(
        &self,
        tenant: &TenantId,
        hash: Digest,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        access!(self, store => store.revoke_token(tenant, hash, now).await)
    }

    async fn revoke_tokens(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        now: Timestamp,
    ) -> Result<u64, StoreError> {
        access!(self, store => store.revoke_tokens(tenant, namespace, now).await)
    }

    async fn count_tokens(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        now: Timestamp,
    ) -> Result<u64, StoreError> {
        access!(self, store => store.count_tokens(tenant, namespace, now).await)
    }

    async fn purge_tokens(&self, before: Timestamp, limit: usize) -> Result<u64, StoreError> {
        access!(self, store => store.purge_tokens(before, limit).await)
    }

    async fn put_token_key(&self, key: &TokenKey) -> Result<(), StoreError> {
        access!(self, store => store.put_token_key(key).await)
    }

    async fn find_token_key(&self, kid: &str) -> Result<Option<TokenKey>, StoreError> {
        access!(self, store => store.find_token_key(kid).await)
    }

    async fn list_token_keys(&self, tenant: &TenantId) -> Result<Vec<TokenKey>, StoreError> {
        access!(self, store => store.list_token_keys(tenant).await)
    }

    async fn delete_token_key(&self, tenant: &TenantId, kid: &str) -> Result<bool, StoreError> {
        access!(self, store => store.delete_token_key(tenant, kid).await)
    }

    async fn put_role(&self, role: &Role) -> Result<(), StoreError> {
        access!(self, store => store.put_role(role).await)
    }

    async fn get_role(&self, tenant: &TenantId, name: &str) -> Result<Option<Role>, StoreError> {
        access!(self, store => store.get_role(tenant, name).await)
    }

    async fn list_roles(&self, tenant: &TenantId) -> Result<Vec<Role>, StoreError> {
        access!(self, store => store.list_roles(tenant).await)
    }

    async fn delete_role(&self, tenant: &TenantId, name: &str) -> Result<RoleDeletion, StoreError> {
        access!(self, store => store.delete_role(tenant, name).await)
    }

    async fn put_grant(&self, grant: &Grant, keep: Option<&str>) -> Result<GrantWrite, StoreError> {
        access!(self, store => store.put_grant(grant, keep).await)
    }

    async fn grant(
        &self,
        tenant: &TenantId,
        principal: &Principal,
    ) -> Result<Option<Grant>, StoreError> {
        access!(self, store => store.grant(tenant, principal).await)
    }

    async fn list_grants(&self, tenant: &TenantId) -> Result<Vec<Grant>, StoreError> {
        access!(self, store => store.list_grants(tenant).await)
    }

    async fn delete_grant(
        &self,
        tenant: &TenantId,
        principal: &Principal,
        keep: Option<&str>,
    ) -> Result<GrantDeletion, StoreError> {
        access!(self, store => store.delete_grant(tenant, principal, keep).await)
    }

    async fn claim_grant(
        &self,
        tenant: &TenantId,
        invitation: &Principal,
        user: &Principal,
    ) -> Result<GrantClaim, StoreError> {
        access!(self, store => store.claim_grant(tenant, invitation, user).await)
    }

    async fn set_default_role(&self, tenant: &TenantId, role: &str) -> Result<(), StoreError> {
        access!(self, store => store.set_default_role(tenant, role).await)
    }

    async fn default_role(&self, tenant: &TenantId) -> Result<Option<String>, StoreError> {
        access!(self, store => store.default_role(tenant).await)
    }

    async fn put_oauth_client(&self, client: &OAuthClient) -> Result<(), StoreError> {
        access!(self, store => store.put_oauth_client(client).await)
    }

    async fn oauth_client(&self, id: &str) -> Result<Option<OAuthClient>, StoreError> {
        access!(self, store => store.oauth_client(id).await)
    }

    async fn count_oauth_clients(&self, kind: ClientKind) -> Result<u64, StoreError> {
        access!(self, store => store.count_oauth_clients(kind).await)
    }

    async fn purge_oauth_clients(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, StoreError> {
        access!(self, store => store.purge_oauth_clients(before, limit).await)
    }

    async fn put_oauth_code(&self, code: &OAuthCode) -> Result<(), StoreError> {
        access!(self, store => store.put_oauth_code(code).await)
    }

    async fn oauth_code(&self, hash: Digest) -> Result<Option<OAuthCode>, StoreError> {
        access!(self, store => store.oauth_code(hash).await)
    }

    async fn redeem_oauth_code(
        &self,
        hash: Digest,
        new: &NewConnection,
        now: Timestamp,
    ) -> Result<CodeRedemption, StoreError> {
        access!(self, store => store.redeem_oauth_code(hash, new, now).await)
    }

    async fn purge_oauth_codes(&self, before: Timestamp, limit: usize) -> Result<u64, StoreError> {
        access!(self, store => store.purge_oauth_codes(before, limit).await)
    }

    async fn connection(&self, id: &str) -> Result<Option<Connection>, StoreError> {
        access!(self, store => store.connection(id).await)
    }

    async fn connections_of(&self, user: &UserId) -> Result<Vec<Connection>, StoreError> {
        access!(self, store => store.connections_of(user).await)
    }

    async fn connection_grants(&self, id: &str) -> Result<Vec<Grant>, StoreError> {
        access!(self, store => store.connection_grants(id).await)
    }

    async fn set_connection_organizations(
        &self,
        id: &str,
        organizations: &[OpenedOrganization],
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        access!(self, store => store.set_connection_organizations(id, organizations, now).await)
    }

    async fn touch_connection(&self, id: &str, now: Timestamp) -> Result<(), StoreError> {
        access!(self, store => store.touch_connection(id, now).await)
    }

    async fn revoke_connection(&self, id: &str, now: Timestamp) -> Result<bool, StoreError> {
        access!(self, store => store.revoke_connection(id, now).await)
    }

    async fn purge_connections(
        &self,
        lapsed_before: Timestamp,
        revoked_before: Timestamp,
        limit: usize,
    ) -> Result<u64, StoreError> {
        access!(self, store => store.purge_connections(lapsed_before, revoked_before, limit).await)
    }

    async fn connection_token(&self, hash: Digest) -> Result<Option<ConnectionToken>, StoreError> {
        access!(self, store => store.connection_token(hash).await)
    }

    async fn rotate_refresh_token(
        &self,
        hash: Digest,
        access: &NewToken,
        refresh: &NewToken,
        now: Timestamp,
    ) -> Result<Rotation, StoreError> {
        access!(self, store => store.rotate_refresh_token(hash, access, refresh, now).await)
    }

    async fn purge_connection_tokens(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, StoreError> {
        access!(self, store => store.purge_connection_tokens(before, limit).await)
    }
}

impl SecretStore for ControlPlane {
    async fn put(
        &self,
        tenant: &TenantId,
        secret: &NewSecret,
        now: Timestamp,
    ) -> Result<PutSecret, SecretError> {
        secrets!(self, store => store.put(tenant, secret, now).await)
    }

    async fn list(
        &self,
        tenant: &TenantId,
        namespace: &SecretNamespace,
        labels: &BTreeMap<String, String>,
        since: Day,
    ) -> Result<Vec<SecretMeta>, SecretError> {
        secrets!(self, store => store.list(tenant, namespace, labels, since).await)
    }

    async fn meta(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
        since: Day,
    ) -> Result<Option<SecretMeta>, SecretError> {
        secrets!(self, store => store.meta(tenant, secret, since).await)
    }

    async fn read(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
        reader: &SecretReader,
        now: Timestamp,
    ) -> Result<Option<Secret>, SecretError> {
        secrets!(self, store => store.read(tenant, secret, reader, now).await)
    }

    async fn delete(&self, tenant: &TenantId, secret: &SecretId) -> Result<bool, SecretError> {
        secrets!(self, store => SecretStore::delete(store, tenant, secret).await)
    }

    async fn delete_namespace(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
    ) -> Result<usize, SecretError> {
        secrets!(self, store => SecretStore::delete_namespace(store, tenant, namespace).await)
    }

    async fn signing_key(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, SecretError> {
        secrets!(self, store => store.signing_key(tenant, now).await)
    }

    async fn rotate_signing_key(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, SecretError> {
        secrets!(self, store => store.rotate_signing_key(tenant, now).await)
    }

    async fn public_keys(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> Result<Vec<PublicKey>, SecretError> {
        secrets!(self, store => store.public_keys(tenant, now).await)
    }

    async fn master_keys_in_use(&self) -> Result<Vec<MasterKeyId>, SecretError> {
        secrets!(self, store => store.master_keys_in_use().await)
    }

    async fn rewrap_data_keys(&self) -> Result<usize, SecretError> {
        secrets!(self, store => store.rewrap_data_keys().await)
    }
}

/// The catalog of this server's control plane.
#[derive(Debug, Clone)]
pub(crate) enum Catalog {
    Turso(TursoCatalog),
    Postgres(PostgresCatalog),
    Memory(Arc<MemoryCatalog>),
}

macro_rules! catalog {
    ($self:ident, $catalog:ident => $call:expr) => {
        match $self {
            Catalog::Turso($catalog) => $call,
            Catalog::Postgres($catalog) => $call,
            Catalog::Memory($catalog) => $call,
        }
    };
}

impl CatalogStore for Catalog {
    async fn put_code(
        &self,
        tenant: &TenantId,
        code: &Code,
        compiled: &CompiledCode,
    ) -> Result<(), CatalogError> {
        catalog!(self, catalog => catalog.put_code(tenant, code, compiled).await)
    }

    async fn get_script(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredCode>, CatalogError> {
        catalog!(self, catalog => catalog.get_script(tenant, digest).await)
    }

    async fn set_script_title(
        &self,
        tenant: &TenantId,
        code: Digest,
        title: Option<&ScriptTitle>,
    ) -> Result<bool, CatalogError> {
        catalog!(self, catalog => catalog.set_script_title(tenant, code, title).await)
    }

    async fn script_titles(
        &self,
        tenant: &TenantId,
        scripts: &[Digest],
    ) -> Result<Vec<(Digest, ScriptTitle)>, CatalogError> {
        catalog!(self, catalog => catalog.script_titles(tenant, scripts).await)
    }

    async fn put_action_set(
        &self,
        tenant: &TenantId,
        action_set: &ActionSet,
        declarations: &ActionDeclarations,
    ) -> Result<(), CatalogError> {
        catalog!(self, catalog => catalog.put_action_set(tenant, action_set, declarations).await)
    }

    async fn get_action_set(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredActionSet>, CatalogError> {
        catalog!(self, catalog => catalog.get_action_set(tenant, digest).await)
    }

    async fn list_action_sets(
        &self,
        tenant: &TenantId,
    ) -> Result<Vec<StoredActionSet>, CatalogError> {
        catalog!(self, catalog => catalog.list_action_sets(tenant).await)
    }

    async fn put_workflow(
        &self,
        tenant: &TenantId,
        workflow: &Workflow,
        now: Timestamp,
    ) -> Result<StoredWorkflow, CatalogError> {
        catalog!(self, catalog => catalog.put_workflow(tenant, workflow, now).await)
    }

    async fn get_workflow(
        &self,
        tenant: &TenantId,
        version: Digest,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        catalog!(self, catalog => catalog.get_workflow(tenant, version).await)
    }

    async fn workflow_number(
        &self,
        tenant: &TenantId,
        name: &str,
        number: u32,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        catalog!(self, catalog => catalog.workflow_number(tenant, name, number).await)
    }

    async fn workflow_versions(
        &self,
        tenant: &TenantId,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        catalog!(self, catalog => catalog.workflow_versions(tenant, name, labels).await)
    }

    async fn workflows_with_code(
        &self,
        tenant: &TenantId,
        code: Digest,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        catalog!(self, catalog => catalog.workflows_with_code(tenant, code, labels).await)
    }

    async fn latest_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        catalog!(self, catalog => catalog.latest_workflows(filter).await)
    }

    async fn reserve_tests(
        &self,
        tenant: &TenantId,
        tests: &[TestRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, CatalogError> {
        catalog!(self, catalog => catalog.reserve_tests(tenant, tests, now).await)
    }

    async fn get_test(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<TestRecord>, CatalogError> {
        catalog!(self, catalog => catalog.get_test(tenant, digest).await)
    }

    async fn namespace_tests(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        limit: usize,
    ) -> Result<Vec<Digest>, CatalogError> {
        catalog!(self, catalog => catalog.namespace_tests(tenant, namespace, limit).await)
    }

    async fn unreferenced_tests(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<(TenantId, Digest)>, CatalogError> {
        catalog!(self, catalog => catalog.unreferenced_tests(now, limit).await)
    }

    async fn drop_tests(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> Result<usize, CatalogError> {
        catalog!(self, catalog => catalog.drop_tests(tests, erased).await)
    }

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), CatalogError> {
        catalog!(self, catalog => catalog.put_subscription(subscription).await)
    }

    async fn subscriptions(&self, tenant: &TenantId) -> Result<Vec<Subscription>, CatalogError> {
        catalog!(self, catalog => catalog.subscriptions(tenant).await)
    }

    async fn delete_subscription(&self, tenant: &TenantId, id: &str) -> Result<bool, CatalogError> {
        catalog!(self, catalog => catalog.delete_subscription(tenant, id).await)
    }

    async fn put_tenant(&self, tenant: &Tenant) -> Result<(), CatalogError> {
        catalog!(self, catalog => catalog.put_tenant(tenant).await)
    }

    async fn get_tenant(&self, id: &TenantId) -> Result<Option<Tenant>, CatalogError> {
        catalog!(self, catalog => catalog.get_tenant(id).await)
    }

    async fn list_tenants(&self) -> Result<Vec<Tenant>, CatalogError> {
        catalog!(self, catalog => catalog.list_tenants().await)
    }

    async fn ensure_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, CatalogError> {
        catalog!(self, catalog => catalog.ensure_namespace(tenant, name, partitions, now).await)
    }

    async fn put_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        exceptions: Option<&Quotas>,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, CatalogError> {
        catalog!(self, catalog => catalog.put_namespace(tenant, name, exceptions, partitions, now).await)
    }

    async fn get_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<Option<Namespace>, CatalogError> {
        catalog!(self, catalog => catalog.get_namespace(tenant, name).await)
    }

    async fn set_namespace_display_name(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        display_name: Option<&DisplayName>,
    ) -> Result<Option<NamespaceWrite>, CatalogError> {
        catalog!(self, catalog => catalog.set_namespace_display_name(tenant, name, display_name).await)
    }

    async fn namespace_display_names(
        &self,
        tenant: &TenantId,
    ) -> Result<Vec<(NamespaceName, DisplayName)>, CatalogError> {
        catalog!(self, catalog => catalog.namespace_display_names(tenant).await)
    }

    async fn list_namespaces(
        &self,
        filter: &NamespaceFilter,
    ) -> Result<NamespacePage, CatalogError> {
        catalog!(self, catalog => catalog.list_namespaces(filter).await)
    }

    async fn revoke_namespace_tokens(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<bool, CatalogError> {
        catalog!(self, catalog => catalog.revoke_namespace_tokens(tenant, name, now).await)
    }

    async fn delete_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<DeleteNamespace, CatalogError> {
        catalog!(self, catalog => catalog.delete_namespace(tenant, name, now).await)
    }

    async fn namespaces_to_erase(
        &self,
        limit: usize,
    ) -> Result<Vec<(TenantId, NamespaceName)>, CatalogError> {
        catalog!(self, catalog => catalog.namespaces_to_erase(limit).await)
    }

    async fn drop_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<DropNamespace, CatalogError> {
        catalog!(self, catalog => catalog.drop_namespace(tenant, name).await)
    }

    async fn legacy_partition(&self, run: RunId) -> Result<Option<PartitionId>, CatalogError> {
        catalog!(self, catalog => catalog.legacy_partition(run).await)
    }

    async fn put_legacy_run(
        &self,
        run: &RunRef,
        partition: PartitionId,
    ) -> Result<(), CatalogError> {
        catalog!(self, catalog => catalog.put_legacy_run(run, partition).await)
    }
}

/// The control plane is its catalog too: what the edge and the routes read of workflows,
/// action sets, workflows, tenants, namespaces and subscriptions.
impl CatalogStore for ControlPlane {
    async fn put_code(
        &self,
        tenant: &TenantId,
        code: &Code,
        compiled: &CompiledCode,
    ) -> Result<(), CatalogError> {
        self.catalog().put_code(tenant, code, compiled).await
    }

    async fn get_script(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredCode>, CatalogError> {
        self.catalog().get_script(tenant, digest).await
    }

    async fn set_script_title(
        &self,
        tenant: &TenantId,
        code: Digest,
        title: Option<&ScriptTitle>,
    ) -> Result<bool, CatalogError> {
        self.catalog().set_script_title(tenant, code, title).await
    }

    async fn script_titles(
        &self,
        tenant: &TenantId,
        scripts: &[Digest],
    ) -> Result<Vec<(Digest, ScriptTitle)>, CatalogError> {
        self.catalog().script_titles(tenant, scripts).await
    }

    async fn put_action_set(
        &self,
        tenant: &TenantId,
        action_set: &ActionSet,
        declarations: &ActionDeclarations,
    ) -> Result<(), CatalogError> {
        self.catalog()
            .put_action_set(tenant, action_set, declarations)
            .await
    }

    async fn get_action_set(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredActionSet>, CatalogError> {
        self.catalog().get_action_set(tenant, digest).await
    }

    async fn list_action_sets(
        &self,
        tenant: &TenantId,
    ) -> Result<Vec<StoredActionSet>, CatalogError> {
        self.catalog().list_action_sets(tenant).await
    }

    async fn put_workflow(
        &self,
        tenant: &TenantId,
        workflow: &Workflow,
        now: Timestamp,
    ) -> Result<StoredWorkflow, CatalogError> {
        self.catalog().put_workflow(tenant, workflow, now).await
    }

    async fn get_workflow(
        &self,
        tenant: &TenantId,
        version: Digest,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        self.catalog().get_workflow(tenant, version).await
    }

    async fn workflow_number(
        &self,
        tenant: &TenantId,
        name: &str,
        number: u32,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        self.catalog().workflow_number(tenant, name, number).await
    }

    async fn workflow_versions(
        &self,
        tenant: &TenantId,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        self.catalog().workflow_versions(tenant, name, labels).await
    }

    async fn workflows_with_code(
        &self,
        tenant: &TenantId,
        code: Digest,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        self.catalog()
            .workflows_with_code(tenant, code, labels)
            .await
    }

    async fn latest_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        self.catalog().latest_workflows(filter).await
    }

    async fn reserve_tests(
        &self,
        tenant: &TenantId,
        tests: &[TestRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, CatalogError> {
        self.catalog().reserve_tests(tenant, tests, now).await
    }

    async fn get_test(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<TestRecord>, CatalogError> {
        self.catalog().get_test(tenant, digest).await
    }

    async fn namespace_tests(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        limit: usize,
    ) -> Result<Vec<Digest>, CatalogError> {
        self.catalog()
            .namespace_tests(tenant, namespace, limit)
            .await
    }

    async fn unreferenced_tests(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<(TenantId, Digest)>, CatalogError> {
        self.catalog().unreferenced_tests(now, limit).await
    }

    async fn drop_tests(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> Result<usize, CatalogError> {
        self.catalog().drop_tests(tests, erased).await
    }

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), CatalogError> {
        self.catalog().put_subscription(subscription).await
    }

    async fn subscriptions(&self, tenant: &TenantId) -> Result<Vec<Subscription>, CatalogError> {
        self.catalog().subscriptions(tenant).await
    }

    async fn delete_subscription(&self, tenant: &TenantId, id: &str) -> Result<bool, CatalogError> {
        self.catalog().delete_subscription(tenant, id).await
    }

    async fn put_tenant(&self, tenant: &Tenant) -> Result<(), CatalogError> {
        self.catalog().put_tenant(tenant).await
    }

    async fn get_tenant(&self, id: &TenantId) -> Result<Option<Tenant>, CatalogError> {
        self.catalog().get_tenant(id).await
    }

    async fn list_tenants(&self) -> Result<Vec<Tenant>, CatalogError> {
        self.catalog().list_tenants().await
    }

    async fn ensure_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, CatalogError> {
        self.catalog()
            .ensure_namespace(tenant, name, partitions, now)
            .await
    }

    async fn put_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        exceptions: Option<&Quotas>,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, CatalogError> {
        self.catalog()
            .put_namespace(tenant, name, exceptions, partitions, now)
            .await
    }

    async fn get_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<Option<Namespace>, CatalogError> {
        self.catalog().get_namespace(tenant, name).await
    }

    async fn set_namespace_display_name(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        display_name: Option<&DisplayName>,
    ) -> Result<Option<NamespaceWrite>, CatalogError> {
        self.catalog()
            .set_namespace_display_name(tenant, name, display_name)
            .await
    }

    async fn namespace_display_names(
        &self,
        tenant: &TenantId,
    ) -> Result<Vec<(NamespaceName, DisplayName)>, CatalogError> {
        self.catalog().namespace_display_names(tenant).await
    }

    async fn list_namespaces(
        &self,
        filter: &NamespaceFilter,
    ) -> Result<NamespacePage, CatalogError> {
        self.catalog().list_namespaces(filter).await
    }

    async fn revoke_namespace_tokens(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<bool, CatalogError> {
        self.catalog()
            .revoke_namespace_tokens(tenant, name, now)
            .await
    }

    async fn delete_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<DeleteNamespace, CatalogError> {
        self.catalog().delete_namespace(tenant, name, now).await
    }

    async fn namespaces_to_erase(
        &self,
        limit: usize,
    ) -> Result<Vec<(TenantId, NamespaceName)>, CatalogError> {
        self.catalog().namespaces_to_erase(limit).await
    }

    async fn drop_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<DropNamespace, CatalogError> {
        self.catalog().drop_namespace(tenant, name).await
    }

    async fn legacy_partition(&self, run: RunId) -> Result<Option<PartitionId>, CatalogError> {
        self.catalog().legacy_partition(run).await
    }

    async fn put_legacy_run(
        &self,
        run: &RunRef,
        partition: PartitionId,
    ) -> Result<(), CatalogError> {
        self.catalog().put_legacy_run(run, partition).await
    }
}

/// The cluster of this server's control plane. One value per process: its size does not
/// matter.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum ServerCluster {
    Turso(TursoCluster),
    Postgres(PostgresCluster),
    Memory(MemoryCluster),
}

impl Cluster for ServerCluster {
    type Membership = ServerMembership;

    async fn join(&self, node: NodeInfo) -> Result<ServerMembership, ClusterError> {
        Ok(match self {
            ServerCluster::Turso(cluster) => ServerMembership::Turso(cluster.join(node).await?),
            ServerCluster::Postgres(cluster) => {
                ServerMembership::Postgres(cluster.join(node).await?)
            }
            ServerCluster::Memory(cluster) => ServerMembership::Memory(cluster.join(node).await?),
        })
    }
}

/// The membership of this node in the cluster. One value per process: its size does not
/// matter.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum ServerMembership {
    Turso(TursoMembership),
    Postgres(PostgresMembership),
    Memory(MemoryMembership),
}

macro_rules! member {
    ($self:ident, $member:ident => $call:expr) => {
        match $self {
            ServerMembership::Turso($member) => $call,
            ServerMembership::Postgres($member) => $call,
            ServerMembership::Memory($member) => $call,
        }
    };
}

/// The changes of the control plane, whatever the backend.
pub(crate) type Changes = BoxStream<'static, ControlPlaneChange>;

impl Membership for ServerMembership {
    type Changes = Changes;

    fn node(&self) -> &NodeId {
        member!(self, member => member.node())
    }

    async fn watch(&self) -> Result<Departed, ClusterError> {
        member!(self, member => member.watch().await)
    }

    async fn assign(&self, moves: &Moves) -> Result<PartitionMap, ClusterError> {
        member!(self, member => member.assign(moves).await)
    }

    async fn partition_map(&self) -> Result<PartitionMap, ClusterError> {
        member!(self, member => member.partition_map().await)
    }

    async fn nodes(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        member!(self, member => member.nodes().await)
    }

    async fn changes(&self) -> Result<Changes, ClusterError> {
        member!(self, member => member.changes().await.map(StreamExt::boxed))
    }

    async fn leave(self) -> Result<(), ClusterError> {
        member!(self, member => member.leave().await)
    }
}
