//! What wakes a run, and the payloads its events hold by digest (`archi/03-execution.md`,
//! "Activations"; `archi/06-storage-and-scale.md`, "Payloads").

use rewind_core::{Completion, Digest, JsonValue, RunStarted, Timestamp};

/// What wakes a run: an item of its inbox.
#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    RunStarted(RunStarted),
    Completion(Completion),
}

#[derive(Debug, Clone, PartialEq)]
pub struct InboxItem {
    /// Unique across the store, and increasing in arrival order.
    pub id: u64,
    pub trigger: Trigger,
    pub received_at: Timestamp,
}

/// A payload value kept outside the events, as the `payloads` table holds it
/// (`archi/06-storage-and-scale.md`, "Payloads"). Size and preview are store metadata: traces
/// and the dashboard render without loading the value.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadRecord {
    pub digest: Digest,
    /// Size of the canonical JSON, in bytes.
    pub size: u64,
    /// The start of the canonical JSON, for traces read by people and models.
    pub preview: String,
    /// The value, for payloads up to the table tier (64 KiB by default). `None`: the value is in
    /// the `BlobStore`.
    pub value: Option<JsonValue>,
}
