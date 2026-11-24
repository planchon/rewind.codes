//! The requests and replies between nodes, their notices and their subscriptions
//! (`archi/07-code-architecture.md`, "`Cluster` and `NodeLink`").
//!
//! Enums are written as their variant's name in snake case, holding the variant's fields: a
//! unit variant is a string (`"done"`), any other an object of one key
//! (`{"not_owner":{"partition":3}}`). A refusal is `{"Err":{…}}` beside `{"Ok":…}`.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::time::Duration;

use rewind_core::{Event, JsonValue, Lifecycle, PartitionId, Timestamp};
use rewind_engine::ports as engine;
use rewind_engine::ports::link;
use rewind_format::partition as record;
use rewind_store_slatedb::codec;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::values::{
    Counters, Found, Month, Page, QuotaExceeded, RunOrder, RunQuery, RunRef, RunView, Scope,
    TaskRef, Usage, Window, summary_of, summary_record,
};
use super::{Hex, Id, Invalid, namespace, node, present, tenant};

// -------------------------------------------------------------------------------------------
// Requests and replies

/// A request to a node: `{"partition":{"partition":3,"request":{…}}}`,
/// `{"organization":{"tenant":"acme","request":{…}}}`, `{"owner":{"partition":3,"notice":{…}}}`
/// or `{"edge":{…}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeRequest {
    Partition {
        partition: u16,
        request: Box<PartitionRequest>,
    },
    Tenant {
        tenant: String,
        request: Box<TenantRequest>,
    },
    Owner {
        partition: u16,
        notice: Box<OwnerNotice>,
    },
    Edge(Box<EdgeNotice>),
}

impl From<&link::NodeRequest> for NodeRequest {
    fn from(request: &link::NodeRequest) -> Self {
        match request {
            link::NodeRequest::Partition { partition, request } => NodeRequest::Partition {
                partition: partition.get(),
                request: Box::new((&**request).into()),
            },
            link::NodeRequest::Tenant { tenant, request } => NodeRequest::Tenant {
                tenant: tenant.to_string(),
                request: Box::new((&**request).into()),
            },
            link::NodeRequest::Owner { partition, notice } => NodeRequest::Owner {
                partition: partition.get(),
                notice: Box::new((&**notice).into()),
            },
            link::NodeRequest::Edge(notice) => NodeRequest::Edge(Box::new((&**notice).into())),
        }
    }
}

impl TryFrom<NodeRequest> for link::NodeRequest {
    type Error = Invalid;

    fn try_from(request: NodeRequest) -> Result<Self, Invalid> {
        Ok(match request {
            NodeRequest::Partition { partition, request } => link::NodeRequest::Partition {
                partition: PartitionId::new(partition),
                request: Box::new((*request).try_into()?),
            },
            NodeRequest::Tenant {
                tenant: id,
                request,
            } => link::NodeRequest::Tenant {
                tenant: tenant(id)?,
                request: Box::new((*request).try_into()?),
            },
            NodeRequest::Owner { partition, notice } => link::NodeRequest::Owner {
                partition: PartitionId::new(partition),
                notice: Box::new((*notice).try_into()?),
            },
            NodeRequest::Edge(notice) => link::NodeRequest::Edge(Box::new((*notice).try_into()?)),
        })
    }
}

/// The answer to a [`NodeRequest`]: `{"partition":{…}}`, `{"organization":{…}}`, `"done"` or
/// `{"not_owner":{"partition":3}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeReply {
    Partition(Box<PartitionReply>),
    Tenant(TenantReply),
    Done,
    NotOwner { partition: u16 },
}

impl From<&link::NodeReply> for NodeReply {
    fn from(reply: &link::NodeReply) -> Self {
        match reply {
            link::NodeReply::Partition(reply) => NodeReply::Partition(Box::new((&**reply).into())),
            link::NodeReply::Tenant(reply) => NodeReply::Tenant(reply.into()),
            link::NodeReply::Done => NodeReply::Done,
            link::NodeReply::NotOwner { partition } => NodeReply::NotOwner {
                partition: partition.get(),
            },
        }
    }
}

impl TryFrom<NodeReply> for link::NodeReply {
    type Error = Invalid;

    fn try_from(reply: NodeReply) -> Result<Self, Invalid> {
        Ok(match reply {
            NodeReply::Partition(reply) => {
                link::NodeReply::Partition(Box::new((*reply).try_into()?))
            }
            NodeReply::Tenant(reply) => link::NodeReply::Tenant(reply.try_into()?),
            NodeReply::Done => link::NodeReply::Done,
            NodeReply::NotOwner { partition } => link::NodeReply::NotOwner {
                partition: PartitionId::new(partition),
            },
        })
    }
}

/// Why an owner refused: `{"run_not_found":"0196…"}`, `"task_gone"`, `{"quota":{…}}`, …
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Refusal {
    RunNotFound(Id),
    RunEnded(Id),
    Signal(String),
    Quota(QuotaExceeded),
    NamespaceDeleting,
    TaskGone,
    TaskNotWaiting { task: String, state: String },
    ResultTooLarge { size: u64, max: u64 },
    ParentNotStarted(Id),
    Unavailable(String),
}

/// The names of the states of a task ([`engine::TaskState::name`]), as a refusal names them.
fn task_state_name(state: &str) -> Result<&'static str, Invalid> {
    ["approval", "ready", "claimed", "deferred", "parked"]
        .into_iter()
        .find(|name| *name == state)
        .ok_or_else(|| Invalid(format!("unknown task state {state:?}")))
}

impl From<&link::Refusal> for Refusal {
    fn from(refusal: &link::Refusal) -> Self {
        match refusal {
            link::Refusal::RunNotFound(run) => Refusal::RunNotFound(Id(*run)),
            link::Refusal::RunEnded(run) => Refusal::RunEnded(Id(*run)),
            link::Refusal::Signal(message) => Refusal::Signal(message.clone()),
            link::Refusal::Quota(exceeded) => Refusal::Quota(exceeded.into()),
            link::Refusal::NamespaceDeleting => Refusal::NamespaceDeleting,
            link::Refusal::TaskGone => Refusal::TaskGone,
            link::Refusal::TaskNotWaiting { task, state } => Refusal::TaskNotWaiting {
                task: task.clone(),
                state: (*state).to_owned(),
            },
            link::Refusal::ResultTooLarge { size, max } => Refusal::ResultTooLarge {
                size: u64::try_from(*size).unwrap_or(u64::MAX),
                max: u64::try_from(*max).unwrap_or(u64::MAX),
            },
            link::Refusal::ParentNotStarted(run) => Refusal::ParentNotStarted(Id(*run)),
            link::Refusal::Unavailable(message) => Refusal::Unavailable(message.clone()),
        }
    }
}

impl TryFrom<Refusal> for link::Refusal {
    type Error = Invalid;

    fn try_from(refusal: Refusal) -> Result<Self, Invalid> {
        Ok(match refusal {
            Refusal::RunNotFound(run) => link::Refusal::RunNotFound(run.0),
            Refusal::RunEnded(run) => link::Refusal::RunEnded(run.0),
            Refusal::Signal(message) => link::Refusal::Signal(message),
            Refusal::Quota(exceeded) => link::Refusal::Quota(exceeded.into()),
            Refusal::NamespaceDeleting => link::Refusal::NamespaceDeleting,
            Refusal::TaskGone => link::Refusal::TaskGone,
            Refusal::TaskNotWaiting { task, state } => link::Refusal::TaskNotWaiting {
                task,
                state: task_state_name(&state)?,
            },
            Refusal::ResultTooLarge { size, max } => link::Refusal::ResultTooLarge {
                size: usize::try_from(size).map_err(Invalid::new)?,
                max: usize::try_from(max).map_err(Invalid::new)?,
            },
            Refusal::ParentNotStarted(run) => link::Refusal::ParentNotStarted(run.0),
            Refusal::Unavailable(message) => link::Refusal::Unavailable(message),
        })
    }
}

fn refused<T, U>(
    result: &Result<T, link::Refusal>,
    into: impl FnOnce(&T) -> U,
) -> Result<U, Refusal> {
    match result {
        Ok(value) => Ok(into(value)),
        Err(refusal) => Err(refusal.into()),
    }
}

fn refused_of<T, U>(
    result: Result<T, Refusal>,
    into: impl FnOnce(T) -> Result<U, Invalid>,
) -> Result<Result<U, link::Refusal>, Invalid> {
    match result {
        Ok(value) => Ok(Ok(into(value)?)),
        Err(refusal) => Ok(Err(refusal.try_into()?)),
    }
}

// -------------------------------------------------------------------------------------------
// What an edge asks the owner of a partition

/// `{"start":{…}}`, `{"signal":{…}}`, `{"get_run":{"run":{…}}}`, …
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PartitionRequest {
    Start(Box<StartAtOwner>),
    Signal(SignalAtOwner),
    Cancel {
        run: RunRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Release {
        run: RunRef,
    },
    GiveUp {
        run: RunRef,
    },
    Erase {
        run: RunRef,
    },
    Answer {
        tenant: String,
        token: String,
        answer: TaskAnswer,
    },
    Decide {
        task: TaskRef,
        decision: Decision,
    },
    SignalUrl {
        token: String,
        payload: JsonValue,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    GetRun {
        run: RunRef,
    },
    /// The run a handle names, written as the handle's text.
    Find {
        tenant: String,
        handle: String,
    },
    History {
        run: RunRef,
        from: u64,
        values: bool,
    },
    Tasks {
        run: RunRef,
    },
    Search {
        scope: Scope,
        query: RunQuery,
        order: RunOrder,
        page: Page,
    },
    Counters {
        scope: Scope,
        window: Window,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval: Option<NonZeroU32>,
    },
    Usage {
        scope: Scope,
        month: Month,
        days: bool,
    },
    QuotaAlerts {
        tenant: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        month: Month,
    },
    Shapes {
        tenant: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        actions: Vec<String>,
    },
}

/// A start the edge checked: `{"run":{…},"namespace":"user-1","started":<run_started, stored
/// event v1>,"metadata":<metadata record>,"start_key":…,"headline":"…","created_at":…}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartAtOwner {
    pub run: RunRef,
    pub namespace: String,
    pub origin: String,
    pub started: Box<RawValue>,
    #[serde(default, skip_serializing_if = "record::Metadata::is_empty")]
    pub metadata: record::Metadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_key: Option<String>,
    pub headline: String,
    pub created_at: u64,
}

/// `{"run":{…},"name":"go","payload":…,"key":…}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalAtOwner {
    pub run: RunRef,
    pub name: String,
    pub payload: JsonValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// `{"complete":…}`, `{"fail":{…}}` or `{"defer":{"timeout":{"secs":…,"nanos":…}}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskAnswer {
    Complete(JsonValue),
    Fail(HarnessFailure),
    Defer {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<Duration>,
    },
}

/// `{"message":"…","data":…,"retryable":false}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessFailure {
    pub message: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<JsonValue>,
    pub retryable: bool,
}

/// `"approve"` or `{"reject":"…"}` (`{"reject":null}` without a message).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Decision {
    Approve,
    Reject(Option<String>),
}

fn started_event(started: &rewind_core::RunStarted) -> Box<RawValue> {
    let event =
        rewind_format::encode_event(&Event::Lifecycle(Lifecycle::RunStarted(started.clone())));
    RawValue::from_string(event).expect("a stored event is JSON")
}

fn started_of(started: &RawValue) -> Result<rewind_core::RunStarted, Invalid> {
    match rewind_format::decode_event(started.get()).map_err(Invalid::new)? {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Ok(started),
        other => Err(Invalid(format!("a start holds {other:?}, not run_started"))),
    }
}

impl From<&link::PartitionRequest> for PartitionRequest {
    fn from(request: &link::PartitionRequest) -> Self {
        use link::PartitionRequest as R;
        match request {
            R::Start(start) => PartitionRequest::Start(Box::new(StartAtOwner {
                run: (&start.run).into(),
                namespace: start.namespace.to_string(),
                origin: start.origin.clone(),
                started: started_event(&start.started),
                metadata: codec::metadata_record(&start.metadata),
                start_key: start.start_key.clone(),
                headline: start.headline.clone(),
                created_at: start.created_at.0,
            })),
            R::Signal(signal) => PartitionRequest::Signal(SignalAtOwner {
                run: (&signal.run).into(),
                name: signal.name.clone(),
                payload: signal.payload.clone(),
                key: signal.key.clone(),
            }),
            R::Cancel { run, message } => PartitionRequest::Cancel {
                run: run.into(),
                message: message.clone(),
            },
            R::Release { run } => PartitionRequest::Release { run: run.into() },
            R::GiveUp { run } => PartitionRequest::GiveUp { run: run.into() },
            R::Erase { run } => PartitionRequest::Erase { run: run.into() },
            R::Answer {
                tenant,
                token,
                answer,
            } => PartitionRequest::Answer {
                tenant: tenant.to_string(),
                token: token.0.clone(),
                answer: match answer {
                    link::TaskAnswer::Complete(value) => TaskAnswer::Complete(value.clone()),
                    link::TaskAnswer::Fail(failure) => TaskAnswer::Fail(HarnessFailure {
                        message: failure.message.clone(),
                        data: failure.data.clone(),
                        retryable: failure.retryable,
                    }),
                    link::TaskAnswer::Defer { timeout } => TaskAnswer::Defer { timeout: *timeout },
                },
            },
            R::Decide { task, decision } => PartitionRequest::Decide {
                task: task.into(),
                decision: match decision {
                    link::Decision::Approve => Decision::Approve,
                    link::Decision::Reject(message) => Decision::Reject(message.clone()),
                },
            },
            R::SignalUrl {
                token,
                payload,
                key,
            } => PartitionRequest::SignalUrl {
                token: token.clone(),
                payload: payload.clone(),
                key: key.clone(),
            },
            R::GetRun { run } => PartitionRequest::GetRun { run: run.into() },
            R::Find { tenant, handle } => PartitionRequest::Find {
                tenant: tenant.to_string(),
                handle: handle.to_string(),
            },
            R::History { run, from, values } => PartitionRequest::History {
                run: run.into(),
                from: *from,
                values: *values,
            },
            R::Tasks { run } => PartitionRequest::Tasks { run: run.into() },
            R::Search {
                scope,
                query,
                order,
                page,
            } => PartitionRequest::Search {
                scope: scope.into(),
                query: query.into(),
                order: (*order).into(),
                page: page.into(),
            },
            R::Counters {
                scope,
                window,
                interval,
            } => PartitionRequest::Counters {
                scope: scope.into(),
                window: (*window).into(),
                interval: *interval,
            },
            R::Usage { scope, month, days } => PartitionRequest::Usage {
                scope: scope.into(),
                month: (*month).into(),
                days: *days,
            },
            R::QuotaAlerts {
                tenant,
                namespace,
                month,
            } => PartitionRequest::QuotaAlerts {
                tenant: tenant.to_string(),
                namespace: namespace.as_ref().map(ToString::to_string),
                month: (*month).into(),
            },
            R::Shapes {
                tenant,
                namespace,
                actions,
            } => PartitionRequest::Shapes {
                tenant: tenant.to_string(),
                namespace: namespace.as_ref().map(ToString::to_string),
                actions: actions.clone(),
            },
        }
    }
}

impl TryFrom<PartitionRequest> for link::PartitionRequest {
    type Error = Invalid;

    fn try_from(request: PartitionRequest) -> Result<Self, Invalid> {
        use link::PartitionRequest as R;
        Ok(match request {
            PartitionRequest::Start(start) => R::Start(Box::new(link::StartAtOwner {
                run: start.run.try_into()?,
                namespace: namespace(start.namespace)?,
                origin: start.origin,
                started: started_of(&start.started)?,
                metadata: codec::metadata_of(start.metadata),
                start_key: start.start_key,
                headline: start.headline,
                created_at: Timestamp(start.created_at),
            })),
            PartitionRequest::Signal(signal) => R::Signal(link::SignalAtOwner {
                run: signal.run.try_into()?,
                name: signal.name,
                payload: signal.payload,
                key: signal.key,
            }),
            PartitionRequest::Cancel { run, message } => R::Cancel {
                run: run.try_into()?,
                message,
            },
            PartitionRequest::Release { run } => R::Release {
                run: run.try_into()?,
            },
            PartitionRequest::GiveUp { run } => R::GiveUp {
                run: run.try_into()?,
            },
            PartitionRequest::Erase { run } => R::Erase {
                run: run.try_into()?,
            },
            PartitionRequest::Answer {
                tenant: id,
                token,
                answer,
            } => R::Answer {
                tenant: tenant(id)?,
                token: engine::TaskToken(token),
                answer: match answer {
                    TaskAnswer::Complete(value) => link::TaskAnswer::Complete(value),
                    TaskAnswer::Fail(failure) => link::TaskAnswer::Fail(link::HarnessFailure {
                        message: failure.message,
                        data: failure.data,
                        retryable: failure.retryable,
                    }),
                    TaskAnswer::Defer { timeout } => link::TaskAnswer::Defer { timeout },
                },
            },
            PartitionRequest::Decide { task, decision } => R::Decide {
                task: task.try_into()?,
                decision: match decision {
                    Decision::Approve => link::Decision::Approve,
                    Decision::Reject(message) => link::Decision::Reject(message),
                },
            },
            PartitionRequest::SignalUrl {
                token,
                payload,
                key,
            } => R::SignalUrl {
                token,
                payload,
                key,
            },
            PartitionRequest::GetRun { run } => R::GetRun {
                run: run.try_into()?,
            },
            PartitionRequest::Find { tenant: id, handle } => R::Find {
                tenant: tenant(id)?,
                handle: handle.parse().map_err(Invalid::new)?,
            },
            PartitionRequest::History { run, from, values } => R::History {
                run: run.try_into()?,
                from,
                values,
            },
            PartitionRequest::Tasks { run } => R::Tasks {
                run: run.try_into()?,
            },
            PartitionRequest::Search {
                scope,
                query,
                order,
                page,
            } => R::Search {
                scope: scope.try_into()?,
                query: query.try_into()?,
                order: order.into(),
                page: page.try_into()?,
            },
            PartitionRequest::Counters {
                scope,
                window,
                interval,
            } => R::Counters {
                scope: scope.try_into()?,
                window: window.into(),
                interval,
            },
            PartitionRequest::Usage { scope, month, days } => R::Usage {
                scope: scope.try_into()?,
                month: month.try_into()?,
                days,
            },
            PartitionRequest::QuotaAlerts {
                tenant: id,
                namespace: name,
                month,
            } => R::QuotaAlerts {
                tenant: tenant(id)?,
                namespace: name.map(namespace).transpose()?,
                month: month.try_into()?,
            },
            PartitionRequest::Shapes {
                tenant: id,
                namespace: name,
                actions,
            } => R::Shapes {
                tenant: tenant(id)?,
                namespace: name.map(namespace).transpose()?,
                actions,
            },
        })
    }
}

/// What the owner of a partition answers: `{"started":{"Ok":{…}}}`, `{"run":{"Err":{…}}}`, …
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PartitionReply {
    Started(Result<Created, Refusal>),
    Enqueued(Result<Enqueued, Refusal>),
    Done(Result<bool, Refusal>),
    Answered(Result<(), Refusal>),
    Run(Result<Option<Box<RunView>>, Refusal>),
    History(Result<Option<Vec<record::HistoryEvent>>, Refusal>),
    Tasks(Result<Option<Vec<record::Task>>, Refusal>),
    Found(Result<Found, Refusal>),
    Counted(Result<Box<Counted>, Refusal>),
    Shapes(Result<BTreeMap<String, record::Shape>, Refusal>),
    Used(Result<BTreeMap<String, Usage>, Refusal>),
    Alerts(Result<Vec<record::QuotaAlert>, Refusal>),
}

/// `{"run":{…},"created":true}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Created {
    pub run: RunRef,
    pub created: bool,
}

/// `"accepted"`, `"duplicate"` or `"run_ended"`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enqueued {
    Accepted,
    Duplicate,
    RunEnded,
}

/// A partition's counters over a window, one per interval, and its gauges:
/// `{"counters":[{…},…],"gauges":<gauges record>}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counted {
    pub counters: Vec<Counters>,
    pub gauges: record::Gauges,
}

impl From<&link::PartitionReply> for PartitionReply {
    fn from(reply: &link::PartitionReply) -> Self {
        use link::PartitionReply as R;
        match reply {
            R::Started(result) => PartitionReply::Started(refused(result, |created| Created {
                run: (&created.run).into(),
                created: created.created,
            })),
            R::Enqueued(result) => {
                PartitionReply::Enqueued(refused(result, |enqueued| match enqueued {
                    engine::Enqueued::Accepted => Enqueued::Accepted,
                    engine::Enqueued::Duplicate => Enqueued::Duplicate,
                    engine::Enqueued::RunEnded => Enqueued::RunEnded,
                }))
            }
            R::Done(result) => PartitionReply::Done(refused(result, |done| *done)),
            R::Answered(result) => PartitionReply::Answered(refused(result, |_| ())),
            R::Run(result) => PartitionReply::Run(refused(result, |view| {
                view.as_ref().map(|view| Box::new(view.into()))
            })),
            R::History(result) => PartitionReply::History(refused(result, |events| {
                events
                    .as_ref()
                    .map(|events| events.iter().map(codec::event_record).collect())
            })),
            R::Tasks(result) => PartitionReply::Tasks(refused(result, |tasks| {
                tasks
                    .as_ref()
                    .map(|tasks| tasks.iter().map(codec::task_record).collect())
            })),
            R::Found(result) => PartitionReply::Found(refused(result, |found| found.into())),
            R::Counted(result) => PartitionReply::Counted(refused(result, |(counters, gauges)| {
                Box::new(Counted {
                    counters: counters.iter().map(Into::into).collect(),
                    gauges: codec::gauges_record(gauges),
                })
            })),
            R::Used(result) => PartitionReply::Used(refused(result, |used| {
                used.iter()
                    .map(|(name, usage)| (name.to_string(), usage.into()))
                    .collect()
            })),
            R::Alerts(result) => PartitionReply::Alerts(refused(result, |alerts| {
                alerts.iter().map(codec::alert_record).collect()
            })),
            R::Shapes(result) => PartitionReply::Shapes(refused(result, |shapes| {
                shapes
                    .iter()
                    .map(|(action, shape)| {
                        (
                            action.clone(),
                            record::Shape {
                                shape: shape.clone(),
                            },
                        )
                    })
                    .collect()
            })),
        }
    }
}

impl TryFrom<PartitionReply> for link::PartitionReply {
    type Error = Invalid;

    fn try_from(reply: PartitionReply) -> Result<Self, Invalid> {
        use link::PartitionReply as R;
        Ok(match reply {
            PartitionReply::Started(result) => R::Started(refused_of(result, |created| {
                Ok(engine::Created {
                    run: created.run.try_into()?,
                    created: created.created,
                })
            })?),
            PartitionReply::Enqueued(result) => R::Enqueued(refused_of(result, |enqueued| {
                Ok(match enqueued {
                    Enqueued::Accepted => engine::Enqueued::Accepted,
                    Enqueued::Duplicate => engine::Enqueued::Duplicate,
                    Enqueued::RunEnded => engine::Enqueued::RunEnded,
                })
            })?),
            PartitionReply::Done(result) => R::Done(refused_of(result, Ok)?),
            PartitionReply::Answered(result) => R::Answered(refused_of(result, Ok)?),
            PartitionReply::Run(result) => R::Run(refused_of(result, |view| {
                view.map(|view| (*view).try_into()).transpose()
            })?),
            PartitionReply::History(result) => R::History(refused_of(result, |events| {
                Ok(events.map(|events| events.into_iter().map(codec::event_of).collect()))
            })?),
            PartitionReply::Tasks(result) => R::Tasks(refused_of(result, |tasks| {
                tasks
                    .map(|tasks| {
                        tasks
                            .into_iter()
                            .map(|task| codec::task_of(task).map_err(Invalid::from))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .transpose()
            })?),
            PartitionReply::Found(result) => R::Found(refused_of(result, TryInto::try_into)?),
            PartitionReply::Counted(result) => R::Counted(refused_of(result, |counted| {
                Ok((
                    counted
                        .counters
                        .into_iter()
                        .map(TryInto::try_into)
                        .collect::<Result<_, Invalid>>()?,
                    codec::gauges_of(counted.gauges),
                ))
            })?),
            PartitionReply::Used(result) => R::Used(refused_of(result, |used| {
                used.into_iter()
                    .map(|(name, usage)| Ok((namespace(name)?, usage.into())))
                    .collect()
            })?),
            PartitionReply::Alerts(result) => R::Alerts(refused_of(result, |alerts| {
                alerts
                    .into_iter()
                    .map(|alert| codec::alert_of(alert).map_err(Invalid::from))
                    .collect()
            })?),
            PartitionReply::Shapes(result) => R::Shapes(refused_of(result, |shapes| {
                Ok(shapes
                    .into_iter()
                    .map(|(action, shape)| (action, shape.shape))
                    .collect())
            })?),
        })
    }
}

// -------------------------------------------------------------------------------------------
// The tenant actor

/// `{"admit":{…}}`, `{"counts":{…}}`, `{"slot":{…}}`, `{"slot_freed":{…}}`,
/// `{"harness_task":{…}}`, `{"harness_task_done":{…}}`, `{"attach":{…}}` or `{"detach":{…}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TenantRequest {
    Admit(Admit),
    Counts(CountsReport),
    Slot(SlotRequest),
    SlotFreed { task: TaskRef, server: String },
    HarnessTask(Box<HarnessTaskOffer>),
    HarnessTaskDone { task: TaskRef, gone: bool },
    Attach(StreamAttach),
    Detach { stream: String },
}

/// `{"partition":3,"generation":7,"run":"0196…","namespace":"user-1","code":"<hex>","at":…}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admit {
    pub partition: u16,
    pub generation: u64,
    pub run: Id,
    pub namespace: String,
    pub code: Hex,
    pub at: u64,
}

/// `{"partition":3,"generation":7,"namespace":"user-1","gauges":<gauges record>,
/// "created_this_month":4,"cpu_this_month":{"secs":…,"nanos":…},"included":[…],
/// "abandoned":[…],"at":…}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountsReport {
    pub partition: u16,
    pub generation: u64,
    pub namespace: String,
    pub gauges: record::Gauges,
    pub created_this_month: u64,
    pub cpu_this_month: Duration,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included: Vec<Id>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub abandoned: Vec<Id>,
    pub at: u64,
}

/// `{"partition":3,"generation":7,"task":{…},"server":"github","limit":4}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotRequest {
    pub partition: u16,
    pub generation: u64,
    pub task: TaskRef,
    pub server: String,
    pub limit: u32,
}

/// `{"partition":3,"generation":7,"task":{…},"action_set":"<hex>","grace":{"secs":60,"nanos":0}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessTaskOffer {
    pub partition: u16,
    pub generation: u64,
    pub task: HarnessTask,
    pub action_set: Hex,
    pub grace: Duration,
}

/// A harness call as an SDK worker receives it: `{"token":"rewind_tk_…","task":{…},
/// "namespace":"user-1","action":"…","args":…,"idempotency_key":"…","delivery":1,
/// "execution_timeout":{…},"labels":{…},"trace_context":{…}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessTask {
    pub token: String,
    pub task: TaskRef,
    pub namespace: String,
    pub action: String,
    pub args: JsonValue,
    pub idempotency_key: String,
    pub delivery: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_timeout: Option<Duration>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<record::TraceContext>,
}

/// An SDK stream opened at an edge: `{"stream":"…","worker":"…","node":"node-a",
/// "namespace":…,"labels":{…},"action_set":"<hex>","actions":[…],"slots":4}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamAttach {
    pub stream: String,
    pub worker: String,
    pub node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_set: Option<Hex>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub actions: BTreeSet<String>,
    pub slots: u32,
}

/// `{"admission":{"Ok":null}}` or `{"admission":{"Err":{…}}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TenantReply {
    Admission(Result<(), Refusal>),
}

impl From<&link::TenantReply> for TenantReply {
    fn from(reply: &link::TenantReply) -> Self {
        match reply {
            link::TenantReply::Admission(result) => TenantReply::Admission(refused(result, |_| ())),
        }
    }
}

impl TryFrom<TenantReply> for link::TenantReply {
    type Error = Invalid;

    fn try_from(reply: TenantReply) -> Result<Self, Invalid> {
        Ok(match reply {
            TenantReply::Admission(result) => link::TenantReply::Admission(refused_of(result, Ok)?),
        })
    }
}

impl From<&link::HarnessTask> for HarnessTask {
    fn from(task: &link::HarnessTask) -> Self {
        Self {
            token: task.token.0.clone(),
            task: (&task.task).into(),
            namespace: task.namespace.to_string(),
            action: task.action.clone(),
            args: task.args.clone(),
            idempotency_key: task.idempotency_key.clone(),
            delivery: task.delivery,
            execution_timeout: task.execution_timeout,
            labels: task.labels.clone(),
            trace_context: task
                .trace_context
                .as_ref()
                .map(|context| record::TraceContext {
                    traceparent: context.traceparent.clone(),
                    tracestate: context.tracestate.clone(),
                }),
        }
    }
}

impl TryFrom<HarnessTask> for link::HarnessTask {
    type Error = Invalid;

    fn try_from(task: HarnessTask) -> Result<Self, Invalid> {
        Ok(link::HarnessTask {
            token: engine::TaskToken(task.token),
            task: task.task.try_into()?,
            namespace: namespace(task.namespace)?,
            action: task.action,
            args: task.args,
            idempotency_key: task.idempotency_key,
            delivery: task.delivery,
            execution_timeout: task.execution_timeout,
            labels: task.labels,
            trace_context: task.trace_context.map(|context| engine::TraceContext {
                traceparent: context.traceparent,
                tracestate: context.tracestate,
            }),
        })
    }
}

impl From<&link::StreamAttach> for StreamAttach {
    fn from(attach: &link::StreamAttach) -> Self {
        Self {
            stream: attach.stream.0.clone(),
            worker: attach.worker.clone(),
            node: attach.node.to_string(),
            namespace: attach.namespace.as_ref().map(ToString::to_string),
            labels: attach.labels.clone(),
            action_set: attach.action_set.map(Hex),
            actions: attach.actions.clone(),
            slots: attach.slots,
        }
    }
}

impl TryFrom<StreamAttach> for link::StreamAttach {
    type Error = Invalid;

    fn try_from(attach: StreamAttach) -> Result<Self, Invalid> {
        Ok(link::StreamAttach {
            stream: link::StreamId(attach.stream),
            worker: attach.worker,
            node: node(attach.node)?,
            namespace: attach.namespace.map(namespace).transpose()?,
            labels: attach.labels,
            action_set: attach.action_set.map(|hex| hex.0),
            actions: attach.actions,
            slots: attach.slots,
        })
    }
}

impl From<&link::TenantRequest> for TenantRequest {
    fn from(request: &link::TenantRequest) -> Self {
        use link::TenantRequest as R;
        match request {
            R::Admit(admit) => TenantRequest::Admit(Admit {
                partition: admit.partition.get(),
                generation: admit.generation.0,
                run: Id(admit.run),
                namespace: admit.namespace.to_string(),
                code: Hex(admit.code),
                at: admit.at.0,
            }),
            R::Counts(report) => TenantRequest::Counts(CountsReport {
                partition: report.partition.get(),
                generation: report.generation.0,
                namespace: report.namespace.to_string(),
                gauges: codec::gauges_record(&report.gauges),
                created_this_month: report.created_this_month,
                cpu_this_month: report.cpu_this_month,
                included: report.included.iter().copied().map(Id).collect(),
                abandoned: report.abandoned.iter().copied().map(Id).collect(),
                at: report.at.0,
            }),
            R::Slot(slot) => TenantRequest::Slot(SlotRequest {
                partition: slot.partition.get(),
                generation: slot.generation.0,
                task: (&slot.task).into(),
                server: slot.server.clone(),
                limit: slot.limit,
            }),
            R::SlotFreed { task, server } => TenantRequest::SlotFreed {
                task: task.into(),
                server: server.clone(),
            },
            R::HarnessTask(offer) => TenantRequest::HarnessTask(Box::new(HarnessTaskOffer {
                partition: offer.partition.get(),
                generation: offer.generation.0,
                task: (&offer.task).into(),
                action_set: Hex(offer.action_set),
                grace: offer.grace,
            })),
            R::HarnessTaskDone { task, gone } => TenantRequest::HarnessTaskDone {
                task: task.into(),
                gone: *gone,
            },
            R::Attach(attach) => TenantRequest::Attach(attach.into()),
            R::Detach { stream } => TenantRequest::Detach {
                stream: stream.0.clone(),
            },
        }
    }
}

impl TryFrom<TenantRequest> for link::TenantRequest {
    type Error = Invalid;

    fn try_from(request: TenantRequest) -> Result<Self, Invalid> {
        use link::TenantRequest as R;
        Ok(match request {
            TenantRequest::Admit(admit) => R::Admit(link::Admit {
                partition: PartitionId::new(admit.partition),
                generation: engine::PartitionGeneration(admit.generation),
                run: admit.run.0,
                namespace: namespace(admit.namespace)?,
                code: admit.code.0,
                at: Timestamp(admit.at),
            }),
            TenantRequest::Counts(report) => R::Counts(link::CountsReport {
                partition: PartitionId::new(report.partition),
                generation: engine::PartitionGeneration(report.generation),
                namespace: namespace(report.namespace)?,
                gauges: codec::gauges_of(report.gauges),
                created_this_month: report.created_this_month,
                cpu_this_month: report.cpu_this_month,
                included: report.included.into_iter().map(|id| id.0).collect(),
                abandoned: report.abandoned.into_iter().map(|id| id.0).collect(),
                at: Timestamp(report.at),
            }),
            TenantRequest::Slot(slot) => R::Slot(link::SlotRequest {
                partition: PartitionId::new(slot.partition),
                generation: engine::PartitionGeneration(slot.generation),
                task: slot.task.try_into()?,
                server: slot.server,
                limit: slot.limit,
            }),
            TenantRequest::SlotFreed { task, server } => R::SlotFreed {
                task: task.try_into()?,
                server,
            },
            TenantRequest::HarnessTask(offer) => R::HarnessTask(Box::new(link::HarnessTaskOffer {
                partition: PartitionId::new(offer.partition),
                generation: engine::PartitionGeneration(offer.generation),
                task: offer.task.try_into()?,
                action_set: offer.action_set.0,
                grace: offer.grace,
            })),
            TenantRequest::HarnessTaskDone { task, gone } => R::HarnessTaskDone {
                task: task.try_into()?,
                gone,
            },
            TenantRequest::Attach(attach) => R::Attach(attach.try_into()?),
            TenantRequest::Detach { stream } => R::Detach {
                stream: link::StreamId(stream),
            },
        })
    }
}

// -------------------------------------------------------------------------------------------
// Notices

/// What an tenant actor tells an owner: `{"slot_granted":{"task":{…}}}`,
/// `{"stream_lost":{"tasks":[{"task":{…},"token":"…"},…]}}` or
/// `{"counts_wanted":{"tenant":"acme"}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OwnerNotice {
    SlotGranted { task: TaskRef },
    StreamLost { tasks: Vec<LostTask> },
    CountsWanted { tenant: String },
}

/// A harness task handed on a stream that did not come back, and the token of that hand-out.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LostTask {
    pub task: TaskRef,
    pub token: String,
}

impl From<&link::OwnerNotice> for OwnerNotice {
    fn from(notice: &link::OwnerNotice) -> Self {
        match notice {
            link::OwnerNotice::SlotGranted { task } => {
                OwnerNotice::SlotGranted { task: task.into() }
            }
            link::OwnerNotice::StreamLost { tasks } => OwnerNotice::StreamLost {
                tasks: tasks
                    .iter()
                    .map(|(task, token)| LostTask {
                        task: task.into(),
                        token: token.0.clone(),
                    })
                    .collect(),
            },
            link::OwnerNotice::CountsWanted { tenant } => OwnerNotice::CountsWanted {
                tenant: tenant.to_string(),
            },
        }
    }
}

impl TryFrom<OwnerNotice> for link::OwnerNotice {
    type Error = Invalid;

    fn try_from(notice: OwnerNotice) -> Result<Self, Invalid> {
        Ok(match notice {
            OwnerNotice::SlotGranted { task } => link::OwnerNotice::SlotGranted {
                task: task.try_into()?,
            },
            OwnerNotice::StreamLost { tasks } => link::OwnerNotice::StreamLost {
                tasks: tasks
                    .into_iter()
                    .map(|lost| Ok((lost.task.try_into()?, engine::TaskToken(lost.token))))
                    .collect::<Result<_, Invalid>>()?,
            },
            OwnerNotice::CountsWanted { tenant: id } => link::OwnerNotice::CountsWanted {
                tenant: tenant(id)?,
            },
        })
    }
}

/// What an tenant actor tells the edge holding an SDK stream:
/// `{"hand_out":{"stream":"…","task":{…}}}` or `{"gone":{"stream":"…","token":"…"}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum EdgeNotice {
    HandOut {
        stream: String,
        task: Box<HarnessTask>,
    },
    Gone {
        stream: String,
        token: String,
    },
    Reopened {
        partition: u16,
    },
}

impl From<&link::EdgeNotice> for EdgeNotice {
    fn from(notice: &link::EdgeNotice) -> Self {
        match notice {
            link::EdgeNotice::HandOut { stream, task } => EdgeNotice::HandOut {
                stream: stream.0.clone(),
                task: Box::new(task.into()),
            },
            link::EdgeNotice::Gone { stream, token } => EdgeNotice::Gone {
                stream: stream.0.clone(),
                token: token.0.clone(),
            },
            link::EdgeNotice::Reopened { partition } => EdgeNotice::Reopened {
                partition: partition.get(),
            },
        }
    }
}

impl TryFrom<EdgeNotice> for link::EdgeNotice {
    type Error = Invalid;

    fn try_from(notice: EdgeNotice) -> Result<Self, Invalid> {
        Ok(match notice {
            EdgeNotice::HandOut { stream, task } => link::EdgeNotice::HandOut {
                stream: link::StreamId(stream),
                task: (*task).try_into()?,
            },
            EdgeNotice::Gone { stream, token } => link::EdgeNotice::Gone {
                stream: link::StreamId(stream),
                token: engine::TaskToken(token),
            },
            EdgeNotice::Reopened { partition } => link::EdgeNotice::Reopened {
                partition: PartitionId::new(partition),
            },
        })
    }
}

// -------------------------------------------------------------------------------------------
// Subscriptions

/// What an edge subscribes to: `{"run":{…}}`, `{"namespace":{"tenant":"acme",
/// "namespace":"user-1"}}` or `{"tenant":"acme"}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SubscriptionKey {
    Run(RunRef),
    Namespace { tenant: String, namespace: String },
    Tenant(String),
}

impl From<&link::SubscriptionKey> for SubscriptionKey {
    fn from(key: &link::SubscriptionKey) -> Self {
        match key {
            link::SubscriptionKey::Run(run) => SubscriptionKey::Run(run.into()),
            link::SubscriptionKey::Namespace { tenant, namespace } => SubscriptionKey::Namespace {
                tenant: tenant.to_string(),
                namespace: namespace.to_string(),
            },
            link::SubscriptionKey::Tenant(tenant) => SubscriptionKey::Tenant(tenant.to_string()),
        }
    }
}

impl TryFrom<SubscriptionKey> for link::SubscriptionKey {
    type Error = Invalid;

    fn try_from(key: SubscriptionKey) -> Result<Self, Invalid> {
        Ok(match key {
            SubscriptionKey::Run(run) => link::SubscriptionKey::Run(run.try_into()?),
            SubscriptionKey::Namespace {
                tenant: id,
                namespace: name,
            } => link::SubscriptionKey::Namespace {
                tenant: tenant(id)?,
                namespace: namespace(name)?,
            },
            SubscriptionKey::Tenant(id) => link::SubscriptionKey::Tenant(tenant(id)?),
        })
    }
}

/// A subscription asked of a node: `{"key":{…},"from":0}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscribe {
    pub key: SubscriptionKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u64>,
}

/// A run changed: `{"run":{…},"namespace":"user-1","summary":<summary record>,
/// "events":[<event record>,…]}`, the summary left out once the run is erased.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Changed {
    pub run: RunRef,
    pub namespace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<record::Summary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<record::HistoryEvent>,
}

impl From<&link::Changed> for Changed {
    fn from(changed: &link::Changed) -> Self {
        Self {
            run: (&changed.run).into(),
            namespace: changed.namespace.to_string(),
            summary: changed.summary.as_ref().map(summary_record),
            events: changed.events.iter().map(codec::event_record).collect(),
        }
    }
}

impl TryFrom<Changed> for link::Changed {
    type Error = Invalid;

    fn try_from(changed: Changed) -> Result<Self, Invalid> {
        Ok(link::Changed {
            run: changed.run.try_into()?,
            namespace: namespace(changed.namespace)?,
            summary: changed.summary.map(summary_of).transpose()?,
            events: changed.events.into_iter().map(codec::event_of).collect(),
        })
    }
}
