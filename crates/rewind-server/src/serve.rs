//! Starting and stopping a node (`archi/07-code-architecture.md`, "Inside a node";
//! `archi/06-storage-and-scale.md`, "Partitions, owners and nodes"):
//!
//! ```text
//!  config ─► control plane (catalog, access, secrets) ─► place_namespaces
//!         ─► bucket (partitions, archive, blobs)
//!         ─► plane ─► cluster.join(NodeInfo) ─► runtime::spawn(plane, config, membership, link)
//!         ─► the control plane's changes: the partition map, every change to the streams
//!         ─► the HTTP edge (TCP or the embedded socket), the node link, the idle stop, the
//!            daily jobs
//! ```
//!
//! A stop hands the node's partitions over to the other nodes before the process exits.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use futures_util::StreamExt as _;
use rewind_api::Problem;
use rewind_api::access::OPERATOR;
use rewind_core::Timestamp;
use rewind_engine::adapters::{LocalLink, MemoryIdentity, SystemClock, SystemEntropy};
use rewind_engine::node::runtime;
use rewind_engine::plane::{NodeConfig, Wiring};
use rewind_engine::ports::cluster::Membership;
use rewind_engine::ports::{
    AccessStore, ApiKey, Clock, Cluster, ControlPlaneChange, Grant, MasterKeyId, NodeInfo,
    PartitionMap, Principal, ScriptExecutor, SecretError, SecretStore, StoreError,
};
use rewind_runtime::{SandboxLimits, V8Executor};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::app::{App, Control, Parts, Settings, SignIn};
use crate::auth::key_hash;
use crate::config::{Config, Listen, MasterKeyRing, ServerArgs};
use crate::connections::{self, McpCaller};
use crate::embedded::Directory;
use crate::http;
use crate::idle::Idle;
use crate::link::{ClusterSecret, HttpLink, SharedMembership};
use crate::oauth::OAuthServer;
use crate::wiring::checker;
use crate::wiring::clock::ServerClock;
use crate::wiring::control::{ControlPlane, ServerMembership};
use crate::wiring::executor::Executor;
use crate::wiring::identity::Identity;
use crate::wiring::link::ServerLink;
use crate::wiring::telemetry::{self, Exporter};
use crate::wiring::tools::Tools;
use crate::wiring::webhooks::{HttpWebhooks, Retries, ServerRender, WebhookEgress};
use crate::wiring::{bucket, jobs};

/// The version of the `rewind` binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the stop may take: the hand-over of the partitions and the requests in flight.
const DRAIN: Duration = Duration::from_secs(30);

/// Why the server could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// The command line or the environment is not a configuration that can be served.
    #[error("{0}")]
    Config(String),
    /// Another embedded server holds the directory's lock.
    #[error("another server holds the lock of {}", .0.display())]
    Locked(PathBuf),
    /// A file, a socket or a port cannot be used.
    #[error("{0}")]
    Io(String),
    /// The store, the runtime or another adapter cannot be set up.
    #[error("{0}")]
    Setup(String),
}

impl From<crate::config::ConfigError> for StartupError {
    fn from(error: crate::config::ConfigError) -> Self {
        StartupError::Config(error.to_string())
    }
}

/// A server running in this process.
#[derive(Debug)]
pub struct Running {
    app: Arc<App>,
    /// The address of the TCP listener of the edge.
    addr: Option<SocketAddr>,
    /// The address of the node link's listener.
    link_addr: Option<SocketAddr>,
    /// The embedded server's socket.
    socket: Option<PathBuf>,
    listeners: Vec<JoinHandle<()>>,
    tasks: Vec<JoinHandle<()>>,
    /// Holds the embedded server's lock until the server has stopped.
    directory: Option<Directory>,
}

impl Running {
    /// `http://<addr>` of a server listening on TCP.
    pub fn url(&self) -> Option<String> {
        self.addr.map(|addr| format!("http://{addr}"))
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.addr
    }

    /// The address the node link listens on, on a deployed node.
    pub fn link_addr(&self) -> Option<SocketAddr> {
        self.link_addr
    }

    /// The embedded server's socket.
    pub fn socket(&self) -> Option<&PathBuf> {
        self.socket.as_ref()
    }

    /// The id of this server's node.
    pub fn node(&self) -> String {
        self.app.node.node().to_string()
    }

    /// Who calls `/mcp` with `bearer`, and the organizations it reaches with the permissions it
    /// may use in each (`archi/07-code-architecture.md`, "MCP and OAuth"): a connection's access
    /// token, an API key or an end user's token. A member's access token, or anything else, is
    /// refused (`unauthorized`).
    pub async fn authenticate_mcp(&self, bearer: &str) -> Result<McpCaller, Box<Problem>> {
        connections::mcp_caller_or_problem(&self.app, bearer).await
    }

    /// Waits until something stops the server: `POST /v1/server/stop`, or the idle timeout.
    pub async fn stopped(&self) {
        let mut stop = self.app.stop.subscribe();
        let _ = stop.wait_for(|stop| *stop).await;
    }

    /// Stops the server: no new request; the node hands its partitions over to the other nodes
    /// (the work in flight is rebuilt by their next owner from the records), and the embedded
    /// server's socket and lock are released.
    pub async fn stop(self) {
        self.app.stop.send_replace(true);
        let drain = async {
            for listener in self.listeners {
                let _ = listener.await;
            }
            if let Err(error) = self.app.node.stop().await {
                tracing::warn!(%error, "the node did not hand its partitions over");
            }
        };
        if tokio::time::timeout(DRAIN, drain).await.is_err() {
            tracing::warn!("the stop did not finish within {DRAIN:?}");
        }
        for task in self.tasks {
            task.abort();
        }
        self.app.tools.close();
        self.app.plane.spans.shutdown().await;
        self.app.store.close().await;
        if let Some(socket) = &self.socket {
            let _ = std::fs::remove_file(socket);
        }
        drop(self.directory);
        tracing::info!("stopped");
    }
}

impl Running {
    /// Stops the server at once, as a process killed (`kill -9`): its node hands nothing over,
    /// its listeners close, and its membership of the cluster ends with the last of its tasks,
    /// so that the other nodes take its partitions over. For the cluster's tests.
    pub fn kill(self) {
        self.app.node.abort();
        self.app.stop.send_replace(true);
        for task in self.listeners.into_iter().chain(self.tasks) {
            task.abort();
        }
        drop(self.directory);
        tracing::info!(node = %self.app.node.node(), "killed");
    }
}

/// Starts a server in this process.
pub async fn start(args: ServerArgs) -> Result<Running, StartupError> {
    start_wired(args, None).await
}

/// What `rewind server --rewrap-data-keys` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrapped {
    /// The data keys it encrypted again: those held under a previous master key.
    pub changed: usize,
    /// The id of the master key every data key is now held under.
    pub master_key: MasterKeyId,
}

/// `rewind server --rewrap-data-keys` (`archi/04-tools.md`, "Secrets": the master key can be
/// replaced): opens the control plane with `--master-key` and `--previous-master-keys`, encrypts
/// with the current key every data key held under a previous one, one tenant at a time, and
/// closes it. Servers may keep running meanwhile, and two rewraps at once change each data key
/// once.
pub async fn rewrap_data_keys(args: ServerArgs) -> Result<Rewrapped, StartupError> {
    let config = args.resolve()?;
    let keys = config
        .master_keys
        .clone()
        .ok_or_else(|| StartupError::Config("--rewrap-data-keys needs --master-key".into()))?;
    let store = ControlPlane::open(&config.store, &keys).await?;
    let rewrapped = async {
        check_master_keys(&store, &keys).await?;
        let changed = store.rewrap_data_keys().await.map_err(secret_setup)?;
        tracing::info!(changed, master_key = %keys.current_id(), "rewrapped the data keys");
        Ok(Rewrapped {
            changed,
            master_key: keys.current_id(),
        })
    }
    .await;
    store.close().await;
    rewrapped
}

fn secret_setup(error: SecretError) -> StartupError {
    StartupError::Setup(error.to_string())
}

/// Refuses to start while a data key is held under a master key this server was not given
/// (`archi/04-tools.md`, "Secrets"), naming its id: the operator gives it in
/// `--previous-master-keys` until `--rewrap-data-keys` has run, so that it is not removed too
/// early.
async fn check_master_keys(store: &ControlPlane, keys: &MasterKeyRing) -> Result<(), StartupError> {
    let in_use = store.master_keys_in_use().await.map_err(secret_setup)?;
    let current = keys.current_id();
    let previous = keys.previous_ids();
    if let Some(unknown) = in_use
        .iter()
        .find(|id| **id != current && !previous.contains(id))
    {
        return Err(secret_setup(SecretError::UnknownMasterKey(unknown.clone())));
    }
    for id in &previous {
        if in_use.contains(id) {
            tracing::warn!(
                master_key = %id,
                "data keys are still held under a previous master key: run rewind server \
                 --rewrap-data-keys"
            );
        } else {
            tracing::info!(
                master_key = %id,
                "no data key is held under this previous master key: it can be removed from \
                 --previous-master-keys"
            );
        }
    }
    Ok(())
}

/// Starts a server in this process whose identity provider is `identity`, the engine's fake of
/// WorkOS, instead of WorkOS: the server's tests. The four `WORKOS_*` are still required for
/// the sign-in to exist (the cookie password and the redirect URI are used; the client id and
/// API key are not).
pub async fn start_with_identity(
    args: ServerArgs,
    identity: MemoryIdentity,
) -> Result<Running, StartupError> {
    start_wired(args, Some(identity)).await
}

/// Starts a server in this process whose control plane is the engine's in-memory adapters of
/// `CatalogStore`, `AccessStore`, `SecretStore` and `Cluster`, the reference implementations of
/// their contracts, instead of `--store`: the server's tests. Its bucket is `--bucket`, which is
/// required. With `identity`, as [`start_with_identity`].
pub async fn start_in_memory(
    mut args: ServerArgs,
    identity: Option<MemoryIdentity>,
) -> Result<Running, StartupError> {
    args.in_memory = true;
    start_wired(args, identity).await
}

/// The dashboard's sign-in, when the configuration has WorkOS.
fn sign_in(
    config: &Config,
    memory: Option<MemoryIdentity>,
) -> Result<Option<SignIn>, StartupError> {
    let Some(workos) = &config.workos else {
        return Ok(None);
    };
    let identity = match memory {
        Some(memory) => Identity::Memory(memory),
        None => Identity::WorkOs(
            rewind_workos::WorkOs::new(&workos.client_id, &workos.api_key)
                .map_err(|error| StartupError::Config(format!("WorkOS: {error}")))?,
        ),
    };
    Ok(Some(SignIn {
        identity,
        seal: workos.cookie_seal.clone(),
        redirect_uri: workos.redirect_uri.clone(),
    }))
}

fn setup(error: impl std::fmt::Display) -> StartupError {
    StartupError::Setup(error.to_string())
}

async fn start_wired(
    args: ServerArgs,
    memory: Option<MemoryIdentity>,
) -> Result<Running, StartupError> {
    let config = args.resolve()?;
    if config.rewrap_data_keys {
        return Err(StartupError::Config(
            "--rewrap-data-keys rewraps the data keys and exits: it does not serve \
             (rewind_server::rewrap_data_keys)"
                .into(),
        ));
    }
    let sign_in = sign_in(&config, memory)?;
    let directory = match &config.listen {
        Listen::Embedded { dir } => Some(Directory::open(dir)?),
        Listen::Tcp { .. } => None,
    };
    let master_keys = match (&directory, &config.master_keys) {
        (Some(directory), _) => MasterKeyRing::new(directory.master_key),
        (None, Some(keys)) => keys.clone(),
        (None, None) => return Err(StartupError::Config("no master key".into())),
    };
    let store = ControlPlane::open(&config.store, &master_keys).await?;
    check_master_keys(&store, &master_keys).await?;
    let placed = store
        .place_namespaces(config.partitions)
        .await
        .map_err(setup)?;
    if placed > 0 {
        tracing::info!(placed, "namespaces placed in their partitions");
    }
    let opened = bucket::open(&config.bucket)?;
    let clock = clock(&config, &store).await?;
    let idle = Idle::new();
    let executor = Executor {
        v8: V8Executor::new(SandboxLimits::default()).map_err(setup)?,
        idle: idle.clone(),
    };
    let epochs = executor.epochs().iter().copied().collect();
    let tools = Tools::new(config.tool_egress.clone()).map_err(StartupError::Setup)?;
    let exporter = Exporter::new(
        config.otlp.as_ref(),
        &telemetry::service(config.node.as_str()),
    )
    .map_err(StartupError::Setup)?;
    let webhook_egress = if config.allow_private_webhooks {
        WebhookEgress::Open
    } else {
        WebhookEgress::public()
    };
    let rendered: Arc<OnceLock<Weak<App>>> = Arc::new(OnceLock::new());
    let webhooks = HttpWebhooks::new(
        ServerRender {
            app: Arc::clone(&rendered),
        },
        // One attempt: the owner tries a failed delivery again on a timer.
        Retries {
            attempts: 1,
            ..Retries::default()
        },
        webhook_egress.clone(),
    );
    let plane = Arc::new(Wiring {
        catalog: store.catalog(),
        partitions: opened.partitions,
        archive: opened.archive,
        blobs: opened.blobs,
        executor,
        dispatcher: tools.dispatcher(),
        secrets: store.clone(),
        spans: exporter,
        webhooks,
        clock,
        entropy: SystemEntropy,
    });
    let build = format!("rewind {VERSION}");
    let mut node_config = NodeConfig::new(config.node.clone(), config.partitions);
    node_config.build.clone_from(&build);
    node_config.pool.clone_from(&config.pool);
    node_config.pools.clone_from(&config.pools);
    node_config.executors_only = config.executors_only;
    node_config.executors = config.executors;
    node_config.calls = config.calls;
    node_config.cache = config.cache;
    node_config.public_url.clone_from(&config.public_url);
    node_config.default_ttl = config.default_ttl;
    node_config.max_ttl = config.max_ttl;
    if plane.clock.is_virtual() {
        // In virtual time the node jumps to its next timer: the wheel holds them all, as the
        // driver's does.
        node_config.horizon = Duration::from_secs(100 * 365 * 86_400);
    }
    // The node link's listener is bound before the node joins: its address is the node's.
    let linked = match &config.listen {
        Listen::Tcp {
            link: Some(link), ..
        } => {
            let listener = tokio::net::TcpListener::bind(link.listen)
                .await
                .map_err(|error| {
                    StartupError::Io(format!("cannot listen on {}: {error}", link.listen))
                })?;
            let bound = listener
                .local_addr()
                .map_err(|error| StartupError::Io(error.to_string()))?;
            let secret = ClusterSecret::new(link.secret.clone()).map_err(setup)?;
            let address = link
                .address
                .clone()
                .unwrap_or_else(|| crate::config::default_link_address(bound));
            Some((listener, bound, secret, address))
        }
        _ => None,
    };
    let address = linked
        .as_ref()
        .map_or_else(|| "local".to_owned(), |(_, _, _, address)| address.clone());
    let cluster = store
        .cluster(config.partitions, &config.cluster_database)
        .await
        .map_err(setup)?;
    let membership: SharedMembership<ServerMembership> = SharedMembership::new(
        cluster
            .join(NodeInfo {
                id: config.node.clone(),
                address,
                build,
                epochs,
                pool: config.pool.clone(),
                executors_only: config.executors_only,
            })
            .await
            .map_err(setup)?,
    );
    let (map, control, changes) = control_plane(&membership).await?;
    let link = Arc::new(match &linked {
        Some((_, _, secret, _)) => {
            ServerLink::Http(HttpLink::new(membership.members(), secret.clone()).map_err(setup)?)
        }
        None => ServerLink::Local(LocalLink::new()),
    });
    let node = if plane.clock.is_virtual() {
        runtime::spawn_in_virtual_time(
            Arc::clone(&plane),
            node_config.clone(),
            membership.clone(),
            Arc::clone(&link),
            Arc::new(plane.clock.clone()),
        )
    } else {
        runtime::spawn(
            Arc::clone(&plane),
            node_config.clone(),
            membership.clone(),
            Arc::clone(&link),
        )
    };
    if let ServerLink::Local(local) = &*link {
        local.register(node.clone());
    }
    let settings = Settings {
        version: VERSION,
        embedded: config.embedded(),
        ceiling: config.ceiling.clone(),
        retention: config.retention,
        allow_stdio: config.allow_stdio,
        public_url: config.public_url.clone(),
        webhook_egress,
    };
    let oauth = match (&config.public_url, &sign_in) {
        (Some(public_url), Some(sign_in)) => Some(
            OAuthServer::new(
                public_url,
                &sign_in.redirect_uri,
                config.internal_hosts.clone(),
            )
            .map_err(StartupError::Config)?,
        ),
        _ => None,
    };
    let app = Arc::new(App::new(Parts {
        plane,
        node,
        config: node_config,
        store,
        membership,
        link,
        map,
        control: control.clone(),
        checker: checker::checker(config.tsgo_cache.clone()),
        tools,
        settings,
        idle,
        sign_in,
        oauth,
    }));
    let _ = rendered.set(Arc::downgrade(&app));
    register_admin_key(&app, &config).await?;

    let mut tasks = vec![tokio::spawn(changes)];
    tasks.push(tokio::spawn(jobs::daily(Arc::clone(&app))));
    if app.clock().is_virtual() {
        tasks.push(tokio::spawn(keep_virtual_time(Arc::clone(&app))));
    }
    let (mut listeners, addr, socket) = listen(&app, &config, directory.as_ref()).await?;
    let mut link_addr = None;
    if let Some((listener, bound, secret, _)) = linked {
        let mut stop = app.stop.subscribe();
        listeners.push(crate::link::serve(
            listener,
            app.node.clone(),
            secret,
            async move {
                let _ = stop.wait_for(|stop| *stop).await;
            },
        ));
        link_addr = Some(bound);
    }
    if let Some(timeout) = config.idle_timeout {
        tasks.push(tokio::spawn(stop_when_idle(Arc::clone(&app), timeout)));
    }
    tracing::info!(
        version = VERSION,
        node = %app.node.node(),
        partitions = config.partitions.get(),
        addr = ?addr,
        socket = ?socket,
        executors_only = config.executors_only,
        virtual_time = app.clock().is_virtual(),
        "Rewind server started"
    );
    Ok(Running {
        app,
        addr,
        link_addr,
        socket,
        listeners,
        tasks,
        directory,
    })
}

/// The partition map as it is now and as each change gives it, and the changes of the control
/// plane fanned out to the process: the task that keeps them.
async fn control_plane(
    membership: &SharedMembership<ServerMembership>,
) -> Result<
    (
        watch::Receiver<PartitionMap>,
        broadcast::Sender<Control>,
        impl std::future::Future<Output = ()> + Send + 'static,
    ),
    StartupError,
> {
    let mut changes = membership.changes().await.map_err(setup)?;
    let initial = membership.partition_map().await.map_err(setup)?;
    let (map_sink, map) = watch::channel(initial);
    let (control, _) = broadcast::channel(4_096);
    let sink = control.clone();
    let membership = membership.clone();
    let task = async move {
        loop {
            while let Some(change) = changes.next().await {
                if let ControlPlaneChange::PartitionMap(moved) = &change {
                    map_sink.send_replace(moved.clone());
                }
                let _ = sink.send(Control::Change(change));
            }
            // The stream ended: changes may have been missed. Listen again, read the map again,
            // and tell whoever cached something.
            let _ = sink.send(Control::Lost);
            loop {
                match membership.changes().await {
                    Ok(listening) => {
                        changes = listening;
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the control plane's changes cannot be read");
                        // A backoff after an error.
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            if let Ok(current) = membership.partition_map().await {
                map_sink.send_replace(current);
            }
        }
    };
    Ok((map, control, task))
}

/// Real or virtual time, as the command line and the store say.
async fn clock(config: &Config, store: &ControlPlane) -> Result<ServerClock, StartupError> {
    let saved = store.virtual_time().await.map_err(setup)?;
    Ok(match (config.virtual_time, saved) {
        (Some(false), _) => {
            store.set_virtual_time(None).await.map_err(setup)?;
            ServerClock::System
        }
        (Some(true), saved) => {
            let at = saved.unwrap_or_else(|| SystemClock.now());
            store.set_virtual_time(Some(at)).await.map_err(setup)?;
            ServerClock::virtual_at(at)
        }
        (None, Some(at)) => ServerClock::virtual_at(at),
        (None, None) => ServerClock::System,
    })
}

/// In virtual time, saves where the clock stands each time the node moves it, so that a server
/// started again goes on from there.
async fn keep_virtual_time(app: Arc<App>) {
    let Some(mut moved) = app.clock().watch() else {
        return;
    };
    let mut saved: Option<Timestamp> = None;
    while moved.changed().await.is_ok() {
        let at = *moved.borrow_and_update();
        if saved != Some(at) {
            if let Err(error) = app.store.set_virtual_time(Some(at)).await {
                tracing::warn!(%error, "the virtual time was not saved");
            }
            saved = Some(at);
            app.idle.touch();
        }
    }
}

/// The name of the key given with `--admin-key`.
const ADMIN_KEY_NAME: &str = "operator key (--admin-key)";

/// Registers `--admin-key`, the first key of a deployment: the key, and its grant of the role
/// `operator`, written again if it was changed.
async fn register_admin_key(app: &App, config: &Config) -> Result<(), StartupError> {
    let Some(key) = &config.admin_key else {
        return Ok(());
    };
    let setup = |error: crate::error::ApiError| StartupError::Setup(error.0.detail.clone());
    let store = |error: StoreError| StartupError::Setup(error.to_string());
    let hash = key_hash(key);
    let tenant = match app.store.find_key(hash).await.map_err(store)? {
        Some(existing) if existing.revoked_at.is_some() => {
            tracing::warn!("--admin-key was revoked: it is not registered again");
            return Ok(());
        }
        Some(existing) => existing.tenant,
        None => {
            app.ensure_tenant(&config.admin_tenant)
                .await
                .map_err(setup)?;
            app.store
                .put_key(&ApiKey {
                    hash,
                    tenant: config.admin_tenant.clone(),
                    name: ADMIN_KEY_NAME.to_owned(),
                    created_by: None,
                    created_at: app.now(),
                    revoked_at: None,
                })
                .await
                .map_err(store)?;
            config.admin_tenant.clone()
        }
    };
    let principal = Principal::Key(hash);
    let granted = app.store.grant(&tenant, &principal).await.map_err(store)?;
    let operator = |grant: &Grant| grant.role == OPERATOR && grant.allowed_labels.is_empty();
    if !granted.as_ref().is_some_and(operator) {
        app.store
            .put_grant(
                &Grant {
                    tenant,
                    principal,
                    role: OPERATOR.to_owned(),
                    allowed_labels: std::collections::BTreeMap::new(),
                    granted_by: None,
                    granted_at: app.now(),
                },
                None,
            )
            .await
            .map_err(store)?;
    }
    Ok(())
}

type Listening = (Vec<JoinHandle<()>>, Option<SocketAddr>, Option<PathBuf>);

async fn listen(
    app: &Arc<App>,
    config: &Config,
    directory: Option<&Directory>,
) -> Result<Listening, StartupError> {
    let shutdown = || {
        let mut stop = app.stop.subscribe();
        async move {
            let _ = stop.wait_for(|stop| *stop).await;
        }
    };
    match (&config.listen, directory) {
        (Listen::Embedded { .. }, Some(directory)) => {
            use std::os::unix::fs::PermissionsExt;
            let socket = directory.socket.clone();
            let listener = tokio::net::UnixListener::bind(&socket).map_err(|error| {
                StartupError::Io(format!("cannot listen on {}: {error}", socket.display()))
            })?;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| StartupError::Io(format!("{}: {error}", socket.display())))?;
            let router = http::router(app.clone(), true, None);
            let shutdown = shutdown();
            let handle = tokio::spawn(async move {
                if let Err(error) = axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown)
                    .await
                {
                    tracing::error!(%error, "the socket listener failed");
                }
            });
            Ok((vec![handle], None, Some(socket)))
        }
        (Listen::Tcp { edge, .. }, _) => {
            let mut handles = Vec::new();
            let mut bound = None;
            if let Some(addr) = edge {
                let listener = tokio::net::TcpListener::bind(addr).await.map_err(|error| {
                    StartupError::Io(format!("cannot listen on {addr}: {error}"))
                })?;
                let addr = listener
                    .local_addr()
                    .map_err(|error| StartupError::Io(error.to_string()))?;
                // `/mcp` calls `/v1` on this listener.
                let mcp = crate::mcp::router(app.clone(), addr).map_err(StartupError::Setup)?;
                let router = http::router(app.clone(), false, Some(mcp));
                let shutdown = shutdown();
                handles.push(tokio::spawn(async move {
                    // The peer's address: the sign-in passes a browser's to the identity
                    // provider.
                    let service = router.into_make_service_with_connect_info::<SocketAddr>();
                    if let Err(error) = axum::serve(listener, service)
                        .with_graceful_shutdown(shutdown)
                        .await
                    {
                        tracing::error!(%error, "the TCP listener failed");
                    }
                }));
                bound = Some(addr);
            }
            Ok((handles, bound, None))
        }
        (Listen::Embedded { .. }, None) => Err(StartupError::Setup(
            "the embedded server has no directory".into(),
        )),
    }
}

/// Stops the server once nothing has happened for `timeout`: no request, no stream, no
/// activation.
async fn stop_when_idle(app: Arc<App>, timeout: Duration) {
    app.idle.until_idle(timeout).await;
    tracing::info!(?timeout, "stopping: idle");
    app.stop.send_replace(true);
}
