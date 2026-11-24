//! Events and journal entries, v1.

use rewind_core::{
    AbortKind, AbortReason, Annotation, Callsite, CancelReason, Command, Completion, Digest, Epoch,
    Event, HistoryEntry, Lifecycle, LogLevel, Payload, RunMode, RunStarted, ScriptError, Seed, Seq,
    Timestamp, ToolFailure, ToolFailureKind, canonical_json,
};
use serde::de::Deserializer;
use serde::ser::{Error as _, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

// -------------------------------------------------------------------------------------------
// DTOs

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EntryV1 {
    index: u64,
    recorded_at: u64,
    event: EventV1,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum EventV1 {
    // Lifecycle
    RunStarted {
        script: String,
        action_set: String,
        input: PayloadV1,
        seed: String,
        epoch: u32,
        expires_at: u64,
        mode: RunModeV1,
    },
    RunAborted {
        reason: AbortReasonV1,
    },
    // Commands
    ScheduleToolCall {
        seq: u32,
        tool: String,
        args: PayloadV1,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<CallsiteV1>,
    },
    StartTimer {
        seq: u32,
        duration_ms: u64,
        fire_at: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<CallsiteV1>,
    },
    CancelTimer {
        seq: u32,
    },
    CompleteRun {
        output: PayloadV1,
    },
    FailRun {
        error: ScriptErrorV1,
    },
    // Completions
    ToolCallCompleted {
        seq: u32,
        result: PayloadV1,
    },
    ToolCallFailed {
        seq: u32,
        error: ToolFailureV1,
    },
    TimerFired {
        seq: u32,
    },
    SignalReceived {
        name: String,
        payload: PayloadV1,
    },
    CancelRequested {
        reason: CancelReasonV1,
    },
    // Annotations
    Log {
        level: LogLevelV1,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<CallsiteV1>,
    },
    SignalAwaited {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callsite: Option<CallsiteV1>,
    },
}

/// `{"value": …}` or `{"digest": "<hex>"}`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PayloadV1 {
    Value(CanonicalJson),
    Digest(String),
}

/// A JSON value, written in its canonical form (RFC 8785): sorted keys, ECMAScript numbers,
/// minimal escaping. The stored bytes of a value are then exactly the bytes its digest is
/// computed on, whatever the JSON library's settings.
pub(crate) struct CanonicalJson(pub(crate) Value);

impl Serialize for CanonicalJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        RawValue::from_string(canonical_json(&self.0))
            .map_err(S::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CanonicalJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunModeV1 {
    Live,
    Mock,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AbortReasonV1 {
    kind: AbortKindV1,
    message: String,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AbortKindV1 {
    LimitExceeded,
    NonDeterminism,
    Cancelled,
    InvalidScript,
    Internal,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub(crate) struct CallsiteV1 {
    line: u32,
    column: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScriptErrorV1 {
    name: String,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stack: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolFailureV1 {
    kind: ToolFailureKindV1,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<CanonicalJson>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolFailureKindV1 {
    Error,
    Unavailable,
    Timeout,
    OutcomeUnknown,
}

/// `{"kind":"requested"}`, `{"kind":"requested","message":"…"}` or `{"kind":"ttl"}`.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CancelReasonV1 {
    Requested {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    // A struct variant, not a unit one: serde ignores unknown fields next to the tag of a unit
    // variant, and a reader refuses them.
    Ttl {},
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LogLevelV1 {
    Debug,
    Info,
    Warn,
    Error,
}

// -------------------------------------------------------------------------------------------
// Decoding entry points

pub(crate) fn decode_event(value: Value) -> Result<Event, String> {
    let dto: EventV1 = serde_json::from_value(value).map_err(|err| err.to_string())?;
    dto.try_into()
}

pub(crate) fn decode_entry(value: Value) -> Result<HistoryEntry, String> {
    let dto: EntryV1 = serde_json::from_value(value).map_err(|err| err.to_string())?;
    Ok(HistoryEntry {
        index: dto.index,
        recorded_at: Timestamp(dto.recorded_at),
        event: dto.event.try_into()?,
    })
}

// -------------------------------------------------------------------------------------------
// Domain → DTO

impl From<&HistoryEntry> for EntryV1 {
    fn from(entry: &HistoryEntry) -> Self {
        Self {
            index: entry.index,
            recorded_at: entry.recorded_at.as_millis(),
            event: EventV1::from(&entry.event),
        }
    }
}

impl From<&Event> for EventV1 {
    fn from(event: &Event) -> Self {
        match event {
            Event::Lifecycle(Lifecycle::RunStarted(started)) => EventV1::RunStarted {
                script: started.script.to_hex(),
                action_set: started.action_set.to_hex(),
                input: (&started.input).into(),
                seed: hex(started.seed.as_bytes()),
                epoch: started.epoch.0,
                expires_at: started.expires_at.as_millis(),
                mode: started.mode.into(),
            },
            Event::Lifecycle(Lifecycle::RunAborted(reason)) => EventV1::RunAborted {
                reason: AbortReasonV1 {
                    kind: reason.kind.into(),
                    message: reason.message.clone(),
                },
            },
            Event::Command(command) => command.into(),
            Event::Completion(completion) => completion.into(),
            Event::Annotation(annotation) => annotation.into(),
        }
    }
}

impl From<&Command> for EventV1 {
    fn from(command: &Command) -> Self {
        match command {
            Command::ScheduleToolCall {
                seq,
                tool,
                args,
                callsite,
            } => EventV1::ScheduleToolCall {
                seq: seq.0,
                tool: tool.clone(),
                args: args.into(),
                callsite: callsite.map(Into::into),
            },
            Command::StartTimer {
                seq,
                duration_ms,
                fire_at,
                callsite,
            } => EventV1::StartTimer {
                seq: seq.0,
                duration_ms: *duration_ms,
                fire_at: fire_at.as_millis(),
                callsite: callsite.map(Into::into),
            },
            Command::CancelTimer { seq } => EventV1::CancelTimer { seq: seq.0 },
            Command::CompleteRun { output } => EventV1::CompleteRun {
                output: output.into(),
            },
            Command::FailRun { error } => EventV1::FailRun {
                error: ScriptErrorV1 {
                    name: error.name.clone(),
                    message: error.message.clone(),
                    stack: error.stack.clone(),
                },
            },
        }
    }
}

impl From<&Completion> for EventV1 {
    fn from(completion: &Completion) -> Self {
        match completion {
            Completion::ToolCallCompleted { seq, result } => EventV1::ToolCallCompleted {
                seq: seq.0,
                result: result.into(),
            },
            Completion::ToolCallFailed { seq, error } => EventV1::ToolCallFailed {
                seq: seq.0,
                error: error.into(),
            },
            Completion::TimerFired { seq } => EventV1::TimerFired { seq: seq.0 },
            Completion::SignalReceived { name, payload } => EventV1::SignalReceived {
                name: name.clone(),
                payload: payload.into(),
            },
            Completion::CancelRequested { reason } => EventV1::CancelRequested {
                reason: match reason {
                    CancelReason::Requested(message) => CancelReasonV1::Requested {
                        message: message.clone(),
                    },
                    CancelReason::Ttl => CancelReasonV1::Ttl {},
                },
            },
        }
    }
}

impl From<&Annotation> for EventV1 {
    fn from(annotation: &Annotation) -> Self {
        match annotation {
            Annotation::Log {
                level,
                message,
                callsite,
            } => EventV1::Log {
                level: (*level).into(),
                message: message.clone(),
                callsite: callsite.map(Into::into),
            },
            Annotation::SignalAwaited {
                name,
                timeout,
                callsite,
            } => EventV1::SignalAwaited {
                name: name.clone(),
                timeout: timeout.map(|seq| seq.0),
                callsite: callsite.map(Into::into),
            },
        }
    }
}

impl From<&Payload> for PayloadV1 {
    fn from(payload: &Payload) -> Self {
        match payload {
            Payload::Value(value) => PayloadV1::Value(CanonicalJson(value.clone())),
            Payload::Digest(digest) => PayloadV1::Digest(digest.to_hex()),
        }
    }
}

impl From<&ToolFailure> for ToolFailureV1 {
    fn from(failure: &ToolFailure) -> Self {
        Self {
            kind: failure.kind.into(),
            message: failure.message.clone(),
            data: failure.data.clone().map(CanonicalJson),
        }
    }
}

impl From<RunMode> for RunModeV1 {
    fn from(mode: RunMode) -> Self {
        match mode {
            RunMode::Live => RunModeV1::Live,
            RunMode::Mock => RunModeV1::Mock,
        }
    }
}

impl From<AbortKind> for AbortKindV1 {
    fn from(kind: AbortKind) -> Self {
        match kind {
            AbortKind::LimitExceeded => AbortKindV1::LimitExceeded,
            AbortKind::NonDeterminism => AbortKindV1::NonDeterminism,
            AbortKind::Cancelled => AbortKindV1::Cancelled,
            AbortKind::InvalidScript => AbortKindV1::InvalidScript,
            AbortKind::Internal => AbortKindV1::Internal,
        }
    }
}

impl From<Callsite> for CallsiteV1 {
    fn from(callsite: Callsite) -> Self {
        Self {
            line: callsite.line,
            column: callsite.column,
        }
    }
}

impl From<ToolFailureKind> for ToolFailureKindV1 {
    fn from(kind: ToolFailureKind) -> Self {
        match kind {
            ToolFailureKind::Error => ToolFailureKindV1::Error,
            ToolFailureKind::Unavailable => ToolFailureKindV1::Unavailable,
            ToolFailureKind::Timeout => ToolFailureKindV1::Timeout,
            ToolFailureKind::OutcomeUnknown => ToolFailureKindV1::OutcomeUnknown,
        }
    }
}

impl From<LogLevel> for LogLevelV1 {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Debug => LogLevelV1::Debug,
            LogLevel::Info => LogLevelV1::Info,
            LogLevel::Warn => LogLevelV1::Warn,
            LogLevel::Error => LogLevelV1::Error,
        }
    }
}

// -------------------------------------------------------------------------------------------
// DTO → domain

impl TryFrom<EventV1> for Event {
    type Error = String;

    fn try_from(dto: EventV1) -> Result<Self, String> {
        Ok(match dto {
            EventV1::RunStarted {
                script,
                action_set,
                input,
                seed,
                epoch,
                expires_at,
                mode,
            } => Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
                script: digest(&script)?,
                action_set: digest(&action_set)?,
                input: input.try_into()?,
                seed: Seed::from_bytes(unhex(&seed).ok_or_else(|| {
                    format!("invalid seed {seed:?}: expected 64 lowercase hex digits")
                })?),
                epoch: Epoch(epoch),
                expires_at: Timestamp(expires_at),
                mode: mode.into(),
            })),
            EventV1::RunAborted { reason } => {
                Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
                    kind: reason.kind.into(),
                    message: reason.message,
                }))
            }
            EventV1::ScheduleToolCall {
                seq,
                tool,
                args,
                callsite,
            } => Event::Command(Command::ScheduleToolCall {
                seq: Seq(seq),
                tool,
                args: args.try_into()?,
                callsite: callsite.map(Into::into),
            }),
            EventV1::StartTimer {
                seq,
                duration_ms,
                fire_at,
                callsite,
            } => Event::Command(Command::StartTimer {
                seq: Seq(seq),
                duration_ms,
                fire_at: Timestamp(fire_at),
                callsite: callsite.map(Into::into),
            }),
            EventV1::CancelTimer { seq } => Event::Command(Command::CancelTimer { seq: Seq(seq) }),
            EventV1::CompleteRun { output } => Event::Command(Command::CompleteRun {
                output: output.try_into()?,
            }),
            EventV1::FailRun { error } => Event::Command(Command::FailRun {
                error: ScriptError {
                    name: error.name,
                    message: error.message,
                    stack: error.stack,
                },
            }),
            EventV1::ToolCallCompleted { seq, result } => {
                Event::Completion(Completion::ToolCallCompleted {
                    seq: Seq(seq),
                    result: result.try_into()?,
                })
            }
            EventV1::ToolCallFailed { seq, error } => {
                Event::Completion(Completion::ToolCallFailed {
                    seq: Seq(seq),
                    error: error.into(),
                })
            }
            EventV1::TimerFired { seq } => {
                Event::Completion(Completion::TimerFired { seq: Seq(seq) })
            }
            EventV1::SignalReceived { name, payload } => {
                Event::Completion(Completion::SignalReceived {
                    name,
                    payload: payload.try_into()?,
                })
            }
            EventV1::CancelRequested { reason } => Event::Completion(Completion::CancelRequested {
                reason: match reason {
                    CancelReasonV1::Requested { message } => CancelReason::Requested(message),
                    CancelReasonV1::Ttl {} => CancelReason::Ttl,
                },
            }),
            EventV1::Log {
                level,
                message,
                callsite,
            } => Event::Annotation(Annotation::Log {
                level: level.into(),
                message,
                callsite: callsite.map(Into::into),
            }),
            EventV1::SignalAwaited {
                name,
                timeout,
                callsite,
            } => Event::Annotation(Annotation::SignalAwaited {
                name,
                timeout: timeout.map(Seq),
                callsite: callsite.map(Into::into),
            }),
        })
    }
}

impl TryFrom<PayloadV1> for Payload {
    type Error = String;

    fn try_from(dto: PayloadV1) -> Result<Self, String> {
        Ok(match dto {
            PayloadV1::Value(CanonicalJson(value)) => Payload::Value(value),
            PayloadV1::Digest(hex) => Payload::Digest(digest(&hex)?),
        })
    }
}

impl From<ToolFailureV1> for ToolFailure {
    fn from(dto: ToolFailureV1) -> Self {
        Self {
            kind: dto.kind.into(),
            message: dto.message,
            data: dto.data.map(|CanonicalJson(data)| data),
        }
    }
}

impl From<RunModeV1> for RunMode {
    fn from(mode: RunModeV1) -> Self {
        match mode {
            RunModeV1::Live => RunMode::Live,
            RunModeV1::Mock => RunMode::Mock,
        }
    }
}

impl From<AbortKindV1> for AbortKind {
    fn from(kind: AbortKindV1) -> Self {
        match kind {
            AbortKindV1::LimitExceeded => AbortKind::LimitExceeded,
            AbortKindV1::NonDeterminism => AbortKind::NonDeterminism,
            AbortKindV1::Cancelled => AbortKind::Cancelled,
            AbortKindV1::InvalidScript => AbortKind::InvalidScript,
            AbortKindV1::Internal => AbortKind::Internal,
        }
    }
}

impl From<CallsiteV1> for Callsite {
    fn from(callsite: CallsiteV1) -> Self {
        Self {
            line: callsite.line,
            column: callsite.column,
        }
    }
}

impl From<ToolFailureKindV1> for ToolFailureKind {
    fn from(kind: ToolFailureKindV1) -> Self {
        match kind {
            ToolFailureKindV1::Error => ToolFailureKind::Error,
            ToolFailureKindV1::Unavailable => ToolFailureKind::Unavailable,
            ToolFailureKindV1::Timeout => ToolFailureKind::Timeout,
            ToolFailureKindV1::OutcomeUnknown => ToolFailureKind::OutcomeUnknown,
        }
    }
}

impl From<LogLevelV1> for LogLevel {
    fn from(level: LogLevelV1) -> Self {
        match level {
            LogLevelV1::Debug => LogLevel::Debug,
            LogLevelV1::Info => LogLevel::Info,
            LogLevelV1::Warn => LogLevel::Warn,
            LogLevelV1::Error => LogLevel::Error,
        }
    }
}

// -------------------------------------------------------------------------------------------
// Hex

pub(crate) fn digest(hex: &str) -> Result<Digest, String> {
    unhex(hex)
        .map(Digest::from_bytes)
        .ok_or_else(|| format!("invalid digest {hex:?}: expected 64 lowercase hex digits"))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// 64 lowercase hex digits → 32 bytes. Uppercase is rejected: one value, one spelling.
pub(crate) fn unhex(text: &str) -> Option<[u8; 32]> {
    let digits = text.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut bytes = [0u8; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = nibble(digits[2 * i])? << 4 | nibble(digits[2 * i + 1])?;
    }
    Some(bytes)
}
