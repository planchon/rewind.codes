//! The tool tasks' use cases (`archi/04-tools.md`, "Tool task lifecycle", "Policies",
//! "Credentials", "When a toolset changes during a run").
//!
//! Each `schedule_tool_call` an activation commits becomes a tool task in the store. Attempts,
//! backoff, claims, approvals and tokens live in the task; the journal only gets the final
//! outcome, as a completion in the run's inbox.
//!
//! | Who | Use cases |
//! | --- | --- |
//! | The engine's dispatcher loop: MCP tools and built-ins (every tool in mock mode) | `claim_tool_tasks`, `run_tool_task`, `heartbeat_tool_claims`, `expire_deferred_tool_tasks` |
//! | The harness's SDK worker, for host tools (`/v1/tasks`) | `claim_host_tasks`, `heartbeat_task`, `defer_task`, `complete_task`, `fail_task` |
//! | The worker role's scheduler, for host tools whose worker died | `fail_interrupted_host_tasks` |
//! | The harness, for a task that waits for it | `approve_task`, `reject_task`, `set_toolset_credentials`, `set_run_credentials` |
//!
//! ```text
//!              approve_task                 claim                 answered, failed for good
//!   approval ──────────────► ready ───────────────────► claimed ──────────────────────────► done
//!      │ reject_task          ▲ ▲ ▲                      │ │ │ │
//!      ▼                      │ │ │  transient failure   │ │ │ │ claim expired, claimed again:
//!    done                     │ │ └── (retry, backoff) ──┘ │ │ │ attempt + 1 up to 5 (retry),
//!                             │ │                          │ │ │ else done, outcome_unknown
//!          approve_task,      │ └──── parked ◄─────────────┘ │ └──► deferred ── complete, fail,
//!          renewed credentials│         │ schema changed, 401│        (token)    late ──► done
//!                             │         └── reject_task ──► done
//! ```
//!
//! - **Policies.** Under `retry`, a transient failure goes back to `ready` after a
//!   [`Backoff`], and a claim that expired is claimed again with the same idempotency key, up
//!   to [`EngineConfig::max_tool_attempts`](crate::EngineConfig) claims in all. Under
//!   `at_most_once`, a transient failure is delivered, and a claim that expired fails the call
//!   with `outcome_unknown` at once.
//! - **Claims and heartbeats.** A claim lasts the tool's heartbeat timeout. The harness's
//!   worker renews it with `heartbeat_task`; the engine renews the claims of the calls it is
//!   dispatching with `heartbeat_tool_claims`, until their execution timeout.
//! - **Credentials** are resolved through the `SecretStore` when an MCP call is dispatched (the
//!   run's, else the toolset's) and handed to the dispatcher only: never to the journal.
//! - **Parked tasks.** A schema change or a 401 parks the task and sends `tool.schema_changed`
//!   or `credentials.expired` to the tenant's subscriptions. Approved after a schema change, the
//!   call goes on against the new schema; renewed credentials make it ready again; rejected, it
//!   fails with a `ToolError`.
//! - **Tenants.** Every use case is scoped by the tenant: a token or a task of another tenant
//!   is treated as gone.

mod dispatch;
mod harness;
mod host;

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rewind_core::{Digest, Timestamp};

use crate::ports::TaskToken;

pub use host::{HostClaim, HostFailure, HostTask};

/// The delay before a call that failed transiently is attempted again: `initial ×
/// factor^(n − 1)` after the n-th attempt, at most `max`, of which the upper half is jitter.
///
/// The jitter is drawn from the call (`{run_id}/{seq}`) and its attempt, not from a random
/// generator: the engine stays deterministic given its ports, and calls that failed together
/// (a server outage) still come back apart. With the defaults, the second attempt comes 0.5 to
/// 1 s after the first, then 2 to 4 s, 8 to 16 s and 32 to 64 s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub factor: u32,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            factor: 4,
            max: Duration::from_secs(5 * 60),
        }
    }
}

impl Backoff {
    /// The delay after the attempt `attempt` (from 1) of the call `key` (`{run_id}/{seq}`)
    /// failed.
    pub fn delay(&self, key: &str, attempt: u32) -> Duration {
        let max = millis(self.max);
        let mut base = millis(self.initial).min(max);
        for _ in 1..attempt {
            if base >= max {
                break;
            }
            base = base.saturating_mul(u64::from(self.factor)).min(max);
        }
        let digest = Digest::of(format!("{key}#{attempt}").as_bytes());
        let mut draw = [0u8; 8];
        draw.copy_from_slice(&digest.as_bytes()[..8]);
        let draw = u128::from(u64::from_be_bytes(draw));
        let half = base / 2;
        let spread = u128::from(base - half);
        // `draw / 2^64` is uniform in [0, 1): the jitter is below `spread`.
        let jitter = u64::try_from((spread * draw) >> 64).unwrap_or(0);
        Duration::from_millis(half + jitter)
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// What the engine keeps about its own tool calls, between the store's calls: the claims of
/// the calls it is dispatching, which it heartbeats. When a retried call is due is the store's
/// (`next_tool_attempt`): an engine restarted during a backoff still wakes up for it.
#[derive(Debug, Default)]
pub(crate) struct ToolTasks {
    in_flight: Mutex<BTreeMap<TaskToken, InFlight>>,
}

/// While it lives, the engine renews the claim of a call being dispatched.
#[derive(Debug)]
struct Dispatching<'a> {
    tasks: &'a ToolTasks,
    token: &'a TaskToken,
}

impl Drop for Dispatching<'_> {
    fn drop(&mut self) {
        self.tasks.dispatched(self.token);
    }
}

/// A call being dispatched, under a claim the engine renews.
#[derive(Debug, Clone, Copy)]
struct InFlight {
    heartbeat_timeout: Duration,
    renewed_at: Timestamp,
    /// The end of its execution timeout: past it, the claim is no longer renewed.
    deadline: Option<Timestamp>,
}

impl ToolTasks {
    fn in_flight(&self) -> MutexGuard<'_, BTreeMap<TaskToken, InFlight>> {
        // The maps stay consistent whatever panicked while holding the lock.
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers a call being dispatched until the returned guard is dropped: when the
    /// dispatcher answers, or when the dispatch is abandoned (its future dropped).
    fn dispatching<'a>(&'a self, token: &'a TaskToken, call: InFlight) -> Dispatching<'a> {
        self.in_flight().insert(token.clone(), call);
        Dispatching { tasks: self, token }
    }

    fn dispatched(&self, token: &TaskToken) {
        self.in_flight().remove(token);
    }

    /// The claims to renew at `now`: a third of their heartbeat timeout has passed since their
    /// last renewal, and their execution timeout has not.
    fn due_heartbeats(&self, now: Timestamp) -> Vec<TaskToken> {
        self.in_flight()
            .iter()
            .filter(|(_, call)| {
                let renew_at = call.renewed_at.saturating_add(call.heartbeat_timeout / 3);
                renew_at <= now && call.deadline.is_none_or(|deadline| now < deadline)
            })
            .map(|(token, _)| token.clone())
            .collect()
    }

    fn renewed(&self, token: &TaskToken, now: Timestamp) {
        if let Some(call) = self.in_flight().get_mut(token) {
            call.renewed_at = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn backoff_grows_by_its_factor_with_its_jitter_in_the_upper_half() {
        let backoff = Backoff::default();
        let ranges = [
            (500, 1_000),
            (2_000, 4_000),
            (8_000, 16_000),
            (32_000, 64_000),
        ];
        for (attempt, (low, high)) in (1..).zip(ranges) {
            for key in ["run/1", "run/2", "other/7"] {
                let delay = millis(backoff.delay(key, attempt));
                assert!(
                    (low..high).contains(&delay),
                    "attempt {attempt} of {key}: {delay} ms"
                );
            }
        }
        assert!(
            millis(backoff.delay("run/1", 30)) < 300_000,
            "capped at max"
        );
        assert!(millis(backoff.delay("run/1", 30)) >= 150_000);
    }

    #[test]
    fn backoff_is_deterministic_and_spreads_calls() {
        let backoff = Backoff::default();
        assert_eq!(backoff.delay("run/1", 2), backoff.delay("run/1", 2));
        let delays: BTreeSet<Duration> = (1..20)
            .map(|seq| backoff.delay(&format!("run/{seq}"), 3))
            .collect();
        assert!(
            delays.len() > 15,
            "calls that failed together come back apart"
        );
    }

    #[test]
    fn claims_are_renewed_until_their_execution_deadline() {
        let tasks = ToolTasks::default();
        let token = TaskToken("t".into());
        let dispatching = tasks.dispatching(
            &token,
            InFlight {
                heartbeat_timeout: Duration::from_secs(60),
                renewed_at: Timestamp(0),
                deadline: Some(Timestamp(100_000)),
            },
        );
        assert!(tasks.due_heartbeats(Timestamp(19_999)).is_empty());
        assert_eq!(
            tasks.due_heartbeats(Timestamp(20_000)),
            std::slice::from_ref(&token)
        );
        tasks.renewed(&token, Timestamp(20_000));
        assert!(tasks.due_heartbeats(Timestamp(39_999)).is_empty());
        assert_eq!(
            tasks.due_heartbeats(Timestamp(40_000)),
            std::slice::from_ref(&token)
        );
        assert!(
            tasks.due_heartbeats(Timestamp(100_000)).is_empty(),
            "past its execution timeout, the claim is left to expire"
        );
        drop(dispatching);
        assert!(
            tasks.due_heartbeats(Timestamp(50_000)).is_empty(),
            "an answered or abandoned call is no longer renewed"
        );
    }
}
