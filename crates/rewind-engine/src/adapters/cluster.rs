use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};

use futures_util::Stream;

use crate::ports::cluster::Membership;
use crate::ports::{
    Cluster, ClusterError, ControlPlaneChange, Departed, Moves, NodeId, NodeInfo, PartitionMap,
};

/// The changes of the control plane, broadcast inside the process: what the memory control
/// plane sends, and what every [`Membership::changes`] of a [`MemoryCluster`] streams. Its
/// clones share their subscribers: a [`MemoryCatalog`](crate::adapters::MemoryCatalog) sends
/// through a clone of its cluster's.
#[derive(Debug, Clone, Default)]
pub struct ControlPlaneChanges {
    subscribers: Arc<Mutex<Vec<Weak<Mutex<Queue>>>>>,
}

#[derive(Debug, Default)]
struct Queue {
    changes: VecDeque<ControlPlaneChange>,
    waker: Option<Waker>,
    closed: bool,
}

impl Queue {
    fn close(&mut self) {
        self.closed = true;
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while a lock is held leaves state that is still consistent: every critical
    // section here writes whole values.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ControlPlaneChanges {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends a change to every stream subscribed so far.
    pub fn send(&self, change: ControlPlaneChange) {
        let mut subscribers = lock(&self.subscribers);
        subscribers.retain(|subscriber| {
            let Some(queue) = subscriber.upgrade() else {
                return false;
            };
            let mut queue = lock(&queue);
            if queue.closed {
                return false;
            }
            queue.changes.push_back(change.clone());
            if let Some(waker) = queue.waker.take() {
                waker.wake();
            }
            true
        });
    }

    /// The changes sent from now on.
    pub fn subscribe(&self) -> ChangeStream {
        let queue = Arc::new(Mutex::new(Queue::default()));
        lock(&self.subscribers).push(Arc::downgrade(&queue));
        ChangeStream { queue }
    }
}

/// The changes of the control plane sent after it was subscribed, in order. It ends when the
/// membership it was opened by ends.
#[derive(Debug)]
pub struct ChangeStream {
    queue: Arc<Mutex<Queue>>,
}

impl Stream for ChangeStream {
    type Item = ControlPlaneChange;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut queue = lock(&self.queue);
        if let Some(change) = queue.changes.pop_front() {
            return Poll::Ready(Some(change));
        }
        if queue.closed {
            return Poll::Ready(None);
        }
        queue.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// `Cluster` in memory: several members in one process, so that tests run many nodes. A
/// member's liveness is its [`MemoryMembership`]: dropping it, or [`MemoryCluster::kill`],
/// frees its "lock", which resolves the `watch` of the node before it in the ring and ends its
/// change streams. Changes are broadcast to every member through [`ControlPlaneChanges`]. It
/// passes the conformance suite: it is the reference implementation of the contract. Its clones
/// share their members.
#[derive(Debug, Clone)]
pub struct MemoryCluster {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<ClusterState>,
    changes: ControlPlaneChanges,
}

#[derive(Debug)]
struct ClusterState {
    members: BTreeMap<NodeId, Member>,
    map: PartitionMap,
    /// Tells a member apart from an earlier member of the same id.
    next_token: u64,
    /// The `watch`es waiting for a member to leave or join.
    watchers: Vec<Waker>,
}

#[derive(Debug)]
struct Member {
    info: NodeInfo,
    token: u64,
    /// The change streams it opened, ended with it.
    streams: Vec<Weak<Mutex<Queue>>>,
}

impl ClusterState {
    fn is_member(&self, node: &NodeId, token: u64) -> bool {
        self.members
            .get(node)
            .is_some_and(|member| member.token == token)
    }

    fn wake_watchers(&mut self) {
        for waker in self.watchers.drain(..) {
            waker.wake();
        }
    }

    fn wait(&mut self, waker: &Waker) {
        if !self.watchers.iter().any(|known| known.will_wake(waker)) {
            self.watchers.push(waker.clone());
        }
    }

    /// The member after `node` in the ring, by id, the last followed by the first.
    fn next_after(&self, node: &NodeId) -> Option<(NodeId, u64)> {
        let after = self
            .members
            .range::<NodeId, _>((std::ops::Bound::Excluded(node), std::ops::Bound::Unbounded))
            .next();
        let first = self.members.iter().next();
        after
            .or(first)
            .filter(|(id, _)| *id != node)
            .map(|(id, member)| (id.clone(), member.token))
    }
}

impl MemoryCluster {
    /// A cluster of `partitions` partitions, none owned, with changes of its own.
    pub fn new(partitions: u16) -> Self {
        Self::with_changes(partitions, ControlPlaneChanges::new())
    }

    /// A cluster whose members stream `changes`, which the memory control plane sends to.
    pub fn with_changes(partitions: u16, changes: ControlPlaneChanges) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(ClusterState {
                    members: BTreeMap::new(),
                    map: PartitionMap::unowned(partitions),
                    next_token: 0,
                    watchers: Vec::new(),
                }),
                changes,
            }),
        }
    }

    /// The changes its members stream, to send to.
    pub fn changes(&self) -> &ControlPlaneChanges {
        &self.shared.changes
    }

    /// Ends a member's liveness as if its process died: its `Membership` fails every call with
    /// [`ClusterError::NotMember`] from now on. Returns whether it was a member.
    pub fn kill(&self, node: &NodeId) -> bool {
        let token = lock(&self.shared.state)
            .members
            .get(node)
            .map(|member| member.token);
        token.is_some_and(|token| self.shared.depart(node, token))
    }
}

impl Shared {
    /// Removes the member if it is still this one; returns whether it was.
    fn depart(&self, node: &NodeId, token: u64) -> bool {
        let streams = {
            let mut state = lock(&self.state);
            if !state.is_member(node, token) {
                return false;
            }
            let member = state.members.remove(node);
            state.wake_watchers();
            member.map(|member| member.streams).unwrap_or_default()
        };
        for stream in streams.iter().filter_map(Weak::upgrade) {
            lock(&stream).close();
        }
        true
    }
}

impl Cluster for MemoryCluster {
    type Membership = MemoryMembership;

    async fn join(&self, node: NodeInfo) -> Result<MemoryMembership, ClusterError> {
        let mut state = lock(&self.shared.state);
        if state.members.contains_key(&node.id) {
            return Err(ClusterError::AlreadyMember(node.id));
        }
        let token = state.next_token;
        state.next_token += 1;
        let id = node.id.clone();
        state.members.insert(
            id.clone(),
            Member {
                info: node,
                token,
                streams: Vec::new(),
            },
        );
        state.wake_watchers();
        Ok(MemoryMembership {
            shared: Arc::clone(&self.shared),
            node: id,
            token,
        })
    }
}

/// A member of a [`MemoryCluster`]; dropping it ends the membership.
#[derive(Debug)]
pub struct MemoryMembership {
    shared: Arc<Shared>,
    node: NodeId,
    token: u64,
}

impl MemoryMembership {
    fn state(&self) -> Result<MutexGuard<'_, ClusterState>, ClusterError> {
        let state = lock(&self.shared.state);
        if state.is_member(&self.node, self.token) {
            Ok(state)
        } else {
            Err(ClusterError::NotMember(self.node.clone()))
        }
    }

    fn poll_watch(
        &self,
        target: &mut Option<(NodeId, u64)>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Departed, ClusterError>> {
        let mut state = match self.state() {
            Ok(state) => state,
            Err(error) => return Poll::Ready(Err(error)),
        };
        if target.is_none() {
            *target = state.next_after(&self.node);
        }
        match target {
            Some((node, token)) if !state.is_member(node, *token) => {
                Poll::Ready(Ok(Departed { node: node.clone() }))
            }
            _ => {
                state.wait(cx.waker());
                Poll::Pending
            }
        }
    }
}

impl Drop for MemoryMembership {
    fn drop(&mut self) {
        self.shared.depart(&self.node, self.token);
    }
}

impl Membership for MemoryMembership {
    type Changes = ChangeStream;

    fn node(&self) -> &NodeId {
        &self.node
    }

    fn watch(&self) -> impl Future<Output = Result<Departed, ClusterError>> + Send {
        let mut target = None;
        std::future::poll_fn(move |cx| self.poll_watch(&mut target, cx))
    }

    async fn assign(&self, moves: &Moves) -> Result<PartitionMap, ClusterError> {
        let map = {
            let mut state = self.state()?;
            for (partition, node) in &moves.moves {
                if !state.map.partitions.contains_key(partition) {
                    return Err(ClusterError::UnknownPartition(*partition));
                }
                if !state.members.contains_key(node) {
                    return Err(ClusterError::UnknownNode(node.clone()));
                }
            }
            for (partition, node) in &moves.moves {
                if let Some(owner) = state.map.partitions.get_mut(partition)
                    && owner.node.as_ref() != Some(node)
                {
                    owner.node = Some(node.clone());
                    owner.epoch = owner.epoch.next();
                }
            }
            state.map.clone()
        };
        self.shared
            .changes
            .send(ControlPlaneChange::PartitionMap(map.clone()));
        Ok(map)
    }

    async fn partition_map(&self) -> Result<PartitionMap, ClusterError> {
        Ok(self.state()?.map.clone())
    }

    async fn nodes(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        Ok(self
            .state()?
            .members
            .values()
            .map(|member| member.info.clone())
            .collect())
    }

    async fn changes(&self) -> Result<ChangeStream, ClusterError> {
        let stream = self.shared.changes.subscribe();
        let mut state = self.state()?;
        if let Some(member) = state.members.get_mut(&self.node) {
            member.streams.retain(|stream| stream.strong_count() > 0);
            member.streams.push(Arc::downgrade(&stream.queue));
        }
        Ok(stream)
    }

    async fn leave(self) -> Result<(), ClusterError> {
        if self.shared.depart(&self.node, self.token) {
            Ok(())
        } else {
            Err(ClusterError::NotMember(self.node.clone()))
        }
    }
}
