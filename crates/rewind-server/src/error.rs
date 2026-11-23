//! Errors of the API: every one is answered as `application/problem+json`, with a detail
//! written for a model (`rewind_api::problem`).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rewind_api::scripts::{Diagnostic, Location, Severity};
use rewind_api::{Problem, ProblemType};
use rewind_engine::edge::EdgeError;
use rewind_engine::ports::link::Refusal;
use rewind_engine::ports::{
    ArchiveError, BlobError, CatalogError, CheckerError, DiagnosticSource, IdentityError,
    InvalidScript, SecretError, StoreError,
};
use rewind_engine::{EngineError, ForkError, NulCharacter, StartError};

/// An error answered by the API.
#[derive(Debug)]
pub(crate) struct ApiError(pub(crate) Box<Problem>);

impl ApiError {
    pub(crate) fn new(kind: ProblemType, detail: impl Into<String>) -> Self {
        Self(Box::new(Problem::new(kind, detail)))
    }

    /// `quota-exceeded`, its detail written for a model: which quota, at which scope, and what
    /// to do. It never names another namespace.
    pub(crate) fn quota_exceeded(exceeded: &rewind_engine::ports::QuotaExceeded) -> Self {
        use rewind_api::namespaces::{QuotaKind, QuotaScope};
        use rewind_engine::ports::{QuotaKind as Kind, QuotaScope as Scope};
        let quota = match exceeded.quota {
            Kind::CpuPerMonth => QuotaKind::CpuPerMonth,
            Kind::ActiveRuns => QuotaKind::ActiveRuns,
            Kind::RunsPerMonth => QuotaKind::RunsPerMonth,
            Kind::Scripts => QuotaKind::Scripts,
            Kind::MaxTtl => QuotaKind::MaxTtl,
            Kind::MaxScriptLines => QuotaKind::MaxScriptLines,
        };
        let (scope, whose) = match exceeded.scope {
            Scope::Namespace => (QuotaScope::Namespace, "this namespace"),
            Scope::Tenant => (QuotaScope::Organization, "the organization"),
        };
        let what = match exceeded.quota {
            Kind::ActiveRuns => "active runs: wait for runs to end, or cancel some",
            Kind::RunsPerMonth => "runs this month: wait for the next month",
            Kind::Scripts => "distinct scripts: reuse a script that already ran, or erase old runs",
            _ => "of this quota",
        };
        let detail = format!(
            "{whose} has {} of its {} {what}",
            exceeded.used, exceeded.limit
        );
        Self(Box::new(Problem::quota_exceeded(
            detail,
            rewind_api::problem::QuotaReached {
                quota,
                scope,
                limit: exceeded.limit,
                used: exceeded.used,
                resets_at: exceeded.resets_at.map(rewind_api::common::timestamp),
            },
        )))
    }

    pub(crate) fn invalid(detail: impl Into<String>) -> Self {
        Self::new(ProblemType::InvalidRequest, detail)
    }

    pub(crate) fn not_found(detail: impl Into<String>) -> Self {
        Self::new(ProblemType::NotFound, detail)
    }

    pub(crate) fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(ProblemType::Forbidden, detail)
    }

    pub(crate) fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "internal error");
        Self::new(
            ProblemType::Internal,
            format!("the server failed: {error}. Retry later; if it persists, report it"),
        )
    }

    pub(crate) fn unavailable(detail: impl Into<String>) -> Self {
        Self::new(ProblemType::Unavailable, detail)
    }

    pub(crate) fn run_not_found(run: &str) -> Self {
        Self::not_found(format!(
            "no run {run}: check the id or the handle (runs are listed by GET /v1/runs)"
        ))
    }

    pub(crate) fn invalid_script(detail: impl Into<String>, diagnostics: Vec<Diagnostic>) -> Self {
        Self(Box::new(
            Problem::new(ProblemType::InvalidScript, detail).with_diagnostics(diagnostics),
        ))
    }

    /// `secret-required`: each attachment at fault, with the secrets the caller may attach for
    /// it instead (none for a server that takes no secret).
    pub(crate) fn secret_required(
        detail: impl Into<String>,
        refused: &rewind_engine::attachments::RefusedAttachments,
    ) -> Self {
        use rewind_api::secrets::{AttachmentProblem, AttachmentProblemKind};
        use rewind_engine::attachments::AttachmentProblem as Problem_;
        let attachable: Vec<String> = refused.attachable.iter().map(ToString::to_string).collect();
        let mut problem = Problem::new(ProblemType::SecretRequired, detail);
        problem.attachments = refused
            .problems
            .iter()
            .map(|problem| {
                let kind = match problem {
                    Problem_::Missing { .. } => AttachmentProblemKind::Missing,
                    Problem_::TakesNone { .. } => AttachmentProblemKind::TakesNone,
                    Problem_::Unknown { .. } => AttachmentProblemKind::Unknown,
                    Problem_::Forbidden { .. } => AttachmentProblemKind::Forbidden,
                };
                AttachmentProblem {
                    server: problem.server().to_owned(),
                    problem: kind,
                    secret: problem.secret().map(ToString::to_string),
                    attachable: if kind == AttachmentProblemKind::TakesNone {
                        Vec::new()
                    } else {
                        attachable.clone()
                    },
                }
            })
            .collect();
        Self(Box::new(problem))
    }
}

/// When the quota a `quota-exceeded` answer reached frees: the router turns it into
/// `Retry-After`, in seconds from the server's now (`http::every_response`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct FreesAt(pub(crate) rewind_core::Timestamp);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(&*self.0).unwrap_or_default();
        let frees_at = self
            .0
            .quota
            .as_ref()
            .and_then(|quota| quota.resets_at)
            .map(crate::http::runs::core_time);
        let mut response = (status, body).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(rewind_api::problem::CONTENT_TYPE),
        );
        if let Some(at) = frees_at {
            response.extensions_mut().insert(FreesAt(at));
        }
        response
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::RunNotFound(run) => Self::run_not_found(&run.to_string()),
            StoreError::Backend(message) => {
                tracing::error!(%message, "store error");
                Self::unavailable(format!(
                    "the store cannot be reached ({message}): retry later"
                ))
            }
            other => Self::internal(other),
        }
    }
}

/// The identity provider's answers (`archi/07-code-architecture.md`, "Sign-in (`/auth`)"): its
/// messages never hold a secret the call sent (`rewind-workos` redacts them).
impl From<IdentityError> for ApiError {
    fn from(error: IdentityError) -> Self {
        match error {
            IdentityError::Unavailable(message) => {
                tracing::warn!(%message, "the identity provider is unavailable");
                Self::new(
                    ProblemType::IdentityUnavailable,
                    format!(
                        "the identity provider (WorkOS) cannot be reached or failed ({message}): \
                         retry later"
                    ),
                )
            }
            IdentityError::Refused { code, message } => Self::invalid(format!(
                "the identity provider refused the request ({code}): {message}"
            )),
            IdentityError::NotFound(what) => Self::not_found(format!(
                "{what} not found in the identity provider: check the id"
            )),
            IdentityError::InvalidToken => Self::new(
                ProblemType::Unauthorized,
                "the token is invalid or expired: sign in again",
            ),
        }
    }
}

impl From<NulCharacter> for ApiError {
    fn from(error: NulCharacter) -> Self {
        Self::invalid(error.to_string())
    }
}

impl From<CatalogError> for ApiError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::Unavailable(message) | CatalogError::Backend(message) => {
                tracing::error!(%message, "control plane error");
                Self::unavailable(format!(
                    "the control plane cannot be reached ({message}): retry later"
                ))
            }
            CatalogError::Conflict(message) => Self::internal(message),
        }
    }
}

impl From<ArchiveError> for ApiError {
    fn from(error: ArchiveError) -> Self {
        Self::internal(error)
    }
}

/// What the owner of a run refused, as the routes answer it (`archi/07-code-architecture.md`,
/// "HTTP API `/v1`").
impl From<Refusal> for ApiError {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::RunNotFound(run) => Self::run_not_found(&run.to_string()),
            Refusal::RunEnded(run) => Self::new(
                ProblemType::RunEnded,
                format!("run {run} has ended: nothing can be sent to it any more"),
            ),
            Refusal::Signal(message) => Self::new(ProblemType::InvalidSignal, message),
            Refusal::Quota(exceeded) => Self::quota_exceeded(&exceeded),
            Refusal::NamespaceDeleting => Self::new(
                ProblemType::NamespaceDeleting,
                "this namespace is being erased: it takes no run",
            ),
            Refusal::TaskGone => Self::new(ProblemType::TaskGone, refusal.to_string()),
            Refusal::TaskNotWaiting { .. } => {
                Self::new(ProblemType::TaskNotWaiting, refusal.to_string())
            }
            Refusal::ResultTooLarge { .. } => Self::new(
                ProblemType::PayloadTooLarge,
                format!("{refusal}: fail the task instead, or return less"),
            ),
            Refusal::ParentNotStarted(_) => Self::invalid(refusal.to_string()),
            Refusal::Unavailable(message) => {
                tracing::warn!(%message, "an owner could not answer");
                Self::unavailable(format!("{message}: retry"))
            }
        }
    }
}

impl From<EdgeError> for ApiError {
    fn from(error: EdgeError) -> Self {
        match error {
            EdgeError::Refused(refusal) => refusal.into(),
            EdgeError::NulCharacter(error) => error.into(),
            EdgeError::Unavailable(message) => {
                tracing::warn!(%message, "the run plane could not answer");
                Self::unavailable(format!("{message}: retry"))
            }
        }
    }
}

impl From<BlobError> for ApiError {
    fn from(error: BlobError) -> Self {
        Self::internal(error)
    }
}

impl From<SecretError> for ApiError {
    fn from(error: SecretError) -> Self {
        Self::internal(error)
    }
}

impl From<CheckerError> for ApiError {
    fn from(error: CheckerError) -> Self {
        tracing::error!(%error, "the type-checker failed");
        Self::unavailable(format!(
            "{error}. The script was neither accepted nor refused: retry, or start it with \
             `check: false`"
        ))
    }
}

impl From<EngineError> for ApiError {
    fn from(error: EngineError) -> Self {
        match error {
            EngineError::RunNotFound(run) => Self::run_not_found(&run.to_string()),
            EngineError::RunEnded(run) => Self::new(
                ProblemType::RunEnded,
                format!("run {run} has ended: nothing can be sent to it any more"),
            ),
            EngineError::TaskGone => Self::new(ProblemType::TaskGone, error.to_string()),
            EngineError::TaskNotWaiting { .. } => {
                Self::new(ProblemType::TaskNotWaiting, error.to_string())
            }
            EngineError::ResultTooLarge { .. } => Self::new(
                ProblemType::PayloadTooLarge,
                format!("{error}: fail the task instead, or return less"),
            ),
            EngineError::ScriptNotFound(digest) => {
                Self::not_found(format!("no script {digest} for this tenant"))
            }
            EngineError::ToolsetNotFound(digest) => Self::not_found(format!(
                "no toolset {digest} for this tenant: register it with POST /v1/toolsets"
            )),
            EngineError::NulCharacter(error) => error.into(),
            EngineError::Store(error) => error.into(),
            EngineError::SignalRefused(message) => Self::new(ProblemType::InvalidSignal, message),
            other => Self::internal(other),
        }
    }
}

impl From<StartError> for ApiError {
    fn from(error: StartError) -> Self {
        match error {
            StartError::InvalidScript(invalid) => {
                let diagnostic = invalid_script_diagnostic(&invalid);
                Self::invalid_script(
                    format!("the script cannot run: {}", invalid.message),
                    vec![diagnostic],
                )
            }
            StartError::TypeErrors(diagnostics) => {
                let errors = diagnostics
                    .iter()
                    .filter(|d| d.severity == rewind_engine::ports::Severity::Error)
                    .count();
                Self::invalid_script(
                    format!(
                        "the script does not type-check ({errors} error{}): fix the diagnostics \
                         and start it again",
                        if errors == 1 { "" } else { "s" }
                    ),
                    diagnostics.iter().map(diagnostic).collect(),
                )
            }
            StartError::InvalidTtl { .. } => Self::invalid(error.to_string()),
            StartError::NulCharacter(error) => error.into(),
            StartError::InputTooLarge { .. } => {
                Self::new(ProblemType::PayloadTooLarge, error.to_string())
            }
            StartError::QuotaExceeded(exceeded) => Self::quota_exceeded(&exceeded),
            StartError::NamespaceDeleting => Self::new(
                ProblemType::NamespaceDeleting,
                "this namespace is being erased: it takes no run",
            ),
            StartError::ScriptTooLong { lines, max } => Self::invalid_script(
                error.to_string(),
                vec![Diagnostic {
                    severity: Severity::Error,
                    code: "rewind/max-script-lines".into(),
                    message: format!(
                        "the script has {lines} lines; this namespace accepts at most {max}"
                    ),
                    location: None,
                    frame: None,
                    hint: Some("split the work into a shorter script".into()),
                }],
            ),
            StartError::Checker(error) => error.into(),
            StartError::Store(error) => error.into(),
            StartError::WorkflowNotFound(version) => {
                Self::not_found(format!("no workflow version {version} for this tenant"))
            }
            StartError::InvalidInput(message) => Self::new(ProblemType::InvalidInput, message),
            StartError::SecretRequired(ref refused) => {
                Self::secret_required(error.to_string(), refused)
            }
            StartError::Secret(error) => error.into(),
            other => Self::internal(other),
        }
    }
}

impl From<ForkError> for ApiError {
    fn from(error: ForkError) -> Self {
        match error {
            ForkError::ParentNotFound(run) => Self::run_not_found(&run.to_string()),
            ForkError::ParentNotStarted(_) | ForkError::ParentArchived(_) => {
                Self::invalid(error.to_string())
            }
            ForkError::Start(error) => error.into(),
            ForkError::Engine(error) => error.into(),
        }
    }
}

/// A diagnostic of the checker, as the API writes it.
pub(crate) fn diagnostic(diagnostic: &rewind_engine::ports::Diagnostic) -> Diagnostic {
    Diagnostic {
        severity: match diagnostic.severity {
            rewind_engine::ports::Severity::Error => Severity::Error,
            rewind_engine::ports::Severity::Warning => Severity::Warning,
        },
        code: match &diagnostic.source {
            DiagnosticSource::TypeScript { code } => format!("TS{code}"),
            DiagnosticSource::Rule(rule) => format!("rewind/{rule}"),
        },
        message: diagnostic.message.clone(),
        location: (diagnostic.line > 0).then_some(Location {
            line: diagnostic.line,
            column: diagnostic.column,
            end_line: None,
            end_column: None,
        }),
        frame: (!diagnostic.frame.is_empty()).then(|| diagnostic.frame.clone()),
        hint: diagnostic.hint.clone(),
    }
}

/// The checks at start refused the script: one diagnostic, located where the message says.
pub(crate) fn invalid_script_diagnostic(invalid: &InvalidScript) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        code: "rewind/invalid-script".into(),
        message: invalid.message.clone(),
        location: rewind_engine::failures::script_location(&invalid.message).map(
            |(line, column)| Location {
                line,
                column,
                end_line: None,
                end_column: None,
            },
        ),
        frame: None,
        hint: None,
    }
}
