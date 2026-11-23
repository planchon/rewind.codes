//! The committer of a partition (`archi/06-storage-and-scale.md`, "Partitions, owners and
//! nodes": batches, messages after facts).
//!
//! Every message the owner handles leaves a [`Unit`]: the changes it makes, and what follows
//! from them once they are durable. The committer writes, as one batch, every unit that reached
//! it while the previous batch was being written: group commit, with no timer. Once the batch is
//! durable, the fan-out runs what follows from each unit, in order.
//!
//! A batch refused as fenced drops the partition at once. A batch refused for a conflict (a
//! change a single writer may get wrong) is committed again unit by unit, so that only the unit
//! at fault is refused: its requests are answered `unavailable`, and the runs it touched are
//! read again. A store that cannot be reached is tried again after a backoff.

use std::collections::{BTreeMap, BTreeSet};

use std::time::Duration;

use rewind_core::RunId;

use crate::executors::ExecMsg;
use crate::node::NodeMsg;
use crate::node::message::{Effect, Message, ReplyTo, job};
use crate::org::OrgMsg;
use crate::partition::{Entry, Internal, Owner, OwnerMsg, Phase};
use crate::plane::Plane;
use crate::ports::link::{Changed, NodeReply, OrgReply, PartitionReply, Refusal};
use crate::ports::partition::{Batch, Change};
use crate::ports::{
    NamespaceName, OutboxChange, OutboxEntry, PartitionError, PartitionLog, RunRef, TaskRef,
    TenantId,
};

/// The changes of one message, and what follows once they are durable.
pub(crate) struct Unit<P: Plane> {
    pub(crate) changes: Vec<Change>,
    pub(crate) after: Vec<After<P>>,
}

/// What follows a durable unit.
#[allow(
    clippy::large_enum_variant,
    reason = "a unit's follow-ups are moved once, after its batch: their size does not matter"
)]
pub(crate) enum After<P: Plane> {
    Effect(Effect<P>),
    /// The reply to a request.
    Reply(ReplyTo, PartitionReply),
    /// The run is queued if it is runnable.
    Queue(RunRef),
    /// The run's activation is committed: it may be queued again.
    Idle(RunRef),
    /// The run's activation failed: it is no longer busy, and waits for its timer.
    Rest(RunRef),
    /// The task is sent on its way if it is ready.
    Dispatch(TaskRef),
    /// The ended attempt goes to the sealer.
    Seal(RunRef),
    /// An outbox entry for its projection.
    Project(OutboxEntry),
    /// The counts of a namespace go to its organization actor; these admitted runs are
    /// included, these were abandoned.
    Report {
        tenant: TenantId,
        namespace: NamespaceName,
        included: Vec<RunId>,
        abandoned: Vec<RunId>,
    },
    /// The executors keep the run's instance at this history length.
    Confirm(RunRef, u64),
    /// The executors drop the run's instance.
    Forget(RunRef),
    /// Another message for the owner.
    Owner(OwnerMsg<P>),
    /// The run metrics of an activation's entries.
    Measure(Vec<crate::metrics::RunMeasurement>),
}

impl<P: Plane> Unit<P> {
    pub(crate) fn new() -> Self {
        Self {
            changes: Vec::new(),
            after: Vec::new(),
        }
    }

    pub(crate) fn change(&mut self, change: impl Into<Change>) {
        self.changes.push(change.into());
    }

    pub(crate) fn then(&mut self, after: After<P>) {
        self.after.push(after);
    }

    /// Adds an outbox entry, consumed by its projection once durable.
    pub(crate) fn project(&mut self, entry: OutboxEntry) {
        self.changes
            .push(Change::Outbox(OutboxChange::Add(Box::new(entry.clone()))));
        self.after.push(After::Project(entry));
    }

    /// Who waits for a reply of this unit.
    pub(crate) fn replies(&self) -> Vec<ReplyTo> {
        self.after
            .iter()
            .filter_map(|after| match after {
                After::Reply(to, _) | After::Effect(Effect::Reply(to, _)) => Some(to.clone()),
                _ => None,
            })
            .collect()
    }

    /// The runs the unit changes.
    fn runs(&self) -> BTreeSet<RunId> {
        self.changes
            .iter()
            .filter_map(changed_run)
            .map(|run| run.id)
            .collect()
    }
}

/// The run a change is about.
fn changed_run(change: &Change) -> Option<&RunRef> {
    match change {
        Change::CreateRun(create) => Some(&create.run),
        Change::Enqueue { run, .. }
        | Change::SignalKey { run, .. }
        | Change::Task { run, .. }
        | Change::Failure { run, .. }
        | Change::Interrupted { run }
        | Change::Quarantine { run, .. }
        | Change::Release { run }
        | Change::Watch { run, .. }
        | Change::Seal { run, .. }
        | Change::Erase { run }
        | Change::Summary { run, .. }
        | Change::WebhookToken { run, .. } => Some(run),
        Change::Activation(activation) => Some(&activation.run),
        Change::Timer(timer) => match timer {
            crate::ports::TimerChange::Set(timer) => timer.kind.run(),
            crate::ports::TimerChange::Cancel(kind) => kind.run(),
        },
        _ => None,
    }
}

/// A refusal in the shape of the reply it replaces.
fn refused(reply: &PartitionReply, refusal: Refusal) -> PartitionReply {
    match reply {
        PartitionReply::Started(_) => PartitionReply::Started(Err(refusal)),
        PartitionReply::Enqueued(_) => PartitionReply::Enqueued(Err(refusal)),
        PartitionReply::Done(_) => PartitionReply::Done(Err(refusal)),
        PartitionReply::Answered(_) => PartitionReply::Answered(Err(refusal)),
        PartitionReply::Run(_) => PartitionReply::Run(Err(refusal)),
        PartitionReply::History(_) => PartitionReply::History(Err(refusal)),
        PartitionReply::Tasks(_) => PartitionReply::Tasks(Err(refusal)),
        PartitionReply::Found(_) => PartitionReply::Found(Err(refusal)),
        PartitionReply::Counted(_) => PartitionReply::Counted(Err(refusal)),
        PartitionReply::Shapes(_) => PartitionReply::Shapes(Err(refusal)),
        PartitionReply::Used(_) => PartitionReply::Used(Err(refusal)),
        PartitionReply::Alerts(_) => PartitionReply::Alerts(Err(refusal)),
    }
}

impl<P: Plane> Owner<P> {
    /// Adds a unit to the next batch.
    pub(crate) fn push(&mut self, unit: Unit<P>) {
        if unit.changes.is_empty() {
            // Nothing to write: what follows follows now.
            for after in unit.after {
                self.after(after);
            }
            return;
        }
        self.pending.push(unit);
    }

    /// Writes the units pending as one batch, unless a batch is being written.
    pub(crate) fn try_commit(&mut self) {
        if self.committing.is_some() || self.phase != Phase::Open {
            return;
        }
        let units: Vec<Unit<P>> = match self.isolating.pop_front() {
            Some(unit) => vec![unit],
            None => std::mem::take(&mut self.pending),
        };
        if units.is_empty() {
            return;
        }
        let Some(log) = self.log.clone() else {
            return;
        };
        let batch = Batch {
            changes: units
                .iter()
                .flat_map(|unit| unit.changes.iter().cloned())
                .collect(),
        };
        self.committing = Some(units);
        let partition = self.partition;
        self.out.push(job(async move {
            let result = log.commit(batch).await;
            Message::Owner(partition, OwnerMsg::Durable(result))
        }));
    }

    /// The batch being written is durable, or was refused.
    pub(crate) fn durable(&mut self, result: Result<(), PartitionError>) {
        let Some(units) = self.committing.take() else {
            return;
        };
        match result {
            Ok(()) => {
                let mut reports: BTreeMap<(TenantId, NamespaceName), (Vec<RunId>, Vec<RunId>)> =
                    BTreeMap::new();
                for unit in units {
                    self.wheel.committed(&unit.changes);
                    self.publish(&unit.changes);
                    for after in unit.after {
                        match after {
                            After::Report {
                                tenant,
                                namespace,
                                included,
                                abandoned,
                            } => {
                                let report = reports.entry((tenant, namespace)).or_default();
                                report.0.extend(included);
                                report.1.extend(abandoned);
                            }
                            other => self.after(other),
                        }
                    }
                }
                for ((tenant, namespace), (included, abandoned)) in reports {
                    self.report(tenant, namespace, included, abandoned);
                }
            }
            Err(PartitionError::Fenced { .. }) => {
                tracing::warn!(partition = %self.partition, "the partition was opened by another node: dropped");
                let mut units = units;
                units.extend(std::mem::take(&mut self.pending));
                for unit in &units {
                    for reply in unit.replies() {
                        self.out.push(Effect::Reply(
                            reply,
                            NodeReply::NotOwner {
                                partition: self.partition,
                            },
                        ));
                    }
                }
                self.close();
                self.out.push(Effect::Local(Message::Node(NodeMsg::Fenced(
                    self.partition,
                ))));
            }
            Err(PartitionError::Conflict(message)) if units.len() > 1 => {
                tracing::warn!(partition = %self.partition, %message, "a batch was refused: its units are committed one by one");
                for unit in units.into_iter().rev() {
                    self.isolating.push_front(unit);
                }
            }
            Err(PartitionError::Conflict(message)) => {
                tracing::error!(partition = %self.partition, %message, "a change was refused");
                for unit in units {
                    self.refuse(
                        unit,
                        Refusal::Unavailable(format!(
                            "the partition refused the change: {message}"
                        )),
                    );
                }
            }
            Err(error) => {
                tracing::error!(partition = %self.partition, %error, "a batch could not be written: tried again");
                for unit in units.into_iter().rev() {
                    self.isolating.push_front(unit);
                }
                let at = self.now().saturating_add(Duration::from_secs(1));
                // The commit waits for the wake: a batch in flight holds the committer.
                self.committing = Some(Vec::new());
                self.out.push(Effect::Wake {
                    at,
                    alone: true,
                    message: Message::Owner(self.partition, OwnerMsg::RetryCommit),
                });
            }
        }
    }

    /// A unit refused for good: its requests are answered `refusal`, and the runs it touched
    /// are read again.
    fn refuse(&mut self, unit: Unit<P>, refusal: Refusal) {
        for run in unit.runs() {
            if matches!(self.runs.get(&run), Some(Entry::Present(_))) {
                self.runs.remove(&run);
            }
        }
        for after in unit.after {
            match after {
                After::Reply(to, reply) => {
                    let reply = refused(&reply, refusal.clone());
                    self.reply(to, reply);
                }
                After::Effect(Effect::Reply(to, NodeReply::Organization(_))) => {
                    self.out.push(Effect::Reply(
                        to,
                        NodeReply::Organization(OrgReply::Admission(Err(refusal.clone()))),
                    ));
                }
                After::Effect(Effect::Reply(to, NodeReply::Partition(reply))) => {
                    let reply = refused(&reply, refusal.clone());
                    self.reply(to, reply);
                }
                After::Report {
                    tenant,
                    namespace,
                    included,
                    ..
                } => self.report(tenant, namespace, Vec::new(), included),
                After::Queue(run) | After::Idle(run) => {
                    self.dispatch(OwnerMsg::Internal(Internal::Queue(run)));
                }
                _ => {}
            }
        }
    }

    /// What follows a durable unit.
    pub(crate) fn after(&mut self, after: After<P>) {
        match after {
            After::Effect(effect) => self.out.push(effect),
            After::Reply(to, reply) => self.reply(to, reply),
            After::Queue(run) => self.dispatch(OwnerMsg::Internal(Internal::Queue(run))),
            After::Idle(run) => {
                if let Some(slot) = self.slot_mut(run.id) {
                    slot.busy = None;
                    let waiting = std::mem::take(&mut slot.waiting);
                    for message in waiting {
                        self.dispatch(message);
                    }
                }
                self.dispatch(OwnerMsg::Internal(Internal::Queue(run)));
            }
            After::Rest(run) => {
                if let Some(slot) = self.slot_mut(run.id) {
                    slot.busy = None;
                    let waiting = std::mem::take(&mut slot.waiting);
                    for message in waiting {
                        self.dispatch(message);
                    }
                }
            }
            After::Dispatch(task) => self.dispatch(OwnerMsg::Internal(Internal::Dispatch(task))),
            After::Seal(run) => self.dispatch(OwnerMsg::Internal(Internal::Seal(run))),
            After::Project(entry) => self.project(entry),
            After::Report {
                tenant,
                namespace,
                included,
                abandoned,
            } => self.report(tenant, namespace, included, abandoned),
            After::Confirm(run, history_len) => {
                self.out
                    .push(Effect::Local(Message::Executors(ExecMsg::Durable {
                        run,
                        history_len,
                    })))
            }
            After::Forget(run) => self
                .out
                .push(Effect::Local(Message::Executors(ExecMsg::Forget { run }))),
            After::Owner(message) => self.dispatch(message),
            After::Measure(measurements) => {
                for measurement in &measurements {
                    measurement.emit();
                }
            }
        }
    }

    /// Tells the subscribed edges what a durable batch changed.
    fn publish(&mut self, changes: &[Change]) {
        let mut changed = Vec::new();
        for change in changes {
            match change {
                Change::CreateRun(create) => changed.push(Changed {
                    run: create.run.clone(),
                    namespace: create.head.namespace.clone(),
                    summary: Some(create.summary.summary.clone()),
                    entries: create.entries.clone(),
                }),
                Change::Activation(activation) => changed.push(Changed {
                    run: activation.run.clone(),
                    namespace: activation.head.namespace.clone(),
                    summary: Some(activation.summary.summary.clone()),
                    entries: activation.entries.clone(),
                }),
                Change::Summary { run, summary } => changed.push(Changed {
                    run: run.clone(),
                    namespace: summary.summary.namespace.clone(),
                    summary: Some(summary.summary.clone()),
                    entries: Vec::new(),
                }),
                Change::Erase { run } => {
                    if let Some(slot) = self.slot(run.id) {
                        changed.push(Changed {
                            run: run.clone(),
                            namespace: slot.head.namespace.clone(),
                            summary: None,
                            entries: Vec::new(),
                        });
                    }
                }
                _ => {}
            }
        }
        if !changed.is_empty() {
            self.out
                .push(Effect::Local(Message::Node(NodeMsg::Publish(changed))));
        }
    }

    /// Sends the counts of a namespace to its organization actor, as absolute values.
    pub(crate) fn report(
        &mut self,
        tenant: TenantId,
        namespace: NamespaceName,
        included: Vec<RunId>,
        abandoned: Vec<RunId>,
    ) {
        self.counted.insert((tenant.clone(), namespace.clone()));
        let Some(log) = self.log.clone() else {
            return;
        };
        let partition = self.partition;
        let now = self.now();
        self.out.push(job(async move {
            let scope = crate::ports::Scope::Namespace {
                tenant: tenant.clone(),
                namespace: namespace.clone(),
            };
            let month = crate::ports::Window::of_month(crate::ports::Month::of(now));
            let result = async {
                let gauges = log.gauges(&scope).await?;
                let counters = log.counters(&scope, month).await?;
                Ok::<_, PartitionError>((gauges, counters))
            }
            .await
            .map_err(|error| error.to_string());
            Message::Owner(
                partition,
                OwnerMsg::Step(Box::new(super::runs::Step::Counted {
                    tenant,
                    namespace,
                    included,
                    abandoned,
                    result,
                })),
            )
        }));
    }

    /// The counts a report read, sent to the organization actor.
    pub(crate) fn counted(
        &mut self,
        tenant: TenantId,
        namespace: NamespaceName,
        included: Vec<RunId>,
        abandoned: Vec<RunId>,
        result: Result<(crate::ports::Gauges, crate::ports::Counters), String>,
    ) {
        let (gauges, counters) = match result {
            Ok(read) => read,
            Err(error) => {
                tracing::warn!(%tenant, %namespace, %error, "the counts could not be read: the next report corrects them");
                return;
            }
        };
        let report = crate::ports::link::CountsReport {
            partition: self.partition,
            epoch: self.epoch,
            namespace,
            gauges,
            created_this_month: counters.created,
            cpu_this_month: counters.cpu,
            included,
            abandoned,
            at: self.now(),
        };
        let request = crate::ports::link::NodeRequest::Organization {
            tenant: tenant.clone(),
            request: Box::new(crate::ports::link::OrgRequest::Counts(report)),
        };
        if crate::ports::home_partition(&tenant, self.config.partitions) == self.partition {
            if let crate::ports::link::NodeRequest::Organization { request, .. } = request {
                self.dispatch(OwnerMsg::Org {
                    tenant,
                    message: OrgMsg::Request(request, ReplyTo::Nobody),
                });
            }
        } else {
            self.tell(crate::node::message::Target::Home(tenant), request);
        }
    }

    /// The commit that waited for its backoff may go.
    pub(crate) fn retry_commit(&mut self) {
        if self.committing.as_ref().is_some_and(Vec::is_empty) {
            self.committing = None;
        }
    }
}
