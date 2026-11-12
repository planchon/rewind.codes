//! Run telemetry, projected from committed entries (`archi/05-testing-and-observability.md`,
//! "Span model").
//!
//! One trace per run. Its id is the harness's when the run was started with a `traceparent`,
//! the run id otherwise. Span ids are derived from the run (`H` is a truncated SHA-256), so a
//! span exported again after a crash is the same span, and live service spans can name journal
//! spans as parents before those are exported:
//!
//! | Span | Id | From → to |
//! | --- | --- | --- |
//! | Run | `H(run, "run")` | `run_started` → the terminal entry |
//! | Activation (service telemetry) | `H(run, fence)` | lease → commit |
//! | Tool call | `H(run, seq)` | `schedule_tool_call` → its completion |
//! | Timer | `H(run, seq)` | `start_timer` → `timer_fired` or `cancel_timer` |
//! | Signal wait | `H(run, index of signal_awaited)` | → `signal_received` or its timeout |
//!
//! Payloads appear as digest, size and preview, never as values. Sampling is decided per run
//! from the trace id, so every worker agrees without coordination.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use ptc_core::{
    Annotation, Callsite, Command, Completion, Digest, Event, HistoryEntry, Lifecycle, Payload,
    RunId, RunMode, Seq, Timestamp, canonical_json,
};

use crate::metrics::RunMeasurement;
use crate::ports::{
    AttributeValue, LogRecord, PayloadRecord, RunRecord, Span, SpanId, SpanKind, SpanStatus,
    TraceContext, TraceId,
};

/// Characters of a payload's preview in a span attribute.
const PREVIEW_CHARS: usize = 120;

/// How run telemetry is exported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// The share of runs whose telemetry is exported, in millionths, decided from the trace id.
    /// 1,000,000 (every run) by default. The read model is never sampled.
    pub sampled_per_million: u32,
    /// The run labels exported as span attributes (`ptc.label.<key>`). Labels may name end
    /// users: the others are never exported. None by default.
    pub exported_labels: BTreeSet<String>,
    /// Export the telemetry of mock runs. Off by default: their timestamps are virtual.
    pub export_mock_runs: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            sampled_per_million: 1_000_000,
            exported_labels: BTreeSet::new(),
            export_mock_runs: false,
        }
    }
}

/// What a span id is derived from, besides the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKey {
    /// The run span.
    Run,
    /// An activation: the fence of its lease.
    Activation(u64),
    /// A tool call or a timer: its `seq`.
    Command(Seq),
    /// A signal wait: the index of its `signal_awaited`.
    Entry(u64),
}

/// `H(run, key)`: the first 8 bytes of a SHA-256 over the run id and the key, with a tag per
/// kind of key so that a `seq`, a fence and an index never give the same id.
pub fn span_id(run: RunId, key: SpanKey) -> SpanId {
    let (tag, n): (&[u8], u64) = match key {
        SpanKey::Run => (b"run", 0),
        SpanKey::Activation(fence) => (b"fence", fence),
        SpanKey::Command(seq) => (b"seq", u64::from(seq.0)),
        SpanKey::Entry(index) => (b"idx", index),
    };
    let mut bytes = Vec::with_capacity(48);
    bytes.extend_from_slice(b"ai-ptc/span/");
    bytes.extend_from_slice(tag);
    bytes.push(0);
    bytes.extend_from_slice(run.as_uuid().as_bytes());
    bytes.extend_from_slice(&n.to_be_bytes());
    let digest = Digest::of(&bytes);
    let mut id = [0u8; 8];
    id.copy_from_slice(&digest.as_bytes()[..8]);
    // An all-zero span id is invalid in W3C trace context.
    if id == [0; 8] {
        id[7] = 1;
    }
    SpanId(id)
}

/// A W3C `traceparent`: version, trace id, parent span id, flags.
fn parse_traceparent(traceparent: &str) -> Option<(TraceId, SpanId)> {
    let mut parts = traceparent.trim().split('-');
    let version = parts.next()?;
    let trace = parts.next()?;
    let parent = parts.next()?;
    let _flags = parts.next()?;
    if version.len() != 2 || version == "ff" || trace.len() != 32 || parent.len() != 16 {
        return None;
    }
    let trace = hex::<16>(trace)?;
    let parent = hex::<8>(parent)?;
    if trace == [0; 16] || parent == [0; 8] {
        return None;
    }
    Some((TraceId(trace), SpanId(parent)))
}

fn hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let mut bytes = [0u8; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(bytes)
}

/// The run's trace: the harness's trace id and span when it gave a valid `traceparent`, the run
/// id and no parent otherwise.
pub fn trace_of(record: &RunRecord) -> (TraceId, Option<SpanId>) {
    record
        .metadata
        .trace_context
        .as_ref()
        .and_then(|context| parse_traceparent(&context.traceparent))
        .map_or(
            (TraceId(*record.run.id.as_uuid().as_bytes()), None),
            |(trace, parent)| (trace, Some(parent)),
        )
}

/// Whether a trace's telemetry is exported. Decided from the right 7 bytes of the trace id,
/// which are random in a UUID v7 and in W3C trace ids: every worker agrees.
pub fn sampled(trace: TraceId, config: &TelemetryConfig) -> bool {
    let mut low = [0u8; 8];
    low[1..].copy_from_slice(&trace.0[9..]);
    let draw = u128::from(u64::from_be_bytes(low));
    draw * 1_000_000 < u128::from(config.sampled_per_million) << 56
}

/// The trace context a tool call carries to its server (`_meta`) or host-tool handler: the run's
/// trace, with the tool call span `H(run, seq)` as parent.
pub fn tool_call_trace_context(
    record: &RunRecord,
    seq: Seq,
    config: &TelemetryConfig,
) -> TraceContext {
    let (trace, _) = trace_of(record);
    let span = span_id(record.run.id, SpanKey::Command(seq));
    let flags = if sampled(trace, config) { "01" } else { "00" };
    TraceContext {
        traceparent: format!("00-{trace}-{span}-{flags}"),
        tracestate: record
            .metadata
            .trace_context
            .as_ref()
            .and_then(|context| context.tracestate.clone()),
    }
}

/// What the span derivation knows of a run besides its entries.
#[derive(Debug, Clone)]
pub(crate) struct RunTelemetry<'a> {
    pub(crate) record: &'a RunRecord,
    pub(crate) config: &'a TelemetryConfig,
    /// The rows of the payloads held by digest: their size and preview.
    pub(crate) payloads: &'a BTreeMap<Digest, PayloadRecord>,
}

impl RunTelemetry<'_> {
    /// Whether this run's spans and logs are exported: sampled, and not a mock run unless the
    /// configuration asks for it.
    pub(crate) fn exported(&self, history: &[HistoryEntry]) -> bool {
        self.measured(history) && sampled(trace_of(self.record).0, self.config)
    }

    /// Whether this run's metrics are recorded: not a mock run, whose times are virtual, unless
    /// the configuration exports them. Metrics are never sampled.
    pub(crate) fn measured(&self, history: &[HistoryEntry]) -> bool {
        let mock = history.first().is_some_and(|entry| {
            matches!(&entry.event, Event::Lifecycle(Lifecycle::RunStarted(started))
                if started.mode == RunMode::Mock)
        });
        !mock || self.config.export_mock_runs
    }

    /// What the entries within `from..to` measure, given the history up to `to` at least: the
    /// run's start and end, and the tool calls that end within the range, timed as their spans
    /// are.
    pub(crate) fn measurements(
        &self,
        history: &[HistoryEntry],
        from: u64,
        to: u64,
    ) -> Vec<RunMeasurement> {
        let mut scheduled: BTreeMap<Seq, (Timestamp, &str)> = BTreeMap::new();
        let mut measurements = Vec::new();
        for entry in history.iter().take_while(|entry| entry.index < to) {
            let within = (from..to).contains(&entry.index);
            let at = entry.recorded_at;
            let mut call = |seq: &Seq, outcome: &'static str, error_kind: Option<&'static str>| {
                if let Some((start, tool)) = scheduled.remove(seq)
                    && within
                {
                    measurements.push(tool_call(tool, outcome, error_kind, start, at));
                }
            };
            match &entry.event {
                Event::Lifecycle(Lifecycle::RunStarted(_)) if within => {
                    measurements.push(RunMeasurement::Started);
                }
                Event::Command(Command::ScheduleToolCall { seq, tool, .. }) => {
                    scheduled.insert(*seq, (at, tool.as_str()));
                }
                Event::Completion(Completion::ToolCallCompleted { seq, .. }) => {
                    call(seq, "completed", None);
                }
                Event::Completion(Completion::ToolCallFailed { seq, error }) => {
                    call(seq, "failed", Some(error.kind.as_str()));
                }
                _ => {}
            }
            if entry.event.is_terminal() {
                let open = std::mem::take(&mut scheduled);
                if within {
                    for (start, tool) in open.into_values() {
                        measurements.push(tool_call(tool, "no_outcome", None, start, at));
                    }
                    measurements.push(RunMeasurement::Ended {
                        outcome: run_outcome(&entry.event),
                    });
                }
            }
        }
        measurements
    }

    /// The spans that end within `from..to`, given the history up to `to` at least. Each is
    /// derived from the whole history before it, so a feed item applied twice, or late, gives
    /// the same spans.
    pub(crate) fn spans(&self, history: &[HistoryEntry], from: u64, to: u64) -> Vec<Span> {
        let mut derivation = Derivation::new(self);
        for entry in history.iter().take_while(|entry| entry.index < to) {
            derivation.apply(entry, (from..to).contains(&entry.index));
        }
        derivation.spans
    }

    /// The `console.*` logs within `from..to`, as log records of the activation span
    /// `H(run, fence)`.
    pub(crate) fn logs(
        &self,
        history: &[HistoryEntry],
        from: u64,
        to: u64,
        fence: u64,
    ) -> Vec<LogRecord> {
        let (trace, _) = trace_of(self.record);
        let run = self.record.run.id;
        history
            .iter()
            .filter(|entry| (from..to).contains(&entry.index))
            .filter_map(|entry| match &entry.event {
                Event::Annotation(Annotation::Log {
                    level,
                    message,
                    callsite,
                }) => {
                    let mut attributes = self.base_attributes();
                    attributes.insert("ptc.index".into(), int(entry.index));
                    if let Some(callsite) = callsite {
                        attributes.insert("ptc.callsite".into(), callsite_attribute(callsite));
                    }
                    Some(LogRecord {
                        trace_id: trace,
                        span_id: Some(span_id(run, SpanKey::Activation(fence))),
                        time: entry.recorded_at,
                        level: *level,
                        message: message.clone(),
                        attributes,
                    })
                }
                _ => None,
            })
            .collect()
    }

    fn base_attributes(&self) -> BTreeMap<String, AttributeValue> {
        let mut attributes = BTreeMap::new();
        attributes.insert(
            "ptc.run_id".into(),
            AttributeValue::String(self.record.run.id.to_string()),
        );
        attributes
    }

    fn payload_attributes(
        &self,
        attributes: &mut BTreeMap<String, AttributeValue>,
        prefix: &str,
        payload: &Payload,
    ) {
        let digest = payload.digest();
        let (size, preview) = match payload {
            Payload::Value(value) => {
                let canonical = canonical_json(value);
                (
                    Some(canonical.len() as u64),
                    Some(payload.preview(PREVIEW_CHARS)),
                )
            }
            Payload::Digest(_) => match self.payloads.get(&digest) {
                Some(record) => (
                    Some(record.size),
                    Some(truncate(&record.preview, PREVIEW_CHARS)),
                ),
                None => (None, None),
            },
        };
        attributes.insert(
            format!("{prefix}.digest"),
            AttributeValue::String(digest.to_hex()),
        );
        if let Some(size) = size {
            attributes.insert(format!("{prefix}.size"), int(size));
        }
        if let Some(preview) = preview {
            attributes.insert(format!("{prefix}.preview"), AttributeValue::String(preview));
        }
    }
}

fn tool_call(
    tool: &str,
    outcome: &'static str,
    error_kind: Option<&'static str>,
    start: Timestamp,
    end: Timestamp,
) -> RunMeasurement {
    RunMeasurement::ToolCall {
        tool: tool.to_owned(),
        outcome,
        error_kind,
        seconds: end.0.saturating_sub(start.0) as f64 / 1_000.0,
    }
}

/// How a run ended at its terminal entry, as its span and its metrics say.
fn run_outcome(event: &Event) -> &'static str {
    match event {
        Event::Command(Command::CompleteRun { .. }) => "completed",
        Event::Command(Command::FailRun { error })
            if error.name == ptc_core::ScriptError::CANCELLED =>
        {
            "cancelled"
        }
        Event::Command(Command::FailRun { .. }) => "failed",
        Event::Lifecycle(Lifecycle::RunAborted(_)) => "aborted",
        _ => "ended",
    }
}

fn int(n: u64) -> AttributeValue {
    AttributeValue::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

fn callsite_attribute(callsite: &Callsite) -> AttributeValue {
    AttributeValue::String(format!("script.ts:{}", callsite.line))
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

struct OpenCall {
    start: Timestamp,
    tool: String,
    args: Payload,
    callsite: Option<Callsite>,
}

struct OpenTimer {
    start: Timestamp,
    duration_ms: u64,
    callsite: Option<Callsite>,
}

struct OpenWait {
    index: u64,
    start: Timestamp,
    name: String,
    timeout: Option<Seq>,
    callsite: Option<Callsite>,
}

/// A walk through a history that closes spans as their end entries come.
struct Derivation<'t, 'a> {
    run: &'t RunTelemetry<'a>,
    trace: TraceId,
    harness_span: Option<SpanId>,
    run_span: SpanId,
    started_at: Timestamp,
    calls: BTreeMap<Seq, OpenCall>,
    timers: BTreeMap<Seq, OpenTimer>,
    /// Signal waits, oldest first: a signal resolves the oldest wait on its name.
    waits: VecDeque<OpenWait>,
    spans: Vec<Span>,
}

impl<'t, 'a> Derivation<'t, 'a> {
    fn new(run: &'t RunTelemetry<'a>) -> Self {
        let (trace, harness_span) = trace_of(run.record);
        Self {
            run,
            trace,
            harness_span,
            run_span: span_id(run.record.run.id, SpanKey::Run),
            started_at: Timestamp(0),
            calls: BTreeMap::new(),
            timers: BTreeMap::new(),
            waits: VecDeque::new(),
            spans: Vec::new(),
        }
    }

    fn span(
        &mut self,
        key: SpanKey,
        name: String,
        kind: SpanKind,
        (start, end): (Timestamp, Timestamp),
        status: SpanStatus,
        attributes: BTreeMap<String, AttributeValue>,
    ) {
        self.spans.push(Span {
            trace_id: self.trace,
            span_id: span_id(self.run.record.run.id, key),
            parent: Some(self.run_span),
            name,
            kind,
            start,
            end,
            status,
            attributes,
        });
    }

    fn apply(&mut self, entry: &HistoryEntry, export: bool) {
        let at = entry.recorded_at;
        match &entry.event {
            Event::Lifecycle(Lifecycle::RunStarted(_)) => self.started_at = at,
            Event::Command(Command::ScheduleToolCall {
                seq,
                tool,
                args,
                callsite,
            }) => {
                self.calls.insert(
                    *seq,
                    OpenCall {
                        start: at,
                        tool: tool.clone(),
                        args: args.clone(),
                        callsite: *callsite,
                    },
                );
            }
            Event::Completion(Completion::ToolCallCompleted { seq, result }) => {
                if let Some(call) = self.calls.remove(seq)
                    && export
                {
                    let mut attributes = self.call_attributes(*seq, &call);
                    attributes.insert("ptc.outcome".into(), string("completed"));
                    self.run
                        .payload_attributes(&mut attributes, "ptc.result", result);
                    self.call_span(*seq, call, at, SpanStatus::Ok, attributes);
                }
            }
            Event::Completion(Completion::ToolCallFailed { seq, error }) => {
                if let Some(call) = self.calls.remove(seq)
                    && export
                {
                    let mut attributes = self.call_attributes(*seq, &call);
                    attributes.insert("ptc.outcome".into(), string("failed"));
                    attributes.insert("ptc.error.kind".into(), string(error.kind.as_str()));
                    let status = SpanStatus::Error {
                        message: error.message.clone(),
                    };
                    self.call_span(*seq, call, at, status, attributes);
                }
            }
            Event::Command(Command::StartTimer {
                seq,
                duration_ms,
                callsite,
                ..
            }) => {
                self.timers.insert(
                    *seq,
                    OpenTimer {
                        start: at,
                        duration_ms: *duration_ms,
                        callsite: *callsite,
                    },
                );
            }
            Event::Completion(Completion::TimerFired { seq }) => {
                self.close_timer(*seq, at, "fired", export);
                if let Some(position) = self.waits.iter().position(|w| w.timeout == Some(*seq)) {
                    let wait = self.waits.remove(position);
                    if let Some(wait) = wait
                        && export
                    {
                        self.wait_span(wait, at, "timed_out");
                    }
                }
            }
            Event::Command(Command::CancelTimer { seq }) => {
                self.close_timer(*seq, at, "cancelled", export);
            }
            Event::Annotation(Annotation::SignalAwaited {
                name,
                timeout,
                callsite,
            }) => self.waits.push_back(OpenWait {
                index: entry.index,
                start: at,
                name: name.clone(),
                timeout: *timeout,
                callsite: *callsite,
            }),
            Event::Completion(Completion::SignalReceived { name, .. }) => {
                if let Some(position) = self.waits.iter().position(|wait| &wait.name == name) {
                    let wait = self.waits.remove(position);
                    if let Some(wait) = wait
                        && export
                    {
                        self.wait_span(wait, at, "received");
                    }
                }
            }
            _ => {}
        }
        if entry.event.is_terminal() {
            self.end_run(entry, export);
        }
    }

    fn call_attributes(&self, seq: Seq, call: &OpenCall) -> BTreeMap<String, AttributeValue> {
        let mut attributes = self.run.base_attributes();
        attributes.insert("ptc.seq".into(), int(u64::from(seq.0)));
        attributes.insert("ptc.tool".into(), string(&call.tool));
        if let Some(callsite) = &call.callsite {
            attributes.insert("ptc.callsite".into(), callsite_attribute(callsite));
        }
        self.run
            .payload_attributes(&mut attributes, "ptc.args", &call.args);
        attributes
    }

    fn call_span(
        &mut self,
        seq: Seq,
        call: OpenCall,
        end: Timestamp,
        status: SpanStatus,
        attributes: BTreeMap<String, AttributeValue>,
    ) {
        self.span(
            SpanKey::Command(seq),
            call.tool,
            SpanKind::Client,
            (call.start, end),
            status,
            attributes,
        );
    }

    fn close_timer(&mut self, seq: Seq, at: Timestamp, outcome: &str, export: bool) {
        let Some(timer) = self.timers.remove(&seq) else {
            return;
        };
        if !export {
            return;
        }
        let mut attributes = self.run.base_attributes();
        attributes.insert("ptc.seq".into(), int(u64::from(seq.0)));
        attributes.insert("ptc.duration_ms".into(), int(timer.duration_ms));
        attributes.insert("ptc.outcome".into(), string(outcome));
        if let Some(callsite) = &timer.callsite {
            attributes.insert("ptc.callsite".into(), callsite_attribute(callsite));
        }
        self.span(
            SpanKey::Command(seq),
            "timer".into(),
            SpanKind::Internal,
            (timer.start, at),
            SpanStatus::Unset,
            attributes,
        );
    }

    fn wait_span(&mut self, wait: OpenWait, end: Timestamp, outcome: &str) {
        let mut attributes = self.run.base_attributes();
        attributes.insert("ptc.signal".into(), string(&wait.name));
        attributes.insert("ptc.outcome".into(), string(outcome));
        if let Some(callsite) = &wait.callsite {
            attributes.insert("ptc.callsite".into(), callsite_attribute(callsite));
        }
        self.span(
            SpanKey::Entry(wait.index),
            format!("signal {}", wait.name),
            SpanKind::Internal,
            (wait.start, end),
            SpanStatus::Unset,
            attributes,
        );
    }

    /// The run ended at `entry`: what is still open ends with it, then the run span.
    fn end_run(&mut self, entry: &HistoryEntry, export: bool) {
        let at = entry.recorded_at;
        let calls = std::mem::take(&mut self.calls);
        let timers: Vec<Seq> = self.timers.keys().copied().collect();
        let waits = std::mem::take(&mut self.waits);
        if !export {
            self.timers.clear();
            return;
        }
        for (seq, call) in calls {
            let mut attributes = self.call_attributes(seq, &call);
            attributes.insert("ptc.outcome".into(), string("no_outcome"));
            let status = SpanStatus::Error {
                message: "the run ended before the call had an outcome".into(),
            };
            self.call_span(seq, call, at, status, attributes);
        }
        for seq in timers {
            self.close_timer(seq, at, "dropped", true);
        }
        for wait in waits {
            self.wait_span(wait, at, "ended");
        }

        let record = self.run.record;
        let mut attributes = self.run.base_attributes();
        attributes.insert("ptc.script".into(), string(&record.script.to_hex()));
        attributes.insert("ptc.toolset".into(), string(&record.toolset.to_hex()));
        for (key, value) in &record.metadata.labels {
            if self.run.config.exported_labels.contains(key) {
                attributes.insert(format!("ptc.label.{key}"), string(value));
            }
        }
        let status = match &entry.event {
            Event::Command(Command::CompleteRun { output }) => {
                self.run
                    .payload_attributes(&mut attributes, "ptc.output", output);
                SpanStatus::Ok
            }
            Event::Command(Command::FailRun { error }) => {
                attributes.insert("ptc.error.name".into(), string(&error.name));
                let message = format!("{}: {}", error.name, error.message);
                SpanStatus::Error { message }
            }
            Event::Lifecycle(Lifecycle::RunAborted(reason)) => {
                attributes.insert("ptc.abort.kind".into(), string(reason.kind.as_str()));
                let message = reason.to_string();
                SpanStatus::Error { message }
            }
            _ => SpanStatus::Unset,
        };
        attributes.insert("ptc.outcome".into(), string(run_outcome(&entry.event)));
        self.spans.push(Span {
            trace_id: self.trace,
            span_id: self.run_span,
            parent: self.harness_span,
            name: "run".into(),
            kind: SpanKind::Internal,
            start: self.started_at,
            end: at,
            status,
            attributes,
        });
    }
}

fn string(text: &str) -> AttributeValue {
    AttributeValue::String(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{RunMetadata, RunRef, TenantId};

    fn record(traceparent: Option<&str>) -> RunRecord {
        RunRecord {
            run: RunRef::new(TenantId::local(), RunId::from_u128(0x0123_4567_89ab_cdef)),
            script: Digest::of(b"s"),
            toolset: Digest::of(b"t"),
            status: ptc_core::RunStatus::Active,
            epoch: ptc_core::Epoch(1),
            expires_at: Timestamp(0),
            history_len: 0,
            quarantine: None,
            failures: 0,
            sticky_worker: None,
            metadata: RunMetadata {
                trace_context: traceparent.map(|traceparent| TraceContext {
                    traceparent: traceparent.into(),
                    tracestate: Some("vendor=1".into()),
                }),
                ..RunMetadata::default()
            },
            idempotency_key: None,
            created_at: Timestamp(0),
            updated_at: Timestamp(0),
            ended_at: None,
            archive: None,
        }
    }

    #[test]
    fn span_ids_are_deterministic_and_distinct_per_kind() {
        let run = RunId::from_u128(1);
        assert_eq!(
            span_id(run, SpanKey::Command(Seq(3))),
            span_id(run, SpanKey::Command(Seq(3)))
        );
        let ids: BTreeSet<SpanId> = [
            span_id(run, SpanKey::Run),
            span_id(run, SpanKey::Command(Seq(3))),
            span_id(run, SpanKey::Activation(3)),
            span_id(run, SpanKey::Entry(3)),
            span_id(RunId::from_u128(2), SpanKey::Command(Seq(3))),
        ]
        .into();
        assert_eq!(ids.len(), 5);
    }

    #[test]
    fn the_trace_is_the_harness_s_or_the_run_s() {
        let harness = record(Some(
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
        ));
        let (trace, parent) = trace_of(&harness);
        assert_eq!(trace.to_string(), "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(parent.unwrap().to_string(), "b7ad6b7169203331");

        for invalid in [
            None,
            Some("garbage"),
            Some("00-00000000000000000000000000000000-b7ad6b7169203331-01"),
        ] {
            let own = record(invalid);
            assert_eq!(
                trace_of(&own),
                (TraceId(*own.run.id.as_uuid().as_bytes()), None)
            );
        }

        let context = tool_call_trace_context(&harness, Seq(2), &TelemetryConfig::default());
        let span = span_id(harness.run.id, SpanKey::Command(Seq(2)));
        assert_eq!(
            context.traceparent,
            format!("00-0af7651916cd43dd8448eb211c80319c-{span}-01")
        );
        assert_eq!(context.tracestate.as_deref(), Some("vendor=1"));
    }

    fn history(mode: RunMode, events: Vec<(u64, Event)>) -> Vec<HistoryEntry> {
        let started = Event::Lifecycle(Lifecycle::RunStarted(ptc_core::RunStarted {
            script: Digest::of(b"s"),
            toolset: Digest::of(b"t"),
            input: Payload::Value(serde_json::json!({})),
            seed: ptc_core::Seed::from_bytes([0; 32]),
            epoch: ptc_core::Epoch(1),
            expires_at: Timestamp(u64::MAX),
            mode,
        }));
        std::iter::once((0, started))
            .chain(events)
            .enumerate()
            .map(|(index, (at, event))| HistoryEntry {
                index: index as u64,
                recorded_at: Timestamp(at),
                event,
            })
            .collect()
    }

    fn schedule(seq: u32, tool: &str) -> Event {
        Event::Command(Command::ScheduleToolCall {
            seq: Seq(seq),
            tool: tool.into(),
            args: Payload::Value(serde_json::json!({})),
            callsite: None,
        })
    }

    #[test]
    fn a_run_measures_its_start_its_end_and_its_tool_calls() {
        let failure = ptc_core::ToolFailure {
            kind: ptc_core::ToolFailureKind::Timeout,
            message: "too slow".into(),
            data: None,
        };
        let entries = history(
            RunMode::Live,
            vec![
                (0, schedule(1, "github.list_issues")),
                (0, schedule(2, "slack.post")),
                (0, schedule(3, "slack.post")),
                (
                    1_500,
                    Event::Completion(Completion::ToolCallCompleted {
                        seq: Seq(1),
                        result: Payload::Value(serde_json::json!([])),
                    }),
                ),
                (
                    4_000,
                    Event::Completion(Completion::ToolCallFailed {
                        seq: Seq(2),
                        error: failure,
                    }),
                ),
                (
                    5_000,
                    Event::Command(Command::FailRun {
                        error: ptc_core::ScriptError {
                            name: ptc_core::ScriptError::CANCELLED.into(),
                            message: "stop".into(),
                            stack: None,
                        },
                    }),
                ),
            ],
        );
        let config = TelemetryConfig::default();
        let payloads = BTreeMap::new();
        let record = record(None);
        let telemetry = RunTelemetry {
            record: &record,
            config: &config,
            payloads: &payloads,
        };
        let call = |tool: &str, outcome, error_kind, seconds| RunMeasurement::ToolCall {
            tool: tool.into(),
            outcome,
            error_kind,
            seconds,
        };
        assert!(telemetry.measured(&entries));
        // The first activation committed the start and the calls; the next ones, the rest.
        assert_eq!(
            telemetry.measurements(&entries, 0, 4),
            vec![RunMeasurement::Started]
        );
        assert_eq!(
            telemetry.measurements(&entries, 4, 6),
            vec![
                call("github.list_issues", "completed", None, 1.5),
                call("slack.post", "failed", Some("timeout"), 4.0),
            ]
        );
        // The call still open when the run ends ends with it.
        assert_eq!(
            telemetry.measurements(&entries, 6, 7),
            vec![
                call("slack.post", "no_outcome", None, 5.0),
                RunMeasurement::Ended {
                    outcome: "cancelled"
                },
            ]
        );
        // Applied again, an item measures the same.
        assert_eq!(
            telemetry.measurements(&entries, 6, 7),
            telemetry.measurements(&entries, 6, 7)
        );

        // Mock runs are not measured unless their telemetry is exported; sampling does not
        // matter.
        let mock = history(RunMode::Mock, Vec::new());
        assert!(!telemetry.measured(&mock));
        let sampled_out = TelemetryConfig {
            sampled_per_million: 0,
            export_mock_runs: true,
            ..TelemetryConfig::default()
        };
        let telemetry = RunTelemetry {
            config: &sampled_out,
            ..telemetry
        };
        assert!(telemetry.measured(&mock));
        assert!(!telemetry.exported(&mock));
    }

    #[test]
    fn sampling_is_decided_by_the_trace_id() {
        let mut config = TelemetryConfig::default();
        let trace = |low: u8| {
            let mut bytes = [0xab; 16];
            bytes[9..].copy_from_slice(&[low, 0, 0, 0, 0, 0, 0]);
            TraceId(bytes)
        };
        assert!(sampled(trace(0xff), &config));
        config.sampled_per_million = 0;
        assert!(!sampled(trace(0), &config));
        config.sampled_per_million = 500_000;
        assert!(sampled(trace(0x10), &config));
        assert!(!sampled(trace(0x90), &config));
    }
}
