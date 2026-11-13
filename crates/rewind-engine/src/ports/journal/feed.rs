//! The feed (`archi/05-testing-and-observability.md`, "The feed and the projector").
//!
//! `commit_activation` writes, in the same transaction, one feed item per consumer: the range
//! of entries the activation committed. The projector claims items the way the dispatcher
//! claims tool tasks, applies them, and acknowledges them. Delivery is at least once: every
//! projection is idempotent.

use std::fmt;

use crate::ports::tenancy::RunRef;

/// A projection fed from committed entries. One feed item per consumer, so that a slow
/// consumer never delays the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FeedConsumer {
    /// The `ReadModel` behind the dashboard and the queries across runs.
    ReadModel,
    /// Run telemetry, exported through the `SpanExporter`.
    Otel,
    /// Lifecycle webhooks (`run.completed`, `run.failed`, …), sent through the `WebhookSender`.
    Webhooks,
}

impl FeedConsumer {
    pub const ALL: [FeedConsumer; 3] = [
        FeedConsumer::ReadModel,
        FeedConsumer::Otel,
        FeedConsumer::Webhooks,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FeedConsumer::ReadModel => "read_model",
            FeedConsumer::Otel => "otel",
            FeedConsumer::Webhooks => "webhooks",
        }
    }
}

impl fmt::Display for FeedConsumer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The entries one activation committed, for one consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedItem {
    /// Unique across the store.
    pub id: u64,
    pub consumer: FeedConsumer,
    pub run: RunRef,
    /// The fence of the lease that committed: the activation span is `H(run, fence)`.
    pub fence: u64,
    /// The committed entries are `from_idx..to_idx` (end excluded).
    pub from_idx: u64,
    pub to_idx: u64,
}
