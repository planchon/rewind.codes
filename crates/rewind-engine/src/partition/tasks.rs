//! The call tasks of a partition's runs, at their owner (`archi/04-tools.md`, "Call task
//! lifecycle", "Policies", "Secrets"; `archi/07-code-architecture.md`, "Inside a node": action
//! tasks).
//!
//! ```text
//! ready ─ per-run limit ─┬─ MCP, API: a slot of the organization ─ SlotGranted ─► call executor
//!                        ├─ built-ins, test runs ───────────────────────────────► call executor
//!                        └─ harness action: prepared, then offered to the organization ─► an SDK stream
//! outcome ─► done (a completion in the inbox), ready after a backoff (delivery + 1), deferred
//!            with a deadline, parked; then the slot is freed
//! ```
//!
//! Sending a call or handing a task out writes nothing: the records say `ready` while the task
//! is in flight, and only the owner's memory ([`Flight`]) knows it left.

use std::collections::BTreeMap;

use rewind_core::{
    ActionFailure, ActionFailureKind, ActionRoute, Completion, Payload, Seq, Timestamp,
};
use serde_json::json;

use crate::executors::derive;
use crate::node::message::{Effect, Message, ReplyTo, Target};
use crate::partition::commit::{After, Unit};
use crate::partition::{Ask, Flight, Owner, OwnerMsg, tokens};
use crate::plane::{HarnessActions, Plane};
use crate::ports::PartitionLog as _;
use crate::ports::link::{
    Decision, HarnessFailure, HarnessTaskOffer, NodeRequest, PartitionReply, Refusal, SlotRequest,
    TaskAnswer, TenantRequest,
};
use crate::ports::partition::{Change, home_partition};
use crate::ports::{
    Dispatched, DueTimer, ParkKind, ParkReason, Projection, RunRef, SecretExpiredReason, SecretId,
    TaskChange, TaskRecord, TaskRef, TaskState, TaskToken, TenantId, TimerChange, TimerKind,
    Trigger, Watcher, Webhook, WebhookKind, deferred_timeout_failure,
};
use crate::tasks::calls::{
    CallDone, CallMsg, CallOutcome, CallWork, ChildStart, asked_wait, interrupted,
};

/// The message of a rejection without one.
const REJECTED: &str = "rejected by the harness";

impl<P: Plane> Owner<P> {
    // ---------------------------------------------------------------------------------------
    // On its way

    /// Sends a task on its way if it is ready, due, and within the run's limit.
    pub(crate) fn dispatch_task(&mut self, task: &TaskRef) {
        let now = self.now();
        let Some(slot) = self.slot(task.run.id) else {
            return;
        };
        if slot.head.is_ended() {
            return;
        }
        let Some(record) = slot.tasks.get(&task.seq).cloned() else {
            return;
        };
        if self
            .flights
            .get(task)
            .is_some_and(|flight| *flight != Flight::Waiting)
        {
            return;
        }
        let TaskState::Ready { next_delivery_at } = record.task.state else {
            return;
        };
        if next_delivery_at > now {
            // Its backoff timer sends it.
            return;
        }
        let in_flight = slot
            .tasks
            .values()
            .filter(|other| other.task.action == record.task.action && other.task.task != *task)
            .filter(|other| {
                self.flights
                    .get(&other.task.task)
                    .is_some_and(|flight| *flight != Flight::Waiting)
            })
            .count();
        if in_flight >= record.task.policy.concurrency.max(1) as usize {
            self.flights.insert(task.clone(), Flight::Waiting);
            return;
        }
        let host = matches!(record.task.route, ActionRoute::Harness { .. })
            && self.config.harness_actions == HarnessActions::Streams;
        let server = match &record.task.route {
            ActionRoute::Mcp { server } => Some(server.clone()),
            ActionRoute::Api { api } => Some(api.clone()),
            _ => None,
        };
        match server {
            Some(server) => {
                self.flights.insert(
                    task.clone(),
                    Flight::Asked {
                        server: server.clone(),
                    },
                );
                self.ask_slot(task, server);
            }
            None => self.send(task, record, None, host),
        }
    }

    /// Asks the tenant actor for a slot of `server` for the task: its concurrency, from
    /// the run's action set.
    fn ask_slot(&mut self, task: &TaskRef, server: String) {
        let limit = self
            .slot(task.run.id)
            .and_then(|slot| {
                let record = slot.tasks.get(&task.seq)?;
                let action_set = slot.action_set.as_ref()?;
                match &record.task.route {
                    ActionRoute::Mcp { server } => action_set.server(server).map(|s| s.concurrency),
                    ActionRoute::Api { api } => action_set.api(api).map(|api| api.concurrency),
                    _ => None,
                }
            })
            .unwrap_or(8);
        let tenant = task.run.tenant.clone();
        self.tell(
            Target::Home(tenant.clone()),
            NodeRequest::Tenant {
                tenant,
                request: Box::new(TenantRequest::Slot(SlotRequest {
                    partition: self.partition,
                    generation: self.generation,
                    task: task.clone(),
                    server,
                    limit,
                })),
            },
        );
    }

    /// The partition `home` opened again: the tenant actors homed there are new, and know
    /// none of the slots asked of the ones before. Each call of a tenant homed there that waits
    /// for its slot asks again; dropped, it would wait forever.
    pub(crate) fn home_reopened(&mut self, home: rewind_core::PartitionId) -> Vec<Effect<P>> {
        if self.is_closed() {
            return Vec::new();
        }
        let asked: Vec<(TaskRef, String)> = self
            .flights
            .iter()
            .filter(|(task, _)| home_partition(&task.run.tenant, self.config.partitions) == home)
            .filter_map(|(task, flight)| match flight {
                Flight::Asked { server } => Some((task.clone(), server.clone())),
                _ => None,
            })
            .collect();
        for (task, server) in asked {
            self.ask_slot(&task, server);
        }
        std::mem::take(&mut self.out)
    }

    /// Hands the task to the call executor, with a new token.
    fn send(&mut self, task: &TaskRef, record: TaskRecord, server: Option<String>, hand_out: bool) {
        let Some(log) = self.log.clone() else {
            return;
        };
        let Some(slot) = self.slot(task.run.id) else {
            return;
        };
        let token = tokens::task_token(task.run.id);
        let hash = tokens::hash(&token.0);
        let head = slot.head.clone();
        let mode = slot.mode;
        self.tokens.insert(hash, task.clone());
        self.flights.insert(
            task.clone(),
            if hand_out {
                Flight::Preparing
            } else {
                Flight::Sent {
                    server,
                    token: hash,
                }
            },
        );
        self.out
            .push(Effect::Local(Message::Calls(CallMsg::Dispatch(Box::new(
                CallWork {
                    partition: self.partition,
                    log,
                    task: record,
                    head,
                    mode,
                    token,
                    hand_out,
                },
            )))));
    }

    /// The tenant actor granted the task's slot.
    pub(crate) fn granted(&mut self, task: &TaskRef) {
        let server = match self.flights.get(task) {
            Some(Flight::Asked { server }) => server.clone(),
            _ => {
                // A grant for a task done or sent meanwhile: the slot goes back.
                self.free_slot(task, None);
                return;
            }
        };
        let record = self
            .slot(task.run.id)
            .and_then(|slot| slot.tasks.get(&task.seq).cloned());
        match record {
            Some(record)
                if !self
                    .slot(task.run.id)
                    .is_some_and(|slot| slot.head.is_ended()) =>
            {
                self.send(task, record, Some(server), false);
            }
            _ => {
                self.flights.remove(task);
                self.free_slot(task, Some(server));
            }
        }
    }

    /// Gives a server's slot back.
    fn free_slot(&mut self, task: &TaskRef, server: Option<String>) {
        let Some(server) = server.or_else(|| match self.flights.get(task) {
            Some(Flight::Asked { server }) => Some(server.clone()),
            Some(Flight::Sent { server, .. }) => server.clone(),
            _ => None,
        }) else {
            return;
        };
        let tenant = task.run.tenant.clone();
        self.tell(
            Target::Home(tenant.clone()),
            NodeRequest::Tenant {
                tenant,
                request: Box::new(TenantRequest::SlotFreed {
                    task: task.clone(),
                    server,
                }),
            },
        );
    }

    /// What follows a task leaving its flight: its slot or its stream's slot is free, and the
    /// run's calls that waited for its limit go.
    pub(crate) fn drop_flight(&mut self, unit: &mut Unit<P>, task: &TaskRef) {
        let flight = self.flights.remove(task);
        self.tokens.retain(|_, of| of != task);
        let tenant = task.run.tenant.clone();
        match flight {
            Some(Flight::Asked { server })
            | Some(Flight::Sent {
                server: Some(server),
                ..
            }) => {
                unit.then(After::Effect(Effect::Request {
                    to: Target::Home(tenant.clone()),
                    request: NodeRequest::Tenant {
                        tenant: tenant.clone(),
                        request: Box::new(TenantRequest::SlotFreed {
                            task: task.clone(),
                            server,
                        }),
                    },
                    asker: None,
                }));
            }
            Some(flight @ (Flight::Handed { .. } | Flight::Answered | Flight::Preparing)) => {
                let gone = matches!(flight, Flight::Handed { .. });
                unit.then(After::Effect(Effect::Request {
                    to: Target::Home(tenant.clone()),
                    request: NodeRequest::Tenant {
                        tenant: tenant.clone(),
                        request: Box::new(TenantRequest::HarnessTaskDone {
                            task: task.clone(),
                            gone,
                        }),
                    },
                    asker: None,
                }));
            }
            _ => {}
        }
        let waiting: Vec<TaskRef> = self
            .flights
            .iter()
            .filter(|(other, flight)| other.run == task.run && **flight == Flight::Waiting)
            .map(|(other, _)| other.clone())
            .collect();
        for other in waiting {
            self.flights.remove(&other);
            unit.then(After::Dispatch(other));
        }
    }

    // ---------------------------------------------------------------------------------------
    // Outcomes

    pub(crate) fn call_done(&mut self, done: CallDone) {
        let CallDone {
            task,
            delivery,
            action,
            outcome,
        } = done;
        let record = self
            .slot(task.run.id)
            .filter(|slot| !slot.head.is_ended())
            .and_then(|slot| slot.tasks.get(&task.seq).cloned())
            .filter(|record| record.task.delivery == delivery);
        let Some(record) = record else {
            // The task is done, its run ended or erased, or it was sent again since: the outcome
            // is dropped, and the slot it held goes back.
            tracing::debug!(%task, "action outcome dropped");
            let mut unit = Unit::new();
            if matches!(
                self.flights.get(&task),
                Some(Flight::Sent { .. } | Flight::Preparing)
            ) {
                self.drop_flight(&mut unit, &task);
            }
            self.push(unit);
            return;
        };
        let _ = action;
        match outcome {
            CallOutcome::Dispatched(dispatched) => self.dispatched(&task, record, dispatched),
            CallOutcome::NotSent(error) => {
                tracing::error!(%task, %error, "a call could not be prepared");
                self.backoff(&task, record, std::time::Duration::ZERO, false);
            }
            CallOutcome::SignalUrl { name } => self.signal_url_created(&task, record, &name),
            CallOutcome::Child(child) => match *child {
                ChildStart::New { start, neighbours } => {
                    let tenant = task.run.tenant.clone();
                    let request = NodeRequest::Tenant {
                        tenant: tenant.clone(),
                        request: Box::new(TenantRequest::Admit(crate::ports::link::Admit {
                            partition: self.partition,
                            generation: self.generation,
                            run: start.run.id,
                            namespace: start.namespace.clone(),
                            workflow: start.metadata.workflow.as_ref().map(|of| of.id.clone()),
                            at: start.created_at,
                        })),
                    };
                    self.ask(
                        Target::Home(tenant),
                        request,
                        Ask::Child {
                            task,
                            delivery,
                            start,
                            neighbours,
                        },
                    );
                }
                ChildStart::Found(child) => self.child_found(&task, record, child),
            },
            CallOutcome::Signal(signal) => {
                let mut unit = Unit::new();
                self.enqueue(
                    &mut unit,
                    &signal.target,
                    Trigger::Completion(Completion::SignalReceived {
                        name: signal.name.clone(),
                        payload: Payload::Value(signal.payload.clone()),
                    }),
                    Some(signal.key.clone()),
                );
                self.finish_in(&mut unit, &task, &record, Ok(json!({})));
                self.push(unit);
            }
            CallOutcome::TestStart { dispatched, end } => match (dispatched, end) {
                (Dispatched::Result(result), Some((child, end))) => {
                    let mut unit = Unit::new();
                    self.finish_in(&mut unit, &task, &record, Ok(result));
                    let name = rewind_core::end_signal_name(child);
                    self.enqueue(
                        &mut unit,
                        &task.run,
                        Trigger::Completion(Completion::SignalReceived {
                            name: name.clone(),
                            payload: Payload::Value(end),
                        }),
                        Some(name),
                    );
                    self.push(unit);
                }
                (dispatched, _) => self.dispatched(&task, record, dispatched),
            },
            CallOutcome::HarnessReady(host) => {
                if self.flights.get(&task) != Some(&Flight::Preparing) {
                    return;
                }
                let hash = tokens::hash(&host.token.0);
                self.tokens.insert(hash, task.clone());
                self.flights
                    .insert(task.clone(), Flight::Handed { token: hash });
                let tenant = task.run.tenant.clone();
                self.tell(
                    Target::Home(tenant.clone()),
                    NodeRequest::Tenant {
                        tenant,
                        request: Box::new(TenantRequest::HarnessTask(Box::new(HarnessTaskOffer {
                            partition: self.partition,
                            generation: self.generation,
                            task: *host,
                            action_set: record.task.action_set,
                            grace: record.task.policy.reconnect_timeout,
                        }))),
                    },
                );
            }
        }
    }

    /// What the dispatcher answered.
    fn dispatched(&mut self, task: &TaskRef, record: TaskRecord, dispatched: Dispatched) {
        match dispatched {
            Dispatched::Result(result) => {
                let mut unit = Unit::new();
                self.finish_in(&mut unit, task, &record, Ok(result));
                self.push(unit);
            }
            Dispatched::Failed(failure) => {
                let mut unit = Unit::new();
                self.finish_in(&mut unit, task, &record, Err(failure));
                self.push(unit);
            }
            Dispatched::Transient(failure) => self.transient(task, record, failure),
            Dispatched::Deferred => {
                let token = match self.flights.get(task) {
                    Some(Flight::Sent { token, .. } | Flight::Handed { token }) => Some(*token),
                    _ => None,
                };
                let until = self.deferral_deadline(&record, None);
                self.defer(task, record, token, until, None);
            }
            Dispatched::Park(reason) => self.park(task, record, reason),
        }
    }

    /// Ends the task with its outcome, in `unit`: the completion in the run's inbox, the task
    /// deleted, its deadline cancelled, its retries counted.
    pub(crate) fn finish_in(
        &mut self,
        unit: &mut Unit<P>,
        task: &TaskRef,
        record: &TaskRecord,
        outcome: Result<serde_json::Value, ActionFailure>,
    ) {
        let max = self.config.limits.max_payload_bytes;
        let completion = match outcome {
            Ok(result) => {
                let payload = Payload::Value(result);
                let size = crate::payloads::value_size(&payload).unwrap_or_default();
                if size > max {
                    Completion::CallFailed {
                        seq: task.seq,
                        error: ActionFailure {
                            kind: ActionFailureKind::Error,
                            message: Refusal::ResultTooLarge { size, max }.to_string(),
                            data: None,
                        },
                    }
                } else {
                    Completion::CallCompleted {
                        seq: task.seq,
                        result: payload,
                    }
                }
            }
            Err(error) => Completion::CallFailed {
                seq: task.seq,
                error,
            },
        };
        self.enqueue(unit, &task.run, Trigger::Completion(completion), None);
        unit.change(Change::Task {
            run: task.run.clone(),
            seq: task.seq,
            change: TaskChange::Delete,
        });
        if matches!(record.task.state, TaskState::Deferred { .. }) {
            unit.change(Change::Timer(TimerChange::Cancel(
                TimerKind::DeferredDeadline {
                    run: task.run.clone(),
                    seq: task.seq,
                },
            )));
        }
        if let Some(slot) = self.slot(task.run.id)
            && let Some(delta) = derive::redeliveries_counters(
                &slot.head,
                &record.task.action,
                record.task.delivery,
                self.now(),
            )
        {
            unit.change(Change::Counters(vec![delta]));
        }
        if let Some(slot) = self.slot_mut(task.run.id) {
            slot.tasks.remove(&task.seq);
        }
        self.drop_flight(unit, task);
    }

    /// A transient failure: when its action is idempotent, the task is ready again after a
    /// backoff, delivery + 1; otherwise the failure is its outcome.
    fn transient(&mut self, task: &TaskRef, record: TaskRecord, failure: ActionFailure) {
        let redeliver =
            record.task.policy.idempotent && record.task.delivery < self.config.max_deliveries;
        if !redeliver {
            let mut unit = Unit::new();
            self.finish_in(&mut unit, task, &record, Err(failure));
            self.push(unit);
            return;
        }
        crate::metrics::redelivery(&record.task.action);
        let asked = asked_wait(&failure);
        self.backoff(task, record, asked, true);
    }

    /// The task is ready again after the backoff of its delivery (at least `at_least`): delivery
    /// + 1 when it was sent.
    fn backoff(
        &mut self,
        task: &TaskRef,
        mut record: TaskRecord,
        at_least: std::time::Duration,
        sent: bool,
    ) {
        let key = crate::tasks::idempotency_key(task.run.id, task.seq);
        let delay = self
            .config
            .backoff
            .delay(&key, record.task.delivery.max(1))
            .max(at_least);
        let next = self.now().saturating_add(delay);
        record.task.state = TaskState::Ready {
            next_delivery_at: next,
        };
        if sent {
            record.task.delivery = record.task.delivery.saturating_add(1);
        }
        let mut unit = Unit::new();
        self.put_task(&mut unit, record);
        unit.change(Change::Timer(TimerChange::Set(DueTimer {
            at: next,
            kind: TimerKind::RedeliveryBackoff {
                run: task.run.clone(),
                seq: task.seq,
            },
        })));
        self.drop_flight(&mut unit, task);
        self.push(unit);
    }

    /// Writes the task as it is now, and the run's summary with its phase.
    fn put_task(&mut self, unit: &mut Unit<P>, record: TaskRecord) {
        let task = record.task.task.clone();
        unit.change(Change::Task {
            run: task.run.clone(),
            seq: task.seq,
            change: TaskChange::Put(Box::new(record.clone())),
        });
        let Some(slot) = self.slot_mut(task.run.id) else {
            return;
        };
        slot.tasks.insert(task.seq, record);
        if let Some(summary) = slot.summary.clone() {
            let phases: BTreeMap<Seq, crate::ports::TaskPhase> = slot
                .tasks
                .iter()
                .map(|(seq, task)| (*seq, derive::phase_of(task)))
                .collect();
            let summary = derive::with_phases(summary, &phases);
            if Some(&summary) != slot.summary.as_ref() {
                slot.head.waits = summary.summary.waits.clone();
                slot.summary = Some(summary.clone());
                unit.change(Change::Summary {
                    run: task.run.clone(),
                    summary: Box::new(summary),
                });
            }
        }
    }

    /// The deadline of a deferred task: its execution timeout from now (and `requested`, when
    /// the handler asks for less), bounded by the run's deadline.
    fn deferral_deadline(
        &self,
        record: &TaskRecord,
        requested: Option<std::time::Duration>,
    ) -> Timestamp {
        let now = self.now();
        let expires_at = self
            .slot(record.task.task.run.id)
            .map_or(Timestamp(u64::MAX), |slot| slot.head.expires_at);
        [requested, record.task.policy.execution_timeout]
            .into_iter()
            .flatten()
            .map(|timeout| now.saturating_add(timeout))
            .fold(expires_at, Timestamp::min)
    }

    /// The handler answers later, through the token, before `until`.
    fn defer(
        &mut self,
        task: &TaskRef,
        mut record: TaskRecord,
        token: Option<rewind_core::Digest>,
        until: Timestamp,
        reply: Option<ReplyTo>,
    ) {
        record.task.state = TaskState::Deferred { until };
        record.token_hash = token.or(record.token_hash);
        let mut unit = Unit::new();
        self.put_task(&mut unit, record);
        unit.change(Change::Timer(TimerChange::Set(DueTimer {
            at: until,
            kind: TimerKind::DeferredDeadline {
                run: task.run.clone(),
                seq: task.seq,
            },
        })));
        // A deferred call holds no slot, nor a stream's; its token stays known.
        let flight = self.flights.remove(task);
        let tenant = task.run.tenant.clone();
        match flight {
            Some(Flight::Sent {
                server: Some(server),
                ..
            }) => {
                unit.then(After::Effect(Effect::Request {
                    to: Target::Home(tenant.clone()),
                    request: NodeRequest::Tenant {
                        tenant,
                        request: Box::new(TenantRequest::SlotFreed {
                            task: task.clone(),
                            server,
                        }),
                    },
                    asker: None,
                }));
            }
            Some(flight @ (Flight::Handed { .. } | Flight::Answered)) => {
                unit.then(After::Effect(Effect::Request {
                    to: Target::Home(tenant.clone()),
                    request: NodeRequest::Tenant {
                        tenant,
                        request: Box::new(TenantRequest::HarnessTaskDone {
                            task: task.clone(),
                            gone: matches!(flight, Flight::Handed { .. }),
                        }),
                    },
                    asker: None,
                }));
            }
            _ => {}
        }
        if let Some(reply) = reply {
            unit.then(After::Reply(reply, PartitionReply::Answered(Ok(()))));
        }
        self.push(unit);
    }

    /// The task waits for the harness: a schema change, a refused call, a missing secret. Its
    /// tenant's subscriptions are told.
    fn park(&mut self, task: &TaskRef, mut record: TaskRecord, reason: ParkReason) {
        record.task.state = TaskState::Parked(reason.clone());
        let mut unit = Unit::new();
        if let Some(event) = self.parked_event(task, &record, &reason) {
            let entry = self.outbox_entry(
                task.run.tenant.clone(),
                Some(task.run.id),
                Projection::Webhooks(event),
            );
            unit.project(entry);
        }
        tracing::warn!(%task, kind = reason.kind.as_str(), "call parked");
        self.put_task(&mut unit, record);
        self.drop_flight(&mut unit, task);
        self.push(unit);
    }

    fn parked_event(
        &self,
        task: &TaskRef,
        record: &TaskRecord,
        reason: &ParkReason,
    ) -> Option<Webhook> {
        let slot = self.slot(task.run.id)?;
        let (seq, action, message) = (task.seq, record.task.action.clone(), reason.message.clone());
        let secret_expired = |reason| WebhookKind::SecretExpired {
            seq,
            action: action.clone(),
            message: message.clone(),
            reason,
            secret: record
                .task
                .route
                .server()
                .and_then(|server| slot.head.metadata.secrets.get(server))
                .cloned(),
        };
        let kind = match reason.kind {
            ParkKind::SchemaChanged => WebhookKind::ActionSchemaChanged {
                seq,
                action: action.clone(),
                message: message.clone(),
            },
            ParkKind::CredentialsRefused => secret_expired(SecretExpiredReason::Refused),
            ParkKind::SecretMissing => secret_expired(SecretExpiredReason::Missing),
        };
        Some(Webhook {
            id: format!("{}/{}/{}", task.run.id, seq.0, record.task.delivery),
            tenant: task.run.tenant.clone(),
            namespace: Some(slot.head.namespace.clone()),
            run: Some(task.run.id),
            labels: slot.head.metadata.labels.clone(),
            occurred_at: self.now(),
            kind,
        })
    }

    /// `signalUrl(name)` of a live run: a token bound to the run and the name, and its URL.
    fn signal_url_created(&mut self, task: &TaskRef, record: TaskRecord, name: &str) {
        let token = tokens::signal_url_token(task.run.id);
        let hash = tokens::hash(&token);
        let base = self.config.public_url.clone().unwrap_or_default();
        let url = format!("{}/hooks/{token}", base.trim_end_matches('/'));
        let mut unit = Unit::new();
        unit.change(Change::SignalUrlToken {
            run: task.run.clone(),
            name: name.to_owned(),
            hash,
        });
        self.finish_in(&mut unit, task, &record, Ok(json!({ "url": url })));
        self.push(unit);
    }

    /// A workflow run a call starts was admitted, or refused.
    pub(crate) fn child_admitted(
        &mut self,
        task: &TaskRef,
        delivery: u32,
        start: Box<crate::ports::link::StartAtOwner>,
        admission: Result<(), Refusal>,
        neighbours: &[rewind_core::RunId],
    ) {
        let record = self
            .slot(task.run.id)
            .filter(|slot| !slot.head.is_ended())
            .and_then(|slot| slot.tasks.get(&task.seq).cloned())
            .filter(|record| record.task.delivery == delivery);
        let Some(record) = record else {
            // The caller ended meanwhile: the admission is released.
            if admission.is_ok() {
                self.report(
                    start.run.tenant.clone(),
                    start.namespace.clone(),
                    Vec::new(),
                    vec![start.run.id],
                );
            }
            let mut unit = Unit::new();
            self.drop_flight(&mut unit, task);
            self.push(unit);
            return;
        };
        let mut unit = Unit::new();
        match admission {
            Ok(()) => {
                let child = start.run.clone();
                let detached = start
                    .metadata
                    .started_by
                    .as_ref()
                    .is_some_and(|by| by.detached);
                self.create(&mut unit, *start, neighbours);
                unit.change(Change::Watch {
                    run: child.clone(),
                    watcher: Watcher {
                        run: task.run.id,
                        seq: task.seq,
                        attached: !detached,
                    },
                });
                self.finish_in(
                    &mut unit,
                    task,
                    &record,
                    Ok(json!({ "runId": child.id.to_string() })),
                );
            }
            Err(refusal) => {
                let message = match &refusal {
                    Refusal::Quota(_) => format!("{refusal}: no run of the workflow was started"),
                    other => other.to_string(),
                };
                if matches!(refusal, Refusal::Unavailable(_)) {
                    self.push(unit);
                    self.backoff(task, record, std::time::Duration::ZERO, false);
                    return;
                }
                self.finish_in(
                    &mut unit,
                    task,
                    &record,
                    Err(ActionFailure {
                        kind: ActionFailureKind::Unavailable,
                        message,
                        data: None,
                    }),
                );
            }
        }
        self.push(unit);
    }

    /// The call's key already started a run: a fork's call its source left without an outcome.
    /// The fork watches it, never attached.
    fn child_found(&mut self, task: &TaskRef, record: TaskRecord, child: rewind_core::RunId) {
        let mut unit = Unit::new();
        unit.change(Change::Watch {
            run: RunRef::new(task.run.tenant.clone(), child),
            watcher: Watcher {
                run: task.run.id,
                seq: task.seq,
                attached: false,
            },
        });
        self.finish_in(
            &mut unit,
            task,
            &record,
            Ok(json!({ "runId": child.to_string() })),
        );
        self.push(unit);
    }

    // ---------------------------------------------------------------------------------------
    // The harness

    /// A handler's answer through its token.
    pub(crate) fn answer_task(
        &mut self,
        tenant: &TenantId,
        token: &TaskToken,
        answer: TaskAnswer,
        reply: ReplyTo,
    ) {
        let hash = tokens::hash(&token.0);
        let run = tokens::run_of(&token.0);
        let found = run.and_then(|run| {
            let slot = self.slot(run)?;
            if slot.head.run.tenant != *tenant || slot.head.is_ended() {
                return None;
            }
            let by_flight = self
                .tokens
                .get(&hash)
                .filter(|task| task.run.id == run)
                .and_then(|task| slot.tasks.get(&task.seq))
                .filter(|record| {
                    matches!(
                        self.flights.get(&record.task.task),
                        Some(Flight::Sent { .. } | Flight::Handed { .. })
                    ) || record.token_hash == Some(hash)
                });
            by_flight
                .or_else(|| {
                    slot.tasks
                        .values()
                        .find(|record| record.token_hash == Some(hash))
                })
                .cloned()
        });
        let Some(record) = found else {
            self.reply(reply, PartitionReply::Answered(Err(Refusal::TaskGone)));
            return;
        };
        let task = record.task.task.clone();
        match answer {
            TaskAnswer::Complete(result) => {
                let max = self.config.limits.max_payload_bytes;
                let payload = Payload::Value(result);
                let size = crate::payloads::value_size(&payload).unwrap_or_default();
                if size > max {
                    self.reply(
                        reply,
                        PartitionReply::Answered(Err(Refusal::ResultTooLarge { size, max })),
                    );
                    return;
                }
                let Payload::Value(result) = payload else {
                    return;
                };
                self.answered(&task);
                let mut unit = Unit::new();
                self.finish_in(&mut unit, &task, &record, Ok(result));
                unit.then(After::Reply(reply, PartitionReply::Answered(Ok(()))));
                self.push(unit);
            }
            TaskAnswer::Fail(HarnessFailure {
                message,
                data,
                retryable,
            }) => {
                let failure = ActionFailure {
                    kind: if retryable {
                        ActionFailureKind::Unavailable
                    } else {
                        ActionFailureKind::Error
                    },
                    message,
                    data,
                };
                self.answered(&task);
                if retryable
                    && record.task.policy.idempotent
                    && record.task.delivery < self.config.max_deliveries
                {
                    self.reply(reply, PartitionReply::Answered(Ok(())));
                    self.backoff(&task, record, asked_wait(&failure), true);
                    return;
                }
                let mut unit = Unit::new();
                self.finish_in(&mut unit, &task, &record, Err(failure));
                unit.then(After::Reply(reply, PartitionReply::Answered(Ok(()))));
                self.push(unit);
            }
            TaskAnswer::Defer { timeout } => {
                self.answered(&task);
                let until = self.deferral_deadline(&record, timeout);
                self.defer(&task, record, Some(hash), until, Some(reply));
            }
        }
    }

    /// The harness decides on a task that waits for it.
    pub(crate) fn decide(&mut self, task: &TaskRef, decision: Decision, reply: ReplyTo) {
        let record = self
            .slot(task.run.id)
            .filter(|slot| !slot.head.is_ended())
            .and_then(|slot| slot.tasks.get(&task.seq).cloned());
        let Some(mut record) = record else {
            self.reply(reply, PartitionReply::Answered(Err(Refusal::TaskGone)));
            return;
        };
        let parked = match &record.task.state {
            TaskState::Approval => None,
            TaskState::Parked(reason) => Some(reason.kind),
            other => {
                let state = other.name();
                self.reply(
                    reply,
                    PartitionReply::Answered(Err(Refusal::TaskNotWaiting {
                        task: task.to_string(),
                        state,
                    })),
                );
                return;
            }
        };
        let mut unit = Unit::new();
        match decision {
            Decision::Approve => {
                if parked == Some(ParkKind::SchemaChanged) {
                    record.task.schema_change_approved = true;
                }
                record.task.state = TaskState::Ready {
                    next_delivery_at: self.now(),
                };
                self.put_task(&mut unit, record);
                unit.then(After::Dispatch(task.clone()));
            }
            Decision::Reject(message) => {
                let failure = ActionFailure {
                    kind: ActionFailureKind::Error,
                    message: message.unwrap_or_else(|| REJECTED.to_owned()),
                    data: None,
                };
                self.finish_in(&mut unit, task, &record, Err(failure));
            }
        }
        unit.then(After::Reply(reply, PartitionReply::Answered(Ok(()))));
        self.push(unit);
    }

    /// A task handed on a stream is answered through its token: what follows frees the stream's
    /// slot without telling it the task is gone.
    fn answered(&mut self, task: &TaskRef) {
        if let Some(flight @ Flight::Handed { .. }) = self.flights.get_mut(task) {
            *flight = Flight::Answered;
        }
    }

    /// A harness task handed on a stream that closed and did not come back within its grace: sent
    /// again with delivery + 1 when its action is idempotent, failed `outcome_unknown` when it is
    /// not or past the maximum of deliveries.
    pub(crate) fn lost(&mut self, task: &TaskRef, token: &TaskToken) {
        let hash = tokens::hash(&token.0);
        if self.flights.get(task) != Some(&Flight::Handed { token: hash }) {
            return;
        }
        let record = self
            .slot(task.run.id)
            .filter(|slot| !slot.head.is_ended())
            .and_then(|slot| slot.tasks.get(&task.seq).cloned());
        let Some(record) = record else {
            return;
        };
        self.interrupt(task, record);
    }

    /// An interrupted call, as its policy says.
    pub(crate) fn interrupt(&mut self, task: &TaskRef, mut record: TaskRecord) {
        let next = record.task.delivery.saturating_add(1);
        let mut unit = Unit::new();
        match interrupted(
            &record.task.action,
            record.task.policy.idempotent,
            next,
            self.config.max_deliveries,
        ) {
            Some(failure) => self.finish_in(&mut unit, task, &record, Err(failure)),
            None => {
                record.task.delivery = next;
                record.task.state = TaskState::Ready {
                    next_delivery_at: self.now(),
                };
                self.put_task(&mut unit, record);
                self.drop_flight(&mut unit, task);
                unit.then(After::Dispatch(task.clone()));
            }
        }
        self.push(unit);
    }

    /// A deferred task's deadline passed: it ends with its timeout failure.
    pub(crate) fn deferred_deadline(&mut self, task: &TaskRef) {
        let record = self
            .slot(task.run.id)
            .and_then(|slot| slot.tasks.get(&task.seq).cloned())
            .filter(|record| matches!(record.task.state, TaskState::Deferred { .. }));
        let Some(record) = record else {
            return;
        };
        let mut unit = Unit::new();
        self.finish_in(&mut unit, task, &record, Err(deferred_timeout_failure()));
        self.push(unit);
    }

    // ---------------------------------------------------------------------------------------
    // Secrets put again

    /// A secret changed: once it is put again, the calls parked by a refusal or a missing
    /// secret in the runs that use it are ready again (`archi/04-tools.md`, "Secrets"). A
    /// secret deleted resumes nothing: its calls would park again.
    pub(crate) fn secret_put(&mut self, tenant: TenantId, secret: SecretId) {
        let Some(log) = self.log.clone() else {
            return;
        };
        let partition = self.partition;
        let plane = std::sync::Arc::clone(&self.plane);
        let today = crate::ports::Day::of(self.now());
        self.out.push(crate::node::message::job(async move {
            match crate::ports::SecretStore::meta(plane.secrets(), &tenant, &secret, today).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Message::Owner(
                        partition,
                        OwnerMsg::Step(Box::new(super::runs::Step::Parked {
                            secret,
                            tasks: Ok(Vec::new()),
                        })),
                    );
                }
                Err(error) => {
                    return Message::Owner(
                        partition,
                        OwnerMsg::Step(Box::new(super::runs::Step::Parked {
                            secret,
                            tasks: Err(error.to_string()),
                        })),
                    );
                }
            }
            let scope = match &secret.namespace {
                crate::ports::SecretNamespace::Namespace(namespace) => {
                    crate::ports::Scope::Namespace {
                        tenant: tenant.clone(),
                        namespace: namespace.clone(),
                    }
                }
                crate::ports::SecretNamespace::Organization => {
                    crate::ports::Scope::Tenant(tenant.clone())
                }
            };
            let query = crate::ports::RunQuery {
                tenant: tenant.clone(),
                terms: vec![crate::ports::Term {
                    negated: false,
                    predicate: crate::ports::Predicate::Waiting(
                        [crate::ports::WaitingFor::Parked].into(),
                    ),
                }],
            };
            let tasks = async {
                let mut tasks = Vec::new();
                let mut page = crate::ports::Page::first(1_000);
                loop {
                    let found = log
                        .search(&scope, &query, crate::ports::RunOrder::Oldest, &page)
                        .await
                        .map_err(|error| error.to_string())?;
                    for summary in &found.summaries {
                        for record in log
                            .tasks(&summary.run)
                            .await
                            .map_err(|error| error.to_string())?
                        {
                            if let TaskState::Parked(reason) = &record.task.state
                                && reason.kind.resumed_by_secret()
                            {
                                tasks.push(record.task.task.clone());
                            }
                        }
                    }
                    match found.cursor {
                        Some(cursor) => page = crate::ports::Page::after(cursor, 1_000),
                        None => break,
                    }
                }
                Ok::<_, String>(tasks)
            }
            .await;
            Message::Owner(
                partition,
                OwnerMsg::Step(Box::new(super::runs::Step::Parked { secret, tasks })),
            )
        }));
    }

    /// Resumes a parked task if its run uses the secret for the task's server.
    pub(crate) fn resume(&mut self, task: &TaskRef, secret: &SecretId) {
        let Some(slot) = self.slot(task.run.id) else {
            return;
        };
        let Some(mut record) = slot.tasks.get(&task.seq).cloned() else {
            return;
        };
        let resumable = matches!(&record.task.state, TaskState::Parked(reason) if reason.kind.resumed_by_secret());
        let server = record
            .task
            .route
            .server()
            .map(str::to_owned)
            .unwrap_or_else(|| record.task.action.clone());
        let uses = slot
            .head
            .metadata
            .secrets
            .get(&server)
            .is_some_and(|attached| SecretId::resolve(attached, &slot.head.namespace) == *secret);
        if !resumable || !uses {
            return;
        }
        record.task.state = TaskState::Ready {
            next_delivery_at: self.now(),
        };
        let mut unit = Unit::new();
        self.put_task(&mut unit, record);
        unit.then(After::Dispatch(task.clone()));
        self.push(unit);
    }
}
