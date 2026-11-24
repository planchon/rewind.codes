//! The runs' use cases at their owner (`archi/07-code-architecture.md`, "Inside a node": the
//! rules the engine keeps; `archi/03-execution.md`, "Activations", "Workflows across runs",
//! "Cancellation", "Run TTL", "Non-determinism and quarantine"; `archi/06-storage-and-scale.md`,
//! "Idempotency of the API", "Retention and deletion").

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rewind_core::{
    CancelReason, Completion, Digest, JsonValue, Payload, RunId, RunStatus, Seq, end_signal_name,
    is_reserved_signal, starter_ended,
};

use crate::executors::{ExecMsg, LocalRun, derive};
use crate::node::message::{Effect, Message, ReplyTo, Target, job};
use crate::partition::commit::{After, Unit};
use crate::partition::{Ask, Entry, Internal, Owner, OwnerMsg, RunSlot};
use crate::plane::Plane;
use crate::ports::link::{
    ActivationRequest, ActivationResult, Admit, Committed, NodeReply, NodeRequest, OrgReply,
    OrgRequest, OwnerNotice, PartitionReply, PartitionRequest, Purpose, Refusal, SignalAtOwner,
    StartAtOwner,
};
use crate::ports::partition::{Change, CreateRun};
use crate::ports::{
    ArchiveError, Attempt, BlobStore, CatalogStore, Created, DueTimer, Enqueued, InboxItem,
    LinkError, Namespace, NamespaceName, NodeId, PartitionError, PartitionLog, Projection,
    Quarantine, QuarantineKind, RunArchive, RunHead, RunRef, RunSummary, TenantId, TimerChange,
    TimerKind, Trigger, Watcher, WebhookTarget,
};
use crate::text;

/// What a job of a use case reports.
pub(crate) enum Step {
    /// The run a start key started, if any; and the runs of the start's namespace whose handle
    /// the new run's must tell apart from.
    StartKey {
        start: Box<StartAtOwner>,
        reply: ReplyTo,
        found: Result<Option<RunId>, String>,
        neighbours: Result<Vec<RunId>, String>,
    },
    /// A signal checked against its run's workflow and its key.
    SignalChecked {
        signal: Box<SignalAtOwner>,
        reply: ReplyTo,
        checked: Result<bool, Refusal>,
    },
    /// The run and signal a webhook's token is bound to.
    Webhook {
        /// The run its token names; none for a token of a run `migrate-runs` moved.
        run: Option<RunId>,
        payload: JsonValue,
        key: Option<String>,
        reply: ReplyTo,
        target: Result<Option<WebhookTarget>, String>,
    },
    /// What a terminal activation, or a fork's first, needs read before it is committed.
    Ending {
        run: RunId,
        committed: Box<Committed>,
        reads: Result<EndReads, String>,
    },
    /// An activation for a remote executor, read.
    Prepared {
        run: RunId,
        node: NodeId,
        request: Result<Box<ActivationRequest>, String>,
    },
    /// The ended attempt is in the archive; the run's retention.
    Sealed {
        run: RunRef,
        attempt: Attempt,
        result: Result<Duration, String>,
    },
    /// The run's objects are erased from the archive and the blobs.
    Erased {
        run: RunRef,
        result: Result<(), String>,
    },
    /// An outbox entry was exported or delivered, or not.
    Consumed {
        id: crate::ports::OutboxId,
        result: Result<(), String>,
    },
    /// The counts of a namespace, read.
    Counted {
        tenant: TenantId,
        namespace: NamespaceName,
        included: Vec<RunId>,
        abandoned: Vec<RunId>,
        result: Result<(crate::ports::Gauges, crate::ports::Counters), String>,
    },
    /// The tasks parked in the runs a secret put again may resume.
    Parked {
        secret: crate::ports::SecretId,
        tasks: Result<Vec<crate::ports::TaskRef>, String>,
    },
    /// A namespace of the partition, read after a change.
    Namespace {
        tenant: TenantId,
        name: NamespaceName,
        found: Result<Option<Namespace>, String>,
    },
    /// The runs left in a namespace being erased.
    NamespaceRuns {
        tenant: TenantId,
        name: NamespaceName,
        found: Result<Vec<RunSummary>, String>,
    },
    /// What the control plane kept of an erased namespace is dropped.
    NamespaceDropped {
        tenant: TenantId,
        name: NamespaceName,
        result: Result<(), String>,
    },
}

/// The ids of the runs of `namespace` whose suffix of 12 characters is `run`'s: those a new
/// run's handle must tell it apart from (`archi/06-storage-and-scale.md`, "Run handles").
pub(crate) async fn neighbours<L: PartitionLog>(
    log: &L,
    run: &RunRef,
    namespace: &NamespaceName,
) -> Result<Vec<RunId>, PartitionError> {
    let mut found = Vec::new();
    for (low, high) in rewind_core::suffix_neighbours(run.id) {
        found.extend(
            log.runs_in_range(&run.tenant, low, high)
                .await?
                .into_iter()
                .filter(|head| head.namespace == *namespace && head.run.id != run.id)
                .map(|head| head.run.id),
        );
    }
    Ok(found)
}

impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::StartKey { start, .. } => write!(f, "StartKey({})", start.run),
            Step::SignalChecked { signal, .. } => write!(f, "SignalChecked({})", signal.run),
            Step::Webhook { .. } => f.write_str("Webhook"),
            Step::Ending { run, .. } => write!(f, "Ending({run})"),
            Step::Prepared { run, node, .. } => write!(f, "Prepared({run}, {node})"),
            Step::Sealed { run, .. } => write!(f, "Sealed({run})"),
            Step::Erased { run, .. } => write!(f, "Erased({run})"),
            Step::Consumed { id, result } => write!(f, "Consumed({id}, {result:?})"),
            Step::Counted { namespace, .. } => write!(f, "Counted({namespace})"),
            Step::Parked { secret, .. } => write!(f, "Parked({secret})"),
            Step::Namespace { name, .. } => write!(f, "Namespace({name})"),
            Step::NamespaceRuns { name, .. } => write!(f, "NamespaceRuns({name})"),
            Step::NamespaceDropped { name, .. } => write!(f, "NamespaceDropped({name})"),
        }
    }
}

/// What a terminal activation, or a fork's first, reads before it is committed.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EndReads {
    /// The runs that watch the run and have not ended: they receive its end.
    pub(crate) watchers: Vec<Watcher>,
    /// The runs it started attached and that have not ended: they are cancelled.
    pub(crate) cancel: Vec<RunId>,
    /// For a fork's first activation: the runs its copied history started, by call, with their
    /// end if they have ended.
    pub(crate) inherited: Vec<(Seq, RunId, Option<JsonValue>)>,
}

fn step<P: Plane>(partition: rewind_core::PartitionId, step: Step) -> Message<P> {
    Message::Owner(partition, OwnerMsg::Step(Box::new(step)))
}

impl<P: Plane> Owner<P> {
    // ---------------------------------------------------------------------------------------
    // The mailbox

    pub(crate) fn request(&mut self, request: PartitionRequest, reply: ReplyTo) {
        match request {
            PartitionRequest::Start(start) => self.start(start, reply),
            PartitionRequest::Signal(signal) => self.signal(Box::new(signal), reply),
            PartitionRequest::Cancel { run, message } => self.cancel(&run, message, reply),
            PartitionRequest::Release { run } => self.release(&run, reply),
            PartitionRequest::GiveUp { run } => self.give_up(&run, reply),
            PartitionRequest::Erase { run } => self.erase(&run, Some(reply)),
            PartitionRequest::Answer {
                tenant,
                token,
                answer,
            } => self.answer_task(&tenant, &token, answer, reply),
            PartitionRequest::Decide { task, decision } => self.decide(&task, decision, reply),
            PartitionRequest::Webhook {
                token,
                payload,
                key,
            } => self.webhook(&token, payload, key, reply),
            read => self.read(read, reply),
        }
    }

    pub(crate) fn notice(&mut self, notice: OwnerNotice) {
        match notice {
            OwnerNotice::SlotGranted { task } => {
                self.dispatch(OwnerMsg::Internal(Internal::Granted(task)));
            }
            OwnerNotice::StreamLost { tasks } => {
                for (task, token) in tasks {
                    self.dispatch(OwnerMsg::Internal(Internal::Lost { task, token }));
                }
            }
            OwnerNotice::CountsWanted { tenant } => {
                let namespaces: Vec<NamespaceName> = self
                    .counted
                    .iter()
                    .filter(|(of, _)| *of == tenant)
                    .map(|(_, namespace)| namespace.clone())
                    .collect();
                for namespace in namespaces {
                    self.report(tenant.clone(), namespace, Vec::new(), Vec::new());
                }
            }
        }
    }

    pub(crate) fn internal(&mut self, internal: Internal) {
        match internal {
            Internal::Queue(run) => self.queue(&run),
            Internal::Dispatch(task) => self.dispatch_task(&task),
            Internal::Lost { task, token } => self.lost(&task, &token),
            Internal::Granted(task) => self.granted(&task),
            Internal::RetryQuarantined(run) => {
                let retry = self.slot(run.id).is_some_and(|slot| {
                    slot.busy.is_none()
                        && !slot.head.is_ended()
                        && slot.head.quarantine.as_ref().is_some_and(|quarantine| {
                            quarantine.retried_by.as_deref() != Some(self.config.build.as_str())
                        })
                });
                if retry {
                    self.activate(&run, Purpose::QuarantineRetry);
                }
            }
            Internal::Signal(signal, reply) => self.signal(signal, reply),
            Internal::Resume { task, secret } => self.resume(&task, &secret),
            Internal::Seal(run) => self.seal(&run),
            Internal::Erase(run) => self.erase(&run, None),
            Internal::Cancel(run, message) => self.cancel(&run, Some(message), ReplyTo::Nobody),
        }
    }

    pub(crate) fn answer(&mut self, tag: u64, answer: Result<NodeReply, LinkError>) {
        let Some(ask) = self.asks.remove(&tag) else {
            return;
        };
        let admission = match answer {
            Ok(NodeReply::Organization(OrgReply::Admission(admission))) => admission,
            Ok(NodeReply::NotOwner { .. }) => Err(Refusal::Unavailable(
                "the organization's actor is moving: try again".into(),
            )),
            Ok(other) => Err(Refusal::Unavailable(format!(
                "the organization's actor answered {other:?}"
            ))),
            Err(error) => Err(Refusal::Unavailable(error.to_string())),
        };
        match ask {
            Ask::Start {
                start,
                reply,
                neighbours,
            } => self.admitted(start, reply, admission, &neighbours),
            Ask::Child {
                task,
                delivery,
                start,
                neighbours,
            } => self.child_admitted(&task, delivery, start, admission, &neighbours),
        }
    }

    pub(crate) fn step(&mut self, step: Step) {
        match step {
            Step::StartKey {
                start,
                reply,
                found,
                neighbours,
            } => self.start_key_read(start, reply, found, neighbours),
            Step::SignalChecked {
                signal,
                reply,
                checked,
            } => match checked {
                Ok(true) => self.reply(reply, PartitionReply::Enqueued(Ok(Enqueued::Duplicate))),
                Ok(false) => self.enqueue_signal(*signal, reply, true),
                Err(refusal) => self.reply(reply, PartitionReply::Enqueued(Err(refusal))),
            },
            Step::Webhook {
                run,
                payload,
                key,
                reply,
                target,
            } => match target {
                Ok(Some(target)) => self.dispatch(OwnerMsg::Internal(Internal::Signal(
                    Box::new(SignalAtOwner {
                        run: target.run,
                        name: target.name,
                        payload,
                        key,
                    }),
                    reply,
                ))),
                // The run it names ended, was erased, or never was.
                Ok(None) => self.reply(
                    reply,
                    PartitionReply::Enqueued(Err(
                        run.map_or(Refusal::TaskGone, Refusal::RunNotFound)
                    )),
                ),
                Err(error) => self.reply(
                    reply,
                    PartitionReply::Enqueued(Err(Refusal::Unavailable(error))),
                ),
            },
            Step::Ending {
                run,
                committed,
                reads,
            } => match reads {
                Ok(reads) => self.commit_activation(run, *committed, reads),
                Err(error) => {
                    tracing::error!(%run, %error, "the end of a run could not be prepared");
                    self.activation_failed(run, error);
                }
            },
            Step::Prepared { run, node, request } => match request {
                Ok(request) => {
                    self.remote.insert(run, node.clone());
                    self.out.push(Effect::Activate {
                        node,
                        request,
                        asker: crate::node::message::Asker {
                            partition: self.partition,
                            tag: 0,
                        },
                    });
                }
                Err(error) => self.activation_failed(run, error),
            },
            Step::Sealed {
                run,
                attempt,
                result,
            } => self.sealed(&run, attempt, result),
            Step::Erased { run, result } => self.erased(&run, result),
            Step::Consumed { id, result } => self.consumed(id, result),
            Step::Counted {
                tenant,
                namespace,
                included,
                abandoned,
                result,
            } => self.counted(tenant, namespace, included, abandoned, result),
            Step::Parked { secret, tasks } => match tasks {
                Ok(tasks) => {
                    for task in tasks {
                        self.dispatch(OwnerMsg::Internal(Internal::Resume {
                            task,
                            secret: secret.clone(),
                        }));
                    }
                }
                Err(error) => {
                    tracing::error!(%secret, %error, "the calls parked for a secret could not be found");
                }
            },
            Step::Namespace {
                tenant,
                name,
                found,
            } => match found {
                Ok(Some(namespace)) if namespace.deleting_since.is_some() => {
                    self.deleting.insert((tenant.clone(), name.clone()));
                    self.erase_namespace(tenant, name);
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(%tenant, %name, %error, "a namespace could not be read")
                }
            },
            Step::NamespaceRuns {
                tenant,
                name,
                found,
            } => self.namespace_runs(tenant, name, found),
            Step::NamespaceDropped {
                tenant,
                name,
                result,
            } => match result {
                Ok(()) => {
                    self.deleting.remove(&(tenant.clone(), name.clone()));
                    self.counted.remove(&(tenant.clone(), name.clone()));
                    tracing::info!(%tenant, namespace = %name, "namespace erased");
                }
                Err(error) => {
                    tracing::error!(%tenant, namespace = %name, %error, "an erased namespace could not be dropped: tried again");
                    let at = self.now().saturating_add(Duration::from_secs(1));
                    self.out.push(Effect::Wake {
                        at,
                        alone: false,
                        message: Message::Owner(
                            self.partition,
                            OwnerMsg::ControlPlane(crate::ports::ControlPlaneChange::Namespace {
                                tenant,
                                name,
                            }),
                        ),
                    });
                }
            },
        }
    }

    // ---------------------------------------------------------------------------------------
    // Starts

    /// A start the edge checked: a known start key answers its run; otherwise the organization
    /// actor admits it, then it is created.
    fn start(&mut self, start: Box<StartAtOwner>, reply: ReplyTo) {
        let tenant = start.run.tenant.clone();
        if self
            .deleting
            .contains(&(tenant.clone(), start.namespace.clone()))
        {
            self.reply(
                reply,
                PartitionReply::Started(Err(Refusal::NamespaceDeleting)),
            );
            return;
        }
        if let Some(parent) = start.metadata.parent
            && !self.runs.contains_key(&parent.run)
            && self.log.is_none()
        {
            self.reply(
                reply,
                PartitionReply::Started(Err(Refusal::ParentNotStarted(parent.run))),
            );
            return;
        }
        let key = start.start_key.clone();
        if let Some(key) = &key {
            let slot = (tenant.clone(), start.namespace.clone(), key.clone());
            if let Some(waiting) = self.starting.get_mut(&slot) {
                waiting.push(reply);
                return;
            }
            self.starting.insert(slot, Vec::new());
        }
        let Some(log) = self.log.clone() else {
            return;
        };
        let partition = self.partition;
        self.out.push(job(async move {
            let found = match &key {
                Some(key) => log
                    .started(&tenant, &start.namespace, key)
                    .await
                    .map_err(|error| error.to_string()),
                None => Ok(None),
            };
            let neighbours = match found {
                Ok(None) => neighbours(&*log, &start.run, &start.namespace)
                    .await
                    .map_err(|error| error.to_string()),
                _ => Ok(Vec::new()),
            };
            step(
                partition,
                Step::StartKey {
                    start,
                    reply,
                    found,
                    neighbours,
                },
            )
        }));
    }

    fn start_key_read(
        &mut self,
        start: Box<StartAtOwner>,
        reply: ReplyTo,
        found: Result<Option<RunId>, String>,
        neighbours: Result<Vec<RunId>, String>,
    ) {
        // A known key answers its run; otherwise the neighbours read go with the start.
        let found = found.and_then(|found| match (found, neighbours) {
            (Some(existing), _) => Ok(Ok(existing)),
            (None, Ok(neighbours)) => Ok(Err(neighbours)),
            (None, Err(error)) => Err(error),
        });
        match found {
            Ok(Ok(existing)) => {
                let created = Created {
                    run: RunRef::new(start.run.tenant.clone(), existing),
                    created: false,
                };
                let waiting = self.starting_done(&start);
                for reply in std::iter::once(reply).chain(waiting) {
                    self.reply(reply, PartitionReply::Started(Ok(created.clone())));
                }
            }
            Ok(Err(neighbours)) => self.ask_admission(start, reply, neighbours),
            Err(error) => {
                let waiting = self.starting_done(&start);
                for reply in std::iter::once(reply).chain(waiting) {
                    self.reply(
                        reply,
                        PartitionReply::Started(Err(Refusal::Unavailable(error.clone()))),
                    );
                }
            }
        }
    }

    /// The other requests with the start's key that waited for it.
    fn starting_done(&mut self, start: &StartAtOwner) -> Vec<ReplyTo> {
        start
            .start_key
            .as_ref()
            .and_then(|key| {
                self.starting.remove(&(
                    start.run.tenant.clone(),
                    start.namespace.clone(),
                    key.clone(),
                ))
            })
            .unwrap_or_default()
    }

    fn admit_request(&self, start: &StartAtOwner) -> (Target, NodeRequest) {
        let tenant = start.run.tenant.clone();
        (
            Target::Home(tenant.clone()),
            NodeRequest::Organization {
                tenant,
                request: Box::new(OrgRequest::Admit(Admit {
                    partition: self.partition,
                    epoch: self.epoch,
                    run: start.run.id,
                    namespace: start.namespace.clone(),
                    code: start.started.code,
                    at: start.created_at,
                })),
            },
        )
    }

    fn ask_admission(&mut self, start: Box<StartAtOwner>, reply: ReplyTo, neighbours: Vec<RunId>) {
        let (to, request) = self.admit_request(&start);
        self.ask(
            to,
            request,
            Ask::Start {
                start,
                reply,
                neighbours,
            },
        );
    }

    fn admitted(
        &mut self,
        start: Box<StartAtOwner>,
        reply: ReplyTo,
        admission: Result<(), Refusal>,
        neighbours: &[RunId],
    ) {
        if let Err(refusal) = admission {
            let waiting = self.starting_done(&start);
            for reply in std::iter::once(reply).chain(waiting) {
                self.reply(reply, PartitionReply::Started(Err(refusal.clone())));
            }
            return;
        }
        let run = start.run.clone();
        // The starts with the same key, those that waited and those that come until the run is
        // durable, get it once it is: until then the partition does not hold its start key.
        let slot = start
            .start_key
            .clone()
            .map(|key| (run.tenant.clone(), start.namespace.clone(), key));
        let mut unit = Unit::new();
        self.create(&mut unit, *start, neighbours);
        let created = Created {
            run: run.clone(),
            created: true,
        };
        unit.then(After::Reply(reply, PartitionReply::Started(Ok(created))));
        if let Some(slot) = slot {
            unit.then(After::Started { slot, run });
        }
        self.push(unit);
    }

    /// The characters of a new run's suffix: the fewest, at least 12, that no other run of its
    /// namespace has, among `neighbours` (read before the start was admitted) and the runs this
    /// owner created since (`archi/06-storage-and-scale.md`, "Run handles").
    fn suffix_len(&self, run: &RunRef, namespace: &NamespaceName, neighbours: &[RunId]) -> u8 {
        let mut others = neighbours.to_vec();
        for (low, high) in rewind_core::suffix_neighbours(run.id) {
            others.extend(
                self.runs
                    .range(low..=high)
                    .filter(|(_, entry)| {
                        matches!(entry, Entry::Present(slot)
                            if slot.head.run.tenant == run.tenant && slot.head.namespace == *namespace)
                    })
                    .map(|(id, _)| *id),
            );
        }
        u8::try_from(rewind_core::suffix_len_apart(run.id, others)).unwrap_or(u8::MAX)
    }

    /// Creates a run in `unit`: its head, with the shortest suffix its namespace leaves it, its
    /// start key, its pending summary, its `run_started` in its inbox, its count; the run is
    /// queued once durable.
    pub(crate) fn create(&mut self, unit: &mut Unit<P>, start: StartAtOwner, neighbours: &[RunId]) {
        let suffix_len = self.suffix_len(&start.run, &start.namespace, neighbours);
        let StartAtOwner {
            run,
            namespace,
            origin,
            started,
            metadata,
            start_key,
            headline,
            created_at,
        } = start;
        let head = RunHead {
            run: run.clone(),
            namespace: namespace.clone(),
            origin,
            suffix_len,
            code: started.code,
            action_set: started.action_set,
            status: RunStatus::Pending,
            epoch: started.epoch,
            attempt: Attempt::FIRST,
            sealed: None,
            failures: 0,
            attempts: 0,
            quarantine: None,
            waits: Vec::new(),
            expires_at: started.expires_at,
            history_len: 0,
            metadata,
            idempotency_key: start_key.clone(),
            created_at,
            updated_at: created_at,
            ended_at: None,
        };
        let summary = derive::pending_summary(&head, &headline);
        unit.change(CreateRun {
            run: run.clone(),
            head: head.clone(),
            start_key,
            entries: Vec::new(),
            payloads: Vec::new(),
            summary: summary.clone(),
        });
        unit.change(Change::Counters(vec![derive::created_counters(&head)]));
        let mut slot = RunSlot::new(head);
        slot.summary = Some(summary);
        self.runs.insert(run.id, Entry::Present(Box::new(slot)));
        self.enqueue(unit, &run, Trigger::RunStarted(started), None);
        unit.then(After::Report {
            tenant: run.tenant.clone(),
            namespace,
            included: vec![run.id],
            abandoned: Vec::new(),
        });
    }

    /// Puts an item in a run's inbox in `unit`; the run is queued once durable.
    pub(crate) fn enqueue(
        &mut self,
        unit: &mut Unit<P>,
        run: &RunRef,
        trigger: Trigger,
        key: Option<String>,
    ) {
        let id = self.next_inbox;
        self.next_inbox += 1;
        let now = self.now();
        unit.change(Change::Enqueue {
            run: run.clone(),
            item: InboxItem {
                id,
                trigger,
                received_at: now,
            },
            signal_key: key.clone(),
        });
        if let Some(slot) = self.slot_mut(run.id)
            && !slot.head.is_ended()
        {
            slot.inbox.insert(id, now);
            slot.keys.extend(key);
        }
        unit.then(After::Queue(run.clone()));
    }

    // ---------------------------------------------------------------------------------------
    // Signals, cancellation, quarantine

    pub(crate) fn signal(&mut self, signal: Box<SignalAtOwner>, reply: ReplyTo) {
        let Some(slot) = self.slot(signal.run.id) else {
            let refusal = if is_reserved_signal(&signal.name) {
                Refusal::Signal(
                    crate::workflows::SignalRefused::Reserved(signal.name.clone()).to_string(),
                )
            } else {
                Refusal::RunNotFound(signal.run.id)
            };
            self.reply(reply, PartitionReply::Enqueued(Err(refusal)));
            return;
        };
        if is_reserved_signal(&signal.name) {
            let refused =
                crate::workflows::SignalRefused::Reserved(signal.name.clone()).to_string();
            self.reply(
                reply,
                PartitionReply::Enqueued(Err(Refusal::Signal(refused))),
            );
            return;
        }
        if signal
            .key
            .as_ref()
            .is_some_and(|key| slot.keys.contains(key))
        {
            self.reply(reply, PartitionReply::Enqueued(Ok(Enqueued::Duplicate)));
            return;
        }
        if slot.head.is_ended() {
            // A key seen is acknowledged for the life of the run, sealed or not.
            match (&signal.key, &self.log) {
                (Some(key), Some(log)) => {
                    let log = Arc::clone(log);
                    let key = key.clone();
                    let run = signal.run.clone();
                    self.out.push(job(async move {
                        let answer = match log.signal_key(&run, &key).await {
                            Ok(true) => Ok(Enqueued::Duplicate),
                            Ok(false) => Ok(Enqueued::RunEnded),
                            Err(error) => Err(Refusal::Unavailable(error.to_string())),
                        };
                        Message::Reply(
                            reply,
                            NodeReply::Partition(Box::new(PartitionReply::Enqueued(answer))),
                        )
                    }));
                }
                _ => self.reply(reply, PartitionReply::Enqueued(Ok(Enqueued::RunEnded))),
            }
            return;
        }
        if slot.head.metadata.workflow.is_none() && signal.key.is_none() {
            self.enqueue_signal(*signal, reply, false);
            return;
        }
        let head = slot.head.clone();
        let Some(log) = self.log.clone() else {
            return;
        };
        let plane = Arc::clone(&self.plane);
        let partition = self.partition;
        self.out.push(job(async move {
            let checked = async {
                if let Err(check) =
                    crate::checks::signal(plane.catalog(), &head, &signal.name, &signal.payload)
                        .await
                {
                    return Err(match check {
                        crate::checks::SignalCheck::Refused(refused) => Refusal::Signal(refused),
                        crate::checks::SignalCheck::Unavailable(error) => {
                            Refusal::Unavailable(error)
                        }
                    });
                }
                match &signal.key {
                    Some(key) => log
                        .signal_key(&signal.run, key)
                        .await
                        .map_err(|error| Refusal::Unavailable(error.to_string())),
                    None => Ok(false),
                }
            }
            .await;
            step(
                partition,
                Step::SignalChecked {
                    signal,
                    reply,
                    checked,
                },
            )
        }));
    }

    fn enqueue_signal(&mut self, signal: SignalAtOwner, reply: ReplyTo, checked: bool) {
        let Some(slot) = self.slot(signal.run.id) else {
            self.reply(
                reply,
                PartitionReply::Enqueued(Err(Refusal::RunNotFound(signal.run.id))),
            );
            return;
        };
        if checked
            && signal
                .key
                .as_ref()
                .is_some_and(|key| slot.keys.contains(key))
        {
            self.reply(reply, PartitionReply::Enqueued(Ok(Enqueued::Duplicate)));
            return;
        }
        if slot.head.is_ended() {
            self.reply(reply, PartitionReply::Enqueued(Ok(Enqueued::RunEnded)));
            return;
        }
        let SignalAtOwner {
            run,
            name,
            payload,
            key,
        } = signal;
        let mut unit = Unit::new();
        self.enqueue(
            &mut unit,
            &run,
            Trigger::Completion(Completion::SignalReceived {
                name,
                payload: Payload::Value(payload),
            }),
            key,
        );
        unit.then(After::Reply(
            reply,
            PartitionReply::Enqueued(Ok(Enqueued::Accepted)),
        ));
        self.push(unit);
    }

    fn webhook(&mut self, token: &str, payload: JsonValue, key: Option<String>, reply: ReplyTo) {
        let Some(log) = self.log.clone() else {
            return;
        };
        let run = super::tokens::run_of(token);
        let hash = super::tokens::hash(token);
        let partition = self.partition;
        self.out.push(job(async move {
            let target = log.webhook(hash).await.map_err(|error| error.to_string());
            step(
                partition,
                Step::Webhook {
                    run,
                    payload,
                    key,
                    reply,
                    target,
                },
            )
        }));
    }

    pub(crate) fn cancel(&mut self, run: &RunRef, message: Option<String>, reply: ReplyTo) {
        let Some(slot) = self.slot(run.id) else {
            self.reply(
                reply,
                PartitionReply::Enqueued(Err(Refusal::RunNotFound(run.id))),
            );
            return;
        };
        if slot.head.is_ended() {
            self.reply(reply, PartitionReply::Enqueued(Ok(Enqueued::RunEnded)));
            return;
        }
        let mut unit = Unit::new();
        self.enqueue(
            &mut unit,
            run,
            Trigger::Completion(Completion::CancelRequested {
                reason: CancelReason::Requested(message),
            }),
            None,
        );
        unit.then(After::Reply(
            reply,
            PartitionReply::Enqueued(Ok(Enqueued::Accepted)),
        ));
        self.push(unit);
    }

    fn release(&mut self, run: &RunRef, reply: ReplyTo) {
        let quarantined = self
            .slot(run.id)
            .is_some_and(|slot| slot.head.quarantine.is_some() && !slot.head.is_ended());
        if !quarantined {
            self.reply(reply, PartitionReply::Done(Ok(false)));
            return;
        }
        let mut unit = Unit::new();
        self.release_in(&mut unit, run);
        unit.then(After::Reply(reply, PartitionReply::Done(Ok(true))));
        unit.then(After::Queue(run.clone()));
        self.push(unit);
    }

    /// Lifts the run's quarantine in `unit`.
    fn release_in(&mut self, unit: &mut Unit<P>, run: &RunRef) {
        unit.change(Change::Release { run: run.clone() });
        if let Some(slot) = self.slot_mut(run.id) {
            slot.head.quarantine = None;
            slot.head.failures = 0;
            slot.head.attempts = 0;
            if let Some(summary) = &mut slot.summary {
                summary.summary.quarantine = None;
            }
        }
    }

    fn give_up(&mut self, run: &RunRef, reply: ReplyTo) {
        let Some(slot) = self.slot_mut(run.id) else {
            self.reply(reply, PartitionReply::Done(Ok(false)));
            return;
        };
        if slot.head.quarantine.is_none() || slot.head.is_ended() {
            self.reply(reply, PartitionReply::Done(Ok(false)));
            return;
        }
        if slot.busy.is_some() {
            slot.waiting.push(OwnerMsg::Request(
                Box::new(PartitionRequest::GiveUp { run: run.clone() }),
                reply,
            ));
            return;
        }
        slot.give_up.push(reply);
        self.activate(run, Purpose::GiveUp);
    }

    // ---------------------------------------------------------------------------------------
    // Activations

    /// Queues the run if it is runnable: something in its inbox, not busy, not quarantined, not
    /// ended.
    pub(crate) fn queue(&mut self, run: &RunRef) {
        let runnable = self.slot(run.id).is_some_and(|slot| {
            slot.busy.is_none()
                && !slot.ending
                && !slot.head.is_ended()
                && slot.head.quarantine.is_none()
                && !slot.inbox.is_empty()
        });
        if runnable {
            self.activate(run, Purpose::Runnable);
        }
    }

    /// Hands the run to an executor: its node's when its build runs the run's epoch (and the
    /// tenant's pool is the node's), another node's through the link otherwise.
    pub(crate) fn activate(&mut self, run: &RunRef, purpose: Purpose) {
        let partition = self.partition;
        let Some(log) = self.log.clone() else {
            return;
        };
        let pool = self.config.pools.get(&run.tenant).cloned();
        let local_pool = pool == self.config.pool;
        let Some(slot) = self.slot_mut(run.id) else {
            return;
        };
        slot.busy = Some(purpose);
        let oldest = slot
            .inbox
            .values()
            .min()
            .copied()
            .unwrap_or(slot.head.updated_at);
        let head = slot.head.clone();
        let summary = slot.summary.clone();
        if self.epochs.contains(&head.epoch) && local_pool {
            self.out
                .push(Effect::Local(Message::Executors(ExecMsg::Runnable(
                    Box::new(LocalRun {
                        partition,
                        log,
                        head,
                        summary,
                        purpose,
                        oldest,
                    }),
                ))));
            return;
        }
        // A remote executor: the node used last while it lives, else the first that runs the
        // epoch in the tenant's pool.
        let candidates: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|node| node.epochs.contains(&head.epoch) && node.pool == pool)
            .map(|node| node.id.clone())
            .collect();
        let sticky = self
            .remote
            .get(&run.id)
            .filter(|node| candidates.contains(node))
            .cloned();
        let warm = sticky.is_some();
        let Some(node) = sticky.or_else(|| candidates.first().cloned()) else {
            // A node may have joined since the members were read: they are read again once.
            if self.awaiting_nodes.insert(run.id) {
                self.awaiting_purposes.insert(run.id, purpose);
                self.out.push(Effect::Local(Message::Node(
                    crate::node::NodeMsg::RefreshNodes,
                )));
                return;
            }
            self.awaiting_nodes.remove(&run.id);
            let error = format!(
                "no node runs the epoch {} of run {}{}",
                head.epoch,
                run.id,
                pool.map(|pool| format!(" in the pool {pool}"))
                    .unwrap_or_default()
            );
            self.activation_failed(run.id, error);
            return;
        };
        let plane = Arc::clone(&self.plane);
        let run_id = run.id;
        self.out.push(job(async move {
            let request = crate::executors::prepare(
                &*plane,
                &*log,
                head,
                summary,
                purpose,
                !warm || purpose != Purpose::Runnable,
            )
            .await
            .map(Box::new);
            step(
                partition,
                Step::Prepared {
                    run: run_id,
                    node,
                    request,
                },
            )
        }));
    }

    /// An activation finished.
    pub(crate) fn activated(&mut self, run: RunId, result: ActivationResult) {
        let Some(slot) = self.slot(run) else {
            // The run was erased meanwhile: what it did is dropped.
            return;
        };
        let run_ref = slot.run().clone();
        let purpose = slot.busy.unwrap_or(Purpose::Runnable);
        match result {
            ActivationResult::Committed(committed) => {
                let terminal = committed.activation.head.is_ended();
                if terminal || committed.copied {
                    if let Some(slot) = self.slot_mut(run) {
                        slot.ending = terminal;
                    }
                    self.read_ending(&run_ref, *committed);
                } else {
                    self.commit_activation(run, *committed, EndReads::default());
                }
            }
            ActivationResult::Quarantined { kind, reason } => {
                let now = self.now();
                let retried_by =
                    (purpose == Purpose::QuarantineRetry).then(|| self.config.build.clone());
                let quarantine = Quarantine {
                    kind,
                    reason,
                    since: now,
                    retried_by,
                };
                let mut unit = Unit::new();
                unit.change(Change::Quarantine {
                    run: run_ref.clone(),
                    quarantine: quarantine.clone(),
                });
                if purpose != Purpose::QuarantineRetry {
                    crate::metrics::quarantined(kind);
                    self.quarantined_webhook(&mut unit, &run_ref, &quarantine);
                }
                if let Some(slot) = self.slot_mut(run) {
                    slot.head.quarantine = Some(quarantine.clone());
                    if let Some(summary) = &mut slot.summary {
                        summary.summary.quarantine =
                            Some(crate::ports::QuarantineNote::from(&quarantine));
                    }
                }
                unit.then(After::Forget(run_ref.clone()));
                unit.then(After::Idle(run_ref));
                self.push(unit);
            }
            ActivationResult::Released => {
                let mut unit = Unit::new();
                if self
                    .slot(run)
                    .is_some_and(|slot| slot.head.quarantine.is_some())
                {
                    self.release_in(&mut unit, &run_ref);
                }
                for reply in self
                    .slot_mut(run)
                    .map(|slot| std::mem::take(&mut slot.give_up))
                    .unwrap_or_default()
                {
                    unit.then(After::Reply(reply, PartitionReply::Done(Ok(false))));
                }
                unit.then(After::Idle(run_ref));
                self.push(unit);
            }
            ActivationResult::Failed { error } => self.activation_failed(run, error),
            ActivationResult::Miss => {
                // The remote executor lost the instance: send the history.
                self.remote.remove(&run);
                let Some(log) = self.log.clone() else {
                    return;
                };
                let Some(slot) = self.slot(run) else {
                    return;
                };
                let (head, summary) = (slot.head.clone(), slot.summary.clone());
                let node = self
                    .nodes
                    .iter()
                    .find(|node| node.epochs.contains(&head.epoch))
                    .map(|node| node.id.clone());
                let Some(node) = node else {
                    self.activation_failed(run, "no node runs the run's epoch".into());
                    return;
                };
                let plane = Arc::clone(&self.plane);
                let partition = self.partition;
                self.out.push(job(async move {
                    let request =
                        crate::executors::prepare(&*plane, &*log, head, summary, purpose, true)
                            .await
                            .map(Box::new);
                    step(partition, Step::Prepared { run, node, request })
                }));
            }
        }
    }

    /// An activation failed and committed nothing: the failure is counted, and a timer tries
    /// the run again; the fifth in a row quarantines it.
    fn activation_failed(&mut self, run: RunId, error: String) {
        let Some(slot) = self.slot(run) else {
            return;
        };
        let run_ref = slot.run().clone();
        let purpose = slot.busy.unwrap_or(Purpose::Runnable);
        let now = self.now();
        let error = text::storable(error);
        let mut unit = Unit::new();
        unit.then(After::Forget(run_ref.clone()));
        if purpose == Purpose::QuarantineRetry {
            // A quarantined run whose retry fails stays quarantined, retried by this build.
            if let Some(mut quarantine) = slot.head.quarantine.clone() {
                quarantine.retried_by = Some(self.config.build.clone());
                unit.change(Change::Quarantine {
                    run: run_ref.clone(),
                    quarantine: quarantine.clone(),
                });
                if let Some(slot) = self.slot_mut(run) {
                    slot.head.quarantine = Some(quarantine);
                }
            }
            unit.then(After::Idle(run_ref));
            self.push(unit);
            return;
        }
        let failures = slot.head.failures.saturating_add(1);
        unit.change(Change::Failure {
            run: run_ref.clone(),
            error: error.clone(),
            at: now,
        });
        if failures >= crate::ports::QUARANTINE_AFTER_FAILURES {
            let quarantine = Quarantine {
                kind: QuarantineKind::Failures,
                reason: error,
                since: now,
                retried_by: None,
            };
            tracing::error!(run = %run_ref, failures, "the run's activations keep failing: run quarantined");
            crate::metrics::quarantined(QuarantineKind::Failures);
            self.quarantined_webhook(&mut unit, &run_ref, &quarantine);
            if let Some(slot) = self.slot_mut(run) {
                slot.head.failures = failures;
                slot.head.quarantine = Some(quarantine);
            }
        } else {
            unit.change(Change::Timer(TimerChange::Set(DueTimer {
                at: now.saturating_add(self.config.activation_retry),
                kind: TimerKind::ActivationRetry {
                    run: run_ref.clone(),
                },
            })));
            if let Some(slot) = self.slot_mut(run) {
                slot.head.failures = failures;
            }
        }
        for reply in self
            .slot_mut(run)
            .map(|slot| std::mem::take(&mut slot.give_up))
            .unwrap_or_default()
        {
            unit.then(After::Reply(
                reply,
                PartitionReply::Done(Err(Refusal::Unavailable(
                    "the run could not be given up: try again".into(),
                ))),
            ));
        }
        // The run is tried again by its timer, not at once.
        unit.then(After::Rest(run_ref));
        self.push(unit);
    }

    /// The run.quarantined webhook, in `unit`'s outbox.
    fn quarantined_webhook(&mut self, unit: &mut Unit<P>, run: &RunRef, quarantine: &Quarantine) {
        let Some(slot) = self.slot(run.id) else {
            return;
        };
        let event = crate::ports::WebhookEvent {
            id: format!("{}/quarantined/{}", run.id, quarantine.since.0),
            tenant: run.tenant.clone(),
            namespace: Some(slot.head.namespace.clone()),
            run: Some(run.id),
            labels: slot.head.metadata.labels.clone(),
            occurred_at: quarantine.since,
            kind: crate::ports::WebhookEventKind::RunQuarantined {
                reason: quarantine.reason.clone(),
            },
        };
        let entry = self.outbox_entry(
            run.tenant.clone(),
            Some(run.id),
            Projection::Webhooks(event),
        );
        unit.project(entry);
    }

    /// Reads what a terminal activation, or a fork's first, needs before it is committed: the
    /// watchers of the run, its attached children, the ends of the children it inherited.
    fn read_ending(&mut self, run: &RunRef, committed: Committed) {
        let Some(log) = self.log.clone() else {
            return;
        };
        let plane = Arc::clone(&self.plane);
        let partition = self.partition;
        let run = run.clone();
        self.out.push(job(async move {
            let reads = end_reads(&*plane, &*log, &run, &committed).await;
            step(
                partition,
                Step::Ending {
                    run: run.id,
                    committed: Box::new(committed),
                    reads,
                },
            )
        }));
    }

    /// Commits an activation: its changes, its outbox entries, and for a terminal one the end
    /// sent to its watchers and the cancellation of the runs it started attached, in the same
    /// batch.
    pub(crate) fn commit_activation(&mut self, run: RunId, committed: Committed, reads: EndReads) {
        let Some(slot) = self.slot(run) else {
            return;
        };
        let run_ref = slot.run().clone();
        let Committed {
            mut activation,
            projections,
            end,
            children: _,
            copied: _,
            cpu,
            mode,
            measurements,
        } = committed;
        let terminal = activation.head.is_ended();
        let previous_len = slot.head.history_len;
        // What the owner knows of the tasks since the activation read them.
        let phases: BTreeMap<Seq, crate::ports::TaskPhase> = slot
            .tasks
            .iter()
            .map(|(seq, task)| (*seq, derive::phase_of(task)))
            .chain(
                activation
                    .tasks
                    .iter()
                    .map(|task| (task.task.task.seq, derive::phase_of(task))),
            )
            .collect();
        activation.summary = derive::with_phases(activation.summary, &phases);
        activation.head.waits = activation.summary.summary.waits.clone();
        let mut unit = Unit::new();
        unit.then(After::Measure(measurements));
        for projection in projections {
            let entry = self.outbox_entry(run_ref.tenant.clone(), Some(run), projection);
            activation.outbox.push(entry.clone());
            unit.then(After::Project(entry));
        }
        let entries = activation.entries.len() as u64;
        let new_tasks: Vec<crate::ports::TaskRef> = activation
            .tasks
            .iter()
            .filter(|task| matches!(task.task.state, crate::ports::TaskState::Ready { .. }))
            .map(|task| task.task.task.clone())
            .collect();
        let head = activation.head.clone();
        let summary = activation.summary.clone();
        let consumed = activation.consumed.clone();
        let tasks = activation.tasks.clone();
        let namespace = head.namespace.clone();
        let started = previous_len == 0 && entries > 0;
        unit.change(Change::Activation(Box::new(activation)));
        // The runs it reaches, in the same batch.
        if terminal && let Some(end) = &end {
            let name = end_signal_name(run);
            for watcher in &reads.watchers {
                let watcher = RunRef::new(run_ref.tenant.clone(), watcher.run);
                self.enqueue(
                    &mut unit,
                    &watcher,
                    Trigger::Completion(Completion::SignalReceived {
                        name: name.clone(),
                        payload: Payload::Value(end.clone()),
                    }),
                    Some(name.clone()),
                );
            }
        }
        if terminal {
            for child in &reads.cancel {
                let child = RunRef::new(run_ref.tenant.clone(), *child);
                self.enqueue(
                    &mut unit,
                    &child,
                    Trigger::Completion(Completion::CancelRequested {
                        reason: starter_ended(run),
                    }),
                    None,
                );
            }
        }
        for (seq, child, child_end) in &reads.inherited {
            let child_ref = RunRef::new(run_ref.tenant.clone(), *child);
            unit.change(Change::Watch {
                run: child_ref,
                watcher: Watcher {
                    run,
                    seq: *seq,
                    attached: false,
                },
            });
            if let Some(child_end) = child_end
                && !terminal
            {
                let name = end_signal_name(*child);
                self.enqueue(
                    &mut unit,
                    &run_ref,
                    Trigger::Completion(Completion::SignalReceived {
                        name: name.clone(),
                        payload: Payload::Value(child_end.clone()),
                    }),
                    Some(name),
                );
            }
        }
        // What the owner keeps, as the batch leaves it.
        let mut freed = Vec::new();
        if let Some(slot) = self.slot_mut(run) {
            slot.head = head.clone();
            slot.summary = Some(summary);
            slot.mode = slot.mode.or(mode);
            for id in &consumed {
                slot.inbox.remove(id);
            }
            for task in tasks {
                slot.tasks.insert(task.task.task.seq, task);
            }
            if terminal {
                slot.inbox.clear();
                freed = slot.tasks.keys().copied().collect();
                slot.tasks.clear();
            }
        }
        if terminal {
            for seq in freed {
                let task = crate::ports::TaskRef {
                    run: run_ref.clone(),
                    seq,
                };
                self.drop_flight(&mut unit, &task);
            }
            let replies = self
                .slot_mut(run)
                .map(|slot| std::mem::take(&mut slot.give_up))
                .unwrap_or_default();
            for reply in replies {
                unit.then(After::Reply(reply, PartitionReply::Done(Ok(true))));
            }
            unit.then(After::Forget(run_ref.clone()));
            unit.then(After::Seal(run_ref.clone()));
            for watcher in &reads.watchers {
                unit.then(After::Queue(RunRef::new(
                    run_ref.tenant.clone(),
                    watcher.run,
                )));
            }
        } else {
            unit.then(After::Confirm(run_ref.clone(), head.history_len));
            let mut new_tasks = new_tasks;
            if self.config.reversed_calls {
                new_tasks.reverse();
            }
            for task in new_tasks {
                unit.then(After::Dispatch(task));
            }
        }
        if terminal || started || !cpu.is_zero() {
            unit.then(After::Report {
                tenant: run_ref.tenant.clone(),
                namespace,
                included: Vec::new(),
                abandoned: Vec::new(),
            });
        }
        unit.then(After::Idle(run_ref));
        self.push(unit);
    }

    // ---------------------------------------------------------------------------------------
    // Sealing, retention and erasure

    /// Writes the ended attempt's journal to the archive; its seal follows in a later batch.
    /// One seal at a time, and none while the run is being erased.
    pub(crate) fn seal(&mut self, run: &RunRef) {
        let Some(slot) = self.slot_mut(run.id) else {
            return;
        };
        if !slot.head.is_unsealed() || slot.sealing || slot.erasing {
            return;
        }
        slot.sealing = true;
        let head = slot.head.clone();
        let Some(log) = self.log.clone() else {
            return;
        };
        let plane = Arc::clone(&self.plane);
        let partition = self.partition;
        let run = run.clone();
        self.out.push(job(async move {
            let result = seal_attempt(&*plane, &*log, &head).await;
            step(
                partition,
                Step::Sealed {
                    run,
                    attempt: head.attempt,
                    result,
                },
            )
        }));
    }

    fn sealed(&mut self, run: &RunRef, attempt: Attempt, result: Result<Duration, String>) {
        let Some(slot) = self.slot_mut(run.id) else {
            return;
        };
        slot.sealing = false;
        // An erasure that waited for the seal goes on.
        let waiting = std::mem::take(&mut slot.waiting);
        let retention = match result {
            Ok(retention) => retention,
            Err(error) => {
                tracing::error!(%run, %error, "an ended attempt could not be sealed: tried again");
                let at = self.now().saturating_add(Duration::from_secs(1));
                self.out.push(Effect::Wake {
                    at,
                    alone: false,
                    message: Message::Owner(
                        self.partition,
                        OwnerMsg::Internal(Internal::Seal(run.clone())),
                    ),
                });
                for message in waiting {
                    self.dispatch(message);
                }
                return;
            }
        };
        let Some(slot) = self.slot_mut(run.id) else {
            return;
        };
        slot.head.sealed = Some(attempt);
        slot.ending = false;
        // The seal deleted the attempt's signal keys.
        slot.keys.clear();
        let ended_at = slot.head.ended_at.unwrap_or(slot.head.updated_at);
        let namespace = slot.head.namespace.clone();
        let mut unit = Unit::new();
        unit.change(Change::Seal {
            run: run.clone(),
            attempt,
        });
        if self.deleting.contains(&(run.tenant.clone(), namespace)) {
            unit.then(After::Owner(OwnerMsg::Internal(Internal::Erase(
                run.clone(),
            ))));
        } else {
            unit.change(Change::Timer(TimerChange::Set(DueTimer {
                at: ended_at.saturating_add(retention),
                kind: TimerKind::RetentionEnd { run: run.clone() },
            })));
        }
        self.push(unit);
        for message in waiting {
            self.dispatch(message);
        }
    }

    /// Erases a run: its objects in the archive and its blobs first, then its records,
    /// summary and counts in one batch.
    pub(crate) fn erase(&mut self, run: &RunRef, reply: Option<ReplyTo>) {
        let Some(slot) = self.slot_mut(run.id) else {
            if let Some(reply) = reply {
                self.reply(reply, PartitionReply::Done(Ok(false)));
            }
            return;
        };
        if slot.erasing {
            if let Some(reply) = reply {
                slot.give_up.push(reply);
            }
            return;
        }
        if slot.busy.is_some() || slot.sealing || (slot.ending && !slot.head.is_ended()) {
            slot.waiting.push(match reply {
                Some(reply) => OwnerMsg::Request(
                    Box::new(PartitionRequest::Erase { run: run.clone() }),
                    reply,
                ),
                None => OwnerMsg::Internal(Internal::Erase(run.clone())),
            });
            return;
        }
        slot.ending = true;
        slot.erasing = true;
        if let Some(reply) = reply {
            slot.give_up.push(reply);
        }
        let plane = Arc::clone(&self.plane);
        let partition = self.partition;
        let run = run.clone();
        self.out.push(job(async move {
            let result = async {
                plane
                    .archive()
                    .erase(&run)
                    .await
                    .map_err(|error: ArchiveError| error.to_string())?;
                plane
                    .blobs()
                    .delete_run(&run)
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            step(partition, Step::Erased { run, result })
        }));
    }

    fn erased(&mut self, run: &RunRef, result: Result<(), String>) {
        let Some(slot) = self.slot_mut(run.id) else {
            return;
        };
        let replies = std::mem::take(&mut slot.give_up);
        if let Err(error) = result {
            slot.ending = false;
            slot.erasing = false;
            tracing::error!(%run, %error, "a run's objects could not be erased");
            for reply in replies {
                self.reply(
                    reply,
                    PartitionReply::Done(Err(Refusal::Unavailable(error.clone()))),
                );
            }
            // A seal skipped while the erasure ran.
            self.seal(run);
            return;
        }
        let namespace = slot.head.namespace.clone();
        let seqs: Vec<Seq> = slot.tasks.keys().copied().collect();
        let mut unit = Unit::new();
        for seq in seqs {
            let task = crate::ports::TaskRef {
                run: run.clone(),
                seq,
            };
            self.drop_flight(&mut unit, &task);
        }
        unit.change(Change::Erase { run: run.clone() });
        for reply in replies {
            unit.then(After::Reply(reply, PartitionReply::Done(Ok(true))));
        }
        unit.then(After::Forget(run.clone()));
        unit.then(After::Report {
            tenant: run.tenant.clone(),
            namespace: namespace.clone(),
            included: Vec::new(),
            abandoned: Vec::new(),
        });
        let deleting = (run.tenant.clone(), namespace);
        if self.deleting.contains(&deleting) {
            unit.then(After::Owner(OwnerMsg::ControlPlane(
                crate::ports::ControlPlaneChange::Namespace {
                    tenant: deleting.0.clone(),
                    name: deleting.1.clone(),
                },
            )));
        }
        // The run leaves the owner's memory with the batch.
        self.push(unit);
        self.erased.insert(run.id);
        self.runs.remove(&run.id);
    }

    // ---------------------------------------------------------------------------------------
    // Timers

    pub(crate) fn timer_due(&mut self, timer: DueTimer) {
        let DueTimer { kind, .. } = timer;
        if let TimerKind::WebhookRetry { entry, .. } = &kind {
            self.redeliver(*entry);
            return;
        }
        let Some(run) = kind.run().cloned() else {
            return;
        };
        let Some(slot) = self.slot(run.id) else {
            return;
        };
        let ended = slot.head.is_ended();
        let mut unit = Unit::new();
        unit.change(Change::Timer(TimerChange::Cancel(kind.clone())));
        match &kind {
            TimerKind::Code { seq, .. } if !ended => {
                self.enqueue(
                    &mut unit,
                    &run,
                    Trigger::Completion(Completion::TimerFired { seq: *seq }),
                    None,
                );
            }
            TimerKind::RunDeadline { .. } if !ended => {
                if slot.head.quarantine.is_some() {
                    // A quarantined run at its deadline is given up.
                    self.push(unit);
                    if self.slot(run.id).is_some_and(|slot| slot.busy.is_none()) {
                        self.activate(&run, Purpose::GiveUp);
                    }
                    return;
                }
                self.enqueue(
                    &mut unit,
                    &run,
                    Trigger::Completion(Completion::CancelRequested {
                        reason: CancelReason::Ttl,
                    }),
                    None,
                );
            }
            TimerKind::DeferredDeadline { seq, .. } if !ended => {
                let task = crate::ports::TaskRef {
                    run: run.clone(),
                    seq: *seq,
                };
                self.push(unit);
                self.deferred_deadline(&task);
                return;
            }
            TimerKind::RedeliveryBackoff { seq, .. } if !ended => {
                unit.then(After::Dispatch(crate::ports::TaskRef {
                    run: run.clone(),
                    seq: *seq,
                }));
            }
            TimerKind::ActivationRetry { .. } if !ended => {
                unit.then(After::Queue(run.clone()));
            }
            TimerKind::RetentionEnd { .. } => {
                self.push(unit);
                self.erase(&run, None);
                return;
            }
            _ => {}
        }
        self.push(unit);
    }

    pub(crate) fn horizon_reached(&mut self) {
        let Some(log) = self.log.clone() else {
            return;
        };
        let (from, to) = self.wheel.advance();
        let partition = self.partition;
        self.out.push(job(async move {
            let read = log
                .timers(from, to)
                .await
                .map_err(|error| error.to_string());
            Message::Owner(partition, OwnerMsg::HorizonRead(read))
        }));
    }

    // ---------------------------------------------------------------------------------------
    // Namespaces erased

    pub(crate) fn namespace_changed(&mut self, tenant: TenantId, name: NamespaceName) {
        let plane = Arc::clone(&self.plane);
        let partition = self.partition;
        self.out.push(job(async move {
            let found = plane
                .catalog()
                .get_namespace(&tenant, &name)
                .await
                .map_err(|error| error.to_string());
            step(
                partition,
                Step::Namespace {
                    tenant,
                    name,
                    found,
                },
            )
        }));
    }

    /// Cancels the namespace's active runs through the normal path, erases its ended runs, and
    /// once the partition holds nothing of it, drops its counters and what the control plane
    /// keeps of it.
    pub(crate) fn erase_namespace(&mut self, tenant: TenantId, name: NamespaceName) {
        let Some(log) = self.log.clone() else {
            return;
        };
        let partition = self.partition;
        self.out.push(job(async move {
            let scope = crate::ports::Scope::Namespace {
                tenant: tenant.clone(),
                namespace: name.clone(),
            };
            let query = crate::ports::RunQuery::new(tenant.clone());
            let found = log
                .search(
                    &scope,
                    &query,
                    crate::ports::RunOrder::Oldest,
                    &crate::ports::Page::first(1_000),
                )
                .await
                .map(|found| found.summaries)
                .map_err(|error| error.to_string());
            step(
                partition,
                Step::NamespaceRuns {
                    tenant,
                    name,
                    found,
                },
            )
        }));
    }

    fn namespace_runs(
        &mut self,
        tenant: TenantId,
        name: NamespaceName,
        found: Result<Vec<RunSummary>, String>,
    ) {
        let runs = match found {
            Ok(runs) => runs,
            Err(error) => {
                tracing::error!(%tenant, namespace = %name, %error, "the runs of an erased namespace could not be listed");
                return;
            }
        };
        if runs.is_empty() {
            let mut unit = Unit::new();
            let erased = NamespaceName::erased(self.partition);
            unit.change(Change::EraseNamespace {
                tenant: tenant.clone(),
                namespace: name.clone(),
                erased: erased.clone(),
            });
            // What the namespace consumed is the tenant's erased namespaces' now: its organization
            // actor counts it in the tenant's ceiling.
            unit.then(After::Report {
                tenant: tenant.clone(),
                namespace: erased,
                included: Vec::new(),
                abandoned: Vec::new(),
            });
            let plane = Arc::clone(&self.plane);
            let partition = self.partition;
            unit.then(After::Effect(job(async move {
                let result = drop_namespace(&*plane, &tenant, &name).await;
                step(
                    partition,
                    Step::NamespaceDropped {
                        tenant,
                        name,
                        result,
                    },
                )
            })));
            self.push(unit);
            return;
        }
        for summary in runs {
            let run = summary.run.clone();
            if summary.status.is_terminal() {
                self.dispatch(OwnerMsg::Internal(Internal::Erase(run)));
            } else if self.cancelling.insert(run.id) {
                // Cancelled once, through the normal path: its end erases it.
                self.dispatch(OwnerMsg::Internal(Internal::Cancel(
                    run,
                    "the namespace is being erased".into(),
                )));
            }
        }
    }
}

/// What a terminal activation, or a fork's first, reads.
async fn end_reads<P: Plane>(
    plane: &P,
    log: &crate::plane::LogOf<P>,
    run: &RunRef,
    committed: &Committed,
) -> Result<EndReads, String> {
    let read = |error: crate::ports::PartitionError| error.to_string();
    let mut reads = EndReads::default();
    let terminal = committed.activation.head.is_ended();
    if terminal {
        for watcher in log.watchers(run).await.map_err(read)? {
            let of = RunRef::new(run.tenant.clone(), watcher.run);
            if log
                .run(&of)
                .await
                .map_err(read)?
                .is_some_and(|head| !head.is_ended())
            {
                reads.watchers.push(watcher);
            }
        }
        for (_, child, attached) in &committed.children {
            if !attached {
                continue;
            }
            let child = RunRef::new(run.tenant.clone(), *child);
            let Some(head) = log.run(&child).await.map_err(read)? else {
                continue;
            };
            if head.is_ended() {
                continue;
            }
            let attached_here = log
                .watchers(&child)
                .await
                .map_err(read)?
                .iter()
                .any(|watcher| watcher.run == run.id && watcher.attached);
            if attached_here {
                reads.cancel.push(child.id);
            }
        }
    }
    if committed.copied {
        for (seq, child, _) in &committed.children {
            let child_ref = RunRef::new(run.tenant.clone(), *child);
            let Some(head) = log.run(&child_ref).await.map_err(read)? else {
                continue;
            };
            let end = if head.is_ended() {
                end_value(plane, log, &child_ref).await?
            } else {
                None
            };
            reads.inherited.push((*seq, *child, end));
        }
    }
    Ok(reads)
}

/// The end of a run that ended, as its watchers receive it; `None` while it has not ended, or
/// once it is erased.
pub(crate) async fn end_value<P: Plane>(
    plane: &P,
    log: &crate::plane::LogOf<P>,
    run: &RunRef,
) -> Result<Option<JsonValue>, String> {
    let Some(read) = super::read_attempt(plane, log, run, 0, true)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let history = read.into_entries(0);
    let state = rewind_core::RunState::from_history(&history).map_err(|error| error.to_string())?;
    let Some(outcome) = state.outcome() else {
        return Ok(None);
    };
    if let Some(end) = rewind_core::RunEnd::from_outcome(outcome) {
        return Ok(Some(end.to_json()));
    }
    let rewind_core::RunOutcome::Completed { output } = outcome else {
        return Ok(None);
    };
    let output = match output {
        Payload::Digest(_) => {
            // Above 64 KiB: in the run's blobs.
            let source = crate::executors::activation::Carried {
                records: Default::default(),
                blobs: plane.blobs(),
                owners: vec![crate::ports::BlobOwner::Run(run.clone())],
            };
            crate::payloads::load(&source, &run.tenant, output)
                .await
                .map_err(|error| error.to_string())?
        }
        Payload::Value(value) => value.clone(),
    };
    Ok(Some(rewind_core::RunEnd::Completed { output }.to_json()))
}

/// Writes the ended attempt's journal to the archive, every payload up to 64 KiB inlined, and
/// answers the run's retention.
async fn seal_attempt<P: Plane>(
    plane: &P,
    log: &crate::plane::LogOf<P>,
    head: &RunHead,
) -> Result<Duration, String> {
    let run = &head.run;
    let mut entries = log
        .history(run, head.attempt, 0)
        .await
        .map_err(|error| error.to_string())?;
    let mut values: BTreeMap<Digest, Option<JsonValue>> = BTreeMap::new();
    for entry in &mut entries {
        if let Some(payload) = crate::payloads::payload_mut(&mut entry.event)
            && let Payload::Digest(digest) = payload
        {
            let digest = *digest;
            let value = match values.get(&digest) {
                Some(value) => value.clone(),
                None => {
                    let value = log
                        .payload(run, digest)
                        .await
                        .map_err(|error| error.to_string())?
                        .and_then(|record| record.value);
                    values.insert(digest, value.clone());
                    value
                }
            };
            if let Some(value) = value {
                *payload = Payload::Value(value);
            }
        }
    }
    let code = plane
        .catalog()
        .get_script(&run.tenant, head.code)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("code {} not found", head.code))?;
    let action_set = plane
        .catalog()
        .get_action_set(&run.tenant, head.action_set)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("action set {} not found", head.action_set))?;
    plane
        .archive()
        .put(&crate::ports::SealedAttempt {
            run: run.clone(),
            attempt: head.attempt,
            code: code.code,
            action_set: action_set.action_set,
            entries,
        })
        .await
        .map_err(|error| error.to_string())?;
    let tenant = plane
        .catalog()
        .get_tenant(&run.tenant)
        .await
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| crate::ports::Tenant::new(run.tenant.clone()));
    Ok(tenant.retention.runs)
}

/// Drops what the control plane keeps of an erased namespace: its tests, its secrets and their
/// reads, and the namespace.
async fn drop_namespace<P: Plane>(
    plane: &P,
    tenant: &TenantId,
    name: &NamespaceName,
) -> Result<(), String> {
    use crate::ports::SecretStore;
    let catalog = plane.catalog();
    loop {
        let tests = catalog
            .namespace_tests(tenant, name, 100)
            .await
            .map_err(|error| error.to_string())?;
        if tests.is_empty() {
            break;
        }
        let owner = crate::ports::BlobOwner::Tests(tenant.clone());
        for digest in &tests {
            plane
                .blobs()
                .delete(&owner, *digest)
                .await
                .map_err(|error| error.to_string())?;
        }
        let rows: Vec<(TenantId, Digest)> = tests
            .into_iter()
            .map(|digest| (tenant.clone(), digest))
            .collect();
        catalog
            .drop_tests(&rows, true)
            .await
            .map_err(|error| error.to_string())?;
    }
    plane
        .secrets()
        .delete_namespace(tenant, name)
        .await
        .map_err(|error| error.to_string())?;
    catalog
        .drop_namespace(tenant, name)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}
