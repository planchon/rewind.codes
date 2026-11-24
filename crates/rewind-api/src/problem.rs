//! Errors of the API: `application/problem+json` (RFC 9457).
//!
//! Every error response carries a [`Problem`]. Its `type` identifies the kind of error for
//! programs ([`ProblemType`]); its `detail` is written for a model: it says what was wrong and
//! what to do instead, because the agent tools hand it to the model as it is.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::common::Timestamp;
use crate::namespaces::{QuotaKind, QuotaScope};
use crate::query::{QueryError, QuerySpan};
use crate::scripts::Diagnostic;

/// The media type of every error response.
pub const CONTENT_TYPE: &str = "application/problem+json";

/// The kinds of error the API answers with. The URI of each is its `type` member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProblemType {
    /// 400: the request is malformed: a missing or invalid field, header or parameter.
    InvalidRequest,
    /// 400: a query (`q`) cannot be read; `query` says where the term at fault is.
    InvalidQuery,
    /// 401: no token, or an unknown, revoked or expired one; on `/auth`, no session.
    Unauthorized,
    /// 403: the caller's grant does not hold the permission this request needs (the message
    /// names it), or its allowed labels do not cover the request; a member without a grant in
    /// the organization.
    Forbidden,
    /// 404: no such run, action set, workflow, code, call, payload, subscription, API key, role,
    /// member or invitation (for this tenant).
    NotFound,
    /// 409: a signal or a cancellation sent to a run that has ended.
    RunEnded,
    /// 409: releasing or giving up a run that is not quarantined.
    NotQuarantined,
    /// 409: approving or rejecting a call that does not wait for the harness.
    CallNotWaiting,
    /// 409: deleting a role that a grant still points to.
    RoleInUse,
    /// 409: a change that would leave the organization without an `owner`.
    LastOwner,
    /// 409: starting a run in, or minting a token for, a namespace being erased.
    NamespaceDeleting,
    /// 410: the call no longer exists: its run ended, it was completed, or its claim expired
    /// and another worker took it.
    CallGone,
    /// 413: a body or a payload above the server's limits.
    PayloadTooLarge,
    /// 415: a body that is not JSON where JSON is expected.
    UnsupportedMediaType,
    /// 422: the code was refused before any run exists; `diagnostics` says why.
    InvalidCode,
    /// 422: the action set was refused: an invalid schema, a duplicate action name, or a transport
    /// this server does not allow.
    InvalidActionSet,
    /// 422: the workflow was refused: its name, a signal's name, or a schema that cannot be
    /// used. A code that does not check is `invalid-code`.
    InvalidWorkflow,
    /// 422: the input of a run of a workflow does not match the workflow's input schema.
    InvalidInput,
    /// 422: a test, or a suite, was refused: a test that is not valid, a stored test that does
    /// not exist, a retired or replaced test that is not inherited or has no reason, two tests
    /// of the same name.
    InvalidTest,
    /// 422: tests did not pass: the version was not saved, the fork not created; `tests` lists
    /// each, with its problem and its failures.
    TestsFailed,
    /// 422: a signal the run does not take: a name starting with `rewind:`, or, for a run of a
    /// workflow, a signal it does not declare or a payload that does not match.
    InvalidSignal,
    /// 422: a code's title that cannot be used: empty, or longer than 120 characters, or a
    /// description longer than 2,000.
    InvalidTitle,
    /// 422: a start, a fork or a workflow's save refused before anything exists: the code
    /// imports a server that needs a secret and none is named, a reference names a server
    /// that takes none or a secret that does not exist, or the caller may not use it;
    /// `secrets` says which, with the secrets the caller may use instead.
    SecretRequired,
    /// 429: the namespace or the tenant reached its `active_runs`, `runs_per_month` or
    /// `scripts`; `quota` says which, at which scope, and when it frees.
    QuotaExceeded,
    /// 500: a fault of the platform.
    Internal,
    /// 502: an MCP server of the action set cannot be reached, or answered with an error.
    ActionSetUnavailable,
    /// 502: the identity provider (WorkOS) cannot be reached, or answered with an error.
    IdentityUnavailable,
    /// 503: the server cannot serve the request now (stopping, or its store is unreachable).
    Unavailable,
}

impl ProblemType {
    pub const ALL: [ProblemType; 28] = [
        ProblemType::InvalidRequest,
        ProblemType::InvalidQuery,
        ProblemType::Unauthorized,
        ProblemType::Forbidden,
        ProblemType::NotFound,
        ProblemType::RunEnded,
        ProblemType::NotQuarantined,
        ProblemType::CallNotWaiting,
        ProblemType::RoleInUse,
        ProblemType::LastOwner,
        ProblemType::NamespaceDeleting,
        ProblemType::CallGone,
        ProblemType::PayloadTooLarge,
        ProblemType::UnsupportedMediaType,
        ProblemType::InvalidCode,
        ProblemType::InvalidActionSet,
        ProblemType::InvalidWorkflow,
        ProblemType::InvalidInput,
        ProblemType::InvalidTest,
        ProblemType::TestsFailed,
        ProblemType::InvalidSignal,
        ProblemType::InvalidTitle,
        ProblemType::SecretRequired,
        ProblemType::QuotaExceeded,
        ProblemType::Internal,
        ProblemType::ActionSetUnavailable,
        ProblemType::IdentityUnavailable,
        ProblemType::Unavailable,
    ];

    /// The `type` member: `urn:rewind:problem:<slug>`.
    pub fn uri(self) -> &'static str {
        match self {
            ProblemType::InvalidRequest => "urn:rewind:problem:invalid-request",
            ProblemType::InvalidQuery => "urn:rewind:problem:invalid-query",
            ProblemType::Unauthorized => "urn:rewind:problem:unauthorized",
            ProblemType::Forbidden => "urn:rewind:problem:forbidden",
            ProblemType::NotFound => "urn:rewind:problem:not-found",
            ProblemType::RunEnded => "urn:rewind:problem:run-ended",
            ProblemType::NotQuarantined => "urn:rewind:problem:not-quarantined",
            ProblemType::CallNotWaiting => "urn:rewind:problem:call-not-waiting",
            ProblemType::RoleInUse => "urn:rewind:problem:role-in-use",
            ProblemType::LastOwner => "urn:rewind:problem:last-owner",
            ProblemType::NamespaceDeleting => "urn:rewind:problem:namespace-deleting",
            ProblemType::CallGone => "urn:rewind:problem:call-gone",
            ProblemType::PayloadTooLarge => "urn:rewind:problem:payload-too-large",
            ProblemType::UnsupportedMediaType => "urn:rewind:problem:unsupported-media-type",
            ProblemType::InvalidCode => "urn:rewind:problem:invalid-code",
            ProblemType::InvalidActionSet => "urn:rewind:problem:invalid-action-set",
            ProblemType::InvalidWorkflow => "urn:rewind:problem:invalid-workflow",
            ProblemType::InvalidTest => "urn:rewind:problem:invalid-test",
            ProblemType::TestsFailed => "urn:rewind:problem:tests-failed",
            ProblemType::InvalidInput => "urn:rewind:problem:invalid-input",
            ProblemType::InvalidSignal => "urn:rewind:problem:invalid-signal",
            ProblemType::InvalidTitle => "urn:rewind:problem:invalid-title",
            ProblemType::SecretRequired => "urn:rewind:problem:secret-required",
            ProblemType::QuotaExceeded => "urn:rewind:problem:quota-exceeded",
            ProblemType::Internal => "urn:rewind:problem:internal",
            ProblemType::ActionSetUnavailable => "urn:rewind:problem:action-set-unavailable",
            ProblemType::IdentityUnavailable => "urn:rewind:problem:identity-unavailable",
            ProblemType::Unavailable => "urn:rewind:problem:unavailable",
        }
    }

    /// The HTTP status the API answers this problem with.
    pub fn status(self) -> u16 {
        match self {
            ProblemType::InvalidRequest | ProblemType::InvalidQuery => 400,
            ProblemType::Unauthorized => 401,
            ProblemType::Forbidden => 403,
            ProblemType::NotFound => 404,
            ProblemType::RunEnded
            | ProblemType::NotQuarantined
            | ProblemType::CallNotWaiting
            | ProblemType::RoleInUse
            | ProblemType::LastOwner
            | ProblemType::NamespaceDeleting => 409,
            ProblemType::CallGone => 410,
            ProblemType::PayloadTooLarge => 413,
            ProblemType::UnsupportedMediaType => 415,
            ProblemType::InvalidCode
            | ProblemType::InvalidActionSet
            | ProblemType::InvalidWorkflow
            | ProblemType::InvalidTest
            | ProblemType::TestsFailed
            | ProblemType::InvalidInput
            | ProblemType::InvalidSignal
            | ProblemType::InvalidTitle
            | ProblemType::SecretRequired => 422,
            ProblemType::QuotaExceeded => 429,
            ProblemType::Internal => 500,
            ProblemType::ActionSetUnavailable | ProblemType::IdentityUnavailable => 502,
            ProblemType::Unavailable => 503,
        }
    }

    /// A short, fixed summary of the kind of problem.
    pub fn title(self) -> &'static str {
        match self {
            ProblemType::InvalidRequest => "Invalid request",
            ProblemType::InvalidQuery => "The query cannot be read",
            ProblemType::Unauthorized => "Missing or invalid token",
            ProblemType::Forbidden => "Not allowed for this caller",
            ProblemType::NotFound => "Not found",
            ProblemType::RunEnded => "The run has ended",
            ProblemType::NotQuarantined => "The run is not quarantined",
            ProblemType::CallNotWaiting => "The call does not wait for the harness",
            ProblemType::RoleInUse => "The role is in use",
            ProblemType::LastOwner => "The organization would have no owner",
            ProblemType::NamespaceDeleting => "The namespace is being erased",
            ProblemType::CallGone => "The call no longer exists",
            ProblemType::PayloadTooLarge => "Payload too large",
            ProblemType::UnsupportedMediaType => "Unsupported media type",
            ProblemType::InvalidCode => "The code was refused",
            ProblemType::InvalidActionSet => "The action set was refused",
            ProblemType::InvalidWorkflow => "The workflow was refused",
            ProblemType::InvalidTest => "The test was refused",
            ProblemType::TestsFailed => "Tests did not pass",
            ProblemType::InvalidInput => "The input does not match the workflow",
            ProblemType::InvalidSignal => "The run does not take this signal",
            ProblemType::InvalidTitle => "The code's title was refused",
            ProblemType::SecretRequired => "A secret is missing or cannot be used",
            ProblemType::QuotaExceeded => "A quota is reached",
            ProblemType::Internal => "Internal error",
            ProblemType::ActionSetUnavailable => "A server of the action set is unavailable",
            ProblemType::IdentityUnavailable => "The identity provider is unavailable",
            ProblemType::Unavailable => "Service unavailable",
        }
    }

    pub fn from_uri(uri: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.uri() == uri)
    }
}

impl fmt::Display for ProblemType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.uri())
    }
}

/// An error answered by the API, as `application/problem+json` (RFC 9457).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(example = json!({
    "type": "urn:rewind:problem:run-ended",
    "title": "The run has ended",
    "status": 409,
    "detail": "run 0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10 has ended (completed): a signal can only be sent to a run that is still active"
}))]
pub struct Problem {
    /// Identifies the kind of problem: `urn:rewind:problem:<slug>`.
    pub r#type: String,
    /// A short summary of the kind of problem, the same for every occurrence.
    pub title: String,
    /// The HTTP status code.
    pub status: u16,
    /// What went wrong in this request, and what to do instead. Written for a model.
    pub detail: String,
    /// The path of the request, or of the resource the problem is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// With `invalid-code`: why the code was refused, with locations in `workflow.ts`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    /// With `invalid-query`: where the term at fault is in the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<QuerySpan>,
    /// With `tests-failed`: every test of the run, those that did not pass with their problem
    /// and failures.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<crate::testing::TestResult>,
    /// With `secret-required`: each secret at fault, with what the caller may use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<crate::secrets::SecretProblem>,
    /// With `quota-exceeded`: the quota reached.
    #[serde(default, skip_serializing_if = "Option::is_none", flatten)]
    #[schema(schema_with = quota_reached_fields)]
    pub quota: Option<QuotaReached>,
}

/// The schema of the fields a `quota-exceeded` problem adds at its top level: every one
/// optional, since other problems have none.
fn quota_reached_fields() -> utoipa::openapi::schema::Object {
    use utoipa::openapi::schema::{ObjectBuilder, Type};
    use utoipa::openapi::{KnownFormat, Ref, SchemaFormat};
    ObjectBuilder::new()
        .description(Some(
            "With `quota-exceeded`: the quota reached, its scope, its limit, what is used, and \
             when a monthly quota frees.",
        ))
        .property("quota", Ref::from_schema_name("QuotaKind"))
        .property("scope", Ref::from_schema_name("QuotaScope"))
        .property(
            "limit",
            ObjectBuilder::new()
                .schema_type(Type::Integer)
                .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int64)))
                .minimum(Some(0)),
        )
        .property(
            "used",
            ObjectBuilder::new()
                .schema_type(Type::Integer)
                .format(Some(SchemaFormat::KnownFormat(KnownFormat::Int64)))
                .minimum(Some(0)),
        )
        .property(
            "resetsAt",
            ObjectBuilder::new()
                .schema_type(Type::String)
                .format(Some(SchemaFormat::KnownFormat(KnownFormat::DateTime))),
        )
        .build()
}

/// The quota a `quota-exceeded` problem reached. A monthly quota's answer also carries
/// `Retry-After`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaReached {
    pub quota: QuotaKind,
    pub scope: QuotaScope,
    /// The limit: a count for `active_runs`, `runs_per_month` and `scripts`.
    pub limit: u64,
    pub used: u64,
    /// When a monthly quota frees: the first instant of the next month (UTC). Absent for
    /// `active_runs` and `scripts`, which free when runs end or are erased.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<Timestamp>,
}

impl Problem {
    pub fn new(kind: ProblemType, detail: impl Into<String>) -> Self {
        Self {
            r#type: kind.uri().to_owned(),
            title: kind.title().to_owned(),
            status: kind.status(),
            detail: detail.into(),
            instance: None,
            diagnostics: Vec::new(),
            query: None,
            tests: Vec::new(),
            secrets: Vec::new(),
            quota: None,
        }
    }

    /// The `tests-failed` problem: `tests` holds every test's outcome.
    pub fn tests_failed(detail: impl Into<String>, tests: Vec<crate::testing::TestResult>) -> Self {
        let mut problem = Self::new(ProblemType::TestsFailed, detail);
        problem.tests = tests;
        problem
    }

    /// The `quota-exceeded` problem of a quota reached; `detail` is written for a model and
    /// never names another namespace.
    pub fn quota_exceeded(detail: impl Into<String>, reached: QuotaReached) -> Self {
        let mut problem = Self::new(ProblemType::QuotaExceeded, detail);
        problem.quota = Some(reached);
        problem
    }

    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    pub fn with_diagnostics(mut self, diagnostics: Vec<Diagnostic>) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// The `invalid-query` problem of a query that cannot be read.
    pub fn invalid_query(error: &QueryError) -> Self {
        let mut problem = Self::new(
            ProblemType::InvalidQuery,
            format!(
                "q: {} (characters {} to {} of the query)",
                error.message, error.span.start, error.span.end
            ),
        );
        problem.query = Some(error.span);
        problem
    }

    /// The kind of problem, if this build knows it.
    pub fn kind(&self) -> Option<ProblemType> {
        ProblemType::from_uri(&self.r#type)
    }

    pub fn is(&self, kind: ProblemType) -> bool {
        self.r#type == kind.uri()
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.title, self.detail)?;
        for diagnostic in &self.diagnostics {
            write!(f, "\n{diagnostic}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn every_kind_has_its_own_uri_and_an_error_status() {
        for kind in ProblemType::ALL {
            assert_eq!(ProblemType::from_uri(kind.uri()), Some(kind));
            assert!((400..600).contains(&kind.status()), "{kind}");
            assert!(kind.uri().starts_with("urn:rewind:problem:"));
        }
        let mut uris: Vec<_> = ProblemType::ALL.iter().map(|kind| kind.uri()).collect();
        uris.sort_unstable();
        uris.dedup();
        assert_eq!(uris.len(), ProblemType::ALL.len());
    }

    #[test]
    fn a_problem_is_written_as_rfc_9457() {
        let problem = Problem::new(ProblemType::RunEnded, "run r1 has ended (completed)")
            .with_instance("/v1/runs/r1/signals/approval");
        assert_eq!(
            serde_json::to_value(&problem).unwrap(),
            json!({
                "type": "urn:rewind:problem:run-ended",
                "title": "The run has ended",
                "status": 409,
                "detail": "run r1 has ended (completed)",
                "instance": "/v1/runs/r1/signals/approval"
            })
        );
        let read: Problem = serde_json::from_value(json!({
            "type": "urn:rewind:problem:run-ended",
            "title": "The run has ended",
            "status": 409,
            "detail": "gone",
            "somethingNew": true
        }))
        .unwrap();
        assert_eq!(read.kind(), Some(ProblemType::RunEnded));
        assert!(read.is(ProblemType::RunEnded));
    }
}
