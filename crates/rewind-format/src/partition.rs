//! The partition records, stored format v1 (`archi/02-history.md`, "Stored format v1";
//! `archi/06-storage-and-scale.md`, "A partition").
//!
//! A partition holds one record per kind of thing it keeps about its runs: [`RunHead`], an
//! [`HistoryEvent`] of an attempt's history (in the stored format of events), an
//! [`InboxItem`], a [`SignalKey`], an action [`Task`], a [`Timer`], a [`PayloadValue`], a
//! [`Watcher`], a [`SignalUrlToken`], a run's [`Summary`] in the index, a [`StartKey`], the hourly
//! [`Counters`] of a namespace, the observed [`Shape`] of an action's results, the [`Gauges`] of a
//! namespace, an [`Outbox`] entry, and the state of an [`Organization`] homed there.
//!
//! [`encode`] writes a record `{"v":1,…}`, its fields in their declared order, optional fields
//! and empty collections left out, payload values in their canonical form (RFC 8785). [`decode`]
//! reads a record of a supported version and refuses an unknown version, field or variant. The
//! keys the records are stored under belong to `rewind-store-slatedb`, and the conversions from
//! and to the engine's types too: this crate knows only `rewind-core`.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::{FormatError, VERSION};

pub use crate::v1::partition::{
    ActionCounts, Admission, Attribute, Callsite, Counters, Ending, FailureCounts, FailureKind,
    ForkSource, Gauges, HistoryEvent, InboxItem, LatestRun, Level, Log, Metadata, Mode,
    Organization, Outbox, ParkKind, PayloadValue, Projection, Quarantine, QuarantineKind,
    QuarantineNote, QuotaAlert, QuotaKind, RunCounts, RunHead, RunWorkflow, ScriptCounts,
    SecretExpiredReason, Shape, SignalKey, SignalUrlToken, Sketch, Span, SpanKind, SpanStatus,
    StartKey, StartedBy, Status, Summary, Task, TaskPhase, TaskState, Threshold, Timer, TimerKind,
    TraceContext, Wait, WaitKind, WaitingFor, Watcher, Webhook, WebhookKind,
};

/// A partition record: what [`encode`] writes and [`decode`] reads.
pub trait Record: Serialize + DeserializeOwned + sealed::Sealed {
    /// The record's name in errors.
    const NAME: &'static str;
}

mod sealed {
    pub trait Sealed {}
}

macro_rules! records {
    ($($record:ident => $name:literal),* $(,)?) => {
        $(
            impl sealed::Sealed for $record {}
            impl Record for $record {
                const NAME: &'static str = $name;
            }
        )*
    };
}

records! {
    RunHead => "run head",
    HistoryEvent => "history event",
    InboxItem => "inbox item",
    SignalKey => "signal key",
    Task => "call task",
    Timer => "timer",
    PayloadValue => "payload",
    Watcher => "watcher",
    SignalUrlToken => "signal URL token",
    Summary => "run summary",
    StartKey => "start key",
    Counters => "counters",
    Shape => "shape",
    Gauges => "gauges",
    Outbox => "outbox entry",
    Organization => "organization state",
}

/// The record as stored: `{"v":1,…}`.
pub fn encode<R: Record>(record: &R) -> Vec<u8> {
    #[derive(Serialize)]
    struct Versioned<'a, R> {
        v: u64,
        #[serde(flatten)]
        record: &'a R,
    }
    serde_json::to_vec(&Versioned { v: VERSION, record })
        .expect("partition records always serialize")
}

/// Decodes a record written by [`encode`], of any supported version.
pub fn decode<R: Record>(bytes: &[u8]) -> Result<R, FormatError> {
    let invalid = |message: String| FormatError::InvalidRecord {
        record: R::NAME,
        message,
    };
    let mut object: Map<String, Value> = match serde_json::from_slice(bytes) {
        Ok(Value::Object(object)) => object,
        Ok(_) => return Err(FormatError::NotAnObject),
        Err(err) => return Err(FormatError::Json(err.to_string())),
    };
    match object.remove("v") {
        None => Err(FormatError::MissingVersion),
        Some(Value::Number(n)) if n.as_u64() == Some(1) => {
            serde_json::from_value(Value::Object(object)).map_err(|err| invalid(err.to_string()))
        }
        Some(other) => Err(FormatError::UnsupportedVersion(other)),
    }
}
