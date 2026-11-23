//! The key layout of a partition (`archi/06-storage-and-scale.md`, "A partition"): which record
//! lives under which key, so that every read of a `PartitionLog` is one point get or one range
//! scan. The engine never sees a key.
//!
//! A key is a family byte, then its parts: a string is its UTF-8 bytes with `00` written `00 ff`,
//! closed by `00 01` (so keys sort as their strings do, and a string is never the prefix of
//! another's key); a run id is its 16 bytes; numbers are big-endian; a part written `!x` is the
//! bitwise complement of `x`, for a descending order. `t` is a tenant, `n` a namespace, `r` a run
//! id.
//!
//! | Key | Value | Read by |
//! | --- | --- | --- |
//! | `h r` | run head | `run`, `runs_in_range` (a range of ids); run ids are unique across tenants, so a creation checks it |
//! | `m t r` | run summary | `summary` |
//! | `e t r attempt:u32 index:u64` | history entry | `history` (a range from `index`) |
//! | `i t r id:u64` | inbox item | `inbox` (a range); the recovery's runnable runs (the family) |
//! | `k t r key` | signal key | `signal_key` |
//! | `j t r seq:u32` | tool task | `tasks` (a range); the recovery's tasks (the family) |
//! | `p t r digest:32` | payload | `payload` |
//! | `w t r watcher:16` | watcher | `watchers` (a range) |
//! | `b t r hash:32` | empty | the run's webhook tokens, removed at its end |
//! | `o t r id:u64` | empty | the run's outbox entries, removed at its erasure |
//! | `y t r key` | empty | the run's start key, removed at its erasure |
//! | `L r` | run head | the runs that have not ended: the recovery's runnable and quarantined runs |
//! | `U r` | run head | the ended attempts not sealed: the recovery's `unsealed` |
//! | `t kind` | timer | a timer by its kind: setting it again finds the due time to move |
//! | `T at:u64 kind` | timer | `timers(from, to)` (a range), in their order |
//! | `W hash:32` | webhook token | `webhook` |
//! | `K t n key` | start key | `started` |
//! | `O id:u64` | outbox entry | the recovery's outbox (the family) |
//! | `G t n` | gauges | `gauges` (a point, or the tenant's range); the recovery's counters |
//! | `N t n` | empty | the namespaces with hourly counters: the recovery's counters |
//! | `C t n hour:u32` | counters | `counters` of a namespace (a range of hours) |
//! | `D t hour:u32 n` | counters | `counters` of a tenant (a range of hours) |
//! | `X t n tool` | shape | `shapes` (the namespace's range, or the tenant's) |
//! | `A t` | organization state | `organization`; the recovery's organizations (the family) |
//! | `Q t 00 order position` | run summary | `search` over the tenant |
//! | `Q t 01 n order position` | run summary | `search` over a namespace |
//!
//! A timer's `kind` is `t (00 | 01 r) rank:u8 detail:u64`: its tenant, its run if it has one,
//! the rank of its kind and its `seq` or outbox entry, the order of timers due together. An index
//! position follows the order of [`RunOrder`]: the run id for `started` (read backwards) and
//! `oldest`; for the others a flag (`01` when the value is missing) and the value, then the run
//! id: `activity` and `calls` as `!value !run`, `wakes` as `value run`, `duration` as
//! `!value !run`.

use rewind_core::{Digest, RunId, Seq};
use rewind_engine::ports::{
    Attempt, Hour, NamespaceName, OutboxId, RunOrder, RunRef, RunSummary, TenantId, TimerKind,
};

pub(crate) const HEAD: u8 = b'h';
pub(crate) const SUMMARY: u8 = b'm';
pub(crate) const HISTORY: u8 = b'e';
pub(crate) const INBOX: u8 = b'i';
pub(crate) const SIGNAL_KEY: u8 = b'k';
pub(crate) const TASK: u8 = b'j';
pub(crate) const PAYLOAD: u8 = b'p';
pub(crate) const WATCHER: u8 = b'w';
pub(crate) const RUN_WEBHOOK: u8 = b'b';
pub(crate) const RUN_OUTBOX: u8 = b'o';
pub(crate) const RUN_START_KEY: u8 = b'y';
pub(crate) const LIVE: u8 = b'L';
pub(crate) const UNSEALED: u8 = b'U';
pub(crate) const TIMER: u8 = b't';
pub(crate) const DUE: u8 = b'T';
pub(crate) const WEBHOOK: u8 = b'W';
pub(crate) const START_KEY: u8 = b'K';
pub(crate) const OUTBOX: u8 = b'O';
pub(crate) const GAUGES: u8 = b'G';
pub(crate) const COUNTED: u8 = b'N';
pub(crate) const HOUR: u8 = b'C';
pub(crate) const TENANT_HOUR: u8 = b'D';
pub(crate) const SHAPE: u8 = b'X';
pub(crate) const ORGANIZATION: u8 = b'A';
pub(crate) const INDEX: u8 = b'Q';

/// The families of keys a run owns, under `family t r`: what an erasure deletes.
pub(crate) const RUN_FAMILIES: [u8; 10] = [
    SUMMARY,
    HISTORY,
    INBOX,
    SIGNAL_KEY,
    TASK,
    PAYLOAD,
    WATCHER,
    RUN_WEBHOOK,
    RUN_OUTBOX,
    RUN_START_KEY,
];

/// A key being written, part by part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key(Vec<u8>);

impl Key {
    pub(crate) fn new(family: u8) -> Self {
        Self(vec![family])
    }

    pub(crate) fn str(mut self, text: &str) -> Self {
        for byte in text.bytes() {
            if byte == 0 {
                self.0.extend([0, 0xff]);
            } else {
                self.0.push(byte);
            }
        }
        self.0.extend([0, 1]);
        self
    }

    pub(crate) fn run(mut self, run: RunId) -> Self {
        self.0.extend(run_bytes(run));
        self
    }

    pub(crate) fn byte(mut self, byte: u8) -> Self {
        self.0.push(byte);
        self
    }

    pub(crate) fn u32(mut self, value: u32) -> Self {
        self.0.extend(value.to_be_bytes());
        self
    }

    pub(crate) fn u64(mut self, value: u64) -> Self {
        self.0.extend(value.to_be_bytes());
        self
    }

    pub(crate) fn bytes(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

pub(crate) fn run_bytes(run: RunId) -> [u8; 16] {
    run.as_uuid().as_u128().to_be_bytes()
}

pub(crate) fn run_from(bytes: &[u8]) -> Option<RunId> {
    let bytes: [u8; 16] = bytes.get(..16)?.try_into().ok()?;
    Some(RunId::from_u128(u128::from_be_bytes(bytes)))
}

/// Reads a string part at the start of `bytes`: the string and what follows it.
pub(crate) fn read_str(bytes: &[u8]) -> Option<(String, &[u8])> {
    let mut text = Vec::new();
    let mut at = 0;
    loop {
        match (bytes.get(at)?, bytes.get(at + 1)) {
            (0, Some(1)) => break,
            (0, Some(0xff)) => {
                text.push(0);
                at += 2;
            }
            (0, _) => return None,
            (byte, _) => {
                text.push(*byte);
                at += 1;
            }
        }
    }
    Some((String::from_utf8(text).ok()?, &bytes[at + 2..]))
}

/// `family t r`: the prefix of what a run owns in a family.
pub(crate) fn of_run(family: u8, run: &RunRef) -> Key {
    Key::new(family).str(run.tenant.as_str()).run(run.id)
}

pub(crate) fn head(run: RunId) -> Key {
    Key::new(HEAD).run(run)
}

pub(crate) fn summary(run: &RunRef) -> Key {
    of_run(SUMMARY, run)
}

pub(crate) fn history_of(run: &RunRef, attempt: Attempt) -> Key {
    of_run(HISTORY, run).u32(attempt.0)
}

pub(crate) fn history(run: &RunRef, attempt: Attempt, index: u64) -> Key {
    history_of(run, attempt).u64(index)
}

pub(crate) fn inbox(run: &RunRef, id: u64) -> Key {
    of_run(INBOX, run).u64(id)
}

pub(crate) fn signal_key(run: &RunRef, key: &str) -> Key {
    of_run(SIGNAL_KEY, run).str(key)
}

pub(crate) fn task(run: &RunRef, seq: Seq) -> Key {
    of_run(TASK, run).u32(seq.0)
}

pub(crate) fn payload(run: &RunRef, digest: Digest) -> Key {
    of_run(PAYLOAD, run).bytes(digest.as_bytes())
}

pub(crate) fn watcher(run: &RunRef, watcher: RunId) -> Key {
    of_run(WATCHER, run).run(watcher)
}

pub(crate) fn run_webhook(run: &RunRef, hash: Digest) -> Key {
    of_run(RUN_WEBHOOK, run).bytes(hash.as_bytes())
}

pub(crate) fn run_outbox(run: &RunRef, id: OutboxId) -> Key {
    of_run(RUN_OUTBOX, run).u64(id.0)
}

pub(crate) fn run_start_key(run: &RunRef, key: &str) -> Key {
    of_run(RUN_START_KEY, run).str(key)
}

pub(crate) fn live(run: RunId) -> Key {
    Key::new(LIVE).run(run)
}

pub(crate) fn unsealed(run: RunId) -> Key {
    Key::new(UNSEALED).run(run)
}

/// The rank of a kind among the timers of a run due together: [`TimerKind`]'s order.
fn rank(kind: &TimerKind) -> u8 {
    match kind {
        TimerKind::Script { .. } => 0,
        TimerKind::CallTimeout { .. } => 1,
        TimerKind::DeferredDeadline { .. } => 2,
        TimerKind::RetryBackoff { .. } => 3,
        TimerKind::RunDeadline { .. } => 4,
        TimerKind::ActivationRetry { .. } => 5,
        TimerKind::RetentionEnd { .. } => 6,
        TimerKind::WebhookRetry { .. } => 7,
    }
}

fn timer_kind(key: Key, kind: &TimerKind) -> Key {
    let key = key.str(kind.tenant().as_str());
    let key = match kind.run() {
        Some(run) => key.byte(1).run(run.id),
        None => key.byte(0),
    };
    let detail = match kind {
        TimerKind::WebhookRetry { entry, .. } => entry.0,
        other => other.seq().map_or(0, |seq| u64::from(seq.0)),
    };
    key.byte(rank(kind)).u64(detail)
}

pub(crate) fn timer(kind: &TimerKind) -> Key {
    timer_kind(Key::new(TIMER), kind)
}

/// The prefix of a run's timers by kind.
pub(crate) fn timers_of(run: &RunRef) -> Key {
    Key::new(TIMER).str(run.tenant.as_str()).byte(1).run(run.id)
}

pub(crate) fn due(at: u64, kind: &TimerKind) -> Key {
    timer_kind(Key::new(DUE).u64(at), kind)
}

pub(crate) fn due_from(at: u64) -> Key {
    Key::new(DUE).u64(at)
}

pub(crate) fn webhook(hash: Digest) -> Key {
    Key::new(WEBHOOK).bytes(hash.as_bytes())
}

pub(crate) fn start_keys_of(tenant: &TenantId, namespace: &NamespaceName) -> Key {
    Key::new(START_KEY)
        .str(tenant.as_str())
        .str(namespace.as_str())
}

pub(crate) fn start_key(tenant: &TenantId, namespace: &NamespaceName, key: &str) -> Key {
    start_keys_of(tenant, namespace).str(key)
}

pub(crate) fn outbox(id: OutboxId) -> Key {
    Key::new(OUTBOX).u64(id.0)
}

pub(crate) fn gauges_of(tenant: &TenantId) -> Key {
    Key::new(GAUGES).str(tenant.as_str())
}

pub(crate) fn gauges(tenant: &TenantId, namespace: &NamespaceName) -> Key {
    gauges_of(tenant).str(namespace.as_str())
}

pub(crate) fn counted(tenant: &TenantId, namespace: &NamespaceName) -> Key {
    Key::new(COUNTED)
        .str(tenant.as_str())
        .str(namespace.as_str())
}

pub(crate) fn hours_of(tenant: &TenantId, namespace: &NamespaceName) -> Key {
    Key::new(HOUR).str(tenant.as_str()).str(namespace.as_str())
}

pub(crate) fn hour(tenant: &TenantId, namespace: &NamespaceName, hour: Hour) -> Key {
    hours_of(tenant, namespace).u32(hour.0)
}

pub(crate) fn tenant_hours_of(tenant: &TenantId) -> Key {
    Key::new(TENANT_HOUR).str(tenant.as_str())
}

pub(crate) fn tenant_hour(tenant: &TenantId, hour: Hour, namespace: &NamespaceName) -> Key {
    tenant_hours_of(tenant).u32(hour.0).str(namespace.as_str())
}

pub(crate) fn shapes_of(tenant: &TenantId, namespace: Option<&NamespaceName>) -> Key {
    let key = Key::new(SHAPE).str(tenant.as_str());
    match namespace {
        Some(namespace) => key.str(namespace.as_str()),
        None => key,
    }
}

pub(crate) fn shape(tenant: &TenantId, namespace: &NamespaceName, tool: &str) -> Key {
    shapes_of(tenant, Some(namespace)).str(tool)
}

pub(crate) fn organization(tenant: &TenantId) -> Key {
    Key::new(ORGANIZATION).str(tenant.as_str())
}

/// The index families, one per order but `started`, which reads `oldest` backwards.
const FAMILIES: [RunOrder; 5] = [
    RunOrder::Oldest,
    RunOrder::Activity,
    RunOrder::Wakes,
    RunOrder::Duration,
    RunOrder::Calls,
];

fn family(order: RunOrder) -> u8 {
    match order {
        RunOrder::Started | RunOrder::Oldest => 0,
        RunOrder::Activity => 1,
        RunOrder::Wakes => 2,
        RunOrder::Duration => 3,
        RunOrder::Calls => 4,
    }
}

/// `Q t 00` or `Q t 01 n`: the index of a tenant, or of one of its namespaces.
pub(crate) fn index_of(tenant: &TenantId, namespace: Option<&NamespaceName>) -> Key {
    let key = Key::new(INDEX).str(tenant.as_str());
    match namespace {
        Some(namespace) => key.byte(1).str(namespace.as_str()),
        None => key.byte(0),
    }
}

/// The prefix of an order's entries in an index.
pub(crate) fn index_family(index: Key, order: RunOrder) -> Key {
    index.byte(family(order))
}

/// The position of `(key, run)` in `order`, after its family's prefix: bytes in the order's
/// ascending order (`started` is read backwards).
pub(crate) fn position(order: RunOrder, key: Option<u64>, run: RunId) -> Vec<u8> {
    let run = run_bytes(run);
    let complement = |bytes: [u8; 16]| bytes.map(|byte| !byte);
    let flag = u8::from(key.is_none());
    let value = key.unwrap_or(0);
    let mut out = Vec::with_capacity(25);
    match order {
        RunOrder::Started | RunOrder::Oldest => out.extend(run),
        RunOrder::Activity | RunOrder::Calls | RunOrder::Duration => {
            out.push(flag);
            out.extend((!value).to_be_bytes());
            out.extend(complement(run));
        }
        RunOrder::Wakes => {
            out.push(flag);
            out.extend(value.to_be_bytes());
            out.extend(run);
        }
    }
    out
}

/// The index entries of a summary: in the tenant's index and its namespace's, one per family.
pub(crate) fn index_entries(summary: &RunSummary) -> Vec<Key> {
    let tenant = &summary.run.tenant;
    let mut keys = Vec::with_capacity(2 * FAMILIES.len());
    for index in [
        index_of(tenant, None),
        index_of(tenant, Some(&summary.namespace)),
    ] {
        for order in FAMILIES {
            let cursor = summary.cursor(order);
            keys.push(
                index_family(index.clone(), order).bytes(&position(order, cursor.key, cursor.run)),
            );
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use rewind_core::{PartitionId, Timestamp};
    use rewind_engine::ports::{DueTimer, SearchCursor, SearchPosition};

    use super::*;

    fn tenant(name: &str) -> TenantId {
        TenantId::new(name).unwrap()
    }

    fn run(n: u64) -> RunRef {
        RunRef::new(
            tenant("acme"),
            RunId::in_partition(PartitionId::new(3), Timestamp(1_000 + n), n),
        )
    }

    #[test]
    fn strings_sort_as_their_text_and_read_back() {
        let texts = ["", "a", "a\0", "a\0b", "ab", "b", "é"];
        let keys: Vec<Vec<u8>> = texts
            .iter()
            .map(|text| Key::new(b'x').str(text).u32(7).into_bytes())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        for (text, key) in texts.iter().zip(&keys) {
            let (read, rest) = read_str(&key[1..]).unwrap();
            assert_eq!((read.as_str(), rest), (*text, &7u32.to_be_bytes()[..]));
        }
        assert!(read_str(b"a\0").is_none());
        assert!(read_str(b"a\0\x07").is_none());
    }

    #[test]
    fn run_ids_read_back() {
        let id = run(7).id;
        assert_eq!(run_from(&run_bytes(id)), Some(id));
        assert_eq!(run_from(&[1, 2]), None);
    }

    #[test]
    fn due_timers_sort_as_the_port_orders_them() {
        let (r1, r2) = (run(1), run(2));
        let other = RunRef::new(tenant("other"), run(3).id);
        let mut timers = vec![];
        for at in [5, 10] {
            for kind in [
                TimerKind::RetentionEnd { run: r2.clone() },
                TimerKind::Script {
                    run: r1.clone(),
                    seq: Seq(2),
                },
                TimerKind::RunDeadline { run: r1.clone() },
                TimerKind::Script {
                    run: r1.clone(),
                    seq: Seq(1),
                },
                TimerKind::WebhookRetry {
                    tenant: tenant("acme"),
                    entry: OutboxId(4),
                },
                TimerKind::CallTimeout {
                    run: other.clone(),
                    seq: Seq(1),
                },
                TimerKind::Script {
                    run: r2.clone(),
                    seq: Seq(300),
                },
            ] {
                timers.push(DueTimer {
                    at: Timestamp(at),
                    kind,
                });
            }
        }
        let mut by_key = timers.clone();
        by_key.sort_by_key(|timer| due(timer.at.0, &timer.kind).into_bytes());
        timers.sort();
        assert_eq!(by_key, timers);
    }

    #[test]
    fn index_positions_sort_as_the_search_orders() {
        let mut summaries = Vec::new();
        for (n, (key, ended)) in [
            (Some(5), true),
            (Some(5), false),
            (None, true),
            (Some(0), true),
            (Some(u64::MAX), false),
            (None, false),
            (Some(7), true),
        ]
        .into_iter()
        .enumerate()
        {
            summaries.push((run(n as u64 + 1).id, key, ended));
        }
        for order in [
            RunOrder::Started,
            RunOrder::Oldest,
            RunOrder::Activity,
            RunOrder::Wakes,
            RunOrder::Duration,
            RunOrder::Calls,
        ] {
            let cursors: Vec<SearchCursor> = summaries
                .iter()
                .map(|(run, key, _)| SearchCursor {
                    key: match order {
                        RunOrder::Started | RunOrder::Oldest => None,
                        _ => *key,
                    },
                    run: *run,
                })
                .collect();
            let mut by_position = cursors.clone();
            by_position.sort_by_key(|cursor| SearchPosition::of(order, cursor));
            let mut by_key = cursors.clone();
            by_key.sort_by_key(|cursor| position(order, cursor.key, cursor.run));
            if order == RunOrder::Started {
                by_key.reverse();
            }
            assert_eq!(by_key, by_position, "{order:?}");
        }
    }
}
