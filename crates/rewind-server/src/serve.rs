//! Starting and stopping the server: the adapters wired by configuration, the engine, the
//! listeners, the worker's loops, and a stop that lets the work in flight commit.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rewind_api::Problem;
use rewind_api::access::OPERATOR;
use rewind_api::admin::ServerRole;
use rewind_engine::adapters::{MemoryIdentity, SystemClock, SystemEntropy, VirtualClock};
use rewind_engine::ports::{AccessStore, ApiKey, Clock, Grant, Principal, StoreError, TaskFilter};
use rewind_engine::projector::ProjectorConfig;
use rewind_engine::{Adapters, Engine, EngineConfig};
use rewind_runtime::{SandboxLimits, V8Executor};
use tokio::task::JoinHandle;

use crate::app::{App, Settings, SignIn};
use crate::auth::key_hash;
use crate::config::{Config, Listen, ServerArgs};
use crate::connections::{self, McpCaller};
use crate::deliveries::{Background, Deliveries, PER_SUBSCRIPTION};
use crate::embedded::Directory;
use crate::oauth::OAuthServer;
use crate::wiring::blobs::Blobs;
use crate::wiring::checker;
use crate::wiring::clock::ServerClock;
use crate::wiring::identity::Identity;
use crate::wiring::store::Store;
use crate::wiring::telemetry::{self, Exporter};
use crate::wiring::tools::Tools;
use crate::wiring::webhooks::{HttpWebhooks, Retries, ServerRender, WebhookEgress};
use crate::{http, worker};

/// The version of the `rewind` binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long the work in flight may take to commit when the server stops.
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
    /// The address of the TCP listener.
    addr: Option<SocketAddr>,
    /// The embedded server's socket.
    socket: Option<PathBuf>,
    listener: JoinHandle<()>,
    workers: Vec<JoinHandle<()>>,
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

    /// The embedded server's socket.
    pub fn socket(&self) -> Option<&PathBuf> {
        self.socket.as_ref()
    }

    /// Waits until something stops the server: `POST /v1/server/stop`, or the idle timeout.
    /// Who calls `/mcp` with `bearer`, and the organizations it reaches with the permissions it
    /// may use in each (`archi/07-code-architecture.md`, "MCP and OAuth"): a connection's access
    /// token, an API key or an end user's token. A member's access token, or anything else, is
    /// refused (`unauthorized`).
    pub async fn authenticate_mcp(&self, bearer: &str) -> Result<McpCaller, Box<Problem>> {
        connections::mcp_caller_or_problem(&self.app, bearer).await
    }

    pub async fn stopped(&self) {
        let mut stop = self.app.stop.subscribe();
        let _ = stop.wait_for(|stop| *stop).await;
    }

    /// Stops the server: no new request, no new work; the work in flight commits, and the
    /// embedded server's socket and lock are released.
    pub async fn stop(self) {
        self.app.stop.send_replace(true);
        self.app.wake.bump();
        let drain = async {
            let _ = self.listener.await;
            for worker in self.workers {
                let _ = worker.await;
            }
        };
        if tokio::time::timeout(DRAIN, drain).await.is_err() {
            tracing::warn!("the work in flight did not finish within {DRAIN:?}");
        }
        // The engine's webhooks still in progress are not sent: none is in the journal.
        self.app.deliveries.abort();
        self.app.tools.close();
        self.app.exporter.shutdown().await;
        self.app.store.close().await;
        if let Some(socket) = &self.socket {
            let _ = std::fs::remove_file(socket);
        }
        drop(self.directory);
        tracing::info!("stopped");
    }
}

/// Starts a server in this process.
pub async fn start(args: ServerArgs) -> Result<Running, StartupError> {
    start_wired(args, None).await
}

/// Starts a server in this process whose identity provider is `identity`, the engine's fake of
/// WorkOS, instead of WorkOS: the server's tests. The four `WORKOS_*` are still required for
/// the sign-in to exist (the cookie password and the redirect URI are used; the client id and
/// API key are not), and only the api role uses it.
pub async fn start_with_identity(
    args: ServerArgs,
    identity: MemoryIdentity,
) -> Result<Running, StartupError> {
    start_wired(args, Some(identity)).await
}

/// Starts a server in this process on the engine's in-memory adapters of `JournalStore`,
/// `AccessStore`, `ReadModel` and `SecretStore`, the reference implementations of their
/// contracts, instead of `--store`: the server's tests. With `identity`, as
/// [`start_with_identity`]. Its blobs are in `--blobs`, which is required.
pub async fn start_in_memory(
    mut args: ServerArgs,
    identity: Option<MemoryIdentity>,
) -> Result<Running, StartupError> {
    args.in_memory = true;
    start_wired(args, identity).await
}

/// The dashboard's sign-in, when the configuration has WorkOS (on the api role).
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

async fn start_wired(
    args: ServerArgs,
    memory: Option<MemoryIdentity>,
) -> Result<Running, StartupError> {
    let config = args.resolve()?;
    let sign_in = sign_in(&config, memory)?;
    let directory = match &config.listen {
        Listen::Embedded { dir } => Some(Directory::open(dir)?),
        Listen::Tcp(_) => None,
    };
    let master_key = match (&directory, config.master_key) {
        (Some(directory), _) => directory.master_key,
        (None, Some(key)) => key,
        (None, None) => return Err(StartupError::Config("no master key".into())),
    };
    let store = Store::open(&config.store, master_key).await?;
    let blobs = Blobs::open(&config.blobs)?;
    let clock = clock(&config, &store).await?;
    let executor = V8Executor::new(SandboxLimits::default())
        .map_err(|error| StartupError::Setup(error.to_string()))?;
    let tools = Tools::new(config.mcp_egress.clone()).map_err(StartupError::Setup)?;
    let exporter = Exporter::new(
        config.otlp.as_ref(),
        &telemetry::service(&config.worker_name),
    )
    .map_err(StartupError::Setup)?;
    let webhook_egress = if config.allow_private_webhooks {
        WebhookEgress::Open
    } else {
        WebhookEgress::public()
    };
    let deliveries = Arc::new(Deliveries::new(
        HttpWebhooks::new(
            ServerRender {
                store: store.clone(),
                blobs: blobs.clone(),
            },
            Retries::default(),
            webhook_egress.clone(),
        ),
        PER_SUBSCRIPTION,
    ));
    let engine_config = EngineConfig {
        worker: config.worker_name.clone(),
        tenants: config.tenants.clone(),
        tool_tasks: TaskFilter::Dispatched {
            tenants: config.tenants.clone(),
        },
        public_url: config.public_url.clone(),
        build: format!("rewind {VERSION}"),
        default_ttl: config.default_ttl,
        max_ttl: config.max_ttl,
        cache: config.cache,
        ..EngineConfig::default()
    };
    let engine = Engine::new(
        Adapters {
            journal: store.clone(),
            blobs: blobs.clone(),
            executor,
            dispatcher: tools.dispatcher(),
            clock,
            entropy: SystemEntropy,
            secrets: store.clone(),
            webhooks: Background(deliveries.clone()),
        },
        engine_config,
    );
    let mut roles = Vec::new();
    if config.api {
        roles.push(ServerRole::Api);
    }
    if config.worker {
        roles.push(ServerRole::Worker);
    }
    let settings = Settings {
        version: VERSION,
        embedded: config.embedded(),
        roles,
        ceiling: config.ceiling.clone(),
        retention: config.retention,
        allow_stdio: config.allow_stdio,
        public_url: config.public_url.clone(),
        webhook_egress,
        projector: ProjectorConfig {
            owner: config.worker_name.clone(),
            ..ProjectorConfig::default()
        },
        changes_per_pass: config.changes_per_pass,
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
    let app = Arc::new(App::new(
        engine,
        store,
        blobs,
        checker::checker(config.tsgo_cache.clone()),
        tools,
        exporter,
        deliveries,
        settings,
        sign_in,
        oauth,
    ));
    register_admin_key(&app, &config).await?;

    let workers = if config.worker {
        worker::spawn(&app, config.activations, config.dispatches)
    } else {
        Vec::new()
    };
    let (listener, addr, socket) = listen(&app, &config, directory.as_ref()).await?;
    let mut workers = workers;
    if let Some(timeout) = config.idle_timeout {
        workers.push(tokio::spawn(stop_when_idle(app.clone(), timeout)));
    }
    tracing::info!(
        version = VERSION,
        addr = ?addr,
        socket = ?socket,
        worker = config.worker,
        virtual_time = app.engine.clock().is_virtual(),
        "Rewind server started"
    );
    Ok(Running {
        app,
        addr,
        socket,
        listener,
        workers,
        directory,
    })
}

/// Real or virtual time, as the command line and the store say.
async fn clock(config: &Config, store: &Store) -> Result<ServerClock, StartupError> {
    let setup = |error: rewind_engine::ports::StoreError| StartupError::Setup(error.to_string());
    let saved = store.virtual_time().await.map_err(setup)?;
    Ok(match (config.virtual_time, saved) {
        (Some(false), _) => {
            store.set_virtual_time(None).await.map_err(setup)?;
            ServerClock::System
        }
        (Some(true), saved) => {
            let at = saved.unwrap_or_else(|| SystemClock.now());
            store.set_virtual_time(Some(at)).await.map_err(setup)?;
            ServerClock::Virtual(Arc::new(VirtualClock::new(at)))
        }
        (None, Some(at)) => ServerClock::Virtual(Arc::new(VirtualClock::new(at))),
        (None, None) => ServerClock::System,
    })
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

type Listening = (JoinHandle<()>, Option<SocketAddr>, Option<PathBuf>);

async fn listen(
    app: &Arc<App>,
    config: &Config,
    directory: Option<&Directory>,
) -> Result<Listening, StartupError> {
    let mut stop = app.stop.subscribe();
    let shutdown = async move {
        let _ = stop.wait_for(|stop| *stop).await;
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
            let handle = tokio::spawn(async move {
                if let Err(error) = axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown)
                    .await
                {
                    tracing::error!(%error, "the socket listener failed");
                }
            });
            Ok((handle, None, Some(socket)))
        }
        (Listen::Tcp(addr), _) => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(|error| StartupError::Io(format!("cannot listen on {addr}: {error}")))?;
            let addr = listener
                .local_addr()
                .map_err(|error| StartupError::Io(error.to_string()))?;
            // `/mcp` calls `/v1` on this listener.
            let mcp = crate::mcp::router(app.clone(), addr).map_err(StartupError::Setup)?;
            let router = http::router(app.clone(), false, Some(mcp));
            let handle = tokio::spawn(async move {
                // The peer's address: the sign-in passes a browser's to the identity provider.
                let service = router.into_make_service_with_connect_info::<SocketAddr>();
                if let Err(error) = axum::serve(listener, service)
                    .with_graceful_shutdown(shutdown)
                    .await
                {
                    tracing::error!(%error, "the TCP listener failed");
                }
            });
            Ok((handle, Some(addr), None))
        }
        (Listen::Embedded { .. }, None) => Err(StartupError::Setup(
            "the embedded server has no directory".into(),
        )),
    }
}

/// Stops the server once nothing has happened for `timeout`: no request, no stream, no work.
async fn stop_when_idle(app: Arc<App>, timeout: Duration) {
    let check = (timeout / 4).clamp(Duration::from_millis(100), Duration::from_secs(5));
    let mut stop = app.stop.subscribe();
    while !app.stopping() {
        let _ = tokio::time::timeout(check, stop.changed()).await;
        if app.activity.idle_for().is_some_and(|idle| idle >= timeout) {
            tracing::info!(?timeout, "stopping: idle");
            app.stop.send_replace(true);
            app.wake.bump();
        }
    }
}
