//! Errors of the API: `application/problem+json` (RFC 9457).
//!
//! Every error response carries a [`Problem`]. Its `type` identifies the kind of error for
//! programs ([`ProblemType`]); its `detail` is written for a model: it says what was wrong and
//! what to do instead, because the agent tools hand it to the model as it is.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::scripts::Diagnostic;

/// The media type of every error response.
pub const CONTENT_TYPE: &str = "application/problem+json";

/// The kinds of error the API answers with. The URI of each is its `type` member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProblemType {
    /// 400: the request is malformed: a missing or invalid field, header or parameter.
    InvalidRequest,
    /// 401: no API key, or an unknown or revoked one.
    Unauthorized,
    /// 403: the key's role or its allowed labels do not cover this request.
    Forbidden,
    /// 404: no such run, toolset, script, task, payload, subscription or API key (for this
    /// tenant).
    NotFound,
    /// 409: a signal or a cancellation sent to a run that has ended.
    RunEnded,
    /// 409: releasing or giving up a run that is not quarantined.
    NotQuarantined,
    /// 409: approving or rejecting a task that does not wait for the harness.
    TaskNotWaiting,
    /// 410: the task no longer exists: its run ended, it was completed, or its claim expired
    /// and another worker took it.
    TaskGone,
    /// 413: a body or a payload above the server's limits.
    PayloadTooLarge,
    /// 415: a body that is not JSON where JSON is expected.
    UnsupportedMediaType,
    /// 422: the script was refused before any run exists; `diagnostics` says why.
    InvalidScript,
    /// 422: the toolset was refused: an invalid schema, a duplicate tool name, or a transport
    /// this server does not allow.
    InvalidToolset,
    /// 429: the tenant already has its maximum of active runs.
    QuotaExceeded,
    /// 500: a fault of the platform.
    Internal,
    /// 502: an MCP server of the toolset cannot be reached, or answered with an error.
    ToolsetUnavailable,
    /// 503: the server cannot serve the request now (stopping, or its store is unreachable).
    Unavailable,
}

impl ProblemType {
    pub const ALL: [ProblemType; 16] = [
        ProblemType::InvalidRequest,
        ProblemType::Unauthorized,
        ProblemType::Forbidden,
        ProblemType::NotFound,
        ProblemType::RunEnded,
        ProblemType::NotQuarantined,
        ProblemType::TaskNotWaiting,
        ProblemType::TaskGone,
        ProblemType::PayloadTooLarge,
        ProblemType::UnsupportedMediaType,
        ProblemType::InvalidScript,
        ProblemType::InvalidToolset,
        ProblemType::QuotaExceeded,
        ProblemType::Internal,
        ProblemType::ToolsetUnavailable,
        ProblemType::Unavailable,
    ];

    /// The `type` member: `urn:ptc:problem:<slug>`.
    pub fn uri(self) -> &'static str {
        match self {
            ProblemType::InvalidRequest => "urn:ptc:problem:invalid-request",
            ProblemType::Unauthorized => "urn:ptc:problem:unauthorized",
            ProblemType::Forbidden => "urn:ptc:problem:forbidden",
            ProblemType::NotFound => "urn:ptc:problem:not-found",
            ProblemType::RunEnded => "urn:ptc:problem:run-ended",
            ProblemType::NotQuarantined => "urn:ptc:problem:not-quarantined",
            ProblemType::TaskNotWaiting => "urn:ptc:problem:task-not-waiting",
            ProblemType::TaskGone => "urn:ptc:problem:task-gone",
            ProblemType::PayloadTooLarge => "urn:ptc:problem:payload-too-large",
            ProblemType::UnsupportedMediaType => "urn:ptc:problem:unsupported-media-type",
            ProblemType::InvalidScript => "urn:ptc:problem:invalid-script",
            ProblemType::InvalidToolset => "urn:ptc:problem:invalid-toolset",
            ProblemType::QuotaExceeded => "urn:ptc:problem:quota-exceeded",
            ProblemType::Internal => "urn:ptc:problem:internal",
            ProblemType::ToolsetUnavailable => "urn:ptc:problem:toolset-unavailable",
            ProblemType::Unavailable => "urn:ptc:problem:unavailable",
        }
    }

    /// The HTTP status the API answers this problem with.
    pub fn status(self) -> u16 {
        match self {
            ProblemType::InvalidRequest => 400,
            ProblemType::Unauthorized => 401,
            ProblemType::Forbidden => 403,
            ProblemType::NotFound => 404,
            ProblemType::RunEnded | ProblemType::NotQuarantined | ProblemType::TaskNotWaiting => {
                409
            }
            ProblemType::TaskGone => 410,
            ProblemType::PayloadTooLarge => 413,
            ProblemType::UnsupportedMediaType => 415,
            ProblemType::InvalidScript | ProblemType::InvalidToolset => 422,
            ProblemType::QuotaExceeded => 429,
            ProblemType::Internal => 500,
            ProblemType::ToolsetUnavailable => 502,
            ProblemType::Unavailable => 503,
        }
    }

    /// A short, fixed summary of the kind of problem.
    pub fn title(self) -> &'static str {
        match self {
            ProblemType::InvalidRequest => "Invalid request",
            ProblemType::Unauthorized => "Missing or invalid API key",
            ProblemType::Forbidden => "Not allowed for this API key",
            ProblemType::NotFound => "Not found",
            ProblemType::RunEnded => "The run has ended",
            ProblemType::NotQuarantined => "The run is not quarantined",
            ProblemType::TaskNotWaiting => "The task does not wait for the harness",
            ProblemType::TaskGone => "The task no longer exists",
            ProblemType::PayloadTooLarge => "Payload too large",
            ProblemType::UnsupportedMediaType => "Unsupported media type",
            ProblemType::InvalidScript => "The script was refused",
            ProblemType::InvalidToolset => "The toolset was refused",
            ProblemType::QuotaExceeded => "Too many active runs",
            ProblemType::Internal => "Internal error",
            ProblemType::ToolsetUnavailable => "A server of the toolset is unavailable",
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
    "type": "urn:ptc:problem:run-ended",
    "title": "The run has ended",
    "status": 409,
    "detail": "run 0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10 has ended (completed): a signal can only be sent to a run that is still active"
}))]
pub struct Problem {
    /// Identifies the kind of problem: `urn:ptc:problem:<slug>`.
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
    /// With `invalid-script`: why the script was refused, with locations in `script.ts`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
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
        }
    }

    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    pub fn with_diagnostics(mut self, diagnostics: Vec<Diagnostic>) -> Self {
        self.diagnostics = diagnostics;
        self
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
            assert!(kind.uri().starts_with("urn:ptc:problem:"));
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
                "type": "urn:ptc:problem:run-ended",
                "title": "The run has ended",
                "status": 409,
                "detail": "run r1 has ended (completed)",
                "instance": "/v1/runs/r1/signals/approval"
            })
        );
        let read: Problem = serde_json::from_value(json!({
            "type": "urn:ptc:problem:run-ended",
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
