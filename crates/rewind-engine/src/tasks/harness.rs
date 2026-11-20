//! What the harness decides for a task that waits for it (`archi/04-tools.md`, "Tool task
//! lifecycle", "Secrets", "When a toolset changes during a run"):
//!
//! | The task | The harness | Use case |
//! | --- | --- | --- |
//! | `approval` (the `approval` policy) | lets the call go | `approve_task` |
//! | `parked` by a schema change | continues against the new schema | `approve_task` |
//! | `parked` by a 401 (`credentials_refused`) | puts the secret again, or approves once the server accepts the key | `put_secret`, `approve_task` |
//! | `parked` by a missing secret (`secret_missing`) | puts the secret again | `put_secret` |
//! | any of them | refuses: the call fails with a `ToolError` | `reject_task` |
//!
//! Cancelling the run, or forking it with a corrected script, are the other ways out.

use rewind_core::{ToolFailure, ToolFailureKind};

use crate::engine::{Engine, EngineError};
use crate::ports::{
    BlobStore, Clock, Entropy, JournalStore, NewSecret, ParkedScope, PutSecret, RunRef,
    ScriptExecutor, SecretId, SecretStore, TaskRef, TaskUpdate, TenantId, ToolDispatcher, ToolTask,
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

    /// Creates or replaces a secret (`PUT /v1/secrets/{name}`, `PUT
    /// /v1/namespaces/{name}/secrets/{secret}`), then makes ready the calls parked by a refusal
    /// or a missing secret in the runs that attach it, sent again with the same idempotency key
    /// (`archi/04-tools.md`, "Secrets"). Returns what was written and how many calls are ready
    /// again.
    pub async fn put_secret(
        &self,
        tenant: &TenantId,
        secret: &NewSecret,
    ) -> Result<(PutSecret, usize), EngineError> {
        let now = self.clock.now();
        let put = self.secrets.put(tenant, secret, now).await?;
        let scope = ParkedScope::Secret {
            tenant: tenant.clone(),
            secret: secret.id.clone(),
        };
        let resumed = self.journal.resume_parked(&scope, now).await?;
        Ok((put, resumed))
    }

    /// Deletes a secret: the next call of a run that attaches it is parked
    /// (`credentials.expired`, reason `missing`). Returns whether it existed.
    pub async fn delete_secret(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
    ) -> Result<bool, EngineError> {
        Ok(self.secrets.delete(tenant, secret).await?)
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
