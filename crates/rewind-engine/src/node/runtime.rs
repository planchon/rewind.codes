//! A node in a process: its actors driven by tokio (`archi/07-code-architecture.md`, "Inside a
//! node").
//!
//! ```text
//!  NodeHandle ── requests, activations, subscriptions, SDK streams ──┐
//!  the link's server (rewind-server) ─── NodeHandle::serve ─────────┤
//!  membership: watch(), changes() ──────────────────────────────────┤
//!                                                                     ▼
//!                       the node's task: one message at a time to the node's actors
//!                        │ jobs: a task each           │ requests to other nodes: NodeLink
//!                        │ timers: the wheels' earliest│ replies: to their waiters
//! ```
//!
//! The actors' state lives in the node's task: they handle a message each in turn, and what
//! they ask for runs concurrently (commits of different partitions, activations, calls, reads).
//! The task waits on its channel and on the earliest timer of its wheels, nothing else.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_util::{Stream, StreamExt as _};
use rewind_core::{PartitionId, Timestamp};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

use crate::edge::{EdgeError, Gateway, partition_reply};
use crate::node::message::{Effect, Message, ReplyTo};
use crate::node::{Node, NodeMsg};
use crate::partition::OwnerMsg;
use crate::plane::{NodeConfig, Plane};
use crate::ports::cluster::Membership;
use crate::ports::link::{
    ActivationRequest, ActivationResult, Changed, HostTask, NodeReply, NodeRequest,
    PartitionReply, PartitionRequest, StreamAttach, StreamId, SubscriptionKey,
};
use crate::ports::{Clock, LinkError, NodeId, NodeLink, TenantId};

/// What reaches a node's task.
enum Input<P: Plane> {
    Message(Message<P>),
    Request(NodeRequest, oneshot::Sender<NodeReply>),
    /// An edge's request for the owner of a partition: routed by the node's map.
    Route(PartitionId, PartitionRequest, oneshot::Sender<Result<NodeReply, EdgeError>>),
    Activation(Box<ActivationRequest>, oneshot::Sender<ActivationResult>),
    Subscribe(SubscriptionKey, Option<u64>, UnboundedSender<Changed>),
    Attach(TenantId, StreamAttach, UnboundedSender<HostTask>),
    Detach(StreamId),
    /// Hand the partitions over and stop.
    Stop(oneshot::Sender<()>),
}

/// A node running in tokio tasks, and its edge.
pub struct NodeHandle<P: Plane> {
    inputs: UnboundedSender<Input<P>>,
    node: NodeId,
}

impl<P: Plane> Clone for NodeHandle<P> {
    fn clone(&self) -> Self {
        Self {
            inputs: self.inputs.clone(),
            node: self.node.clone(),
        }
    }
}

impl<P: Plane> fmt::Debug for NodeHandle<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeHandle").field("node", &self.node).finish()
    }
}

/// The node has stopped: its task is gone.
fn stopped<T>(_: T) -> LinkError {
    LinkError::Failed("the node has stopped".into())
}

impl<P: Plane> NodeHandle<P> {
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// A request that reached the node from the link: answered by its actors.
    pub async fn serve(&self, request: NodeRequest) -> Result<NodeReply, LinkError> {
        let (reply, answer) = oneshot::channel();
        self.inputs
            .send(Input::Request(request, reply))
            .map_err(stopped)?;
        answer.await.map_err(stopped)
    }

    /// An activation a remote owner sent: run by the node's executors.
    pub async fn serve_activation(&self, request: ActivationRequest) -> Result<ActivationResult, LinkError> {
        let (reply, answer) = oneshot::channel();
        self.inputs
            .send(Input::Activation(Box::new(request), reply))
            .map_err(stopped)?;
        answer.await.map_err(stopped)
    }

    /// Subscribes to the changes of a key at this node.
    pub fn subscribe(&self, key: SubscriptionKey, from: Option<u64>) -> Result<Changes, LinkError> {
        let (sink, changes) = unbounded_channel();
        self.inputs
            .send(Input::Subscribe(key, from, sink))
            .map_err(stopped)?;
        Ok(Changes(changes))
    }

    /// An SDK stream opened at this edge: the host tasks it is handed arrive on the receiver.
    /// Dropping the receiver does not close the stream: [`detach_stream`](Self::detach_stream)
    /// does, once the response it feeds ends.
    pub fn attach_stream(
        &self,
        tenant: TenantId,
        attach: StreamAttach,
    ) -> Result<UnboundedReceiver<HostTask>, LinkError> {
        let (sink, tasks) = unbounded_channel();
        self.inputs
            .send(Input::Attach(tenant, attach, sink))
            .map_err(stopped)?;
        Ok(tasks)
    }

    /// An SDK stream closed at this edge.
    pub fn detach_stream(&self, stream: StreamId) -> Result<(), LinkError> {
        self.inputs.send(Input::Detach(stream)).map_err(stopped)
    }

    /// Hands the node's partitions over to the other nodes, and stops its task.
    pub async fn stop(&self) -> Result<(), LinkError> {
        let (done, stopped_at) = oneshot::channel();
        self.inputs.send(Input::Stop(done)).map_err(stopped)?;
        stopped_at.await.map_err(stopped)
    }
}

impl<P: Plane> Gateway for NodeHandle<P> {
    async fn partition(
        &mut self,
        partition: PartitionId,
        request: PartitionRequest,
    ) -> Result<PartitionReply, EdgeError> {
        // An owner that answers NotOwner moved: the map the node holds is newer by the time the
        // request goes again.
        let mut last = None;
        for _ in 0..3 {
            let (reply, answer) = oneshot::channel();
            self.inputs
                .send(Input::Route(partition, request.clone(), reply))
                .map_err(|_| EdgeError::Unavailable("the node has stopped".into()))?;
            let answer = answer
                .await
                .map_err(|_| EdgeError::Unavailable("the node has stopped".into()))??;
            match answer {
                NodeReply::NotOwner { .. } => {
                    last = Some(answer);
                    tokio::task::yield_now().await;
                }
                answer => return partition_reply(answer),
            }
        }
        partition_reply(last.unwrap_or(NodeReply::NotOwner { partition }))
    }
}

/// The changes a subscription streams.
#[derive(Debug)]
pub struct Changes(UnboundedReceiver<Changed>);

impl Stream for Changes {
    type Item = Changed;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Changed>> {
        self.0.poll_recv(cx)
    }
}

/// Starts a node of the cluster `membership` joined, its remote requests carried by `link`.
/// Returns its handle; the node's task runs until [`NodeHandle::stop`].
pub fn spawn<P, M, L>(plane: Arc<P>, config: NodeConfig, membership: M, link: Arc<L>) -> NodeHandle<P>
where
    P: Plane,
    M: Membership + 'static,
    L: NodeLink + 'static,
{
    let (inputs, receiver) = unbounded_channel();
    let membership = Arc::new(membership);
    let id = config.node.clone();
    let node = Node::new(Arc::clone(&plane), config, Arc::clone(&membership));
    watch_membership(Arc::clone(&membership), inputs.clone());
    let task = Task {
        node,
        plane,
        link,
        inputs: inputs.clone(),
        receiver,
        local: VecDeque::new(),

        routes: BTreeMap::new(),
        links: BTreeMap::new(),
        activations: BTreeMap::new(),
        wakes: BTreeMap::new(),
        next_id: 1,
    };
    tokio::spawn(task.run());
    NodeHandle { inputs, node: id }
}

/// The membership's events, to the node's task: the departure of the node it watches, and the
/// changes of the control plane.
fn watch_membership<P: Plane, M: Membership + 'static>(membership: Arc<M>, inputs: UnboundedSender<Input<P>>) {
    let watched = Arc::clone(&membership);
    let departures = inputs.clone();
    tokio::spawn(async move {
        while let Ok(departed) = watched.watch().await {
            let message = Message::Node(NodeMsg::Departed(departed.node));
            if departures.send(Input::Message(message)).is_err() {
                return;
            }
        }
    });
    tokio::spawn(async move {
        let Ok(mut changes) = membership.changes().await else {
            return;
        };
        while let Some(change) = changes.next().await {
            if inputs
                .send(Input::Message(Message::Node(NodeMsg::Change(change))))
                .is_err()
            {
                return;
            }
        }
    });
}

/// The node's task.
struct Task<P: Plane, M: Membership + 'static, L: NodeLink> {
    node: Node<P, M>,
    plane: Arc<P>,
    link: Arc<L>,
    inputs: UnboundedSender<Input<P>>,
    receiver: UnboundedReceiver<Input<P>>,
    local: VecDeque<Message<P>>,

    routes: BTreeMap<u64, oneshot::Sender<Result<NodeReply, EdgeError>>>,
    links: BTreeMap<u64, oneshot::Sender<NodeReply>>,
    activations: BTreeMap<u64, oneshot::Sender<ActivationResult>>,
    wakes: BTreeMap<(Timestamp, u64), Message<P>>,
    next_id: u64,
}

impl<P: Plane, M: Membership + 'static, L: NodeLink + 'static> Task<P, M, L> {
    async fn run(mut self) {
        let effects = self.node.start();
        self.carry(effects);
        loop {
            self.drain_local();
            let next = self.next_timer();
            let input = match next {
                Some(at) => {
                    tokio::select! {
                        input = self.receiver.recv() => input,
                        () = crate::partition::wheel::until(self.plane.clock(), at) => {
                            self.fire(at);
                            continue;
                        }
                    }
                }
                None => self.receiver.recv().await,
            };
            let Some(input) = input else {
                return;
            };
            if self.input(input) {
                return;
            }
        }
    }

    /// The earliest of the wheels' timers and horizons, and of the wakes.
    fn next_timer(&self) -> Option<Timestamp> {
        let timers = self.node.next_timer().map(|(at, _)| at);
        let horizon = self.node.next_horizon();
        let wake = self.wakes.keys().next().map(|(at, _)| *at);
        [timers, horizon, wake].into_iter().flatten().min()
    }

    fn fire(&mut self, now: Timestamp) {
        let now = now.max(self.plane.clock().now());
        for message in self.node.due(now) {
            self.local.push_back(message);
        }
        let due: Vec<(Timestamp, u64)> = self
            .wakes
            .range(..(now, u64::MAX))
            .map(|(key, _)| *key)
            .collect();
        for key in due {
            if let Some(message) = self.wakes.remove(&key) {
                self.local.push_back(message);
            }
        }
    }

    /// Handles an input; returns whether the task stops.
    fn input(&mut self, input: Input<P>) -> bool {
        match input {
            Input::Message(message) => self.local.push_back(message),
            Input::Request(request, reply) => {
                let id = self.id();
                self.links.insert(id, reply);
                self.local
                    .push_back(Message::Inbound(request, ReplyTo::Link(id)));
            }
            Input::Route(partition, request, reply) => self.route(partition, request, reply),
            Input::Activation(request, reply) => {
                let id = self.id();
                self.activations.insert(id, reply);
                self.local
                    .push_back(Message::InboundActivation(request, ReplyTo::Link(id)));
            }
            Input::Subscribe(key, from, sink) => {
                let effects = self.node.subscribe(key, from, sink);
                self.carry(effects);
            }
            Input::Attach(tenant, attach, sink) => {
                let effects = self.node.attach_stream(tenant, attach, sink);
                self.carry(effects);
            }
            Input::Detach(stream) => {
                let effects = self.node.detach_stream(&stream);
                self.carry(effects);
            }
            Input::Stop(done) => {
                let effects = self.node.hand_over();
                self.carry(effects);
                self.drain_local();
                let _ = done.send(());
                return true;
            }
        }
        false
    }

    /// An edge's request: to the local owner, or through the link to the owner on another
    /// node.
    fn route(
        &mut self,
        partition: PartitionId,
        request: PartitionRequest,
        reply: oneshot::Sender<Result<NodeReply, EdgeError>>,
    ) {
        let request = NodeRequest::Partition {
            partition,
            request: Box::new(request),
        };
        match self.node.map().owner(partition).cloned() {
            Some(owner) if owner == *self.node.id() => {
                let id = self.id();
                self.routes.insert(id, reply);
                self.local
                    .push_back(Message::Inbound(request, ReplyTo::Waiter(id)));
            }
            Some(owner) => {
                let link = Arc::clone(&self.link);
                tokio::spawn(async move {
                    let answer = link
                        .call(&owner, request)
                        .await
                        .map_err(|error| EdgeError::Unavailable(error.to_string()));
                    let _ = reply.send(answer);
                });
            }
            None => {
                let _ = reply.send(Err(EdgeError::Unavailable(format!(
                    "no node owns partition {partition}"
                ))));
            }
        }
    }

    fn id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn drain_local(&mut self) {
        while let Some(message) = self.local.pop_front() {
            let effects = self.node.handle(message);
            self.carry(effects);
        }
    }

    fn carry(&mut self, effects: Vec<Effect<P>>) {
        for effect in effects {
            match effect {
                Effect::Local(message) => self.local.push_back(message),
                Effect::Request { .. } => tracing::error!("a request left a node unrouted"),
                Effect::Remote {
                    node,
                    request,
                    asker,
                } => {
                    let link = Arc::clone(&self.link);
                    let inputs = self.inputs.clone();
                    tokio::spawn(async move {
                        let answer = link.call(&node, request).await;
                        if let Some(asker) = asker {
                            let _ = inputs.send(Input::Message(Message::Answer(asker, answer)));
                        } else if let Err(error) = answer {
                            tracing::warn!(%node, %error, "a notice was lost on the link");
                        }
                    });
                }
                Effect::Reply(to, reply) => match to {
                    ReplyTo::Waiter(id) => {
                        if let Some(route) = self.routes.remove(&id) {
                            let _ = route.send(Ok(reply));
                        }
                    }
                    ReplyTo::Link(id) => {
                        if let Some(waiter) = self.links.remove(&id) {
                            let _ = waiter.send(reply);
                        }
                    }
                    ReplyTo::Owner(asker) => {
                        self.local.push_back(Message::Answer(asker, Ok(reply)));
                    }
                    ReplyTo::Nobody => {}
                },
                Effect::Job(_, job) => {
                    let inputs = self.inputs.clone();
                    tokio::spawn(async move {
                        let message = job.await;
                        let _ = inputs.send(Input::Message(message));
                    });
                }
                Effect::Wake { at, message, .. } => {
                    let id = self.id();
                    self.wakes.insert((at, id), message);
                }
                Effect::Activate {
                    node,
                    request,
                    asker,
                } => {
                    let link = Arc::clone(&self.link);
                    let inputs = self.inputs.clone();
                    let run = request.head.run.id;
                    tokio::spawn(async move {
                        let result = link
                            .activate(&node, *request)
                            .await
                            .unwrap_or_else(|error| ActivationResult::Failed {
                                error: error.to_string(),
                            });
                        let message = Message::Owner(
                            asker.partition,
                            OwnerMsg::Activated {
                                run,
                                result: Box::new(result),
                            },
                        );
                        let _ = inputs.send(Input::Message(message));
                    });
                }
                Effect::ActivationReply(to, result) => {
                    if let ReplyTo::Link(id) = to
                        && let Some(waiter) = self.activations.remove(&id)
                    {
                        let _ = waiter.send(*result);
                    }
                }
            }
        }
    }
}
