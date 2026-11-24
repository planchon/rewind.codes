//! The command line of `rewind server` and what it resolves to.
//!
//! ```text
//! rewind server --embedded [--dir .rewind] [--virtual-time | --real-time] [--idle-timeout 10m]
//! rewind server --store postgres://… --bucket s3://<bucket> --cluster-secret … [--partitions 64]
//!            [--listen 0.0.0.0:8080] [--link-listen 0.0.0.0:7070 --link-address <host>:7070]
//!            [--executors-only] [--pool <name>] [--public-url https://…] …
//! rewind server --store … --master-key <new> --previous-master-keys <old> --rewrap-data-keys
//! rewind server migrate-runs --store postgres://… --bucket … [the same options]
//! ```
//!
//! Every option also reads an environment variable (`REWIND_*`), for deployments; WorkOS's four
//! keep WorkOS's names (`WORKOS_*`, `archi/07-code-architecture.md`, "Sign-in (`/auth`)"). The
//! secrets (`--master-key`, `--previous-master-keys`, `--admin-key`, `--cluster-secret`, S3 keys,
//! WorkOS's API key and cookie password) are never printed: a master key is named by its id, the
//! first 8 bytes of its SHA-256 in hex.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Parser, ValueEnum};
use rewind_engine::CacheConfig;
use rewind_engine::ports::{MasterKeyId, NodeId, Quotas, Retention, TenantId};
use rewind_outbound::{InternalAllowList, ToolEgress};

use crate::wiring::bucket::{BucketLocation, S3Options};
use crate::wiring::control::StoreLocation;

/// A configuration that cannot be served, with what to change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(String);

impl ConfigError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The partitions of a deployment, unless `--partitions` says otherwise.
pub(crate) const PARTITIONS: u16 = 64;

/// `rewind server`: a node of the run plane and its HTTP edge. Runs in the foreground until
/// stopped.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "rewind server",
    about = "Runs a Rewind node in the foreground: the HTTP API /v1, the dashboard, the owners of \
             its partitions and its executors",
    version
)]
pub struct ServerArgs {
    /// The embedded server of a `.rewind/` directory: one node owning the one partition, SlateDB
    /// on local files and Turso in the directory, the API on its unix socket `server.sock` (no
    /// key, tenant `local`).
    #[arg(long)]
    pub embedded: bool,

    /// The `.rewind/` directory of the embedded server.
    #[arg(long, env = "REWIND_DIR", default_value = ".rewind")]
    pub dir: PathBuf,

    /// Stop after this long without requests and without work in progress (the CLI starts its
    /// embedded server with `10m`). Without it, the server never stops on idle.
    #[arg(long, value_parser = parse_duration)]
    pub idle_timeout: Option<Duration>,

    /// Run in virtual time from now on: timers fire as soon as nothing else can advance. The
    /// choice is kept in the store; `--real-time` goes back.
    #[arg(long, conflicts_with = "real_time")]
    pub virtual_time: bool,

    /// Run in real time from now on.
    #[arg(long)]
    pub real_time: bool,

    /// Write `ready` on standard output once the server listens, then send standard output where
    /// standard error goes: the CLI that starts the embedded server waits for it on the pipe it
    /// gave as standard output.
    #[arg(long, hide = true)]
    pub notify_ready: bool,

    /// Run activations sent by the owners of other nodes, for the epochs of this build, and own
    /// no partition: no HTTP route but the node link (`archi/06-storage-and-scale.md`,
    /// "Sandboxed nodes"). Kept up after an upgrade for the runs of the previous epoch.
    #[arg(long, env = "REWIND_EXECUTORS_ONLY")]
    pub executors_only: bool,

    /// The pool of executors this node belongs to: the owners send the activations of the
    /// tenants of this pool here. The shared pool without it.
    #[arg(long, env = "REWIND_POOL")]
    pub pool: Option<String>,

    /// A tenant whose activations run on a pool of its own, as `<tenant>=<pool>`: the owners send
    /// them to the nodes started with `--pool <pool>`. Repeat, or separate with commas; give the
    /// same list to every node.
    #[arg(
        long = "tenant-pool",
        env = "REWIND_TENANT_POOLS",
        value_delimiter = ','
    )]
    pub tenant_pools: Vec<String>,

    /// Where the HTTP API listens.
    #[arg(long, env = "REWIND_LISTEN", default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,

    /// The control plane: `turso:<path>` for a single node, or `postgres://…` for a deployment.
    #[arg(long, env = "REWIND_STORE", hide_env_values = true)]
    pub store: Option<String>,

    /// The database of the same Postgres server where the nodes take their liveness locks,
    /// where nothing else lives (a node waiting on a lock holds back the cleanup of that
    /// database only).
    #[arg(long, env = "REWIND_CLUSTER_DATABASE", default_value = "postgres")]
    pub cluster_database: String,

    /// The partitions, the archive and the blobs: a directory, or `s3://<bucket>` (AWS S3 or any
    /// S3-compatible service, with `--s3-*`). Next to a Turso database by default.
    #[arg(long, env = "REWIND_BUCKET")]
    pub bucket: Option<String>,

    /// The endpoint of an S3-compatible service (R2, MinIO…).
    #[arg(long, env = "REWIND_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,

    /// The bucket's region; `auto` for R2.
    #[arg(long, env = "REWIND_S3_REGION")]
    pub s3_region: Option<String>,

    /// Allows an `http://` S3 endpoint, on a trusted network only.
    #[arg(long, env = "REWIND_S3_ALLOW_HTTP")]
    pub s3_allow_http: bool,

    #[arg(long, env = "REWIND_S3_ACCESS_KEY_ID", hide_env_values = true)]
    pub s3_access_key_id: Option<String>,

    #[arg(long, env = "REWIND_S3_SECRET_ACCESS_KEY", hide_env_values = true)]
    pub s3_secret_access_key: Option<String>,

    /// The partitions of the deployment: 64 by default, 1 embedded. Fixed once a deployment
    /// holds runs: more only serve the namespaces created after them.
    #[arg(long, env = "REWIND_PARTITIONS")]
    pub partitions: Option<u16>,

    /// The secret every node of the deployment shares: it authenticates the node link.
    /// Required with Postgres.
    #[arg(long, env = "REWIND_CLUSTER_SECRET", hide_env_values = true)]
    pub cluster_secret: Option<String>,

    /// Where the node link listens, on the private network.
    #[arg(long, env = "REWIND_LINK_LISTEN", default_value = "0.0.0.0:7070")]
    pub link_listen: SocketAddr,

    /// Where the other nodes reach this node's link: `host:port` on the private network. The
    /// host name and the port of `--link-listen` by default.
    #[arg(long, env = "REWIND_LINK_ADDRESS")]
    pub link_address: Option<String>,

    /// This node's id: 1 to 64 letters, digits, `-`, `_` and `.`. The host name and the process
    /// id by default.
    #[arg(long, env = "REWIND_NODE_ID")]
    pub node_id: Option<String>,

    /// The master key of the secret store: 32 bytes in base64. It encrypts each tenant's data
    /// key. The embedded server keeps its own in `.rewind/master.key`.
    #[arg(long, env = "REWIND_MASTER_KEY", hide_env_values = true)]
    pub master_key: Option<String>,

    /// The master keys `--master-key` replaced, 32 bytes in base64 each, separated by commas:
    /// they only decrypt the data keys still held under them, until `--rewrap-data-keys` has
    /// run. A server refuses to start while a data key is held under a key it was not given.
    #[arg(
        long,
        env = "REWIND_PREVIOUS_MASTER_KEYS",
        hide_env_values = true,
        value_delimiter = ','
    )]
    pub previous_master_keys: Vec<String>,

    /// Encrypts with `--master-key` every data key held under one of `--previous-master-keys`,
    /// one tenant at a time, prints how many it changed, and exits without serving. The secrets
    /// and the private keys are not encrypted again; servers may keep running meanwhile.
    #[arg(long)]
    pub rewrap_data_keys: bool,

    /// An API key (`rewind_sk_…`) with the role `operator`, registered at start: the first key,
    /// from which the others are made with `POST /v1/api-keys`.
    #[arg(long, env = "REWIND_ADMIN_KEY", hide_env_values = true)]
    pub admin_key: Option<String>,

    /// The tenant of `--admin-key`.
    #[arg(long, env = "REWIND_ADMIN_TENANT", default_value = "default")]
    pub admin_tenant: String,

    /// The URL clients reach this server at: `webhook(name)` returns `{public-url}/hooks/…`.
    /// Without it, scripts cannot get a webhook URL.
    #[arg(long, env = "REWIND_PUBLIC_URL")]
    pub public_url: Option<String>,

    /// Activations run at once on this node: its executor threads. The machine's cores by
    /// default.
    #[arg(long, env = "REWIND_EXECUTORS")]
    pub executors: Option<usize>,

    /// Tool calls sent at once by this node.
    #[arg(long, env = "REWIND_CALLS", default_value_t = 256)]
    pub calls: usize,

    /// The ceiling of the tenants this server creates (`archi/06-storage-and-scale.md`,
    /// "Quotas"): the runs active at once across their namespaces; one more is refused with 429.
    /// The operator changes a tenant's ceiling with `PUT /v1/organizations/{tenant}/quotas`.
    #[arg(long, env = "REWIND_QUOTA_ACTIVE_RUNS")]
    pub quota_active_runs: Option<u32>,

    /// The ceiling of the tenants this server creates: the runs created in a calendar month,
    /// forks included.
    #[arg(long, env = "REWIND_QUOTA_RUNS_PER_MONTH")]
    pub quota_runs_per_month: Option<u64>,

    /// The ceiling of the tenants this server creates: the CPU time of their scripts in a
    /// calendar month, such as `100h`. Never refused: crossing 80 % and 100 % sends
    /// `quota.alert`.
    #[arg(long, env = "REWIND_QUOTA_CPU_PER_MONTH", value_parser = parse_duration)]
    pub quota_cpu_per_month: Option<Duration>,

    /// The ceiling of the tenants this server creates: the distinct scripts with a run kept,
    /// summed over their namespaces.
    #[arg(long, env = "REWIND_QUOTA_SCRIPTS")]
    pub quota_scripts: Option<u64>,

    /// The ceiling of the tenants this server creates: the longest TTL a run may ask for in any
    /// of their namespaces.
    #[arg(long, env = "REWIND_QUOTA_MAX_TTL", value_parser = parse_duration)]
    pub quota_max_ttl: Option<Duration>,

    /// The ceiling of the tenants this server creates: the most lines a script may have in any
    /// of their namespaces.
    #[arg(long, env = "REWIND_QUOTA_MAX_SCRIPT_LINES")]
    pub quota_max_script_lines: Option<u32>,

    /// Accept action sets with stdio MCP servers: processes this server starts. The embedded
    /// server always does.
    #[arg(long, env = "REWIND_ALLOW_STDIO")]
    pub allow_stdio: bool,

    /// An internal host or network whose MCP servers and APIs action sets may name: a host name
    /// (`mcp.internal.example`), its subdomains (`*.svc.cluster.local`), an address or a CIDR
    /// network (`10.20.0.0/16`). Repeat, or separate with commas. Without it, a server reaches
    /// MCP servers, APIs and OpenAPI documents only at public addresses: private, loopback,
    /// link-local and cloud-metadata addresses are refused. What such an internal server
    /// answers is never quoted to the tenant. The embedded server reaches any address.
    #[arg(
        long = "allow-internal-tools",
        env = "REWIND_ALLOW_INTERNAL_TOOLS",
        value_delimiter = ','
    )]
    pub allow_internal_tools: Vec<String>,

    /// Send lifecycle webhooks to private, loopback and link-local addresses too, over
    /// `http://` or `https://`: for a harness on the same trusted network. Without it, a server
    /// sends them only to `https://` URLs with public addresses. The embedded server always
    /// does.
    #[arg(long, env = "REWIND_ALLOW_PRIVATE_WEBHOOKS")]
    pub allow_private_webhooks: bool,

    /// Where tsgo, the type-checker, is extracted. The user's cache directory by default.
    #[arg(long, env = "REWIND_TSGO_CACHE")]
    pub tsgo_cache: Option<PathBuf>,

    /// What is logged on standard error, as `RUST_LOG` writes it.
    #[arg(long, env = "REWIND_LOG", default_value = "info")]
    pub log: String,

    /// Sends service and run telemetry over OTLP to this collector, such as
    /// `http://localhost:4318`. The `OTEL_EXPORTER_OTLP_*` variables work too.
    #[arg(long, env = "REWIND_OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,

    /// The OTLP transport: `grpc`, or `http` (protobuf over HTTP, the default).
    #[arg(long, value_enum, env = "REWIND_OTLP_PROTOCOL")]
    pub otlp_protocol: Option<OtlpArg>,

    /// The TTL of a run started without one: 30 days by default.
    #[arg(long, env = "REWIND_DEFAULT_TTL", value_parser = parse_duration)]
    pub default_ttl: Option<Duration>,

    /// The longest TTL a run may ask for: 365 days by default. After an upgrade that changes the
    /// determinism epoch, the executors of the previous epoch stay up at most this long.
    #[arg(long, env = "REWIND_MAX_TTL", value_parser = parse_duration)]
    pub max_ttl: Option<Duration>,

    /// The script instances a node keeps in memory after their activation (sticky
    /// execution): 500 by default. 0 turns the cache off, and every activation replays.
    #[arg(long, env = "REWIND_CACHE_INSTANCES")]
    pub cache_instances: Option<usize>,

    /// The memory the cached instances may hold together, as V8 measures their heaps: `4GiB`
    /// by default, or `512MiB`, or bytes. Keep it well below the node's memory limit.
    #[arg(long, env = "REWIND_CACHE_MEMORY", value_parser = parse_bytes)]
    pub cache_memory: Option<u64>,

    /// For the tenants this server creates: how long an ended run (its summary, its sealed
    /// journals and its payloads) is kept after it ended. 365 days by default.
    #[arg(long, env = "REWIND_RETENTION", value_parser = parse_duration)]
    pub retention: Option<Duration>,

    /// The WorkOS environment's client, `client_…`: with the three other `WORKOS_*`, the
    /// dashboard's sign-in (`/auth`) and members' access tokens on `/v1`. The four are given
    /// together or not at all; without them the server accepts only API keys. Never given to a
    /// node started with `--executors-only`; the embedded server ignores them.
    #[arg(long, env = "WORKOS_CLIENT_ID")]
    pub workos_client_id: Option<String>,

    /// The WorkOS environment's secret key, `sk_…`.
    #[arg(long, env = "WORKOS_API_KEY", hide_env_values = true)]
    pub workos_api_key: Option<String>,

    /// 32 random bytes in hexadecimal (64 characters): the key of the sealed sign-in cookies.
    /// Changing it signs everyone out.
    #[arg(long, env = "WORKOS_COOKIE_PASSWORD", hide_env_values = true)]
    pub workos_cookie_password: Option<String>,

    /// Where WorkOS sends the browser back after GitHub: the dashboard's origin and
    /// `/auth/callback` (`https://app.rewind.codes/auth/callback`), registered in WorkOS.
    #[arg(long, env = "WORKOS_REDIRECT_URI")]
    pub workos_redirect_uri: Option<String>,

    /// The control plane on the engine's in-memory adapters instead of `--store`: set by
    /// `rewind_server::start_in_memory`, for the server's tests, never from the command line.
    #[arg(skip)]
    pub(crate) in_memory: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OtlpArg {
    Grpc,
    Http,
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    text.parse::<rewind_api::common::Duration>()
        .map(Into::into)
        .map_err(|error| error.to_string())
}

/// A size in bytes: a number, optionally followed by `KiB`, `MiB` or `GiB`.
fn parse_bytes(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("{text:?}: expected a size such as 512MiB or 4GiB"))?;
    let unit: u64 = match unit.trim() {
        "" | "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        other => {
            return Err(format!(
                "{text:?}: unknown unit {other:?}; use B, KiB, MiB or GiB"
            ));
        }
    };
    number
        .checked_mul(unit)
        .ok_or_else(|| format!("{text:?} is too large"))
}

/// The WorkOS environment of the dashboard's sign-in: the four `WORKOS_*`, checked.
#[derive(Clone)]
pub(crate) struct WorkOsConfig {
    pub(crate) client_id: String,
    pub(crate) api_key: String,
    pub(crate) cookie_seal: rewind_workos::CookieSeal,
    pub(crate) redirect_uri: String,
}

impl fmt::Debug for WorkOsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkOsConfig")
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl ServerArgs {
    /// The four `WORKOS_*`: all of them, or none (`None`).
    fn workos(&self) -> Result<Option<WorkOsConfig>, ConfigError> {
        let given = [
            ("WORKOS_CLIENT_ID", &self.workos_client_id),
            ("WORKOS_API_KEY", &self.workos_api_key),
            ("WORKOS_COOKIE_PASSWORD", &self.workos_cookie_password),
            ("WORKOS_REDIRECT_URI", &self.workos_redirect_uri),
        ];
        let missing: Vec<&str> = given
            .iter()
            .filter(|(_, value)| value.as_deref().is_none_or(|value| value.trim().is_empty()))
            .map(|(name, _)| *name)
            .collect();
        if missing.len() == given.len() {
            return Ok(None);
        }
        if !missing.is_empty() {
            return Err(ConfigError::new(format!(
                "the four WORKOS_* variables are given together or not at all: {} missing. \
                 Without them the server has no sign-in and accepts only API keys",
                missing.join(", ")
            )));
        }
        let value = |value: &Option<String>| value.as_deref().unwrap_or_default().trim().to_owned();
        let cookie_seal = rewind_workos::CookieSeal::new(&value(&self.workos_cookie_password))
            .map_err(|_| {
                ConfigError::new(
                    "WORKOS_COOKIE_PASSWORD must be 32 random bytes in hexadecimal: 64 \
                     characters 0-9 and a-f (openssl rand -hex 32)",
                )
            })?;
        let redirect_uri = value(&self.workos_redirect_uri);
        if !(redirect_uri.starts_with("https://") || redirect_uri.starts_with("http://")) {
            return Err(ConfigError::new(
                "WORKOS_REDIRECT_URI must be an http:// or https:// URL: the dashboard's origin \
                 and /auth/callback, as registered in WorkOS",
            ));
        }
        Ok(Some(WorkOsConfig {
            client_id: value(&self.workos_client_id),
            api_key: value(&self.workos_api_key),
            cookie_seal,
            redirect_uri,
        }))
    }
}

/// The master keys of the secret store (`archi/04-tools.md`, "Secrets": the master key can be
/// replaced): the current one, which encrypts, and the previous ones, which only decrypt. `Debug`
/// shows their ids, never a key.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MasterKeyRing {
    pub(crate) current: [u8; 32],
    /// Never the current one, and no key twice.
    pub(crate) previous: Vec<[u8; 32]>,
}

impl MasterKeyRing {
    pub(crate) fn new(current: [u8; 32]) -> Self {
        Self {
            current,
            previous: Vec::new(),
        }
    }

    pub(crate) fn with_previous(mut self, previous: impl IntoIterator<Item = [u8; 32]>) -> Self {
        for key in previous {
            if key != self.current && !self.previous.contains(&key) {
                self.previous.push(key);
            }
        }
        self
    }

    pub(crate) fn current_id(&self) -> MasterKeyId {
        MasterKeyId::of(&self.current)
    }

    pub(crate) fn previous_ids(&self) -> Vec<MasterKeyId> {
        self.previous
            .iter()
            .map(|key| MasterKeyId::of(key))
            .collect()
    }
}

impl fmt::Debug for MasterKeyRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MasterKeyRing")
            .field("current", &self.current_id())
            .field("previous", &self.previous_ids())
            .finish()
    }
}

/// How the server listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Listen {
    /// The embedded server: the unix socket of its directory, the node link in the process.
    Embedded { dir: PathBuf },
    /// A deployed node: the HTTP edge on TCP (none with `--executors-only`), and the node link
    /// on its own port when the deployment has a cluster secret.
    Tcp {
        edge: Option<SocketAddr>,
        link: Option<LinkListen>,
    },
}

/// The node link of a deployed node (`archi/06-storage-and-scale.md`, "The edge").
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LinkListen {
    pub(crate) listen: SocketAddr,
    /// What the other nodes dial: `host:port`; without it, the host name and the port the link
    /// listens on (the address itself when it is not unspecified).
    pub(crate) address: Option<String>,
    pub(crate) secret: String,
}

impl fmt::Debug for LinkListen {
    /// The secret is never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkListen")
            .field("listen", &self.listen)
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

/// What the command line resolves to.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) listen: Listen,
    pub(crate) store: StoreLocation,
    pub(crate) bucket: BucketLocation,
    pub(crate) partitions: NonZeroU16,
    pub(crate) cluster_database: String,
    pub(crate) node: NodeId,
    pub(crate) executors_only: bool,
    pub(crate) pool: Option<String>,
    /// The tenants whose activations run on a pool of their own.
    pub(crate) pools: BTreeMap<TenantId, String>,
    pub(crate) executors: usize,
    pub(crate) calls: usize,
    /// `None` for the embedded server, which keeps its own.
    pub(crate) master_keys: Option<MasterKeyRing>,
    /// `--rewrap-data-keys`: rewrap, print, exit.
    pub(crate) rewrap_data_keys: bool,
    pub(crate) admin_key: Option<String>,
    pub(crate) admin_tenant: TenantId,
    pub(crate) public_url: Option<String>,
    /// The ceiling of the tenants this server creates.
    pub(crate) ceiling: Quotas,
    pub(crate) allow_stdio: bool,
    /// Where MCP servers, APIs and OpenAPI documents may be: anywhere for the embedded server;
    /// public addresses and the internal hosts of `--allow-internal-tools` for a deployed one.
    pub(crate) tool_egress: ToolEgress,
    /// The internal hosts and networks of `--allow-internal-tools`: MCP servers, APIs, and the
    /// metadata documents of OAuth clients, may be there.
    pub(crate) internal_hosts: InternalAllowList,
    /// Lifecycle webhooks may go to the private network, over `http://` too.
    pub(crate) allow_private_webhooks: bool,
    /// `Some(true)`: switch to virtual time; `Some(false)`: to real time; `None`: as the store
    /// says.
    pub(crate) virtual_time: Option<bool>,
    pub(crate) idle_timeout: Option<Duration>,
    pub(crate) tsgo_cache: Option<PathBuf>,
    /// Run telemetry over OTLP, when configured.
    pub(crate) otlp: Option<rewind_otel::OtlpConfig>,
    pub(crate) default_ttl: Duration,
    pub(crate) max_ttl: Duration,
    /// The bounds of the node's sticky instance cache.
    pub(crate) cache: CacheConfig,
    /// The retention of the tenants this server creates.
    pub(crate) retention: Retention,
    /// The dashboard's sign-in: `None` without WorkOS, on an executors-only node and on the
    /// embedded server.
    pub(crate) workos: Option<WorkOsConfig>,
}

impl Config {
    pub(crate) fn embedded(&self) -> bool {
        matches!(self.listen, Listen::Embedded { .. })
    }
}

/// A run's TTL without one, and the longest one, by default.
const DEFAULT_TTL: Duration = Duration::from_secs(30 * 86_400);
const MAX_TTL: Duration = Duration::from_secs(365 * 86_400);

impl ServerArgs {
    pub(crate) fn resolve(self) -> Result<Config, ConfigError> {
        let s3 = S3Options {
            bucket: String::new(),
            endpoint: self.s3_endpoint.clone(),
            region: self.s3_region.clone(),
            allow_http: self.s3_allow_http,
            access_key_id: self.s3_access_key_id.clone(),
            secret_access_key: self.s3_secret_access_key.clone(),
        };
        let virtual_time = if self.virtual_time {
            Some(true)
        } else if self.real_time {
            Some(false)
        } else {
            None
        };
        let node = match &self.node_id {
            Some(id) => NodeId::new(id.clone()).map_err(|error| {
                ConfigError::new(format!("--node-id (REWIND_NODE_ID): {error}"))
            })?,
            None => default_node_id(),
        };
        let otlp = crate::wiring::telemetry::otlp(&self);
        let executors = self
            .executors
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, usize::from));
        if executors == 0 || self.calls == 0 {
            return Err(ConfigError::new(
                "--executors and --calls must be at least 1",
            ));
        }
        let default_ttl = self.default_ttl.unwrap_or(DEFAULT_TTL);
        let max_ttl = self.max_ttl.unwrap_or(MAX_TTL);
        if default_ttl.is_zero() || default_ttl > max_ttl {
            return Err(ConfigError::new(format!(
                "--default-ttl ({}) must be above zero and at most --max-ttl ({})",
                rewind_api::common::Duration::from(default_ttl),
                rewind_api::common::Duration::from(max_ttl),
            )));
        }
        let defaults = CacheConfig::default();
        let cache = CacheConfig {
            max_instances: self.cache_instances.unwrap_or(defaults.max_instances),
            max_bytes: self.cache_memory.unwrap_or(defaults.max_bytes),
        };
        let mut retention = Retention::default();
        if let Some(kept) = self.retention {
            if kept.is_zero() {
                return Err(ConfigError::new("--retention must be above zero"));
            }
            retention.runs = kept;
        }
        if let Some(url) = &self.public_url
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            return Err(ConfigError::new(format!(
                "--public-url {url:?}: expected an http:// or https:// URL"
            )));
        }
        let mut pools = BTreeMap::new();
        for pair in self
            .tenant_pools
            .iter()
            .map(|pair| pair.trim())
            .filter(|pair| !pair.is_empty())
        {
            let (tenant, pool) = pair.split_once('=').ok_or_else(|| {
                ConfigError::new(format!("--tenant-pool {pair:?}: expected <tenant>=<pool>"))
            })?;
            let tenant = TenantId::new(tenant.trim().to_owned())
                .map_err(|error| ConfigError::new(format!("--tenant-pool {pair:?}: {error}")))?;
            pools.insert(tenant, pool.trim().to_owned());
        }
        let partitions = |default: u16| {
            NonZeroU16::new(self.partitions.unwrap_or(default)).ok_or_else(|| {
                ConfigError::new("--partitions must be at least 1: a deployment has 64")
            })
        };

        if self.embedded {
            if self.rewrap_data_keys {
                return Err(ConfigError::new(
                    "the embedded server keeps its master key in --dir: --rewrap-data-keys does \
                     not apply to it",
                ));
            }
            if self.store.is_some() || self.bucket.is_some() {
                return Err(ConfigError::new(
                    "the embedded server keeps everything in --dir: --store and --bucket do not \
                     apply to it",
                ));
            }
            if self.executors_only || self.pool.is_some() {
                return Err(ConfigError::new(
                    "the embedded server is one node that owns its partition: --executors-only \
                     and --pool do not apply to it",
                ));
            }
            if self.partitions.is_some_and(|count| count != 1) {
                return Err(ConfigError::new(
                    "the embedded server has one partition: --partitions does not apply to it",
                ));
            }
            if !self.allow_internal_tools.is_empty() {
                return Err(ConfigError::new(
                    "the embedded server reaches MCP servers and APIs at any address: \
                     --allow-internal-tools does not apply to it",
                ));
            }
            let dir = self.dir.clone();
            return Ok(Config {
                store: StoreLocation::Turso(dir.join("control.db")),
                bucket: BucketLocation::Files(dir.clone()),
                partitions: NonZeroU16::MIN,
                cluster_database: self.cluster_database,
                listen: Listen::Embedded { dir },
                node,
                executors_only: false,
                pool: None,
                pools: BTreeMap::new(),
                executors,
                calls: self.calls,
                master_keys: None,
                rewrap_data_keys: false,
                admin_key: None,
                admin_tenant: TenantId::local(),
                public_url: self
                    .public_url
                    .map(|url| url.trim_end_matches('/').to_owned()),
                ceiling: Quotas::default(),
                allow_stdio: true,
                tool_egress: ToolEgress::open(),
                internal_hosts: InternalAllowList::default(),
                allow_private_webhooks: true,
                virtual_time,
                idle_timeout: self.idle_timeout,
                tsgo_cache: self.tsgo_cache,
                otlp,
                default_ttl,
                max_ttl,
                cache,
                retention,
                // The embedded server's socket takes no token: WorkOS's variables, which a
                // developer's shell may hold for another project, do not apply to it.
                workos: None,
            });
        }

        let internal_tools = InternalAllowList::parse(&self.allow_internal_tools)
            .map_err(|error| ConfigError::new(format!("--allow-internal-tools: {error}")))?;
        let store = if self.in_memory {
            StoreLocation::Memory
        } else {
            let store_text = self.store.clone().ok_or_else(|| {
                ConfigError::new(
                    "--store is required (postgres://… or turso:<path>), or run the embedded \
                     server with --embedded",
                )
            })?;
            StoreLocation::parse(&store_text)?
        };
        let bucket = match &self.bucket {
            Some(bucket) => BucketLocation::parse(bucket, s3)?,
            None => match &store {
                StoreLocation::Turso(path) => BucketLocation::Files(
                    path.parent()
                        .map_or_else(|| PathBuf::from("."), PathBuf::from),
                ),
                StoreLocation::Postgres(_) => {
                    return Err(ConfigError::new(
                        "--bucket is required with Postgres: s3://<bucket> (AWS S3 or any \
                         S3-compatible service), where every node reads the partitions",
                    ));
                }
                StoreLocation::Memory => {
                    return Err(ConfigError::new(
                        "--bucket is required with the in-memory control plane: a directory",
                    ));
                }
            },
        };
        if matches!(store, StoreLocation::Postgres(_)) && virtual_time == Some(true) {
            return Err(ConfigError::new(
                "--virtual-time is for a single node on Turso: a deployment on Postgres runs in \
                 real time",
            ));
        }
        let link = match (&store, self.cluster_secret.as_deref().map(str::trim)) {
            (_, Some(secret)) if secret.len() < 32 => {
                return Err(ConfigError::new(
                    "--cluster-secret must be at least 32 characters: it authenticates the node \
                     link (openssl rand -hex 32)",
                ));
            }
            (StoreLocation::Postgres(_), None) => {
                return Err(ConfigError::new(
                    "--cluster-secret (REWIND_CLUSTER_SECRET) is required with Postgres: the \
                     nodes of the deployment reach each other through the node link it \
                     authenticates",
                ));
            }
            (StoreLocation::Postgres(_), Some(secret)) => Some(LinkListen {
                listen: self.link_listen,
                address: self.link_address.clone(),
                secret: secret.to_owned(),
            }),
            _ if self.executors_only || self.pool.is_some() || !self.tenant_pools.is_empty() => {
                return Err(ConfigError::new(
                    "--executors-only, --pool and --tenant-pool are for a deployment of several \
                     nodes on Postgres",
                ));
            }
            _ => None,
        };
        let master_key = self.master_key.as_deref().ok_or_else(|| {
            ConfigError::new(
                "--master-key (REWIND_MASTER_KEY) is required: 32 random bytes in base64, which \
                 encrypt the secrets",
            )
        })?;
        let master_key = parse_master_key(master_key)?;
        let previous = self
            .previous_master_keys
            .iter()
            .map(|key| key.trim())
            .filter(|key| !key.is_empty())
            .enumerate()
            .map(|(at, key)| {
                parse_master_key(key).map_err(|error| {
                    ConfigError::new(format!(
                        "--previous-master-keys (REWIND_PREVIOUS_MASTER_KEYS), key {}: {error}",
                        at + 1
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let master_keys = MasterKeyRing::new(master_key).with_previous(previous);
        let admin_tenant = TenantId::new(self.admin_tenant.clone())
            .map_err(|error| ConfigError::new(error.to_string()))?;
        if let Some(key) = &self.admin_key
            && (!key.starts_with(rewind_api::headers::API_KEY_PREFIX) || key.len() < 32)
        {
            return Err(ConfigError::new(
                "--admin-key must be a key rewind_sk_… of at least 32 characters",
            ));
        }
        let workos = self.workos()?;
        if workos.is_some() && self.executors_only {
            tracing::warn!(
                "the WORKOS_* variables are set on an executors-only node, which serves no HTTP \
                 route: they are not used, and WORKOS_API_KEY belongs on the other nodes only"
            );
        }
        let partitions = partitions(match store {
            StoreLocation::Turso(_) => 1,
            _ => PARTITIONS,
        })?;
        Ok(Config {
            listen: Listen::Tcp {
                edge: (!self.executors_only).then_some(self.listen),
                link,
            },
            store,
            bucket,
            partitions,
            cluster_database: self.cluster_database,
            node,
            executors_only: self.executors_only,
            pool: self.pool,
            pools,
            executors,
            calls: self.calls,
            master_keys: Some(master_keys),
            rewrap_data_keys: self.rewrap_data_keys,
            admin_key: self.admin_key,
            admin_tenant,
            public_url: self
                .public_url
                .map(|url| url.trim_end_matches('/').to_owned()),
            ceiling: Quotas {
                cpu_per_month: self.quota_cpu_per_month,
                active_runs: self.quota_active_runs,
                runs_per_month: self.quota_runs_per_month,
                scripts: self.quota_scripts,
                max_ttl: self.quota_max_ttl,
                max_script_lines: self.quota_max_script_lines,
            },
            allow_stdio: self.allow_stdio,
            tool_egress: ToolEgress::checked(internal_tools.clone()),
            internal_hosts: internal_tools,
            allow_private_webhooks: self.allow_private_webhooks,
            virtual_time,
            idle_timeout: self.idle_timeout,
            tsgo_cache: self.tsgo_cache,
            otlp,
            default_ttl,
            max_ttl,
            cache,
            retention,
            workos: workos.filter(|_| !self.executors_only),
        })
    }
}

/// Reads a master key: 32 bytes in base64. An error never quotes the text, which may be a key.
pub(crate) fn parse_master_key(text: &str) -> Result<[u8; 32], ConfigError> {
    let bytes = BASE64
        .decode(text.trim())
        .map_err(|_| ConfigError::new("the master key is not base64"))?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
        ConfigError::new(format!(
            "the master key has {} bytes: it must have 32",
            bytes.len()
        ))
    })
}

/// The host name, the characters a node id refuses replaced, and the process id.
pub(crate) fn default_node_id() -> NodeId {
    let host: String = host_name()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    NodeId::new(format!("{host}-{}", std::process::id()))
        .unwrap_or_else(|_| NodeId::new(format!("node-{}", std::process::id())).expect("valid"))
}

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "node".into())
}

/// The host name and the port the link listens on.
pub(crate) fn default_link_address(listen: SocketAddr) -> String {
    if listen.ip().is_unspecified() {
        format!("{}:{}", host_name(), listen.port())
    } else {
        listen.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, ConfigError> {
        let args = ServerArgs::try_parse_from(
            std::iter::once("rewind server").chain(args.iter().copied()),
        )
        .map_err(|error| ConfigError::new(error.to_string()))?;
        args.resolve()
    }

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_embedded_server_keeps_everything_in_its_directory() {
        let config = parse(&[
            "--embedded",
            "--dir",
            "/tmp/x/.rewind",
            "--idle-timeout",
            "10m",
        ])
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            config.store,
            StoreLocation::Turso(PathBuf::from("/tmp/x/.rewind/control.db"))
        );
        assert!(
            matches!(&config.bucket, BucketLocation::Files(dir) if dir == &PathBuf::from("/tmp/x/.rewind"))
        );
        assert_eq!(config.partitions.get(), 1);
        assert!(config.embedded() && !config.executors_only);
        assert_eq!(config.idle_timeout, Some(Duration::from_secs(600)));
        assert!(parse(&["--embedded", "--executors-only"]).is_err());
        assert!(parse(&["--embedded", "--store", "turso:x.db"]).is_err());
        assert!(parse(&["--embedded", "--partitions", "4"]).is_err());
    }

    #[test]
    fn a_deployed_node_needs_a_store_a_bucket_a_secret_and_a_master_key() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--store", "turso:/tmp/j/control.db"]).is_err());
        let key = BASE64.encode([7u8; 32]);
        let single = parse(&["--store", "turso:/tmp/j/control.db", "--master-key", &key])
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(single.partitions.get(), 1);
        assert_eq!(single.master_keys, Some(MasterKeyRing::new([7u8; 32])));
        assert!(matches!(
            single.listen,
            Listen::Tcp {
                edge: Some(_),
                link: None
            }
        ));
        assert!(
            parse(&["--store", "postgres://x", "--master-key", &key]).is_err(),
            "Postgres needs --bucket"
        );
        let postgres = [
            "--store",
            "postgres://u:p@localhost/rewind",
            "--bucket",
            "s3://rewind",
            "--master-key",
            &key,
        ];
        assert!(parse(&postgres).is_err(), "Postgres needs --cluster-secret");
        let mut linked = postgres.to_vec();
        linked.extend(["--cluster-secret", SECRET, "--link-address", "node-a:7070"]);
        let node = parse(&linked).unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(node.store, StoreLocation::Postgres(_)));
        assert_eq!(node.partitions.get(), PARTITIONS);
        let Listen::Tcp {
            link: Some(link), ..
        } = &node.listen
        else {
            panic!("a node on Postgres listens on its link");
        };
        assert_eq!(link.address.as_deref(), Some("node-a:7070"));
        assert!(
            !format!("{node:?}").contains(SECRET),
            "the secret is not printed"
        );
        let mut executors = linked.clone();
        executors.extend([
            "--executors-only",
            "--pool",
            "acme",
            "--tenant-pool",
            "acme=acme, beta = shared-b",
        ]);
        let executors = parse(&executors).unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            executors.listen,
            Listen::Tcp {
                edge: None,
                link: Some(_)
            }
        ));
        assert_eq!(executors.pool.as_deref(), Some("acme"));
        assert_eq!(
            executors
                .pools
                .iter()
                .map(|(tenant, pool)| (tenant.to_string(), pool.as_str()))
                .collect::<Vec<_>>(),
            [("acme".to_owned(), "acme"), ("beta".to_owned(), "shared-b")]
        );
        let mut wrong = linked.clone();
        wrong.extend(["--tenant-pool", "acme"]);
        assert!(parse(&wrong).is_err(), "a tenant pool is <tenant>=<pool>");
        assert!(
            parse(&[
                "--store",
                "turso:/tmp/j.db",
                "--master-key",
                &key,
                "--tenant-pool",
                "acme=a"
            ])
            .is_err(),
            "a single node has no pools"
        );
        let mut short = postgres.to_vec();
        short.extend(["--cluster-secret", "short"]);
        assert!(parse(&short).is_err());
        assert!(parse(&["--store", "mysql://x", "--master-key", &key]).is_err());
        assert!(parse(&["--store", "turso:/tmp/j.db", "--master-key", "short"]).is_err());
        assert!(
            parse(&[
                "--store",
                "turso:/tmp/j.db",
                "--master-key",
                &key,
                "--executors-only"
            ])
            .is_err(),
            "a single node owns its partitions"
        );
    }

    #[test]
    fn ttls_cache_and_retention_have_defaults_and_can_be_set() {
        let key = BASE64.encode([7u8; 32]);
        let base = ["--store", "turso:/tmp/j.db", "--master-key", key.as_str()];
        let config = parse(&base).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(config.default_ttl, DEFAULT_TTL);
        assert_eq!(config.max_ttl, MAX_TTL);
        assert_eq!(config.cache, CacheConfig::default());
        assert_eq!(config.retention, Retention::default());

        let mut args = base.to_vec();
        args.extend([
            "--default-ttl",
            "1d",
            "--max-ttl",
            "7d",
            "--cache-instances",
            "50",
            "--cache-memory",
            "512MiB",
            "--retention",
            "30d",
        ]);
        let config = parse(&args).unwrap_or_else(|error| panic!("{error}"));
        const DAY: u64 = 86_400;
        assert_eq!(config.default_ttl, Duration::from_secs(DAY));
        assert_eq!(config.max_ttl, Duration::from_secs(7 * DAY));
        assert_eq!(
            config.cache,
            CacheConfig {
                max_instances: 50,
                max_bytes: 512 << 20
            }
        );
        assert_eq!(config.retention.runs, Duration::from_secs(30 * DAY));

        let mut longer = base.to_vec();
        longer.extend(["--default-ttl", "8d", "--max-ttl", "7d"]);
        assert!(parse(&longer).is_err(), "the default TTL above the maximum");
    }

    #[test]
    fn workos_takes_its_four_variables_or_none() {
        let key = BASE64.encode([7u8; 32]);
        let password = "0123456789abcdef".repeat(4);
        let all = [
            ("--workos-client-id", "client_01JTEST"),
            ("--workos-api-key", "sk_test_secret"),
            ("--workos-cookie-password", password.as_str()),
            (
                "--workos-redirect-uri",
                "https://app.rewind.codes/auth/callback",
            ),
        ];
        let deployed = |workos: &[(&str, &str)], extra: &[&str]| {
            let mut args = vec![
                "--store",
                "postgres://u:p@localhost/rewind",
                "--bucket",
                "s3://rewind",
                "--cluster-secret",
                SECRET,
                "--master-key",
                &key,
            ];
            args.extend_from_slice(extra);
            for (flag, value) in workos {
                args.extend([*flag, *value]);
            }
            parse(&args)
        };
        let none = deployed(&[], &[]).unwrap_or_else(|error| panic!("{error}"));
        assert!(none.workos.is_none());
        let configured = deployed(&all, &[]).unwrap_or_else(|error| panic!("{error}"));
        let workos = configured.workos.expect("WorkOS is configured");
        assert_eq!(workos.client_id, "client_01JTEST");
        assert!(!format!("{workos:?}").contains("sk_test_secret"));
        for left_out in 0..all.len() {
            let some: Vec<_> = all
                .iter()
                .enumerate()
                .filter(|(at, _)| *at != left_out)
                .map(|(_, flag)| *flag)
                .collect();
            let error = deployed(&some, &[]).unwrap_err().to_string();
            assert!(error.contains("together or not at all"), "{error}");
        }
        let mut short = all;
        short[2].1 = "abcd";
        let error = deployed(&short, &[]).unwrap_err().to_string();
        assert!(error.contains("WORKOS_COOKIE_PASSWORD"), "{error}");
        assert!(
            !error.contains("abcd"),
            "the password is not printed: {error}"
        );
        let mut relative = all;
        relative[3].1 = "/auth/callback";
        assert!(deployed(&relative, &[]).is_err());

        let executors =
            deployed(&all, &["--executors-only"]).unwrap_or_else(|error| panic!("{error}"));
        assert!(
            executors.workos.is_none(),
            "an executors-only node serves no HTTP route"
        );
        let mut embedded = vec!["--embedded"];
        for (flag, value) in &all[..2] {
            embedded.extend([*flag, *value]);
        }
        let embedded = parse(&embedded).unwrap_or_else(|error| panic!("{error}"));
        assert!(
            embedded.workos.is_none(),
            "the embedded server ignores them"
        );
    }

    #[test]
    fn previous_master_keys_only_decrypt_and_are_never_printed() {
        let (current, old, older) = (
            BASE64.encode([7u8; 32]),
            BASE64.encode([8u8; 32]),
            BASE64.encode([9u8; 32]),
        );
        let previous = format!("{old}, {older},{current},{old}");
        let config = parse(&[
            "--store",
            "turso:/tmp/j.db",
            "--master-key",
            &current,
            "--previous-master-keys",
            &previous,
            "--rewrap-data-keys",
        ])
        .unwrap_or_else(|error| panic!("{error}"));
        let keys = config.master_keys.clone().expect("master keys");
        assert_eq!(keys.current, [7u8; 32]);
        assert_eq!(
            keys.previous,
            [[8u8; 32], [9u8; 32]],
            "in order, without the current key nor twice the same"
        );
        assert!(config.rewrap_data_keys);
        let printed = format!("{config:?}");
        for key in [&current, &old, &older] {
            assert!(!printed.contains(key.as_str()), "{printed}");
        }
        assert!(!printed.contains("[7, 7, 7"), "{printed}");
        assert!(
            printed.contains(MasterKeyId::of(&[8u8; 32]).as_str()),
            "{printed}"
        );

        let invalid = parse(&[
            "--store",
            "turso:/tmp/j.db",
            "--master-key",
            &current,
            "--previous-master-keys",
            "c2VjcmV0LWJ1dC1zaG9ydA==",
        ])
        .unwrap_err()
        .to_string();
        assert!(invalid.contains("key 1"), "{invalid}");
        assert!(
            !invalid.contains("c2VjcmV0"),
            "the key is not printed: {invalid}"
        );
        let not_base64 = parse(&["--store", "turso:/tmp/j.db", "--master-key", "s3cr3t!!"])
            .unwrap_err()
            .to_string();
        assert!(!not_base64.contains("s3cr3t"), "{not_base64}");
        assert!(
            parse(&["--embedded", "--rewrap-data-keys"]).is_err(),
            "the embedded server keeps its master key"
        );
    }

    #[test]
    fn sizes_take_binary_units() {
        assert_eq!(parse_bytes("1024"), Ok(1024));
        assert_eq!(parse_bytes("4GiB"), Ok(4 << 30));
        assert_eq!(parse_bytes("512 MiB"), Ok(512 << 20));
        assert_eq!(parse_bytes("8KiB"), Ok(8 << 10));
        assert!(parse_bytes("4GB").is_err());
        assert!(parse_bytes("GiB").is_err());
        assert!(parse_bytes("99999999999999GiB").is_err());
    }

    #[test]
    fn node_ids_default_to_the_host_and_the_process() {
        let id = default_node_id();
        assert!(id.as_str().ends_with(&format!("-{}", std::process::id())));
        let key = BASE64.encode([7u8; 32]);
        assert!(
            parse(&[
                "--store",
                "turso:/tmp/j.db",
                "--master-key",
                &key,
                "--node-id",
                "a b"
            ])
            .is_err()
        );
    }

    #[test]
    fn only_a_deployed_server_checks_where_its_mcp_servers_and_apis_are() {
        let key = BASE64.encode([7u8; 32]);
        let deployed = |extra: &[&str]| {
            let mut args = vec!["--store", "turso:/tmp/j.db", "--master-key", &key];
            args.extend_from_slice(extra);
            parse(&args)
        };
        let checked = deployed(&[]).unwrap_or_else(|error| panic!("{error}"));
        assert!(!checked.tool_egress.is_open());
        let allowed = deployed(&[
            "--allow-internal-tools",
            "mcp.internal.example,10.20.0.0/16",
            "--allow-internal-tools",
            "*.svc.cluster.local",
        ])
        .unwrap_or_else(|error| panic!("{error}"));
        assert!(!allowed.tool_egress.is_open());
        let invalid = deployed(&["--allow-internal-tools", "10.0.0.0/33"]).unwrap_err();
        assert!(
            invalid
                .to_string()
                .starts_with("--allow-internal-tools: \"10.0.0.0/33\""),
            "{invalid}"
        );

        let embedded = parse(&["--embedded"]).unwrap_or_else(|error| panic!("{error}"));
        assert!(embedded.tool_egress.is_open());
        assert!(parse(&["--embedded", "--allow-internal-tools", "10.0.0.0/8"]).is_err());
    }
}
