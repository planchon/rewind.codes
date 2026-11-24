//! The adapters a node of the run plane is wired with, and its configuration
//! (`archi/07-code-architecture.md`, "Inside a node").
//!
//! [`Plane`] names every port the actors of a node use; [`Wiring`] is the plain struct that
//! implements it, which composition roots and tests fill with their adapters. The actors hold
//! the plane behind an [`Arc`](std::sync::Arc): the work they ask for (a commit, a read, an
//! activation, a call) runs in tasks of its own that keep it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::NonZeroU16;
use std::time::Duration;

use rewind_core::Epoch;

use crate::cache::CacheConfig;
use crate::payloads::PayloadTiers;
use crate::ports::{
    ActionDispatcher, BlobStore, CatalogStore, Clock, CodeExecutor, Entropy, NodeId,
    PartitionStore, RunArchive, SecretStore, SpanExporter, TenantId, WebhookSender,
};
use crate::tasks::Backoff;
use crate::telemetry::TelemetryConfig;

/// The ports of a node.
pub trait Plane: Send + Sync + 'static {
    type Catalog: CatalogStore + 'static;
    type Partitions: PartitionStore<Log: 'static> + 'static;
    type Archive: RunArchive + 'static;
    type Blobs: BlobStore + 'static;
    type Executor: CodeExecutor<Instance: 'static> + 'static;
    type Dispatcher: ActionDispatcher + 'static;
    type Secrets: SecretStore + 'static;
    type Spans: SpanExporter + 'static;
    type Webhooks: WebhookSender + 'static;
    type Clock: Clock + 'static;
    type Entropy: Entropy + 'static;

    fn catalog(&self) -> &Self::Catalog;
    fn partitions(&self) -> &Self::Partitions;
    fn archive(&self) -> &Self::Archive;
    fn blobs(&self) -> &Self::Blobs;
    fn executor(&self) -> &Self::Executor;
    fn dispatcher(&self) -> &Self::Dispatcher;
    fn secrets(&self) -> &Self::Secrets;
    fn spans(&self) -> &Self::Spans;
    fn webhooks(&self) -> &Self::Webhooks;
    fn clock(&self) -> &Self::Clock;
    fn entropy(&self) -> &Self::Entropy;
}

/// The log of a partition of a plane.
pub type LogOf<P> = <<P as Plane>::Partitions as PartitionStore>::Log;

/// The instance an executor of a plane runs.
pub type InstanceOf<P> = <<P as Plane>::Executor as CodeExecutor>::Instance;

/// A [`Plane`] made of its adapters.
pub struct Wiring<C, P, A, B, X, D, S, T, W, K, E> {
    pub catalog: C,
    pub partitions: P,
    pub archive: A,
    pub blobs: B,
    pub executor: X,
    pub dispatcher: D,
    pub secrets: S,
    pub spans: T,
    pub webhooks: W,
    pub clock: K,
    pub entropy: E,
}

impl<C, P, A, B, X, D, S, T, W, K, E> fmt::Debug for Wiring<C, P, A, B, X, D, S, T, W, K, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wiring").finish_non_exhaustive()
    }
}

impl<C, P, A, B, X, D, S, T, W, K, E> Plane for Wiring<C, P, A, B, X, D, S, T, W, K, E>
where
    C: CatalogStore + 'static,
    P: PartitionStore<Log: 'static> + 'static,
    A: RunArchive + 'static,
    B: BlobStore + 'static,
    X: CodeExecutor<Instance: 'static> + 'static,
    D: ActionDispatcher + 'static,
    S: SecretStore + 'static,
    T: SpanExporter + 'static,
    W: WebhookSender + 'static,
    K: Clock + 'static,
    E: Entropy + 'static,
{
    type Catalog = C;
    type Partitions = P;
    type Archive = A;
    type Blobs = B;
    type Executor = X;
    type Dispatcher = D;
    type Secrets = S;
    type Spans = T;
    type Webhooks = W;
    type Clock = K;
    type Entropy = E;

    fn catalog(&self) -> &C {
        &self.catalog
    }
    fn partitions(&self) -> &P {
        &self.partitions
    }
    fn archive(&self) -> &A {
        &self.archive
    }
    fn blobs(&self) -> &B {
        &self.blobs
    }
    fn executor(&self) -> &X {
        &self.executor
    }
    fn dispatcher(&self) -> &D {
        &self.dispatcher
    }
    fn secrets(&self) -> &S {
        &self.secrets
    }
    fn spans(&self) -> &T {
        &self.spans
    }
    fn webhooks(&self) -> &W {
        &self.webhooks
    }
    fn clock(&self) -> &K {
        &self.clock
    }
    fn entropy(&self) -> &E {
        &self.entropy
    }
}

/// Where the tasks of harness actions go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HarnessActions {
    /// To the SDK streams of the harness, through the organization actor: a server.
    #[default]
    Streams,
    /// To the dispatcher, as every other call: test runs and the engine's tests, where the
    /// handlers answer every action.
    Dispatched,
}

/// Resource limits an activation enforces (`archi/03-execution.md`, "Resource limits"). CPU
/// time and heap size are enforced by the executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Events in a run's history.
    pub max_history_len: u64,
    /// Commands the code may emit in one activation.
    pub max_commands_per_activation: usize,
    /// Size of one payload the code emits (action args, output), as canonical JSON.
    pub max_payload_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_history_len: 50_000,
            max_commands_per_activation: 10_000,
            max_payload_bytes: 8 * 1024 * 1024,
        }
    }
}

/// How a node runs (`archi/07-code-architecture.md`, "Inside a node").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    pub node: NodeId,
    /// The partitions of the deployment: 64 in production, 1 embedded.
    pub partitions: NonZeroU16,
    /// The build it runs. A node of a new build tries each quarantined run of its epochs once.
    pub build: String,
    /// Its pool of executors (`--pool`); `None`: the shared pool.
    pub pool: Option<String>,
    /// The tenants whose activations run on a pool of their own, by pool.
    pub pools: BTreeMap<TenantId, String>,
    /// It runs activations for other owners and owns no partition (`--executors-only`).
    pub executors_only: bool,
    /// Activations run at once: the executor threads.
    pub executors: usize,
    /// Calls dispatched at once.
    pub calls: usize,
    pub cache: CacheConfig,
    pub limits: Limits,
    pub payloads: PayloadTiers,
    pub backoff: Backoff,
    /// Deliveries of a call (`archi/04-tools.md`, "Policies").
    pub max_deliveries: u32,
    /// The server's public URL, for `signalUrl(name)` and the issuer of signed calls.
    pub public_url: Option<String>,
    pub telemetry: TelemetryConfig,
    /// TTL of a run started without one.
    pub default_ttl: Duration,
    /// The largest TTL a run may ask for.
    pub max_ttl: Duration,
    /// The timer wheel's horizon: the timers due before `now + horizon` are held in memory, and
    /// the next ones are read once per horizon. One hour; the driver opens partitions with a
    /// horizon past every timer.
    pub horizon: Duration,
    /// A failed activation is tried again after this.
    pub activation_retry: Duration,
    /// A webhook delivery is tried this many times, then given up.
    pub webhook_attempts: u32,
    pub harness_actions: HarnessActions,
    /// The calls an activation schedules are sent latest first: tests exercise completions that
    /// arrive out of order under `Promise.all`. Off by default.
    pub reversed_calls: bool,
    /// The determinism epochs its executors run; `None`: those its executor supports. A node
    /// that declares fewer sends the activations of the others to a node that runs them.
    pub epochs: Option<BTreeSet<Epoch>>,
}

impl NodeConfig {
    /// The defaults of a node of `partitions`.
    pub fn new(node: NodeId, partitions: NonZeroU16) -> Self {
        const DAY: u64 = 86_400;
        Self {
            node,
            partitions,
            build: concat!("rewind-engine ", env!("CARGO_PKG_VERSION")).into(),
            pool: None,
            pools: BTreeMap::new(),
            executors_only: false,
            executors: 8,
            calls: 256,
            cache: CacheConfig::default(),
            limits: Limits::default(),
            payloads: PayloadTiers::default(),
            backoff: Backoff::default(),
            max_deliveries: 5,
            public_url: None,
            telemetry: TelemetryConfig::default(),
            default_ttl: Duration::from_secs(30 * DAY),
            max_ttl: Duration::from_secs(365 * DAY),
            horizon: Duration::from_secs(3_600),
            activation_retry: Duration::from_secs(30),
            webhook_attempts: 8,
            harness_actions: HarnessActions::Streams,
            reversed_calls: false,
            epochs: None,
        }
    }
}
