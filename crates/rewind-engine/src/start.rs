//! What a start asks, and why it is refused (`archi/03-execution.md`, "Starting a run";
//! `archi/06-storage-and-scale.md`, "Quotas", "Namespaces").
//!
//! The edge checks a start before it asks the owner of the run's partition
//! ([`Edge::start_run`](crate::edge::Edge::start_run)): the namespace, the script, the TTL, the
//! input, the attachments, the type-check; the owner applies the start key and the admission.

use std::time::Duration;

use rewind_core::{ActionSet, JsonValue, RunMode, Script};

use crate::attachments::{AttachRights, RefusedAttachments};
use crate::ports::{
    CheckerError, Diagnostic, InvalidScript, NamespaceName, QuotaExceeded, RunMetadata,
    SecretError, Severity, StoreError, TenantId, ToolDeclarations, TypeChecker,
};
use crate::text::{self, NulCharacter};

/// A request to start a run.
#[derive(Debug, Clone)]
pub struct StartRun {
    pub tenant: TenantId,
    /// The end user the run acts for (`archi/06-storage-and-scale.md`, "Namespaces").
    pub namespace: NamespaceName,
    pub script: Script,
    pub action_set: ActionSet,
    /// The declarations generated from the action set, stored with it.
    pub declarations: ToolDeclarations,
    pub input: JsonValue,
    pub mode: RunMode,
    /// The run's TTL; the node's default TTL when `None`, bounded by its namespace's.
    pub ttl: Option<Duration>,
    /// Its `secrets` are the request's attachments (`archi/04-tools.md`, "Secrets"), checked
    /// against the action set, the secrets that exist and `attach`.
    pub metadata: RunMetadata,
    /// A retried request with the same key returns the same run instead of starting another.
    pub idempotency_key: Option<String>,
    /// What the caller may attach.
    pub attach: AttachRights,
}

impl StartRun {
    /// A live run of the tenant `local`, in its namespace `local`, with no declarations, TTL,
    /// metadata, idempotency key nor attachment.
    pub fn new(script: Script, action_set: ActionSet, input: JsonValue) -> Self {
        Self {
            tenant: TenantId::local(),
            namespace: NamespaceName::new(NamespaceName::LOCAL)
                .unwrap_or_else(|_| NamespaceName::migrated()),
            script,
            action_set,
            declarations: ToolDeclarations::default(),
            input,
            mode: RunMode::Live,
            ttl: None,
            metadata: RunMetadata::default(),
            idempotency_key: None,
            attach: AttachRights::NamespaceOnly,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StartError {
    /// The script cannot run: it does not compile, imports what does not exist, has no
    /// default export… The message is written to be read by the model that wrote the script.
    #[error("the script is invalid: {0}")]
    InvalidScript(InvalidScript),
    /// The script does not type-check against the sandbox lib and the action set's declarations:
    /// at least one diagnostic is an error. Every diagnostic is given, warnings included.
    #[error("the script does not type-check: {}", first_error(.0))]
    TypeErrors(Vec<Diagnostic>),
    /// The type-checker itself failed: the script was neither accepted nor refused.
    #[error(transparent)]
    Checker(#[from] CheckerError),
    #[error("invalid TTL {requested:?}: it must be positive and at most {max:?}")]
    InvalidTtl { requested: Duration, max: Duration },
    /// The script has more lines than its namespace's `max_script_lines`, or its tenant's.
    #[error("the script has {lines} lines; this namespace accepts at most {max}")]
    ScriptTooLong { lines: u64, max: u64 },
    #[error("the input is {size} bytes as canonical JSON, above the limit of {max} bytes")]
    InputTooLarge { size: usize, max: usize },
    /// A text to store (the script, a label, the idempotency key, the trace context, the
    /// action set's declarations) holds a NUL character.
    #[error(transparent)]
    NulCharacter(#[from] NulCharacter),
    #[error("the executor supports no determinism epoch")]
    NoEpoch,
    /// The namespace's limit or the tenant's ceiling of `active_runs`, `runs_per_month` or
    /// `scripts` is reached.
    #[error("the {} quota {} is reached: {} of {}", scope_name(.0), .0.quota, .0.used, .0.limit)]
    QuotaExceeded(QuotaExceeded),
    /// The namespace is being erased: it takes no run.
    #[error("the namespace is being erased: it takes no run")]
    NamespaceDeleting,
    /// No such workflow version in the tenant.
    #[error("workflow version {0} not found")]
    WorkflowNotFound(String),
    /// An attachment is missing, takes no secret, names a secret that does not exist or one the
    /// caller may not attach (`archi/04-tools.md`, "Secrets"): 422 `secret-required`, whose
    /// message lists the secrets the caller may attach instead.
    #[error("{0}")]
    SecretRequired(RefusedAttachments),
    /// The input does not match the workflow's input schema.
    #[error("{0}")]
    InvalidInput(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Secret(#[from] SecretError),
}

/// The bounds of a start or a fork in a namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NamespaceBounds {
    pub(crate) max_ttl: Duration,
    /// The deployment's default TTL, lowered to `max_ttl`.
    pub(crate) default_ttl: Duration,
    pub(crate) max_script_lines: Option<u64>,
}

impl NamespaceBounds {
    /// Refuses a script with more lines than `max_script_lines`.
    pub(crate) fn check_lines(&self, script: &Script) -> Result<(), StartError> {
        let Some(max) = self.max_script_lines else {
            return Ok(());
        };
        let lines = script.source().lines().count() as u64;
        if lines > max {
            return Err(StartError::ScriptTooLong { lines, max });
        }
        Ok(())
    }
}

fn scope_name(exceeded: &QuotaExceeded) -> &'static str {
    match exceeded.scope {
        crate::ports::QuotaScope::Namespace => "namespace's",
        crate::ports::QuotaScope::Tenant => "organization's",
    }
}

/// Type-checks a script; refuses it when a diagnostic is an error.
pub(crate) async fn type_check<K: TypeChecker>(
    checker: &K,
    script: &Script,
    declarations: &ToolDeclarations,
) -> Result<(), StartError> {
    let diagnostics = checker.check(script, declarations).await?;
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == Severity::Error)
    {
        return Err(StartError::TypeErrors(diagnostics));
    }
    Ok(())
}

/// The first error of a refused check, for the message of [`StartError::TypeErrors`].
fn first_error(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .find(|diagnostic| diagnostic.severity == Severity::Error)
        .map_or_else(String::new, ToString::to_string)
}

/// Refuses run metadata, or an idempotency key, holding a NUL character.
pub(crate) fn refuse_nul_in_metadata(
    metadata: &RunMetadata,
    idempotency_key: Option<&str>,
) -> Result<(), NulCharacter> {
    text::refuse_nul_in_labels(&metadata.labels)?;
    text::refuse_nul_in_labels(&metadata.scope)?;
    if let Some(trace) = &metadata.trace_context {
        text::refuse_nul_in_trace(trace)?;
    }
    idempotency_key.map_or(Ok(()), |key| text::refuse_nul("the idempotency key", key))
}
