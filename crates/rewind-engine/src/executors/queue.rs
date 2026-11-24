//! The ready queue of a node (`archi/03-execution.md`, "Activations"): fair across tenants,
//! which take turns, and within a tenant the runs with the fewest started activations first,
//! then the run whose oldest trigger has waited longest.

use std::collections::{BTreeMap, VecDeque};

use rewind_core::{RunId, Timestamp};

use crate::ports::TenantId;

/// Runs waiting for an executor.
#[derive(Debug)]
pub(crate) struct ReadyQueue<T> {
    /// The tenants with runnable runs, in turn.
    turns: VecDeque<TenantId>,
    runs: BTreeMap<TenantId, BTreeMap<(u32, Timestamp, RunId), T>>,
}

impl<T> Default for ReadyQueue<T> {
    fn default() -> Self {
        Self {
            turns: VecDeque::new(),
            runs: BTreeMap::new(),
        }
    }
}

impl<T> ReadyQueue<T> {
    /// Queues a run of `tenant` with `unfinished` activations never finished and its oldest trigger
    /// received at `oldest`. A run queued again replaces its entry.
    pub(crate) fn push(
        &mut self,
        tenant: TenantId,
        unfinished: u32,
        oldest: Timestamp,
        run: RunId,
        item: T,
    ) {
        let runs = self.runs.entry(tenant.clone()).or_default();
        runs.retain(|(_, _, queued), _| *queued != run);
        if runs.is_empty() {
            self.turns.push_back(tenant);
        }
        runs.insert((unfinished, oldest, run), item);
    }

    /// The next run: the first of the tenant whose turn it is.
    pub(crate) fn pop(&mut self) -> Option<T> {
        let tenant = self.turns.pop_front()?;
        let runs = self.runs.get_mut(&tenant)?;
        let (_, item) = runs.pop_first()?;
        if runs.is_empty() {
            self.runs.remove(&tenant);
        } else {
            self.turns.push_back(tenant);
        }
        Some(item)
    }

    pub(crate) fn len(&self) -> usize {
        self.runs.values().map(BTreeMap::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(name: &str) -> TenantId {
        TenantId::new(name).unwrap()
    }

    #[test]
    fn tenants_take_turns_and_fewer_unfinished_go_first() {
        let mut queue = ReadyQueue::default();
        let (a, b) = (tenant("a"), tenant("b"));
        for n in 1..=3 {
            queue.push(
                a.clone(),
                0,
                Timestamp(n),
                RunId::from_u128(u128::from(n)),
                n,
            );
        }
        queue.push(b.clone(), 0, Timestamp(9), RunId::from_u128(10), 10);
        // A run that killed its node twice waits behind those that never started.
        queue.push(a.clone(), 2, Timestamp(0), RunId::from_u128(20), 20);
        let order: Vec<u64> = std::iter::from_fn(|| queue.pop()).collect();
        assert_eq!(order, vec![1, 10, 2, 3, 20]);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn a_run_queued_again_keeps_one_place() {
        let mut queue = ReadyQueue::default();
        let a = tenant("a");
        queue.push(a.clone(), 0, Timestamp(5), RunId::from_u128(1), "first");
        queue.push(a.clone(), 0, Timestamp(1), RunId::from_u128(1), "again");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.pop(), Some("again"));
        assert_eq!(queue.pop(), None);
    }
}
