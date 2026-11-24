//! The instance cache of sticky execution (`archi/03-execution.md`, "Sticky execution").
//!
//! After a commit, the executors of a node keep the code instance, with the run's state and
//! history, in a cache bounded by a number of instances and by the memory they hold. The next
//! activation of the run on this node is a **hit** when the cached state's history length is
//! the one the run's commit made durable: only the new triggers are delivered, without a replay.
//! Otherwise it is a **miss**: a new instance replays the history. A run's owner sends its next
//! activation to the node that ran the last one while that node lives
//! (`archi/07-code-architecture.md`, "Inside a node").

use std::collections::{BTreeMap, HashMap};

use rewind_core::RunState;

use crate::ports::{CodeInstance, RunRef};

/// The bounds of a node's instance cache. When either is passed, the least recently used
/// instances are evicted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheConfig {
    /// 500 by default. 0 turns the cache off: every activation replays.
    pub max_instances: usize,
    /// The memory the instances hold together, as their executor measures it. 4 GiB by default.
    pub max_bytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_instances: 500,
            max_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}

/// An instance as the cache keeps it: in exactly the state of its run's last commit.
#[derive(Debug)]
pub(crate) struct Cached<I> {
    pub(crate) instance: I,
    /// The run's state after the last commit: its history length tells a hit from a miss.
    pub(crate) state: RunState,
    /// The run's history as committed: a hit derives what its new entries project without
    /// reading the history again.
    pub(crate) history: Vec<rewind_core::HistoryEntry>,
}

/// A least-recently-used cache of instances, by run.
#[derive(Debug)]
pub(crate) struct InstanceCache<I> {
    config: CacheConfig,
    entries: HashMap<RunRef, Entry<I>>,
    /// Use order: the smallest tick is the least recently used.
    order: BTreeMap<u64, RunRef>,
    tick: u64,
    bytes: u64,
}

#[derive(Debug)]
struct Entry<I> {
    cached: Cached<I>,
    tick: u64,
    bytes: u64,
}

impl<I: CodeInstance> InstanceCache<I> {
    pub(crate) fn new(config: CacheConfig) -> Self {
        Self {
            config,
            entries: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
            bytes: 0,
        }
    }

    /// Takes the run's instance out of the cache: an activation uses it alone, and puts it back
    /// only once its commit succeeded.
    pub(crate) fn take(&mut self, run: &RunRef) -> Option<Cached<I>> {
        let entry = self.entries.remove(run)?;
        self.order.remove(&entry.tick);
        self.bytes -= entry.bytes;
        Some(entry.cached)
    }

    /// Keeps the run's instance as the most recently used, and evicts what passes the bounds.
    /// Returns the runs whose instance was evicted, this one included if it alone passes them.
    pub(crate) fn insert(&mut self, run: RunRef, cached: Cached<I>) -> Vec<RunRef> {
        let mut evicted = Vec::new();
        if let Some(previous) = self.take(&run) {
            drop(previous);
        }
        let bytes = cached.instance.memory_usage();
        self.tick += 1;
        self.order.insert(self.tick, run.clone());
        self.entries.insert(
            run,
            Entry {
                cached,
                tick: self.tick,
                bytes,
            },
        );
        self.bytes += bytes;
        while self.entries.len() > self.config.max_instances || self.bytes > self.config.max_bytes {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.bytes -= entry.bytes;
            }
            evicted.push(oldest);
        }
        evicted
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, run: &RunRef) -> bool {
        self.entries.contains_key(run)
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::{RunId, Timestamp};

    use super::*;
    use crate::ports::{Delivery, Emitted, ExecutorError, TenantId};

    #[derive(Debug)]
    struct Sized(u64);

    impl CodeInstance for Sized {
        async fn deliver(
            &mut self,
            _delivery: Delivery,
            _now: Timestamp,
        ) -> Result<Vec<Emitted>, ExecutorError> {
            Ok(Vec::new())
        }

        fn memory_usage(&self) -> u64 {
            self.0
        }
    }

    fn run(n: u128) -> RunRef {
        RunRef::new(TenantId::local(), RunId::from_u128(n))
    }

    fn cached(bytes: u64) -> Cached<Sized> {
        Cached {
            instance: Sized(bytes),
            state: RunState::new(),
            history: Vec::new(),
        }
    }

    #[test]
    fn evicts_the_least_recently_used_past_its_number_of_instances() {
        let mut cache = InstanceCache::new(CacheConfig {
            max_instances: 2,
            max_bytes: u64::MAX,
        });
        assert!(cache.insert(run(1), cached(0)).is_empty());
        assert!(cache.insert(run(2), cached(0)).is_empty());
        // Run 1 is used again: run 2 is now the least recently used.
        let one = cache.take(&run(1)).unwrap();
        assert!(cache.insert(run(1), one).is_empty());
        assert_eq!(cache.insert(run(3), cached(0)), vec![run(2)]);
        assert!(cache.contains(&run(1)) && cache.contains(&run(3)));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn evicts_past_its_memory_bound() {
        let mut cache = InstanceCache::new(CacheConfig {
            max_instances: 10,
            max_bytes: 100,
        });
        assert!(cache.insert(run(1), cached(60)).is_empty());
        assert_eq!(cache.insert(run(2), cached(50)), vec![run(1)]);
        assert_eq!(cache.bytes(), 50);
        // An instance that alone passes the bound is not kept.
        assert_eq!(cache.insert(run(3), cached(200)), vec![run(2), run(3)]);
        assert_eq!((cache.len(), cache.bytes()), (0, 0));
    }

    #[test]
    fn a_cache_of_no_instance_keeps_nothing() {
        let mut cache = InstanceCache::new(CacheConfig {
            max_instances: 0,
            max_bytes: u64::MAX,
        });
        assert_eq!(cache.insert(run(1), cached(0)), vec![run(1)]);
        assert!(cache.take(&run(1)).is_none());
    }
}
