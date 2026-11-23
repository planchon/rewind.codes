//! What the actors of a node take and return (`archi/07-code-architecture.md`, "Inside a node").
//!
//! An actor handles one [`Message`] at a time and returns [`Effect`]s; it does no I/O itself.
//! Work with I/O (a commit, a read, an activation, a call) is a [`Job`]: a future that owns what
//! it needs and resolves to the message that reports it. Whoever drives the node (the tokio
//! runtime, or the driver in tests) runs the jobs, delivers the messages, routes the requests
//! to their node, and fires the wakes.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use rewind_core::{PartitionId, Timestamp};

use crate::executors::ExecMsg;
use crate::node::NodeMsg;
use crate::partition::OwnerMsg;
use crate::plane::Plane;
use crate::ports::link::{ActivationRequest, ActivationResult, NodeReply, NodeRequest};
use crate::ports::{LinkError, NodeId, TenantId};
use crate::tasks::calls::CallMsg;

/// Work with I/O, and the message that reports it.
pub(crate) type Job<P> = Pin<Box<dyn Future<Output = Message<P>> + Send + 'static>>;

/// A message to an actor of the node.
pub(crate) enum Message<P: Plane> {
    /// To the owner of a partition held here (and the organization actors it hosts).
    Owner(PartitionId, OwnerMsg<P>),
    Executors(ExecMsg<P>),
    Calls(CallMsg<P>),
    Node(NodeMsg),
    /// A request reached the node: from its edge, from one of its actors, or from the link.
    Inbound(NodeRequest, ReplyTo),
    /// A remote executor's activation reached the node.
    InboundActivation(Box<ActivationRequest>, ReplyTo),
    /// The answer to a request an owner of this node asked.
    Answer(Asker, Result<NodeReply, LinkError>),
    /// A reply a job made for a request.
    Reply(ReplyTo, NodeReply),
    /// A job that reports nothing.
    Nothing,
}

impl<P: Plane> fmt::Debug for Message<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Message::Owner(partition, message) => write!(f, "Owner({partition}, {message:?})"),
            Message::Executors(message) => write!(f, "Executors({message:?})"),
            Message::Calls(message) => write!(f, "Calls({message:?})"),
            Message::Node(message) => write!(f, "Node({message:?})"),
            Message::Inbound(request, reply) => write!(f, "Inbound({request:?}, {reply:?})"),
            Message::InboundActivation(request, reply) => {
                write!(f, "InboundActivation({}, {reply:?})", request.head.run)
            }
            Message::Answer(asker, answer) => write!(f, "Answer({asker:?}, {answer:?})"),
            Message::Reply(to, reply) => write!(f, "Reply({to:?}, {reply:?})"),
            Message::Nothing => f.write_str("Nothing"),
        }
    }
}

/// Who waits for the reply to a request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum ReplyTo {
    /// A caller of this process (the edge's use cases), by the id the runtime gave it.
    Waiter(u64),
    /// An owner of this node.
    Owner(Asker),
    /// A request that came through the link, by the id the runtime gave it.
    Link(u64),
    /// Nobody: a notice.
    Nobody,
}

/// An owner that asked, and the tag it gave its request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Asker {
    pub(crate) partition: PartitionId,
    pub(crate) tag: u64,
}

/// Where a request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// The owner of a partition.
    Partition(PartitionId),
    /// The organization actor of a tenant: on the owner of its home partition.
    Home(TenantId),
    /// The edge of a node.
    Node(NodeId),
}

/// What an actor asks for.
pub(crate) enum Effect<P: Plane> {
    /// A message to an actor of this node.
    Local(Message<P>),
    /// A request to the owner of a partition, an organization actor or an edge, here or on
    /// another node; its answer goes to `asker`.
    Request {
        to: Target,
        request: NodeRequest,
        asker: Option<Asker>,
    },
    /// A request for another node, once routed: carried by the link.
    Remote {
        node: NodeId,
        request: NodeRequest,
        asker: Option<Asker>,
    },
    /// The reply to a request.
    Reply(ReplyTo, NodeReply),
    /// Work with I/O.
    Job(JobClass, Job<P>),
    /// A message at `at`, from memory: lost if the node dies. `alone`: in virtual time, the
    /// driver moves the clock to it even when nothing else is due.
    Wake {
        at: Timestamp,
        alone: bool,
        message: Message<P>,
    },
    /// An activation for an executor of another node.
    Activate {
        node: NodeId,
        request: Box<ActivationRequest>,
        asker: Asker,
    },
    /// The result of an activation a remote owner asked for.
    ActivationReply(ReplyTo, Box<ActivationResult>),
}

impl<P: Plane> fmt::Debug for Effect<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Effect::Local(message) => write!(f, "Local({message:?})"),
            Effect::Request { to, request, asker } => {
                write!(f, "Request({to:?}, {request:?}, {asker:?})")
            }
            Effect::Remote {
                node,
                request,
                asker,
            } => write!(f, "Remote({node}, {request:?}, {asker:?})"),
            Effect::Reply(to, reply) => write!(f, "Reply({to:?}, {reply:?})"),
            Effect::Job(class, _) => write!(f, "Job({class:?})"),
            Effect::Wake { at, alone, message } => {
                write!(f, "Wake({at:?}, alone: {alone}, {message:?})")
            }
            Effect::Activate { node, request, .. } => {
                write!(f, "Activate({node}, {})", request.head.run)
            }
            Effect::ActivationReply(to, _) => write!(f, "ActivationReply({to:?})"),
        }
    }
}

/// What a job does: what the driver orders jobs by. A process runs them all at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum JobClass {
    /// Reads and writes of the stores.
    Store,
    /// An activation.
    Activation,
    /// A call of this run.
    Call(rewind_core::RunId),
}

/// Boxes a job of a class.
pub(crate) fn job_of<P: Plane>(
    class: JobClass,
    work: impl Future<Output = Message<P>> + Send + 'static,
) -> Effect<P> {
    Effect::Job(class, Box::pin(work))
}

/// Boxes a job that reads or writes the stores.
pub(crate) fn job<P: Plane>(work: impl Future<Output = Message<P>> + Send + 'static) -> Effect<P> {
    Effect::Job(JobClass::Store, Box::pin(work))
}
