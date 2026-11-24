//! The load's report: throughput, latencies, errors and the invariants that broke.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::Serialize;

/// Percentiles of a set of durations, in milliseconds (nearest rank).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub(crate) struct Percentiles {
    pub(crate) count: usize,
    pub(crate) p50: f64,
    pub(crate) p90: f64,
    pub(crate) p99: f64,
    pub(crate) max: f64,
}

impl Percentiles {
    pub(crate) fn of(samples: &[Duration]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let rank = |p: f64| {
            // Nearest rank: the smallest sample with at least p of the samples at or below it.
            let index = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1;
            millis(sorted[index])
        };
        Self {
            count: sorted.len(),
            p50: rank(0.50),
            p90: rank(0.90),
            p99: rank(0.99),
            max: millis(sorted[sorted.len() - 1]),
        }
    }
}

fn millis(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1_000_000.0).round() / 1_000.0
}

/// What the load measured.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Report {
    pub(crate) runs: usize,
    pub(crate) concurrency: usize,
    /// The padding of each run's payloads, in bytes.
    pub(crate) payload_bytes: usize,
    /// From the first start to the last end, in seconds.
    pub(crate) elapsed_s: f64,
    /// Runs that ended, per second of `elapsed_s`.
    pub(crate) runs_per_s: f64,
    /// Calls that reached the fake MCP server or the host-tool worker, per second.
    pub(crate) calls_per_s: f64,
    /// Runs by final status (`completed`, `failed`…, or `unfinished`).
    pub(crate) statuses: BTreeMap<String, usize>,
    pub(crate) latency_ms: Latencies,
    /// Requests that failed, by kind.
    pub(crate) errors: BTreeMap<String, usize>,
    /// Broken invariants, one line each (the first ones only).
    pub(crate) violations: Vec<String>,
    pub(crate) violation_count: usize,
    /// The calls of this load, as the server's counters count them (`GET /v1/actions/stats`).
    pub(crate) server_action_stats: Vec<ActionStatsLine>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Latencies {
    /// `POST /v1/runs`, type-check included.
    pub(crate) start: Percentiles,
    /// From the start's answer to the run's first call at the MCP server: the first
    /// activation.
    pub(crate) first_call: Percentiles,
    /// From a host tool's result being accepted to the run's next call at the MCP server:
    /// one activation and the dispatch of its call.
    pub(crate) activation: Percentiles,
    /// From the start's request to the run's end as observed through the API.
    pub(crate) end_to_end: Percentiles,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ActionStatsLine {
    pub(crate) action: String,
    pub(crate) calls: u64,
    pub(crate) errors: u64,
    pub(crate) retries: u64,
    pub(crate) p50_ms: u64,
    pub(crate) p99_ms: u64,
}

/// The broken invariants kept in the report; all of them are counted.
const VIOLATIONS_SHOWN: usize = 20;

impl Report {
    pub(crate) fn violation(&mut self, message: String) {
        self.violation_count += 1;
        if self.violations.len() < VIOLATIONS_SHOWN {
            self.violations.push(message);
        }
    }

    pub(crate) fn error(&mut self, kind: impl Into<String>) {
        *self.errors.entry(kind.into()).or_default() += 1;
    }

    /// Every run completed as expected, and no invariant broke.
    pub(crate) fn passed(&self) -> bool {
        self.violation_count == 0 && self.statuses.get("completed") == Some(&self.runs)
    }
}

impl fmt::Display for Percentiles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "p50 {:>8.1}  p90 {:>8.1}  p99 {:>8.1}  max {:>8.1}  (n={})",
            self.p50, self.p90, self.p99, self.max, self.count
        )
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} runs, {} at once, payloads of {} bytes, in {:.1} s: {:.1} runs/s, {:.1} action \
             calls/s",
            self.runs,
            self.concurrency,
            self.payload_bytes,
            self.elapsed_s,
            self.runs_per_s,
            self.calls_per_s
        )?;
        let statuses: Vec<String> = self
            .statuses
            .iter()
            .map(|(status, count)| format!("{status} {count}"))
            .collect();
        writeln!(f, "statuses: {}", statuses.join(", "))?;
        writeln!(f, "latency (ms)")?;
        writeln!(f, "  start       {}", self.latency_ms.start)?;
        writeln!(f, "  first call  {}", self.latency_ms.first_call)?;
        writeln!(f, "  activation  {}", self.latency_ms.activation)?;
        writeln!(f, "  end to end  {}", self.latency_ms.end_to_end)?;
        if self.errors.is_empty() {
            writeln!(f, "errors: none")?;
        } else {
            let errors: Vec<String> = self
                .errors
                .iter()
                .map(|(kind, count)| format!("{kind} {count}"))
                .collect();
            writeln!(f, "errors: {}", errors.join(", "))?;
        }
        if !self.server_action_stats.is_empty() {
            writeln!(f, "server action stats")?;
            for line in &self.server_action_stats {
                writeln!(
                    f,
                    "  {:<12} calls {:>6}  errors {:>4}  retries {:>5}  p50 {:>6} ms  p99 {:>6} ms",
                    line.action, line.calls, line.errors, line.retries, line.p50_ms, line.p99_ms
                )?;
            }
        }
        if self.violation_count == 0 {
            write!(f, "invariants: held")
        } else {
            writeln!(f, "invariants: {} broken", self.violation_count)?;
            for violation in &self.violations {
                writeln!(f, "  {violation}")?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_take_the_nearest_rank() {
        let samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let p = Percentiles::of(&samples);
        assert_eq!(p.count, 100);
        assert_eq!(p.p50, 50.0);
        assert_eq!(p.p90, 90.0);
        assert_eq!(p.p99, 99.0);
        assert_eq!(p.max, 100.0);
        assert_eq!(Percentiles::of(&[]), Percentiles::default());
        let one = Percentiles::of(&[Duration::from_micros(1500)]);
        assert_eq!((one.p50, one.max), (1.5, 1.5));
    }

    #[test]
    fn a_report_passes_only_when_every_run_completed_and_nothing_broke() {
        let mut report = Report {
            runs: 2,
            ..Report::default()
        };
        report.statuses.insert("completed".into(), 2);
        assert!(report.passed());
        report.violation("echo called twice".into());
        assert!(!report.passed());
        let mut short = Report {
            runs: 2,
            ..Report::default()
        };
        short.statuses.insert("completed".into(), 1);
        short.statuses.insert("failed".into(), 1);
        assert!(!short.passed());
    }
}
