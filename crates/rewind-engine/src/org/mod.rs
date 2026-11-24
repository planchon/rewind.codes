//! The organization actor (`archi/06-storage-and-scale.md`, "Quotas", "Partitions, owners and
//! nodes"; `archi/04-tools.md`, "Call lifecycle", "Policies").
//!
//! One per tenant, on the owner of the tenant's home partition, which hosts it: its writes are
//! that owner's batches. It holds:
//!
//! - **Admission.** The absolute counts each owner reports for each namespace, and the starts it
//!   admitted that no report includes yet. `Admit` passes the namespace's limit and the tenant's
//!   ceiling for `activeRuns`, `runsPerMonth` and `scripts`, or is refused with the quota; an
//!   admission is written before it is answered, and released once a report includes its run.
//! - **Alerts.** A threshold crossed is recorded once per namespace (or the tenant), quota,
//!   month and threshold, with an outbox entry that sends `quota.alert`.
//! - **Slots.** The calls in flight of each MCP server or API across the organization, granted
//!   in the order they are asked for.
//! - **Host tasks.** The SDK streams attached at the edges, and the host tasks they hold. A
//!   stream that closes keeps its tasks for their grace; its worker opening a stream again takes
//!   them back, otherwise their owners interrupt them.
//!
//! Everything but the admissions and the alerts lives in memory: owners send their slots and
//! host tasks again when the actor moves, and the edges their streams.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use rewind_core::{PartitionId, Timestamp};

use crate::node::message::{Effect, Message, ReplyTo, Target, job};
use crate::partition::OwnerMsg;
use crate::plane::Plane;
use crate::ports::link::{
    Admit, CountsReport, EdgeNotice, HostTaskOffer, NodeReply, NodeRequest, OrgReply, OrgRequest,
    OwnerNotice, Refusal, SlotRequest, StreamAttach, StreamId,
};
use crate::ports::{
    Admission, CatalogStore, Gauges, Month, NamespaceName, OrganizationState, PartitionEpoch,
    QuotaAlert, QuotaAlertKey, QuotaExceeded, QuotaKind, QuotaScope, Quotas, TaskRef, TaskToken,
    Tenant, TenantId, Threshold, namespace_partition,
};

/// A message to an organization actor.
#[allow(
    clippy::large_enum_variant,
    reason = "a message is moved once per step: its size does not matter"
)]
pub(crate) enum OrgMsg {
    Request(Box<OrgRequest>, ReplyTo),
    /// The tenant's settings and a namespace's limits, read from the control plane.
    Limits {
        namespace: NamespaceName,
        read: Result<(Tenant, Quotas), String>,
    },
    /// The grace of a closed stream is over.
    Grace {
        stream: StreamId,
    },
    /// A change of the control plane dropped what the actor cached of the tenant or a
    /// namespace.
    Forget {
        namespace: Option<NamespaceName>,
    },
    /// The nodes of the cluster: a stream held at a node that left is closed, as its edge would
    /// have said had it lived; its tasks get their grace.
    Members(Vec<crate::ports::NodeId>),
}

impl fmt::Debug for OrgMsg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OrgMsg::Request(request, reply) => write!(f, "Request({request:?}, {reply:?})"),
            OrgMsg::Limits { namespace, .. } => write!(f, "Limits({namespace})"),
            OrgMsg::Grace { stream } => write!(f, "Grace({stream:?})"),
            OrgMsg::Forget { namespace } => write!(f, "Forget({namespace:?})"),
            OrgMsg::Members(nodes) => write!(f, "Members({})", nodes.len()),
        }
    }
}

/// What an organization actor asks its host for.
#[allow(
    clippy::large_enum_variant,
    reason = "an effect is moved once per step: its size does not matter"
)]
pub(crate) enum OrgEffect<P: Plane> {
    /// Write the actor's state in its home partition, with an outbox entry per alert; then
    /// these effects.
    Persist {
        state: OrganizationState,
        alerts: Vec<QuotaAlert>,
        then: Vec<Effect<P>>,
    },
    Effect(Effect<P>),
}

/// What an owner reported of a namespace.
#[derive(Debug, Clone, Default)]
struct Reported {
    gauges: Gauges,
    created_this_month: u64,
    month: Option<Month>,
    cpu_this_month: Duration,
}

/// The limits that apply to a tenant's namespaces.
#[derive(Debug, Clone)]
struct Limits {
    tenant: Tenant,
    namespaces: BTreeMap<NamespaceName, Quotas>,
}

/// The calls in flight of one server, and those waiting for a slot.
#[derive(Debug, Default)]
struct Server {
    held: BTreeMap<TaskRef, (PartitionId, PartitionEpoch)>,
    queue: VecDeque<SlotRequest>,
}

/// An SDK stream attached at an edge.
#[derive(Debug)]
struct Stream {
    attach: StreamAttach,
    /// The tasks handed on it and not answered.
    held: BTreeSet<TaskRef>,
    /// `false` once it closed: it waits for its worker for the grace of its tasks.
    open: bool,
}

/// The organization actor of a tenant.
pub(crate) struct OrgActor<P: Plane> {
    tenant: TenantId,
    home: PartitionId,
    partitions: NonZeroU16,
    plane: Arc<P>,
    state: OrganizationState,
    /// The epoch of each admission in flight, by run: an admission of an owner that is gone is
    /// released when the next owner of its partition reports.
    epochs: BTreeMap<rewind_core::RunId, (PartitionId, PartitionEpoch)>,
    reported: BTreeMap<NamespaceName, Reported>,
    owners: BTreeMap<PartitionId, PartitionEpoch>,
    limits: Option<Limits>,
    /// The admissions waiting for limits to be read, by namespace.
    waiting: BTreeMap<NamespaceName, Vec<(Admit, ReplyTo)>>,
    servers: BTreeMap<String, Server>,
    offers: VecDeque<HostTaskOffer>,
    handed: BTreeMap<TaskRef, (StreamId, HostTaskOffer)>,
    streams: BTreeMap<StreamId, Stream>,
}

impl<P: Plane> fmt::Debug for OrgActor<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrgActor")
            .field("tenant", &self.tenant)
            .field("home", &self.home)
            .field("admissions", &self.state.admissions.len())
            .field("streams", &self.streams.len())
            .finish_non_exhaustive()
    }
}

impl<P: Plane> OrgActor<P> {
    /// The actor of `tenant`, with the state its home partition kept. A new actor asks every
    /// owner for the counts of the tenant once.
    pub(crate) fn new(
        tenant: TenantId,
        home: PartitionId,
        partitions: NonZeroU16,
        plane: Arc<P>,
        state: OrganizationState,
    ) -> (Self, Vec<OrgEffect<P>>) {
        let actor = Self {
            tenant: tenant.clone(),
            home,
            partitions,
            plane,
            state,
            epochs: BTreeMap::new(),
            reported: BTreeMap::new(),
            owners: BTreeMap::new(),
            limits: None,
            waiting: BTreeMap::new(),
            servers: BTreeMap::new(),
            offers: VecDeque::new(),
            handed: BTreeMap::new(),
            streams: BTreeMap::new(),
        };
        let asks = (0..partitions.get())
            .map(|partition| {
                OrgEffect::Effect(Effect::Request {
                    to: Target::Partition(PartitionId::new(partition)),
                    request: NodeRequest::Owner {
                        partition: PartitionId::new(partition),
                        notice: Box::new(OwnerNotice::CountsWanted {
                            tenant: tenant.clone(),
                        }),
                    },
                    asker: None,
                })
            })
            .collect();
        (actor, asks)
    }

    pub(crate) fn handle(&mut self, message: OrgMsg, now: Timestamp) -> Vec<OrgEffect<P>> {
        let mut out = Vec::new();
        match message {
            OrgMsg::Request(request, reply) => self.request(*request, reply, now, &mut out),
            OrgMsg::Limits { namespace, read } => {
                let waiting = self.waiting.remove(&namespace).unwrap_or_default();
                match read {
                    Ok((tenant, quotas)) => {
                        let limits = self.limits.get_or_insert_with(|| Limits {
                            tenant: tenant.clone(),
                            namespaces: BTreeMap::new(),
                        });
                        limits.tenant = tenant;
                        limits.namespaces.insert(namespace, quotas);
                        for (admit, reply) in waiting {
                            self.admit(admit, reply, now, &mut out);
                        }
                    }
                    Err(error) => {
                        for (_, reply) in waiting {
                            out.push(reply_admission(
                                reply,
                                Err(Refusal::Unavailable(format!(
                                    "the organization's limits could not be read: {error}"
                                ))),
                            ));
                        }
                    }
                }
            }
            OrgMsg::Grace { stream } => self.grace_over(&stream, &mut out),
            OrgMsg::Forget { namespace } => match (namespace, &mut self.limits) {
                (Some(namespace), Some(limits)) => {
                    limits.namespaces.remove(&namespace);
                }
                (None, _) => self.limits = None,
                (Some(_), None) => {}
            },
            OrgMsg::Members(nodes) => {
                let gone: Vec<StreamId> = self
                    .streams
                    .iter()
                    .filter(|(_, stream)| stream.open && !nodes.contains(&stream.attach.node))
                    .map(|(id, _)| id.clone())
                    .collect();
                for stream in gone {
                    self.detach(&stream, now, &mut out);
                }
            }
        }
        out
    }

    fn request(
        &mut self,
        request: OrgRequest,
        reply: ReplyTo,
        now: Timestamp,
        out: &mut Vec<OrgEffect<P>>,
    ) {
        let done = |out: &mut Vec<OrgEffect<P>>, reply: ReplyTo| {
            out.push(OrgEffect::Effect(Effect::Reply(reply, NodeReply::Done)));
        };
        match request {
            OrgRequest::Admit(admit) => self.admit(admit, reply, now, out),
            OrgRequest::Counts(report) => {
                done(out, reply);
                self.counts(report, now, out);
            }
            OrgRequest::Slot(slot) => {
                done(out, reply);
                self.slot(slot, out);
            }
            OrgRequest::SlotFreed { task, server } => {
                done(out, reply);
                self.free(&server, &task, out);
            }
            OrgRequest::HostTask(offer) => {
                done(out, reply);
                let task = offer.task.task.clone();
                // An owner that sends a task again (a new owner, a new attempt) replaces it: the
                // token it was handed with no longer answers it.
                self.offers.retain(|queued| queued.task.task != task);
                self.release(&task, true, out);
                self.offers.push_back(*offer);
                self.hand_out(out);
            }
            OrgRequest::HostTaskDone { task, gone } => {
                done(out, reply);
                self.offers.retain(|queued| queued.task.task != task);
                self.release(&task, gone, out);
                self.hand_out(out);
            }
            OrgRequest::Attach(attach) => {
                done(out, reply);
                self.attach(attach);
                self.hand_out(out);
            }
            OrgRequest::Detach { stream } => {
                done(out, reply);
                self.detach(&stream, now, out);
            }
        }
    }

    // ---------------------------------------------------------------------------------------
    // Admission and alerts

    fn admit(&mut self, admit: Admit, reply: ReplyTo, now: Timestamp, out: &mut Vec<OrgEffect<P>>) {
        let loaded = self
            .limits
            .as_ref()
            .is_some_and(|limits| limits.namespaces.contains_key(&admit.namespace));
        if !loaded {
            let namespace = admit.namespace.clone();
            let first = !self.waiting.contains_key(&namespace);
            self.waiting
                .entry(namespace.clone())
                .or_default()
                .push((admit, reply));
            if first {
                out.push(OrgEffect::Effect(self.read_limits(namespace)));
            }
            return;
        }
        let Some(limits) = &self.limits else {
            return;
        };
        let namespace_limits = limits
            .namespaces
            .get(&admit.namespace)
            .cloned()
            .unwrap_or_default();
        let ceiling = limits.tenant.quotas.clone();
        let month = Month::of(admit.at);
        let in_flight = &self.state.admissions;
        let of_namespace = |admission: &&Admission| admission.namespace == admit.namespace;
        let reported = self.reported.get(&admit.namespace);
        let new_script = !reported
            .is_some_and(|counts| counts.gauges.scripts.contains_key(&admit.script))
            && !in_flight.iter().any(|admission| {
                admission.namespace == admit.namespace && admission.script == admit.script
            });
        let active_namespace = reported.map_or(0, |counts| counts.gauges.active_runs)
            + in_flight.iter().filter(of_namespace).count() as u64;
        let active_tenant = self
            .reported
            .values()
            .map(|counts| counts.gauges.active_runs)
            .sum::<u64>()
            + in_flight.len() as u64;
        let month_of = |counts: &Reported| {
            if counts.month == Some(month) {
                counts.created_this_month
            } else {
                0
            }
        };
        let runs_namespace =
            reported.map_or(0, month_of) + in_flight.iter().filter(of_namespace).count() as u64;
        let runs_tenant =
            self.reported.values().map(month_of).sum::<u64>() + in_flight.len() as u64;
        let scripts_namespace = reported.map_or(0, |counts| counts.gauges.distinct_scripts())
            + distinct_new_scripts(in_flight, Some(&admit.namespace), &self.reported);
        let mut tenant_scripts: BTreeSet<rewind_core::Digest> = BTreeSet::new();
        for counts in self.reported.values() {
            tenant_scripts.extend(counts.gauges.scripts.keys().copied());
        }
        for admission in in_flight {
            tenant_scripts.insert(admission.script);
        }
        let scripts_tenant = tenant_scripts.len() as u64;
        let checks = [
            (
                QuotaKind::ActiveRuns,
                QuotaScope::Namespace,
                namespace_limits.active_runs.map(u64::from),
                active_namespace,
            ),
            (
                QuotaKind::ActiveRuns,
                QuotaScope::Tenant,
                ceiling.active_runs.map(u64::from),
                active_tenant,
            ),
            (
                QuotaKind::RunsPerMonth,
                QuotaScope::Namespace,
                namespace_limits.runs_per_month,
                runs_namespace,
            ),
            (
                QuotaKind::RunsPerMonth,
                QuotaScope::Tenant,
                ceiling.runs_per_month,
                runs_tenant,
            ),
            (
                QuotaKind::Scripts,
                QuotaScope::Namespace,
                namespace_limits.scripts.filter(|_| new_script),
                scripts_namespace,
            ),
            (
                QuotaKind::Scripts,
                QuotaScope::Tenant,
                ceiling.scripts.filter(|_| new_script),
                scripts_tenant,
            ),
        ];
        for (quota, scope, limit, used) in checks {
            let Some(limit) = limit else { continue };
            if used < limit {
                continue;
            }
            let exceeded = QuotaExceeded {
                quota,
                scope,
                limit,
                used,
                resets_at: (quota == QuotaKind::RunsPerMonth).then(|| month.end()),
            };
            let mut alerts = Vec::new();
            if quota == QuotaKind::ActiveRuns {
                let key = QuotaAlertKey {
                    tenant: self.tenant.clone(),
                    namespace: (scope == QuotaScope::Namespace).then(|| admit.namespace.clone()),
                    quota,
                    period: month,
                    threshold: Threshold::Reached,
                };
                alerts.extend(self.raise(key, used, limit, now));
            }
            let refusal = reply_admission(reply, Err(Refusal::Quota(exceeded)));
            if alerts.is_empty() {
                out.push(refusal);
            } else {
                out.push(OrgEffect::Persist {
                    state: self.state.clone(),
                    alerts,
                    then: vec![unwrap_effect(refusal)],
                });
            }
            return;
        }
        let mut alerts = Vec::new();
        let crossed = [
            (
                Some(admit.namespace.clone()),
                QuotaKind::RunsPerMonth,
                runs_namespace,
                namespace_limits.runs_per_month,
            ),
            (
                None,
                QuotaKind::RunsPerMonth,
                runs_tenant,
                ceiling.runs_per_month,
            ),
        ];
        for (namespace, quota, used, limit) in crossed {
            alerts.extend(self.crossed(namespace, quota, month, used, used + 1, limit, now));
        }
        if new_script {
            let crossed = [
                (
                    Some(admit.namespace.clone()),
                    scripts_namespace,
                    namespace_limits.scripts,
                ),
                (None, scripts_tenant, ceiling.scripts),
            ];
            for (namespace, used, limit) in crossed {
                alerts.extend(self.crossed(
                    namespace,
                    QuotaKind::Scripts,
                    month,
                    used,
                    used + 1,
                    limit,
                    now,
                ));
            }
        }
        self.state.admissions.push(Admission {
            run: admit.run,
            namespace: admit.namespace.clone(),
            script: admit.script,
            admitted_at: admit.at,
        });
        self.epochs
            .insert(admit.run, (admit.partition, admit.epoch));
        out.push(OrgEffect::Persist {
            state: self.state.clone(),
            alerts,
            then: vec![unwrap_effect(reply_admission(reply, Ok(())))],
        });
    }

    fn read_limits(&self, namespace: NamespaceName) -> Effect<P> {
        let plane = Arc::clone(&self.plane);
        let tenant = self.tenant.clone();
        let home = self.home;
        job(async move {
            let catalog = plane.catalog();
            let read = async {
                let record = catalog
                    .get_tenant(&tenant)
                    .await
                    .map_err(|error| error.to_string())?
                    .unwrap_or_else(|| Tenant::new(tenant.clone()));
                let found = catalog
                    .get_namespace(&tenant, &namespace)
                    .await
                    .map_err(|error| error.to_string())?;
                let quotas = crate::ports::limits(&record, found.as_ref());
                Ok::<_, String>((record, quotas))
            }
            .await;
            Message::Owner(
                home,
                OwnerMsg::Org {
                    tenant,
                    message: OrgMsg::Limits { namespace, read },
                },
            )
        })
    }

    fn counts(&mut self, report: CountsReport, now: Timestamp, out: &mut Vec<OrgEffect<P>>) {
        let known = self.owners.get(&report.partition).copied();
        if known.is_some_and(|known| known > report.epoch) {
            return;
        }
        if known.is_none_or(|known| known < report.epoch) {
            self.owners.insert(report.partition, report.epoch);
            self.release_owner(report.partition, report.epoch, out);
        }
        let mut changed = false;
        let released: BTreeSet<_> = report
            .included
            .iter()
            .chain(&report.abandoned)
            .copied()
            .collect();
        if !released.is_empty() {
            let before = self.state.admissions.len();
            self.state
                .admissions
                .retain(|admission| !released.contains(&admission.run));
            for run in &released {
                self.epochs.remove(run);
            }
            changed |= before != self.state.admissions.len();
        }
        let month = Month::of(report.at);
        let previous = self
            .reported
            .get(&report.namespace)
            .cloned()
            .unwrap_or_default();
        let previous_cpu = if previous.month == Some(month) {
            previous.cpu_this_month
        } else {
            Duration::ZERO
        };
        self.reported.insert(
            report.namespace.clone(),
            Reported {
                gauges: report.gauges,
                created_this_month: report.created_this_month,
                month: Some(month),
                cpu_this_month: report.cpu_this_month,
            },
        );
        let mut alerts = Vec::new();
        if let Some(limits) = &self.limits {
            let namespace_limit = limits
                .namespaces
                .get(&report.namespace)
                .and_then(|quotas| quotas.cpu_per_month);
            let tenant_limit = limits.tenant.quotas.cpu_per_month;
            let millis = |cpu: Duration| u64::try_from(cpu.as_millis()).unwrap_or(u64::MAX);
            let tenant_cpu: Duration = self
                .reported
                .values()
                .filter(|counts| counts.month == Some(month))
                .map(|counts| counts.cpu_this_month)
                .sum();
            let tenant_before =
                tenant_cpu.saturating_sub(report.cpu_this_month.saturating_sub(previous_cpu));
            alerts.extend(self.crossed(
                Some(report.namespace.clone()),
                QuotaKind::CpuPerMonth,
                month,
                millis(previous_cpu),
                millis(report.cpu_this_month),
                namespace_limit.map(millis),
                now,
            ));
            alerts.extend(self.crossed(
                None,
                QuotaKind::CpuPerMonth,
                month,
                millis(tenant_before),
                millis(tenant_cpu),
                tenant_limit.map(millis),
                now,
            ));
        }
        if changed || !alerts.is_empty() {
            out.push(OrgEffect::Persist {
                state: self.state.clone(),
                alerts,
                then: Vec::new(),
            });
        }
    }

    /// Releases what an earlier owner of a partition held: its admissions not reported, its
    /// slots, its host tasks.
    fn release_owner(
        &mut self,
        partition: PartitionId,
        epoch: PartitionEpoch,
        out: &mut Vec<OrgEffect<P>>,
    ) {
        let partitions = self.partitions;
        let tenant = self.tenant.clone();
        let stale: BTreeSet<_> = self
            .state
            .admissions
            .iter()
            .filter(|admission| {
                namespace_partition(&tenant, &admission.namespace, partitions) == partition
                    && self
                        .epochs
                        .get(&admission.run)
                        .is_none_or(|(_, at)| *at < epoch)
            })
            .map(|admission| admission.run)
            .collect();
        self.state
            .admissions
            .retain(|admission| !stale.contains(&admission.run));
        let mut freed = Vec::new();
        for (name, server) in &mut self.servers {
            server
                .held
                .retain(|_, (of, at)| !(*of == partition && *at < epoch));
            server
                .queue
                .retain(|slot| !(slot.partition == partition && slot.epoch < epoch));
            freed.push(name.clone());
        }
        for name in freed {
            self.grant(&name, out);
        }
        self.offers
            .retain(|offer| !(offer.partition == partition && offer.epoch < epoch));
        let gone: Vec<TaskRef> = self
            .handed
            .iter()
            .filter(|(_, (_, offer))| offer.partition == partition && offer.epoch < epoch)
            .map(|(task, _)| task.clone())
            .collect();
        for task in gone {
            self.release(&task, true, out);
        }
    }

    /// The alert of a threshold the use crossed going from `before` to `after`, once.
    #[allow(clippy::too_many_arguments)]
    fn crossed(
        &mut self,
        namespace: Option<NamespaceName>,
        quota: QuotaKind,
        month: Month,
        before: u64,
        after: u64,
        limit: Option<u64>,
        now: Timestamp,
    ) -> Vec<QuotaAlert> {
        let Some(limit) = limit.filter(|limit| *limit > 0) else {
            return Vec::new();
        };
        let mut alerts = Vec::new();
        for threshold in [Threshold::Near, Threshold::Reached] {
            let at = u128::from(limit) * u128::from(threshold.percent());
            let crossed = u128::from(before) * 100 < at && at <= u128::from(after) * 100;
            if !crossed {
                continue;
            }
            let key = QuotaAlertKey {
                tenant: self.tenant.clone(),
                namespace: namespace.clone(),
                quota,
                period: month,
                threshold,
            };
            alerts.extend(self.raise(key, after, limit, now));
        }
        alerts
    }

    /// Records an alert once.
    fn raise(
        &mut self,
        key: QuotaAlertKey,
        used: u64,
        limit: u64,
        now: Timestamp,
    ) -> Option<QuotaAlert> {
        if self.state.alerts.iter().any(|alert| alert.key == key) {
            return None;
        }
        let alert = QuotaAlert {
            key,
            used,
            limit,
            raised_at: now,
            sent_at: None,
        };
        self.state.alerts.push(alert.clone());
        Some(alert)
    }

    // ---------------------------------------------------------------------------------------
    // Slots

    fn slot(&mut self, slot: SlotRequest, out: &mut Vec<OrgEffect<P>>) {
        let server = self.servers.entry(slot.server.clone()).or_default();
        if let Some(held) = server.held.get_mut(&slot.task) {
            // An owner that asks again for a slot it holds lost the grant (a message lost, a new
            // owner of the partition): it is granted again.
            *held = (slot.partition, slot.epoch);
            out.push(OrgEffect::Effect(Effect::Request {
                to: Target::Partition(slot.partition),
                request: NodeRequest::Owner {
                    partition: slot.partition,
                    notice: Box::new(OwnerNotice::SlotGranted { task: slot.task }),
                },
                asker: None,
            }));
            return;
        }
        server.queue.retain(|queued| queued.task != slot.task);
        let name = slot.server.clone();
        server.queue.push_back(slot);
        self.grant(&name, out);
    }

    fn free(&mut self, server: &str, task: &TaskRef, out: &mut Vec<OrgEffect<P>>) {
        if let Some(entry) = self.servers.get_mut(server) {
            entry.held.remove(task);
            entry.queue.retain(|queued| queued.task != *task);
        }
        self.grant(server, out);
    }

    fn grant(&mut self, name: &str, out: &mut Vec<OrgEffect<P>>) {
        let Some(server) = self.servers.get_mut(name) else {
            return;
        };
        while let Some(next) = server.queue.front() {
            if server.held.len() >= next.limit.max(1) as usize {
                break;
            }
            let Some(next) = server.queue.pop_front() else {
                break;
            };
            server
                .held
                .insert(next.task.clone(), (next.partition, next.epoch));
            out.push(OrgEffect::Effect(Effect::Request {
                to: Target::Partition(next.partition),
                request: NodeRequest::Owner {
                    partition: next.partition,
                    notice: Box::new(OwnerNotice::SlotGranted { task: next.task }),
                },
                asker: None,
            }));
        }
        if server.held.is_empty() && server.queue.is_empty() {
            self.servers.remove(name);
        }
    }

    // ---------------------------------------------------------------------------------------
    // Host tasks and SDK streams

    fn attach(&mut self, attach: StreamAttach) {
        // The worker of a stream that closed comes back within its grace: it keeps its tasks.
        let returning: Vec<StreamId> = self
            .streams
            .iter()
            .filter(|(id, stream)| {
                !stream.open && stream.attach.worker == attach.worker && **id != attach.stream
            })
            .map(|(id, _)| id.clone())
            .collect();
        // A stream attached again keeps the tasks it holds.
        let mut held = self
            .streams
            .remove(&attach.stream)
            .map(|stream| stream.held)
            .unwrap_or_default();
        for id in returning {
            if let Some(stream) = self.streams.remove(&id) {
                for task in &stream.held {
                    if let Some((on, _)) = self.handed.get_mut(task) {
                        *on = attach.stream.clone();
                    }
                }
                held.extend(stream.held);
            }
        }
        self.streams.insert(
            attach.stream.clone(),
            Stream {
                attach,
                held,
                open: true,
            },
        );
    }

    fn detach(&mut self, id: &StreamId, now: Timestamp, out: &mut Vec<OrgEffect<P>>) {
        let Some(stream) = self.streams.get_mut(id) else {
            return;
        };
        if stream.held.is_empty() {
            self.streams.remove(id);
            return;
        }
        stream.open = false;
        let grace = stream
            .held
            .iter()
            .filter_map(|task| self.handed.get(task))
            .map(|(_, offer)| offer.grace)
            .max()
            .unwrap_or_default();
        out.push(OrgEffect::Effect(Effect::Wake {
            at: now.saturating_add(grace),
            alone: false,
            message: Message::Owner(
                self.home,
                OwnerMsg::Org {
                    tenant: self.tenant.clone(),
                    message: OrgMsg::Grace { stream: id.clone() },
                },
            ),
        }));
    }

    fn grace_over(&mut self, id: &StreamId, out: &mut Vec<OrgEffect<P>>) {
        let closed = self.streams.get(id).is_some_and(|stream| !stream.open);
        if !closed {
            return;
        }
        let Some(stream) = self.streams.remove(id) else {
            return;
        };
        let mut lost: BTreeMap<PartitionId, Vec<(TaskRef, TaskToken)>> = BTreeMap::new();
        for task in stream.held {
            if let Some((_, offer)) = self.handed.remove(&task) {
                lost.entry(offer.partition)
                    .or_default()
                    .push((task, offer.task.token.clone()));
            }
        }
        for (partition, tasks) in lost {
            out.push(OrgEffect::Effect(Effect::Request {
                to: Target::Partition(partition),
                request: NodeRequest::Owner {
                    partition,
                    notice: Box::new(OwnerNotice::StreamLost { tasks }),
                },
                asker: None,
            }));
        }
        self.hand_out(out);
    }

    /// Hands the queued tasks to the open streams that take them and have a free slot, in turn.
    /// A task handed on a stream leaves it: its slot is free. `gone`: its token no longer
    /// answers it, and the stream, if open, hears `gone`.
    fn release(&mut self, task: &TaskRef, gone: bool, out: &mut Vec<OrgEffect<P>>) {
        let Some((id, offer)) = self.handed.remove(task) else {
            return;
        };
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        stream.held.remove(task);
        if gone && stream.open {
            out.push(OrgEffect::Effect(Effect::Request {
                to: Target::Node(stream.attach.node.clone()),
                request: NodeRequest::Edge(Box::new(EdgeNotice::Gone {
                    stream: id,
                    token: offer.task.token,
                })),
                asker: None,
            }));
        }
    }

    /// The alerts it recorded.
    pub(crate) fn alerts(&self) -> &[QuotaAlert] {
        &self.state.alerts
    }

    fn hand_out(&mut self, out: &mut Vec<OrgEffect<P>>) {
        let mut kept = VecDeque::new();
        while let Some(offer) = self.offers.pop_front() {
            let stream = self
                .streams
                .iter()
                .filter(|(_, stream)| {
                    stream.open
                        && (stream.held.len() as u64) < u64::from(stream.attach.slots.max(1))
                        && takes(&stream.attach, &offer)
                })
                .min_by_key(|(id, stream)| (stream.held.len(), (*id).clone()))
                .map(|(id, _)| id.clone());
            let Some(stream_id) = stream else {
                kept.push_back(offer);
                continue;
            };
            let Some(stream) = self.streams.get_mut(&stream_id) else {
                kept.push_back(offer);
                continue;
            };
            let task = offer.task.task.clone();
            stream.held.insert(task.clone());
            out.push(OrgEffect::Effect(Effect::Request {
                to: Target::Node(stream.attach.node.clone()),
                request: NodeRequest::Edge(Box::new(EdgeNotice::HandOut {
                    stream: stream_id.clone(),
                    task: offer.task.clone(),
                })),
                asker: None,
            }));
            self.handed.insert(task, (stream_id, offer));
        }
        self.offers = kept;
    }
}

/// Whether a stream may be handed a task: its namespace, its labels, its action set, its actions.
fn takes(stream: &StreamAttach, offer: &HostTaskOffer) -> bool {
    let task = &offer.task;
    stream
        .namespace
        .as_ref()
        .is_none_or(|namespace| *namespace == task.namespace)
        && stream
            .labels
            .iter()
            .all(|(key, value)| task.labels.get(key) == Some(value))
        && stream
            .action_set
            .is_none_or(|action_set| action_set == offer.action_set)
        && (stream.actions.is_empty() || stream.actions.contains(&task.action))
}

/// The scripts new to a namespace among the admissions in flight.
fn distinct_new_scripts(
    in_flight: &[Admission],
    namespace: Option<&NamespaceName>,
    reported: &BTreeMap<NamespaceName, Reported>,
) -> u64 {
    let mut scripts = BTreeSet::new();
    for admission in in_flight {
        if namespace.is_some_and(|namespace| admission.namespace != *namespace) {
            continue;
        }
        let known = reported
            .get(&admission.namespace)
            .is_some_and(|counts| counts.gauges.scripts.contains_key(&admission.script));
        if !known {
            scripts.insert(admission.script);
        }
    }
    scripts.len() as u64
}

fn reply_admission<P: Plane>(reply: ReplyTo, admission: Result<(), Refusal>) -> OrgEffect<P> {
    OrgEffect::Effect(Effect::Reply(
        reply,
        NodeReply::Organization(OrgReply::Admission(admission)),
    ))
}

fn unwrap_effect<P: Plane>(effect: OrgEffect<P>) -> Effect<P> {
    match effect {
        OrgEffect::Effect(effect) => effect,
        OrgEffect::Persist { then, .. } => then
            .into_iter()
            .next()
            .unwrap_or(Effect::Local(Message::Nothing)),
    }
}
