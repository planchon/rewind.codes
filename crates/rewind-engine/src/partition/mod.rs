//! The owner of a partition (`archi/07-code-architecture.md`, "Inside a node";
//! `archi/06-storage-and-scale.md`, "Partitions, owners and nodes", "The outbox").
//!
//! The only writer of its partition. It takes one message at a time from its mailbox: the
//! edge's requests, the executors' results, the call executor's outcomes, `TimerDue` from its
//! wheel, the notices of the organization actors, and the answers to what it asked. It answers
//! with effects, never with I/O:
//!
//! ```text
//! mailbox ─► handle ─► units (changes + what follows once durable) ─► committer: one batch
//!                                                                      of every unit pending
//!   Durable ─► fan-out: replies, runs to the ready queue, tasks on their way, timers to the
//!              wheel, ended attempts to the sealer, events to the projections, counts to the
//!              organization actors, Changed to the subscribed edges
//! ```
//!
//! The owner keeps in memory what it needs of the runs it touches (their heads, their inbox
//! ids, their call tasks, the calls in flight), loaded once from the partition and kept up to
//! date by its own batches, since it is their only writer. A message is never the only trace of
//! anything: an owner that opens the partition after another rebuilds the messages from the
//! records ([`recovery`]).

mod attempt;
mod commit;
mod outbox;
mod reads;
mod recovery;
pub(crate) mod runs;
mod tasks;
pub(crate) mod tokens;
pub(crate) mod wheel;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use rewind_core::{ActionSet, Digest, Epoch, PartitionId, RunId, RunMode, Seq, Timestamp};

use crate::node::message::{Effect, Message, ReplyTo, job};
use crate::org::{OrgActor, OrgEffect, OrgMsg};
use crate::plane::{LogOf, NodeConfig, Plane};
use crate::ports::link::{
    ActivationResult, NodeReply, OwnerNotice, PartitionReply, PartitionRequest, Purpose, Refusal,
    StartAtOwner,
};
use crate::ports::partition::Change;
use crate::ports::{
    CatalogStore, ControlPlaneChange, DueTimer, IndexedSummary, LinkError, NamespaceName, NodeId,
    NodeInfo, OrganizationState, OutboxEntry, OutboxId, PartitionEpoch, PartitionError,
    PartitionLog, RunHead, RunRef, TaskRecord, TaskRef, TenantId, home_partition,
};
use crate::tasks::calls::CallDone;
pub(crate) use attempt::{AttemptRead, read_attempt};
use commit::Unit;
pub(crate) use reads::buckets;
pub(crate) use recovery::Opened;
use wheel::Wheel;

/// A message to the owner of a partition.
#[allow(
    clippy::large_enum_variant,
    reason = "a message is moved once per step: its size does not matter"
)]
pub(crate) enum OwnerMsg<P: Plane> {
    /// The partition is open and recovered.
    Opened(Box<Opened<P>>),
    /// The partition could not be opened.
    OpenFailed(String),
    /// A request of an edge.
    Request(Box<PartitionRequest>, ReplyTo),
    /// A notice of an organization actor.
    Notice(Box<OwnerNotice>),
    /// A message to an organization actor homed in the partition.
    Org { tenant: TenantId, message: OrgMsg },
    /// The answer to a request the owner asked, by its tag.
    Answer {
        tag: u64,
        answer: Result<NodeReply, LinkError>,
    },
    /// A run read from the partition.
    Loaded {
        run: RunId,
        loaded: Result<Option<Box<Loaded>>, String>,
    },
    /// The batch being committed is durable, or was refused.
    Durable(Result<(), PartitionError>),
    /// An activation finished.
    Activated {
        run: RunId,
        result: Box<ActivationResult>,
    },
    /// What became of a delivery of a call.
    CallDone(Box<CallDone>),
    /// A timer of the wheel is due.
    TimerDue(DueTimer),
    /// The wheel's horizon is reached: the next hour's timers are read.
    Horizon,
    /// The next hour's timers.
    HorizonRead(Result<Vec<DueTimer>, String>),
    /// A job of a use case reports.
    Step(Box<runs::Step>),
    /// A change of the control plane.
    ControlPlane(ControlPlaneChange),
    /// The nodes of the cluster, for remote executors.
    Nodes(Vec<NodeInfo>),
    /// Something to do about a run once it is loaded.
    Internal(Internal),
    /// The partition moves to another node: the owner stops taking requests.
    Close,
    /// A job found the partition opened by another node: the request it answered, if any, is
    /// answered `NotOwner`, and the owner drops the partition.
    Fenced(Option<ReplyTo>),
    /// Try the commit that failed again.
    RetryCommit,
}

impl<P: Plane> OwnerMsg<P> {
    /// Who waits for an answer to this message: a request, the job of a request (a read, a
    /// start's key, a checked signal, a webhook), or a signal queued for its run. An owner that
    /// closed, or that the node no longer has, answers it `NotOwner`, and the asker finds the
    /// owner the map names; dropped, the asker would wait forever.
    pub(crate) fn into_reply(self) -> Option<ReplyTo> {
        match self {
            OwnerMsg::Request(_, reply)
            | OwnerMsg::Org {
                message: OrgMsg::Request(_, reply),
                ..
            }
            | OwnerMsg::Fenced(Some(reply))
            | OwnerMsg::Internal(Internal::Signal(_, reply)) => Some(reply),
            OwnerMsg::Step(step) => match *step {
                runs::Step::StartKey { reply, .. }
                | runs::Step::SignalChecked { reply, .. }
                | runs::Step::Webhook { reply, .. } => Some(reply),
                _ => None,
            },
            _ => None,
        }
    }
}

impl<P: Plane> fmt::Debug for OwnerMsg<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnerMsg::Opened(_) => f.write_str("Opened"),
            OwnerMsg::OpenFailed(error) => write!(f, "OpenFailed({error})"),
            OwnerMsg::Request(request, reply) => write!(f, "Request({request:?}, {reply:?})"),
            OwnerMsg::Notice(notice) => write!(f, "Notice({notice:?})"),
            OwnerMsg::Org { tenant, message } => write!(f, "Org({tenant}, {message:?})"),
            OwnerMsg::Answer { tag, answer } => write!(f, "Answer({tag}, {answer:?})"),
            OwnerMsg::Loaded { run, loaded } => {
                write!(
                    f,
                    "Loaded({run}, {})",
                    loaded.as_ref().map_or("error", |l| if l.is_some() {
                        "found"
                    } else {
                        "missing"
                    })
                )
            }
            OwnerMsg::Durable(result) => write!(f, "Durable({result:?})"),
            OwnerMsg::Activated { run, .. } => write!(f, "Activated({run})"),
            OwnerMsg::CallDone(done) => write!(f, "CallDone({}, {:?})", done.task, done.outcome),
            OwnerMsg::TimerDue(timer) => write!(f, "TimerDue({:?} at {:?})", timer.kind, timer.at),
            OwnerMsg::Horizon => f.write_str("Horizon"),
            OwnerMsg::HorizonRead(_) => f.write_str("HorizonRead"),
            OwnerMsg::Step(step) => write!(f, "Step({step:?})"),
            OwnerMsg::ControlPlane(change) => write!(f, "ControlPlane({change:?})"),
            OwnerMsg::Nodes(nodes) => write!(f, "Nodes({})", nodes.len()),
            OwnerMsg::Internal(internal) => write!(f, "Internal({internal:?})"),
            OwnerMsg::Close => f.write_str("Close"),
            OwnerMsg::Fenced(_) => f.write_str("Fenced"),
            OwnerMsg::RetryCommit => f.write_str("RetryCommit"),
        }
    }
}

/// Something the owner does about a run once it is loaded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Internal {
    /// Queue the run if it is runnable.
    Queue(RunRef),
    /// Send the task on its way if it is ready.
    Dispatch(TaskRef),
    /// A harness task handed on a stream that did not come back: interrupt it.
    Lost {
        task: TaskRef,
        token: crate::ports::TaskToken,
    },
    /// The organization actor granted the task's slot.
    Granted(TaskRef),
    /// Try the quarantined run once, as a node of a new build does.
    RetryQuarantined(RunRef),
    /// A signal checked and ready: a webhook's.
    Signal(Box<crate::ports::link::SignalAtOwner>, ReplyTo),
    /// Resume the task parked for the secret put again.
    Resume {
        task: TaskRef,
        secret: crate::ports::SecretId,
    },
    /// Seal the ended attempt.
    Seal(RunRef),
    /// Erase the run: its retention ended, or its namespace is erased.
    Erase(RunRef),
    /// Cancel the run: its namespace is erased.
    Cancel(RunRef, String),
}

impl Internal {
    fn run(&self) -> &RunRef {
        match self {
            Internal::Queue(run)
            | Internal::RetryQuarantined(run)
            | Internal::Seal(run)
            | Internal::Erase(run)
            | Internal::Cancel(run, _) => run,
            Internal::Dispatch(task)
            | Internal::Lost { task, .. }
            | Internal::Granted(task)
            | Internal::Resume { task, .. } => &task.run,
            Internal::Signal(signal, _) => &signal.run,
        }
    }
}

/// A run as the owner reads it once.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Loaded {
    pub(crate) head: RunHead,
    pub(crate) summary: Option<IndexedSummary>,
    /// The ids of its inbox items, with when they were received.
    pub(crate) inbox: Vec<(u64, Timestamp)>,
    pub(crate) tasks: Vec<TaskRecord>,
    pub(crate) action_set: Option<ActionSet>,
}

/// What the owner keeps of a run.
pub(crate) enum Entry<P: Plane> {
    /// Being read; the messages about it wait.
    Loading(Vec<OwnerMsg<P>>),
    Present(Box<RunSlot<P>>),
    /// The partition holds no such run.
    Missing,
}

/// A run the owner holds in memory, as its records are once its batches are durable.
pub(crate) struct RunSlot<P: Plane> {
    pub(crate) head: RunHead,
    pub(crate) summary: Option<IndexedSummary>,
    pub(crate) mode: Option<RunMode>,
    /// Its inbox items, with when they were received.
    pub(crate) inbox: BTreeMap<u64, Timestamp>,
    pub(crate) tasks: BTreeMap<Seq, TaskRecord>,
    pub(crate) action_set: Option<Arc<ActionSet>>,
    /// The signal keys this owner enqueued.
    pub(crate) keys: BTreeSet<String>,
    /// An activation is in flight; what it was for.
    pub(crate) busy: Option<Purpose>,
    /// The batch of a terminal activation is being prepared or committed.
    pub(crate) ending: bool,
    /// The ended attempt's history is being written to the archive: an erasure waits for it,
    /// so that nothing reaches the archive after the erasure emptied it.
    pub(crate) sealing: bool,
    /// The run's objects are being erased: no seal starts.
    pub(crate) erasing: bool,
    /// Messages that wait until the run is not busy.
    pub(crate) waiting: Vec<OwnerMsg<P>>,
    /// Who waits for the end of a give-up.
    pub(crate) give_up: Vec<ReplyTo>,
}

impl<P: Plane> RunSlot<P> {
    fn new(head: RunHead) -> Self {
        Self {
            head,
            summary: None,
            mode: None,
            inbox: BTreeMap::new(),
            tasks: BTreeMap::new(),
            action_set: None,
            keys: BTreeSet::new(),
            busy: None,
            ending: false,
            sealing: false,
            erasing: false,
            waiting: Vec::new(),
            give_up: Vec::new(),
        }
    }

    pub(crate) fn run(&self) -> &RunRef {
        &self.head.run
    }
}

/// Where a call of the owner's runs is, in memory: what the records never say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Flight {
    /// Waits for the run's own limit of calls of the action in flight.
    Waiting,
    /// Waits for a slot of its server, asked of the organization actor.
    Asked { server: String },
    /// Being prepared by the call executor, before it is handed to an SDK stream.
    Preparing,
    /// Sent by the node's call executor, with this token.
    Sent {
        server: Option<String>,
        token: Digest,
    },
    /// Handed to the organization actor for an SDK stream, with this token.
    Handed { token: Digest },
    /// Handed, and answered through its token: what follows frees its stream's slot without
    /// telling the stream it is gone.
    Answered,
}

/// A request the owner asked and waits for the answer of.
pub(crate) enum Ask {
    /// A start waits for its admission, with the runs of its namespace its handle must tell
    /// apart from.
    Start {
        start: Box<StartAtOwner>,
        reply: ReplyTo,
        neighbours: Vec<RunId>,
    },
    /// A workflow run a call starts waits for its admission.
    Child {
        task: TaskRef,
        delivery: u32,
        start: Box<StartAtOwner>,
        neighbours: Vec<RunId>,
    },
}

/// Where the owner is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Opening the partition: messages wait.
    Opening,
    Open,
    /// Fenced, or handed over: it answers `NotOwner` and holds nothing.
    Closed,
}

/// The owner of a partition.
pub(crate) struct Owner<P: Plane> {
    pub(crate) partition: PartitionId,
    pub(crate) node: NodeId,
    pub(crate) epoch: PartitionEpoch,
    pub(crate) plane: Arc<P>,
    pub(crate) config: Arc<NodeConfig>,
    pub(crate) phase: Phase,
    pub(crate) log: Option<Arc<LogOf<P>>>,
    /// What waits for the open.
    pub(crate) opening: Vec<OwnerMsg<P>>,
    pub(crate) pending: Vec<Unit<P>>,
    pub(crate) committing: Option<Vec<Unit<P>>>,
    /// Units committed alone after their batch was refused, to find the one refused.
    pub(crate) isolating: VecDeque<Unit<P>>,
    pub(crate) next_inbox: u64,
    pub(crate) next_outbox: u64,
    pub(crate) next_tag: u64,
    pub(crate) runs: BTreeMap<RunId, Entry<P>>,
    pub(crate) starting: BTreeMap<(TenantId, NamespaceName, String), Vec<ReplyTo>>,
    pub(crate) asks: BTreeMap<u64, Ask>,
    pub(crate) flights: BTreeMap<TaskRef, Flight>,
    pub(crate) tokens: BTreeMap<Digest, TaskRef>,
    pub(crate) wheel: Wheel,
    pub(crate) outbox: BTreeMap<OutboxId, (OutboxEntry, u32)>,
    pub(crate) orgs: BTreeMap<TenantId, OrgActor<P>>,
    /// The states the partition kept of the organizations homed here, for the actors to come.
    pub(crate) org_states: BTreeMap<TenantId, OrganizationState>,
    pub(crate) counted: BTreeSet<(TenantId, NamespaceName)>,
    pub(crate) deleting: BTreeSet<(TenantId, NamespaceName)>,
    pub(crate) remote: BTreeMap<RunId, NodeId>,
    /// Runs whose activation waits for the members to be read again: no node known runs their
    /// epoch.
    pub(crate) awaiting_nodes: BTreeSet<RunId>,
    pub(crate) awaiting_purposes: BTreeMap<RunId, Purpose>,
    /// The runs of erased namespaces this owner cancelled.
    pub(crate) cancelling: BTreeSet<RunId>,
    pub(crate) epochs: BTreeSet<Epoch>,
    pub(crate) nodes: Vec<NodeInfo>,
    /// Runs erased by this owner: what arrives for them is dropped.
    pub(crate) erased: BTreeSet<RunId>,
    pub(crate) out: Vec<Effect<P>>,
}

impl<P: Plane> fmt::Debug for Owner<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Owner")
            .field("partition", &self.partition)
            .field("phase", &self.phase)
            .field("runs", &self.runs.len())
            .field("pending", &self.pending.len())
            .field("timers", &self.wheel.len())
            .finish_non_exhaustive()
    }
}

impl<P: Plane> Owner<P> {
    /// An owner that opens `partition` at `epoch`: the open and the recovery are its first job.
    pub(crate) fn open(
        partition: PartitionId,
        epoch: PartitionEpoch,
        plane: Arc<P>,
        config: Arc<NodeConfig>,
        epochs: BTreeSet<Epoch>,
        nodes: Vec<NodeInfo>,
    ) -> (Self, Vec<Effect<P>>) {
        let now = crate::ports::Clock::now(plane.clock());
        let mut owner = Self {
            partition,
            node: config.node.clone(),
            epoch,
            plane,
            wheel: Wheel::new(now, config.horizon),
            config,
            phase: Phase::Opening,
            log: None,
            opening: Vec::new(),
            pending: Vec::new(),
            committing: None,
            isolating: VecDeque::new(),
            next_inbox: 1,
            next_outbox: 1,
            next_tag: 1,
            runs: BTreeMap::new(),
            starting: BTreeMap::new(),
            asks: BTreeMap::new(),
            flights: BTreeMap::new(),
            tokens: BTreeMap::new(),
            outbox: BTreeMap::new(),
            orgs: BTreeMap::new(),
            org_states: BTreeMap::new(),
            counted: BTreeSet::new(),
            deleting: BTreeSet::new(),
            remote: BTreeMap::new(),
            awaiting_nodes: BTreeSet::new(),
            awaiting_purposes: BTreeMap::new(),
            cancelling: BTreeSet::new(),
            epochs,
            nodes,
            erased: BTreeSet::new(),
            out: Vec::new(),
        };
        owner.out.push(recovery::open_job(&owner));
        let out = std::mem::take(&mut owner.out);
        (owner, out)
    }

    pub(crate) fn now(&self) -> Timestamp {
        crate::ports::Clock::now(self.plane.clock())
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    /// The earliest timer of the wheel, and whether it wakes the driver by itself in virtual
    /// time: a deadline, a deferred call's or a retention's does not, a code's timer, a retry
    /// or a backoff does.
    pub(crate) fn next_timer(&self) -> Option<(Timestamp, bool)> {
        use crate::ports::TimerKind;
        let next = self.wheel.next()?;
        let alone = matches!(
            next.kind,
            TimerKind::Code { .. }
                | TimerKind::RedeliveryBackoff { .. }
                | TimerKind::ActivationRetry { .. }
                | TimerKind::WebhookRetry { .. }
        );
        Some((next.at, alone))
    }

    /// The earliest timer that wakes the driver by itself, if any.
    pub(crate) fn next_waking_timer(&self) -> Option<Timestamp> {
        use crate::ports::TimerKind;
        self.wheel.timers_matching(|kind| {
            matches!(
                kind,
                TimerKind::Code { .. }
                    | TimerKind::RedeliveryBackoff { .. }
                    | TimerKind::ActivationRetry { .. }
                    | TimerKind::WebhookRetry { .. }
            )
        })
    }

    /// The earliest run deadline of the wheel.
    pub(crate) fn next_deadline(&self) -> Option<Timestamp> {
        use crate::ports::TimerKind;
        self.wheel
            .timers_matching(|kind| matches!(kind, TimerKind::RunDeadline { .. }))
    }

    /// The timers due at `now`, as messages; and the horizon when it is reached.
    pub(crate) fn due(&mut self, now: Timestamp) -> Vec<OwnerMsg<P>> {
        if self.phase != Phase::Open {
            return Vec::new();
        }
        let mut due: Vec<OwnerMsg<P>> = self
            .wheel
            .due(now)
            .into_iter()
            .map(OwnerMsg::TimerDue)
            .collect();
        if self.wheel.horizon() <= now {
            due.push(OwnerMsg::Horizon);
        }
        due
    }

    /// The horizon of the wheel.
    pub(crate) fn horizon(&self) -> Timestamp {
        self.wheel.horizon()
    }

    /// Handles one message; returns what follows.
    pub(crate) fn handle(&mut self, message: OwnerMsg<P>) -> Vec<Effect<P>> {
        self.dispatch(message);
        self.try_commit();
        std::mem::take(&mut self.out)
    }

    pub(crate) fn dispatch(&mut self, message: OwnerMsg<P>) {
        match self.phase {
            Phase::Opening => match message {
                OwnerMsg::Opened(opened) => return self.opened(*opened),
                OwnerMsg::OpenFailed(error) => return self.open_failed(error),
                OwnerMsg::Fenced(reply) => return self.fenced(reply),
                other => {
                    self.opening.push(other);
                    return;
                }
            },
            Phase::Closed => return self.refuse_closed(message),
            Phase::Open => {}
        }
        if let Some(run) = self.needs(&message) {
            match self.runs.get_mut(&run.id) {
                None => {
                    self.runs.insert(run.id, Entry::Loading(vec![message]));
                    self.load(run);
                    return;
                }
                Some(Entry::Loading(waiting)) => {
                    waiting.push(message);
                    return;
                }
                Some(_) => {}
            }
        }
        match message {
            OwnerMsg::Opened(_) | OwnerMsg::OpenFailed(_) => {}
            OwnerMsg::Request(request, reply) => self.request(*request, reply),
            OwnerMsg::Notice(notice) => self.notice(*notice),
            OwnerMsg::Org { tenant, message } => self.org(tenant, message),
            OwnerMsg::Answer { tag, answer } => self.answer(tag, answer),
            OwnerMsg::Loaded { run, loaded } => self.loaded(run, loaded),
            OwnerMsg::Durable(result) => self.durable(result),
            OwnerMsg::Activated { run, result } => self.activated(run, *result),
            OwnerMsg::CallDone(done) => self.call_done(*done),
            OwnerMsg::TimerDue(timer) => self.timer_due(timer),
            OwnerMsg::Horizon => self.horizon_reached(),
            OwnerMsg::HorizonRead(read) => match read {
                Ok(timers) => self.wheel.read(timers),
                Err(error) => {
                    tracing::error!(partition = %self.partition, %error, "the next hour's timers could not be read");
                    let at = self.now().saturating_add(std::time::Duration::from_secs(1));
                    self.out.push(Effect::Wake {
                        at,
                        alone: false,
                        message: Message::Owner(self.partition, OwnerMsg::Horizon),
                    });
                }
            },
            OwnerMsg::Step(step) => self.step(*step),
            OwnerMsg::ControlPlane(change) => self.control_plane(change),
            OwnerMsg::Nodes(nodes) => {
                // The streams of the nodes that left are closed for the organization actors.
                let members: Vec<NodeId> = nodes.iter().map(|node| node.id.clone()).collect();
                let tenants: Vec<TenantId> = self.orgs.keys().cloned().collect();
                for tenant in tenants {
                    self.org(tenant, OrgMsg::Members(members.clone()));
                }
                self.nodes = nodes;
                let waiting: Vec<RunId> = self.awaiting_nodes.iter().copied().collect();
                for run in waiting {
                    let purpose = self
                        .awaiting_purposes
                        .remove(&run)
                        .unwrap_or(Purpose::Runnable);
                    if let Some(slot) = self.slot(run) {
                        let run = slot.run().clone();
                        self.activate(&run, purpose);
                    }
                }
            }
            OwnerMsg::Internal(internal) => self.internal(internal),
            OwnerMsg::Close => self.close(),
            OwnerMsg::Fenced(reply) => self.fenced(reply),
            OwnerMsg::RetryCommit => self.retry_commit(),
        }
    }

    /// The run a message is about, which must be loaded before it is handled.
    fn needs(&self, message: &OwnerMsg<P>) -> Option<RunRef> {
        match message {
            OwnerMsg::Request(request, _) => match request.as_ref() {
                PartitionRequest::Signal(signal) => Some(signal.run.clone()),
                PartitionRequest::Cancel { run, .. }
                | PartitionRequest::Release { run }
                | PartitionRequest::GiveUp { run }
                | PartitionRequest::Erase { run } => Some(run.clone()),
                PartitionRequest::Decide { task, .. } => Some(task.run.clone()),
                PartitionRequest::Answer { tenant, token, .. } => {
                    tokens::run_of(&token.0).map(|run| RunRef::new(tenant.clone(), run))
                }
                _ => None,
            },
            OwnerMsg::CallDone(done) => Some(done.task.run.clone()),
            OwnerMsg::TimerDue(timer) => timer.kind.run().cloned(),
            OwnerMsg::Internal(internal) => Some(internal.run().clone()),
            OwnerMsg::Notice(notice) => match notice.as_ref() {
                OwnerNotice::SlotGranted { task } => Some(task.run.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Reads a run from the partition.
    fn load(&mut self, run: RunRef) {
        let Some(log) = self.log.clone() else {
            return;
        };
        let plane = Arc::clone(&self.plane);
        let partition = self.partition;
        self.out.push(job(async move {
            let loaded = match load_run(&*plane, &*log, &run).await {
                Err(PartitionError::Fenced { .. }) => {
                    return Message::Owner(partition, OwnerMsg::Fenced(None));
                }
                loaded => loaded.map_err(|error| error.to_string()),
            };
            Message::Owner(
                partition,
                OwnerMsg::Loaded {
                    run: run.id,
                    loaded,
                },
            )
        }));
    }

    fn loaded(&mut self, run: RunId, loaded: Result<Option<Box<Loaded>>, String>) {
        let Some(Entry::Loading(waiting)) = self.runs.remove(&run) else {
            return;
        };
        match loaded {
            Ok(Some(loaded)) => {
                let Loaded {
                    head,
                    summary,
                    inbox,
                    tasks,
                    action_set,
                } = *loaded;
                let mut slot = RunSlot::new(head);
                slot.mode = summary.as_ref().and_then(|summary| summary.summary.mode);
                slot.summary = summary;
                slot.inbox = inbox.into_iter().collect();
                slot.tasks = tasks
                    .into_iter()
                    .map(|task| (task.task.task.seq, task))
                    .collect();
                slot.action_set = action_set.map(Arc::new);
                self.runs.insert(run, Entry::Present(Box::new(slot)));
            }
            Ok(None) => {
                self.runs.insert(run, Entry::Missing);
            }
            Err(error) => {
                tracing::error!(partition = %self.partition, %run, %error, "a run could not be read");
                for message in waiting {
                    self.fail(
                        message,
                        Refusal::Unavailable(format!("run {run} could not be read: {error}")),
                    );
                }
                return;
            }
        }
        for message in waiting {
            self.dispatch(message);
        }
        if matches!(self.runs.get(&run), Some(Entry::Missing)) {
            self.runs.remove(&run);
        }
    }

    /// The run, if the partition holds it.
    pub(crate) fn slot(&self, run: RunId) -> Option<&RunSlot<P>> {
        match self.runs.get(&run) {
            Some(Entry::Present(slot)) => Some(slot),
            _ => None,
        }
    }

    pub(crate) fn slot_mut(&mut self, run: RunId) -> Option<&mut RunSlot<P>> {
        match self.runs.get_mut(&run) {
            Some(Entry::Present(slot)) => Some(slot),
            _ => None,
        }
    }

    /// Answers a message that cannot be handled with `refusal`; drops the others.
    pub(crate) fn fail(&mut self, message: OwnerMsg<P>, refusal: Refusal) {
        match message {
            OwnerMsg::Request(request, reply) => {
                let refused = reads::refused(&request, refusal);
                self.reply(reply, refused);
            }
            OwnerMsg::Internal(Internal::Signal(_, reply)) => {
                self.reply(reply, PartitionReply::Enqueued(Err(refusal)));
            }
            _ => {}
        }
    }

    /// Replies to a request.
    pub(crate) fn reply(&mut self, to: ReplyTo, reply: PartitionReply) {
        self.out
            .push(Effect::Reply(to, NodeReply::Partition(Box::new(reply))));
    }

    fn refuse_closed(&mut self, message: OwnerMsg<P>) {
        if let Some(reply) = message.into_reply() {
            self.out.push(Effect::Reply(
                reply,
                NodeReply::NotOwner {
                    partition: self.partition,
                },
            ));
        }
    }

    /// Asks a request; its answer comes back by its tag.
    pub(crate) fn ask(
        &mut self,
        to: crate::node::message::Target,
        request: crate::ports::link::NodeRequest,
        ask: Ask,
    ) {
        let tag = self.next_tag;
        self.next_tag += 1;
        self.asks.insert(tag, ask);
        self.out.push(Effect::Request {
            to,
            request,
            asker: Some(crate::node::message::Asker {
                partition: self.partition,
                tag,
            }),
        });
    }

    /// Sends a notice nobody answers.
    pub(crate) fn tell(
        &mut self,
        to: crate::node::message::Target,
        request: crate::ports::link::NodeRequest,
    ) {
        self.out.push(Effect::Request {
            to,
            request,
            asker: None,
        });
    }

    // ---------------------------------------------------------------------------------------
    // Organization actors

    fn org(&mut self, tenant: TenantId, message: OrgMsg) {
        if home_partition(&tenant, self.config.partitions) != self.partition {
            if let OrgMsg::Request(_, reply) = message {
                self.out.push(Effect::Reply(
                    reply,
                    NodeReply::NotOwner {
                        partition: self.partition,
                    },
                ));
            }
            return;
        }
        let now = self.now();
        if !self.orgs.contains_key(&tenant) {
            let state = self.org_states.remove(&tenant).unwrap_or_default();
            let (actor, effects) = OrgActor::new(
                tenant.clone(),
                self.partition,
                self.config.partitions,
                Arc::clone(&self.plane),
                state,
            );
            self.orgs.insert(tenant.clone(), actor);
            self.org_effects(&tenant, effects);
        }
        let Some(actor) = self.orgs.get_mut(&tenant) else {
            return;
        };
        let effects = actor.handle(message, now);
        self.org_effects(&tenant, effects);
    }

    fn org_effects(&mut self, tenant: &TenantId, effects: Vec<OrgEffect<P>>) {
        for effect in effects {
            match effect {
                OrgEffect::Effect(effect) => self.out.push(effect),
                OrgEffect::Persist {
                    state,
                    alerts,
                    then,
                } => {
                    let mut unit = Unit::new();
                    unit.change(Change::Organization {
                        tenant: tenant.clone(),
                        state,
                    });
                    for alert in alerts {
                        let entry = self.outbox_entry(
                            tenant.clone(),
                            None,
                            crate::ports::Projection::QuotaAlert(alert),
                        );
                        unit.project(entry);
                    }
                    for effect in then {
                        unit.then(commit::After::Effect(effect));
                    }
                    self.push(unit);
                }
            }
        }
    }

    /// A new outbox entry, numbered.
    pub(crate) fn outbox_entry(
        &mut self,
        tenant: TenantId,
        run: Option<RunId>,
        projection: crate::ports::Projection,
    ) -> OutboxEntry {
        let id = OutboxId(self.next_outbox);
        self.next_outbox += 1;
        OutboxEntry {
            id,
            tenant,
            run,
            projection,
        }
    }

    // ---------------------------------------------------------------------------------------
    // The control plane

    fn control_plane(&mut self, change: ControlPlaneChange) {
        match change {
            ControlPlaneChange::Secret { tenant, secret } => self.secret_put(tenant, secret),
            ControlPlaneChange::Namespace { tenant, name } => {
                if let Some(actor) = self.orgs.get_mut(&tenant) {
                    let effects = actor.handle(
                        OrgMsg::Forget {
                            namespace: Some(name.clone()),
                        },
                        crate::ports::Clock::now(self.plane.clock()),
                    );
                    self.org_effects(&tenant, effects);
                }
                if crate::ports::namespace_partition(&tenant, &name, self.config.partitions)
                    == self.partition
                {
                    self.namespace_changed(tenant, name);
                }
            }
            ControlPlaneChange::Tenant { tenant } => {
                if let Some(actor) = self.orgs.get_mut(&tenant) {
                    let effects = actor.handle(
                        OrgMsg::Forget { namespace: None },
                        crate::ports::Clock::now(self.plane.clock()),
                    );
                    self.org_effects(&tenant, effects);
                }
            }
            _ => {}
        }
    }

    fn close(&mut self) {
        self.phase = Phase::Closed;
        for (_, ask) in std::mem::take(&mut self.asks) {
            if let Ask::Start { reply, .. } = ask {
                self.out.push(Effect::Reply(
                    reply,
                    NodeReply::NotOwner {
                        partition: self.partition,
                    },
                ));
            }
        }
        for reply in std::mem::take(&mut self.starting).into_values().flatten() {
            self.out.push(Effect::Reply(
                reply,
                NodeReply::NotOwner {
                    partition: self.partition,
                },
            ));
        }
        for unit in std::mem::take(&mut self.pending) {
            for reply in unit.replies() {
                self.out.push(Effect::Reply(
                    reply,
                    NodeReply::NotOwner {
                        partition: self.partition,
                    },
                ));
            }
        }
        for (_, entry) in std::mem::take(&mut self.runs) {
            if let Entry::Loading(waiting) = entry {
                for message in waiting {
                    self.refuse_closed(message);
                }
            }
        }
        for message in std::mem::take(&mut self.opening) {
            self.refuse_closed(message);
        }
    }
}

/// Reads what the owner keeps of a run.
async fn load_run<P: Plane>(
    plane: &P,
    log: &LogOf<P>,
    run: &RunRef,
) -> Result<Option<Box<Loaded>>, PartitionError> {
    let Some(head) = log.run(run).await? else {
        return Ok(None);
    };
    let summary = log.summary(run).await?;
    let inbox = log
        .inbox(run)
        .await?
        .into_iter()
        .map(|item| (item.id, item.received_at))
        .collect();
    let tasks = log.tasks(run).await?;
    let action_set = plane
        .catalog()
        .get_action_set(&run.tenant, head.action_set)
        .await
        .map_err(|error| PartitionError::Backend(error.to_string()))?
        .map(|stored| stored.action_set);
    Ok(Some(Box::new(Loaded {
        head,
        summary,
        inbox,
        tasks,
        action_set,
    })))
}
