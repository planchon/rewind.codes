//! What the engine measures (`archi/05-testing-and-observability.md`, "LLM trace and metrics").
//!
//! The engine never names OpenTelemetry: it measures through the `tracing` facade, and the
//! composition root's subscriber (`rewind-otel`) records the measurements with OpenTelemetry
//! instruments. A field names the instrument, as `tracing-opentelemetry` does:
//! `monotonic_counter.<name>` on an event adds its value to a counter, `histogram.<name>` on an
//! event records its value, and `histogram.<name>` on a span records the span's duration, in
//! seconds, when it closes. The fields named `rewind.*` are the measurement's attributes; no other
//! field is, so a run id never multiplies the series. The engine reads no clock to measure a
//! duration: the subscriber times the span.
//!
//! Service metrics, measured live:
//!
//! | Instrument | Kind | Attributes | Measures |
//! | --- | --- | --- | --- |
//! | `rewind.activation.duration` | histogram, s (span `activation`) | `rewind.outcome`: `committed`, `quarantined`, `released`, `failed`, `miss` | an activation on an executor, from its request to what it commits or why it cannot; `miss`: the instance was not kept and the request did not carry the history |
//! | `rewind.activation.inbox` | histogram | | the triggers an activation took from its run's inbox: the inbox's depth |
//! | `rewind.replay.duration` | histogram, s (span `replay`) | `rewind.outcome`: `end_of_history`, `aborted`, `non_determinism`, `error` | rebuilding an instance from its history |
//! | `rewind.instance_cache.lookups` | counter | `rewind.result`: `hit`, `miss` | activations of a started run, by whether its instance was kept (sticky execution) |
//! | `rewind.instance_cache.evictions` | counter | | instances dropped to keep the cache within its bounds |
//! | `rewind.runs.quarantined` | counter | `rewind.reason`: `non_determinism`, `failures` | runs quarantined |
//! | `rewind.delivery.duration` | histogram, s (span `delivery`) | `rewind.action`, `rewind.outcome`: `result`, `failed`, `transient`, `deferred`, `parked` | one dispatch of a call, to its answer |
//! | `rewind.delivery.redeliveries` | counter | `rewind.action` | transient failures delivered again after a backoff |
//!
//! Run metrics, derived from an activation's new events and recorded by the run's owner once
//! the batch that commits them is durable, for every run (sampling is for traces), mock runs
//! aside unless their telemetry is exported:
//!
//! | Instrument | Kind | Attributes | Measures |
//! | --- | --- | --- | --- |
//! | `rewind.runs.started` | counter | | runs started |
//! | `rewind.runs.ended` | counter | `rewind.outcome`: `completed`, `failed`, `cancelled`, `aborted` | runs ended |
//! | `rewind.call.duration` | histogram, s | `rewind.action`, `rewind.outcome`: `completed`, `failed`, `no_outcome`; `rewind.error.kind` | calls, from scheduled to completed in the run's logical time |
//!
//! Runs by status are the started runs less the ended ones, and the ended ones by outcome; an
//! action's error rate is the share of its calls whose outcome is not `completed`.

use crate::ports::{Dispatched, QuarantineKind};
use crate::replay::{ReplayEnd, ReplayError};

pub const ACTIVATION_DURATION: &str = "rewind.activation.duration";
pub const ACTIVATION_INBOX: &str = "rewind.activation.inbox";
pub const REPLAY_DURATION: &str = "rewind.replay.duration";
pub const INSTANCE_CACHE_LOOKUPS: &str = "rewind.instance_cache.lookups";
pub const INSTANCE_CACHE_EVICTIONS: &str = "rewind.instance_cache.evictions";
pub const RUNS_QUARANTINED: &str = "rewind.runs.quarantined";
pub const DELIVERY_DURATION: &str = "rewind.delivery.duration";
pub const REDELIVERIES: &str = "rewind.delivery.redeliveries";
pub const RUNS_STARTED: &str = "rewind.runs.started";
pub const RUNS_ENDED: &str = "rewind.runs.ended";
pub const CALL_DURATION: &str = "rewind.call.duration";

/// The attribute of an activation's, a replay's or an attempt's outcome.
pub const OUTCOME: &str = "rewind.outcome";

/// How a replay ended.
pub(crate) fn replay_outcome<I>(
    replayed: &Result<crate::replay::Rebuilt<I>, ReplayError>,
) -> &'static str {
    match replayed {
        Ok(rebuilt) => match rebuilt.end {
            ReplayEnd::EndOfHistory => "end_of_history",
            ReplayEnd::Aborted(_) => "aborted",
        },
        Err(ReplayError::NonDeterminism(_)) => "non_determinism",
        Err(_) => "error",
    }
}

/// How one dispatch of a call ended.
pub(crate) fn attempt_outcome(dispatched: &Dispatched) -> &'static str {
    match dispatched {
        Dispatched::Result(_) => "result",
        Dispatched::Failed(_) => "failed",
        Dispatched::Transient(_) => "transient",
        Dispatched::Deferred => "deferred",
        Dispatched::Park(_) => "parked",
    }
}

/// An activation took `triggers` from its run's inbox.
pub(crate) fn inbox(triggers: usize) {
    tracing::info!(histogram.rewind.activation.inbox = triggers as u64);
}

/// An activation of a started run found its instance in the cache, or not.
pub(crate) fn instance_cache_lookup(hit: bool) {
    let result = if hit { "hit" } else { "miss" };
    tracing::info!(
        monotonic_counter.rewind.instance_cache.lookups = 1_u64,
        rewind.result = result
    );
}

/// The cache dropped `evicted` instances.
pub(crate) fn instance_cache_evictions(evicted: usize) {
    if evicted > 0 {
        tracing::info!(monotonic_counter.rewind.instance_cache.evictions = evicted as u64);
    }
}

/// A run was quarantined.
pub(crate) fn quarantined(kind: QuarantineKind) {
    let reason = match kind {
        QuarantineKind::NonDeterminism => "non_determinism",
        QuarantineKind::Failures => "failures",
    };
    tracing::info!(
        monotonic_counter.rewind.runs.quarantined = 1_u64,
        rewind.reason = reason
    );
}

/// A transient failure of `action` is delivered again after a backoff.
pub(crate) fn redelivery(action: &str) {
    tracing::info!(
        monotonic_counter.rewind.delivery.redeliveries = 1_u64,
        rewind.action = action
    );
}

/// What a run's committed events measure: a run metric, recorded once the batch that commits
/// them is durable. An activation's executor derives them from the run's history; they reach the
/// run's owner with the activation's result.
#[derive(Debug, Clone, PartialEq)]
pub enum RunMeasurement {
    /// `rewind.runs.started`.
    Started,
    /// `rewind.runs.ended`, by how the run ended: `completed`, `failed`, `cancelled`, `aborted`.
    Ended { outcome: String },
    /// `rewind.call.duration`: a call that ended (`completed`, `failed`, or `no_outcome`
    /// when the run ended first), from its schedule to its outcome in the run's logical time.
    Call {
        action: String,
        outcome: String,
        error_kind: Option<String>,
        seconds: f64,
    },
}

impl RunMeasurement {
    /// Records the measurement through the `tracing` facade.
    pub fn emit(&self) {
        match self {
            RunMeasurement::Started => {
                tracing::info!(monotonic_counter.rewind.runs.started = 1_u64);
            }
            RunMeasurement::Ended { outcome } => {
                tracing::info!(
                    monotonic_counter.rewind.runs.ended = 1_u64,
                    rewind.outcome = outcome.as_str()
                );
            }
            RunMeasurement::Call {
                action,
                outcome,
                error_kind,
                seconds,
            } => {
                tracing::info!(
                    histogram.rewind.call.duration = *seconds,
                    rewind.action = action.as_str(),
                    rewind.outcome = outcome.as_str(),
                    rewind.error.kind = error_kind.as_deref(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::{ActionFailure, ActionFailureKind};

    use super::*;
    use crate::ports::{ParkKind, ParkReason};

    #[test]
    fn every_answer_of_a_dispatcher_has_its_outcome() {
        let failure = ActionFailure {
            kind: ActionFailureKind::Error,
            message: "no".into(),
            data: None,
        };
        let parked = ParkReason {
            kind: ParkKind::SchemaChanged,
            message: "changed".into(),
        };
        let outcomes = [
            Dispatched::Result(serde_json::json!(1)),
            Dispatched::Failed(failure.clone()),
            Dispatched::Transient(failure),
            Dispatched::Deferred,
            Dispatched::Park(parked),
        ]
        .map(|dispatched| attempt_outcome(&dispatched));
        assert_eq!(
            outcomes,
            ["result", "failed", "transient", "deferred", "parked"]
        );
    }
}
