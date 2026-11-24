//! Application layer of Rewind (`archi/07-code-architecture.md`).
//!
//! The run plane (`archi/07-code-architecture.md`, "Inside a node"):
//!
//! - [`node`]: a node's actors (the owners of its partitions and the organization actors they
//!   host, its executors, its call executor, the node actor), as state machines that take a
//!   message and return effects; [`node::runtime`] drives them with tokio in a process.
//! - [`driver`]: the same actors of every node stepped on one thread in virtual time: the
//!   engine's tests, test runs and the runs of tests.
//! - [`edge`]: the use cases at the edge (start, fork, signal, cancel, tasks, reads, searches,
//!   statistics, traces, tests recorded from runs), through a [`edge::Gateway`] to the owner of
//!   the run's partition; workflow versions saved through their gate, in the control plane.
//! - [`router`]: where a run's partition and its owner are.
//! - [`plane`]: the adapters a node is wired with, and its configuration.
//! - [`ports`]: what the engine needs from the outside world, as traits. Adapters implement
//!   them; the engine never names Postgres, SlateDB, S3, V8, MCP or OpenTelemetry.
//!
//! What the actors share:
//!
//! - [`fork`]: following a parent's history with a corrected code, and whether a code is
//!   compatible with a history; [`replay`]: rebuilding a code from its history.
//! - [`projector`]: what committed events project (a run's summary, its lifecycle events);
//!   [`telemetry`]: the spans, logs and run metrics derived from them; [`failures`]: the
//!   signatures and fingerprints that group failed, cancelled and aborted runs.
//! - [`attachments`]: the secrets a run attaches, and who may attach them.
//! - [`quotas`]: the alerts of the quotas the organization actors keep.
//! - [`metrics`]: what the engine measures, through the `tracing` facade.
//! - [`test`]: test runs: the dispatcher that answers from a test's rules, handlers,
//!   cassette and static responses.
//! - [`suites`]: the suites of workflow versions, composed from their parent's and stored as
//!   blobs in the control plane; tests recorded from runs.
//! - [`testing`]: tests run in virtual time against a code, several at once, each on a run
//!   plane of its own in the driver; the checks of a test against its action set and its
//!   workflow's interface.
//! - [`transcript`]: the transcript of a run.
//! - [`migrate`]: `rewind server migrate-runs`, the runs of the set-aside tables moved into their
//!   partitions and the archive, once.
//! - [`adapters`]: the in-memory control plane (catalog, cluster), run plane (partitions),
//!   archive, link between nodes, blobs, access, secrets, telemetry and webhooks; a fake
//!   identity provider; static action responses and type-checking; system and virtual time;
//!   system and seeded entropy. With a virtual clock they make test runs and the test harness.
//! - `conformance` (feature): the suites every adapter of `CatalogStore`, `PartitionStore`,
//!   `RunArchive`, `Cluster`, `AccessStore`, `BlobStore`, `SecretStore`, `SpanExporter` and
//!   `WebhookSender` must pass.
//!
//! The engine is deterministic given its ports. Service telemetry (spans, logs and measurements)
//! goes through the `tracing` facade; what a code did is recorded in its history, never
//! emitted from here.

#![forbid(unsafe_code)]

pub mod adapters;
pub mod attachments;
mod batch;
mod cache;
mod checks;
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
pub mod driver;
pub mod edge;
mod executors;
pub mod failures;
pub mod fork;
pub mod metrics;
pub mod migrate;
pub mod node;
mod org;
mod partition;
mod payloads;
pub mod plane;
pub mod ports;
pub mod projector;
pub mod quotas;
pub mod replay;
pub mod router;
pub mod schema;
mod start;
pub mod suites;
mod tasks;
pub mod telemetry;
pub mod testing;
pub mod text;
pub mod transcript;
mod workflows;
pub mod world;

pub use batch::EngineError;
pub use cache::CacheConfig;
pub use fork::{Compatibility, ForkError, ForkPoint, ForkRun};
pub use payloads::PayloadTiers;
pub use plane::Limits;
pub use replay::ReplayVerdict;
pub use start::{StartError, StartRun};
pub use tasks::{Backoff, idempotency_key};
pub use text::NulCharacter;
pub use workflows::{RegisterError, RegisterWorkflow, SignalRefused, StartWorkflow};
