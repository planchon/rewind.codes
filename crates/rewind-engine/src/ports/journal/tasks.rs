//! Tool tasks (`archi/04-tools.md`, "Tool task lifecycle").
//!
//! ```text
//!               approve            sent or handed out          answer, failure
//!   approval ───────────► ready ─────────────────────► claimed ─────────────────────► done
//!      │ reject ▲  ▲  ▲     ▲   next_attempt_at,          │ │ │ interrupted: the node   ▲
//!      ▼        │  │  │     │   a concurrency slot        │ │ │ died, the stream did    │
//!    done       │  │  │     └──── retry (backoff) ────────┘ │ │ not come back:          │
//!               │  │  │           attempt + 1               │ │ attempt + 1             │
//!               │  │  └── approve, the secret ────┐         │ │                         │
//!               │  │      put again               │         │ └─► deferred ── complete, ┤
//!               │  │                           parked ◄─────┘     (a token)   fail,     │
//!               │  │                              │ reject                    timeout   │
//!               │  │                              └───────────────────────────────────┤
//! ```
//!
//! `done` is not a state the partition keeps: a task that is done is deleted, and its outcome
//! is in the run's inbox. Attempts, backoff, approvals and tokens live in the task, never in
//! the journal. Sending a call or handing out a task writes nothing: the records say `ready`
//! while the run's owner holds the task in flight, and a read shows it `claimed`.

use std::fmt;

use rewind_core::{
    Digest, Payload, Seq, Timestamp, ToolFailure, ToolFailureKind, ToolPolicy, ToolRoute,
};

use crate::ports::tenancy::RunRef;

/// A tool task: the call `seq` of a run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskRef {
    pub run: RunRef,
    pub seq: Seq,
}

impl fmt::Display for TaskRef {
    /// `{run_id}/{seq}`, as the API names a task.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.run.id, self.seq.0)
    }
}

/// An unguessable token for one attempt of a task, drawn from a CSPRNG (at least 128 bits)
/// when the call is sent or the task handed out; it names the run, so an answer reaches the
/// run's owner from whichever process receives it. The handler completes, fails or defers the
/// task with it. Once the task is in flight again, the previous token is no longer accepted; a
/// deferred task keeps its token until it is answered.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskToken(pub String);

impl fmt::Debug for TaskToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TaskToken(<redacted>)")
    }
}

/// Where a task is in its lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    /// Waits for the harness's approval (the `approval` policy).
    Approval,
    /// Sent or handed out once `next_attempt_at` has passed and a concurrency slot is free.
    Ready { next_attempt_at: Timestamp },
    /// Attempt n is in flight: sent by the node of the run's owner, `owner`, or handed to an SDK
    /// worker's stream. `until`: when the attempt was made ready. Only a read shows it: the
    /// records say `ready`.
    Claimed { owner: String, until: Timestamp },
    /// The handler answers later, with the token of its attempt, before `until`.
    Deferred { until: Timestamp },
    /// Waits for the harness after a schema change, a refused call or a missing secret.
    Parked(ParkReason),
}

impl TaskState {
    pub fn name(&self) -> &'static str {
        match self {
            TaskState::Approval => "approval",
            TaskState::Ready { .. } => "ready",
            TaskState::Claimed { .. } => "claimed",
            TaskState::Deferred { .. } => "deferred",
            TaskState::Parked(_) => "parked",
        }
    }
}

/// Why a task is parked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkReason {
    pub kind: ParkKind,
    /// Written for the harness developer and the model: what changed, or what was refused.
    pub message: String,
}

/// Stored as `schema_changed`, `credentials_refused` and `secret_missing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParkKind {
    /// The tool's current schema no longer matches the call (`tool.schema_changed`). Approved,
    /// the call is sent against the new schema: the task's `schema_change_approved` is set.
    SchemaChanged,
    /// The server answered 401 to the attached secret or to the signature
    /// (`credentials.expired`, reason `refused`). The secret put again, or the harness's
    /// approval, makes the task ready again.
    CredentialsRefused,
    /// The secret the run attached for the server no longer exists: parked by the engine before
    /// anything was sent (`credentials.expired`, reason `missing`). The secret put again makes
    /// the task ready again.
    SecretMissing,
}

impl ParkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ParkKind::SchemaChanged => "schema_changed",
            ParkKind::CredentialsRefused => "credentials_refused",
            ParkKind::SecretMissing => "secret_missing",
        }
    }

    /// Whether putting the secret the call reads again makes the task ready: a refusal or a
    /// missing secret (`archi/04-tools.md`, "Secrets").
    pub fn resumed_by_secret(self) -> bool {
        matches!(self, ParkKind::CredentialsRefused | ParkKind::SecretMissing)
    }
}

/// A task as the store keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolTask {
    pub task: TaskRef,
    /// The toolset of the task's run.
    pub toolset: Digest,
    pub tool: String,
    pub route: ToolRoute,
    pub args: Payload,
    pub policy: ToolPolicy,
    pub state: TaskState,
    /// The attempt the task is at, from 1: one more after each transient failure or
    /// interruption retried.
    pub attempt: u32,
    /// The harness approved the task while it was parked by
    /// [`SchemaChanged`](ParkKind::SchemaChanged): the call goes on against the tool's current
    /// schema, and the dispatcher no longer checks it. It stays set for the task's life.
    pub schema_change_approved: bool,
    pub created_at: Timestamp,
}

/// The failure a deferred task ends with when no answer came before its deadline. Every store
/// writes this one, so that scripts see the same failure whatever the backend.
pub fn deferred_timeout_failure() -> ToolFailure {
    ToolFailure {
        kind: ToolFailureKind::Timeout,
        message: "the tool did not answer before its deadline".into(),
        data: None,
    }
}
