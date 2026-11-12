//! The command line of `ptc server` and what it resolves to.
//!
//! ```text
//! ptc server --embedded [--dir .ptc] [--virtual-time | --real-time] [--idle-timeout 10m]
//! ptc server --store turso:<path> [--blobs <dir> | s3://<bucket>] [--listen 127.0.0.1:8080]
//!            [--role api|worker]… [--public-url https://…] …
//! ```
//!
//! Every option also reads an environment variable (`PTC_*`), for deployments. The secrets
//! (`--master-key`, `--admin-key`, S3 keys) are never printed.

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Parser, ValueEnum};
use ptc_engine::ports::{Retention, TenantId, TenantScope};
use ptc_engine::{CacheConfig, EngineConfig};
use ptc_mcp::{InternalAllowList, McpEgress};

use crate::wiring::blobs::{BlobLocation, S3Options};
use crate::wiring::store::StoreLocation;

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

/// `ptc server`: the engine host. Runs in the foreground until stopped.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "ptc server",
    about = "Runs the ai-ptc server in the foreground: the HTTP API /v1, the workers, the dashboard",
    version
)]
pub struct ServerArgs {
    /// The embedded server of a `.ptc/` directory: Turso and local files in the directory, the
    /// API on its unix socket `server.sock` (no key, tenant `local`), both roles.
    #[arg(long)]
    pub embedded: bool,

    /// The `.ptc/` directory of the embedded server.
    #[arg(long, env = "PTC_DIR", default_value = ".ptc")]
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

    /// The roles of this process: `api` (HTTP API, no script runs) and `worker` (activations,
    /// tool dispatch, scheduler, projector, retention). Both by default.
    #[arg(long = "role", value_enum, value_delimiter = ',')]
    pub roles: Vec<RoleArg>,

    /// Where the HTTP API listens.
    #[arg(long, env = "PTC_LISTEN", default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,

    /// The store: `turso:<path>`, or `postgres://…` for a deployment.
    #[arg(long, env = "PTC_STORE", hide_env_values = true)]
    pub store: Option<String>,

    /// Large payloads and archives: a directory, or `s3://<bucket>`. Next to a Turso database
    /// by default.
    #[arg(long, env = "PTC_BLOBS")]
    pub blobs: Option<String>,

    /// The endpoint of an S3-compatible service (MinIO, R2…).
    #[arg(long, env = "PTC_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,

    #[arg(long, env = "PTC_S3_REGION")]
    pub s3_region: Option<String>,

    /// Allows an `http://` S3 endpoint, on a trusted network only.
    #[arg(long, env = "PTC_S3_ALLOW_HTTP")]
    pub s3_allow_http: bool,

    #[arg(long, env = "PTC_S3_ACCESS_KEY_ID", hide_env_values = true)]
    pub s3_access_key_id: Option<String>,

    #[arg(long, env = "PTC_S3_SECRET_ACCESS_KEY", hide_env_values = true)]
    pub s3_secret_access_key: Option<String>,

    /// The master key of the secret store: 32 bytes in base64. It encrypts each tenant's data
    /// key. The embedded server keeps its own in `.ptc/master.key`.
    #[arg(long, env = "PTC_MASTER_KEY", hide_env_values = true)]
    pub master_key: Option<String>,

    /// An API key (`ptc_sk_…`) with the role `admin`, registered at start: the first key, from
    /// which the others are made with `POST /v1/api-keys`.
    #[arg(long, env = "PTC_ADMIN_KEY", hide_env_values = true)]
    pub admin_key: Option<String>,

    /// The tenant of `--admin-key`.
    #[arg(long, env = "PTC_ADMIN_TENANT", default_value = "default")]
    pub admin_tenant: String,

    /// The URL clients reach this server at: `webhook(name)` returns `{public-url}/hooks/…`.
    /// Without it, scripts cannot get a webhook URL.
    #[arg(long, env = "PTC_PUBLIC_URL")]
    pub public_url: Option<String>,

    /// This worker's name: the owner of its leases and claims. The host name and the process
    /// id by default.
    #[arg(long, env = "PTC_WORKER_NAME")]
    pub worker_name: Option<String>,

    /// Serve only these tenants (a worker pool per tenant). Repeat for several.
    #[arg(long = "tenant", conflicts_with = "except_tenants")]
    pub tenants: Vec<String>,

    /// Serve every tenant but these.
    #[arg(long = "except-tenant")]
    pub except_tenants: Vec<String>,

    /// Activations run at once.
    #[arg(long, env = "PTC_ACTIVATIONS")]
    pub activations: Option<usize>,

    /// Tool calls dispatched at once.
    #[arg(long, env = "PTC_DISPATCHES", default_value_t = 64)]
    pub dispatches: usize,

    /// The most active runs a tenant created with its first key may have.
    #[arg(long, env = "PTC_RUN_QUOTA")]
    pub run_quota: Option<u32>,

    /// Accept toolsets with stdio MCP servers: processes this server starts. The embedded
    /// server always does.
    #[arg(long, env = "PTC_ALLOW_STDIO")]
    pub allow_stdio: bool,

    /// An internal host or network whose MCP servers toolsets may name: a host name
    /// (`mcp.internal.example`), its subdomains (`*.svc.cluster.local`), an address or a CIDR
    /// network (`10.20.0.0/16`). Repeat, or separate with commas. Without it, a server reaches
    /// MCP servers only at public addresses: private, loopback, link-local and cloud-metadata
    /// addresses are refused. The embedded server reaches any address.
    #[arg(
        long = "allow-internal-mcp",
        env = "PTC_ALLOW_INTERNAL_MCP",
        value_delimiter = ','
    )]
    pub allow_internal_mcp: Vec<String>,

    /// Send lifecycle webhooks to private, loopback and link-local addresses too, over
    /// `http://` or `https://`: for a harness on the same trusted network. Without it, a server
    /// sends them only to `https://` URLs with public addresses. The embedded server always
    /// does.
    #[arg(long, env = "PTC_ALLOW_PRIVATE_WEBHOOKS")]
    pub allow_private_webhooks: bool,

    /// Where tsgo, the type-checker, is extracted. The user's cache directory by default.
    #[arg(long, env = "PTC_TSGO_CACHE")]
    pub tsgo_cache: Option<PathBuf>,

    /// What is logged on standard error, as `RUST_LOG` writes it.
    #[arg(long, env = "PTC_LOG", default_value = "info")]
    pub log: String,

    /// Sends service and run telemetry over OTLP to this collector, such as
    /// `http://localhost:4318`. The `OTEL_EXPORTER_OTLP_*` variables work too.
    #[arg(long, env = "PTC_OTLP_ENDPOINT")]
    pub otlp_endpoint: Option<String>,

    /// The OTLP transport: `grpc`, or `http` (protobuf over HTTP, the default).
    #[arg(long, value_enum, env = "PTC_OTLP_PROTOCOL")]
    pub otlp_protocol: Option<OtlpArg>,

    /// The TTL of a run started without one: 30 days by default.
    #[arg(long, env = "PTC_DEFAULT_TTL", value_parser = parse_duration)]
    pub default_ttl: Option<Duration>,

    /// The longest TTL a run may ask for: 365 days by default. After an upgrade that changes the
    /// determinism epoch, the worker pool of the previous epoch stays up at most this long.
    #[arg(long, env = "PTC_MAX_TTL", value_parser = parse_duration)]
    pub max_ttl: Option<Duration>,

    /// The script instances a worker keeps in memory after their activation (sticky
    /// execution): 500 by default. 0 turns the cache off, and every activation replays.
    #[arg(long, env = "PTC_CACHE_INSTANCES")]
    pub cache_instances: Option<usize>,

    /// The memory the cached instances may hold together, as V8 measures their heaps: `4GiB`
    /// by default, or `512MiB`, or bytes. Keep it well below the worker's memory limit.
    #[arg(long, env = "PTC_CACHE_MEMORY", value_parser = parse_bytes)]
    pub cache_memory: Option<u64>,

    /// For the tenants this server creates: how long a finished run's journal stays in the
    /// store before it is archived. 30 days by default.
    #[arg(long, env = "PTC_JOURNAL_RETENTION", value_parser = parse_duration)]
    pub journal_retention: Option<Duration>,

    /// For the tenants this server creates: how long an archived journal is kept before the
    /// run is deleted. 365 days by default.
    #[arg(long, env = "PTC_ARCHIVE_RETENTION", value_parser = parse_duration)]
    pub archive_retention: Option<Duration>,

    /// For the tenants this server creates: how long the read model keeps a finished run.
    /// 90 days by default.
    #[arg(long, env = "PTC_READ_MODEL_RETENTION", value_parser = parse_duration)]
    pub read_model_retention: Option<Duration>,

    /// For the tenants this server creates: how long a payload no run references any more is
    /// kept. 24 hours by default.
    #[arg(long, env = "PTC_PAYLOAD_GRACE", value_parser = parse_duration)]
    pub payload_grace: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OtlpArg {
    Grpc,
    Http,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RoleArg {
    Api,
    Worker,
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    text.parse::<ptc_api::common::Duration>()
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

/// How the server listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Listen {
    /// The embedded server: the unix socket of its directory.
    Embedded {
        dir: PathBuf,
    },
    Tcp(SocketAddr),
}

/// What the command line resolves to.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) listen: Listen,
    pub(crate) api: bool,
    pub(crate) worker: bool,
    pub(crate) store: StoreLocation,
    pub(crate) blobs: BlobLocation,
    /// `None` for the embedded server, which keeps its own.
    pub(crate) master_key: Option<[u8; 32]>,
    pub(crate) admin_key: Option<String>,
    pub(crate) admin_tenant: TenantId,
    pub(crate) public_url: Option<String>,
    pub(crate) worker_name: String,
    pub(crate) tenants: TenantScope,
    pub(crate) activations: usize,
    pub(crate) dispatches: usize,
    pub(crate) run_quota: Option<u32>,
    pub(crate) allow_stdio: bool,
    /// Where MCP servers may be: anywhere for the embedded server; public addresses and the
    /// internal hosts of `--allow-internal-mcp` for a deployed one.
    pub(crate) mcp_egress: McpEgress,
    /// Lifecycle webhooks may go to the private network, over `http://` too.
    pub(crate) allow_private_webhooks: bool,
    /// `Some(true)`: switch to virtual time; `Some(false)`: to real time; `None`: as the store
    /// says.
    pub(crate) virtual_time: Option<bool>,
    pub(crate) idle_timeout: Option<Duration>,
    pub(crate) tsgo_cache: Option<PathBuf>,
    /// Run telemetry over OTLP, when configured.
    pub(crate) otlp: Option<ptc_otel::OtlpConfig>,
    pub(crate) default_ttl: Duration,
    pub(crate) max_ttl: Duration,
    /// The bounds of the workers' sticky instance cache.
    pub(crate) cache: CacheConfig,
    /// The retention of the tenants this server creates.
    pub(crate) retention: Retention,
}

impl Config {
    pub(crate) fn embedded(&self) -> bool {
        matches!(self.listen, Listen::Embedded { .. })
    }
}

impl ServerArgs {
    pub(crate) fn resolve(self) -> Result<Config, ConfigError> {
        let tenant = |id: &String| {
            TenantId::new(id.clone()).map_err(|error| ConfigError::new(error.to_string()))
        };
        let tenants = if !self.tenants.is_empty() {
            TenantScope::Only(
                self.tenants
                    .iter()
                    .map(tenant)
                    .collect::<Result<BTreeSet<_>, _>>()?,
            )
        } else if !self.except_tenants.is_empty() {
            TenantScope::Except(
                self.except_tenants
                    .iter()
                    .map(tenant)
                    .collect::<Result<BTreeSet<_>, _>>()?,
            )
        } else {
            TenantScope::All
        };
        let (api, worker) = if self.roles.is_empty() {
            (true, true)
        } else {
            (
                self.roles.contains(&RoleArg::Api),
                self.roles.contains(&RoleArg::Worker),
            )
        };
        let s3 = S3Options {
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
        let worker_name = self.worker_name.clone().unwrap_or_else(default_worker_name);
        let otlp = crate::wiring::telemetry::otlp(&self);
        let activations = self
            .activations
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, usize::from));
        if activations == 0 || self.dispatches == 0 {
            return Err(ConfigError::new(
                "--activations and --dispatches must be at least 1",
            ));
        }
        let engine = EngineConfig::default();
        let default_ttl = self.default_ttl.unwrap_or(engine.default_ttl);
        let max_ttl = self.max_ttl.unwrap_or(engine.max_ttl);
        if default_ttl.is_zero() || default_ttl > max_ttl {
            return Err(ConfigError::new(format!(
                "--default-ttl ({}) must be above zero and at most --max-ttl ({})",
                ptc_api::common::Duration::from(default_ttl),
                ptc_api::common::Duration::from(max_ttl),
            )));
        }
        let cache = CacheConfig {
            max_instances: self.cache_instances.unwrap_or(engine.cache.max_instances),
            max_bytes: self.cache_memory.unwrap_or(engine.cache.max_bytes),
        };
        let defaults = Retention::default();
        let retention = Retention {
            journal: self.journal_retention.unwrap_or(defaults.journal),
            archive: self.archive_retention.unwrap_or(defaults.archive),
            read_model: self.read_model_retention.unwrap_or(defaults.read_model),
            payload_grace: self.payload_grace.unwrap_or(defaults.payload_grace),
        };
        if let Some(url) = &self.public_url
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            return Err(ConfigError::new(format!(
                "--public-url {url:?}: expected an http:// or https:// URL"
            )));
        }

        if self.embedded {
            if !self.roles.is_empty() {
                return Err(ConfigError::new(
                    "the embedded server takes both roles: --role does not apply to it",
                ));
            }
            if self.store.is_some() || self.blobs.is_some() {
                return Err(ConfigError::new(
                    "the embedded server keeps everything in --dir: --store and --blobs do not \
                     apply to it",
                ));
            }
            if !self.allow_internal_mcp.is_empty() {
                return Err(ConfigError::new(
                    "the embedded server reaches MCP servers at any address: \
                     --allow-internal-mcp does not apply to it",
                ));
            }
            let dir = self.dir.clone();
            return Ok(Config {
                store: StoreLocation::Turso(dir.join("journal.db")),
                blobs: BlobLocation::Files(dir.join("blobs")),
                listen: Listen::Embedded { dir },
                api: true,
                worker: true,
                master_key: None,
                admin_key: None,
                admin_tenant: TenantId::local(),
                public_url: self
                    .public_url
                    .map(|url| url.trim_end_matches('/').to_owned()),
                worker_name,
                tenants,
                activations,
                dispatches: self.dispatches,
                run_quota: None,
                allow_stdio: true,
                mcp_egress: McpEgress::open(),
                allow_private_webhooks: true,
                virtual_time,
                idle_timeout: self.idle_timeout,
                tsgo_cache: self.tsgo_cache,
                otlp,
                default_ttl,
                max_ttl,
                cache,
                retention,
            });
        }

        if !api && !worker {
            return Err(ConfigError::new("--role takes api, worker, or both"));
        }
        let internal_mcp = InternalAllowList::parse(&self.allow_internal_mcp)
            .map_err(|error| ConfigError::new(format!("--allow-internal-mcp: {error}")))?;
        let store_text = self.store.clone().ok_or_else(|| {
            ConfigError::new(
                "--store is required (turso:<path>), or run the embedded server with --embedded",
            )
        })?;
        let store = StoreLocation::parse(&store_text)?;
        let blobs = match &self.blobs {
            Some(blobs) => BlobLocation::parse(blobs, s3)?,
            None => match &store {
                StoreLocation::Turso(path) => BlobLocation::Files(
                    path.parent()
                        .map_or_else(|| PathBuf::from("blobs"), |dir| dir.join("blobs")),
                ),
                StoreLocation::Postgres(_) => {
                    return Err(ConfigError::new(
                        "--blobs is required with Postgres: s3://<bucket>, or a directory every \
                         worker shares",
                    ));
                }
            },
        };
        if matches!(store, StoreLocation::Postgres(_)) && virtual_time == Some(true) {
            return Err(ConfigError::new(
                "--virtual-time is for a Turso store: a shared Postgres database runs in real \
                 time",
            ));
        }
        let master_key = self.master_key.as_deref().ok_or_else(|| {
            ConfigError::new(
                "--master-key (PTC_MASTER_KEY) is required: 32 random bytes in base64, which \
                 encrypt the secrets",
            )
        })?;
        let master_key = parse_master_key(master_key)?;
        let admin_tenant = TenantId::new(self.admin_tenant.clone())
            .map_err(|error| ConfigError::new(error.to_string()))?;
        if let Some(key) = &self.admin_key
            && (!key.starts_with(ptc_api::headers::API_KEY_PREFIX) || key.len() < 32)
        {
            return Err(ConfigError::new(
                "--admin-key must be a key ptc_sk_… of at least 32 characters",
            ));
        }
        Ok(Config {
            listen: Listen::Tcp(self.listen),
            api,
            worker,
            store,
            blobs,
            master_key: Some(master_key),
            admin_key: self.admin_key,
            admin_tenant,
            public_url: self
                .public_url
                .map(|url| url.trim_end_matches('/').to_owned()),
            worker_name,
            tenants,
            activations,
            dispatches: self.dispatches,
            run_quota: self.run_quota,
            allow_stdio: self.allow_stdio,
            mcp_egress: McpEgress::checked(internal_mcp),
            allow_private_webhooks: self.allow_private_webhooks,
            virtual_time,
            idle_timeout: self.idle_timeout,
            tsgo_cache: self.tsgo_cache,
            otlp,
            default_ttl,
            max_ttl,
            cache,
            retention,
        })
    }
}

/// Reads a master key: 32 bytes in base64.
pub(crate) fn parse_master_key(text: &str) -> Result<[u8; 32], ConfigError> {
    let bytes = BASE64
        .decode(text.trim())
        .map_err(|error| ConfigError::new(format!("the master key is not base64: {error}")))?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
        ConfigError::new(format!(
            "the master key has {} bytes: it must have 32",
            bytes.len()
        ))
    })
}

pub(crate) fn default_worker_name() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "worker".into());
    format!("{host}-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, ConfigError> {
        let args =
            ServerArgs::try_parse_from(std::iter::once("ptc server").chain(args.iter().copied()))
                .map_err(|error| ConfigError::new(error.to_string()))?;
        args.resolve()
    }

    #[test]
    fn the_embedded_server_keeps_everything_in_its_directory() {
        let config = parse(&[
            "--embedded",
            "--dir",
            "/tmp/x/.ptc",
            "--idle-timeout",
            "10m",
        ])
        .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            config.store,
            StoreLocation::Turso(PathBuf::from("/tmp/x/.ptc/journal.db"))
        );
        assert!(config.embedded() && config.api && config.worker);
        assert_eq!(config.idle_timeout, Some(Duration::from_secs(600)));
        assert!(parse(&["--embedded", "--role", "api"]).is_err());
        assert!(parse(&["--embedded", "--store", "turso:x.db"]).is_err());
    }

    #[test]
    fn a_deployed_server_needs_a_store_and_a_master_key() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--store", "turso:/tmp/j.db"]).is_err());
        let key = BASE64.encode([7u8; 32]);
        let config = parse(&[
            "--store",
            "turso:/tmp/j.db",
            "--master-key",
            &key,
            "--role",
            "api",
        ])
        .unwrap_or_else(|error| panic!("{error}"));
        assert!(config.api && !config.worker);
        assert_eq!(config.master_key, Some([7u8; 32]));
        assert!(
            parse(&["--store", "postgres://x", "--master-key", &key]).is_err(),
            "Postgres needs --blobs"
        );
        let postgres = parse(&[
            "--store",
            "postgres://u:p@localhost/ptc",
            "--blobs",
            "s3://payloads",
            "--master-key",
            &key,
        ])
        .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(postgres.store, StoreLocation::Postgres(_)));
        assert!(parse(&["--store", "mysql://x", "--master-key", &key]).is_err());
        assert!(parse(&["--store", "turso:/tmp/j.db", "--master-key", "short"]).is_err());
    }

    #[test]
    fn ttls_cache_and_retention_default_to_the_engines_and_can_be_set() {
        let key = BASE64.encode([7u8; 32]);
        let base = ["--store", "turso:/tmp/j.db", "--master-key", key.as_str()];
        let config = parse(&base).unwrap_or_else(|error| panic!("{error}"));
        let engine = EngineConfig::default();
        assert_eq!(config.default_ttl, engine.default_ttl);
        assert_eq!(config.max_ttl, engine.max_ttl);
        assert_eq!(config.cache, engine.cache);
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
            "--journal-retention",
            "7d",
            "--archive-retention",
            "30d",
            "--read-model-retention",
            "14d",
            "--payload-grace",
            "1h",
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
        assert_eq!(
            config.retention,
            Retention {
                journal: Duration::from_secs(7 * DAY),
                archive: Duration::from_secs(30 * DAY),
                read_model: Duration::from_secs(14 * DAY),
                payload_grace: Duration::from_secs(3_600),
            }
        );

        let mut longer = base.to_vec();
        longer.extend(["--default-ttl", "8d", "--max-ttl", "7d"]);
        assert!(parse(&longer).is_err(), "the default TTL above the maximum");
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
    fn only_a_deployed_server_checks_where_its_mcp_servers_are() {
        let key = BASE64.encode([7u8; 32]);
        let deployed = |extra: &[&str]| {
            let mut args = vec!["--store", "turso:/tmp/j.db", "--master-key", &key];
            args.extend_from_slice(extra);
            parse(&args)
        };
        let checked = deployed(&[]).unwrap_or_else(|error| panic!("{error}"));
        assert!(!checked.mcp_egress.is_open());
        let allowed = deployed(&[
            "--allow-internal-mcp",
            "mcp.internal.example,10.20.0.0/16",
            "--allow-internal-mcp",
            "*.svc.cluster.local",
        ])
        .unwrap_or_else(|error| panic!("{error}"));
        assert!(!allowed.mcp_egress.is_open());
        let invalid = deployed(&["--allow-internal-mcp", "10.0.0.0/33"]).unwrap_err();
        assert!(
            invalid
                .to_string()
                .starts_with("--allow-internal-mcp: \"10.0.0.0/33\""),
            "{invalid}"
        );

        let embedded = parse(&["--embedded"]).unwrap_or_else(|error| panic!("{error}"));
        assert!(embedded.mcp_egress.is_open());
        assert!(parse(&["--embedded", "--allow-internal-mcp", "10.0.0.0/8"]).is_err());
    }
}
