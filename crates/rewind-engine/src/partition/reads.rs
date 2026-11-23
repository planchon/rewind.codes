//! The reads an edge asks the owner of a partition (`archi/05-testing-and-observability.md`,
//! "Read model", "Statistics"; `archi/06-storage-and-scale.md`, "Retention and deletion").
//!
//! A read answers what the partition's last durable batch left: a run's head and summary, its
//! history (from its partition while its attempt is live, from the archive once sealed), its
//! tasks (those in flight from the owner's memory), a page of a search, counters and gauges by
//! bucket, the use of its namespaces, shapes. Each read is a job of its own; the quota alerts of
//! a tenant homed here are answered from its organization actor's state.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rewind_core::{Seq, Timestamp};

use crate::node::message::{Message, ReplyTo, job};
use crate::partition::{Flight, Owner, OwnerMsg};
use crate::plane::Plane;
use crate::ports::link::{NodeReply, PartitionReply, PartitionRequest, Refusal, RunView};
use crate::ports::{
    Day, DayUsage, Hour, Month, NamespaceName, PartitionError, PartitionLog, QuotaAlert, Scope,
    TaskState, TenantId, Usage, Window,
};

/// A refusal in the shape of the request's reply.
pub(crate) fn refused(request: &PartitionRequest, refusal: Refusal) -> PartitionReply {
    match request {
        PartitionRequest::Start(_) => PartitionReply::Started(Err(refusal)),
        PartitionRequest::Signal(_)
        | PartitionRequest::Cancel { .. }
        | PartitionRequest::Webhook { .. } => PartitionReply::Enqueued(Err(refusal)),
        PartitionRequest::Release { .. }
        | PartitionRequest::GiveUp { .. }
        | PartitionRequest::Erase { .. } => PartitionReply::Done(Err(refusal)),
        PartitionRequest::Answer { .. } | PartitionRequest::Decide { .. } => {
            PartitionReply::Answered(Err(refusal))
        }
        PartitionRequest::GetRun { .. } => PartitionReply::Run(Err(refusal)),
        PartitionRequest::History { .. } => PartitionReply::History(Err(refusal)),
        PartitionRequest::Tasks { .. } => PartitionReply::Tasks(Err(refusal)),
        PartitionRequest::Search { .. } => PartitionReply::Found(Err(refusal)),
        PartitionRequest::Counters { .. } => PartitionReply::Counted(Err(refusal)),
        PartitionRequest::Shapes { .. } => PartitionReply::Shapes(Err(refusal)),
        PartitionRequest::Usage { .. } => PartitionReply::Used(Err(refusal)),
        PartitionRequest::QuotaAlerts { .. } => PartitionReply::Alerts(Err(refusal)),
    }
}

impl<P: Plane> Owner<P> {
    pub(crate) fn read(&mut self, request: PartitionRequest, reply: ReplyTo) {
        if let PartitionRequest::QuotaAlerts {
            tenant,
            namespace,
            month,
        } = &request
        {
            let alerts = self.quota_alerts(tenant, namespace.as_ref(), *month);
            self.reply(reply, PartitionReply::Alerts(Ok(alerts)));
            return;
        }
        let Some(log) = self.log.clone() else {
            return;
        };
        // What the records never say: the tasks in flight, and the namespaces counted here.
        let in_flight: BTreeSet<Seq> = match &request {
            PartitionRequest::Tasks { run } => self
                .flights
                .iter()
                .filter(|(task, flight)| {
                    task.run == *run
                        && matches!(flight, Flight::Sent { .. } | Flight::Handed { .. })
                })
                .map(|(task, _)| task.seq)
                .collect(),
            _ => BTreeSet::new(),
        };
        let counted: Vec<(TenantId, NamespaceName)> = match &request {
            PartitionRequest::Usage { scope, .. } => self
                .counted
                .iter()
                .filter(|(tenant, namespace)| scope.contains(tenant, namespace))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        let node = self.node.to_string();
        let now = self.now();
        let plane = Arc::clone(&self.plane);
        let partition = self.partition;
        self.out.push(job(async move {
            // A read that finds the partition opened by another node drops it, as a commit does.
            let fenced = AtomicBool::new(false);
            let unavailable = |error: PartitionError| {
                if matches!(error, PartitionError::Fenced { .. }) {
                    fenced.store(true, Ordering::Relaxed);
                }
                Refusal::Unavailable(error.to_string())
            };
            let answer = match request {
                PartitionRequest::GetRun { run } => PartitionReply::Run(
                    async {
                        let Some(head) = log.run(&run).await.map_err(unavailable)? else {
                            return Ok(None);
                        };
                        let summary = log.summary(&run).await.map_err(unavailable)?;
                        Ok(Some(RunView {
                            head,
                            summary: summary.map(|indexed| indexed.summary),
                        }))
                    }
                    .await,
                ),
                PartitionRequest::History { run, from, values } => PartitionReply::History(
                    super::attempt::read_attempt(&*plane, &*log, &run, from, values)
                        .await
                        .map(|read| read.map(|read| read.into_entries(from)))
                        .map_err(unavailable),
                ),
                PartitionRequest::Tasks { run } => PartitionReply::Tasks(
                    async {
                        if log.run(&run).await.map_err(unavailable)?.is_none() {
                            return Ok(None);
                        }
                        let mut tasks = log.tasks(&run).await.map_err(unavailable)?;
                        for record in &mut tasks {
                            if let TaskState::Ready { next_attempt_at } = record.task.state
                                && in_flight.contains(&record.task.task.seq)
                            {
                                record.task.state = TaskState::Claimed {
                                    owner: node.clone(),
                                    until: next_attempt_at,
                                };
                            }
                        }
                        Ok(Some(tasks))
                    }
                    .await,
                ),
                PartitionRequest::Search {
                    scope,
                    query,
                    order,
                    page,
                } => PartitionReply::Found(
                    log.search(&scope, &query, order, &page)
                        .await
                        .map_err(unavailable),
                ),
                PartitionRequest::Counters {
                    scope,
                    window,
                    bucket,
                } => PartitionReply::Counted(
                    async {
                        let mut counters = Vec::new();
                        for bucket in buckets(window, bucket) {
                            counters.push(log.counters(&scope, bucket).await.map_err(unavailable)?);
                        }
                        let gauges = log.gauges(&scope).await.map_err(unavailable)?;
                        Ok((counters, gauges))
                    }
                    .await,
                ),
                PartitionRequest::Usage { month, days, .. } => PartitionReply::Used(
                    usage(&*log, &counted, month, days, now)
                        .await
                        .map_err(unavailable),
                ),
                PartitionRequest::Shapes {
                    tenant,
                    namespace,
                    tools,
                } => PartitionReply::Shapes(
                    log.shapes(&tenant, namespace.as_ref(), &tools)
                        .await
                        .map_err(unavailable),
                ),
                other => refused(&other, Refusal::Unavailable("not a read".into())),
            };
            if fenced.load(Ordering::Relaxed) {
                return Message::Owner(partition, OwnerMsg::Fenced(Some(reply)));
            }
            Message::Reply(reply, NodeReply::Partition(Box::new(answer)))
        }));
    }
}

impl<P: Plane> Owner<P> {
    /// The alerts of a month an organization actor homed here recorded: of one namespace, or
    /// every alert of the tenant, the newest first. A tenant whose actor has not started yet
    /// answers from the state the partition kept of it.
    fn quota_alerts(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        month: Month,
    ) -> Vec<QuotaAlert> {
        let recorded = match self.orgs.get(tenant) {
            Some(actor) => actor.alerts(),
            None => self
                .org_states
                .get(tenant)
                .map_or(&[][..], |state| state.alerts.as_slice()),
        };
        let mut alerts: Vec<QuotaAlert> = recorded
            .iter()
            .filter(|alert| {
                alert.key.period == month
                    && namespace.is_none_or(|only| alert.key.namespace.as_ref() == Some(only))
            })
            .cloned()
            .collect();
        // The newest first; of those raised at once, the one recorded last.
        alerts.reverse();
        alerts.sort_by_key(|alert| std::cmp::Reverse(alert.raised_at));
        alerts
    }
}

/// The windows of `bucket` whole hours that cover `window`, from its start, the last one cut at
/// its end; the window itself without a bucket.
pub(crate) fn buckets(window: Window, bucket: Option<NonZeroU32>) -> Vec<Window> {
    let Some(width) = bucket else {
        return vec![window];
    };
    let mut windows = Vec::new();
    let mut from = window.from;
    while from < window.to {
        let to = Hour(from.0.saturating_add(width.get())).min(window.to);
        windows.push(Window::hours(from, to));
        from = to;
    }
    windows
}

/// The use in `month` of each namespace counted here: CPU time and runs created over the month,
/// with its days up to `now` when `days`; its active runs and distinct scripts now. A namespace
/// that used nothing is left out.
async fn usage<L: PartitionLog>(
    log: &L,
    counted: &[(TenantId, NamespaceName)],
    month: Month,
    days: bool,
    now: Timestamp,
) -> Result<BTreeMap<NamespaceName, Usage>, PartitionError> {
    let mut used = BTreeMap::new();
    for (tenant, namespace) in counted {
        let scope = Scope::Namespace {
            tenant: tenant.clone(),
            namespace: namespace.clone(),
        };
        let counters = log.counters(&scope, Window::of_month(month)).await?;
        let gauges = log.gauges(&scope).await?;
        let mut usage = Usage {
            cpu: counters.cpu,
            runs_started: counters.created,
            active_runs: gauges.active_runs,
            scripts: gauges.distinct_scripts(),
            days: Vec::new(),
        };
        if days && !counters.is_empty() {
            let mut day = month.first_day();
            while month.contains(day) && day.start() <= now {
                let of_day = log.counters(&scope, Window::of_day(day)).await?;
                if of_day.cpu > std::time::Duration::ZERO || of_day.created > 0 {
                    usage.days.push((
                        day,
                        DayUsage {
                            cpu: of_day.cpu,
                            runs_started: of_day.created,
                        },
                    ));
                }
                day = Day(day.0.saturating_add(1));
            }
        }
        if usage != Usage::default() {
            used.insert(namespace.clone(), usage);
        }
    }
    Ok(used)
}
