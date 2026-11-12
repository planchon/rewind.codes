//! Leases, inboxes and what an activation commits (`archi/03-execution.md`, "Activations").

use std::time::Duration;

use ptc_core::{
    Completion, Digest, Epoch, HistoryEntry, JsonValue, Payload, RunStarted, RunStatus, Seq,
    Timestamp, ToolPolicy, ToolRoute,
};

use crate::ports::tenancy::{RunRef, TenantScope};

/// The right to write a run's history. Acquiring a lease increments the run's fence, and a
/// write only succeeds while the fence still matches: a worker that lost its lease can never
/// corrupt a history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    pub run: RunRef,
    pub fence: u64,
}

/// How long another worker waits before it takes a runnable run whose instance is cached by
/// its sticky worker (`archi/03-execution.md`, "Sticky execution").
pub const STICKY_GRACE: Duration = Duration::from_secs(5);

/// What a worker asks `acquire_runnable` for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireRunnable<'a> {
    /// The worker, as the owner of the lease and as a sticky worker.
    pub owner: &'a str,
    /// The determinism epochs the worker supports.
    pub epochs: &'a [Epoch],
    /// The tenants the worker serves.
    pub tenants: &'a TenantScope,
    pub now: Timestamp,
    pub lease_duration: Duration,
    /// How long a run stays reserved to its sticky worker once runnable: [`STICKY_GRACE`].
    pub sticky_grace: Duration,
    /// A random number, from which the store draws the tenant: the tenants with runnable work,
    /// ordered by id, are indexed by `draw` modulo their number. Passing it in keeps stores
    /// deterministic.
    pub draw: u64,
}

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

/// Everything an activation commits, in one transaction: the history entries (complete
/// "trigger → response" groups), the inbox items they consumed, the payloads that left the
/// events, and the effects derived from the new commands (outbox pattern).
#[derive(Debug, Clone, PartialEq)]
pub struct Activation {
    pub entries: Vec<HistoryEntry>,
    /// Ids of the inbox items consumed, delivered or dropped.
    pub consumed: Vec<u64>,
    /// The payloads of `entries` held only by their digest: their `payloads` rows, and the run's
    /// `payload_refs`.
    pub payloads: Vec<PayloadRecord>,
    pub tool_calls: Vec<NewToolTask>,
    pub timers: Vec<NewTimer>,
    pub cancelled_timers: Vec<Seq>,
    /// The run's status after the entries. A terminal status clears the run's timers, tool
    /// tasks, inbox and webhook tokens.
    pub status: RunStatus,
}

impl Activation {
    /// An activation that commits these entries and consumes these inbox items, with no
    /// payload and no effect.
    pub fn new(entries: Vec<HistoryEntry>, consumed: Vec<u64>, status: RunStatus) -> Self {
        Self {
            entries,
            consumed,
            payloads: Vec::new(),
            tool_calls: Vec::new(),
            timers: Vec::new(),
            cancelled_timers: Vec::new(),
            status,
        }
    }
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

/// A tool task created by a `schedule_tool_call`. Its route and policy are copied from the
/// pinned toolset: the store enforces the policy's concurrency at claim, and starts the task in
/// `approval` when the policy asks for it.
#[derive(Debug, Clone, PartialEq)]
pub struct NewToolTask {
    pub seq: Seq,
    pub tool: String,
    pub args: Payload,
    pub route: ToolRoute,
    pub policy: ToolPolicy,
    /// For an MCP tool, the concurrency of its server: calls in flight to it per worker.
    pub server_concurrency: Option<u32>,
}

/// How long a cleanup may take to delete the blobs of the payloads it started deleting
/// (`JournalStore::forget_payloads`): it deletes them within this time, or it stopped. Past it,
/// another cleanup starts their deletion again.
pub const PAYLOAD_DELETION_TIMEOUT: Duration = Duration::from_secs(3_600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewTimer {
    pub seq: Seq,
    pub fire_at: Timestamp,
}
