//! The stored form of the Rewind history, versioned (`archi/07-code-architecture.md`,
//! principle 4).
//!
//! Runs stay paused for days across deploys, so what is stored must stay readable while the
//! domain types of `rewind-core` change. This crate owns that representation: domain events are
//! converted to versioned DTOs at the edge, and a stored v1 history must always decode.
//! A future v2 gets its own module and an explicit migration from v1.
//!
//! Seven documents are defined (`archi/02-history.md`, "Stored format v1"):
//!
//! - one **event**, as stored in a row (`history.event`, `inbox.event`):
//!   `{"v":1,"type":"schedule_call","seq":1,…}`;
//! - a **run export** (exports, the archive of a finished run, the epoch corpus):
//!   `{"v":1,"run":"…","code":"…","action_set":{…},"events":[{"index":0,"recorded_at":…,
//!   "event":{"type":"run_started",…}},…]}`: the run's id, its TypeScript source, its pinned
//!   action set document and its events, written with one event per line so that diffs stay
//!   readable. It replays on its own;
//! - a **cassette** (mocks recorded from a run, replayed by `rewind test`):
//!   `{"v":1,"matching":"sequential","calls":[…],"signals":[…]}`, one call or signal per line;
//! - a **pinned action set** (the `action_sets` table): `{"servers":[…],"actions":[…],"v":1}`, with
//!   `"apis":[…]` when it calls APIs, a server's or an API's `"auth":{…}` when its calls carry a
//!   secret or a signature, and `"workflows":[…]` when it pins workflows, in its canonical form,
//!   so that its bytes hash to the action set's digest;
//! - a **workflow version** (the `workflows` table): `{"name":…,"schemas":{…},"code":…,
//!   "action_set":…,"v":1}`, with its suite (`parent`, `tests`, `retired`, `updated`) when it has
//!   one, `"labels":{…}` when it was saved under a grant limited to labels and `"secrets":{…}`
//!   when it attaches secrets, in its canonical form, so that its bytes hash to the version;
//! - a **test** (a blob of the `BlobStore`): `{"name":…,"world":{…},"expect":{…},"v":1}`, in
//!   its canonical form, so that its bytes hash to the test's digest;
//! - the **partition records** ([`partition`]): one record per kind of thing a partition holds
//!   about its runs, `{"v":1,…}`, under keys that belong to `rewind-store-slatedb`.
//!
//! In events, payloads are written `{"value": …}` or `{"digest": "<sha-256 hex>"}`; in
//! cassettes, as plain values. Seeds and digests are lowercase hex, times and durations are
//! milliseconds. A reader refuses an unknown version, type or field.

#![forbid(unsafe_code)]

pub mod partition;
mod v1;

use rewind_core::{
    ActionSet, Cassette, Code, Digest, Event, RunExport, RunId, Test, Workflow, canonical_json,
};
use serde::Serialize;
use serde_json::{Map, Value};

/// The format version this build writes.
pub const VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    #[error("invalid JSON: {0}")]
    Json(String),
    #[error("expected a JSON object")]
    NotAnObject,
    #[error("missing format version (\"v\")")]
    MissingVersion,
    #[error("unsupported format version {0} (this build reads version 1)")]
    UnsupportedVersion(Value),
    #[error("invalid event: {0}")]
    InvalidEvent(String),
    #[error("invalid run export: {0}")]
    InvalidDocument(String),
    #[error("invalid event at position {position}: {message}")]
    InvalidHistoryEvent { position: usize, message: String },
    #[error("invalid cassette: {0}")]
    InvalidCassette(String),
    #[error("invalid pinned action set: {0}")]
    InvalidActionSet(String),
    #[error("invalid workflow version: {0}")]
    InvalidWorkflow(String),
    #[error("invalid test: {0}")]
    InvalidTest(String),
    #[error("invalid partition record ({record}): {message}")]
    InvalidRecord {
        record: &'static str,
        message: String,
    },
}

/// One event, as stored in a row: `{"v":1,"type":…,…}`.
pub fn encode_event(event: &Event) -> String {
    #[derive(Serialize)]
    struct Versioned<'a> {
        v: u64,
        #[serde(flatten)]
        event: &'a v1::EventV1,
    }
    to_json(&Versioned {
        v: VERSION,
        event: &v1::EventV1::from(event),
    })
}

/// Decodes an event written by [`encode_event`], of any supported version.
pub fn decode_event(json: &str) -> Result<Event, FormatError> {
    let mut object = parse_object(json)?;
    match take_version(&mut object)? {
        Version::V1 => v1::decode_event(Value::Object(object)).map_err(FormatError::InvalidEvent),
    }
}

/// A run export: the version, the run, its code and its action set, then one event per
/// line.
pub fn encode_run_export(export: &RunExport) -> String {
    let mut out = format!(
        "{{\"v\":{VERSION},\"run\":{},\"code\":{},\"action_set\":{},\"events\":[",
        to_json(&export.run.to_string()),
        to_json(&export.code.source()),
        encode_action_set(&export.action_set),
    );
    for (i, entry) in export.events.iter().enumerate() {
        out.push_str(if i == 0 { "\n" } else { ",\n" });
        out.push_str(&to_json(&v1::HistoryEventV1::from(entry)));
    }
    out.push_str("\n]}\n");
    out
}

/// Decodes a run export written by [`encode_run_export`], of any supported version.
///
/// Only the encoding is checked here. Whether the events form a valid history, and whether
/// the code and the action set are those of its `run_started`, is the domain's business
/// ([`RunExport::check`]).
pub fn decode_run_export(json: &str) -> Result<RunExport, FormatError> {
    let mut object = parse_object(json)?;
    match take_version(&mut object)? {
        Version::V1 => {
            let invalid = |message: &str| FormatError::InvalidDocument(message.to_owned());
            let run = match object.remove("run") {
                Some(Value::String(run)) => run
                    .parse::<RunId>()
                    .map_err(|err| FormatError::InvalidDocument(format!("\"run\": {err}")))?,
                Some(_) => return Err(invalid("\"run\" is not a string")),
                None => return Err(invalid("missing \"run\"")),
            };
            let code = match object.remove("code") {
                Some(Value::String(source)) => Code::new(source),
                Some(_) => return Err(invalid("\"code\" is not a string")),
                None => return Err(invalid("missing \"code\"")),
            };
            let action_set = match object.remove("action_set") {
                Some(Value::Object(mut action_set)) => match take_version(&mut action_set)? {
                    Version::V1 => v1::decode_action_set(Value::Object(action_set))
                        .map_err(FormatError::InvalidActionSet)?,
                },
                Some(_) => return Err(invalid("\"action_set\" is not an object")),
                None => return Err(invalid("missing \"action_set\"")),
            };
            let events = match object.remove("events") {
                Some(Value::Array(events)) => events,
                Some(_) => return Err(invalid("\"events\" is not an array")),
                None => return Err(invalid("missing \"events\"")),
            };
            if let Some(unknown) = object.keys().next() {
                return Err(FormatError::InvalidDocument(format!(
                    "unknown field {unknown:?}"
                )));
            }
            let events = events
                .into_iter()
                .enumerate()
                .map(|(position, entry)| {
                    v1::decode_history_event(entry)
                        .map_err(|message| FormatError::InvalidHistoryEvent { position, message })
                })
                .collect::<Result<_, _>>()?;
            Ok(RunExport {
                run,
                code,
                action_set,
                events,
            })
        }
    }
}

/// A cassette document: the version and the matching, then one call or signal per line.
pub fn encode_cassette(cassette: &Cassette) -> String {
    v1::encode_cassette(cassette)
}

/// Decodes a cassette written by [`encode_cassette`], or by hand, of any supported version.
pub fn decode_cassette(json: &str) -> Result<Cassette, FormatError> {
    let mut object = parse_object(json)?;
    match take_version(&mut object)? {
        Version::V1 => v1::decode_cassette(object).map_err(FormatError::InvalidCassette),
    }
}

/// The pinned action set document, in its canonical form (RFC 8785): its bytes hash to
/// [`ActionSet::digest`].
pub fn encode_action_set(action_set: &ActionSet) -> String {
    canonical_json(&v1::action_set_document(action_set))
}

/// Decodes a pinned action set document, of any supported version. Servers and actions may come in
/// any order, and the action set's invariants are checked ([`ActionSet::new`]).
pub fn decode_action_set(json: &str) -> Result<ActionSet, FormatError> {
    let mut object = parse_object(json)?;
    match take_version(&mut object)? {
        Version::V1 => {
            v1::decode_action_set(Value::Object(object)).map_err(FormatError::InvalidActionSet)
        }
    }
}

/// The workflow version document, in its canonical form (RFC 8785): its bytes hash to
/// [`Workflow::version`].
pub fn encode_workflow(workflow: &Workflow) -> String {
    canonical_json(&v1::workflow_document(workflow))
}

/// Decodes a workflow version document, of any supported version. Its invariants are checked
/// ([`Workflow::new`]).
pub fn decode_workflow(json: &str) -> Result<Workflow, FormatError> {
    let mut object = parse_object(json)?;
    match take_version(&mut object)? {
        Version::V1 => {
            v1::decode_workflow(Value::Object(object)).map_err(FormatError::InvalidWorkflow)
        }
    }
}

/// The test document, in its canonical form (RFC 8785): its bytes hash to [`test_digest`].
pub fn encode_test(test: &Test) -> String {
    canonical_json(&v1::test_document(test))
}

/// The digest of a test: SHA-256 of its canonical document, the key of its blob.
pub fn test_digest(test: &Test) -> Digest {
    Digest::of(encode_test(test).as_bytes())
}

/// Decodes a test document, of any supported version. An action's single rule may be given without
/// its array, and the test's invariants are checked ([`Test::check`]).
pub fn decode_test(json: &str) -> Result<Test, FormatError> {
    let mut object = parse_object(json)?;
    match take_version(&mut object)? {
        Version::V1 => v1::decode_test(Value::Object(object)).map_err(FormatError::InvalidTest),
    }
}

enum Version {
    V1,
}

fn parse_object(json: &str) -> Result<Map<String, Value>, FormatError> {
    match serde_json::from_str(json) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => Err(FormatError::NotAnObject),
        Err(err) => Err(FormatError::Json(err.to_string())),
    }
}

fn take_version(object: &mut Map<String, Value>) -> Result<Version, FormatError> {
    match object.remove("v") {
        None => Err(FormatError::MissingVersion),
        Some(Value::Number(n)) if n.as_u64() == Some(1) => Ok(Version::V1),
        Some(other) => Err(FormatError::UnsupportedVersion(other)),
    }
}

fn to_json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("format DTOs always serialize")
}
