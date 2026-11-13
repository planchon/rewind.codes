//! What the harness decides for a task that waits for it (`archi/04-tools.md`, "Tool task
//! lifecycle", "Credentials", "When a toolset changes during a run"):
//!
//! | The task | The harness | Use case |
//! | --- | --- | --- |
//! | `approval` (the `approval` policy) | lets the call go | `approve_task` |
//! | `parked` by a schema change | continues against the new schema | `approve_task` |
//! | `parked` by a 401 | renews the credentials | `set_run_credentials`, `set_toolset_credentials` |
//! | any of them | refuses: the call fails with a `ToolError` | `reject_task` |
//!
//! Cancelling the run, or forking it with a corrected script, are the other ways out.

use rewind_core::{Digest, ToolFailure, ToolFailureKind};

use crate::engine::{Engine, EngineError};
use crate::ports::{
    BlobStore, Clock, Credentials, Entropy, JournalStore, ParkedScope, RunRef, ScriptExecutor,
    SecretOwner, SecretStore, TaskRef, TaskUpdate, TenantId, ToolDispatcher, ToolTask,
    WebhookSender,
};

/// The message of a rejection without one.
const REJECTED: &str = "rejected by the harness";

impl<J, B, X, D, C, E, S, W> Engine<J, B, X, D, C, E, S, W>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: Clock,
    E: Entropy,
    S: SecretStore,
    W: WebhookSender,
{
    /// Lets a task that waits for the harness go on: a task under the `approval` policy is
    /// sent, a task parked by a schema change is sent against the tool's current schema.
    pub async fn approve_task(&self, task: &TaskRef) -> Result<(), EngineError> {
        let update = self
            .journal
            .approve_tool_task(task, self.clock.now())
            .await?;
        waiting(task, update)
    }

    /// Refuses a task that waits for the harness: the call fails with a `ToolError` carrying
    /// `message` ("rejected by the harness" without one).
    pub async fn reject_task(
        &self,
        task: &TaskRef,
        message: Option<String>,
    ) -> Result<(), EngineError> {
        let failure = ToolFailure {
            kind: ToolFailureKind::Error,
            message: message.unwrap_or_else(|| REJECTED.to_owned()),
            data: None,
        };
        let update = self
            .journal
            .reject_tool_task(task, failure, self.clock.now())
            .await?;
        waiting(task, update)
    }

    /// Sets or renews the credentials of a toolset, used by every run of the toolset without
    /// credentials of its own. The calls parked by `credentials.expired` in those runs are
    /// ready again, and sent with the same idempotency key. Returns how many.
    pub async fn set_toolset_credentials(
        &self,
        tenant: &TenantId,
        toolset: Digest,
        credentials: &Credentials,
    ) -> Result<usize, EngineError> {
        self.toolset(tenant, toolset).await?;
        self.secrets
            .put(tenant, &SecretOwner::Toolset(toolset), credentials)
            .await?;
        let scope = ParkedScope::Toolset {
            tenant: tenant.clone(),
            toolset,
        };
        Ok(self.journal.resume_parked(&scope, self.clock.now()).await?)
    }

    /// Sets or renews a run's own credentials, which override its toolset's. The run's calls
    /// parked by `credentials.expired` are ready again, and sent with the same idempotency key.
    /// Returns how many.
    pub async fn set_run_credentials(
        &self,
        run: &RunRef,
        credentials: &Credentials,
    ) -> Result<usize, EngineError> {
        let record = self.run_record(run).await?;
        if record.status.is_terminal() {
            return Err(EngineError::RunEnded(run.id));
        }
        self.secrets
            .put(&run.tenant, &SecretOwner::Run(run.id), credentials)
            .await?;
        let scope = ParkedScope::Run(run.clone());
        Ok(self.journal.resume_parked(&scope, self.clock.now()).await?)
    }

    /// The run's tool tasks, by seq: the calls in flight and the ones that wait, for the
    /// run's state.
    pub async fn tool_tasks(&self, run: &RunRef) -> Result<Vec<ToolTask>, EngineError> {
        self.run_record(run).await?;
        Ok(self.journal.tool_tasks(run).await?)
    }
}

/// The outcome of a decision on a task that must wait for the harness.
fn waiting(task: &TaskRef, update: TaskUpdate) -> Result<(), EngineError> {
    match update {
        TaskUpdate::Applied => Ok(()),
        TaskUpdate::NotFound => Err(EngineError::TaskGone),
        TaskUpdate::InvalidState(state) => Err(EngineError::TaskNotWaiting {
            task: task.to_string(),
            state: state.name(),
        }),
    }
}
