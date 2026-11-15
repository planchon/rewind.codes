//! The state every request and every loop shares: the engine wired with this server's
//! adapters, and what the HTTP API needs beside it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rewind_api::admin::ServerRole;
use rewind_core::{Digest, RunId, Timestamp, Toolset};
use rewind_engine::Engine;
use rewind_engine::adapters::SystemEntropy;
use rewind_engine::ports::{
    Clock, JournalStore, Predicate, ReadModel, Retention, RunOrder, RunQuery, RunRecord, RunRef,
    StoredToolset, TaskProgress, Tenant, TenantId, ToolDeclarations, ToolTask, WaitingFor,
};
use rewind_engine::projector::{Projector, ProjectorConfig};
use rewind_engine::retention::Housekeeper;
use rewind_runtime::V8Executor;
use rewind_workos::CookieSeal;
use tokio::sync::watch;

use crate::auth::Caller;
use crate::deliveries::{Background, Deliveries};
use crate::error::ApiError;
use crate::views::Views;
use crate::wake::{Activity, POLL, Wake, wait};
use crate::wiring::blobs::Blobs;
use crate::wiring::checker::Checker;
use crate::wiring::clock::ServerClock;
use crate::wiring::identity::Identity;
use crate::wiring::store::Store;
use crate::wiring::telemetry::Exporter;
use crate::wiring::tools::{Dispatcher, Tools};
use crate::wiring::webhooks::{ServerRender, WebhookEgress};

/// The engine's sender of lifecycle webhooks: in the background.
pub(crate) type Webhooks = Background<ServerRender>;

/// The engine of this server.
pub(crate) type ServerEngine =
    Engine<Store, Blobs, V8Executor, Dispatcher, ServerClock, SystemEntropy, Store, Webhooks>;

/// The longest `wait` of a request: a client waits longer with several requests.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(300);

/// What does not change while the server runs.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) version: &'static str,
    pub(crate) embedded: bool,
    pub(crate) roles: Vec<ServerRole>,
    pub(crate) run_quota: Option<u32>,
    /// The retention of the tenants this server creates.
    pub(crate) retention: Retention,
    pub(crate) allow_stdio: bool,
    /// Where lifecycle webhooks may go.
    pub(crate) webhook_egress: WebhookEgress,
    pub(crate) projector: ProjectorConfig,
}

impl Settings {
    pub(crate) fn worker(&self) -> bool {
        self.roles.contains(&ServerRole::Worker)
    }
}

pub(crate) struct App {
    pub(crate) engine: ServerEngine,
    pub(crate) store: Store,
    pub(crate) blobs: Blobs,
    pub(crate) checker: Checker,
    pub(crate) tools: Tools,
    pub(crate) exporter: Exporter,
    /// Every delivery of lifecycle webhooks: those of the feed, and the engine's.
    pub(crate) deliveries: Arc<Deliveries<ServerRender>>,
    pub(crate) settings: Settings,
    pub(crate) wake: Wake,
    pub(crate) activity: Activity,
    /// Set to stop the server.
    pub(crate) stop: watch::Sender<bool>,
    /// The dashboard's sign-in: `None` without WorkOS, and on a worker alone.
    pub(crate) sign_in: Option<SignIn>,
    /// Script headlines, by tenant and digest: a script never changes.
    headlines: Mutex<HashMap<(TenantId, Digest), String>>,
    /// Tenants known to have their record: a tenant is never deleted.
    tenants: Mutex<HashSet<TenantId>>,
}

/// The dashboard's sign-in (`archi/07-code-architecture.md`, "Sign-in (`/auth`)"): the identity
/// provider, the seal of the cookies, and where the provider sends the browser back.
#[derive(Debug)]
pub(crate) struct SignIn {
    pub(crate) identity: Identity,
    pub(crate) seal: CookieSeal,
    pub(crate) redirect_uri: String,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        engine: ServerEngine,
        store: Store,
        blobs: Blobs,
        checker: Checker,
        tools: Tools,
        exporter: Exporter,
        deliveries: Arc<Deliveries<ServerRender>>,
        settings: Settings,
        sign_in: Option<SignIn>,
    ) -> Self {
        Self {
            engine,
            store,
            blobs,
            checker,
            tools,
            exporter,
            deliveries,
            settings,
            wake: Wake::new(),
            activity: Activity::new(),
            stop: watch::Sender::new(false),
            sign_in,
            headlines: Mutex::new(HashMap::new()),
            tenants: Mutex::new(HashSet::new()),
        }
    }

    /// The sign-in, or 404 on a server without WorkOS: organizations, members and invitations
    /// are WorkOS's.
    pub(crate) fn sign_in(&self) -> Result<&SignIn, ApiError> {
        self.sign_in.as_ref().ok_or_else(|| {
            ApiError::not_found(
                "this server has no sign-in (WorkOS is not configured): it has no organizations, \
                 members or invitations, and accepts only API keys, whose rights come from \
                 their grants",
            )
        })
    }

    /// Creates the tenant's record, with the deployment's quota and retention, when it has
    /// none: at its first key, at its organization's creation, at a member's first request.
    pub(crate) async fn ensure_tenant(&self, tenant: &TenantId) -> Result<(), ApiError> {
        let known = |tenants: &HashSet<TenantId>| tenants.contains(tenant);
        if self.tenants.lock().is_ok_and(|tenants| known(&tenants)) {
            return Ok(());
        }
        if self.store.get_tenant(tenant).await?.is_none() {
            self.store
                .put_tenant(&Tenant {
                    id: tenant.clone(),
                    active_run_quota: self.settings.run_quota,
                    retention: self.settings.retention,
                })
                .await?;
        }
        if let Ok(mut tenants) = self.tenants.lock() {
            if tenants.len() > 100_000 {
                tenants.clear();
            }
            tenants.insert(tenant.clone());
        }
        Ok(())
    }

    pub(crate) fn views(&self) -> Views<'_> {
        Views {
            store: &self.store,
            blobs: &self.blobs,
        }
    }

    pub(crate) fn now(&self) -> Timestamp {
        self.engine.clock().now()
    }

    pub(crate) fn projector(&self) -> Projector<'_, Store, Store, Exporter, Webhooks> {
        Projector {
            journal: &self.store,
            read_model: &self.store,
            exporter: &self.exporter,
            webhooks: self.engine.webhooks(),
            config: self.settings.projector.clone(),
        }
    }

    pub(crate) fn housekeeper(&self) -> Housekeeper<'_, Store, Blobs, Store, Store> {
        Housekeeper {
            journal: &self.store,
            blobs: &self.blobs,
            read_model: &self.store,
            secrets: &self.store,
            batch: 100,
        }
    }

    /// Whether the server is stopping.
    pub(crate) fn stopping(&self) -> bool {
        *self.stop.borrow()
    }

    /// A run the caller sees: of its tenant, with its allowed labels. Any other is answered
    /// as missing.
    pub(crate) async fn run(&self, caller: &Caller, id: &str) -> Result<RunRecord, ApiError> {
        let run_id: RunId = id.parse().map_err(|_| ApiError::run_not_found(id))?;
        let run = RunRef::new(caller.tenant.clone(), run_id);
        match self.store.get_run(&run).await? {
            Some(record) if caller.sees(&record.metadata.labels) => Ok(record),
            _ => Err(ApiError::run_not_found(id)),
        }
    }

    /// The run, once it has ended or `wait` has passed.
    pub(crate) async fn wait_for_end(
        &self,
        run: &RunRef,
        wait_for: Option<Duration>,
    ) -> Result<RunRecord, ApiError> {
        let mut changes = self.wake.subscribe();
        let deadline = wait_for.map(|wait| tokio::time::Instant::now() + wait.min(MAX_WAIT));
        loop {
            let record = self
                .store
                .get_run(run)
                .await?
                .ok_or_else(|| ApiError::run_not_found(&run.id.to_string()))?;
            let done = record.status.is_terminal()
                || deadline.is_none_or(|deadline| tokio::time::Instant::now() >= deadline)
                || self.stopping();
            if done {
                return Ok(record);
            }
            wait(&mut changes, POLL, deadline).await;
        }
    }

    /// The toolset of a run started without one: only the built-ins every toolset has.
    pub(crate) async fn default_toolset(
        &self,
        tenant: &TenantId,
    ) -> Result<StoredToolset, ApiError> {
        let toolset = Toolset::new([], [self.tools.webhook_tool()]).map_err(ApiError::internal)?;
        if let Some(stored) = self.store.get_toolset(tenant, toolset.digest()).await? {
            return Ok(stored);
        }
        let generated = self
            .tools
            .declarations(&toolset, &std::collections::BTreeMap::new());
        let declarations = ToolDeclarations {
            typescript: generated.typescript,
            prompt: generated.prompt,
        };
        self.store
            .put_toolset(tenant, &toolset, &declarations)
            .await?;
        Ok(StoredToolset {
            toolset,
            declarations,
        })
    }

    /// The first non-blank line of a script.
    pub(crate) async fn headline(&self, tenant: &TenantId, digest: Digest) -> String {
        let key = (tenant.clone(), digest);
        if let Some(headline) = self
            .headlines
            .lock()
            .ok()
            .and_then(|headlines| headlines.get(&key).cloned())
        {
            return headline;
        }
        let headline = match self.store.get_script(tenant, digest).await {
            Ok(Some(stored)) => rewind_api::scripts::headline(stored.script.source()),
            _ => return String::new(),
        };
        if let Ok(mut headlines) = self.headlines.lock() {
            if headlines.len() > 10_000 {
                headlines.clear();
            }
            headlines.insert(key, headline.clone());
        }
        headline
    }

    /// Records the state of a task the caller holds in the read model: a claim, a deferral.
    pub(crate) async fn record_task(&self, task: &ToolTask) {
        self.record_progress(
            &task.task.run,
            rewind_engine::projector::task_progress(task),
        )
        .await;
    }

    /// Records a task's state in the read model.
    pub(crate) async fn record_progress(&self, run: &RunRef, progress: TaskProgress) {
        if let Err(error) = self.store.record_tasks(run, &[progress]).await {
            tracing::warn!(run = %run, seq = progress.seq.0, %error, "the task's state was not recorded");
        }
    }

    /// Records the states of a run's open tasks, as the store holds them now, after a change
    /// the caller does not see the result of: an approval, a rejection, a call that may have
    /// been parked, renewed credentials.
    pub(crate) async fn record_tasks(&self, run: &RunRef) {
        if let Err(error) = self.projector().record_tasks(run).await {
            tracing::warn!(run = %run, %error, "the tasks' states were not recorded");
        }
    }

    /// Records the states of the tasks of the runs of a toolset that had a call parked, after
    /// the toolset's credentials were renewed: their parked calls are ready again.
    pub(crate) async fn record_parked_tasks(&self, tenant: &TenantId, toolset: Digest) {
        const PAGE: usize = 100;
        let query = RunQuery::new(tenant.clone())
            .with(Predicate::Toolset(vec![toolset.to_hex()]))
            .with(Predicate::Waiting(BTreeSet::from([WaitingFor::Parked])));
        let mut after = None;
        loop {
            let runs = match self
                .store
                .search_runs(&query, RunOrder::Started, after.as_ref(), PAGE)
                .await
            {
                Ok(runs) => runs,
                Err(error) => {
                    tracing::warn!(%toolset, %error, "the parked tasks' states were not recorded");
                    return;
                }
            };
            for run in &runs {
                self.record_tasks(&run.run).await;
            }
            match runs.last() {
                Some(last) if runs.len() == PAGE => after = Some(last.cursor(RunOrder::Started)),
                _ => return,
            }
        }
    }

    /// Records a run's quarantine, as the store holds it now, after a quarantine, a release or
    /// a give-up.
    pub(crate) async fn record_quarantine(&self, run: &RunRef) {
        if let Err(error) = self.projector().record_quarantine(run, self.now()).await {
            tracing::warn!(run = %run, %error, "the quarantine was not recorded");
        }
    }
}
