//! The state every request and every loop shares: the engine wired with this server's
//! adapters, and what the HTTP API needs beside it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use ptc_api::admin::ServerRole;
use ptc_core::{Digest, RunId, Timestamp, Toolset};
use ptc_engine::Engine;
use ptc_engine::adapters::SystemEntropy;
use ptc_engine::ports::{
    Clock, JournalStore, ReadModel, Retention, RunRecord, RunRef, StoredToolset, TenantId,
    ToolDeclarations,
};
use ptc_engine::projector::{Projector, ProjectorConfig};
use ptc_engine::retention::Housekeeper;
use ptc_runtime::V8Executor;
use tokio::sync::watch;

use crate::auth::Caller;
use crate::error::ApiError;
use crate::views::Views;
use crate::wake::{Activity, POLL, Wake, wait};
use crate::wiring::blobs::Blobs;
use crate::wiring::checker::Checker;
use crate::wiring::clock::ServerClock;
use crate::wiring::store::Store;
use crate::wiring::telemetry::Exporter;
use crate::wiring::tools::{Dispatcher, Tools};
use crate::wiring::webhooks::Webhooks;

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
    /// Webhook subscriptions may use `http://` URLs (the embedded server, tests).
    pub(crate) allow_http_webhooks: bool,
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
    pub(crate) settings: Settings,
    pub(crate) wake: Wake,
    pub(crate) activity: Activity,
    /// Set to stop the server.
    pub(crate) stop: watch::Sender<bool>,
    /// Script headlines, by tenant and digest: a script never changes.
    headlines: Mutex<HashMap<(TenantId, Digest), String>>,
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
        settings: Settings,
    ) -> Self {
        Self {
            engine,
            store,
            blobs,
            checker,
            tools,
            exporter,
            settings,
            wake: Wake::new(),
            activity: Activity::new(),
            stop: watch::Sender::new(false),
            headlines: Mutex::new(HashMap::new()),
        }
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
            Ok(Some(stored)) => ptc_api::scripts::headline(stored.script.source()),
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

    /// Records the attempt of a claimed call, for the read model's statistics.
    pub(crate) async fn record_attempt(&self, run: &RunRef, seq: ptc_core::Seq, attempt: u32) {
        if let Err(error) = self.store.record_attempt(run, seq, attempt).await {
            tracing::warn!(run = %run, seq = seq.0, %error, "the attempt was not recorded");
        }
    }
}
