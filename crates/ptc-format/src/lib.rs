//! The stored form of the ai-ptc journal, versioned (`archi/07-code-architecture.md`,
//! principle 4).
//!
//! Runs stay paused for days across deploys, so what is stored must stay readable while the
//! domain types of `ptc-core` change. This crate owns that representation: domain events are
//! converted to versioned DTOs at the edge, and a stored v1 journal must always decode.
//! A future v2 gets its own module and an explicit migration from v1.
//!
//! Two shapes are defined:
//!
//! - one **event**, as stored in a row (`history.event`, `inbox.event`):
//!   `{"v":1,"type":"schedule_tool_call","seq":1,…}`;
//! - a **journal document** (exports, the epoch corpus, cassettes):
//!   `{"v":1,"entries":[{"index":0,"recorded_at":…,"event":{"type":"run_started",…}},…]}`,
//!   written with one entry per line so that diffs stay readable.
//!
//! Payloads are written `{"value": …}` or `{"digest": "<sha-256 hex>"}`, seeds and digests as
//! lowercase hex, times as milliseconds since the Unix epoch.

#![forbid(unsafe_code)]

mod v1;

use ptc_core::{Event, HistoryEntry};
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

/// A journal document: the version, then one entry per line.
pub fn encode_history(history: &[HistoryEntry]) -> String {
    let mut out = format!("{{\"v\":{VERSION},\"entries\":[");
    for (i, entry) in history.iter().enumerate() {
        out.push_str(if i == 0 { "\n" } else { ",\n" });
        out.push_str(&to_json(&v1::EntryV1::from(entry)));
    }
    out.push_str("\n]}\n");
    out
}

/// Decodes a journal document written by [`encode_history`], of any supported version.
///
/// Only the encoding is checked here. Whether the entries form a valid history is the
/// domain's business (`ptc_core::validate_history`).
pub fn decode_history(json: &str) -> Result<Vec<HistoryEntry>, FormatError> {
    let mut object = parse_object(json)?;
    match take_version(&mut object)? {
        Version::V1 => {
            let entries = match object.remove("entries") {
                Some(Value::Array(entries)) => entries,
                Some(_) => {
                    return Err(FormatError::InvalidDocument(
                        "\"entries\" is not an array".into(),
                    ));
                }
                None => return Err(FormatError::InvalidDocument("missing \"entries\"".into())),
            };
            if let Some(unknown) = object.keys().next() {
                return Err(FormatError::InvalidDocument(format!(
                    "unknown field {unknown:?}"
                )));
            }
            entries
                .into_iter()
                .enumerate()
                .map(|(position, entry)| {
                    v1::decode_entry(entry)
                        .map_err(|message| FormatError::InvalidEntry { position, message })
                })
                .collect()
        }
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
