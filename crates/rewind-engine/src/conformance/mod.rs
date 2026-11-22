//! The conformance suites of the ports (feature `conformance`): `CatalogStore`,
//! `PartitionStore`, `RunArchive`, `Cluster`, `JournalStore`, `AccessStore`, `BlobStore`,
//! `ReadModel`, `SecretStore`, `SpanExporter` and `WebhookSender`.
//!
//! Every adapter of a port passes its suite: they check the contract of the port, not a table
//! layout. Each suite is deterministic: time is always passed in, never read, and the only
//! random values (tokens) are compared for equality only. An adapter's test calls `run_all`
//! with a function that creates an empty store:
//!
//! ```ignore
//! #[tokio::test]
//! async fn journal_conformance() {
//!     rewind_engine::conformance::journal::run_all(|| MyStore::new_empty()).await;
//! }
//! ```
//!
//! A span exporter has no read side: its suite also reads the exporter's backend, through a
//! [`spans::Collector`] the adapter's test provides. The suites of `CatalogStore` and
//! `RunArchive` take a function that creates the store with the port it reaches over the same
//! backend: the catalog with its `Cluster`, whose members receive its changes; the archive with
//! its `BlobStore`, from which it erases a run's blobs.
//!
//! `run_all` prints the name of each check before running it, so a failure names its check.
//! Each check is also public, to run one alone while debugging.

pub mod access;
pub mod archive;
pub mod blobs;
pub mod catalog;
pub mod cluster;
pub mod journal;
pub mod partition;
pub mod read_model;
pub mod secrets;
pub mod spans;
pub mod webhooks;

use rewind_core::Timestamp;

use crate::ports::TenantId;

/// 2026-01-01T00:00:00Z.
pub const T0: Timestamp = Timestamp(1_767_225_600_000);

/// A tenant of the suites.
pub fn tenant(name: &str) -> TenantId {
    TenantId::new(name).expect("the suites use valid tenant names")
}

/// Runs a check of a suite, naming it first.
macro_rules! check {
    ($suite:literal, $store:expr, $check:ident) => {{
        eprintln!(concat!($suite, " conformance: ", stringify!($check)));
        $check(&$store).await;
    }};
}
pub(crate) use check;
