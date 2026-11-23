//! What every request shares: the node of this process and its plane, the edge's use cases, the
//! control plane, and what the HTTP API needs beside them.
//!
//! ```text
//!  request ─► App ─► Edge (checks) ─► NodeHandle (gateway) ─► owner of the run's partition
//!                │                                             here, or through the node link
//!                ├─► follow(key) ◄── Changed from the owners, once their batches are durable
//!                └─► control()   ◄── ControlPlaneChange from Membership::changes
//! ```

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rewind_core::{Digest, RunHandle, RunId, Timestamp, Toolset};
use rewind_engine::adapters::SystemEntropy;
use rewind_engine::edge::{Edge, EdgeError};
use rewind_engine::node::runtime::NodeHandle;
use rewind_engine::plane::{NodeConfig, Plane, Wiring};
use rewind_engine::ports::link::{RunView, SubscriptionKey};
use rewind_engine::ports::{
    CatalogStore, Clock, ControlPlaneChange, PartitionMap, Quotas, Retention, RunRef,
    StoredToolset, Tenant, TenantId, ToolDeclarations,
};
use rewind_store_slatedb::SlatePartitions;
use rewind_workos::CookieSeal;
use tokio::sync::{broadcast, watch};

use crate::auth::Caller;
use crate::error::ApiError;
use crate::follow;
use crate::idle::Idle;
use crate::link::SharedMembership;
use crate::oauth::OAuthServer;
use crate::titles::{ScriptTexts, Title};
use crate::views::Views;
use crate::wiring::bucket::{Archive, Blobs};
use crate::wiring::checker::Checker;
use crate::wiring::clock::ServerClock;
use crate::wiring::control::{Catalog, ControlPlane, ServerMembership};
use crate::wiring::executor::Executor;
use crate::wiring::identity::Identity;
use crate::wiring::link::ServerLink;
use crate::wiring::telemetry::Exporter;
use crate::wiring::tools::{Dispatcher, Tools};
use crate::wiring::webhooks::{HttpWebhooks, ServerRender, WebhookEgress};

/// The sender of lifecycle webhooks: one attempt per delivery, the next a timer of the owner.
pub(crate) type Webhooks = HttpWebhooks<ServerRender>;

/// The ports of this node.
pub(crate) type ServerPlane = Wiring<
    Catalog,
    SlatePartitions,
    Archive,
    Blobs,
    Executor,
    Dispatcher,
    ControlPlane,
    Exporter,
    Webhooks,
    ServerClock,
    SystemEntropy,
>;

/// The node of this process, and the gateway of its edge.
pub(crate) type Node = NodeHandle<ServerPlane>;

/// What the change stream of the control plane carries inside the process.
#[derive(Debug, Clone)]
pub(crate) enum Control {
    Change(ControlPlaneChange),
    /// The listener lost changes: whoever cached something drops it, and the change streams
    /// send `resync`.
    Lost,
}

/// What does not change while the server runs.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) version: &'static str,
    pub(crate) embedded: bool,
    /// The ceiling of the tenants this server creates.
    pub(crate) ceiling: Quotas,
    /// The retention of the tenants this server creates.
    pub(crate) retention: Retention,
    pub(crate) allow_stdio: bool,
    /// The URL clients reach this server at, without a trailing `/`.
    pub(crate) public_url: Option<String>,
    /// Where lifecycle webhooks may go.
    pub(crate) webhook_egress: WebhookEgress,
}

pub(crate) struct App {
    pub(crate) plane: Arc<ServerPlane>,
    pub(crate) edge: Edge<ServerPlane>,
    pub(crate) node: Node,
    pub(crate) config: NodeConfig,
    /// Access and secrets.
    pub(crate) store: ControlPlane,
    pub(crate) membership: SharedMembership<ServerMembership>,
    pub(crate) link: Arc<ServerLink>,
    /// The partition map, as the last change of the control plane gave it.
    pub(crate) map: watch::Receiver<PartitionMap>,
    /// The changes of the control plane, to every stream and cache of the process.
    pub(crate) control: broadcast::Sender<Control>,
    pub(crate) checker: Checker,
    pub(crate) tools: Tools,
    pub(crate) settings: Settings,
    pub(crate) idle: Idle,
    /// Set to stop the server.
    pub(crate) stop: watch::Sender<bool>,
    /// The dashboard's sign-in: `None` without WorkOS.
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
            .field("node", self.node.node())
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// What [`App::new`] takes.
pub(crate) struct Parts {
    pub(crate) plane: Arc<ServerPlane>,
    pub(crate) node: Node,
    pub(crate) config: NodeConfig,
    pub(crate) store: ControlPlane,
    pub(crate) membership: SharedMembership<ServerMembership>,
    pub(crate) link: Arc<ServerLink>,
    pub(crate) map: watch::Receiver<PartitionMap>,
    pub(crate) control: broadcast::Sender<Control>,
    pub(crate) checker: Checker,
    pub(crate) tools: Tools,
    pub(crate) settings: Settings,
    pub(crate) idle: Idle,
    pub(crate) sign_in: Option<SignIn>,
    pub(crate) oauth: Option<OAuthServer>,
}

impl App {
    pub(crate) fn new(parts: Parts) -> Self {
        Self {
            edge: Edge::new(Arc::clone(&parts.plane), parts.config.clone()),
            plane: parts.plane,
            node: parts.node,
            config: parts.config,
            store: parts.store,
            membership: parts.membership,
            link: parts.link,
            map: parts.map,
            control: parts.control,
            checker: parts.checker,
            tools: parts.tools,
            settings: parts.settings,
            idle: parts.idle,
            stop: watch::Sender::new(false),
            sign_in: parts.sign_in,
            oauth: parts.oauth,
            texts: Arc::new(ScriptTexts::default()),
            tenants: Mutex::new(HashSet::new()),
        }
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        self.plane.catalog()
    }

    pub(crate) fn blobs(&self) -> &Blobs {
        self.plane.blobs()
    }

    pub(crate) fn clock(&self) -> &ServerClock {
        self.plane.clock()
    }

    /// The way of a request to the owner of a partition: this node's router.
    pub(crate) fn gateway(&self) -> Node {
        self.node.clone()
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
        let catalog = self.catalog();
        if catalog.get_tenant(tenant).await?.is_none() {
            catalog
                .put_tenant(&Tenant {
                    id: tenant.clone(),
                    quotas: self.settings.ceiling.clone(),
                    namespace_defaults: Quotas::default(),
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
        Views { app: self }
    }

    pub(crate) fn now(&self) -> Timestamp {
        self.clock().now()
    }

    /// Whether the server is stopping.
    pub(crate) fn stopping(&self) -> bool {
        *self.stop.borrow()
    }

    /// The changes of the control plane from now on.
    pub(crate) fn control(&self) -> broadcast::Receiver<Control> {
        self.control.subscribe()
    }

    /// A run as its owner knows it, `None` when it does not exist.
    pub(crate) async fn read_run(&self, run: &RunRef) -> Result<Option<RunView>, ApiError> {
        match self.edge.get_run(&mut self.gateway(), run).await {
            Ok(view) => Ok(view),
            Err(EdgeError::Refused(rewind_engine::ports::link::Refusal::RunNotFound(_))) => {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// A run of the tenant, named by its id or its handle (`archi/06-storage-and-scale.md`,
    /// "Run handles"), as its owner knows it; `None` when it does not exist, or when `id` is
    /// neither.
    pub(crate) async fn find_run(
        &self,
        tenant: &TenantId,
        id: &str,
    ) -> Result<Option<RunView>, ApiError> {
        if let Ok(run) = id.parse::<RunId>() {
            return self.read_run(&RunRef::new(tenant.clone(), run)).await;
        }
        let Ok(handle) = id.parse::<RunHandle>() else {
            return Ok(None);
        };
        Ok(self
            .edge
            .find_run(&mut self.gateway(), tenant, &handle)
            .await?)
    }

    /// A run the caller sees, named by its id or its handle: of its tenant, with its allowed
    /// labels, in the namespace its request is bound to, if any. Any other is answered as
    /// missing, by what the caller asked for.
    pub(crate) async fn run(&self, caller: &Caller, id: &str) -> Result<RunView, ApiError> {
        match self.find_run(&caller.tenant, id).await? {
            Some(view) if caller.sees_run(&view.head) => Ok(view),
            _ => Err(ApiError::run_not_found(id)),
        }
    }

    /// The run, once it has ended, is quarantined, or `wait` has passed: one request held, woken
    /// by the run's changes as its owner pushes them.
    pub(crate) async fn wait_for_end(
        self: &Arc<Self>,
        run: &RunRef,
        wait: Option<Duration>,
    ) -> Result<RunView, ApiError> {
        let missing = || ApiError::run_not_found(&run.id.to_string());
        let Some(wait) = wait else {
            return self.read_run(run).await?.ok_or_else(missing);
        };
        let _busy = self.idle.busy();
        let deadline = tokio::time::Instant::now() + wait;
        let mut changes = follow::follow(self, SubscriptionKey::Run(run.clone()));
        let mut stop = self.stop.subscribe();
        loop {
            let view = self.read_run(run).await?.ok_or_else(missing)?;
            let settled = view.head.is_ended() || view.head.quarantine.is_some();
            if settled || tokio::time::Instant::now() >= deadline || self.stopping() {
                return Ok(view);
            }
            tokio::select! {
                change = changes.recv() => {
                    if change.is_none() {
                        return Ok(view);
                    }
                }
                () = tokio::time::sleep_until(deadline) => {}
                _ = stop.changed() => {}
            }
        }
    }

    /// The toolset of a run started without one: only the built-ins every toolset has.
    pub(crate) async fn default_toolset(
        &self,
        tenant: &TenantId,
    ) -> Result<StoredToolset, ApiError> {
        let toolset = Toolset::new([], [self.tools.webhook_tool()]).map_err(ApiError::internal)?;
        let catalog = self.catalog();
        if let Some(stored) = catalog.get_toolset(tenant, toolset.digest()).await? {
            return Ok(stored);
        }
        let generated = self
            .tools
            .declarations(&toolset, &std::collections::BTreeMap::new());
        let declarations = ToolDeclarations {
            typescript: generated.typescript,
            prompt: generated.prompt,
        };
        catalog.put_toolset(tenant, &toolset, &declarations).await?;
        Ok(StoredToolset {
            toolset,
            declarations,
        })
    }

    /// The first non-blank line of a script.
    pub(crate) async fn headline(&self, tenant: &TenantId, digest: Digest) -> String {
        self.texts
            .text(self.catalog(), tenant, digest)
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
        Ok(self.texts.titles(self.catalog(), tenant, scripts).await?)
    }
}
