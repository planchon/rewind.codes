//! Driving runs until nothing more can happen (`run_until_idle`), in real or virtual time.
//!
//! This is the loop of an embedded engine (`ptc run`, tests, mock mode): expire runs, fire
//! timers, run tool calls, activate runs, and repeat while anything progressed. In virtual time,
//! when nothing is ready, the clock jumps to the next timer or deadline, so a 10-day sleep takes
//! no wall-clock time. The projector and the retention jobs are driven by the composition root,
//! with their own adapters.

use ptc_core::Timestamp;

use crate::engine::{ActivationOutcome, Engine, EngineError};
use crate::ports::{
    BlobStore, Clock, Entropy, JournalStore, RunRef, ScriptExecutor, ToolDispatcher, VirtualTime,
};

/// The order in which claimed tool calls are run, and so the order of their completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolOrder {
    /// In the order they were scheduled.
    #[default]
    Scheduled,
    /// Latest first: exercises out-of-order completions under `Promise.all`.
    Reversed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveOptions {
    pub tool_order: ToolOrder,
    /// Tool tasks claimed per pass.
    pub tool_batch: usize,
    /// Passes before giving up: a guard against scripts that never stop (an endless
    /// `setInterval` in virtual time).
    pub max_passes: usize,
}

impl Default for DriveOptions {
    fn default() -> Self {
        Self {
            tool_order: ToolOrder::Scheduled,
            tool_batch: 1_000,
            max_passes: 100_000,
        }
    }
}

/// What a drive did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriveReport {
    pub activations: usize,
    pub tool_calls: usize,
    pub timers_fired: usize,
    pub expired: usize,
    pub quarantined: usize,
    /// Activations that failed: those runs are activated again once their lease expires.
    pub failures: Vec<(RunRef, EngineError)>,
    /// `max_passes` was reached while things were still happening.
    pub interrupted: bool,
}

impl<J, B, X, D, C, E> Engine<J, B, X, D, C, E>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: Clock,
    E: Entropy,
{
    /// Does everything that is ready now, repeatedly, until nothing progresses.
    pub async fn run_until_idle(&self, options: &DriveOptions) -> Result<DriveReport, EngineError> {
        let mut report = DriveReport::default();
        for _ in 0..options.max_passes {
            if !self.pass(options, &mut report).await? {
                return Ok(report);
            }
        }
        report.interrupted = true;
        Ok(report)
    }

    /// One pass. Returns whether anything progressed.
    async fn pass(
        &self,
        options: &DriveOptions,
        report: &mut DriveReport,
    ) -> Result<bool, EngineError> {
        let mut progressed = false;

        let expired = self.expire_due_runs().await?;
        report.expired += expired;
        progressed |= expired > 0;

        let fired = self.fire_due_timers().await?;
        report.timers_fired += fired;
        progressed |= fired > 0;

        let mut tasks = self.claim_tool_tasks(options.tool_batch).await?;
        if options.tool_order == ToolOrder::Reversed {
            tasks.reverse();
        }
        report.tool_calls += tasks.len();
        progressed |= !tasks.is_empty();
        for task in tasks {
            self.run_tool_task(task).await?;
        }

        while let Some(activation) = self.activate_next().await? {
            progressed = true;
            report.activations += 1;
            match activation.outcome {
                ActivationOutcome::Quarantined { .. } => report.quarantined += 1,
                ActivationOutcome::Failed(error) => report.failures.push((activation.run, error)),
                ActivationOutcome::Committed { .. } | ActivationOutcome::NothingToDo => {}
            }
        }
        Ok(progressed)
    }
}

impl<J, B, X, D, C, E> Engine<J, B, X, D, C, E>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: VirtualTime,
    E: Entropy,
{
    /// [`run_until_idle`](Self::run_until_idle) in virtual time: when nothing is ready but a
    /// timer is pending, the clock jumps to the next timer, or to an earlier deadline. Returns
    /// when only the outside world (a signal, a cancellation) can wake a run.
    pub async fn run_until_idle_virtual(
        &self,
        options: &DriveOptions,
    ) -> Result<DriveReport, EngineError> {
        let mut report = DriveReport::default();
        for _ in 0..options.max_passes {
            if self.pass(options, &mut report).await? {
                continue;
            }
            let Some(next) = self.next_wake_up().await? else {
                return Ok(report);
            };
            self.clock.advance_to(next);
        }
        report.interrupted = true;
        Ok(report)
    }

    /// [`run_until_idle_virtual`](Self::run_until_idle_virtual), without moving the clock past
    /// `until`: once nothing is ready before it, the clock is moved to `until` and the drive
    /// returns. A scenario sends a signal at a given time this way.
    pub async fn run_virtual_until(
        &self,
        options: &DriveOptions,
        until: Timestamp,
    ) -> Result<DriveReport, EngineError> {
        let mut report = DriveReport::default();
        for _ in 0..options.max_passes {
            if self.pass(options, &mut report).await? {
                continue;
            }
            match self.next_wake_up().await? {
                Some(next) if next <= until => self.clock.advance_to(next),
                _ => {
                    self.clock.advance_to(until);
                    return Ok(report);
                }
            }
        }
        report.interrupted = true;
        Ok(report)
    }

    /// The next timer, or the next deadline if it comes first. A deadline alone does not wake
    /// anything up: a run waiting for a signal is waiting for the outside world.
    async fn next_wake_up(&self) -> Result<Option<Timestamp>, EngineError> {
        let Some(timer) = self.journal.next_timer().await? else {
            return Ok(None);
        };
        let deadline = self.journal.next_deadline().await?;
        Ok(Some(deadline.map_or(timer, |deadline| deadline.min(timer))))
    }
}
