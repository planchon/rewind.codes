//! The store: `JournalStore`, `ReadModel` and `SecretStore` on one backend, chosen by the
//! configuration (`archi/06-storage-and-scale.md`, "Backends"):
//!
//! | `--store` | Backend |
//! | --- | --- |
//! | `turso:<path>` | `ptc-store-turso`: the embedded server, local development |
//! | `postgres://…` | `ptc-store-postgres`: production, several workers on one database |
//!
//! Each method is handed to the backend as it is: the enum only picks it.

#![allow(clippy::too_many_lines)]

use std::path::PathBuf;
use std::time::Duration;

use ptc_core::{
    Completion, Digest, Epoch, HistoryEntry, RunId, Script, Seq, Shape, Timestamp, ToolFailure,
    ToolOutcome, Toolset,
};
use ptc_engine::ports::{
    AcquireRunnable, Activation, ApiKey, ClaimedTask, CompiledScript, CreateRun, Credentials,
    Enqueued, ExpiredRuns, Failures, FeedConsumer, FeedItem, InboxItem, JournalStore, Lease,
    NewRun, ParkReason, ParkedScope, PayloadRecord, ProjectedRun, ReadModel, ReadModelError,
    RunFilter, RunRecord, RunRef, RunSearch, RunSummary, ScriptFilter, ScriptSummary, SecretError,
    SecretOwner, SecretStore, StoreError, StoredScript, StoredToolset, Subscription, TaskFilter,
    TaskRef, TaskToken, TaskUpdate, Tenant, TenantId, TenantScope, ToolDeclarations, ToolStats,
    ToolStatsFilter, ToolTask, WebhookTarget, WebhookToken,
};
use ptc_store_postgres::PostgresStore;
use ptc_store_turso::TursoStore;
use std::collections::BTreeMap;

use crate::config::ConfigError;

/// Where the store lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoreLocation {
    /// A Turso database file.
    Turso(PathBuf),
    /// A Postgres connection string.
    Postgres(String),
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
}

impl Store {
    pub(crate) async fn open(
        location: &StoreLocation,
        master_key: [u8; 32],
    ) -> Result<Self, ConfigError> {
        match location {
            StoreLocation::Turso(path) => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        ConfigError::new(format!("cannot create {}: {error}", parent.display()))
                    })?;
                }
                let master_key = ptc_store_turso::MasterKey::from_bytes(master_key);
                let store = TursoStore::open(path, master_key).await.map_err(|error| {
                    ConfigError::new(format!("cannot open {}: {error}", path.display()))
                })?;
                Ok(Self::Turso(store))
            }
            StoreLocation::Postgres(url) => {
                let master_key = ptc_store_postgres::MasterKey::from_bytes(master_key);
                let store =
                    PostgresStore::connect(&ptc_store_postgres::Config::new(url), master_key)
                        .await
                        .map_err(|error| ConfigError::new(format!("Postgres: {error}")))?;
                Ok(Self::Postgres(store))
            }
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
        }
    }
}

macro_rules! each {
    ($self:ident, $store:ident => $call:expr) => {
        match $self {
            Store::Turso($store) => $call,
            Store::Postgres($store) => $call,
        }
    };
}

impl JournalStore for Store {
    async fn put_script(
        &self,
        tenant: &TenantId,
        script: &Script,
        compiled: &CompiledScript,
    ) -> Result<(), StoreError> {
        each!(self, store => store.put_script(tenant, script, compiled).await)
    }

    async fn get_script(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredScript>, StoreError> {
        each!(self, store => store.get_script(tenant, digest).await)
    }

    async fn put_toolset(
        &self,
        tenant: &TenantId,
        toolset: &Toolset,
        declarations: &ToolDeclarations,
    ) -> Result<(), StoreError> {
        each!(self, store => store.put_toolset(tenant, toolset, declarations).await)
    }

    async fn get_toolset(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredToolset>, StoreError> {
        each!(self, store => store.get_toolset(tenant, digest).await)
    }

    async fn list_toolsets(&self, tenant: &TenantId) -> Result<Vec<StoredToolset>, StoreError> {
        each!(self, store => store.list_toolsets(tenant).await)
    }

    async fn create_run(
        &self,
        run: NewRun,
        idempotency_key: Option<String>,
    ) -> Result<CreateRun, StoreError> {
        each!(self, store => store.create_run(run, idempotency_key).await)
    }

    async fn enqueue(
        &self,
        run: &RunRef,
        completion: Completion,
        idempotency_key: Option<String>,
        now: Timestamp,
    ) -> Result<Enqueued, StoreError> {
        each!(self, store => store.enqueue(run, completion, idempotency_key, now).await)
    }

    async fn get_run(&self, run: &RunRef) -> Result<Option<RunRecord>, StoreError> {
        each!(self, store => store.get_run(run).await)
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<Vec<RunRecord>, StoreError> {
        each!(self, store => store.list_runs(filter).await)
    }

    async fn acquire_runnable(
        &self,
        request: &AcquireRunnable<'_>,
    ) -> Result<Option<Lease>, StoreError> {
        each!(self, store => store.acquire_runnable(request).await)
    }

    async fn acquire_run(
        &self,
        run: &RunRef,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        each!(self, store => store.acquire_run(run, owner, now, lease_duration).await)
    }

    async fn acquire_quarantined(
        &self,
        owner: &str,
        build: &str,
        epochs: &[Epoch],
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        each!(self, store => store.acquire_quarantined(owner, build, epochs, now, lease_duration).await)
    }

    async fn renew(
        &self,
        lease: &Lease,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<(), StoreError> {
        each!(self, store => store.renew(lease, now, lease_duration).await)
    }

    async fn load_history(&self, run: &RunRef, from: u64) -> Result<Vec<HistoryEntry>, StoreError> {
        each!(self, store => store.load_history(run, from).await)
    }

    async fn take_inbox(&self, lease: &Lease) -> Result<Vec<InboxItem>, StoreError> {
        each!(self, store => store.take_inbox(lease).await)
    }

    async fn commit_activation(
        &self,
        lease: &Lease,
        activation: Activation,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        each!(self, store => store.commit_activation(lease, activation, now).await)
    }

    async fn release(&self, lease: &Lease) -> Result<(), StoreError> {
        each!(self, store => store.release(lease).await)
    }

    async fn forget_sticky(&self, run: &RunRef, owner: &str) -> Result<(), StoreError> {
        each!(self, store => store.forget_sticky(run, owner).await)
    }

    async fn record_failure(
        &self,
        lease: &Lease,
        error: String,
        now: Timestamp,
    ) -> Result<Failures, StoreError> {
        each!(self, store => store.record_failure(lease, error, now).await)
    }

    async fn quarantine(
        &self,
        lease: &Lease,
        mismatch: String,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        each!(self, store => store.quarantine(lease, mismatch, now).await)
    }

    async fn release_quarantine(&self, run: &RunRef) -> Result<bool, StoreError> {
        each!(self, store => store.release_quarantine(run).await)
    }

    async fn fire_due_timers(&self, now: Timestamp, limit: usize) -> Result<usize, StoreError> {
        each!(self, store => store.fire_due_timers(now, limit).await)
    }

    async fn next_timer(&self) -> Result<Option<Timestamp>, StoreError> {
        each!(self, store => store.next_timer().await)
    }

    async fn expire_due_runs(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<ExpiredRuns, StoreError> {
        each!(self, store => store.expire_due_runs(now, limit).await)
    }

    async fn next_deadline(&self) -> Result<Option<Timestamp>, StoreError> {
        each!(self, store => store.next_deadline().await)
    }

    async fn claim_tool_tasks(
        &self,
        owner: &str,
        filter: &TaskFilter,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<ClaimedTask>, StoreError> {
        each!(self, store => store.claim_tool_tasks(owner, filter, now, limit).await)
    }

    async fn heartbeat_tool_task(
        &self,
        token: &TaskToken,
        now: Timestamp,
    ) -> Result<Option<Timestamp>, StoreError> {
        each!(self, store => store.heartbeat_tool_task(token, now).await)
    }

    async fn next_tool_attempt(&self, after: Timestamp) -> Result<Option<Timestamp>, StoreError> {
        each!(self, store => store.next_tool_attempt(after).await)
    }

    async fn retry_tool_task(
        &self,
        token: &TaskToken,
        next_attempt_at: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        each!(self, store => store.retry_tool_task(token, next_attempt_at).await)
    }

    async fn defer_tool_task(
        &self,
        token: &TaskToken,
        until: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        each!(self, store => store.defer_tool_task(token, until).await)
    }

    async fn park_tool_task(
        &self,
        token: &TaskToken,
        reason: ParkReason,
    ) -> Result<TaskUpdate, StoreError> {
        each!(self, store => store.park_tool_task(token, reason).await)
    }

    async fn complete_tool_task(
        &self,
        token: &TaskToken,
        outcome: ToolOutcome,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        each!(self, store => store.complete_tool_task(token, outcome, now).await)
    }

    async fn approve_tool_task(
        &self,
        task: &TaskRef,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        each!(self, store => store.approve_tool_task(task, now).await)
    }

    async fn reject_tool_task(
        &self,
        task: &TaskRef,
        failure: ToolFailure,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        each!(self, store => store.reject_tool_task(task, failure, now).await)
    }

    async fn resume_parked(
        &self,
        scope: &ParkedScope,
        now: Timestamp,
    ) -> Result<usize, StoreError> {
        each!(self, store => store.resume_parked(scope, now).await)
    }

    async fn expire_deferred_tool_tasks(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<usize, StoreError> {
        each!(self, store => store.expire_deferred_tool_tasks(now, limit).await)
    }

    async fn tool_tasks(&self, run: &RunRef) -> Result<Vec<ToolTask>, StoreError> {
        each!(self, store => store.tool_tasks(run).await)
    }

    async fn tool_task(&self, token: &TaskToken) -> Result<Option<ToolTask>, StoreError> {
        each!(self, store => store.tool_task(token).await)
    }

    async fn create_webhook_token(
        &self,
        run: &RunRef,
        name: &str,
    ) -> Result<WebhookToken, StoreError> {
        each!(self, store => store.create_webhook_token(run, name).await)
    }

    async fn resolve_webhook_token(
        &self,
        token: &WebhookToken,
    ) -> Result<Option<WebhookTarget>, StoreError> {
        each!(self, store => store.resolve_webhook_token(token).await)
    }

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), StoreError> {
        each!(self, store => store.put_subscription(subscription).await)
    }

    async fn subscriptions(&self, tenant: &TenantId) -> Result<Vec<Subscription>, StoreError> {
        each!(self, store => store.subscriptions(tenant).await)
    }

    async fn delete_subscription(&self, tenant: &TenantId, id: &str) -> Result<bool, StoreError> {
        each!(self, store => store.delete_subscription(tenant, id).await)
    }

    async fn get_payload(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<PayloadRecord>, StoreError> {
        each!(self, store => store.get_payload(tenant, digest).await)
    }

    async fn unreferenced_payloads(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        each!(self, store => store.unreferenced_payloads(now, limit).await)
    }

    async fn forget_payloads(
        &self,
        payloads: &[(TenantId, Digest)],
        now: Timestamp,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        each!(self, store => store.forget_payloads(payloads, now).await)
    }

    async fn runs_to_archive(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<RunRef>, StoreError> {
        each!(self, store => store.runs_to_archive(now, limit).await)
    }

    async fn archive_run(
        &self,
        run: &RunRef,
        document: Digest,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        each!(self, store => store.archive_run(run, document, now).await)
    }

    async fn runs_to_delete(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<RunRef>, StoreError> {
        each!(self, store => store.runs_to_delete(now, limit).await)
    }

    async fn delete_run(&self, run: &RunRef, now: Timestamp) -> Result<bool, StoreError> {
        each!(self, store => JournalStore::delete_run(store, run, now).await)
    }

    async fn put_tenant(&self, tenant: &Tenant) -> Result<(), StoreError> {
        each!(self, store => store.put_tenant(tenant).await)
    }

    async fn get_tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StoreError> {
        each!(self, store => store.get_tenant(id).await)
    }

    async fn list_tenants(&self) -> Result<Vec<Tenant>, StoreError> {
        each!(self, store => store.list_tenants().await)
    }

    async fn put_api_key(&self, key: &ApiKey) -> Result<(), StoreError> {
        each!(self, store => store.put_api_key(key).await)
    }

    async fn find_api_key(&self, hash: Digest) -> Result<Option<ApiKey>, StoreError> {
        each!(self, store => store.find_api_key(hash).await)
    }

    async fn revoke_api_key(&self, hash: Digest, now: Timestamp) -> Result<bool, StoreError> {
        each!(self, store => store.revoke_api_key(hash, now).await)
    }

    async fn claim_feed(
        &self,
        consumer: FeedConsumer,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
        limit: usize,
    ) -> Result<Vec<FeedItem>, StoreError> {
        each!(self, store => store.claim_feed(consumer, owner, now, lease_duration, limit).await)
    }

    async fn ack_feed(&self, ids: &[u64]) -> Result<(), StoreError> {
        each!(self, store => store.ack_feed(ids).await)
    }
}

impl ReadModel for Store {
    async fn apply(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
    ) -> Result<(), ReadModelError> {
        each!(self, store => store.apply(run, entries).await)
    }

    async fn search_runs(&self, search: &RunSearch) -> Result<Vec<RunSummary>, ReadModelError> {
        each!(self, store => store.search_runs(search).await)
    }

    async fn scripts(&self, filter: &ScriptFilter) -> Result<Vec<ScriptSummary>, ReadModelError> {
        each!(self, store => store.scripts(filter).await)
    }

    async fn tool_stats(&self, filter: &ToolStatsFilter) -> Result<Vec<ToolStats>, ReadModelError> {
        each!(self, store => store.tool_stats(filter).await)
    }

    async fn observed_shapes(
        &self,
        tenant: &TenantId,
        tools: &[String],
    ) -> Result<BTreeMap<String, Shape>, ReadModelError> {
        each!(self, store => store.observed_shapes(tenant, tools).await)
    }

    async fn record_attempt(
        &self,
        run: &RunRef,
        seq: Seq,
        attempt: u32,
    ) -> Result<(), ReadModelError> {
        each!(self, store => store.record_attempt(run, seq, attempt).await)
    }

    async fn attempts(&self, run: &RunRef) -> Result<BTreeMap<Seq, u32>, ReadModelError> {
        each!(self, store => store.attempts(run).await)
    }

    async fn purge(
        &self,
        ended_before: Timestamp,
        tenants: &TenantScope,
    ) -> Result<usize, ReadModelError> {
        each!(self, store => store.purge(ended_before, tenants).await)
    }

    async fn delete_run(&self, run: &RunRef) -> Result<bool, ReadModelError> {
        each!(self, store => ReadModel::delete_run(store, run).await)
    }
}

impl SecretStore for Store {
    async fn put(
        &self,
        tenant: &TenantId,
        owner: &SecretOwner,
        credentials: &Credentials,
    ) -> Result<(), SecretError> {
        each!(self, store => store.put(tenant, owner, credentials).await)
    }

    async fn resolve(
        &self,
        tenant: &TenantId,
        run: RunId,
        toolset: Digest,
    ) -> Result<Option<Credentials>, SecretError> {
        each!(self, store => store.resolve(tenant, run, toolset).await)
    }

    async fn delete(&self, tenant: &TenantId, owner: &SecretOwner) -> Result<bool, SecretError> {
        each!(self, store => store.delete(tenant, owner).await)
    }
}
