//! What a partition keeps of each run: its head, its watchers and its tool tasks
//! (`archi/06-storage-and-scale.md`, "A partition").

use std::fmt;

use rewind_core::{Digest, Epoch, RunHandle, RunId, RunStatus, Seq, Timestamp};

use crate::ports::journal::{Quarantine, RunMetadata, TaskState, ToolTask};
use crate::ports::namespaces::NamespaceName;
use crate::ports::read_model::Wait;
use crate::ports::tenancy::RunRef;

/// An attempt of a run: the run's history, its inbox, its tool tasks and its sealed journal
/// belong to one attempt. A run starts at [`Attempt::FIRST`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Attempt(pub u32);

impl Attempt {
    pub const FIRST: Attempt = Attempt(1);

    /// The attempt after this one.
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl Default for Attempt {
    fn default() -> Self {
        Self::FIRST
    }
}

impl fmt::Display for Attempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The head of a run: its service state and its metadata, the record every other record of the
/// run hangs from. `status`, `epoch`, `expires_at` and `history_len` are copies kept for the
/// owner and for queries; the history is authoritative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunHead {
    pub run: RunRef,
    /// The end user the run belongs to.
    pub namespace: NamespaceName,
    /// What ran, the middle of the run's handle: its workflow's name, or its script's title as
    /// a slug, or `script`; a fork's is its parent's (`archi/06-storage-and-scale.md`, "Run
    /// handles"). Frozen when the run is created.
    pub origin: String,
    /// The characters of the suffix of the run's handle: [`SUFFIX_LEN`](rewind_core::SUFFIX_LEN),
    /// more when another run of its namespace shares them. Frozen when the run is created.
    pub suffix_len: u8,
    pub script: Digest,
    pub action_set: Digest,
    pub status: RunStatus,
    /// The determinism epoch of `run_started`: which executors may run it.
    pub epoch: Epoch,
    /// The attempt the run is at.
    pub attempt: Attempt,
    /// The last attempt sealed into the archive. The store keeps it: `Seal` sets it.
    pub sealed: Option<Attempt>,
    /// Activations that failed in a row ([`QUARANTINE_AFTER_FAILURES`] quarantine the run).
    ///
    /// [`QUARANTINE_AFTER_FAILURES`]: crate::ports::QUARANTINE_AFTER_FAILURES
    pub failures: u32,
    /// Activations an owner started without seeing their end: one more for each runnable run
    /// at each recovery of the partition.
    pub attempts: u32,
    pub quarantine: Option<Quarantine>,
    /// What the run waits for, while it is active.
    pub waits: Vec<Wait>,
    /// The deadline of `run_started`.
    pub expires_at: Timestamp,
    /// The length of the attempt's history. The store keeps it: it is the number of entries
    /// committed to the attempt.
    pub history_len: u64,
    pub metadata: RunMetadata,
    /// The start's idempotency key, unique in the namespace.
    pub idempotency_key: Option<String>,
    pub created_at: Timestamp,
    /// `recorded_at` of the last entry, or `created_at` before the first commit.
    pub updated_at: Timestamp,
    /// `recorded_at` of the entry that ended the attempt.
    pub ended_at: Option<Timestamp>,
}

impl RunHead {
    /// The run's handle, `<namespace>/<origin>/<suffix>`.
    pub fn handle(&self) -> RunHandle {
        RunHandle::new(
            self.namespace.as_str(),
            &self.origin,
            self.run.id,
            usize::from(self.suffix_len),
        )
    }

    /// The attempt has ended.
    pub fn is_ended(&self) -> bool {
        self.status.is_terminal()
    }

    /// The attempt has ended and is not sealed yet.
    pub fn is_unsealed(&self) -> bool {
        self.is_ended() && self.sealed != Some(self.attempt)
    }
}

/// A run that waits for another's end (`archi/03-execution.md`, "Workflows across runs"): the
/// run that started it, or a fork whose copied history started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Watcher {
    /// The watching run, of the same tenant and namespace.
    pub run: RunId,
    /// The `seq` of the call that started the watched run.
    pub seq: Seq,
    /// The watched run is cancelled when the watcher ends. A fork's watch is never attached.
    pub attached: bool,
}

/// A tool task as a partition keeps it (`archi/04-tools.md`, "Tool task lifecycle"): the task,
/// and the token handed out with it while it is deferred.
///
/// Sending a call or handing a task out writes nothing: a task in flight reads `ready`. What
/// is written is what happened to it: its creation, a backoff, an interruption (attempt + 1),
/// a deferral, a park, its end.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskRecord {
    pub task: ToolTask,
    /// The SHA-256 of the token its handler answers a deferred task with.
    pub token_hash: Option<Digest>,
}

impl TaskRecord {
    /// What `recover` hands back: a task `ready`, `deferred` or waiting for its approval.
    pub fn is_pending(&self) -> bool {
        matches!(
            self.task.state,
            TaskState::Ready { .. } | TaskState::Deferred { .. } | TaskState::Approval
        )
    }
}

/// A change to one tool task of a run.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskChange {
    /// Writes the task as it is now: its state, its attempt, its token. Its
    /// [`TaskRef`](crate::ports::TaskRef) is the change's run and seq.
    Put(Box<TaskRecord>),
    /// The task is done: its outcome is in the run's inbox.
    Delete,
}
