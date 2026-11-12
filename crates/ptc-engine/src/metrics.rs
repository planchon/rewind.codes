//! What the engine measures (`archi/05-testing-and-observability.md`, "LLM trace and metrics").
//!
//! The engine never names OpenTelemetry: it measures through the `tracing` facade, and the
//! composition root's subscriber (`ptc-otel`) records the measurements with OpenTelemetry
//! instruments. A field names the instrument, as `tracing-opentelemetry` does:
//! `monotonic_counter.<name>` on an event adds its value to a counter, `histogram.<name>` on an
//! event records its value, and `histogram.<name>` on a span records the span's duration, in
//! seconds, when it closes. The fields named `ptc.*` are the measurement's attributes; no other
//! field is, so a run id never multiplies the series. The engine reads no clock to measure a
//! duration: the subscriber times the span.
//!
//! Service metrics, measured live:
//!
//! | Instrument | Kind | Attributes | Measures |
//! | --- | --- | --- | --- |
//! | `ptc.activation.duration` | histogram, s (span `activation`) | `ptc.outcome`: `committed`, `nothing_to_do`, `quarantined`, `failed`, `lease_lost` | an activation, from its lease to its commit or its failure |
//! | `ptc.activation.inbox` | histogram | | the triggers an activation took from its run's inbox: the inbox's depth |
//! | `ptc.replay.duration` | histogram, s (span `replay`) | `ptc.outcome`: `end_of_history`, `aborted`, `non_determinism`, `error` | rebuilding an instance from its history |
//! | `ptc.instance_cache.lookups` | counter | `ptc.result`: `hit`, `miss` | activations of a started run, by whether its instance was kept (sticky execution) |
//! | `ptc.instance_cache.evictions` | counter | | instances dropped to keep the cache within its bounds |
//! | `ptc.runs.quarantined` | counter | `ptc.reason`: `non_determinism`, `failures` | runs quarantined |
//! | `ptc.tool_attempt.duration` | histogram, s (span `tool_attempt`) | `ptc.tool`, `ptc.outcome`: `result`, `failed`, `transient`, `deferred`, `parked` | one dispatch of a tool call, to its answer |
//! | `ptc.tool_attempt.retries` | counter | `ptc.tool` | transient failures dispatched again after a backoff |
//!
//! Lease contention shows as activations whose outcome is `lease_lost`: another worker took
//! the run over.
//!
//! Run metrics, projected from committed entries by the projector's `otel` consumer once the
//! feed item is acknowledged, for every run (sampling is for traces), mock runs aside unless
//! their telemetry is exported:
//!
//! | Instrument | Kind | Attributes | Measures |
//! | --- | --- | --- | --- |
//! | `ptc.runs.started` | counter | | runs started |
//! | `ptc.runs.ended` | counter | `ptc.outcome`: `completed`, `failed`, `cancelled`, `aborted` | runs ended |
//! | `ptc.tool_call.duration` | histogram, s | `ptc.tool`, `ptc.outcome`: `completed`, `failed`, `no_outcome`; `ptc.error.kind` | tool calls, from scheduled to completed in the run's logical time |
//!
//! Runs by status are the started runs less the ended ones, and the ended ones by outcome; a
//! tool's error rate is the share of its calls whose outcome is not `completed`.

use crate::engine::{ActivationOutcome, EngineError};
use crate::ports::{Dispatched, QuarantineKind, StoreError};
use crate::replay::{ReplayEnd, ReplayError};

pub const ACTIVATION_DURATION: &str = "ptc.activation.duration";
pub const ACTIVATION_INBOX: &str = "ptc.activation.inbox";
pub const REPLAY_DURATION: &str = "ptc.replay.duration";
pub const INSTANCE_CACHE_LOOKUPS: &str = "ptc.instance_cache.lookups";
pub const INSTANCE_CACHE_EVICTIONS: &str = "ptc.instance_cache.evictions";
pub const RUNS_QUARANTINED: &str = "ptc.runs.quarantined";
pub const TOOL_ATTEMPT_DURATION: &str = "ptc.tool_attempt.duration";
pub const TOOL_ATTEMPT_RETRIES: &str = "ptc.tool_attempt.retries";
pub const RUNS_STARTED: &str = "ptc.runs.started";
pub const RUNS_ENDED: &str = "ptc.runs.ended";
pub const TOOL_CALL_DURATION: &str = "ptc.tool_call.duration";

/// The attribute of an activation's, a replay's or an attempt's outcome.
pub const OUTCOME: &str = "ptc.outcome";

/// How an activation ended.
pub(crate) fn activation_outcome(outcome: &ActivationOutcome) -> &'static str {
    match outcome {
        ActivationOutcome::Committed { .. } => "committed",
        ActivationOutcome::NothingToDo => "nothing_to_do",
        ActivationOutcome::Quarantined { .. } => "quarantined",
        ActivationOutcome::Failed(EngineError::Store(StoreError::Fenced { .. })) => "lease_lost",
        ActivationOutcome::Failed(_) => "failed",
    }
}

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

/// How one dispatch of a tool call ended.
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
    tracing::info!(histogram.ptc.activation.inbox = triggers as u64);
}

/// An activation of a started run found its instance in the cache, or not.
pub(crate) fn instance_cache_lookup(hit: bool) {
    let result = if hit { "hit" } else { "miss" };
    tracing::info!(
        monotonic_counter.ptc.instance_cache.lookups = 1_u64,
        ptc.result = result
    );
}

/// The cache dropped `evicted` instances.
pub(crate) fn instance_cache_evictions(evicted: usize) {
    if evicted > 0 {
        tracing::info!(monotonic_counter.ptc.instance_cache.evictions = evicted as u64);
    }
}

/// A run was quarantined.
pub(crate) fn quarantined(kind: QuarantineKind) {
    let reason = match kind {
        QuarantineKind::NonDeterminism => "non_determinism",
        QuarantineKind::Failures => "failures",
    };
    tracing::info!(
        monotonic_counter.ptc.runs.quarantined = 1_u64,
        ptc.reason = reason
    );
}

/// A transient failure of `tool` is dispatched again after a backoff.
pub(crate) fn tool_retry(tool: &str) {
    tracing::info!(
        monotonic_counter.ptc.tool_attempt.retries = 1_u64,
        ptc.tool = tool
    );
}

/// What a run's committed entries measure, projected once their feed item is acknowledged.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RunMeasurement {
    Started,
    Ended {
        outcome: &'static str,
    },
    ToolCall {
        tool: String,
        outcome: &'static str,
        error_kind: Option<&'static str>,
        seconds: f64,
    },
}

impl RunMeasurement {
    pub(crate) fn emit(&self) {
        match self {
            RunMeasurement::Started => {
                tracing::info!(monotonic_counter.ptc.runs.started = 1_u64);
            }
            RunMeasurement::Ended { outcome } => {
                tracing::info!(
                    monotonic_counter.ptc.runs.ended = 1_u64,
                    ptc.outcome = *outcome
                );
            }
            RunMeasurement::ToolCall {
                tool,
                outcome,
                error_kind,
                seconds,
            } => {
                tracing::info!(
                    histogram.ptc.tool_call.duration = *seconds,
                    ptc.tool = tool.as_str(),
                    ptc.outcome = *outcome,
                    ptc.error.kind = *error_kind,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ptc_core::{RunId, RunStatus, ToolFailure, ToolFailureKind};

    use super::*;
    use crate::ports::{ParkKind, ParkReason};

    #[test]
    fn a_lost_lease_is_its_own_outcome() {
        let fenced = EngineError::Store(StoreError::Fenced {
            run_id: RunId::from_u128(1),
            fence: 3,
        });
        let other = EngineError::Store(StoreError::Backend("down".into()));
        let committed = ActivationOutcome::Committed {
            entries: 2,
            status: RunStatus::Active,
        };
        let quarantined = ActivationOutcome::Quarantined {
            kind: QuarantineKind::Failures,
            reason: "x".into(),
        };
        let outcomes = [
            committed,
            ActivationOutcome::NothingToDo,
            quarantined,
            ActivationOutcome::Failed(fenced),
            ActivationOutcome::Failed(other),
        ]
        .map(|outcome| activation_outcome(&outcome));
        assert_eq!(
            outcomes,
            [
                "committed",
                "nothing_to_do",
                "quarantined",
                "lease_lost",
                "failed"
            ]
        );
    }

    #[test]
    fn every_answer_of_a_dispatcher_has_its_outcome() {
        let failure = ToolFailure {
            kind: ToolFailureKind::Error,
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
