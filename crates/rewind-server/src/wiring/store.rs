//! The store: `JournalStore`, `AccessStore`, `ReadModel` and `SecretStore` on one backend, chosen
//! by the configuration (`archi/06-storage-and-scale.md`, "Backends"):
//!
//! | `--store` | Backend |
//! | --- | --- |
//! | `turso:<path>` | `rewind-store-turso`: the embedded server, local development |
//! | `postgres://…` | `rewind-store-postgres`: production, several workers on one database |
//!
//! The server's tests also run it on the engine's in-memory adapters ([`MemoryStore`]), through
//! `rewind_server::start_in_memory`: the reference implementations of the ports' contracts.
//!
//! Each method is handed to the backend as it is: the enum only picks it.

#![allow(clippy::too_many_lines)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rewind_core::{
    Completion, Digest, Epoch, HistoryEntry, Script, Seq, Shape, Timestamp, ToolFailure,
    ToolOutcome, Toolset,
};
use rewind_engine::adapters::{MemoryAccess, MemoryJournal, MemoryReadModel, MemorySecrets};
use rewind_engine::ports::{
    AccessStore, AcquireRunnable, Activation, ApiKey, ClaimedTask, CompiledScript, CreateRun, Day,
    Enqueued, ExpiredRuns, FailureGroups, FailureWindow, Failures, FeedConsumer, FeedItem, Grant,
    GrantClaim, GrantDeletion, GrantWrite, InboxItem, JournalStore, Lease, MasterKeyId, NewRun,
    NewSecret, ParkReason, ParkedScope, PayloadRecord, Principal, ProjectedRun, PublicKey,
    PutSecret, QuarantineKind, QuarantineNote, ReadModel, ReadModelError, Role, RoleDeletion,
    RunFilter, RunOrder, RunQuery, RunRecord, RunRef, RunSnapshot, RunStats, RunSummary,
    ScriptFilter, ScriptSummary, SearchCursor, Secret, SecretError, SecretId, SecretMeta,
    SecretNamespace, SecretReader, SecretStore, SecretUsage, SigningKey, StatsWindow, StoreError,
    StoredScript, StoredToolset, Subscription, TaskFilter, TaskProgress, TaskRef, TaskToken,
    TaskUpdate, Tenant, TenantId, TenantScope, ToolDeclarations, ToolStats, ToolStatsFilter,
    ToolTask, WebhookTarget, WebhookToken,
};
use rewind_engine::ports::{
    ClientKind, CodeRedemption, Connection, ConnectionToken, NewConnection, NewToken, OAuthClient,
    OAuthCode, OpenedOrganization, Rotation, UserId,
};
use rewind_engine::ports::{
    DeleteNamespace, DisplayName, DropNamespace, Month, Namespace, NamespaceFilter, NamespaceName,
    NamespacePage, NamespaceWrite, QuotaAlert, QuotaAlertKey, Quotas, Token, TokenKey, Usage,
    UsageDelta, UsageOf,
};
use rewind_store_postgres::PostgresStore;
use rewind_store_turso::TursoStore;
use std::collections::BTreeMap;

use crate::config::{ConfigError, MasterKeyRing};

/// Where the store lives.
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

/// The store of this server.
#[derive(Debug, Clone)]
pub(crate) enum Store {
    Turso(TursoStore),
    Postgres(PostgresStore),
    Memory(Arc<MemoryStore>),
}

/// The engine's in-memory adapters of the four ports, and the virtual time a server on them
/// keeps while it runs.
#[derive(Debug, Default)]
pub(crate) struct MemoryStore {
    journal: MemoryJournal,
    access: MemoryAccess,
    read_model: MemoryReadModel,
    secrets: MemorySecrets,
    virtual_time: Mutex<Option<Timestamp>>,
}

impl Store {
    /// Opens the store at `location`, whose secrets are encrypted under `master_keys`.
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
            StoreLocation::Memory => Ok(Self::Memory(Arc::new(MemoryStore::default()))),
        }
    }

    /// Closes the store's connections.
    pub(crate) async fn close(&self) {
        if let Store::Postgres(store) = self {
            store.close().await;
        }
    }

    /// The virtual time saved by a server that runs in virtual time; only Turso keeps it, a
    /// shared Postgres database always runs in real time.
    pub(crate) async fn virtual_time(&self) -> Result<Option<Timestamp>, StoreError> {
        match self {
            Store::Turso(store) => store
                .virtual_time()
                .await
                .map_err(|error| StoreError::Backend(error.to_string())),
            Store::Postgres(_) => Ok(None),
            Store::Memory(store) => Ok(*store
                .virtual_time
                .lock()
                .unwrap_or_else(PoisonError::into_inner)),
        }
    }

    pub(crate) async fn set_virtual_time(&self, at: Option<Timestamp>) -> Result<(), StoreError> {
        match (self, at) {
            (Store::Turso(store), at) => store
                .set_virtual_time(at)
                .await
                .map_err(|error| StoreError::Backend(error.to_string())),
            (Store::Postgres(_), None) => Ok(()),
            (Store::Postgres(_), Some(_)) => Err(StoreError::Backend(
                "a server on Postgres runs in real time: virtual time is for Turso".into(),
            )),
            (Store::Memory(store), at) => {
                *store
                    .virtual_time
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = at;
                Ok(())
            }
        }
    }
}

/// Hands a call of one port to the backend: Turso and Postgres implement every port on one
/// type, the in-memory store has one adapter per port (`$port`).
macro_rules! each {
    ($port:ident, $self:ident, $store:ident => $call:expr) => {
        match $self {
            Store::Turso($store) => $call,
            Store::Postgres($store) => $call,
            Store::Memory(memory) => {
                let $store = &memory.$port;
                $call
            }
        }
    };
}

macro_rules! journal {
    ($self:ident, $store:ident => $call:expr) => {
        each!(journal, $self, $store => $call)
    };
}

macro_rules! access {
    ($self:ident, $store:ident => $call:expr) => {
        each!(access, $self, $store => $call)
    };
}

macro_rules! read_model {
    ($self:ident, $store:ident => $call:expr) => {
        each!(read_model, $self, $store => $call)
    };
}

macro_rules! secrets {
    ($self:ident, $store:ident => $call:expr) => {
        each!(secrets, $self, $store => $call)
    };
}

impl JournalStore for Store {
    async fn put_script(
        &self,
        tenant: &TenantId,
        script: &Script,
        compiled: &CompiledScript,
    ) -> Result<(), StoreError> {
        journal!(self, store => store.put_script(tenant, script, compiled).await)
    }

    async fn get_script(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredScript>, StoreError> {
        journal!(self, store => store.get_script(tenant, digest).await)
    }

    async fn put_toolset(
        &self,
        tenant: &TenantId,
        toolset: &Toolset,
        declarations: &ToolDeclarations,
    ) -> Result<(), StoreError> {
        journal!(self, store => store.put_toolset(tenant, toolset, declarations).await)
    }

    async fn get_toolset(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredToolset>, StoreError> {
        journal!(self, store => store.get_toolset(tenant, digest).await)
    }

    async fn list_toolsets(&self, tenant: &TenantId) -> Result<Vec<StoredToolset>, StoreError> {
        journal!(self, store => store.list_toolsets(tenant).await)
    }

    async fn put_workflow(
        &self,
        tenant: &TenantId,
        workflow: &rewind_core::Workflow,
        now: Timestamp,
    ) -> Result<rewind_engine::ports::StoredWorkflow, StoreError> {
        journal!(self, store => store.put_workflow(tenant, workflow, now).await)
    }

    async fn get_workflow(
        &self,
        tenant: &TenantId,
        version: Digest,
    ) -> Result<Option<rewind_engine::ports::StoredWorkflow>, StoreError> {
        journal!(self, store => store.get_workflow(tenant, version).await)
    }

    async fn workflow_versions(
        &self,
        tenant: &TenantId,
        name: &str,
        labels: &std::collections::BTreeMap<String, String>,
    ) -> Result<Vec<rewind_engine::ports::StoredWorkflow>, StoreError> {
        journal!(self, store => store.workflow_versions(tenant, name, labels).await)
    }

    async fn workflows_with_script(
        &self,
        tenant: &TenantId,
        script: Digest,
        labels: &std::collections::BTreeMap<String, String>,
    ) -> Result<Vec<rewind_engine::ports::StoredWorkflow>, StoreError> {
        journal!(self, store => store.workflows_with_script(tenant, script, labels).await)
    }

    async fn latest_workflows(
        &self,
        filter: &rewind_engine::ports::WorkflowFilter,
    ) -> Result<Vec<rewind_engine::ports::StoredWorkflow>, StoreError> {
        journal!(self, store => store.latest_workflows(filter).await)
    }

    async fn watch_run(
        &self,
        run: &RunRef,
        watcher: rewind_core::RunId,
        seq: rewind_core::Seq,
    ) -> Result<Option<rewind_engine::ports::Watched>, StoreError> {
        journal!(self, store => store.watch_run(run, watcher, seq).await)
    }

    async fn create_run(
        &self,
        run: NewRun,
        idempotency_key: Option<String>,
    ) -> Result<CreateRun, StoreError> {
        journal!(self, store => store.create_run(run, idempotency_key).await)
    }

    async fn enqueue(
        &self,
        run: &RunRef,
        completion: Completion,
        idempotency_key: Option<String>,
        now: Timestamp,
    ) -> Result<Enqueued, StoreError> {
        journal!(self, store => store.enqueue(run, completion, idempotency_key, now).await)
    }

    async fn get_run(&self, run: &RunRef) -> Result<Option<RunRecord>, StoreError> {
        journal!(self, store => store.get_run(run).await)
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<Vec<RunRecord>, StoreError> {
        journal!(self, store => store.list_runs(filter).await)
    }

    async fn acquire_runnable(
        &self,
        request: &AcquireRunnable<'_>,
    ) -> Result<Option<Lease>, StoreError> {
        journal!(self, store => store.acquire_runnable(request).await)
    }

    async fn acquire_run(
        &self,
        run: &RunRef,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        journal!(self, store => store.acquire_run(run, owner, now, lease_duration).await)
    }

    async fn acquire_quarantined(
        &self,
        owner: &str,
        build: &str,
        epochs: &[Epoch],
        tenants: &TenantScope,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        journal!(self, store => {
            store
                .acquire_quarantined(owner, build, epochs, tenants, now, lease_duration)
                .await
        })
    }

    async fn renew(
        &self,
        lease: &Lease,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<(), StoreError> {
        journal!(self, store => store.renew(lease, now, lease_duration).await)
    }

    async fn load_history(&self, run: &RunRef, from: u64) -> Result<Vec<HistoryEntry>, StoreError> {
        journal!(self, store => store.load_history(run, from).await)
    }

    async fn take_inbox(&self, lease: &Lease) -> Result<Vec<InboxItem>, StoreError> {
        journal!(self, store => store.take_inbox(lease).await)
    }

    async fn commit_activation(
        &self,
        lease: &Lease,
        activation: Activation,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        journal!(self, store => store.commit_activation(lease, activation, now).await)
    }

    async fn release(&self, lease: &Lease) -> Result<(), StoreError> {
        journal!(self, store => store.release(lease).await)
    }

    async fn forget_sticky(&self, run: &RunRef, owner: &str) -> Result<(), StoreError> {
        journal!(self, store => store.forget_sticky(run, owner).await)
    }

    async fn record_failure(
        &self,
        lease: &Lease,
        error: String,
        now: Timestamp,
    ) -> Result<Failures, StoreError> {
        journal!(self, store => store.record_failure(lease, error, now).await)
    }

    async fn quarantine(
        &self,
        lease: &Lease,
        kind: QuarantineKind,
        reason: String,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        journal!(self, store => store.quarantine(lease, kind, reason, now).await)
    }

    async fn release_quarantine(&self, run: &RunRef) -> Result<bool, StoreError> {
        journal!(self, store => store.release_quarantine(run).await)
    }

    async fn fire_due_timers(&self, now: Timestamp, limit: usize) -> Result<usize, StoreError> {
        journal!(self, store => store.fire_due_timers(now, limit).await)
    }

    async fn next_timer(&self) -> Result<Option<Timestamp>, StoreError> {
        journal!(self, store => store.next_timer().await)
    }

    async fn expire_due_runs(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<ExpiredRuns, StoreError> {
        journal!(self, store => store.expire_due_runs(now, limit).await)
    }

    async fn next_deadline(&self) -> Result<Option<Timestamp>, StoreError> {
        journal!(self, store => store.next_deadline().await)
    }

    async fn claim_tool_tasks(
        &self,
        owner: &str,
        filter: &TaskFilter,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<ClaimedTask>, StoreError> {
        journal!(self, store => store.claim_tool_tasks(owner, filter, now, limit).await)
    }

    async fn heartbeat_tool_task(
        &self,
        token: &TaskToken,
        now: Timestamp,
    ) -> Result<Option<Timestamp>, StoreError> {
        journal!(self, store => store.heartbeat_tool_task(token, now).await)
    }

    async fn next_tool_attempt(&self, after: Timestamp) -> Result<Option<Timestamp>, StoreError> {
        journal!(self, store => store.next_tool_attempt(after).await)
    }

    async fn retry_tool_task(
        &self,
        token: &TaskToken,
        next_attempt_at: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        journal!(self, store => store.retry_tool_task(token, next_attempt_at).await)
    }

    async fn defer_tool_task(
        &self,
        token: &TaskToken,
        until: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        journal!(self, store => store.defer_tool_task(token, until).await)
    }

    async fn park_tool_task(
        &self,
        token: &TaskToken,
        reason: ParkReason,
    ) -> Result<TaskUpdate, StoreError> {
        journal!(self, store => store.park_tool_task(token, reason).await)
    }

    async fn complete_tool_task(
        &self,
        token: &TaskToken,
        outcome: ToolOutcome,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        journal!(self, store => store.complete_tool_task(token, outcome, now).await)
    }

    async fn approve_tool_task(
        &self,
        task: &TaskRef,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        journal!(self, store => store.approve_tool_task(task, now).await)
    }

    async fn reject_tool_task(
        &self,
        task: &TaskRef,
        failure: ToolFailure,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        journal!(self, store => store.reject_tool_task(task, failure, now).await)
    }

    async fn resume_parked(
        &self,
        scope: &ParkedScope,
        now: Timestamp,
    ) -> Result<usize, StoreError> {
        journal!(self, store => store.resume_parked(scope, now).await)
    }

    async fn secret_usage(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
        labels: &BTreeMap<String, String>,
        limit: usize,
    ) -> Result<SecretUsage, StoreError> {
        journal!(self, store => store.secret_usage(tenant, secret, labels, limit).await)
    }

    async fn expire_deferred_tool_tasks(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<usize, StoreError> {
        journal!(self, store => store.expire_deferred_tool_tasks(now, limit).await)
    }

    async fn tool_tasks(&self, run: &RunRef) -> Result<Vec<ToolTask>, StoreError> {
        journal!(self, store => store.tool_tasks(run).await)
    }

    async fn tool_task(&self, token: &TaskToken) -> Result<Option<ToolTask>, StoreError> {
        journal!(self, store => store.tool_task(token).await)
    }

    async fn create_webhook_token(
        &self,
        run: &RunRef,
        name: &str,
    ) -> Result<WebhookToken, StoreError> {
        journal!(self, store => store.create_webhook_token(run, name).await)
    }

    async fn resolve_webhook_token(
        &self,
        token: &WebhookToken,
    ) -> Result<Option<WebhookTarget>, StoreError> {
        journal!(self, store => store.resolve_webhook_token(token).await)
    }

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), StoreError> {
        journal!(self, store => store.put_subscription(subscription).await)
    }

    async fn subscriptions(&self, tenant: &TenantId) -> Result<Vec<Subscription>, StoreError> {
        journal!(self, store => store.subscriptions(tenant).await)
    }

    async fn delete_subscription(&self, tenant: &TenantId, id: &str) -> Result<bool, StoreError> {
        journal!(self, store => store.delete_subscription(tenant, id).await)
    }

    async fn get_payload(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<PayloadRecord>, StoreError> {
        journal!(self, store => store.get_payload(tenant, digest).await)
    }

    async fn reserve_payloads(
        &self,
        tenant: &TenantId,
        payloads: &[PayloadRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, StoreError> {
        journal!(self, store => store.reserve_payloads(tenant, payloads, now).await)
    }

    async fn reserve_tests(
        &self,
        tenant: &TenantId,
        tests: &[rewind_engine::ports::TestRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, StoreError> {
        journal!(self, store => store.reserve_tests(tenant, tests, now).await)
    }

    async fn get_test(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<rewind_engine::ports::TestRecord>, StoreError> {
        journal!(self, store => store.get_test(tenant, digest).await)
    }

    async fn namespace_tests(
        &self,
        tenant: &TenantId,
        namespace: &rewind_engine::ports::NamespaceName,
        limit: usize,
    ) -> Result<Vec<Digest>, StoreError> {
        journal!(self, store => store.namespace_tests(tenant, namespace, limit).await)
    }

    async fn unreferenced_tests(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        journal!(self, store => store.unreferenced_tests(now, limit).await)
    }

    async fn drop_tests(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> Result<usize, StoreError> {
        journal!(self, store => store.drop_tests(tests, erased).await)
    }

    async fn unreferenced_payloads(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        journal!(self, store => store.unreferenced_payloads(now, limit).await)
    }

    async fn forget_payloads(
        &self,
        payloads: &[(TenantId, Digest)],
        now: Timestamp,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        journal!(self, store => store.forget_payloads(payloads, now).await)
    }

    async fn drop_payloads(
        &self,
        payloads: &[(TenantId, Digest)],
        started: Timestamp,
    ) -> Result<usize, StoreError> {
        journal!(self, store => store.drop_payloads(payloads, started).await)
    }

    async fn runs_to_archive(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<RunRef>, StoreError> {
        journal!(self, store => store.runs_to_archive(now, limit).await)
    }

    async fn archive_run(
        &self,
        run: &RunRef,
        document: Digest,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        journal!(self, store => store.archive_run(run, document, now).await)
    }

    async fn runs_to_delete(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<RunRef>, StoreError> {
        journal!(self, store => store.runs_to_delete(now, limit).await)
    }

    async fn delete_run(&self, run: &RunRef, now: Timestamp) -> Result<bool, StoreError> {
        journal!(self, store => JournalStore::delete_run(store, run, now).await)
    }

    async fn put_tenant(&self, tenant: &Tenant) -> Result<(), StoreError> {
        journal!(self, store => store.put_tenant(tenant).await)
    }

    async fn get_tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StoreError> {
        journal!(self, store => store.get_tenant(id).await)
    }

    async fn list_tenants(&self) -> Result<Vec<Tenant>, StoreError> {
        journal!(self, store => store.list_tenants().await)
    }

    async fn put_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        exceptions: Option<&Quotas>,
        now: Timestamp,
    ) -> Result<NamespaceWrite, StoreError> {
        journal!(self, store => store.put_namespace(tenant, name, exceptions, now).await)
    }

    async fn get_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<Option<Namespace>, StoreError> {
        journal!(self, store => store.get_namespace(tenant, name).await)
    }

    async fn set_namespace_display_name(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        display_name: Option<&DisplayName>,
    ) -> Result<Option<NamespaceWrite>, StoreError> {
        journal!(self, store => store.set_namespace_display_name(tenant, name, display_name).await)
    }

    async fn namespace_display_names(
        &self,
        tenant: &TenantId,
    ) -> Result<Vec<(NamespaceName, DisplayName)>, StoreError> {
        journal!(self, store => store.namespace_display_names(tenant).await)
    }

    async fn list_namespaces(&self, filter: &NamespaceFilter) -> Result<NamespacePage, StoreError> {
        journal!(self, store => store.list_namespaces(filter).await)
    }

    async fn namespace_usage(
        &self,
        tenant: &TenantId,
        of: &UsageOf,
        month: Month,
    ) -> Result<Usage, StoreError> {
        journal!(self, store => store.namespace_usage(tenant, of, month).await)
    }

    async fn namespace_has_script(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        script: Digest,
    ) -> Result<bool, StoreError> {
        journal!(self, store => store.namespace_has_script(tenant, name, script).await)
    }

    async fn revoke_namespace_tokens(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        journal!(self, store => store.revoke_namespace_tokens(tenant, name, now).await)
    }

    async fn delete_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<DeleteNamespace, StoreError> {
        journal!(self, store => JournalStore::delete_namespace(store, tenant, name, now).await)
    }

    async fn namespaces_to_erase(
        &self,
        limit: usize,
    ) -> Result<Vec<(TenantId, NamespaceName)>, StoreError> {
        journal!(self, store => store.namespaces_to_erase(limit).await)
    }

    async fn drop_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<DropNamespace, StoreError> {
        journal!(self, store => store.drop_namespace(tenant, name).await)
    }

    async fn record_usage(
        &self,
        deltas: &[UsageDelta],
        now: Timestamp,
    ) -> Result<Vec<QuotaAlert>, StoreError> {
        journal!(self, store => store.record_usage(deltas, now).await)
    }

    async fn quota_alerts(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        month: Month,
    ) -> Result<Vec<QuotaAlert>, StoreError> {
        journal!(self, store => store.quota_alerts(tenant, namespace, month).await)
    }

    async fn claim_quota_alerts(
        &self,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
        limit: usize,
    ) -> Result<Vec<QuotaAlert>, StoreError> {
        journal!(self, store => store.claim_quota_alerts(owner, now, lease_duration, limit).await)
    }

    async fn ack_quota_alerts(
        &self,
        keys: &[QuotaAlertKey],
        now: Timestamp,
    ) -> Result<(), StoreError> {
        journal!(self, store => store.ack_quota_alerts(keys, now).await)
    }

    async fn claim_feed(
        &self,
        consumer: FeedConsumer,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
        limit: usize,
    ) -> Result<Vec<FeedItem>, StoreError> {
        journal!(self, store => store.claim_feed(consumer, owner, now, lease_duration, limit).await)
    }

    async fn ack_feed(&self, ids: &[u64]) -> Result<(), StoreError> {
        journal!(self, store => store.ack_feed(ids).await)
    }
}

impl AccessStore for Store {
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

impl ReadModel for Store {
    async fn apply(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
        snapshot: &RunSnapshot,
    ) -> Result<(), ReadModelError> {
        read_model!(self, store => store.apply(run, entries, snapshot).await)
    }

    async fn search_runs(
        &self,
        query: &RunQuery,
        order: RunOrder,
        after: Option<&SearchCursor>,
        limit: usize,
    ) -> Result<Vec<RunSummary>, ReadModelError> {
        read_model!(self, store => store.search_runs(query, order, after, limit).await)
    }

    async fn run_stats(
        &self,
        query: &RunQuery,
        window: &StatsWindow,
    ) -> Result<RunStats, ReadModelError> {
        read_model!(self, store => store.run_stats(query, window).await)
    }

    async fn failure_groups(
        &self,
        query: &RunQuery,
        window: &FailureWindow,
    ) -> Result<FailureGroups, ReadModelError> {
        read_model!(self, store => store.failure_groups(query, window).await)
    }

    async fn scripts(&self, filter: &ScriptFilter) -> Result<Vec<ScriptSummary>, ReadModelError> {
        read_model!(self, store => store.scripts(filter).await)
    }

    async fn tool_stats(&self, filter: &ToolStatsFilter) -> Result<Vec<ToolStats>, ReadModelError> {
        read_model!(self, store => store.tool_stats(filter).await)
    }

    async fn observed_shapes(
        &self,
        tenant: &TenantId,
        tools: &[String],
    ) -> Result<BTreeMap<String, Shape>, ReadModelError> {
        read_model!(self, store => store.observed_shapes(tenant, tools).await)
    }

    async fn record_tasks(
        &self,
        run: &RunRef,
        tasks: &[TaskProgress],
    ) -> Result<(), ReadModelError> {
        read_model!(self, store => store.record_tasks(run, tasks).await)
    }

    async fn attempts(&self, run: &RunRef) -> Result<BTreeMap<Seq, u32>, ReadModelError> {
        read_model!(self, store => store.attempts(run).await)
    }

    async fn record_quarantine(
        &self,
        run: &RunRef,
        quarantine: Option<&QuarantineNote>,
        at: Timestamp,
    ) -> Result<(), ReadModelError> {
        read_model!(self, store => store.record_quarantine(run, quarantine, at).await)
    }

    async fn purge(
        &self,
        ended_before: Timestamp,
        tenants: &TenantScope,
    ) -> Result<usize, ReadModelError> {
        read_model!(self, store => store.purge(ended_before, tenants).await)
    }

    async fn delete_run(&self, run: &RunRef) -> Result<bool, ReadModelError> {
        read_model!(self, store => ReadModel::delete_run(store, run).await)
    }
}

impl SecretStore for Store {
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
