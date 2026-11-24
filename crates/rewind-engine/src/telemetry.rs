//! Run telemetry, projected from committed events (`archi/05-testing-and-observability.md`,
//! "Span model").
//!
//! One trace per run. Its id is the harness's when the run was started with a `traceparent`,
//! the run id otherwise. Span ids are derived from the run (`H` is a truncated SHA-256), so a
//! span exported again after a crash is the same span, and live service spans can name the
//! spans of a run's events as parents before those are exported:
//!
//! | Span | Id | From → to |
//! | --- | --- | --- |
//! | Run | `H(run, "run")` | `run_started` → the terminal event |
//! | Activation (service telemetry) | `H(run, "activation", index of its first event)` | leaving the ready queue → its batch durable |
//! | Call | `H(run, seq)` | `schedule_call` → its completion |
//! | Timer | `H(run, seq)` | `start_timer` → `timer_fired` or `cancel_timer` |
//! | Signal wait | `H(run, index of signal_awaited)` | → `signal_received` or its timeout |
//!
//! Payloads appear as digest, size and preview, never as values. Sampling is decided per run
//! from the trace id, so every node agrees without coordination.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rewind_core::{
    Annotation, Callsite, Command, Completion, Digest, Event, HistoryEvent, Lifecycle, Payload,
    RunId, RunMode, RunState, Seq, Timestamp, canonical_json,
};

use crate::metrics::RunMeasurement;
use crate::ports::{
    AttributeValue, LogRecord, PayloadRecord, RunHead, RunMetadata, Span, SpanId, SpanKind,
    SpanStatus, TraceContext, TraceId,
};

/// Characters of a payload's preview in a span attribute.
const PREVIEW_CHARS: usize = 120;

/// How run telemetry is exported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// The share of runs whose telemetry is exported, in millionths, decided from the trace id.
    /// 1,000,000 (every run) by default. Run summaries and metrics are never sampled.
    pub sampled_per_million: u32,
    /// The run labels exported as span attributes (`rewind.label.<key>`). Labels may name end
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
    /// An activation: the index of the first event it committed (`archi/08-decisions.md`,
    /// "Taken while writing the object-storage spec").
    Activation(u64),
    /// A call or a timer: its `seq`.
    Command(Seq),
    /// A signal wait: the index of its `signal_awaited`.
    Event(u64),
}

/// `H(run, key)`: the first 8 bytes of a SHA-256 over the run id and the key, with a tag per
/// kind of key so that a `seq` and an index never give the same id.
pub fn span_id(run: RunId, key: SpanKey) -> SpanId {
    let (tag, n): (&[u8], u64) = match key {
        SpanKey::Run => (b"run", 0),
        SpanKey::Activation(index) => (b"activation", index),
        SpanKey::Command(seq) => (b"seq", u64::from(seq.0)),
        SpanKey::Event(index) => (b"idx", index),
    };
    let mut bytes = Vec::with_capacity(48);
    bytes.extend_from_slice(b"rewind/span/");
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
pub fn trace_of(run: RunId, metadata: &RunMetadata) -> (TraceId, Option<SpanId>) {
    metadata
        .trace_context
        .as_ref()
        .and_then(|context| parse_traceparent(&context.traceparent))
        .map_or(
            (TraceId(*run.as_uuid().as_bytes()), None),
            |(trace, parent)| (trace, Some(parent)),
        )
}

/// Whether a trace's telemetry is exported. Decided from the right 7 bytes of the trace id,
/// which are random in a run id and in W3C trace ids: every node agrees.
pub fn sampled(trace: TraceId, config: &TelemetryConfig) -> bool {
    let mut low = [0u8; 8];
    low[1..].copy_from_slice(&trace.0[9..]);
    let draw = u128::from(u64::from_be_bytes(low));
    draw * 1_000_000 < u128::from(config.sampled_per_million) << 56
}

/// The trace context a call carries to its server (`_meta`) or harness handler: the run's
/// trace, with the call span `H(run, seq)` as parent.
pub fn call_trace_context(
    run: RunId,
    metadata: &RunMetadata,
    seq: Seq,
    config: &TelemetryConfig,
) -> TraceContext {
    let (trace, _) = trace_of(run, metadata);
    let span = span_id(run, SpanKey::Command(seq));
    let flags = if sampled(trace, config) { "01" } else { "00" };
    TraceContext {
        traceparent: format!("00-{trace}-{span}-{flags}"),
        tracestate: metadata
            .trace_context
            .as_ref()
            .and_then(|context| context.tracestate.clone()),
    }
}

/// What the span derivation knows of a run besides its events.
#[derive(Debug, Clone)]
pub(crate) struct RunTelemetry<'a> {
    pub(crate) head: &'a RunHead,
    pub(crate) config: &'a TelemetryConfig,
    /// The rows of the payloads held by digest: their size and preview.
    pub(crate) payloads: &'a BTreeMap<Digest, PayloadRecord>,
}

impl RunTelemetry<'_> {
    /// Whether this run's spans and logs are exported: sampled, and not a mock run unless the
    /// configuration asks for it.
    pub(crate) fn exported(&self, history: &[HistoryEvent]) -> bool {
        self.measured(history) && sampled(self.trace().0, self.config)
    }

    /// The run's trace.
    pub(crate) fn trace(&self) -> (TraceId, Option<SpanId>) {
        trace_of(self.head.run.id, &self.head.metadata)
    }

    /// Whether this run's metrics are recorded: not a mock run, whose times are virtual, unless
    /// the configuration exports them. Metrics are never sampled.
    pub(crate) fn measured(&self, history: &[HistoryEvent]) -> bool {
        let mock = history.first().is_some_and(|entry| {
            matches!(&entry.event, Event::Lifecycle(Lifecycle::RunStarted(started))
                if started.mode == RunMode::Mock)
        });
        !mock || self.config.export_mock_runs
    }

    /// What the events within `from..to` measure, given the history up to `to` at least and
    /// `before`, the run's state after the events before `from`: the run's start and end, and
    /// the calls that end within the range, timed as their spans are. The calls pending
    /// before the range are read at their opening event: the cost is the range's.
    pub(crate) fn measurements<'h>(
        &self,
        history: &'h [HistoryEvent],
        before: &RunState,
        from: u64,
        to: u64,
    ) -> Vec<RunMeasurement> {
        let mut scheduled: BTreeMap<Seq, (Timestamp, &'h str)> = before
            .pending_calls()
            .keys()
            .filter_map(|seq| {
                let entry = crate::projector::opening(history, before, *seq)?;
                match &entry.event {
                    Event::Command(Command::ScheduleCall { action, .. }) => {
                        Some((*seq, (entry.recorded_at, action.as_str())))
                    }
                    _ => None,
                }
            })
            .collect();
        let mut measurements = Vec::new();
        for entry in range(history, from, to) {
            let at = entry.recorded_at;
            let mut call = |seq: &Seq, outcome: &'static str, error_kind: Option<&'static str>| {
                if let Some((start, action)) = scheduled.remove(seq) {
                    measurements.push(call_measurement(action, outcome, error_kind, start, at));
                }
            };
            match &entry.event {
                Event::Lifecycle(Lifecycle::RunStarted(_)) => {
                    measurements.push(RunMeasurement::Started);
                }
                Event::Command(Command::ScheduleCall { seq, action, .. }) => {
                    scheduled.insert(*seq, (at, action.as_str()));
                }
                Event::Completion(Completion::CallCompleted { seq, .. }) => {
                    call(seq, "completed", None);
                }
                Event::Completion(Completion::CallFailed { seq, error }) => {
                    call(seq, "failed", Some(error.kind.as_str()));
                }
                _ => {}
            }
            if entry.event.is_terminal() {
                for (start, action) in std::mem::take(&mut scheduled).into_values() {
                    measurements.push(call_measurement(action, "no_outcome", None, start, at));
                }
                measurements.push(RunMeasurement::Ended {
                    outcome: run_outcome(&entry.event).to_owned(),
                });
            }
        }
        measurements
    }

    /// The spans that end within `from..to`, given the history up to `to` at least and
    /// `before`, the run's state after the events before `from`. A span is the same whichever
    /// range it is derived in: what was open before the range (calls, timers, signal waits) is
    /// read at its opening event, so the cost is the range's.
    pub(crate) fn spans(
        &self,
        history: &[HistoryEvent],
        before: &RunState,
        from: u64,
        to: u64,
    ) -> Vec<Span> {
        let mut derivation = Derivation::at(self, history, before);
        for entry in range(history, from, to) {
            derivation.apply(entry, true);
        }
        derivation.spans
    }

    /// The `console.*` logs within `from..to`, as log records of the activation span
    /// `activation`.
    pub(crate) fn logs(
        &self,
        history: &[HistoryEvent],
        from: u64,
        to: u64,
        activation: SpanKey,
    ) -> Vec<LogRecord> {
        let (trace, _) = self.trace();
        let run = self.head.run.id;
        range(history, from, to)
            .iter()
            .filter_map(|entry| match &entry.event {
                Event::Annotation(Annotation::Log {
                    level,
                    message,
                    callsite,
                }) => {
                    let mut attributes = self.base_attributes();
                    attributes.insert("rewind.index".into(), int(entry.index));
                    if let Some(callsite) = callsite {
                        attributes.insert("rewind.callsite".into(), callsite_attribute(callsite));
                    }
                    Some(LogRecord {
                        trace_id: trace,
                        span_id: Some(span_id(run, activation)),
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
            "rewind.run_id".into(),
            AttributeValue::String(self.head.run.id.to_string()),
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

fn call_measurement(
    action: &str,
    outcome: &'static str,
    error_kind: Option<&'static str>,
    start: Timestamp,
    end: Timestamp,
) -> RunMeasurement {
    RunMeasurement::Call {
        action: action.to_owned(),
        outcome: outcome.to_owned(),
        error_kind: error_kind.map(str::to_owned),
        seconds: end.0.saturating_sub(start.0) as f64 / 1_000.0,
    }
}

/// How a run ended at its terminal event, as its span and its metrics say.
fn run_outcome(event: &Event) -> &'static str {
    match event {
        Event::Command(Command::CompleteRun { .. }) => "completed",
        Event::Command(Command::FailRun { error })
            if error.name == rewind_core::ScriptError::CANCELLED =>
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
    AttributeValue::String(format!("workflow.ts:{}", callsite.line))
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

struct OpenCall {
    start: Timestamp,
    action: String,
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

/// A walk through a history that closes spans as their end events come.
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
        let (trace, harness_span) = run.trace();
        Self {
            run,
            trace,
            harness_span,
            run_span: span_id(run.head.run.id, SpanKey::Run),
            started_at: Timestamp(0),
            calls: BTreeMap::new(),
            timers: BTreeMap::new(),
            waits: VecDeque::new(),
            spans: Vec::new(),
        }
    }

    /// The walk as it stands after the events `before` folds: the run's start, and what is
    /// open (calls, timers, signal waits), each read at the event that opened it.
    fn at(run: &'t RunTelemetry<'a>, history: &[HistoryEvent], before: &RunState) -> Self {
        let mut derivation = Self::new(run);
        if let Some(first) = history.first()
            && before.history_len() > 0
            && matches!(first.event, Event::Lifecycle(Lifecycle::RunStarted(_)))
        {
            derivation.started_at = first.recorded_at;
        }
        let opening = |seq: &Seq| crate::projector::opening(history, before, *seq);
        for seq in before.pending_calls().keys() {
            if let Some(entry) = opening(seq)
                && let Event::Command(Command::ScheduleCall {
                    action,
                    args,
                    callsite,
                    ..
                }) = &entry.event
            {
                derivation.calls.insert(
                    *seq,
                    OpenCall {
                        start: entry.recorded_at,
                        action: action.clone(),
                        args: args.clone(),
                        callsite: *callsite,
                    },
                );
            }
        }
        for seq in before.pending_timers().keys() {
            if let Some(entry) = opening(seq)
                && let Event::Command(Command::StartTimer {
                    duration_ms,
                    callsite,
                    ..
                }) = &entry.event
            {
                derivation.timers.insert(
                    *seq,
                    OpenTimer {
                        start: entry.recorded_at,
                        duration_ms: *duration_ms,
                        callsite: *callsite,
                    },
                );
            }
        }
        for wait in before.awaited_signals() {
            let entry = usize::try_from(wait.since)
                .ok()
                .and_then(|index| history.get(index))
                .filter(|entry| entry.index == wait.since);
            if let Some(entry) = entry
                && let Event::Annotation(Annotation::SignalAwaited {
                    name,
                    timeout,
                    callsite,
                }) = &entry.event
            {
                derivation.waits.push_back(OpenWait {
                    index: entry.index,
                    start: entry.recorded_at,
                    name: name.clone(),
                    timeout: *timeout,
                    callsite: *callsite,
                });
            }
        }
        derivation
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
            span_id: span_id(self.run.head.run.id, key),
            parent: Some(self.run_span),
            name,
            kind,
            start,
            end,
            status,
            attributes,
        });
    }

    fn apply(&mut self, entry: &HistoryEvent, export: bool) {
        let at = entry.recorded_at;
        match &entry.event {
            Event::Lifecycle(Lifecycle::RunStarted(_)) => self.started_at = at,
            Event::Command(Command::ScheduleCall {
                seq,
                action,
                args,
                callsite,
            }) => {
                self.calls.insert(
                    *seq,
                    OpenCall {
                        start: at,
                        action: action.clone(),
                        args: args.clone(),
                        callsite: *callsite,
                    },
                );
            }
            Event::Completion(Completion::CallCompleted { seq, result }) => {
                if let Some(call) = self.calls.remove(seq)
                    && export
                {
                    let mut attributes = self.call_attributes(*seq, &call);
                    attributes.insert("rewind.outcome".into(), string("completed"));
                    self.run
                        .payload_attributes(&mut attributes, "rewind.result", result);
                    self.call_span(*seq, call, at, SpanStatus::Ok, attributes);
                }
            }
            Event::Completion(Completion::CallFailed { seq, error }) => {
                if let Some(call) = self.calls.remove(seq)
                    && export
                {
                    let mut attributes = self.call_attributes(*seq, &call);
                    attributes.insert("rewind.outcome".into(), string("failed"));
                    attributes.insert("rewind.error.kind".into(), string(error.kind.as_str()));
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
                let (timed_out, waiting): (VecDeque<OpenWait>, VecDeque<OpenWait>) =
                    std::mem::take(&mut self.waits)
                        .into_iter()
                        .partition(|wait| wait.timeout == Some(*seq));
                self.waits = waiting;
                if export {
                    for wait in timed_out {
                        self.wait_span(wait, at, "timed_out");
                    }
                }
            }
            // Every pending await is rejected, signal waits included: the run ends with its
            // grace delivery.
            Event::Completion(Completion::CancelRequested { .. }) => {
                let waits = std::mem::take(&mut self.waits);
                if export {
                    for wait in waits {
                        self.wait_span(wait, at, "ended");
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
        attributes.insert("rewind.seq".into(), int(u64::from(seq.0)));
        attributes.insert("rewind.action".into(), string(&call.action));
        if let Some(callsite) = &call.callsite {
            attributes.insert("rewind.callsite".into(), callsite_attribute(callsite));
        }
        self.run
            .payload_attributes(&mut attributes, "rewind.args", &call.args);
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
            call.action,
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
        attributes.insert("rewind.seq".into(), int(u64::from(seq.0)));
        attributes.insert("rewind.duration_ms".into(), int(timer.duration_ms));
        attributes.insert("rewind.outcome".into(), string(outcome));
        if let Some(callsite) = &timer.callsite {
            attributes.insert("rewind.callsite".into(), callsite_attribute(callsite));
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
        attributes.insert("rewind.signal".into(), string(&wait.name));
        attributes.insert("rewind.outcome".into(), string(outcome));
        if let Some(callsite) = &wait.callsite {
            attributes.insert("rewind.callsite".into(), callsite_attribute(callsite));
        }
        self.span(
            SpanKey::Event(wait.index),
            format!("signal {}", wait.name),
            SpanKind::Internal,
            (wait.start, end),
            SpanStatus::Unset,
            attributes,
        );
    }

    /// The run ended at `event`: what is still open ends with it, then the run span.
    fn end_run(&mut self, entry: &HistoryEvent, export: bool) {
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
            attributes.insert("rewind.outcome".into(), string("no_outcome"));
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

        let record = self.run.head;
        let mut attributes = self.run.base_attributes();
        attributes.insert("rewind.code".into(), string(&record.code.to_hex()));
        attributes.insert(
            "rewind.action_set".into(),
            string(&record.action_set.to_hex()),
        );
        for (key, value) in &record.metadata.labels {
            if self.run.config.exported_labels.contains(key) {
                attributes.insert(format!("rewind.label.{key}"), string(value));
            }
        }
        let status = match &entry.event {
            Event::Command(Command::CompleteRun { output }) => {
                self.run
                    .payload_attributes(&mut attributes, "rewind.output", output);
                SpanStatus::Ok
            }
            Event::Command(Command::FailRun { error }) => {
                attributes.insert("rewind.error.name".into(), string(&error.name));
                let message = format!("{}: {}", error.name, error.message);
                SpanStatus::Error { message }
            }
            Event::Lifecycle(Lifecycle::RunAborted(reason)) => {
                attributes.insert("rewind.abort.kind".into(), string(reason.kind.as_str()));
                let message = reason.to_string();
                SpanStatus::Error { message }
            }
            _ => SpanStatus::Unset,
        };
        attributes.insert("rewind.outcome".into(), string(run_outcome(&entry.event)));
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

/// The events `from..to` of an attempt's history, whose events sit at their index.
fn range(history: &[HistoryEvent], from: u64, to: u64) -> &[HistoryEvent] {
    let to = usize::try_from(to).map_or(history.len(), |to| to.min(history.len()));
    let from = usize::try_from(from).map_or(to, |from| from.min(to));
    &history[from..to]
}

fn string(text: &str) -> AttributeValue {
    AttributeValue::String(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{RunRef, TenantId};

    fn record(traceparent: Option<&str>) -> RunHead {
        RunHead {
            run: RunRef::new(TenantId::local(), RunId::from_u128(0x0123_4567_89ab_cdef)),
            namespace: crate::ports::NamespaceName::migrated(),
            origin: "script".into(),
            suffix_len: 12,
            code: Digest::of(b"s"),
            action_set: Digest::of(b"t"),
            status: rewind_core::RunStatus::Active,
            epoch: rewind_core::Epoch(1),
            attempt: crate::ports::Attempt::FIRST,
            sealed: None,
            failures: 0,
            attempts: 0,
            quarantine: None,
            waits: Vec::new(),
            expires_at: Timestamp(0),
            history_len: 0,
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
            span_id(run, SpanKey::Event(3)),
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
        let (trace, parent) = trace_of(harness.run.id, &harness.metadata);
        assert_eq!(trace.to_string(), "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(parent.unwrap().to_string(), "b7ad6b7169203331");

        for invalid in [
            None,
            Some("garbage"),
            Some("00-00000000000000000000000000000000-b7ad6b7169203331-01"),
        ] {
            let own = record(invalid);
            assert_eq!(
                trace_of(own.run.id, &own.metadata),
                (TraceId(*own.run.id.as_uuid().as_bytes()), None)
            );
        }

        let context = call_trace_context(
            harness.run.id,
            &harness.metadata,
            Seq(2),
            &TelemetryConfig::default(),
        );
        let span = span_id(harness.run.id, SpanKey::Command(Seq(2)));
        assert_eq!(
            context.traceparent,
            format!("00-0af7651916cd43dd8448eb211c80319c-{span}-01")
        );
        assert_eq!(context.tracestate.as_deref(), Some("vendor=1"));
    }

    fn history(mode: RunMode, events: Vec<(u64, Event)>) -> Vec<HistoryEvent> {
        let started = Event::Lifecycle(Lifecycle::RunStarted(rewind_core::RunStarted {
            code: Digest::of(b"s"),
            action_set: Digest::of(b"t"),
            input: Payload::Value(serde_json::json!({})),
            seed: rewind_core::Seed::from_bytes([0; 32]),
            epoch: rewind_core::Epoch(1),
            expires_at: Timestamp(u64::MAX),
            mode,
        }));
        std::iter::once((0, started))
            .chain(events)
            .enumerate()
            .map(|(index, (at, event))| HistoryEvent {
                index: index as u64,
                recorded_at: Timestamp(at),
                event,
            })
            .collect()
    }

    /// What the events `from..to` measure, the run's state before them folded.
    fn measure(
        telemetry: &RunTelemetry<'_>,
        events: &[HistoryEvent],
        from: usize,
        to: usize,
    ) -> Vec<RunMeasurement> {
        let before = RunState::from_history(&events[..from]).unwrap();
        telemetry.measurements(&events[..to], &before, from as u64, to as u64)
    }

    fn schedule(seq: u32, action: &str) -> Event {
        Event::Command(Command::ScheduleCall {
            seq: Seq(seq),
            action: action.into(),
            args: Payload::Value(serde_json::json!({})),
            callsite: None,
        })
    }

    #[test]
    fn a_run_measures_its_start_its_end_and_its_calls() {
        let failure = rewind_core::ActionFailure {
            kind: rewind_core::ActionFailureKind::Timeout,
            message: "too slow".into(),
            data: None,
        };
        let events = history(
            RunMode::Live,
            vec![
                (0, schedule(1, "github.list_issues")),
                (0, schedule(2, "slack.post")),
                (0, schedule(3, "slack.post")),
                (
                    1_500,
                    Event::Completion(Completion::CallCompleted {
                        seq: Seq(1),
                        result: Payload::Value(serde_json::json!([])),
                    }),
                ),
                (
                    4_000,
                    Event::Completion(Completion::CallFailed {
                        seq: Seq(2),
                        error: failure,
                    }),
                ),
                (
                    5_000,
                    Event::Command(Command::FailRun {
                        error: rewind_core::ScriptError {
                            name: rewind_core::ScriptError::CANCELLED.into(),
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
            head: &record,
            config: &config,
            payloads: &payloads,
        };
        let call =
            |action: &str, outcome: &str, error_kind: Option<&str>, seconds| RunMeasurement::Call {
                action: action.into(),
                outcome: outcome.into(),
                error_kind: error_kind.map(str::to_owned),
                seconds,
            };
        assert!(telemetry.measured(&events));
        // The first activation committed the start and the calls; the next ones, the rest.
        assert_eq!(
            measure(&telemetry, &events, 0, 4),
            vec![RunMeasurement::Started]
        );
        assert_eq!(
            measure(&telemetry, &events, 4, 6),
            vec![
                call("github.list_issues", "completed", None, 1.5),
                call("slack.post", "failed", Some("timeout"), 4.0),
            ]
        );
        // The call still open when the run ends ends with it.
        assert_eq!(
            measure(&telemetry, &events, 6, 7),
            vec![
                call("slack.post", "no_outcome", None, 5.0),
                RunMeasurement::Ended {
                    outcome: "cancelled".into()
                },
            ]
        );
        // Applied again, an item measures the same.
        assert_eq!(
            measure(&telemetry, &events, 6, 7),
            measure(&telemetry, &events, 6, 7)
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
