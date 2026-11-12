//! Service metrics: measurements the engine and the adapters emit through the `tracing` facade,
//! recorded with OpenTelemetry instruments and exported over OTLP.
//!
//! A field names the instrument, as `tracing-opentelemetry` does for events, and a span can
//! measure its own duration:
//!
//! | Field | On an event | On a span |
//! | --- | --- | --- |
//! | `monotonic_counter.<name>` | adds its value (an integer) to the counter `<name>` | nothing |
//! | `counter.<name>` | adds its value (an integer) to the up-down counter `<name>` | nothing |
//! | `histogram.<name>` | records its value in the histogram `<name>` | records the span's duration, in seconds, when it closes |
//! | `gauge.<name>` | records its value in the gauge `<name>` | nothing |
//! | `ptc.*`, but `ptc.run_id` and `ptc.seq` | an attribute of the measurement | an attribute of the measurement, as last recorded |
//!
//! No other field becomes an attribute, and neither do `ptc.run_id` and `ptc.seq`: a run id or a
//! task id next to a measurement never multiplies its series. A histogram named `*.duration` is in seconds (unit `s`, buckets from
//! 1 ms to a day); the others count things (buckets at the powers of two up to 1024).
//!
//! An event that measures is not a log: it is never exported as a log record nor written to
//! stderr, and `RUST_LOG` does not filter it out. A span that measures is still a span.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Instant;

use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, UpDownCounter};
use opentelemetry::{Context, Key, KeyValue, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context as LayerContext;
use tracing_subscriber::registry::LookupSpan;

/// The fields that make an attribute of a measurement start with this.
const ATTRIBUTE_PREFIX: &str = "ptc.";

/// The suffix of the histograms that hold seconds.
const DURATION_SUFFIX: &str = ".duration";

/// Bucket boundaries of durations, in seconds: from 1 ms (an activation of a cached instance) to
/// a day (a host tool waiting for a person).
const DURATION_BOUNDARIES: [f64; 20] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
    300.0, 900.0, 3_600.0, 21_600.0, 86_400.0,
];

/// Bucket boundaries of counts.
const COUNT_BOUNDARIES: [f64; 12] = [
    0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1_024.0,
];

/// The kind of instrument a field names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Counter,
    UpDownCounter,
    Histogram,
    Gauge,
}

const PREFIXES: [(&str, Kind); 4] = [
    ("monotonic_counter.", Kind::Counter),
    ("counter.", Kind::UpDownCounter),
    ("histogram.", Kind::Histogram),
    ("gauge.", Kind::Gauge),
];

/// The instrument a field names, if it names one.
fn instrument(field: &'static str) -> Option<(Kind, &'static str)> {
    PREFIXES.iter().find_map(|(prefix, kind)| {
        field
            .strip_prefix(prefix)
            .filter(|name| !name.is_empty())
            .map(|name| (*kind, name))
    })
}

/// Whether an event is a measurement: it has a field that names an instrument.
pub(crate) fn is_measurement(metadata: &Metadata<'_>) -> bool {
    metadata.is_event()
        && metadata
            .fields()
            .iter()
            .any(|field| instrument(field.name()).is_some())
}

/// The histograms a span records its duration in.
fn timed_histograms(metadata: &Metadata<'_>) -> Vec<&'static str> {
    metadata
        .fields()
        .iter()
        .filter_map(|field| match instrument(field.name()) {
            Some((Kind::Histogram, name)) => Some(name),
            _ => None,
        })
        .collect()
}

/// Whether a span or an event measures something.
pub(crate) fn measures(metadata: &Metadata<'_>) -> bool {
    if metadata.is_span() {
        !timed_histograms(metadata).is_empty()
    } else {
        is_measurement(metadata)
    }
}

/// The instruments of service metrics, created by name at first use.
pub(crate) struct Instruments {
    meter: Meter,
    counters: Registry<Counter<u64>>,
    up_down_counters: Registry<UpDownCounter<i64>>,
    histograms: Registry<Histogram<f64>>,
    gauges: Registry<Gauge<f64>>,
}

/// Instruments by name.
struct Registry<T>(RwLock<HashMap<&'static str, T>>);

impl<T: Clone> Registry<T> {
    fn new() -> Self {
        Self(RwLock::new(HashMap::new()))
    }

    fn get(&self, name: &'static str, create: impl FnOnce() -> T) -> T {
        let known = self
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned();
        known.unwrap_or_else(|| {
            self.0
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(name)
                .or_insert_with(create)
                .clone()
        })
    }
}

impl Instruments {
    pub(crate) fn new(meter: Meter) -> Self {
        Self {
            meter,
            counters: Registry::new(),
            up_down_counters: Registry::new(),
            histograms: Registry::new(),
            gauges: Registry::new(),
        }
    }

    fn record(&self, kind: Kind, name: &'static str, value: Number, attributes: &[KeyValue]) {
        match kind {
            Kind::Counter => {
                if let Some(value) = value.as_u64() {
                    self.counters
                        .get(name, || self.meter.u64_counter(name).build())
                        .add(value, attributes);
                }
            }
            Kind::UpDownCounter => {
                if let Some(value) = value.as_i64() {
                    self.up_down_counters
                        .get(name, || self.meter.i64_up_down_counter(name).build())
                        .add(value, attributes);
                }
            }
            Kind::Histogram => self.histogram(name).record(value.as_f64(), attributes),
            Kind::Gauge => self
                .gauges
                .get(name, || self.meter.f64_gauge(name).build())
                .record(value.as_f64(), attributes),
        }
    }

    fn histogram(&self, name: &'static str) -> Histogram<f64> {
        self.histograms.get(name, || {
            let builder = self.meter.f64_histogram(name);
            if name.ends_with(DURATION_SUFFIX) {
                builder
                    .with_unit("s")
                    .with_boundaries(DURATION_BOUNDARIES.to_vec())
                    .build()
            } else {
                builder.with_boundaries(COUNT_BOUNDARIES.to_vec()).build()
            }
        })
    }
}

impl fmt::Debug for Instruments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instruments").finish_non_exhaustive()
    }
}

/// A measured value, as `tracing` recorded it.
#[derive(Debug, Clone, Copy)]
enum Number {
    U64(u64),
    I64(i64),
    F64(f64),
}

impl Number {
    fn as_u64(self) -> Option<u64> {
        match self {
            Number::U64(n) => Some(n),
            Number::I64(n) => u64::try_from(n).ok(),
            Number::F64(_) => None,
        }
    }

    fn as_i64(self) -> Option<i64> {
        match self {
            Number::U64(n) => i64::try_from(n).ok(),
            Number::I64(n) => Some(n),
            Number::F64(_) => None,
        }
    }

    fn as_f64(self) -> f64 {
        match self {
            Number::U64(n) => n as f64,
            Number::I64(n) => n as f64,
            Number::F64(x) => x,
        }
    }
}

/// The `tracing` layer that records measurements. Its filter must let through only what
/// [`measures`].
pub(crate) struct MetricsLayer {
    pub(crate) instruments: Arc<Instruments>,
}

/// A span that measures its duration, until it closes.
struct TimedSpan {
    start: Instant,
    histograms: Vec<&'static str>,
    attributes: Vec<KeyValue>,
}

impl<S> Layer<S> for MetricsLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: LayerContext<'_, S>) {
        if Context::is_current_telemetry_suppressed() {
            return;
        }
        let histograms = timed_histograms(attrs.metadata());
        let Some(span) = ctx.span(id) else {
            return;
        };
        if histograms.is_empty() {
            return;
        }
        let mut timed = TimedSpan {
            start: Instant::now(),
            histograms,
            attributes: Vec::new(),
        };
        attrs.record(&mut AttributeFields {
            attributes: &mut timed.attributes,
        });
        span.extensions_mut().insert(timed);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: LayerContext<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        if let Some(timed) = span.extensions_mut().get_mut::<TimedSpan>() {
            values.record(&mut AttributeFields {
                attributes: &mut timed.attributes,
            });
        }
    }

    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        if Context::is_current_telemetry_suppressed() {
            return;
        }
        let mut fields = EventFields::default();
        event.record(&mut fields);
        for (kind, name, value) in fields.values {
            self.instruments
                .record(kind, name, value, &fields.attributes);
        }
    }

    fn on_close(&self, id: Id, ctx: LayerContext<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let Some(timed) = span.extensions_mut().remove::<TimedSpan>() else {
            return;
        };
        let seconds = timed.start.elapsed().as_secs_f64();
        for name in timed.histograms {
            self.instruments
                .histogram(name)
                .record(seconds, &timed.attributes);
        }
    }
}

/// Sets an attribute, replacing the value a field recorded before.
fn set(attributes: &mut Vec<KeyValue>, field: &Field, value: Value) {
    let key = Key::from_static_str(field.name());
    match attributes.iter_mut().find(|kv| kv.key == key) {
        Some(kv) => kv.value = value,
        None => attributes.push(KeyValue::new(key, value)),
    }
}

/// Fields named `ptc.*` that never become attributes of a measurement: one series per run or
/// per call.
const PER_RUN_FIELDS: [&str; 2] = ["ptc.run_id", "ptc.seq"];

fn is_attribute(field: &Field) -> bool {
    let name = field.name();
    name.starts_with(ATTRIBUTE_PREFIX) && !PER_RUN_FIELDS.contains(&name)
}

/// A span's `ptc.*` fields.
struct AttributeFields<'a> {
    attributes: &'a mut Vec<KeyValue>,
}

impl AttributeFields<'_> {
    fn attribute(&mut self, field: &Field, value: Value) {
        if is_attribute(field) {
            set(self.attributes, field, value);
        }
    }
}

impl Visit for AttributeFields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.attribute(field, Value::String(value.to_owned().into()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.attribute(field, Value::String(format!("{value:?}").into()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.attribute(field, Value::I64(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.attribute(field, Value::I64(i64::try_from(value).unwrap_or(i64::MAX)));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.attribute(field, Value::F64(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.attribute(field, Value::Bool(value));
    }
}

/// An event's measured values and its `ptc.*` fields.
#[derive(Default)]
struct EventFields {
    values: Vec<(Kind, &'static str, Number)>,
    attributes: Vec<KeyValue>,
}

impl EventFields {
    fn number(&mut self, field: &Field, value: Number) {
        if let Some((kind, name)) = instrument(field.name()) {
            self.values.push((kind, name, value));
        } else if is_attribute(field) {
            let value = match value {
                Number::U64(n) => Value::I64(i64::try_from(n).unwrap_or(i64::MAX)),
                Number::I64(n) => Value::I64(n),
                Number::F64(x) => Value::F64(x),
            };
            set(&mut self.attributes, field, value);
        }
    }

    fn attribute(&mut self, field: &Field, value: Value) {
        if is_attribute(field) {
            set(&mut self.attributes, field, value);
        }
    }
}

impl Visit for EventFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.attribute(field, Value::String(value.to_owned().into()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.attribute(field, Value::String(format!("{value:?}").into()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.number(field, Number::I64(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.number(field, Number::U64(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.number(field, Number::F64(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.attribute(field, Value::Bool(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_name_instruments_by_their_prefix() {
        assert_eq!(
            instrument("monotonic_counter.ptc.runs.started"),
            Some((Kind::Counter, "ptc.runs.started"))
        );
        assert_eq!(
            instrument("counter.ptc.queue"),
            Some((Kind::UpDownCounter, "ptc.queue"))
        );
        assert_eq!(
            instrument("histogram.ptc.activation.duration"),
            Some((Kind::Histogram, "ptc.activation.duration"))
        );
        assert_eq!(
            instrument("gauge.ptc.heap"),
            Some((Kind::Gauge, "ptc.heap"))
        );
        assert_eq!(instrument("histogram."), None);
        assert_eq!(instrument("ptc.outcome"), None);
        assert_eq!(instrument("counters.x"), None);
    }

    #[test]
    fn numbers_convert_only_when_they_fit() {
        assert_eq!(Number::I64(-1).as_u64(), None);
        assert_eq!(Number::I64(3).as_u64(), Some(3));
        assert_eq!(Number::U64(u64::MAX).as_i64(), None);
        assert_eq!(Number::F64(1.0).as_u64(), None);
        assert!((Number::U64(2).as_f64() - 2.0).abs() < f64::EPSILON);
    }
}
