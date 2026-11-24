//! Types shared by several groups of endpoints.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{ObjectBuilder, OneOfBuilder, Schema, SchemaType, Type};
use utoipa::{PartialSchema, ToSchema};

use crate::Error;

/// Run labels: a bounded string map set when a run starts (agent, conversation, end user…).
/// They filter runs and scope API keys and webhooks. The script never sees them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(transparent)]
pub struct Labels(pub BTreeMap<String, String>);

impl Labels {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// As query strings write them: `key:value`.
    pub fn to_filters(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|(key, value)| format!("{key}:{value}"))
            .collect()
    }
}

impl std::ops::Deref for Labels {
    type Target = BTreeMap<String, String>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Labels {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<BTreeMap<String, String>> for Labels {
    fn from(labels: BTreeMap<String, String>) -> Self {
        Self(labels)
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for Labels {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(labels: I) -> Self {
        Self(
            labels
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        )
    }
}

impl<'a> IntoIterator for &'a Labels {
    type Item = (&'a String, &'a String);
    type IntoIter = std::collections::btree_map::Iter<'a, String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A time, as an RFC 3339 string in UTC (`"2026-11-09T10:02:11.120Z"`).
pub use jiff::Timestamp;

/// Converts a journal time (milliseconds since the Unix epoch) to an API time.
pub fn timestamp(time: rewind_core::Timestamp) -> Timestamp {
    let ms = i64::try_from(time.as_millis()).unwrap_or(i64::MAX);
    Timestamp::from_millisecond(ms).unwrap_or(Timestamp::MAX)
}

// -------------------------------------------------------------------------------------------
// Durations

/// A duration, written as scripts write them (`"500ms"`, `"30s"`, `"1h30m"`, `"2d"`, `"1w"`)
/// or as a number of milliseconds. Written back in the first form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration(std::time::Duration);

impl Duration {
    pub const fn from_millis(ms: u64) -> Self {
        Self(std::time::Duration::from_millis(ms))
    }

    pub const fn get(self) -> std::time::Duration {
        self.0
    }

    pub fn as_millis(self) -> u64 {
        u64::try_from(self.0.as_millis()).unwrap_or(u64::MAX)
    }
}

impl From<std::time::Duration> for Duration {
    fn from(duration: std::time::Duration) -> Self {
        Self(duration)
    }
}

impl From<Duration> for std::time::Duration {
    fn from(duration: Duration) -> Self {
        duration.0
    }
}

impl FromStr for Duration {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        if let Ok(ms) = text.trim().parse::<u64>() {
            return Ok(Self::from_millis(ms));
        }
        rewind_core::parse_duration(text)
            .map(Self)
            .map_err(|error| Error::invalid("duration", format!("{text:?}: {error}")))
    }
}

/// The shortest form: `"1h30m"`, `"500ms"`, `"0ms"`.
impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(&str, u64); 6] = [
            ("w", 604_800_000),
            ("d", 86_400_000),
            ("h", 3_600_000),
            ("m", 60_000),
            ("s", 1_000),
            ("ms", 1),
        ];
        let mut rest = self.as_millis();
        if rest == 0 {
            return f.write_str("0ms");
        }
        for (unit, ms) in UNITS {
            if rest >= ms {
                write!(f, "{}{unit}", rest / ms)?;
                rest %= ms;
            }
        }
        Ok(())
    }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DurationVisitor;

        impl Visitor<'_> for DurationVisitor {
            type Value = Duration;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"a duration such as "30s" or "1h30m", or a number of milliseconds"#)
            }

            fn visit_u64<E: de::Error>(self, ms: u64) -> Result<Duration, E> {
                Ok(Duration::from_millis(ms))
            }

            fn visit_i64<E: de::Error>(self, ms: i64) -> Result<Duration, E> {
                u64::try_from(ms)
                    .map(Duration::from_millis)
                    .map_err(|_| E::custom("a duration cannot be negative"))
            }

            fn visit_f64<E: de::Error>(self, ms: f64) -> Result<Duration, E> {
                if ms.is_finite() && ms >= 0.0 && ms <= u64::MAX as f64 {
                    Ok(Duration::from_millis(ms.round() as u64))
                } else {
                    Err(E::custom(
                        "a duration must be a non-negative number of milliseconds",
                    ))
                }
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Duration, E> {
                text.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(DurationVisitor)
    }
}

impl PartialSchema for Duration {
    fn schema() -> RefOr<Schema> {
        OneOfBuilder::new()
            .item(
                ObjectBuilder::new()
                    .schema_type(SchemaType::new(Type::String))
                    .pattern(Some(r"^\s*(\d+(\.\d+)?\s*(ms|s|m|h|d|w)\s*)+$"))
                    .examples(["30s", "1h30m", "500ms"]),
            )
            .item(
                ObjectBuilder::new()
                    .schema_type(SchemaType::new(Type::Integer))
                    .minimum(Some(0))
                    .description(Some("milliseconds")),
            )
            .description(Some(
                "A duration: a number and a unit (ms, s, m, h, d, w), possibly several \
                 (\"1h30m\"), or a number of milliseconds.",
            ))
            .into()
    }
}

impl ToSchema for Duration {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("Duration")
    }
}

// -------------------------------------------------------------------------------------------
// Payloads

/// Above this size, a payload's value is left out of run states, entries streamed and
/// webhooks: read it with `GET /v1/payloads/{digest}`. The agent tools bound what they hand the
/// model to the same size.
pub const PAYLOAD_VALUE_LIMIT: u64 = 256 * 1024;

/// A JSON value carried by a run (its input and output, action arguments and results, signal
/// payloads), identified by the SHA-256 of its canonical JSON (RFC 8785).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    /// SHA-256 of the canonical JSON, lowercase hex.
    pub digest: String,
    /// Size of the canonical JSON, in bytes.
    pub bytes: u64,
    /// The start of the canonical JSON, for display.
    pub preview: String,
    /// The value itself. Left out when it is larger than the endpoint returns inline
    /// (`PAYLOAD_VALUE_LIMIT`, 256 KiB): read it with `GET /v1/payloads/{digest}`.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub value: Option<Value>,
}

impl Payload {
    /// Characters of canonical JSON kept in a preview.
    pub const PREVIEW_CHARS: usize = 120;

    /// The payload of a value, with the value included if it is at most `limit` bytes.
    pub fn of(value: Value, limit: u64) -> Self {
        let canonical = rewind_core::canonical_json(&value);
        let bytes = canonical.len() as u64;
        Self {
            digest: rewind_core::Payload::digest_of(&value).to_hex(),
            bytes,
            preview: preview(&canonical),
            value: (bytes <= limit).then_some(value),
        }
    }

    /// A payload whose value is not at hand: only its identity and metadata.
    pub fn reference(digest: impl Into<String>, bytes: u64, preview: impl Into<String>) -> Self {
        Self {
            digest: digest.into(),
            bytes,
            preview: preview.into(),
            value: None,
        }
    }
}

/// The start of a canonical JSON, cut on a character boundary, with `…` when cut.
pub fn preview(canonical: &str) -> String {
    match canonical.char_indices().nth(Payload::PREVIEW_CHARS) {
        Some((end, _)) => format!("{}…", &canonical[..end]),
        None => canonical.to_owned(),
    }
}

/// Keeps a JSON `null` that is present apart from an absent field.
pub(crate) fn present<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

// -------------------------------------------------------------------------------------------
// Runs

/// The status of a run, a function of its journal (`archi/02-journal.md`, "Run state").
/// Quarantine is not a status: see `Run.quarantine`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Started, not activated yet.
    Pending,
    /// Running, or waiting for calls, timers or signals.
    Active,
    /// `main` returned and every call settled.
    Completed,
    /// The script failed.
    Failed,
    /// The script ended with `CancelledError`.
    Cancelled,
    /// The host ended the run (a limit, a script that cannot load, a given-up quarantine…).
    Aborted,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, RunStatus::Pending | RunStatus::Active)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Pending => "pending",
            RunStatus::Active => "active",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Aborted => "aborted",
        }
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<RunStatus> for rewind_core::RunStatus {
    fn from(status: RunStatus) -> Self {
        match status {
            RunStatus::Pending => rewind_core::RunStatus::Pending,
            RunStatus::Active => rewind_core::RunStatus::Active,
            RunStatus::Completed => rewind_core::RunStatus::Completed,
            RunStatus::Failed => rewind_core::RunStatus::Failed,
            RunStatus::Cancelled => rewind_core::RunStatus::Cancelled,
            RunStatus::Aborted => rewind_core::RunStatus::Aborted,
        }
    }
}

impl From<rewind_core::RunStatus> for RunStatus {
    fn from(status: rewind_core::RunStatus) -> Self {
        match status {
            rewind_core::RunStatus::Pending => RunStatus::Pending,
            rewind_core::RunStatus::Active => RunStatus::Active,
            rewind_core::RunStatus::Completed => RunStatus::Completed,
            rewind_core::RunStatus::Failed => RunStatus::Failed,
            rewind_core::RunStatus::Cancelled => RunStatus::Cancelled,
            rewind_core::RunStatus::Aborted => RunStatus::Aborted,
        }
    }
}

/// `live`: actions are really called and time is real. `mock`: actions are answered by cassettes
/// and mocks, and time is virtual.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Live,
    Mock,
}

impl From<rewind_core::RunMode> for RunMode {
    fn from(mode: rewind_core::RunMode) -> Self {
        match mode {
            rewind_core::RunMode::Live => RunMode::Live,
            rewind_core::RunMode::Mock => RunMode::Mock,
        }
    }
}

/// A position in the script's TypeScript source (`script.ts`), 1-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct Callsite {
    pub line: u32,
    pub column: u32,
}

impl From<rewind_core::Callsite> for Callsite {
    fn from(callsite: rewind_core::Callsite) -> Self {
        Self {
            line: callsite.line,
            column: callsite.column,
        }
    }
}

impl fmt::Display for Callsite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "script.ts:{}:{}", self.line, self.column)
    }
}

/// How a script failed: what `main` threw, the rejection it left unhandled, `DeadlockError`,
/// or `CancelledError`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScriptError {
    pub name: String,
    pub message: String,
    /// The stack, source-mapped to `script.ts`, without the runtime's own frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

impl From<&rewind_core::ScriptError> for ScriptError {
    fn from(error: &rewind_core::ScriptError) -> Self {
        Self {
            name: error.name.clone(),
            message: error.message.clone(),
            stack: error.stack.clone(),
        }
    }
}

/// How a call failed, as the script sees it (a thrown `ActionError`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActionFailure {
    pub kind: ActionFailureKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl From<&rewind_core::ActionFailure> for ActionFailure {
    fn from(failure: &rewind_core::ActionFailure) -> Self {
        Self {
            kind: failure.kind.into(),
            message: failure.message.clone(),
            data: failure.data.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionFailureKind {
    /// The action ran and reported an error.
    Error,
    /// The action could not be reached, after redeliveries.
    Unavailable,
    /// The action did not answer in time.
    Timeout,
    /// The call may or may not have taken effect (`ActionError.outcomeUnknown`).
    OutcomeUnknown,
}

impl From<ActionFailureKind> for rewind_core::ActionFailureKind {
    fn from(kind: ActionFailureKind) -> Self {
        match kind {
            ActionFailureKind::Error => rewind_core::ActionFailureKind::Error,
            ActionFailureKind::Unavailable => rewind_core::ActionFailureKind::Unavailable,
            ActionFailureKind::Timeout => rewind_core::ActionFailureKind::Timeout,
            ActionFailureKind::OutcomeUnknown => rewind_core::ActionFailureKind::OutcomeUnknown,
        }
    }
}

impl From<rewind_core::ActionFailureKind> for ActionFailureKind {
    fn from(kind: rewind_core::ActionFailureKind) -> Self {
        match kind {
            rewind_core::ActionFailureKind::Error => ActionFailureKind::Error,
            rewind_core::ActionFailureKind::Unavailable => ActionFailureKind::Unavailable,
            rewind_core::ActionFailureKind::Timeout => ActionFailureKind::Timeout,
            rewind_core::ActionFailureKind::OutcomeUnknown => ActionFailureKind::OutcomeUnknown,
        }
    }
}

/// Why the host ended a run (`run_aborted`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AbortReason {
    pub kind: AbortKind,
    pub message: String,
}

impl From<&rewind_core::AbortReason> for AbortReason {
    fn from(reason: &rewind_core::AbortReason) -> Self {
        Self {
            kind: reason.kind.into(),
            message: reason.message.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AbortKind {
    /// A resource limit was exceeded (CPU time, heap, commands, history length, payload size).
    LimitExceeded,
    /// A quarantined run was given up after a replay mismatch.
    NonDeterminism,
    /// The script did not end within its cancellation grace.
    Cancelled,
    /// The script could not be loaded.
    InvalidScript,
    /// A fault of the platform, or a quarantined run given up after repeated failures.
    Internal,
}

impl From<rewind_core::AbortKind> for AbortKind {
    fn from(kind: rewind_core::AbortKind) -> Self {
        match kind {
            rewind_core::AbortKind::LimitExceeded => AbortKind::LimitExceeded,
            rewind_core::AbortKind::NonDeterminism => AbortKind::NonDeterminism,
            rewind_core::AbortKind::Cancelled => AbortKind::Cancelled,
            rewind_core::AbortKind::InvalidScript => AbortKind::InvalidScript,
            rewind_core::AbortKind::Internal => AbortKind::Internal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl From<rewind_core::LogLevel> for LogLevel {
    fn from(level: rewind_core::LogLevel) -> Self {
        match level {
            rewind_core::LogLevel::Debug => LogLevel::Debug,
            rewind_core::LogLevel::Info => LogLevel::Info,
            rewind_core::LogLevel::Warn => LogLevel::Warn,
            rewind_core::LogLevel::Error => LogLevel::Error,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn durations_read_both_forms_and_write_the_shortest() {
        for (input, ms, written) in [
            (json!("30s"), 30_000, "30s"),
            (json!("1h30m"), 5_400_000, "1h30m"),
            (json!("1h 30m"), 5_400_000, "1h30m"),
            (json!("1.5d"), 129_600_000, "1d12h"),
            (json!("2w"), 1_209_600_000, "2w"),
            (json!(1500), 1500, "1s500ms"),
            (json!("250"), 250, "250ms"),
            (json!(0), 0, "0ms"),
        ] {
            let duration: Duration = serde_json::from_value(input.clone()).unwrap();
            assert_eq!(duration.as_millis(), ms, "{input}");
            assert_eq!(serde_json::to_value(duration).unwrap(), json!(written));
            let again: Duration = serde_json::from_value(json!(written)).unwrap();
            assert_eq!(again, duration);
        }
        for invalid in [
            json!(""),
            json!("ten seconds"),
            json!("5y"),
            json!(-1),
            json!(null),
        ] {
            assert!(
                serde_json::from_value::<Duration>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn a_payload_keeps_a_present_null_apart_from_an_absent_value() {
        let null = Payload::of(Value::Null, PAYLOAD_VALUE_LIMIT);
        assert_eq!(null.value, Some(Value::Null));
        let written = serde_json::to_value(&null).unwrap();
        assert_eq!(written["value"], Value::Null);
        assert!(written.as_object().unwrap().contains_key("value"));
        let read: Payload = serde_json::from_value(written).unwrap();
        assert_eq!(read.value, Some(Value::Null));

        let large = Payload::of(json!({ "text": "x".repeat(300) }), 100);
        assert_eq!(large.value, None);
        assert_eq!(large.bytes, 311);
        assert!(large.preview.ends_with('…'));
        assert_eq!(large.preview.chars().count(), Payload::PREVIEW_CHARS + 1);
        let read: Payload = serde_json::from_value(serde_json::to_value(&large).unwrap()).unwrap();
        assert_eq!(read, large);
        assert_eq!(
            large.digest,
            rewind_core::Payload::digest_of(&json!({ "text": "x".repeat(300) })).to_hex()
        );
    }

    #[test]
    fn times_are_rfc_3339() {
        let time = timestamp(rewind_core::Timestamp::from_millis(1_762_682_531_120));
        assert_eq!(
            serde_json::to_value(time).unwrap(),
            json!("2025-11-09T10:02:11.12Z")
        );
    }
}
