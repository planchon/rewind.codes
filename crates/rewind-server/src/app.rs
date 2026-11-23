//! The state every request and every loop shares: the engine wired with this server's
//! adapters, and what the HTTP API needs beside it.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rewind_api::admin::ServerRole;
use rewind_core::{Digest, Timestamp, Toolset};
use rewind_engine::Engine;
use rewind_engine::adapters::SystemEntropy;
use rewind_engine::handles::find_run;
use rewind_engine::ports::{
    Clock, JournalStore, ReadModel, Retention, RunRecord, RunRef, StoredToolset, TaskProgress,
    Tenant, TenantId, ToolDeclarations, ToolTask,
};
use rewind_engine::projector::{Projector, ProjectorConfig};
use rewind_engine::retention::Housekeeper;
use rewind_runtime::V8Executor;
use rewind_workos::CookieSeal;
use tokio::sync::watch;

use crate::auth::Caller;
use crate::changes::Changes;
use crate::deliveries::{Background, Deliveries};
use crate::error::ApiError;
use crate::oauth::OAuthServer;
use crate::titles::{ScriptTexts, Title};
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
    /// The ceiling of the tenants this server creates.
    pub(crate) ceiling: rewind_engine::ports::Quotas,
    /// The retention of the tenants this server creates.
    pub(crate) retention: Retention,
    pub(crate) allow_stdio: bool,
    /// The URL clients reach this server at, without a trailing `/`.
    pub(crate) public_url: Option<String>,
    /// Where lifecycle webhooks may go.
    pub(crate) webhook_egress: WebhookEgress,
    pub(crate) projector: ProjectorConfig,
    /// The most runs a pass of the change stream's loop reads (`changes`).
    pub(crate) changes_per_pass: usize,
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
    /// The change streams' loop (`GET /v1/changes`).
    pub(crate) changes: Changes,
    /// Set to stop the server.
    pub(crate) stop: watch::Sender<bool>,
    /// The dashboard's sign-in: `None` without WorkOS, and on a worker alone.
    pub(crate) sign_in: Option<SignIn>,
    /// The OAuth authorization server of people's connections: with a public URL and a
    /// sign-in only.
    pub(crate) oauth: Option<OAuthServer>,
    /// What scripts' sources say of them, by tenant and digest: a script never changes. The
    /// webhooks' rendering shares it.
    pub(crate) texts: Arc<ScriptTexts>,
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
        texts: Arc<ScriptTexts>,
        settings: Settings,
        sign_in: Option<SignIn>,
        oauth: Option<OAuthServer>,
    ) -> Self {
        Self {
            engine,
            store,
            blobs,
            checker,
            tools,
            exporter,
            deliveries,
            changes: Changes::new(settings.changes_per_pass),
            settings,
            wake: Wake::new(),
            activity: Activity::new(),
            stop: watch::Sender::new(false),
            sign_in,
            oauth,
            texts,
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

    /// The OAuth authorization server and the sign-in it needs, or 404: OAuth exists only with
    /// a public URL and WorkOS (`archi/07-code-architecture.md`, "MCP and OAuth").
    pub(crate) fn oauth(&self) -> Result<(&OAuthServer, &SignIn), ApiError> {
        match (&self.oauth, &self.sign_in) {
            (Some(oauth), Some(sign_in)) => Ok((oauth, sign_in)),
            _ => Err(ApiError::not_found(
                "this server has no OAuth: it needs a public URL (--public-url) and a sign-in \
                 (WorkOS). /mcp takes API keys and end users' tokens only",
            )),
        }
    }

    /// Creates the tenant's record, with the deployment's ceiling and retention, when it has
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
                    quotas: self.settings.ceiling.clone(),
                    namespace_defaults: rewind_engine::ports::Quotas::default(),
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
            texts: &self.texts,
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

    /// A run the caller sees, named by its id or its handle: of its tenant, with its allowed
    /// labels, in the namespace its request is bound to, if any. Any other is answered as
    /// missing, by what the caller asked for.
    pub(crate) async fn run(&self, caller: &Caller, id: &str) -> Result<RunRecord, ApiError> {
        match find_run(&self.store, &caller.tenant, id).await? {
            Some(record) if caller.sees_run(&record) => Ok(record),
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
        self.texts
            .text(&self.store, tenant, digest)
            .await
            .map(|text| text.headline.clone())
            .unwrap_or_default()
    }

    /// The titles of these scripts of the tenant, for those that have one.
    pub(crate) async fn titles(
        &self,
        tenant: &TenantId,
        scripts: impl IntoIterator<Item = Digest>,
    ) -> Result<BTreeMap<Digest, Title>, ApiError> {
        Ok(self.texts.titles(&self.store, tenant, scripts).await?)
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
        match self.store.record_tasks(run, &[progress]).await {
            Ok(()) => self.changes.poke(),
            Err(error) => {
                tracing::warn!(run = %run, seq = progress.seq.0, %error, "the task's state was not recorded");
            }
        }
    }

    /// Records the states of a run's open tasks, as the store holds them now, after a change
    /// the caller does not see the result of: an approval, a rejection, a call that may have
    /// been parked, the secret it reads put again.
    pub(crate) async fn record_tasks(&self, run: &RunRef) {
        match self.projector().record_tasks(run).await {
            Ok(_) => self.changes.poke(),
            Err(error) => tracing::warn!(run = %run, %error, "the tasks' states were not recorded"),
        }
    }

    /// Records a run's quarantine, as the store holds it now, after a quarantine, a release or
    /// a give-up.
    pub(crate) async fn record_quarantine(&self, run: &RunRef) {
        match self.projector().record_quarantine(run, self.now()).await {
            Ok(_) => self.changes.poke(),
            Err(error) => tracing::warn!(run = %run, %error, "the quarantine was not recorded"),
        }
    }
}
