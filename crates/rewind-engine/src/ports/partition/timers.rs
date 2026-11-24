//! The timers of a partition (`archi/03-execution.md`, "Signals and timers in the glue";
//! `archi/06-storage-and-scale.md`, "Partitions, owners and nodes": the timer wheel).
//!
//! Every timer is a record sorted by its due time, written in the batch of the fact that set it.
//! The owner's timer wheel holds those due before its horizon, an hour ahead, and reads the next
//! hour's once an hour: the one scheduling loop of a partition.

use std::cmp::Ordering;

use rewind_core::{RunId, Seq, Timestamp};

use super::outbox::OutboxId;
use crate::ports::tenancy::{RunRef, TenantId};

/// What fires, and for whom. A timer is known by its kind: setting a timer of a kind already set
/// moves it to its new due time.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TimerKind {
    /// A code's `start_timer`: `timer_fired { seq }` when it fires.
    Code { run: RunRef, seq: Seq },
    /// A call's timeout: `call_failed { timeout }`.
    CallTimeout { run: RunRef, seq: Seq },
    /// The deadline of a deferred task: it ends with its timeout failure.
    DeferredDeadline { run: RunRef, seq: Seq },
    /// A task's next delivery after a backoff (`next_delivery_at`): it is sent again.
    RedeliveryBackoff { run: RunRef, seq: Seq },
    /// The run's deadline (`expires_at`): `cancel_requested { ttl }`, once.
    RunDeadline { run: RunRef },
    /// A failed activation is tried again (`archi/03-execution.md`, "Activations").
    ActivationRetry { run: RunRef },
    /// The end of an ended run's retention: the run is erased.
    RetentionEnd { run: RunRef },
    /// The next try of the webhook an outbox entry delivers.
    WebhookRetry { tenant: TenantId, entry: OutboxId },
}

impl TimerKind {
    /// The run the timer is about: every kind but a webhook's retry, which belongs to its
    /// outbox entry.
    pub fn run(&self) -> Option<&RunRef> {
        match self {
            TimerKind::Code { run, .. }
            | TimerKind::CallTimeout { run, .. }
            | TimerKind::DeferredDeadline { run, .. }
            | TimerKind::RedeliveryBackoff { run, .. }
            | TimerKind::RunDeadline { run }
            | TimerKind::ActivationRetry { run }
            | TimerKind::RetentionEnd { run } => Some(run),
            TimerKind::WebhookRetry { .. } => None,
        }
    }

    pub fn tenant(&self) -> &TenantId {
        match self {
            TimerKind::Code { run, .. }
            | TimerKind::CallTimeout { run, .. }
            | TimerKind::DeferredDeadline { run, .. }
            | TimerKind::RedeliveryBackoff { run, .. }
            | TimerKind::RunDeadline { run }
            | TimerKind::ActivationRetry { run }
            | TimerKind::RetentionEnd { run } => &run.tenant,
            TimerKind::WebhookRetry { tenant, .. } => tenant,
        }
    }

    /// The `seq` of the code's timer or of the call it is about.
    pub fn seq(&self) -> Option<Seq> {
        match self {
            TimerKind::Code { seq, .. }
            | TimerKind::CallTimeout { seq, .. }
            | TimerKind::DeferredDeadline { seq, .. }
            | TimerKind::RedeliveryBackoff { seq, .. } => Some(*seq),
            _ => None,
        }
    }

    /// Whether the timer belongs to the run's attempt: a terminal activation clears these. A
    /// run's retention outlives its end, and a webhook's retry belongs to its outbox entry.
    pub fn ends_with_the_attempt(&self) -> bool {
        !matches!(
            self,
            TimerKind::RetentionEnd { .. } | TimerKind::WebhookRetry { .. }
        )
    }

    fn rank(&self) -> u8 {
        match self {
            TimerKind::Code { .. } => 0,
            TimerKind::CallTimeout { .. } => 1,
            TimerKind::DeferredDeadline { .. } => 2,
            TimerKind::RedeliveryBackoff { .. } => 3,
            TimerKind::RunDeadline { .. } => 4,
            TimerKind::ActivationRetry { .. } => 5,
            TimerKind::RetentionEnd { .. } => 6,
            TimerKind::WebhookRetry { .. } => 7,
        }
    }

    /// The order of timers due at the same time: by tenant and run (a webhook's retry before
    /// the runs of its tenant), then by kind, then by `seq` or outbox entry.
    fn order_key(&self) -> (&TenantId, Option<RunId>, u8, u64) {
        let detail = match self {
            TimerKind::WebhookRetry { entry, .. } => entry.0,
            other => other.seq().map_or(0, |seq| u64::from(seq.0)),
        };
        (
            self.tenant(),
            self.run().map(|run| run.id),
            self.rank(),
            detail,
        )
    }
}

/// A timer and when it is due.
///
/// Timers are ordered by due time, then by run and kind ([`TimerKind`]'s order of timers due
/// together).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DueTimer {
    pub at: Timestamp,
    pub kind: TimerKind,
}

impl Ord for DueTimer {
    fn cmp(&self, other: &Self) -> Ordering {
        self.at
            .cmp(&other.at)
            .then_with(|| self.kind.order_key().cmp(&other.kind.order_key()))
            .then_with(|| self.kind.cmp(&other.kind))
    }
}

impl PartialOrd for DueTimer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A timer set or cancelled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimerChange {
    /// Sets the timer, or moves it to its new due time.
    Set(DueTimer),
    /// Cancels the timer of this kind; cancelling a timer not set changes nothing.
    Cancel(TimerKind),
}
