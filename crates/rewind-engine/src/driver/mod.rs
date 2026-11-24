//! The driver: the run plane in tests and test runs, on one thread
//! (`archi/07-code-architecture.md`, "Inside a node"; `archi/05-testing-and-observability.md`,
//! "Test runs").
//!
//! It steps every actor of every node one message at a time, in the order they were sent; when
//! no message is left it runs the oldest job to its end; when no job is left it fires the timers
//! and wakes due now, then what the cluster's memberships tell. In virtual time, when nothing
//! is left to do, it moves the clock to the earliest timer that wakes a run by itself (a
//! code's timer, a retry, a backoff), or to an earlier run deadline, and goes on. The same
//! inputs always give the same steps in the same order.
//!
//! Messages between nodes go through the driver's link, which the tests may cut, drop or hold
//! ([`Faults`]): the cluster tests run several nodes in one process over the memory adapters.

mod faults;

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::Stream;
use rewind_core::{PartitionId, RunId, Timestamp};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel as unbounded};

pub use faults::{Faults, Traffic};

use crate::node::message::{Effect, Job, JobClass, Message, ReplyTo};
use crate::node::{Node, NodeMsg};
use crate::partition::OwnerMsg;
use crate::plane::{NodeConfig, Plane};
use crate::ports::cluster::Membership;
use crate::ports::link::{
    ActivationResult, Changed, NodeReply, NodeRequest, StreamAttach, StreamEvent, StreamId,
    SubscriptionKey,
};
use crate::ports::{
    Cluster, ClusterError, CodeExecutor, Departed, LinkError, NodeId, NodeInfo, PartitionMap,
    VirtualTime,
};

type Watch = Pin<Box<dyn Future<Output = Result<Departed, ClusterError>> + Send>>;

struct DriverNode<P: Plane, M: Membership + 'static> {
    node: Node<P, M>,
    membership: Arc<M>,
    watch: Option<Watch>,
    changes: Option<M::Changes>,
}

/// Why a request through the driver got no reply.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DriveError {
    /// Nothing was left to do and the request had no reply: it was lost.
    #[error("the request got no reply: it was lost, or its node died")]
    NoReply,
    #[error("no node {0}")]
    NoNode(NodeId),
    #[error(transparent)]
    Cluster(#[from] ClusterError),
    /// The driver took more steps than its bound: something never stops.
    #[error("the driver did not become idle within {0} steps")]
    NeverIdle(usize),
}

/// The steps a drive took.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Steps {
    pub messages: usize,
    pub jobs: usize,
    pub jumps: usize,
}

/// Every node of a cluster in one process, stepped on one thread.
pub struct Driver<P: Plane, K: Cluster + 'static> {
    cluster: K,
    clock: Arc<dyn VirtualClockHandle>,
    nodes: BTreeMap<NodeId, DriverNode<P, K::Membership>>,
    queue: VecDeque<(NodeId, Message<P>)>,
    /// Jobs by class: what reads and writes the stores first, then activations, then calls, the
    /// calls of several runs in the order of the runs' ids; in the order they came within a class.
    jobs: BTreeMap<(JobClass, u64), (NodeId, Job<P>)>,
    wakes: BTreeMap<(Timestamp, u64), (NodeId, Message<P>, bool)>,
    next_id: u64,
    waiters: BTreeMap<u64, Option<NodeReply>>,
    /// Requests carried to another node, by the id of their reply: who asked.
    links: BTreeMap<u64, (NodeId, Option<crate::node::message::Asker>)>,
    /// Activations sent to another node: who asked, for which run.
    activations: BTreeMap<u64, (NodeId, PartitionId, RunId)>,
    faults: Faults,
    /// Messages held by the faults, released by [`Driver::release`].
    held: Vec<(NodeId, Message<P>)>,
    steps: Steps,
    /// The most steps a drive takes.
    pub max_steps: usize,
}

/// The clock of the driver: virtual time, moved forward.
pub trait VirtualClockHandle: Send + Sync {
    fn now(&self) -> Timestamp;
    fn advance_to(&self, at: Timestamp);
}

/// The virtual clock of a plane, as the driver moves it.
#[derive(Debug)]
pub struct PlaneClock<P>(pub Arc<P>);

impl<P: Plane> VirtualClockHandle for PlaneClock<P>
where
    P::Clock: VirtualTime,
{
    fn now(&self) -> Timestamp {
        crate::ports::Clock::now(self.0.clock())
    }
    fn advance_to(&self, at: Timestamp) {
        VirtualTime::advance_to(self.0.clock(), at);
    }
}

impl<P: Plane, K: Cluster + 'static> fmt::Debug for Driver<P, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("nodes", &self.nodes.keys().collect::<Vec<_>>())
            .field("queued", &self.queue.len())
            .field("jobs", &self.jobs.len())
            .field("steps", &self.steps)
            .finish_non_exhaustive()
    }
}

fn noop_context<T>(poll: impl FnOnce(&mut Context<'_>) -> Poll<T>) -> Poll<T> {
    let waker = futures_util::task::noop_waker_ref();
    let mut context = Context::from_waker(waker);
    poll(&mut context)
}

impl<P: Plane, K: Cluster + 'static> Driver<P, K> {
    /// A driver over `cluster`, whose clock is `clock` (the planes' clock, shared).
    pub fn new(cluster: K, clock: Arc<dyn VirtualClockHandle>) -> Self {
        Self {
            cluster,
            clock,
            nodes: BTreeMap::new(),
            queue: VecDeque::new(),
            jobs: BTreeMap::new(),
            wakes: BTreeMap::new(),
            next_id: 1,
            waiters: BTreeMap::new(),
            links: BTreeMap::new(),
            activations: BTreeMap::new(),
            faults: Faults::default(),
            held: Vec::new(),
            steps: Steps::default(),
            max_steps: 1_000_000,
        }
    }

    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Moves the clock to `at`.
    pub fn advance_to(&self, at: Timestamp) {
        self.clock.advance_to(at);
    }

    pub fn faults(&mut self) -> &mut Faults {
        &mut self.faults
    }

    pub fn steps(&self) -> Steps {
        self.steps
    }

    /// The activations of a node that wait for their job to run.
    pub fn pending_activations(&self, node: &NodeId) -> usize {
        self.jobs
            .iter()
            .filter(|((class, _), (of, _))| *class == JobClass::Activation && of == node)
            .count()
    }

    /// The nodes alive.
    pub fn node_ids(&self) -> Vec<NodeId> {
        self.nodes.keys().cloned().collect()
    }

    pub fn node(&self, id: &NodeId) -> Option<&Node<P, K::Membership>> {
        self.nodes.get(id).map(|node| &node.node)
    }

    /// Joins a node to the cluster: it takes the partitions it wins.
    pub async fn add_node(
        &mut self,
        plane: Arc<P>,
        config: NodeConfig,
    ) -> Result<NodeId, DriveError> {
        let id = config.node.clone();
        let epochs = config
            .epochs
            .clone()
            .unwrap_or_else(|| plane.executor().epochs().iter().copied().collect());
        let info = NodeInfo {
            id: id.clone(),
            address: format!("{id}.local"),
            build: config.build.clone(),
            epochs,
            pool: config.pool.clone(),
            executors_only: config.executors_only,
        };
        let membership = Arc::new(self.cluster.join(info).await?);
        let changes = membership.changes().await?;
        let mut node = Node::new(plane, config, Arc::clone(&membership));
        let effects = node.start();
        let watch = Self::watch(&membership);
        self.nodes.insert(
            id.clone(),
            DriverNode {
                node,
                membership,
                watch: Some(watch),
                changes: Some(changes),
            },
        );
        self.carry(&id, effects);
        Ok(id)
    }

    fn watch(membership: &Arc<K::Membership>) -> Watch {
        let membership = Arc::clone(membership);
        Box::pin(async move { membership.watch().await })
    }

    /// The node dies: what it held in memory is lost (its messages, its jobs in flight, its
    /// instances), and its liveness ends.
    pub fn kill(&mut self, id: &NodeId) {
        self.nodes.remove(id);
        self.queue.retain(|(node, _)| node != id);
        self.jobs.retain(|_, (node, _)| node != id);
        self.wakes.retain(|_, (node, _, _)| node != id);
        self.held.retain(|(node, _)| node != id);
    }

    /// The node's process lives on, but its link is cut and its liveness lost: the others take
    /// its partitions, and its owners are fenced at their next commit.
    pub fn cut_off(&mut self, id: &NodeId) {
        self.faults.cut(id.clone());
        if let Some(node) = self.nodes.get_mut(id) {
            node.watch = None;
            node.changes = None;
        }
    }

    /// The membership of the node is the cluster's to end: it is lost.
    pub fn membership(&self, id: &NodeId) -> Option<Arc<K::Membership>> {
        self.nodes.get(id).map(|node| Arc::clone(&node.membership))
    }

    /// The node hands its partitions over and leaves.
    pub async fn stop(&mut self, id: &NodeId) -> Result<(), DriveError> {
        let effects = match self.nodes.get_mut(id) {
            Some(node) => node.node.hand_over(),
            None => return Err(DriveError::NoNode(id.clone())),
        };
        self.carry(id, effects);
        self.run_until_idle().await?;
        self.nodes.remove(id);
        Ok(())
    }

    /// Releases the messages the faults held.
    pub fn release(&mut self) {
        for (node, message) in std::mem::take(&mut self.held) {
            self.queue.push_back((node, message));
        }
    }

    // ---------------------------------------------------------------------------------------
    // Stepping

    /// Takes one step; returns whether there was one.
    pub async fn step(&mut self) -> bool {
        if let Some((node, message)) = self.queue.pop_front() {
            self.steps.messages += 1;
            self.deliver(&node, message);
            return true;
        }
        if let Some((_, (node, job))) = self.jobs.pop_first() {
            self.steps.jobs += 1;
            let message = job.await;
            if self.nodes.contains_key(&node) {
                self.queue.push_back((node, message));
            }
            return true;
        }
        if self.fire_due() {
            return true;
        }
        self.poll_memberships()
    }

    /// Steps until nothing is left to do now; the clock does not move.
    pub async fn run_until_idle(&mut self) -> Result<Steps, DriveError> {
        let start = self.steps;
        let mut taken = 0;
        while self.step().await {
            taken += 1;
            if taken > self.max_steps {
                return Err(DriveError::NeverIdle(self.max_steps));
            }
        }
        Ok(self.since(start))
    }

    /// Steps until nothing is left to do, moving the clock to the next timer that wakes a run
    /// by itself (or an earlier run deadline) each time nothing is left now. Returns once only
    /// the outside world can wake a run.
    pub async fn run_until_idle_virtual(&mut self) -> Result<Steps, DriveError> {
        let start = self.steps;
        loop {
            self.run_until_idle().await?;
            let Some(next) = self.next_jump() else {
                return Ok(self.since(start));
            };
            self.steps.jumps += 1;
            self.clock.advance_to(next);
            if self.steps.jumps.saturating_sub(start.jumps) > self.max_steps {
                return Err(DriveError::NeverIdle(self.max_steps));
            }
        }
    }

    /// [`run_until_idle_virtual`](Self::run_until_idle_virtual) without moving the clock past
    /// `until`, then to `until`.
    pub async fn run_until(&mut self, until: Timestamp) -> Result<Steps, DriveError> {
        let start = self.steps;
        loop {
            self.run_until_idle().await?;
            match self.next_jump() {
                Some(next) if next <= until => {
                    self.steps.jumps += 1;
                    self.clock.advance_to(next);
                }
                _ => {
                    self.clock.advance_to(until);
                    self.run_until_idle().await?;
                    return Ok(self.since(start));
                }
            }
        }
    }

    fn since(&self, start: Steps) -> Steps {
        Steps {
            messages: self.steps.messages - start.messages,
            jobs: self.steps.jobs - start.jobs,
            jumps: self.steps.jumps - start.jumps,
        }
    }

    /// Where virtual time goes next: the earliest timer or wake that wakes something by itself,
    /// or an earlier run deadline. A deadline alone wakes nothing: a run waiting for a signal
    /// waits for the outside world.
    pub fn next_jump(&self) -> Option<Timestamp> {
        let now = self.clock.now();
        let mut waking: Option<Timestamp> = None;
        let mut deadline: Option<Timestamp> = None;
        for node in self.nodes.values() {
            let (node_waking, node_deadline) = node.node.wake_ups();
            waking = min(waking, node_waking);
            deadline = min(deadline, node_deadline);
        }
        for ((at, _), (_, _, alone)) in &self.wakes {
            if *alone {
                waking = min(waking, Some(*at));
            }
        }
        let next = waking?;
        let next = deadline.map_or(next, |deadline| deadline.min(next));
        (next > now).then_some(next)
    }

    fn fire_due(&mut self) -> bool {
        let now = self.clock.now();
        let mut fired = false;
        let ids: Vec<NodeId> = self.nodes.keys().cloned().collect();
        for id in ids {
            let Some(node) = self.nodes.get_mut(&id) else {
                continue;
            };
            for message in node.node.due(now) {
                fired = true;
                self.queue.push_back((id.clone(), message));
            }
        }
        let due: Vec<(Timestamp, u64)> = self
            .wakes
            .range(..(now, u64::MAX))
            .map(|(key, _)| *key)
            .collect();
        for key in due {
            if let Some((node, message, _)) = self.wakes.remove(&key) {
                fired = true;
                self.queue.push_back((node, message));
            }
        }
        fired
    }

    fn poll_memberships(&mut self) -> bool {
        let mut polled = false;
        let ids: Vec<NodeId> = self.nodes.keys().cloned().collect();
        for id in ids {
            let Some(node) = self.nodes.get_mut(&id) else {
                continue;
            };
            let mut departed = None;
            if let Some(watch) = &mut node.watch {
                match noop_context(|context| watch.as_mut().poll(context)) {
                    Poll::Ready(Ok(Departed { node: gone })) => departed = Some(Ok(gone)),
                    Poll::Ready(Err(error)) => departed = Some(Err(error)),
                    Poll::Pending => {}
                }
            }
            match departed {
                Some(Ok(gone)) => {
                    node.watch = Some(Self::watch(&node.membership));
                    self.queue
                        .push_back((id.clone(), Message::Node(NodeMsg::Departed(gone))));
                    polled = true;
                }
                Some(Err(error @ ClusterError::NotMember(_))) => {
                    node.watch = None;
                    node.changes = None;
                    self.queue
                        .push_back((id.clone(), Message::Node(NodeMsg::Lost(error.to_string()))));
                    polled = true;
                }
                Some(Err(_)) => node.watch = None,
                None => {}
            }
            let Some(node) = self.nodes.get_mut(&id) else {
                continue;
            };
            if let Some(changes) = &mut node.changes {
                loop {
                    match noop_context(|context| Pin::new(&mut *changes).poll_next(context)) {
                        Poll::Ready(Some(change)) => {
                            polled = true;
                            self.queue
                                .push_back((id.clone(), Message::Node(NodeMsg::Change(change))));
                        }
                        Poll::Ready(None) => {
                            node.changes = None;
                            break;
                        }
                        Poll::Pending => break,
                    }
                }
            }
        }
        polled
    }

    fn deliver(&mut self, id: &NodeId, message: Message<P>) {
        let Some(node) = self.nodes.get_mut(id) else {
            return;
        };
        let effects = node.node.handle(message);
        self.carry(id, effects);
    }

    /// Carries the effects of a node's step.
    fn carry(&mut self, origin: &NodeId, effects: Vec<Effect<P>>) {
        for effect in effects {
            match effect {
                Effect::Local(message) => self.queue.push_back((origin.clone(), message)),
                Effect::Request { .. } => {
                    tracing::error!("a request left a node unrouted");
                }
                Effect::Remote {
                    node,
                    request,
                    asker,
                } => {
                    let traffic = self.faults.traffic(origin, &node, &request);
                    let reachable = self.nodes.contains_key(&node) && traffic != Traffic::Drop;
                    if !reachable {
                        if let Some(asker) = asker {
                            self.queue.push_back((
                                origin.clone(),
                                Message::Answer(asker, Err(LinkError::Unreachable(node))),
                            ));
                        }
                        continue;
                    }
                    let id = self.next_id;
                    self.next_id += 1;
                    self.links.insert(id, (origin.clone(), asker));
                    let message = Message::Inbound(request, ReplyTo::Link(id));
                    if traffic == Traffic::Hold {
                        self.held.push((node, message));
                    } else {
                        self.queue.push_back((node, message));
                    }
                }
                Effect::Reply(to, reply) => match to {
                    ReplyTo::Waiter(id) => {
                        if let Some(slot) = self.waiters.get_mut(&id) {
                            *slot = Some(reply);
                        }
                    }
                    ReplyTo::Link(id) => {
                        let Some((asker_node, asker)) = self.links.remove(&id) else {
                            continue;
                        };
                        if self.faults.cut_between(origin, &asker_node) {
                            continue;
                        }
                        if let Some(asker) = asker {
                            self.queue
                                .push_back((asker_node, Message::Answer(asker, Ok(reply))));
                        }
                    }
                    ReplyTo::Owner(asker) => {
                        self.queue
                            .push_back((origin.clone(), Message::Answer(asker, Ok(reply))));
                    }
                    ReplyTo::Nobody => {}
                },
                Effect::Job(class, job) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.jobs.insert((class, id), (origin.clone(), job));
                }
                Effect::Wake { at, alone, message } => {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.wakes
                        .insert((at, id), (origin.clone(), message, alone));
                }
                Effect::Activate {
                    node,
                    request,
                    asker,
                } => {
                    let run = request.head.run.id;
                    let unreachable =
                        !self.nodes.contains_key(&node) || self.faults.cut_between(origin, &node);
                    if unreachable {
                        self.queue.push_back((
                            origin.clone(),
                            Message::Owner(
                                asker.partition,
                                OwnerMsg::Activated {
                                    run,
                                    result: Box::new(ActivationResult::Failed {
                                        error: format!("node {node} cannot be reached"),
                                    }),
                                },
                            ),
                        ));
                        continue;
                    }
                    let id = self.next_id;
                    self.next_id += 1;
                    self.activations
                        .insert(id, (origin.clone(), asker.partition, run));
                    self.queue
                        .push_back((node, Message::InboundActivation(request, ReplyTo::Link(id))));
                }
                Effect::ActivationReply(to, result) => {
                    let ReplyTo::Link(id) = to else {
                        continue;
                    };
                    let Some((asker_node, partition, run)) = self.activations.remove(&id) else {
                        continue;
                    };
                    self.queue.push_back((
                        asker_node,
                        Message::Owner(partition, OwnerMsg::Activated { run, result }),
                    ));
                }
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // The edge

    /// Sends a request to node `at`, as its edge does, and steps until it is answered.
    pub async fn request(
        &mut self,
        at: &NodeId,
        request: NodeRequest,
    ) -> Result<NodeReply, DriveError> {
        if !self.nodes.contains_key(at) {
            return Err(DriveError::NoNode(at.clone()));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.waiters.insert(id, None);
        self.queue
            .push_back((at.clone(), Message::Inbound(request, ReplyTo::Waiter(id))));
        let mut taken = 0;
        loop {
            if let Some(Some(reply)) = self.waiters.get(&id) {
                let reply = reply.clone();
                self.waiters.remove(&id);
                return Ok(reply);
            }
            if !self.step().await {
                self.waiters.remove(&id);
                return Err(DriveError::NoReply);
            }
            taken += 1;
            if taken > self.max_steps {
                return Err(DriveError::NeverIdle(self.max_steps));
            }
        }
    }

    /// Sends requests to node `at` together, as several edges at once would, and steps until
    /// each is answered: their replies, in the order of the requests.
    pub async fn requests(
        &mut self,
        at: &NodeId,
        requests: Vec<NodeRequest>,
    ) -> Result<Vec<NodeReply>, DriveError> {
        if !self.nodes.contains_key(at) {
            return Err(DriveError::NoNode(at.clone()));
        }
        let mut ids = Vec::with_capacity(requests.len());
        for request in requests {
            let id = self.next_id;
            self.next_id += 1;
            self.waiters.insert(id, None);
            self.queue
                .push_back((at.clone(), Message::Inbound(request, ReplyTo::Waiter(id))));
            ids.push(id);
        }
        let mut taken = 0;
        loop {
            if ids
                .iter()
                .all(|id| matches!(self.waiters.get(id), Some(Some(_))))
            {
                return Ok(ids
                    .iter()
                    .filter_map(|id| self.waiters.remove(id).flatten())
                    .collect());
            }
            if !self.step().await {
                for id in &ids {
                    self.waiters.remove(id);
                }
                return Err(DriveError::NoReply);
            }
            taken += 1;
            if taken > self.max_steps {
                return Err(DriveError::NeverIdle(self.max_steps));
            }
        }
    }

    /// The partition map node `at` knows.
    pub fn map(&self, at: &NodeId) -> Option<PartitionMap> {
        self.nodes.get(at).map(|node| node.node.map().clone())
    }

    /// Sends a request to the owner of `partition` from the edge of node `at`: through the
    /// link when another node owns it. A `NotOwner` reply waits for the map to change, and the
    /// request goes again.
    pub async fn route(
        &mut self,
        at: &NodeId,
        partition: PartitionId,
        request: crate::ports::link::PartitionRequest,
    ) -> Result<NodeReply, DriveError> {
        for _ in 0..4 {
            let owner = self
                .map(at)
                .and_then(|map| map.owner(partition).cloned())
                .ok_or(DriveError::NoReply)?;
            let node_request = NodeRequest::Partition {
                partition,
                request: Box::new(request.clone()),
            };
            let reply = if owner == *at {
                self.request(at, node_request).await?
            } else {
                if self.faults.cut_between(at, &owner) || !self.nodes.contains_key(&owner) {
                    return Err(DriveError::NoReply);
                }
                self.request(&owner, node_request).await?
            };
            if let NodeReply::NotOwner { .. } = reply {
                self.run_until_idle().await?;
                continue;
            }
            return Ok(reply);
        }
        Err(DriveError::NoReply)
    }

    /// Subscribes to a key at node `at`.
    pub fn subscribe(
        &mut self,
        at: &NodeId,
        key: SubscriptionKey,
        from: Option<u64>,
    ) -> Option<UnboundedReceiver<Changed>> {
        let (sink, changes) = unbounded();
        let effects = self.nodes.get_mut(at)?.node.subscribe(key, from, sink);
        self.carry(at, effects);
        Some(changes)
    }

    /// Opens an SDK stream at node `at`: the harness tasks it is handed, and those that no longer
    /// wait for an answer, arrive on the receiver.
    pub fn attach_stream(
        &mut self,
        at: &NodeId,
        tenant: crate::ports::TenantId,
        attach: StreamAttach,
    ) -> Option<UnboundedReceiver<StreamEvent>> {
        let (sink, tasks) = unbounded();
        let effects = self
            .nodes
            .get_mut(at)?
            .node
            .attach_stream(tenant, attach, sink);
        self.carry(at, effects);
        Some(tasks)
    }

    /// Closes an SDK stream at node `at`.
    pub fn detach_stream(&mut self, at: &NodeId, stream: &StreamId) {
        if let Some(node) = self.nodes.get_mut(at) {
            let effects = node.node.detach_stream(stream);
            self.carry(at, effects);
        }
    }

    /// The edge of node `at`, as a [`Gateway`](crate::edge::Gateway) for the use cases.
    pub fn gateway(&mut self, at: &NodeId) -> DriverGateway<'_, P, K> {
        DriverGateway {
            driver: self,
            at: at.clone(),
        }
    }

    /// Takes what a receiver holds now.
    pub fn drain<T>(receiver: &mut UnboundedReceiver<T>) -> Vec<T> {
        let mut items = Vec::new();
        while let Ok(item) = receiver.try_recv() {
            items.push(item);
        }
        items
    }
}

/// The edge of a node of the driver: requests go to the owner of their partition, through the
/// driver's link when another node owns it, and the driver steps until they are answered.
pub struct DriverGateway<'a, P: Plane, K: Cluster + 'static> {
    driver: &'a mut Driver<P, K>,
    at: NodeId,
}

impl<P: Plane, K: Cluster + 'static> fmt::Debug for DriverGateway<'_, P, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverGateway")
            .field("at", &self.at)
            .finish()
    }
}

impl<P: Plane, K: Cluster + 'static> crate::edge::Gateway for DriverGateway<'_, P, K> {
    async fn partition(
        &mut self,
        partition: PartitionId,
        request: crate::ports::link::PartitionRequest,
    ) -> Result<crate::ports::link::PartitionReply, crate::edge::EdgeError> {
        let reply = self
            .driver
            .route(&self.at, partition, request)
            .await
            .map_err(|error| crate::edge::EdgeError::Unavailable(error.to_string()))?;
        crate::edge::partition_reply(reply)
    }
}

fn min(a: Option<Timestamp>, b: Option<Timestamp>) -> Option<Timestamp> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
