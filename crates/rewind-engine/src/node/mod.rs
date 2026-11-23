//! A node of the run plane (`archi/07-code-architecture.md`, "Inside a node", "`Cluster` and
//! `NodeLink`"; `archi/06-storage-and-scale.md`, "Partitions, owners and nodes").
//!
//! A [`Node`] holds the actors of one process: the owners of the partitions it holds (and the
//! organization actors they host), its executors, its call executor, and the node actor, which
//! keeps the partition map, opens the partitions the map gives the node and hands over those it
//! loses. It routes what the actors send each other: to an actor of the node, or to another
//! node through the link. It holds the edge's side of the SDK streams and of the subscriptions.
//!
//! The node does no I/O: whoever drives it ([`runtime`] in a process, the
//! [`driver`](crate::driver) in tests) runs its jobs, delivers its messages, carries its remote
//! requests and fires its wakes.
//!
//! Partitions go to nodes by rendezvous hashing over the live nodes that own partitions: a node
//! that joins takes the partitions it wins, a node that leaves or dies gives each of its
//! partitions to the node that wins it among the others.

pub(crate) mod message;
pub mod runtime;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use rewind_core::{Digest, Epoch, PartitionId, Timestamp};
use tokio::sync::mpsc::UnboundedSender;

use crate::executors::{ExecMsg, Executors};
use crate::org::OrgMsg;
use crate::partition::{Owner, OwnerMsg};
use crate::plane::{NodeConfig, Plane};
use crate::ports::cluster::Membership;
use crate::ports::link::{
    Changed, EdgeNotice, HostTask, NodeReply, NodeRequest, OrgRequest, StreamAttach, StreamId,
    SubscriptionKey,
};
use crate::ports::{
    ControlPlaneChange, Moves, NodeId, NodeInfo, PartitionMap, ScriptExecutor, TenantId,
    home_partition,
};
use crate::tasks::calls::Calls;
use message::{Effect, Message, ReplyTo, Target, job};

/// A message to the node actor.
#[derive(Debug)]
pub(crate) enum NodeMsg {
    /// The owner of a partition was fenced: another node opened it.
    Fenced(PartitionId),
    /// A partition could not be opened.
    OpenFailed(PartitionId, String),
    /// Open the partition again, if the map still gives it to the node.
    Reopen(PartitionId),
    /// Runs changed: tell the subscribed edges.
    Publish(Vec<Changed>),
    /// The partition map, as the cluster committed it.
    Map(PartitionMap),
    /// The members of the cluster.
    Nodes(Vec<NodeInfo>),
    /// The members, and the map once the node moved what it had to.
    Rebalanced {
        map: PartitionMap,
        nodes: Vec<NodeInfo>,
    },
    /// A change of the control plane.
    Change(ControlPlaneChange),
    /// The node this one watches left the cluster.
    Departed(NodeId),
    /// An owner needs the members again: one may have joined.
    RefreshNodes,
    /// A request to the membership failed; it is tried again.
    MembershipFailed(String),
}

/// A node: the actors of a process.
pub struct Node<P: Plane, M: Membership + 'static> {
    pub(crate) config: Arc<NodeConfig>,
    pub(crate) plane: Arc<P>,
    pub(crate) membership: Arc<M>,
    pub(crate) map: PartitionMap,
    pub(crate) nodes: Vec<NodeInfo>,
    pub(crate) owners: BTreeMap<PartitionId, Owner<P>>,
    pub(crate) executors: Executors<P>,
    pub(crate) calls: Calls<P>,
    epochs: BTreeSet<Epoch>,
    subscriptions: BTreeMap<SubscriptionKey, Vec<UnboundedSender<Changed>>>,
    streams: BTreeMap<StreamId, (TenantId, StreamAttach, UnboundedSender<HostTask>)>,
}

impl<P: Plane, M: Membership + 'static> fmt::Debug for Node<P, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("node", &self.config.node)
            .field("owners", &self.owners.keys().collect::<Vec<_>>())
            .field("executors", &self.executors)
            .field("calls", &self.calls)
            .finish_non_exhaustive()
    }
}

/// The winner of a partition among nodes: the highest hash of the partition and the node's id
/// (rendezvous hashing).
pub fn rendezvous<'a>(
    partition: PartitionId,
    nodes: impl IntoIterator<Item = &'a NodeId>,
) -> Option<&'a NodeId> {
    nodes.into_iter().max_by_key(|node| {
        let digest = Digest::of(format!("{partition}/{node}").as_bytes());
        let mut first = [0u8; 8];
        first.copy_from_slice(&digest.as_bytes()[..8]);
        (u64::from_be_bytes(first), (*node).clone())
    })
}

/// The nodes that own partitions.
fn owners_of(nodes: &[NodeInfo]) -> Vec<&NodeId> {
    nodes
        .iter()
        .filter(|node| !node.executors_only)
        .map(|node| &node.id)
        .collect()
}

impl<P: Plane, M: Membership + 'static> Node<P, M> {
    /// A node of the cluster `membership` joined. Its first job reads the members and the map
    /// and takes the partitions it wins.
    pub fn new(plane: Arc<P>, config: NodeConfig, membership: Arc<M>) -> Self {
        let config = Arc::new(config);
        let epochs = config
            .epochs
            .clone()
            .unwrap_or_else(|| plane.executor().epochs().iter().copied().collect());
        Self {
            executors: Executors::new(Arc::clone(&plane), Arc::clone(&config)),
            calls: Calls::new(Arc::clone(&plane), Arc::clone(&config)),
            config,
            plane,
            membership,
            map: PartitionMap::default(),
            nodes: Vec::new(),
            owners: BTreeMap::new(),
            epochs,
            subscriptions: BTreeMap::new(),
            streams: BTreeMap::new(),
        }
    }

    pub fn id(&self) -> &NodeId {
        &self.config.node
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn plane(&self) -> &Arc<P> {
        &self.plane
    }

    /// The partitions the node holds open.
    pub fn partitions(&self) -> Vec<PartitionId> {
        self.owners
            .iter()
            .filter(|(_, owner)| !owner.is_closed())
            .map(|(partition, _)| *partition)
            .collect()
    }

    /// The partition map the node knows.
    pub fn map(&self) -> &PartitionMap {
        &self.map
    }

    /// The instances its executors keep.
    pub fn cached_instances(&self) -> usize {
        self.executors.cached()
    }

    /// Reads the members and the map, and takes the partitions the node wins, the unowned
    /// ones, and those of nodes that are gone.
    pub(crate) fn start(&mut self) -> Vec<Effect<P>> {
        vec![self.rebalance(None)]
    }

    /// Hands the node's partitions to the others, before it leaves.
    pub(crate) fn hand_over(&mut self) -> Vec<Effect<P>> {
        let membership = Arc::clone(&self.membership);
        let me = self.config.node.clone();
        vec![job(async move {
            let result = async {
                let nodes = membership.nodes().await?;
                let map = membership.partition_map().await?;
                let others: Vec<&NodeId> = owners_of(&nodes)
                    .into_iter()
                    .filter(|node| **node != me)
                    .collect();
                let mut moves = Moves::new();
                for partition in map.owned_by(&me) {
                    if let Some(winner) = rendezvous(partition, others.iter().copied()) {
                        moves = moves.to(partition, winner.clone());
                    }
                }
                membership.assign(&moves).await
            }
            .await;
            match result {
                Ok(map) => Message::Node(NodeMsg::Map(map)),
                Err(error) => Message::Node(NodeMsg::MembershipFailed(error.to_string())),
            }
        })]
    }

    /// The job that reads the members and moves partitions: those of `departed`, or (at a
    /// join) those the node wins, the unowned ones and those of nodes that are gone.
    fn rebalance(&self, departed: Option<NodeId>) -> Effect<P> {
        let membership = Arc::clone(&self.membership);
        let me = self.config.node.clone();
        let executors_only = self.config.executors_only;
        job(async move {
            let result = async {
                let nodes = membership.nodes().await?;
                let map = membership.partition_map().await?;
                let live = owners_of(&nodes);
                let alive: BTreeSet<&NodeId> = nodes.iter().map(|node| &node.id).collect();
                let mut moves = Moves::new();
                for (partition, owner) in &map.partitions {
                    let Some(winner) = rendezvous(*partition, live.iter().copied()) else {
                        continue;
                    };
                    let gone = owner.node.as_ref().is_none_or(|node| {
                        !alive.contains(node) || departed.as_ref() == Some(node)
                    });
                    let mine = !executors_only && *winner == me && owner.node.as_ref() != Some(&me);
                    if gone || mine {
                        moves = moves.to(*partition, winner.clone());
                    }
                }
                let map = if moves.moves.is_empty() {
                    map
                } else {
                    membership.assign(&moves).await?
                };
                Ok::<_, crate::ports::ClusterError>((map, nodes))
            }
            .await;
            match result {
                Ok((map, nodes)) => Message::Node(NodeMsg::Rebalanced { map, nodes }),
                Err(error) => Message::Node(NodeMsg::MembershipFailed(error.to_string())),
            }
        })
    }

    /// Handles a message; returns the effects to carry, routed.
    pub(crate) fn handle(&mut self, message: Message<P>) -> Vec<Effect<P>> {
        let effects = match message {
            Message::Owner(partition, message) => match self.owners.get_mut(&partition) {
                Some(owner) => owner.handle(message),
                None => refuse_missing_owner(partition, message),
            },
            Message::Executors(message) => self.executors.handle(message),
            Message::Calls(message) => self.calls.handle(message),
            Message::Node(message) => self.node(message),
            Message::Inbound(request, reply) => self.inbound(request, reply),
            Message::InboundActivation(request, reply) => {
                self.executors.handle(ExecMsg::Remote(request, reply))
            }
            Message::Answer(asker, answer) => match self.owners.get_mut(&asker.partition) {
                Some(owner) => owner.handle(OwnerMsg::Answer {
                    tag: asker.tag,
                    answer,
                }),
                None => Vec::new(),
            },
            Message::Reply(to, reply) => vec![Effect::Reply(to, reply)],
            Message::Nothing => Vec::new(),
        };
        self.route(effects)
    }

    /// Resolves where each request goes: an actor of this node, or another node.
    fn route(&mut self, effects: Vec<Effect<P>>) -> Vec<Effect<P>> {
        let mut routed = Vec::with_capacity(effects.len());
        for effect in effects {
            match effect {
                Effect::Request { to, request, asker } => {
                    let node = match &to {
                        Target::Partition(partition) => self.map.owner(*partition).cloned(),
                        Target::Home(tenant) => self
                            .map
                            .owner(home_partition(tenant, self.config.partitions))
                            .cloned(),
                        Target::Node(node) => Some(node.clone()),
                    };
                    let reply = asker.map_or(ReplyTo::Nobody, ReplyTo::Owner);
                    match node {
                        Some(node) if node == self.config.node => {
                            routed.push(Effect::Local(Message::Inbound(request, reply)));
                        }
                        Some(node) => routed.push(Effect::Remote {
                            node,
                            request,
                            asker,
                        }),
                        None => {
                            if let Some(asker) = asker {
                                let error = crate::ports::LinkError::Failed(format!(
                                    "no node holds {to:?}"
                                ));
                                routed.push(Effect::Local(Message::Answer(asker, Err(error))));
                            }
                        }
                    }
                }
                Effect::Reply(ReplyTo::Owner(asker), reply) => {
                    routed.push(Effect::Local(Message::Answer(asker, Ok(reply))));
                }
                Effect::Reply(ReplyTo::Nobody, _) => {}
                other => routed.push(other),
            }
        }
        routed
    }

    /// A request that reached the node.
    fn inbound(&mut self, request: NodeRequest, reply: ReplyTo) -> Vec<Effect<P>> {
        match request {
            NodeRequest::Partition { partition, request } => {
                match self.owners.get_mut(&partition) {
                    Some(owner) if !owner.is_closed() => {
                        owner.handle(OwnerMsg::Request(request, reply))
                    }
                    _ => vec![Effect::Reply(reply, NodeReply::NotOwner { partition })],
                }
            }
            NodeRequest::Organization { tenant, request } => {
                let home = home_partition(&tenant, self.config.partitions);
                match self.owners.get_mut(&home) {
                    Some(owner) if !owner.is_closed() => owner.handle(OwnerMsg::Org {
                        tenant,
                        message: OrgMsg::Request(request, reply),
                    }),
                    _ => vec![Effect::Reply(
                        reply,
                        NodeReply::NotOwner { partition: home },
                    )],
                }
            }
            NodeRequest::Owner { partition, notice } => match self.owners.get_mut(&partition) {
                Some(owner) if !owner.is_closed() => {
                    let mut effects = owner.handle(OwnerMsg::Notice(notice));
                    effects.push(Effect::Reply(reply, NodeReply::Done));
                    effects
                }
                _ => vec![Effect::Reply(reply, NodeReply::NotOwner { partition })],
            },
            NodeRequest::Edge(notice) => {
                let EdgeNotice::HandOut { stream, task } = *notice;
                let mut effects = vec![Effect::Reply(reply, NodeReply::Done)];
                let delivered = self
                    .streams
                    .get(&stream)
                    .is_some_and(|(_, _, sink)| sink.send(task.clone()).is_ok());
                if !delivered {
                    // The stream closed: its organization actor gives its tasks their grace.
                    self.streams.remove(&stream);
                    let tenant = task.task.run.tenant.clone();
                    effects.push(Effect::Request {
                        to: Target::Home(tenant.clone()),
                        request: NodeRequest::Organization {
                            tenant,
                            request: Box::new(OrgRequest::Detach { stream }),
                        },
                        asker: None,
                    });
                }
                effects
            }
        }
    }

    fn node(&mut self, message: NodeMsg) -> Vec<Effect<P>> {
        match message {
            NodeMsg::Fenced(partition) => {
                self.owners.remove(&partition);
                Vec::new()
            }
            NodeMsg::OpenFailed(partition, error) => {
                tracing::error!(node = %self.config.node, %partition, %error, "the partition could not be opened: tried again");
                self.owners.remove(&partition);
                let at = crate::ports::Clock::now(self.plane.clock())
                    .saturating_add(std::time::Duration::from_secs(1));
                vec![Effect::Wake {
                    at,
                    alone: false,
                    message: Message::Node(NodeMsg::Reopen(partition)),
                }]
            }
            NodeMsg::Reopen(partition) => {
                if self.map.owner(partition) == Some(&self.config.node)
                    && !self.owners.contains_key(&partition)
                {
                    self.open(partition)
                } else {
                    Vec::new()
                }
            }
            NodeMsg::Publish(changed) => {
                self.publish(changed);
                Vec::new()
            }
            NodeMsg::Map(map) => self.apply_map(map),
            NodeMsg::Nodes(nodes) => {
                self.nodes.clone_from(&nodes);
                let mut effects = Vec::new();
                for owner in self.owners.values_mut() {
                    effects.extend(owner.handle(OwnerMsg::Nodes(nodes.clone())));
                }
                effects
            }
            NodeMsg::Rebalanced { map, nodes } => {
                let mut effects = self.node(NodeMsg::Nodes(nodes));
                effects.extend(self.apply_map(map));
                effects
            }
            NodeMsg::Change(change) => match change {
                ControlPlaneChange::PartitionMap(map) => {
                    let mut effects = self.apply_map(map);
                    effects.push(self.read_nodes());
                    effects
                }
                change => {
                    let mut effects = Vec::new();
                    for owner in self.owners.values_mut() {
                        effects.extend(owner.handle(OwnerMsg::ControlPlane(change.clone())));
                    }
                    effects
                }
            },
            NodeMsg::Departed(node) => {
                tracing::warn!(node = %self.config.node, departed = %node, "a node left the cluster: its partitions move");
                vec![self.rebalance(Some(node))]
            }
            NodeMsg::RefreshNodes => vec![self.read_nodes()],
            NodeMsg::MembershipFailed(error) => {
                tracing::error!(node = %self.config.node, %error, "the cluster could not be reached");
                Vec::new()
            }
        }
    }

    fn read_nodes(&self) -> Effect<P> {
        let membership = Arc::clone(&self.membership);
        job(async move {
            match membership.nodes().await {
                Ok(nodes) => Message::Node(NodeMsg::Nodes(nodes)),
                Err(error) => Message::Node(NodeMsg::MembershipFailed(error.to_string())),
            }
        })
    }

    /// The map changed: the node opens the partitions it gained and closes those it lost; the
    /// streams of the tenants whose home moved are attached again.
    fn apply_map(&mut self, map: PartitionMap) -> Vec<Effect<P>> {
        let stale = map.partitions.iter().any(|(partition, owner)| {
            self.map
                .partitions
                .get(partition)
                .is_some_and(|known| known.epoch > owner.epoch)
        });
        if stale {
            return Vec::new();
        }
        let previous = std::mem::replace(&mut self.map, map);
        let me = self.config.node.clone();
        let mut effects = Vec::new();
        let gained: Vec<PartitionId> = self
            .map
            .owned_by(&me)
            .into_iter()
            .filter(|partition| {
                self.owners.get(partition).is_none_or(|owner| {
                    owner.is_closed() || previous.epoch(*partition) != self.map.epoch(*partition)
                })
            })
            .collect();
        let lost: Vec<PartitionId> = self
            .owners
            .keys()
            .filter(|partition| self.map.owner(**partition) != Some(&me))
            .copied()
            .collect();
        for partition in lost {
            if let Some(mut owner) = self.owners.remove(&partition) {
                tracing::info!(node = %me, %partition, "partition handed over");
                effects.extend(owner.handle(OwnerMsg::Close));
            }
        }
        for partition in gained {
            effects.extend(self.open(partition));
        }
        // An organization actor whose home moved lost its streams: they are attached again.
        let moved: Vec<StreamAttach> = self
            .streams
            .values()
            .filter(|(tenant, _, _)| {
                let home = home_partition(tenant, self.config.partitions);
                previous.owner(home) != self.map.owner(home)
            })
            .map(|(_, attach, _)| attach.clone())
            .collect();
        for attach in moved {
            if let Some((tenant, _, _)) = self.streams.get(&attach.stream) {
                let tenant = tenant.clone();
                effects.push(Effect::Request {
                    to: Target::Home(tenant.clone()),
                    request: NodeRequest::Organization {
                        tenant,
                        request: Box::new(OrgRequest::Attach(attach)),
                    },
                    asker: None,
                });
            }
        }
        effects
    }

    /// Opens a partition: its owner's first job opens and recovers it.
    fn open(&mut self, partition: PartitionId) -> Vec<Effect<P>> {
        if self.config.executors_only {
            return Vec::new();
        }
        let epoch = self.map.epoch(partition).unwrap_or_default();
        let (owner, effects) = Owner::open(
            partition,
            epoch,
            Arc::clone(&self.plane),
            Arc::clone(&self.config),
            self.epochs.clone(),
            self.nodes.clone(),
        );
        tracing::info!(node = %self.config.node, %partition, %epoch, "partition opened");
        self.owners.insert(partition, owner);
        effects
    }

    /// Tells the subscribers what changed.
    fn publish(&mut self, changed: Vec<Changed>) {
        if self.subscriptions.is_empty() {
            return;
        }
        for change in changed {
            let keys = [
                SubscriptionKey::Run(change.run.clone()),
                SubscriptionKey::Namespace {
                    tenant: change.run.tenant.clone(),
                    namespace: change.namespace.clone(),
                },
                SubscriptionKey::Tenant(change.run.tenant.clone()),
            ];
            for key in keys {
                let entries = matches!(key, SubscriptionKey::Run(_));
                if let Some(sinks) = self.subscriptions.get_mut(&key) {
                    let mut change = change.clone();
                    if !entries {
                        change.entries.clear();
                    }
                    sinks.retain(|sink| sink.send(change.clone()).is_ok());
                }
            }
        }
        self.subscriptions.retain(|_, sinks| !sinks.is_empty());
    }

    /// Subscribes a sink to a key. For a run and `from`, the entries from `from` are sent
    /// first, read from the run's partition when this node holds it.
    pub(crate) fn subscribe(
        &mut self,
        key: SubscriptionKey,
        from: Option<u64>,
        sink: UnboundedSender<Changed>,
    ) -> Vec<Effect<P>> {
        let mut effects = Vec::new();
        if let (SubscriptionKey::Run(run), Some(from)) = (&key, from) {
            let partition = self.partition_of(run);
            if let Some(log) = partition
                .and_then(|partition| self.owners.get(&partition))
                .and_then(|owner| owner.log.clone())
            {
                let run = run.clone();
                let plane = Arc::clone(&self.plane);
                let sink = sink.clone();
                effects.push(job(async move {
                    use crate::ports::PartitionLog;
                    if let Ok(Some(head)) = log.run(&run).await
                        && let Ok(history) =
                            crate::partition::history_of(&*plane, &*log, &head).await
                    {
                        let summary = log.summary(&run).await.ok().flatten();
                        let _ = sink.send(Changed {
                            run: run.clone(),
                            namespace: head.namespace.clone(),
                            summary: summary.map(|indexed| indexed.summary),
                            entries: history
                                .into_iter()
                                .filter(|entry| entry.index >= from)
                                .collect(),
                        });
                    }
                    Message::Nothing
                }));
            }
        }
        self.subscriptions.entry(key).or_default().push(sink);
        effects
    }

    /// The partition of a run, from its id.
    fn partition_of(&self, run: &crate::ports::RunRef) -> Option<PartitionId> {
        match run.id.partition() {
            Some(partition) => Some(partition),
            None if self.config.partitions.get() == 1 => Some(PartitionId::new(0)),
            None => None,
        }
    }

    /// An SDK stream opened at this edge.
    pub(crate) fn attach_stream(
        &mut self,
        tenant: TenantId,
        attach: StreamAttach,
        sink: UnboundedSender<HostTask>,
    ) -> Vec<Effect<P>> {
        self.streams.insert(
            attach.stream.clone(),
            (tenant.clone(), attach.clone(), sink),
        );
        self.route(vec![Effect::Request {
            to: Target::Home(tenant.clone()),
            request: NodeRequest::Organization {
                tenant,
                request: Box::new(OrgRequest::Attach(attach)),
            },
            asker: None,
        }])
    }

    /// An SDK stream closed at this edge.
    pub(crate) fn detach_stream(&mut self, stream: &StreamId) -> Vec<Effect<P>> {
        let Some((tenant, _, _)) = self.streams.remove(stream) else {
            return Vec::new();
        };
        self.route(vec![Effect::Request {
            to: Target::Home(tenant.clone()),
            request: NodeRequest::Organization {
                tenant,
                request: Box::new(OrgRequest::Detach {
                    stream: stream.clone(),
                }),
            },
            asker: None,
        }])
    }

    /// The timers and wakes due at `now`, as messages to handle.
    pub(crate) fn due(&mut self, now: Timestamp) -> Vec<Message<P>> {
        let mut due = Vec::new();
        for (partition, owner) in &mut self.owners {
            for message in owner.due(now) {
                due.push(Message::Owner(*partition, message));
            }
        }
        due
    }

    /// The earliest timer of the node's wheels, and whether it wakes the driver alone.
    pub(crate) fn next_timer(&self) -> Option<(Timestamp, bool)> {
        self.owners
            .values()
            .filter_map(Owner::next_timer)
            .min_by_key(|(at, alone)| (*at, !*alone))
    }

    /// The earliest timer that wakes the driver alone, and the earliest run deadline.
    pub(crate) fn wake_ups(&self) -> (Option<Timestamp>, Option<Timestamp>) {
        let waking = self
            .owners
            .values()
            .filter_map(Owner::next_waking_timer)
            .min();
        let deadline = self.owners.values().filter_map(Owner::next_deadline).min();
        (waking, deadline)
    }

    /// The earliest horizon of the node's wheels.
    pub(crate) fn next_horizon(&self) -> Option<Timestamp> {
        self.owners.values().map(Owner::horizon).min()
    }
}

/// A message for an owner the node does not have: its requests are answered `NotOwner`.
fn refuse_missing_owner<P: Plane>(partition: PartitionId, message: OwnerMsg<P>) -> Vec<Effect<P>> {
    match message {
        OwnerMsg::Request(_, reply)
        | OwnerMsg::Org {
            message: OrgMsg::Request(_, reply),
            ..
        } => vec![Effect::Reply(reply, NodeReply::NotOwner { partition })],
        _ => Vec::new(),
    }
}
