use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

use crate::node::runtime::{Changes, NodeHandle};
use crate::plane::Plane;
use crate::ports::link::{
    ActivationRequest, ActivationResult, NodeReply, NodeRequest, SubscriptionKey,
};
use crate::ports::{LinkError, NodeId, NodeLink};

type Matcher = Box<dyn Fn(&NodeRequest) -> bool + Send>;

#[derive(Default)]
struct Faults {
    cut: BTreeSet<NodeId>,
    drops: Vec<(Matcher, usize)>,
    holds: Vec<Matcher>,
    dropped: usize,
}

/// `NodeLink` inside a process: the nodes of a cluster running in one process, each reached
/// through its [`NodeHandle`], as the HTTP/2 link reaches another process. Tests make it drop
/// the next requests that match, hold requests until released, or cut a node off.
pub struct LocalLink<P: Plane> {
    nodes: Mutex<BTreeMap<NodeId, NodeHandle<P>>>,
    faults: Mutex<Faults>,
    /// Bumped by [`LocalLink::release`]: the requests held wait for it.
    released: watch::Sender<u64>,
}

impl<P: Plane> fmt::Debug for LocalLink<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalLink")
            .field("nodes", &self.lock_nodes().keys().cloned().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl<P: Plane> Default for LocalLink<P> {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<P: Plane> LocalLink<P> {
    pub fn new() -> Self {
        Self {
            nodes: Mutex::new(BTreeMap::new()),
            faults: Mutex::new(Faults::default()),
            released: watch::Sender::new(0),
        }
    }

    fn lock_nodes(&self) -> MutexGuard<'_, BTreeMap<NodeId, NodeHandle<P>>> {
        lock(&self.nodes)
    }

    /// Makes a node reachable.
    pub fn register(&self, handle: NodeHandle<P>) {
        self.lock_nodes().insert(handle.node().clone(), handle);
    }

    /// The node is gone: it is not reached any more.
    pub fn unregister(&self, node: &NodeId) {
        self.lock_nodes().remove(node);
    }

    /// Nothing reaches the node through the link.
    pub fn cut(&self, node: NodeId) {
        lock(&self.faults).cut.insert(node);
    }

    pub fn heal(&self, node: &NodeId) {
        lock(&self.faults).cut.remove(node);
    }

    /// The next `times` requests that match are lost: their caller gets `Unreachable`.
    pub fn drop_next(&self, times: usize, matcher: impl Fn(&NodeRequest) -> bool + Send + 'static) {
        lock(&self.faults).drops.push((Box::new(matcher), times));
    }

    /// The requests that match wait until [`release`](Self::release).
    pub fn hold(&self, matcher: impl Fn(&NodeRequest) -> bool + Send + 'static) {
        lock(&self.faults).holds.push(Box::new(matcher));
    }

    /// The requests held go, and no request is held any more.
    pub fn release(&self) {
        lock(&self.faults).holds.clear();
        self.released.send_modify(|generation| *generation += 1);
    }

    /// How many requests were dropped.
    pub fn dropped(&self) -> usize {
        lock(&self.faults).dropped
    }

    fn handle(&self, node: &NodeId) -> Result<NodeHandle<P>, LinkError> {
        if lock(&self.faults).cut.contains(node) {
            return Err(LinkError::Unreachable(node.clone()));
        }
        self.lock_nodes()
            .get(node)
            .cloned()
            .ok_or_else(|| LinkError::Unreachable(node.clone()))
    }

    /// Whether the request is dropped, held or carried.
    fn fault(&self, node: &NodeId, request: &NodeRequest) -> Result<bool, LinkError> {
        let mut faults = lock(&self.faults);
        for (matcher, times) in &mut faults.drops {
            if *times > 0 && matcher(request) {
                *times -= 1;
                faults.dropped += 1;
                return Err(LinkError::Unreachable(node.clone()));
            }
        }
        Ok(faults.holds.iter().any(|matcher| matcher(request)))
    }
}

impl<P: Plane> NodeLink for LocalLink<P> {
    type Changes = Changes;

    async fn call(&self, node: &NodeId, request: NodeRequest) -> Result<NodeReply, LinkError> {
        let handle = self.handle(node)?;
        if self.fault(node, &request)? {
            let mut released = self.released.subscribe();
            let _ = released.changed().await;
        }
        handle.serve(request).await
    }

    async fn subscribe(
        &self,
        node: &NodeId,
        key: SubscriptionKey,
        from: Option<u64>,
    ) -> Result<Changes, LinkError> {
        self.handle(node)?.subscribe(key, from)
    }

    async fn activate(
        &self,
        node: &NodeId,
        request: ActivationRequest,
    ) -> Result<ActivationResult, LinkError> {
        self.handle(node)?.serve_activation(request).await
    }
}
