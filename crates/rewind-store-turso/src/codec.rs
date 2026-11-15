//! How values the store keeps in columns are written: events in the stored format v1
//! (`rewind-format`), JSON values in their canonical form, and the store's own small encodings for
//! enumerations, routes, label maps and shapes.

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{
    Builtin, Event, Field, Interruption, JsonValue, Lifecycle, Payload, RunMode, RunStatus, Shape,
    ToolFailureKind, ToolRoute, WorkflowOp, canonical_json,
};
use rewind_engine::ports::{
    ParkKind, QuarantineKind, TaskPhase, Trigger, WaitKind, WebhookEventType,
};
use serde_json::{Map, Value, json};

use crate::Error;
use crate::sql::parse_digest;

fn corrupt(what: &str, found: &str) -> Error {
    Error::Corrupt(format!("unknown {what} {found:?}"))
}

// -------------------------------------------------------------------------------------------
// Enumerations

pub(crate) fn parse_status(name: &str) -> Result<RunStatus, Error> {
    Ok(match name {
        "pending" => RunStatus::Pending,
        "active" => RunStatus::Active,
        "completed" => RunStatus::Completed,
        "failed" => RunStatus::Failed,
        "cancelled" => RunStatus::Cancelled,
        "aborted" => RunStatus::Aborted,
        other => return Err(corrupt("run status", other)),
    })
}

pub(crate) fn quarantine_kind(kind: QuarantineKind) -> &'static str {
    match kind {
        QuarantineKind::NonDeterminism => "non_determinism",
        QuarantineKind::Failures => "failures",
    }
}

pub(crate) fn parse_quarantine_kind(name: &str) -> Result<QuarantineKind, Error> {
    Ok(match name {
        "non_determinism" => QuarantineKind::NonDeterminism,
        "failures" => QuarantineKind::Failures,
        other => return Err(corrupt("quarantine kind", other)),
    })
}

pub(crate) fn park_kind(kind: ParkKind) -> &'static str {
    match kind {
        ParkKind::SchemaChanged => "schema_changed",
        ParkKind::CredentialsExpired => "credentials_expired",
    }
}

pub(crate) fn parse_park_kind(name: &str) -> Result<ParkKind, Error> {
    Ok(match name {
        "schema_changed" => ParkKind::SchemaChanged,
        "credentials_expired" => ParkKind::CredentialsExpired,
        other => return Err(corrupt("park kind", other)),
    })
}

pub(crate) fn parse_interruption(name: &str) -> Result<Interruption, Error> {
    Ok(match name {
        "retry" => Interruption::Retry,
        "at_most_once" => Interruption::AtMostOnce,
        other => return Err(corrupt("interruption policy", other)),
    })
}

pub(crate) fn parse_failure_kind(name: &str) -> Result<ToolFailureKind, Error> {
    Ok(match name {
        "error" => ToolFailureKind::Error,
        "unavailable" => ToolFailureKind::Unavailable,
        "timeout" => ToolFailureKind::Timeout,
        "outcome_unknown" => ToolFailureKind::OutcomeUnknown,
        other => return Err(corrupt("tool failure kind", other)),
    })
}

pub(crate) fn run_mode(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Live => "live",
        RunMode::Mock => "mock",
    }
}

pub(crate) fn parse_run_mode(name: &str) -> Result<RunMode, Error> {
    Ok(match name {
        "live" => RunMode::Live,
        "mock" => RunMode::Mock,
        other => return Err(corrupt("run mode", other)),
    })
}

/// The names of `WaitingFor` for the same kinds: a query compares them.
pub(crate) fn wait_kind(kind: WaitKind) -> &'static str {
    match kind {
        WaitKind::Signal => "signal",
        WaitKind::Timer => "timer",
        WaitKind::Call => "call",
        WaitKind::Host => "host",
    }
}

pub(crate) fn parse_wait_kind(name: &str) -> Result<WaitKind, Error> {
    Ok(match name {
        "signal" => WaitKind::Signal,
        "timer" => WaitKind::Timer,
        "call" => WaitKind::Call,
        "host" => WaitKind::Host,
        other => return Err(corrupt("wait kind", other)),
    })
}

/// The names of `WaitingFor` for approval, parked and deferred: a query compares them.
pub(crate) fn task_phase(phase: TaskPhase) -> &'static str {
    match phase {
        TaskPhase::Approval => "approval",
        TaskPhase::Ready => "ready",
        TaskPhase::Claimed => "claimed",
        TaskPhase::Deferred => "deferred",
        TaskPhase::Parked => "parked",
    }
}

pub(crate) fn parse_task_phase(name: &str) -> Result<TaskPhase, Error> {
    Ok(match name {
        "approval" => TaskPhase::Approval,
        "ready" => TaskPhase::Ready,
        "claimed" => TaskPhase::Claimed,
        "deferred" => TaskPhase::Deferred,
        "parked" => TaskPhase::Parked,
        other => return Err(corrupt("task phase", other)),
    })
}

// -------------------------------------------------------------------------------------------
// JSON values and payloads

/// A JSON value in its canonical form (RFC 8785): the bytes its digest is computed on.
pub(crate) fn encode_value(value: &JsonValue) -> String {
    canonical_json(value)
}

pub(crate) fn decode_value(json: &str) -> Result<JsonValue, Error> {
    serde_json::from_str(json).map_err(|err| Error::Corrupt(format!("stored JSON value: {err}")))
}

/// A payload as two columns: its digest, and its value when the payload holds it.
pub(crate) fn payload_columns(payload: &Payload) -> (String, Option<String>) {
    (payload.digest().to_hex(), payload.value().map(encode_value))
}

pub(crate) fn decode_payload(digest: &str, value: Option<&str>) -> Result<Payload, Error> {
    match value {
        Some(json) => decode_value(json).map(Payload::Value),
        None => parse_digest(digest).map(Payload::Digest),
    }
}

/// A string map, as a JSON object.
pub(crate) fn encode_labels(labels: &BTreeMap<String, String>) -> String {
    let object: Map<String, Value> = labels
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    canonical_json(&Value::Object(object))
}

pub(crate) fn decode_labels(json: &str) -> Result<BTreeMap<String, String>, Error> {
    let Value::Object(object) = decode_value(json)? else {
        return Err(Error::Corrupt(format!("labels {json:?}")));
    };
    object
        .into_iter()
        .map(|(key, value)| match value {
            Value::String(value) => Ok((key, value)),
            other => Err(Error::Corrupt(format!("label {key:?} is {other}"))),
        })
        .collect()
}

// -------------------------------------------------------------------------------------------
// Events

pub(crate) fn decode_event(json: &str) -> Result<Event, Error> {
    rewind_format::decode_event(json).map_err(|err| Error::Corrupt(format!("stored event: {err}")))
}

/// An inbox item holds its trigger as an event: `run_started`, or a completion.
pub(crate) fn encode_trigger(trigger: &Trigger) -> String {
    let event = match trigger {
        Trigger::RunStarted(started) => Event::Lifecycle(Lifecycle::RunStarted(started.clone())),
        Trigger::Completion(completion) => Event::Completion(completion.clone()),
    };
    rewind_format::encode_event(&event)
}

pub(crate) fn decode_trigger(json: &str) -> Result<Trigger, Error> {
    match decode_event(json)? {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Ok(Trigger::RunStarted(started)),
        Event::Completion(completion) => Ok(Trigger::Completion(completion)),
        other => Err(Error::Corrupt(format!(
            "an inbox item holds a {} event",
            other.name()
        ))),
    }
}

// -------------------------------------------------------------------------------------------
// Tool routes

/// `{"mcp":"github"}`, `{"host":null}`, `{"host":"group"}`, `{"builtin":"webhook"}`,
/// `{"builtin":"http","allowed_hosts":[…]}`,
/// `{"builtin":"workflow","workflow":"triage","op":"start"}`.
pub(crate) fn encode_route(route: &ToolRoute) -> String {
    let value = match route {
        ToolRoute::Mcp { server } => json!({ "mcp": server }),
        ToolRoute::Host { group } => json!({ "host": group }),
        ToolRoute::Builtin(Builtin::Webhook) => json!({ "builtin": "webhook" }),
        ToolRoute::Builtin(Builtin::Http { allowed_hosts }) => {
            json!({ "builtin": "http", "allowed_hosts": allowed_hosts })
        }
        ToolRoute::Builtin(Builtin::Workflow { workflow, op }) => {
            json!({ "builtin": "workflow", "workflow": workflow, "op": op.as_str() })
        }
    };
    canonical_json(&value)
}

pub(crate) fn decode_route(json: &str) -> Result<ToolRoute, Error> {
    let invalid = || Error::Corrupt(format!("tool route {json:?}"));
    let value = decode_value(json)?;
    if let Some(server) = value.get("mcp") {
        let server = server.as_str().ok_or_else(invalid)?;
        return Ok(ToolRoute::Mcp {
            server: server.to_owned(),
        });
    }
    if let Some(group) = value.get("host") {
        let group = match group {
            Value::Null => None,
            Value::String(group) => Some(group.clone()),
            _ => return Err(invalid()),
        };
        return Ok(ToolRoute::Host { group });
    }
    match value.get("builtin").and_then(Value::as_str) {
        Some("webhook") => Ok(ToolRoute::Builtin(Builtin::Webhook)),
        Some("http") => {
            let hosts = value
                .get("allowed_hosts")
                .and_then(Value::as_array)
                .ok_or_else(invalid)?;
            let allowed_hosts = hosts
                .iter()
                .map(|host| host.as_str().map(str::to_owned).ok_or_else(invalid))
                .collect::<Result<_, _>>()?;
            Ok(ToolRoute::Builtin(Builtin::Http { allowed_hosts }))
        }
        Some("workflow") => {
            let workflow = value
                .get("workflow")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            let op = match value.get("op").and_then(Value::as_str) {
                Some("start") => WorkflowOp::Start,
                Some("signal") => WorkflowOp::Signal,
                _ => return Err(invalid()),
            };
            Ok(ToolRoute::Builtin(Builtin::Workflow {
                workflow: workflow.to_owned(),
                op,
            }))
        }
        _ => Err(invalid()),
    }
}

/// The MCP server of a route, which the claim limits per worker.
pub(crate) fn mcp_server(route: &ToolRoute) -> Option<&str> {
    match route {
        ToolRoute::Mcp { server } => Some(server),
        _ => None,
    }
}

// -------------------------------------------------------------------------------------------
// Webhook subscriptions

pub(crate) fn encode_event_types(events: &BTreeSet<WebhookEventType>) -> String {
    let names: Vec<Value> = events
        .iter()
        .map(|event| Value::String(event.as_str().to_owned()))
        .collect();
    canonical_json(&Value::Array(names))
}

pub(crate) fn decode_event_types(json: &str) -> Result<BTreeSet<WebhookEventType>, Error> {
    let Value::Array(names) = decode_value(json)? else {
        return Err(Error::Corrupt(format!("webhook events {json:?}")));
    };
    names
        .iter()
        .map(|name| {
            let name = name.as_str().unwrap_or_default();
            WebhookEventType::ALL
                .into_iter()
                .find(|event| event.as_str() == name)
                .ok_or_else(|| corrupt("webhook event", name))
        })
        .collect()
}

// -------------------------------------------------------------------------------------------
// Observed shapes

/// `"unknown"`, `"null"`, `"boolean"`, `"number"`, `"string"`, `{"array":…}`,
/// `{"object":{"key":{"shape":…,"optional":false}}}`, `{"union":[…]}`.
pub(crate) fn encode_shape(shape: &Shape) -> String {
    canonical_json(&shape_value(shape))
}

fn shape_value(shape: &Shape) -> Value {
    match shape {
        Shape::Unknown => json!("unknown"),
        Shape::Null => json!("null"),
        Shape::Boolean => json!("boolean"),
        Shape::Number => json!("number"),
        Shape::String => json!("string"),
        Shape::Array(items) => json!({ "array": shape_value(items) }),
        Shape::Object(fields) => {
            let fields: Map<String, Value> = fields
                .iter()
                .map(|(key, field)| {
                    let value = json!({
                        "shape": shape_value(&field.shape),
                        "optional": field.optional,
                    });
                    (key.clone(), value)
                })
                .collect();
            json!({ "object": fields })
        }
        Shape::Union(shapes) => {
            json!({ "union": shapes.iter().map(shape_value).collect::<Vec<_>>() })
        }
    }
}

pub(crate) fn decode_shape(json: &str) -> Result<Shape, Error> {
    shape_of(&decode_value(json)?).ok_or_else(|| Error::Corrupt(format!("shape {json:?}")))
}

fn shape_of(value: &Value) -> Option<Shape> {
    match value {
        Value::String(name) => match name.as_str() {
            "unknown" => Some(Shape::Unknown),
            "null" => Some(Shape::Null),
            "boolean" => Some(Shape::Boolean),
            "number" => Some(Shape::Number),
            "string" => Some(Shape::String),
            _ => None,
        },
        Value::Object(object) if object.len() == 1 => {
            let (tag, inner) = object.iter().next()?;
            match tag.as_str() {
                "array" => Some(Shape::Array(Box::new(shape_of(inner)?))),
                "object" => {
                    let fields = inner
                        .as_object()?
                        .iter()
                        .map(|(key, field)| {
                            let shape = shape_of(field.get("shape")?)?;
                            let optional = field.get("optional")?.as_bool()?;
                            Some((key.clone(), Field { shape, optional }))
                        })
                        .collect::<Option<_>>()?;
                    Some(Shape::Object(fields))
                }
                "union" => {
                    let shapes = inner
                        .as_array()?
                        .iter()
                        .map(shape_of)
                        .collect::<Option<_>>()?;
                    Some(Shape::Union(shapes))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rewind_engine::ports::WaitingFor;

    #[test]
    fn shapes_round_trip() {
        let shape = Shape::of(&json!({
            "id": 1,
            "tags": ["a", null],
            "nested": { "flag": true },
            "empty": [],
        }))
        .merge(Shape::of(&json!({ "id": "x", "extra": 2.5 })));
        assert_eq!(decode_shape(&encode_shape(&shape)), Ok(shape));
        assert!(decode_shape("{\"tuple\":[]}").is_err());
    }

    #[test]
    fn routes_round_trip() {
        for route in [
            ToolRoute::Mcp {
                server: "github".into(),
            },
            ToolRoute::Host { group: None },
            ToolRoute::Host {
                group: Some("ui".into()),
            },
            ToolRoute::Builtin(Builtin::Webhook),
            ToolRoute::Builtin(Builtin::Http {
                allowed_hosts: vec!["api.example.com".into()],
            }),
            ToolRoute::Builtin(Builtin::Workflow {
                workflow: "triage".into(),
                op: WorkflowOp::Start,
            }),
            ToolRoute::Builtin(Builtin::Workflow {
                workflow: "triage".into(),
                op: WorkflowOp::Signal,
            }),
        ] {
            assert_eq!(decode_route(&encode_route(&route)), Ok(route));
        }
    }

    #[test]
    fn payloads_keep_their_value_or_their_digest() {
        let value = Payload::Value(json!({ "b": 1, "a": [1.5, "x"] }));
        let (digest, held) = payload_columns(&value);
        assert_eq!(held.as_deref(), Some(r#"{"a":[1.5,"x"],"b":1}"#));
        assert_eq!(decode_payload(&digest, held.as_deref()), Ok(value.clone()));
        let digest_only = Payload::Digest(value.digest());
        let (digest, held) = payload_columns(&digest_only);
        assert_eq!(held, None);
        assert_eq!(decode_payload(&digest, None), Ok(digest_only));
    }

    #[test]
    fn waits_and_task_phases_are_named_as_queries_name_them() {
        for (kind, waiting) in [
            (WaitKind::Signal, WaitingFor::Signal),
            (WaitKind::Timer, WaitingFor::Timer),
            (WaitKind::Call, WaitingFor::Call),
            (WaitKind::Host, WaitingFor::Host),
        ] {
            assert_eq!(wait_kind(kind), waiting.as_str());
            assert_eq!(parse_wait_kind(wait_kind(kind)), Ok(kind));
        }
        for (phase, waiting) in [
            (TaskPhase::Approval, Some(WaitingFor::Approval)),
            (TaskPhase::Parked, Some(WaitingFor::Parked)),
            (TaskPhase::Deferred, Some(WaitingFor::Deferred)),
            (TaskPhase::Ready, None),
            (TaskPhase::Claimed, None),
        ] {
            if let Some(waiting) = waiting {
                assert_eq!(task_phase(phase), waiting.as_str());
            }
            assert_eq!(parse_task_phase(task_phase(phase)), Ok(phase));
        }
        for mode in [RunMode::Live, RunMode::Mock] {
            assert_eq!(parse_run_mode(run_mode(mode)), Ok(mode));
        }
    }

    #[test]
    fn labels_round_trip() {
        let labels = BTreeMap::from([
            ("agent".to_owned(), "triage".to_owned()),
            ("a,b".to_owned(), "c\"d".to_owned()),
        ]);
        assert_eq!(decode_labels(&encode_labels(&labels)), Ok(labels));
        assert!(decode_labels("{\"n\":1}").is_err());
    }
}
