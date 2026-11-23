//! The timer wheel of a partition (`archi/03-execution.md`, "Signals and timers in the glue";
//! `archi/06-storage-and-scale.md`, "Partitions, owners and nodes").
//!
//! It holds the timers due before its horizon, an hour ahead, as the batches that set them made
//! them durable, and once per horizon reads the next hour's timers from the partition: the one
//! scheduling loop of a partition. A timer is known by its kind: setting it again moves it.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rewind_core::Timestamp;

use crate::ports::partition::{Activation, Change};
use crate::ports::{DueTimer, OutboxChange, RunRef, TimerChange, TimerKind};

/// The timers of a partition due before the horizon.
#[derive(Debug)]
pub(crate) struct Wheel {
    timers: BTreeSet<DueTimer>,
    by_kind: BTreeMap<TimerKind, Timestamp>,
    /// Timers due from here on are read from the partition when the horizon moves.
    horizon: Timestamp,
    every: Duration,
    /// While the next hour is being read: what the batches set or cancelled meanwhile, which
    /// the read may not hold.
    reading: Option<BTreeMap<TimerKind, Option<Timestamp>>>,
}

impl Wheel {
    pub(crate) fn new(now: Timestamp, every: Duration) -> Self {
        Self {
            timers: BTreeSet::new(),
            by_kind: BTreeMap::new(),
            horizon: now.saturating_add(every),
            every,
            reading: None,
        }
    }

    pub(crate) fn horizon(&self) -> Timestamp {
        self.horizon
    }

    /// The timers the partition held before the horizon, at its recovery.
    pub(crate) fn load(&mut self, timers: impl IntoIterator<Item = DueTimer>) {
        for timer in timers {
            self.insert(timer);
        }
    }

    fn insert(&mut self, timer: DueTimer) {
        if let Some(at) = self.by_kind.insert(timer.kind.clone(), timer.at) {
            self.timers.remove(&DueTimer {
                at,
                kind: timer.kind.clone(),
            });
        }
        self.timers.insert(timer);
    }

    fn remove(&mut self, kind: &TimerKind) {
        if let Some(at) = self.by_kind.remove(kind) {
            self.timers.remove(&DueTimer {
                at,
                kind: kind.clone(),
            });
        }
    }

    /// A timer change made durable.
    pub(crate) fn apply(&mut self, change: &TimerChange) {
        match change {
            TimerChange::Set(timer) => {
                if let Some(reading) = &mut self.reading {
                    reading.insert(timer.kind.clone(), Some(timer.at));
                }
                if timer.at < self.horizon {
                    self.insert(timer.clone());
                } else {
                    self.remove(&timer.kind);
                }
            }
            TimerChange::Cancel(kind) => {
                if let Some(reading) = &mut self.reading {
                    reading.insert(kind.clone(), None);
                }
                self.remove(kind);
            }
        }
    }

    /// What a durable batch did to the timers: those its changes set and cancelled, those of
    /// the attempts that ended, of the runs erased, of the outbox entries removed.
    pub(crate) fn committed(&mut self, changes: &[Change]) {
        for change in changes {
            match change {
                Change::Timer(timer) => self.apply(timer),
                Change::Activation(activation) => {
                    let Activation {
                        run, timers, head, ..
                    } = activation.as_ref();
                    for timer in timers {
                        self.apply(timer);
                    }
                    if head.is_ended() {
                        self.clear(run, true);
                    }
                }
                Change::Erase { run } => self.clear(run, false),
                Change::Outbox(OutboxChange::Remove(entry)) => {
                    let kinds: Vec<TimerKind> = self
                        .by_kind
                        .keys()
                        .filter(|kind| {
                            matches!(kind, TimerKind::WebhookRetry { entry: of, .. } if of == entry)
                        })
                        .cloned()
                        .collect();
                    for kind in kinds {
                        self.remove(&kind);
                    }
                }
                _ => {}
            }
        }
    }

    /// Drops the run's timers: those of its attempt, or all of them.
    fn clear(&mut self, run: &RunRef, attempt_only: bool) {
        let kinds: Vec<TimerKind> = self
            .by_kind
            .keys()
            .filter(|kind| {
                kind.run() == Some(run) && (!attempt_only || kind.ends_with_the_attempt())
            })
            .cloned()
            .collect();
        for kind in kinds {
            if let Some(reading) = &mut self.reading {
                reading.insert(kind.clone(), None);
            }
            self.remove(&kind);
        }
    }

    /// Takes the timers due at `now`.
    pub(crate) fn due(&mut self, now: Timestamp) -> Vec<DueTimer> {
        let mut due = Vec::new();
        while let Some(first) = self.timers.first() {
            if first.at > now {
                break;
            }
            let Some(first) = self.timers.pop_first() else {
                break;
            };
            self.by_kind.remove(&first.kind);
            due.push(first);
        }
        due
    }

    /// The earliest timer held.
    pub(crate) fn next(&self) -> Option<&DueTimer> {
        self.timers.first()
    }

    /// The earliest due time of the timers of the kinds `matching` takes.
    pub(crate) fn timers_matching(&self, matching: impl Fn(&TimerKind) -> bool) -> Option<Timestamp> {
        self.timers
            .iter()
            .find(|timer| matching(&timer.kind))
            .map(|timer| timer.at)
    }

    /// The horizon moves: from now on the timers up to the next one are held. Returns the range
    /// of the partition to read.
    pub(crate) fn advance(&mut self) -> (Timestamp, Timestamp) {
        let from = self.horizon;
        self.horizon = from.saturating_add(self.every);
        self.reading = Some(BTreeMap::new());
        (from, self.horizon)
    }

    /// The next hour's timers, read from the partition: added, but for those a batch set or
    /// cancelled while they were read.
    pub(crate) fn read(&mut self, timers: Vec<DueTimer>) {
        let changed = self.reading.take().unwrap_or_default();
        for timer in timers {
            if changed.contains_key(&timer.kind) {
                continue;
            }
            if timer.at < self.horizon {
                self.insert(timer);
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.timers.len()
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::{RunId, Seq};

    use super::*;
    use crate::ports::TenantId;

    fn script(n: u32) -> TimerKind {
        TimerKind::Script {
            run: RunRef::new(TenantId::local(), RunId::from_u128(1)),
            seq: Seq(n),
        }
    }

    #[test]
    fn timers_past_the_horizon_wait_for_its_read() {
        let hour = Duration::from_secs(3_600);
        let mut wheel = Wheel::new(Timestamp(0), hour);
        wheel.apply(&TimerChange::Set(DueTimer {
            at: Timestamp(10),
            kind: script(1),
        }));
        wheel.apply(&TimerChange::Set(DueTimer {
            at: Timestamp(4_000_000),
            kind: script(2),
        }));
        assert_eq!(wheel.len(), 1, "the second is past the horizon");
        // Moved, then cancelled: the last change wins.
        wheel.apply(&TimerChange::Set(DueTimer {
            at: Timestamp(20),
            kind: script(1),
        }));
        assert_eq!(wheel.next().map(|timer| timer.at), Some(Timestamp(20)));
        assert!(wheel.due(Timestamp(19)).is_empty());
        assert_eq!(wheel.due(Timestamp(20)).len(), 1);

        let (from, to) = wheel.advance();
        assert_eq!((from, to), (Timestamp(3_600_000), Timestamp(7_200_000)));
        // Cancelled while the hour was read: the read does not bring it back.
        wheel.apply(&TimerChange::Cancel(script(3)));
        wheel.read(vec![
            DueTimer {
                at: Timestamp(4_000_000),
                kind: script(2),
            },
            DueTimer {
                at: Timestamp(5_000_000),
                kind: script(3),
            },
        ]);
        assert_eq!(wheel.len(), 1);
        assert_eq!(wheel.due(Timestamp(4_000_000)).len(), 1);
    }
}
