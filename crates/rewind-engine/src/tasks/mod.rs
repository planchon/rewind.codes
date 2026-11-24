//! Call tasks (`archi/04-tools.md`, "Call lifecycle", "Policies", "Authenticating calls",
//! "When an action set changes during a run").
//!
//! Each `schedule_call` an activation commits becomes a call's task in the run's partition.
//! Its deliveries, backoff, approval, deferral and park live in the task; the journal only gets
//! the final outcome, as a completion in the run's inbox. The owner of the partition keeps the
//! lifecycle (`partition::tasks`), the call executor of the node sends the calls ([`calls`]).
//!
//! ```text
//!              approve_task                sent, or handed                answered, failed for good
//!   approval ──────────────► ready ───────────────────► in flight ──────────────────────────► done
//!      │ reject_task          ▲ ▲ ▲                      │ │ │ │
//!      ▼                      │ │ │  transient failure   │ │ │ │ interrupted (a node died, a
//!    done                     │ │ └── (retry, backoff) ──┘ │ │ │ stream gone): delivery + 1 up to
//!                             │ │                          │ │ │ 5 (idempotent), else outcome_unknown
//!          approve_task,      │ └──── parked ◄─────────────┘ │ └──► deferred ── complete, fail,
//!          a secret put again │         │ schema changed, 401│        (token)    late ──► done
//!                             │         │ secret missing     │
//!                             │         └── reject_task ──► done
//! ```
//!
//! - **Policies.** When the action is idempotent, a transient failure goes back to `ready` after a
//!   [`Backoff`], and an interrupted call is sent again with the same idempotency key, up to the
//!   node's maximum of deliveries. When it is not, a transient failure is delivered, and an
//!   interrupted call fails with `outcome_unknown`.
//! - **Authentication** is resolved when an MCP or API call is sent, from its server's `auth`:
//!   the secret the run attached, read from the `SecretStore` in the run's namespace, or the
//!   tenant's signing key. It is handed to the dispatcher only ([`CallAuth`]), and its texts are
//!   taken out of the answer ([`redacted`]): never in the journal.
//! - **Parked tasks.** A schema change, a 401 or a missing secret parks the task and sends
//!   `action.schema_changed` or `credentials.expired` (reason `refused` or `missing`) to the
//!   tenant's subscriptions. Approved after a schema change, the call goes on against the new
//!   schema; the secret put again makes it ready again; rejected, it fails with an `ActionError`.
//!
//! [`CallAuth`]: crate::ports::CallAuth

pub(crate) mod calls;

use std::time::Duration;

use rewind_core::{ActionFailure, Digest, JsonValue, RunId, SecretEncoding, SecretPlacement, Seq};

use crate::ports::{CallAuth, Dispatched};

/// `"{run_id}/{seq}"`, the key every delivery of a call carries.
pub fn idempotency_key(run_id: RunId, seq: Seq) -> String {
    format!("{run_id}/{}", seq.0)
}

/// The delay before a call that failed transiently is attempted again: `initial ×
/// factor^(n − 1)` after the n-th delivery, at most `max`, of which the upper half is jitter.
///
/// The jitter is drawn from the call (`{run_id}/{seq}`) and its delivery, not from a random
/// generator: the engine stays deterministic given its ports, and calls that failed together
/// (a server outage) still come back apart. With the defaults, the second delivery comes 0.5 to
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
    /// The delay after the delivery `delivery` (from 1) of the call `key` (`{run_id}/{seq}`)
    /// failed.
    pub fn delay(&self, key: &str, delivery: u32) -> Duration {
        let max = millis(self.max);
        let mut base = millis(self.initial).min(max);
        for _ in 1..delivery {
            if base >= max {
                break;
            }
            base = base.saturating_mul(u64::from(self.factor)).min(max);
        }
        let digest = Digest::of(format!("{key}#{delivery}").as_bytes());
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

/// The shortest secret whose value is taken out of answers: a shorter one would match ordinary
/// text.
const MIN_REDACTED_SECRET: usize = 4;

/// What replaces a secret's value in an answer.
const REDACTED: &str = "[secret]";

/// The texts of the attached secret an answer must not carry: its value, and its base64 form
/// when its header encodes it (HTTP basic).
pub(crate) fn secret_texts(auth: &CallAuth) -> Vec<String> {
    let CallAuth::Secret { value, placement } = auth else {
        return Vec::new();
    };
    let value = value.expose();
    if value.len() < MIN_REDACTED_SECRET {
        return Vec::new();
    }
    let mut texts = vec![value.to_owned()];
    if let SecretPlacement::Header {
        encoding: SecretEncoding::Base64,
        ..
    } = placement
    {
        use base64::Engine as _;
        texts.push(base64::engine::general_purpose::STANDARD.encode(value));
    }
    texts
}

/// `dispatched` without the attached secret's texts: a server that echoes its credential (an
/// error quoting the key, a debug endpoint) must not bring it into the journal and the script
/// (`archi/04-tools.md`, "Secrets": secrets never enter the journal nor the isolate).
pub(crate) fn redacted(dispatched: Dispatched, texts: &[String]) -> Dispatched {
    if texts.is_empty() {
        return dispatched;
    }
    let failure = |mut failure: ActionFailure| {
        failure.message = redacted_text(failure.message, texts);
        failure.data = failure.data.map(|data| redacted_json(data, texts));
        failure
    };
    match dispatched {
        Dispatched::Result(value) => Dispatched::Result(redacted_json(value, texts)),
        Dispatched::Failed(failed) => Dispatched::Failed(failure(failed)),
        Dispatched::Transient(failed) => Dispatched::Transient(failure(failed)),
        Dispatched::Park(mut reason) => {
            reason.message = redacted_text(reason.message, texts);
            Dispatched::Park(reason)
        }
        Dispatched::Deferred => Dispatched::Deferred,
    }
}

fn redacted_text(text: String, texts: &[String]) -> String {
    if texts.iter().any(|secret| text.contains(secret.as_str())) {
        texts
            .iter()
            .fold(text, |text, secret| text.replace(secret.as_str(), REDACTED))
    } else {
        text
    }
}

fn redacted_json(value: JsonValue, texts: &[String]) -> JsonValue {
    match value {
        JsonValue::String(text) => JsonValue::String(redacted_text(text, texts)),
        JsonValue::Array(items) => JsonValue::Array(
            items
                .into_iter()
                .map(|item| redacted_json(item, texts))
                .collect(),
        ),
        JsonValue::Object(fields) => JsonValue::Object(
            fields
                .into_iter()
                .map(|(key, value)| (redacted_text(key, texts), redacted_json(value, texts)))
                .collect(),
        ),
        other => other,
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
        for (delivery, (low, high)) in (1..).zip(ranges) {
            for key in ["run/1", "run/2", "other/7"] {
                let delay = millis(backoff.delay(key, delivery));
                assert!(
                    (low..high).contains(&delay),
                    "delivery {delivery} of {key}: {delay} ms"
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
}
