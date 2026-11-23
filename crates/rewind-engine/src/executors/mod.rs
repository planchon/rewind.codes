//! The executors of a node (`archi/07-code-architecture.md`, "Inside a node"): its ready queue,
//! its instance cache, and the activations its executor threads run, for the owners of the node
//! and for the owners of other nodes whose build does not run the run's epoch.
//!
//! ```text
//! Runnable (a local owner) ─┐                         ┌─► Activated → the owner
//! Remote (the node link) ───┴─► ready queue ─► job ───┤
//!                                (fair)       activate└─► ActivationReply → the link
//! Durable ─► the cached instance is its commit's      Forget ─► the instance is dropped
//! ```
//!
//! A cached instance is used only once its owner made the batch of its activation durable: a
//! commit that failed never leaves an instance ahead of its run.

pub(crate) mod activation;
pub(crate) mod derive;
mod queue;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use rewind_core::{Digest, PartitionId, Payload, Timestamp};

use crate::cache::InstanceCache;
use crate::node::message::{Effect, JobClass, Message, ReplyTo, job_of};
use crate::partition::OwnerMsg;
use crate::plane::{InstanceOf, LogOf, NodeConfig, Plane};
use crate::ports::link::{ActivationRequest, ActivationResult, ParentHistory, Purpose};
use crate::ports::{
    CatalogStore, IndexedSummary, PartitionLog, PayloadRecord, RunHead, RunRef, Trigger,
};
pub(crate) use activation::Warm;
use queue::ReadyQueue;

/// A message to the executors of a node.
pub(crate) enum ExecMsg<P: Plane> {
    /// A run of a partition of this node to activate.
    Runnable(Box<LocalRun<P>>),
    /// An activation a remote owner sent.
    Remote(Box<ActivationRequest>, ReplyTo),
    /// An activation finished.
    Finished(Box<Finished<P>>),
    /// The owner made the batch of the run's activation durable, at this history length.
    Durable { run: RunRef, history_len: u64 },
    /// The run's instance is dropped: its commit failed, it is quarantined, ended or erased.
    Forget { run: RunRef },
}

impl<P: Plane> fmt::Debug for ExecMsg<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecMsg::Runnable(run) => write!(f, "Runnable({}, {:?})", run.head.run, run.purpose),
            ExecMsg::Remote(request, _) => write!(f, "Remote({})", request.head.run),
            ExecMsg::Finished(finished) => write!(f, "Finished({})", finished.run),
            ExecMsg::Durable { run, history_len } => write!(f, "Durable({run}, {history_len})"),
            ExecMsg::Forget { run } => write!(f, "Forget({run})"),
        }
    }
}

/// A run of a partition of this node, as its owner hands it to the ready queue.
pub(crate) struct LocalRun<P: Plane> {
    pub(crate) partition: PartitionId,
    pub(crate) log: Arc<LogOf<P>>,
    pub(crate) head: RunHead,
    pub(crate) summary: Option<IndexedSummary>,
    pub(crate) purpose: Purpose,
    /// When its oldest trigger was received.
    pub(crate) oldest: Timestamp,
}

/// An activation that finished, and where its result goes.
pub(crate) struct Finished<P: Plane> {
    pub(crate) run: RunRef,
    pub(crate) origin: Origin,
    pub(crate) result: ActivationResult,
    pub(crate) warm: Option<Warm<P>>,
}

/// Who asked for an activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Origin {
    Local(PartitionId),
    Remote(ReplyTo),
}

enum Work<P: Plane> {
    Local(Box<LocalRun<P>>),
    Remote(Box<ActivationRequest>, ReplyTo),
}

impl<P: Plane> Work<P> {
    fn head(&self) -> &RunHead {
        match self {
            Work::Local(run) => &run.head,
            Work::Remote(request, _) => &request.head,
        }
    }
}

/// The executors of a node.
pub(crate) struct Executors<P: Plane> {
    plane: Arc<P>,
    config: Arc<NodeConfig>,
    queue: ReadyQueue<Work<P>>,
    running: usize,
    cache: InstanceCache<InstanceOf<P>>,
    /// The history length each cached instance's commit made durable.
    durable: BTreeMap<RunRef, u64>,
}

impl<P: Plane> fmt::Debug for Executors<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Executors")
            .field("queued", &self.queue.len())
            .field("running", &self.running)
            .field("cached", &self.cache.len())
            .finish_non_exhaustive()
    }
}

impl<P: Plane> Executors<P> {
    pub(crate) fn new(plane: Arc<P>, config: Arc<NodeConfig>) -> Self {
        let cache = InstanceCache::new(config.cache);
        Self {
            plane,
            config,
            queue: ReadyQueue::default(),
            running: 0,
            cache,
            durable: BTreeMap::new(),
        }
    }

    /// The instances kept.
    pub(crate) fn cached(&self) -> usize {
        self.cache.len()
    }

    pub(crate) fn handle(&mut self, message: ExecMsg<P>) -> Vec<Effect<P>> {
        let mut out = Vec::new();
        match message {
            ExecMsg::Runnable(run) => {
                let head = &run.head;
                self.queue.push(
                    head.run.tenant.clone(),
                    head.attempts,
                    run.oldest,
                    head.run.id,
                    Work::Local(run),
                );
            }
            ExecMsg::Remote(request, reply) => {
                let head = &request.head;
                let oldest = request
                    .inbox
                    .first()
                    .map_or(head.updated_at, |item| item.received_at);
                self.queue.push(
                    head.run.tenant.clone(),
                    head.attempts,
                    oldest,
                    head.run.id,
                    Work::Remote(request, reply),
                );
            }
            ExecMsg::Finished(finished) => {
                self.running = self.running.saturating_sub(1);
                let Finished {
                    run,
                    origin,
                    result,
                    warm,
                } = *finished;
                if let Some(warm) = warm {
                    if matches!(origin, Origin::Remote(_)) {
                        // A remote owner keeps sending the run here while this node lives.
                        self.durable.insert(run.clone(), warm.state.history_len());
                    }
                    if matches!(result, ActivationResult::Released) {
                        self.durable.insert(run.clone(), warm.state.history_len());
                    }
                    let evicted = self.cache.insert(run.clone(), warm);
                    crate::metrics::instance_cache_evictions(evicted.len());
                    for evicted in evicted {
                        self.durable.remove(&evicted);
                    }
                } else {
                    drop(self.cache.take(&run));
                    self.durable.remove(&run);
                }
                out.push(match origin {
                    Origin::Local(partition) => Effect::Local(Message::Owner(
                        partition,
                        OwnerMsg::Activated {
                            run: run.id,
                            result: Box::new(result),
                        },
                    )),
                    Origin::Remote(reply) => Effect::ActivationReply(reply, Box::new(result)),
                });
            }
            ExecMsg::Durable { run, history_len } => {
                self.durable.insert(run, history_len);
            }
            ExecMsg::Forget { run } => {
                drop(self.cache.take(&run));
                self.durable.remove(&run);
            }
        }
        self.pump(&mut out);
        out
    }

    /// Starts the next activations, while an executor is free.
    fn pump(&mut self, out: &mut Vec<Effect<P>>) {
        while self.running < self.config.executors.max(1) {
            let Some(work) = self.queue.pop() else {
                return;
            };
            self.running += 1;
            let head = work.head();
            let run = head.run.clone();
            let length = head.history_len;
            let warm = self.cache.take(&run);
            let warm = match (&work, warm) {
                (Work::Local(_), Some(warm)) if self.durable.get(&run) == Some(&length) => {
                    Some(warm)
                }
                (Work::Remote(..), Some(warm)) => Some(warm),
                _ => None,
            };
            let plane = Arc::clone(&self.plane);
            let config = Arc::clone(&self.config);
            out.push(job_of(JobClass::Activation, async move {
                let (origin, result, warm) = match work {
                    Work::Local(local) => {
                        let LocalRun {
                            partition,
                            log,
                            head,
                            summary,
                            purpose,
                            ..
                        } = *local;
                        let with_history = warm.is_none() || purpose != Purpose::Runnable;
                        let prepared =
                            prepare(&*plane, &*log, head, summary, purpose, with_history).await;
                        let (result, warm) = match prepared {
                            Ok(request) => {
                                activation::activate(&*plane, &config, request, warm).await
                            }
                            Err(error) => (ActivationResult::Failed { error }, None),
                        };
                        (Origin::Local(partition), result, warm)
                    }
                    Work::Remote(request, reply) => {
                        let (result, warm) =
                            activation::activate(&*plane, &config, *request, warm).await;
                        (Origin::Remote(reply), result, warm)
                    }
                };
                Message::Executors(ExecMsg::Finished(Box::new(Finished {
                    run,
                    origin,
                    result,
                    warm,
                })))
            }));
        }
    }
}

/// Reads what an activation of a run needs from its partition: its inbox, its tasks, its
/// history unless the instance cache holds the run (`with_history`), the payload records the
/// history and the inbox hold by digest, a fork's parent at its first activation, its script
/// and its toolset.
pub(crate) async fn prepare<P: Plane>(
    plane: &P,
    log: &LogOf<P>,
    head: RunHead,
    summary: Option<IndexedSummary>,
    purpose: Purpose,
    with_history: bool,
) -> Result<ActivationRequest, String> {
    let run = head.run.clone();
    let failed =
        |what: &str, error: String| format!("{what} of run {} could not be read: {error}", run.id);
    let inbox = log
        .inbox(&run)
        .await
        .map_err(|error| failed("the inbox", error.to_string()))?;
    let tasks = log
        .tasks(&run)
        .await
        .map_err(|error| failed("the tool tasks", error.to_string()))?;
    let history = if with_history {
        Some(
            log.history(&run, head.attempt, 0)
                .await
                .map_err(|error| failed("the history", error.to_string()))?,
        )
    } else {
        None
    };
    let mut digests = activation::digests_of(history.iter().flatten());
    for item in &inbox {
        if let Some(digest) = trigger_digest(&item.trigger) {
            digests.insert(digest);
        }
    }
    let payloads = records(log, &run, digests).await?;
    let parent = match head.metadata.parent {
        Some(link) if head.history_len == 0 => parent_history(plane, log, &run, link.run).await?,
        _ => None,
    };
    let tenant = &run.tenant;
    let script = plane
        .catalog()
        .get_script(tenant, head.script)
        .await
        .map_err(|error| failed("the script", error.to_string()))?
        .ok_or_else(|| format!("script {} not found", head.script))?;
    let toolset = plane
        .catalog()
        .get_toolset(tenant, head.toolset)
        .await
        .map_err(|error| failed("the toolset", error.to_string()))?
        .ok_or_else(|| format!("toolset {} not found", head.toolset))?;
    Ok(ActivationRequest {
        partition: log.partition(),
        head,
        summary,
        inbox,
        history,
        payloads,
        parent,
        script,
        toolset,
        purpose,
        tasks,
    })
}

/// The digest a trigger holds its payload by, if it does.
fn trigger_digest(trigger: &Trigger) -> Option<Digest> {
    let payload = match trigger {
        Trigger::RunStarted(started) => &started.input,
        Trigger::Completion(rewind_core::Completion::ToolCallCompleted { result, .. }) => result,
        Trigger::Completion(rewind_core::Completion::SignalReceived { payload, .. }) => payload,
        Trigger::Completion(_) => return None,
    };
    match payload {
        Payload::Digest(digest) => Some(*digest),
        Payload::Value(_) => None,
    }
}

/// The payload records of a run for these digests.
async fn records<L: PartitionLog>(
    log: &L,
    run: &RunRef,
    digests: impl IntoIterator<Item = Digest>,
) -> Result<Vec<PayloadRecord>, String> {
    let mut records = Vec::new();
    for digest in digests {
        let record = log.payload(run, digest).await.map_err(|error| {
            format!(
                "the payload {digest} of run {} could not be read: {error}",
                run.id
            )
        })?;
        records.extend(record);
    }
    Ok(records)
}

/// The history a fork follows: its parent's attempt from the partition, or its sealed journal
/// from the archive. `None` when the parent is gone.
async fn parent_history<P: Plane>(
    plane: &P,
    log: &LogOf<P>,
    run: &RunRef,
    parent: rewind_core::RunId,
) -> Result<Option<ParentHistory>, String> {
    let parent_ref = RunRef::new(run.tenant.clone(), parent);
    let failed = |error: String| {
        format!(
            "the parent {parent} of run {} could not be read: {error}",
            run.id
        )
    };
    let read = crate::partition::read_attempt(plane, log, &parent_ref, 0, true)
        .await
        .map_err(failed)?;
    Ok(read.map(|read| match read {
        crate::partition::AttemptRead::Live {
            entries, payloads, ..
        } => ParentHistory {
            run: parent,
            entries,
            payloads,
            sealed: None,
        },
        crate::partition::AttemptRead::Sealed { sealed, .. } => ParentHistory {
            run: parent,
            entries: Vec::new(),
            payloads: Vec::new(),
            sealed: Some(sealed),
        },
    }))
}
