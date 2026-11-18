//! The stored form of the Rewind journal, versioned (`archi/07-code-architecture.md`,
//! principle 4).
//!
//! Runs stay paused for days across deploys, so what is stored must stay readable while the
//! domain types of `rewind-core` change. This crate owns that representation: domain events are
//! converted to versioned DTOs at the edge, and a stored v1 journal must always decode.
//! A future v2 gets its own module and an explicit migration from v1.
//!
//! Six documents are defined (`archi/02-journal.md`, "Stored format v1"):
//!
//! - one **event**, as stored in a row (`history.event`, `inbox.event`):
//!   `{"v":1,"type":"schedule_tool_call","seq":1,…}`;
//! - a **journal document** (exports, the archive of a finished run, the epoch corpus):
//!   `{"v":1,"run":"…","script":"…","toolset":{…},"entries":[{"index":0,"recorded_at":…,
//!   "event":{"type":"run_started",…}},…]}`: the run's id, its TypeScript source, its pinned
//!   toolset document and its entries, written with one entry per line so that diffs stay
//!   readable. It replays on its own;
//! - a **cassette** (mocks recorded from a run, replayed by `rewind test`):
//!   `{"v":1,"matching":"sequential","calls":[…],"signals":[…]}`, one call or signal per line;
//! - a **pinned toolset** (the `toolsets` table): `{"servers":[…],"tools":[…],"v":1}`, with
//!   `"workflows":[…]` when it pins workflows, in its canonical form, so that its bytes hash to
//!   the toolset's digest;
//! - a **workflow version** (the `workflows` table): `{"name":…,"schemas":{…},"script":…,
//!   "toolset":…,"v":1}`, with its suite (`parent`, `tests`, `retired`, `updated`) when it has
//!   one and `"labels":{…}` when it was saved under a grant limited to labels, in its canonical
//!   form, so that its bytes hash to the version;
//! - a **test** (a blob of the `BlobStore`): `{"name":…,"world":{…},"expect":{…},"v":1}`, in
//!   its canonical form, so that its bytes hash to the test's digest.
//!
//! In events, payloads are written `{"value": …}` or `{"digest": "<sha-256 hex>"}`; in
//! cassettes, as plain values. Seeds and digests are lowercase hex, times and durations are
//! milliseconds. A reader refuses an unknown version, type or field.

#![forbid(unsafe_code)]

mod v1;

use rewind_core::{
    Cassette, Digest, Event, JournalDocument, RunId, Script, Test, Toolset, Workflow,
    canonical_json,
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
    #[error("invalid journal document: {0}")]
    InvalidDocument(String),
    #[error("invalid journal entry at position {position}: {message}")]
    InvalidEntry { position: usize, message: String },
    #[error("invalid cassette: {0}")]
    InvalidCassette(String),
    #[error("invalid pinned toolset: {0}")]
    InvalidToolset(String),
    #[error("invalid workflow version: {0}")]
    InvalidWorkflow(String),
    #[error("invalid test: {0}")]
    InvalidTest(String),
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

/// A journal document: the version, the run, its script and its toolset, then one entry per
/// line.
pub fn encode_journal(journal: &JournalDocument) -> String {
    let mut out = format!(
        "{{\"v\":{VERSION},\"run\":{},\"script\":{},\"toolset\":{},\"entries\":[",
        to_json(&journal.run.to_string()),
        to_json(&journal.script.source()),
        encode_toolset(&journal.toolset),
    );
    for (i, entry) in journal.entries.iter().enumerate() {
        out.push_str(if i == 0 { "\n" } else { ",\n" });
        out.push_str(&to_json(&v1::EntryV1::from(entry)));
    }
    out.push_str("\n]}\n");
    out
}

/// Decodes a journal document written by [`encode_journal`], of any supported version.
///
/// Only the encoding is checked here. Whether the entries form a valid history, and whether
/// the script and the toolset are those of its `run_started`, is the domain's business
/// ([`JournalDocument::check`]).
pub fn decode_journal(json: &str) -> Result<JournalDocument, FormatError> {
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
            let script = match object.remove("script") {
                Some(Value::String(source)) => Script::new(source),
                Some(_) => return Err(invalid("\"script\" is not a string")),
                None => return Err(invalid("missing \"script\"")),
            };
            let toolset = match object.remove("toolset") {
                Some(Value::Object(mut toolset)) => match take_version(&mut toolset)? {
                    Version::V1 => v1::decode_toolset(Value::Object(toolset))
                        .map_err(FormatError::InvalidToolset)?,
                },
                Some(_) => return Err(invalid("\"toolset\" is not an object")),
                None => return Err(invalid("missing \"toolset\"")),
            };
            let entries = match object.remove("entries") {
                Some(Value::Array(entries)) => entries,
                Some(_) => return Err(invalid("\"entries\" is not an array")),
                None => return Err(invalid("missing \"entries\"")),
            };
            if let Some(unknown) = object.keys().next() {
                return Err(FormatError::InvalidDocument(format!(
                    "unknown field {unknown:?}"
                )));
            }
            let entries = entries
                .into_iter()
                .enumerate()
                .map(|(position, entry)| {
                    v1::decode_entry(entry)
                        .map_err(|message| FormatError::InvalidEntry { position, message })
                })
                .collect::<Result<_, _>>()?;
            Ok(JournalDocument {
                run,
                script,
                toolset,
                entries,
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

/// The pinned toolset document, in its canonical form (RFC 8785): its bytes hash to
/// [`Toolset::digest`].
pub fn encode_toolset(toolset: &Toolset) -> String {
    canonical_json(&v1::toolset_document(toolset))
}

/// Decodes a pinned toolset document, of any supported version. Servers and tools may come in
/// any order, and the toolset's invariants are checked ([`Toolset::new`]).
pub fn decode_toolset(json: &str) -> Result<Toolset, FormatError> {
    let mut object = parse_object(json)?;
    match take_version(&mut object)? {
        Version::V1 => {
            v1::decode_toolset(Value::Object(object)).map_err(FormatError::InvalidToolset)
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

/// Decodes a test document, of any supported version. A tool's single rule may be given without
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
