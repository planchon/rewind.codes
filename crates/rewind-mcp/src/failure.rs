//! What went wrong talking to an MCP server, sorted the way the engine needs it
//! (`archi/04-tools.md`, "Call lifecycle"): a refused secret or signature parks the call,
//! transient failures are retried under `retry`, the others fail the call for good.
//!
//! | What happened | Outcome |
//! | --- | --- |
//! | HTTP 401, with or without a `WWW-Authenticate` challenge | parked: `credentials_refused` |
//! | The server cannot be reached, the connection or the process closed, HTTP 429 or 5xx, JSON-RPC internal error (-32603), a session that could not be recovered | transient: `unavailable` |
//! | No answer within the execution timeout | transient: `timeout` |
//! | The tool reported an error (`isError`), any other JSON-RPC error, an answer that is not a tool result, another HTTP 4xx, a command that does not exist | failed |
//! | The egress checks refuse the server's address, a redirect elsewhere than the server's origin | failed: `unavailable` |
//!
//! A failure quotes what the server answered (a body, a challenge), cut at 2,000 characters,
//! unless the connection reached an internal server ([`Answers::Withheld`]): it then says only
//! the HTTP status and what Rewind saw itself, such as a connection refused.

use std::error::Error;
use std::time::Duration;

use rewind_core::{ActionFailure, ActionFailureKind};
use rewind_engine::ports::{Dispatched, ParkKind, ParkReason};
use rmcp::ServiceError;
use rmcp::model::ErrorCode;
use rmcp::service::ClientInitializeError;
use rmcp::transport::DynamicTransportError;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, HttpStatusError, InsufficientScopeError, StreamableHttpError,
};
use serde_json::Value;

use rewind_outbound::{Egress, SigningError};

use crate::McpError;

/// Characters of a message from a server kept in a failure: the script and the model read it.
const MESSAGE: usize = 2000;

/// Whether a failure may quote what the server answered over HTTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answers {
    /// A public server, a stdio server, or any server of the embedded server.
    Quoted,
    /// A server reached at an address only the operator's allow-list lets through: a tenant
    /// must not read an internal service through the messages of an action set that points at it.
    Withheld,
}

/// Ends a message whose detail is withheld.
const WITHHELD: &str = "the answer of an internal server is not quoted";

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Failure {
    /// The server refused the secret or the signature (HTTP 401).
    Unauthorized(String),
    /// Worth retrying: nothing says the server will refuse again.
    Transient(ActionFailure),
    /// The same call would fail again.
    Permanent(ActionFailure),
}

impl Failure {
    pub(crate) fn transient(kind: ActionFailureKind, message: impl Into<String>) -> Self {
        Self::Transient(failure(kind, message, None))
    }

    pub(crate) fn permanent(
        kind: ActionFailureKind,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        Self::Permanent(failure(kind, message, data))
    }

    /// The outcome of the attempt, for the engine.
    pub(crate) fn into_dispatched(self) -> Dispatched {
        match self {
            Failure::Unauthorized(message) => Dispatched::Park(ParkReason {
                kind: ParkKind::CredentialsRefused,
                message,
            }),
            Failure::Transient(failure) => Dispatched::Transient(failure),
            Failure::Permanent(failure) => Dispatched::Failed(failure),
        }
    }

    /// The error of a discovery, for the harness developer who registers the action set.
    pub(crate) fn into_error(self, server: &str) -> McpError {
        let server = server.to_owned();
        match self {
            Failure::Unauthorized(message) => McpError::Unauthorized { server, message },
            Failure::Transient(failure) if failure.kind == ActionFailureKind::Timeout => {
                McpError::Timeout {
                    server,
                    message: failure.message,
                }
            }
            Failure::Transient(failure) => McpError::Unreachable {
                server,
                message: failure.message,
            },
            Failure::Permanent(failure) => McpError::Refused {
                server,
                message: failure.message,
            },
        }
    }
}

pub(crate) fn failure(
    kind: ActionFailureKind,
    message: impl Into<String>,
    data: Option<Value>,
) -> ActionFailure {
    ActionFailure {
        kind,
        message: bounded(&message.into()),
        data,
    }
}

/// A message cut at [`MESSAGE`] characters.
pub(crate) fn bounded(message: &str) -> String {
    if message.chars().count() <= MESSAGE {
        return message.to_owned();
    }
    let mut cut: String = message.chars().take(MESSAGE).collect();
    cut.push('…');
    cut
}

/// A failed request on an open connection. The second value says whether the connection is
/// broken and must be dropped. The errors of the JSON-RPC protocol are what an MCP server says
/// to its clients, and are quoted whatever `answers` says.
pub(crate) fn of_service(server: &str, error: &ServiceError, answers: Answers) -> (Failure, bool) {
    match error {
        ServiceError::McpError(data) => {
            let message = format!("{server} answered with an error: {}", data.message);
            let failure = if data.code == ErrorCode::INTERNAL_ERROR {
                Failure::transient(ActionFailureKind::Unavailable, message)
            } else {
                Failure::permanent(ActionFailureKind::Error, message, data.data.clone())
            };
            (failure, false)
        }
        ServiceError::TransportSend(transport) => (of_transport(server, transport, answers), true),
        ServiceError::TransportClosed => (
            Failure::transient(
                ActionFailureKind::Unavailable,
                format!("the connection to {server} closed"),
            ),
            true,
        ),
        ServiceError::Timeout { timeout } => (timed_out(server, *timeout), false),
        ServiceError::UnexpectedResponse => (
            Failure::permanent(
                ActionFailureKind::Error,
                format!("{server} did not answer with an action result"),
                None,
            ),
            false,
        ),
        ServiceError::InputRequiredRoundsExceeded { .. } => (
            Failure::permanent(
                ActionFailureKind::Error,
                format!("{server} asked for input that Rewind cannot give"),
                None,
            ),
            false,
        ),
        other => (
            Failure::transient(
                ActionFailureKind::Unavailable,
                format!("{server} could not answer: {other}"),
            ),
            true,
        ),
    }
}

/// A connection that could not be opened.
pub(crate) fn of_initialize(
    server: &str,
    error: &ClientInitializeError,
    answers: Answers,
) -> Failure {
    if let Some(transport) = transport_error(error) {
        return of_transport(server, transport, answers);
    }
    match error {
        ClientInitializeError::NoCompatibleProtocolVersion {
            server_supported, ..
        } => Failure::permanent(
            ActionFailureKind::Unavailable,
            format!(
                "{server} speaks no MCP version Rewind supports (it supports {})",
                server_supported
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None,
        ),
        other => Failure::transient(
            ActionFailureKind::Unavailable,
            match answers {
                Answers::Quoted => format!("{server} could not be reached: {}", chain(other)),
                Answers::Withheld => {
                    format!("{server} did not answer as an MCP server; {WITHHELD}")
                }
            },
        ),
    }
}

/// The transport's error that stopped the initialization: a field of the variant, not its
/// source. After a failed discovery and a failed fallback, the fallback's, else the discovery's.
fn transport_error(error: &ClientInitializeError) -> Option<&DynamicTransportError> {
    match error {
        ClientInitializeError::TransportError { error, .. } => Some(error),
        ClientInitializeError::LegacyFallbackFailed { discover, fallback } => {
            transport_error(fallback).or_else(|| transport_error(discover))
        }
        _ => None,
    }
}

/// A failure of the transport, sorted by the HTTP status it carries, if any: 401 parks the
/// call, 408, 429 and 5xx are transient, another 4xx will not change by retrying (a wrong URL,
/// a forbidden request); without a status, the server could not be reached. An address the
/// egress checks refuse, or a redirect the client does not follow, fails for good.
fn of_transport(server: &str, error: &(dyn Error + 'static), answers: Answers) -> Failure {
    if let Some(reason) = refusal(error) {
        return Failure::permanent(
            ActionFailureKind::Unavailable,
            format!("{server} cannot be reached: {reason}"),
            None,
        );
    }
    if let Some(unsigned) = signing_error(error) {
        return Failure::permanent(
            ActionFailureKind::Unavailable,
            format!("{server}: {unsigned}"),
            None,
        );
    }
    let status = http_status(error);
    // What follows the status, or the place of one: the error's text, or for an internal
    // server, only the client's own error.
    let detail = match (answers, client_error(error)) {
        (Answers::Quoted, _) => format!(": {}", chain(error)),
        (Answers::Withheld, Some(client)) if status.is_none() => format!(": {}", chain(client)),
        (Answers::Withheld, _) => format!("; {WITHHELD}"),
    };
    match status {
        Some((401, challenge)) => {
            let challenge = match answers {
                Answers::Quoted => challenge,
                Answers::Withheld => "authorization required".to_owned(),
            };
            Failure::Unauthorized(unauthorized_message(server, &challenge))
        }
        Some((status, _)) if (400..500).contains(&status) && !matches!(status, 408 | 429) => {
            Failure::permanent(
                ActionFailureKind::Unavailable,
                format!("{server} refused the request (HTTP {status}){detail}"),
                None,
            )
        }
        Some((status, _)) if answers == Answers::Withheld => Failure::transient(
            ActionFailureKind::Unavailable,
            format!("{server} could not be reached (HTTP {status}){detail}"),
        ),
        _ => Failure::transient(
            ActionFailureKind::Unavailable,
            format!("{server} could not be reached{detail}"),
        ),
    }
}

/// The client's own error somewhere in the error's sources: what Rewind saw (no connection, a
/// timeout, a refusal), never what the server answered. rmcp keeps it in a variant that is not
/// a source.
fn client_error<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a reqwest::Error> {
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(client) = current.downcast_ref::<reqwest::Error>() {
            return Some(client);
        }
        if let Some(StreamableHttpError::Client(client)) =
            current.downcast_ref::<StreamableHttpError<reqwest::Error>>()
        {
            return Some(client);
        }
        source = current.source();
    }
    None
}

/// Why the client refused to go where the request led: an address the egress checks refuse, or
/// a redirect it does not follow.
fn refusal(error: &(dyn Error + 'static)) -> Option<String> {
    Egress::refusal_of(error)
        .or_else(|| client_error(error).and_then(|client| Egress::refusal_of(client)))
}

/// Rewind could not sign a request of a signed session: its key is unusable, which retrying
/// does not change.
fn signing_error(error: &(dyn Error + 'static)) -> Option<SigningError> {
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(signing) = current.downcast_ref::<SigningError>() {
            return Some(signing.clone());
        }
        if let Some(StreamableHttpError::Io(io)) =
            current.downcast_ref::<StreamableHttpError<reqwest::Error>>()
            && let Some(signing) = io.get_ref().and_then(|inner| inner.downcast_ref())
        {
            return Some(SigningError::clone(signing));
        }
        source = current.source();
    }
    None
}

/// A stdio server that could not be started. A missing or forbidden program will not appear
/// by retrying.
pub(crate) fn of_spawn(server: &str, error: &std::io::Error) -> Failure {
    let message = format!("the MCP server {server} could not be started: {error}");
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
            Failure::permanent(ActionFailureKind::Unavailable, message, None)
        }
        _ => Failure::transient(ActionFailureKind::Unavailable, message),
    }
}

pub(crate) fn timed_out(server: &str, timeout: Duration) -> Failure {
    Failure::transient(
        ActionFailureKind::Timeout,
        format!("{server} did not answer within {}", human(timeout)),
    )
}

/// `30s`, `5m`, `1500ms`.
pub(crate) fn human(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis >= 60_000 && millis.is_multiple_of(60_000) {
        format!("{}m", millis / 60_000)
    } else if millis.is_multiple_of(1000) {
        format!("{}s", millis / 1000)
    } else {
        format!("{millis}ms")
    }
}

/// The challenge comes from the server: cut like its other messages.
fn unauthorized_message(server: &str, detail: &str) -> String {
    bounded(&format!(
        "{server} refused the credentials (HTTP 401): {detail}"
    ))
}

/// The HTTP status somewhere in the error's sources, with its detail, if there is one.
fn http_status(error: &(dyn Error + 'static)) -> Option<(u16, String)> {
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(required) = current.downcast_ref::<AuthRequiredError>() {
            return Some((401, challenge(&required.www_authenticate_header)));
        }
        if current.is::<InsufficientScopeError>() {
            return Some((403, "insufficient scope".to_owned()));
        }
        if let Some(status) = current.downcast_ref::<HttpStatusError>() {
            return Some((status.status, format!("HTTP {}", status.status)));
        }
        if let Some(http) = current.downcast_ref::<StreamableHttpError<reqwest::Error>>() {
            match http {
                StreamableHttpError::AuthRequired(required) => {
                    return Some((401, challenge(&required.www_authenticate_header)));
                }
                StreamableHttpError::InsufficientScope(_) => {
                    return Some((403, "insufficient scope".to_owned()));
                }
                StreamableHttpError::HttpStatus(status) => {
                    return Some((status.status, format!("HTTP {}", status.status)));
                }
                // A status without a JSON-RPC body keeps its code in the message.
                StreamableHttpError::UnexpectedServerResponse(message) => {
                    let message: &str = message;
                    if let Some(status) = message
                        .strip_prefix("HTTP ")
                        .and_then(|rest| rest.get(..3))
                        .and_then(|code| code.parse::<u16>().ok())
                    {
                        return Some((status, message.to_owned()));
                    }
                }
                _ => {}
            }
        }
        source = current.source();
    }
    None
}

fn challenge(header: &str) -> String {
    format!("authorization required: {header}")
}

/// The error and its sources, as one line.
pub(crate) fn chain(error: &(dyn Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(current) = source {
        let text = current.to_string();
        if !parts.iter().any(|part| part.contains(&text)) {
            parts.push(text);
        }
        source = current.source();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;
    use std::borrow::Cow;

    use rmcp::model::ErrorData;

    use super::*;

    fn transport(error: StreamableHttpError<reqwest::Error>) -> ServiceError {
        ServiceError::TransportSend(DynamicTransportError::from_parts(
            "streamable-http",
            TypeId::of::<()>(),
            Box::new(error),
        ))
    }

    #[test]
    fn a_401_parks_the_call_with_or_without_a_challenge() {
        for error in [
            StreamableHttpError::AuthRequired(AuthRequiredError::new("Bearer realm=\"x\"".into())),
            StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                "HTTP 401 Unauthorized: token expired".into(),
            )),
        ] {
            let (failure, broken) = of_service("github", &transport(error), Answers::Quoted);
            assert!(broken);
            let Failure::Unauthorized(message) = failure else {
                panic!("a 401 parks the call: {failure:?}");
            };
            assert!(message.starts_with("github refused the credentials (HTTP 401)"));
        }
    }

    #[test]
    fn other_client_errors_are_not_retried() {
        for message in ["HTTP 403 Forbidden: no", "HTTP 404 Not Found: wrong path"] {
            let (failure, _) = of_service(
                "github",
                &transport(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                    message.into(),
                ))),
                Answers::Quoted,
            );
            assert!(
                matches!(failure, Failure::Permanent(ref f) if f.kind == ActionFailureKind::Unavailable),
                "{failure:?}"
            );
        }
        let (failure, _) = of_service(
            "github",
            &transport(StreamableHttpError::HttpStatus(HttpStatusError::new(
                429, None,
            ))),
            Answers::Quoted,
        );
        assert!(matches!(failure, Failure::Transient(_)));
    }

    #[test]
    fn overloaded_servers_and_internal_errors_are_transient() {
        let (failure, broken) = of_service(
            "github",
            &transport(StreamableHttpError::HttpStatus(HttpStatusError::new(
                503, None,
            ))),
            Answers::Quoted,
        );
        assert!(broken);
        assert!(
            matches!(failure, Failure::Transient(ref f) if f.kind == ActionFailureKind::Unavailable)
        );

        let (failure, broken) = of_service(
            "github",
            &ServiceError::McpError(ErrorData::internal_error("database down", None)),
            Answers::Quoted,
        );
        assert!(!broken);
        assert!(matches!(failure, Failure::Transient(_)));
    }

    #[test]
    fn other_protocol_errors_fail_for_good() {
        let (failure, _) = of_service(
            "github",
            &ServiceError::McpError(ErrorData::invalid_params(
                "repo is required",
                Some(serde_json::json!({ "field": "repo" })),
            )),
            Answers::Quoted,
        );
        assert_eq!(
            failure,
            Failure::permanent(
                ActionFailureKind::Error,
                "github answered with an error: repo is required",
                Some(serde_json::json!({ "field": "repo" }))
            )
        );
        assert!(matches!(
            of_spawn("local", &std::io::Error::from(std::io::ErrorKind::NotFound)),
            Failure::Permanent(_)
        ));
    }

    #[test]
    fn an_internal_server_s_answers_are_not_quoted() {
        let body = "<html>internal admin console: secret=hunter2</html>";
        let unexpected = || {
            transport(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP 404 Not Found: {body}"),
            )))
        };
        let (quoted, _) = of_service("crm", &unexpected(), Answers::Quoted);
        assert!(format!("{quoted:?}").contains("hunter2"), "{quoted:?}");
        let (withheld, _) = of_service("crm", &unexpected(), Answers::Withheld);
        assert_eq!(
            withheld,
            Failure::permanent(
                ActionFailureKind::Unavailable,
                "crm refused the request (HTTP 404); the answer of an internal server is not \
                 quoted",
                None
            )
        );
        let (withheld, _) = of_service(
            "crm",
            &transport(StreamableHttpError::HttpStatus(HttpStatusError::new(
                503, None,
            ))),
            Answers::Withheld,
        );
        assert_eq!(
            withheld,
            Failure::transient(
                ActionFailureKind::Unavailable,
                "crm could not be reached (HTTP 503); the answer of an internal server is not \
                 quoted"
            )
        );
        // A body that is not JSON-RPC, after a 200.
        let (withheld, _) = of_service(
            "crm",
            &transport(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("could not parse JSON response as ServerJsonRpcMessage: {body}"),
            ))),
            Answers::Withheld,
        );
        assert!(!format!("{withheld:?}").contains("hunter2"), "{withheld:?}");
        // A challenge is the server's text too.
        let (withheld, _) = of_service(
            "crm",
            &transport(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                body.into(),
            ))),
            Answers::Withheld,
        );
        assert_eq!(
            withheld,
            Failure::Unauthorized(
                "crm refused the credentials (HTTP 401): authorization required".into()
            )
        );
        // A JSON-RPC error is what an MCP server says to its clients.
        let (failure, _) = of_service(
            "crm",
            &ServiceError::McpError(ErrorData::invalid_params("repo is required", None)),
            Answers::Withheld,
        );
        assert!(format!("{failure:?}").contains("repo is required"));
    }

    #[test]
    fn long_messages_are_cut() {
        let message = bounded(&"x".repeat(5000));
        assert_eq!(message.chars().count(), MESSAGE + 1);
        assert!(message.ends_with('…'));
        // A 401's challenge comes from the server too.
        let (failure, _) = of_service(
            "github",
            &transport(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                "x".repeat(5000),
            ))),
            Answers::Quoted,
        );
        let Failure::Unauthorized(message) = failure else {
            panic!("a 401 parks the call: {failure:?}");
        };
        assert_eq!(message.chars().count(), MESSAGE + 1);
    }
}
