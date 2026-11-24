//! An activation sent to a remote executor, and what it did (`archi/06-storage-and-scale.md`,
//! "Partitions, owners and nodes": executors).

use std::time::Duration;

use rewind_core::{JsonValue, PartitionId, Script, Seq};
use rewind_engine::metrics::RunMeasurement;
use rewind_engine::ports as engine;
use rewind_engine::ports::link;
use rewind_engine::ports::partition as store;
use rewind_format::partition as record;
use rewind_store_slatedb::codec;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::values::{Counters, RunRef};
use super::{Id, Invalid, namespace, present, tenant};

fn all<T, U, E>(items: Vec<T>, into: impl Fn(T) -> Result<U, E>) -> Result<Vec<U>, Invalid>
where
    Invalid: From<E>,
{
    items
        .into_iter()
        .map(|item| into(item).map_err(Invalid::from))
        .collect()
}

/// An activation an owner sends: `{"partition":3,"head":<run head record>,"summary":<summary
/// record>,"inbox":[<inbox item record>,…],"history":[<history entry record>,…],
/// "payloads":[<payload record>,…],"parent":{…},"script":{…},"action_set":{…},
/// "purpose":"runnable","tasks":[<tool task record>,…]}`; `history` left out when the owner
/// expects the executor's instance cache to hold the run.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationRequest {
    pub partition: u16,
    pub head: record::RunHead,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<record::Summary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inbox: Vec<record::InboxItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<record::Entry>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payloads: Vec<record::PayloadValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentHistory>,
    pub script: StoredScript,
    pub action_set: StoredActionSet,
    pub purpose: Purpose,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<record::Task>,
}

/// The history a fork follows: `{"run":"0196…","entries":[…],"payloads":[…],"sealed":{…}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentHistory {
    pub run: Id,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<record::Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payloads: Vec<record::PayloadValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<Box<SealedAttempt>>,
}

/// A sealed attempt: `{"tenant":"acme","attempt":1,"journal":<journal document v1>}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedAttempt {
    pub tenant: String,
    pub attempt: u32,
    pub journal: Box<RawValue>,
}

/// A script as stored: `{"source":"…","javascript":"…","source_map":"…"}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredScript {
    pub source: String,
    pub javascript: String,
    pub source_map: String,
}

/// A pinned action set as stored: `{"action_set":<pinned action_set document v1>,"typescript":"…",
/// "prompt":"…"}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredActionSet {
    pub action_set: Box<RawValue>,
    pub typescript: String,
    pub prompt: String,
}

/// `"runnable"`, `"quarantine_retry"` or `"give_up"`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    Runnable,
    QuarantineRetry,
    GiveUp,
}

/// What an activation did: `{"committed":{…}}`, `{"quarantined":{"kind":"failures",
/// "reason":"…"}}`, `"released"`, `{"failed":{"error":"…"}}` or `"miss"`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ActivationResult {
    Committed(Box<Committed>),
    Quarantined {
        kind: record::QuarantineKind,
        reason: String,
    },
    Released,
    Failed {
        error: String,
    },
    Miss,
}

/// An activation to commit: `{"activation":{…},"projections":[<projection record>,…],
/// "end":…,"children":[{"seq":1,"run":"0196…","attached":true},…],"copied":false,
/// "cpu":{"secs":…,"nanos":…},"mode":"live","measurements":["started",…]}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Committed {
    pub activation: Activation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projections: Vec<record::Projection>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub end: Option<JsonValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Child>,
    pub copied: bool,
    pub cpu: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<record::Mode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub measurements: Vec<Measurement>,
}

/// A run metric of the new entries: `"started"`, `{"ended":{"outcome":"completed"}}` or
/// `{"tool_call":{"tool":"…","outcome":"failed","error_kind":"timeout","seconds":1.5}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Measurement {
    Started,
    Ended {
        outcome: String,
    },
    ToolCall {
        tool: String,
        outcome: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_kind: Option<String>,
        seconds: f64,
    },
}

impl From<&RunMeasurement> for Measurement {
    fn from(measurement: &RunMeasurement) -> Self {
        match measurement {
            RunMeasurement::Started => Measurement::Started,
            RunMeasurement::Ended { outcome } => Measurement::Ended {
                outcome: outcome.clone(),
            },
            RunMeasurement::ToolCall {
                tool,
                outcome,
                error_kind,
                seconds,
            } => Measurement::ToolCall {
                tool: tool.clone(),
                outcome: outcome.clone(),
                error_kind: error_kind.clone(),
                seconds: *seconds,
            },
        }
    }
}

impl From<Measurement> for RunMeasurement {
    fn from(measurement: Measurement) -> Self {
        match measurement {
            Measurement::Started => RunMeasurement::Started,
            Measurement::Ended { outcome } => RunMeasurement::Ended { outcome },
            Measurement::ToolCall {
                tool,
                outcome,
                error_kind,
                seconds,
            } => RunMeasurement::ToolCall {
                tool,
                outcome,
                error_kind,
                seconds,
            },
        }
    }
}

/// A run the attempt started and whose end its history does not hold.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Child {
    pub seq: u32,
    pub run: Id,
    pub attached: bool,
}

/// Everything one activation commits: `{"run":{…},"attempt":1,"entries":[…],"consumed":[…],
/// "payloads":[…],"tasks":[…],"timers":[…],"head":<run head record>,"summary":<summary
/// record>,"counters":[…],"outbox":[<outbox entry record>,…]}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub run: RunRef,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<record::Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consumed: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payloads: Vec<record::PayloadValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<record::Task>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timers: Vec<TimerChange>,
    pub head: record::RunHead,
    pub summary: record::Summary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub counters: Vec<CounterDelta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outbox: Vec<record::Outbox>,
}

/// `{"set":<timer record>}` or `{"cancel":{"tenant":"acme","kind":<timer kind of the record>}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TimerChange {
    Set(record::Timer),
    Cancel {
        tenant: String,
        kind: record::TimerKind,
    },
}

/// `{"hour":{"tenant":"acme","namespace":"user-1","hour":…,"counts":{…}}}` or
/// `{"shape":{"tenant":"acme","namespace":"user-1","tool":"…","shape":<shape record>}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CounterDelta {
    Hour {
        tenant: String,
        namespace: String,
        hour: u32,
        counts: Box<Counters>,
    },
    Shape {
        tenant: String,
        namespace: String,
        tool: String,
        shape: record::Shape,
    },
}

// -------------------------------------------------------------------------------------------
// Conversions

impl From<&link::ActivationRequest> for ActivationRequest {
    fn from(request: &link::ActivationRequest) -> Self {
        Self {
            partition: request.partition.get(),
            head: codec::head_record(&request.head),
            summary: request.summary.as_ref().map(codec::summary_record),
            inbox: request.inbox.iter().map(codec::inbox_record).collect(),
            history: request
                .history
                .as_ref()
                .map(|history| history.iter().map(codec::entry_record).collect()),
            payloads: request.payloads.iter().map(codec::payload_record).collect(),
            parent: request.parent.as_ref().map(|parent| ParentHistory {
                run: Id(parent.run),
                entries: parent.entries.iter().map(codec::entry_record).collect(),
                payloads: parent.payloads.iter().map(codec::payload_record).collect(),
                sealed: parent
                    .sealed
                    .as_ref()
                    .map(|sealed| Box::new((&**sealed).into())),
            }),
            script: StoredScript {
                source: request.script.script.source().to_owned(),
                javascript: request.script.compiled.javascript.clone(),
                source_map: request.script.compiled.source_map.clone(),
            },
            action_set: StoredActionSet {
                action_set: RawValue::from_string(rewind_format::encode_action_set(
                    &request.action_set.action_set,
                ))
                .expect("a pinned action set document is JSON"),
                typescript: request.action_set.declarations.typescript.clone(),
                prompt: request.action_set.declarations.prompt.clone(),
            },
            purpose: match request.purpose {
                link::Purpose::Runnable => Purpose::Runnable,
                link::Purpose::QuarantineRetry => Purpose::QuarantineRetry,
                link::Purpose::GiveUp => Purpose::GiveUp,
            },
            tasks: request.tasks.iter().map(codec::task_record).collect(),
        }
    }
}

impl TryFrom<ActivationRequest> for link::ActivationRequest {
    type Error = Invalid;

    fn try_from(request: ActivationRequest) -> Result<Self, Invalid> {
        Ok(link::ActivationRequest {
            partition: PartitionId::new(request.partition),
            head: codec::head_of(request.head)?,
            summary: request.summary.map(codec::summary_of).transpose()?,
            inbox: all(request.inbox, codec::inbox_of)?,
            history: request
                .history
                .map(|history| history.into_iter().map(codec::entry_of).collect()),
            payloads: request
                .payloads
                .into_iter()
                .map(codec::payload_of)
                .collect(),
            parent: request
                .parent
                .map(|parent| {
                    Ok::<_, Invalid>(link::ParentHistory {
                        run: parent.run.0,
                        entries: parent.entries.into_iter().map(codec::entry_of).collect(),
                        payloads: parent.payloads.into_iter().map(codec::payload_of).collect(),
                        sealed: parent
                            .sealed
                            .map(|sealed| (*sealed).try_into().map(Box::new))
                            .transpose()?,
                    })
                })
                .transpose()?,
            script: engine::StoredScript {
                script: Script::new(request.script.source),
                compiled: engine::CompiledScript {
                    javascript: request.script.javascript,
                    source_map: request.script.source_map,
                },
            },
            action_set: engine::StoredActionSet {
                action_set: rewind_format::decode_action_set(request.action_set.action_set.get())
                    .map_err(Invalid::new)?,
                declarations: engine::ToolDeclarations {
                    typescript: request.action_set.typescript,
                    prompt: request.action_set.prompt,
                },
            },
            purpose: match request.purpose {
                Purpose::Runnable => link::Purpose::Runnable,
                Purpose::QuarantineRetry => link::Purpose::QuarantineRetry,
                Purpose::GiveUp => link::Purpose::GiveUp,
            },
            tasks: all(request.tasks, codec::task_of)?,
        })
    }
}

impl From<&engine::SealedAttempt> for SealedAttempt {
    fn from(sealed: &engine::SealedAttempt) -> Self {
        Self {
            tenant: sealed.run.tenant.to_string(),
            attempt: sealed.attempt.0,
            journal: RawValue::from_string(rewind_format::encode_journal(&sealed.document()))
                .expect("a journal document is JSON"),
        }
    }
}

impl TryFrom<SealedAttempt> for engine::SealedAttempt {
    type Error = Invalid;

    fn try_from(sealed: SealedAttempt) -> Result<Self, Invalid> {
        let journal = rewind_format::decode_journal(sealed.journal.get()).map_err(Invalid::new)?;
        Ok(engine::SealedAttempt {
            run: engine::RunRef::new(tenant(sealed.tenant)?, journal.run),
            attempt: engine::Attempt(sealed.attempt),
            script: journal.script,
            action_set: journal.action_set,
            entries: journal.entries,
        })
    }
}

impl From<&link::ActivationResult> for ActivationResult {
    fn from(result: &link::ActivationResult) -> Self {
        match result {
            link::ActivationResult::Committed(committed) => {
                ActivationResult::Committed(Box::new((&**committed).into()))
            }
            link::ActivationResult::Quarantined { kind, reason } => ActivationResult::Quarantined {
                kind: codec::quarantine_kind_record(*kind),
                reason: reason.clone(),
            },
            link::ActivationResult::Released => ActivationResult::Released,
            link::ActivationResult::Failed { error } => ActivationResult::Failed {
                error: error.clone(),
            },
            link::ActivationResult::Miss => ActivationResult::Miss,
        }
    }
}

impl TryFrom<ActivationResult> for link::ActivationResult {
    type Error = Invalid;

    fn try_from(result: ActivationResult) -> Result<Self, Invalid> {
        Ok(match result {
            ActivationResult::Committed(committed) => {
                link::ActivationResult::Committed(Box::new((*committed).try_into()?))
            }
            ActivationResult::Quarantined { kind, reason } => link::ActivationResult::Quarantined {
                kind: codec::quarantine_kind_of(kind),
                reason,
            },
            ActivationResult::Released => link::ActivationResult::Released,
            ActivationResult::Failed { error } => link::ActivationResult::Failed { error },
            ActivationResult::Miss => link::ActivationResult::Miss,
        })
    }
}

impl From<&link::Committed> for Committed {
    fn from(committed: &link::Committed) -> Self {
        Self {
            activation: (&committed.activation).into(),
            projections: committed
                .projections
                .iter()
                .map(codec::projection_record)
                .collect(),
            end: committed.end.clone(),
            children: committed
                .children
                .iter()
                .map(|&(seq, run, attached)| Child {
                    seq: seq.0,
                    run: Id(run),
                    attached,
                })
                .collect(),
            copied: committed.copied,
            cpu: committed.cpu,
            mode: committed.mode.map(Into::into),
            measurements: committed.measurements.iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<Committed> for link::Committed {
    type Error = Invalid;

    fn try_from(committed: Committed) -> Result<Self, Invalid> {
        Ok(link::Committed {
            activation: committed.activation.try_into()?,
            projections: all(committed.projections, codec::projection_of)?,
            end: committed.end,
            children: committed
                .children
                .into_iter()
                .map(|child| (Seq(child.seq), child.run.0, child.attached))
                .collect(),
            copied: committed.copied,
            cpu: committed.cpu,
            mode: committed.mode.map(Into::into),
            measurements: committed.measurements.into_iter().map(Into::into).collect(),
        })
    }
}

impl From<&store::Activation> for Activation {
    fn from(activation: &store::Activation) -> Self {
        Self {
            run: (&activation.run).into(),
            attempt: activation.attempt.0,
            entries: activation.entries.iter().map(codec::entry_record).collect(),
            consumed: activation.consumed.clone(),
            payloads: activation
                .payloads
                .iter()
                .map(codec::payload_record)
                .collect(),
            tasks: activation.tasks.iter().map(codec::task_record).collect(),
            timers: activation.timers.iter().map(Into::into).collect(),
            head: codec::head_record(&activation.head),
            summary: codec::summary_record(&activation.summary),
            counters: activation.counters.iter().map(Into::into).collect(),
            outbox: activation.outbox.iter().map(codec::outbox_record).collect(),
        }
    }
}

impl TryFrom<Activation> for store::Activation {
    type Error = Invalid;

    fn try_from(activation: Activation) -> Result<Self, Invalid> {
        Ok(store::Activation {
            run: activation.run.try_into()?,
            attempt: engine::Attempt(activation.attempt),
            entries: activation
                .entries
                .into_iter()
                .map(codec::entry_of)
                .collect(),
            consumed: activation.consumed,
            payloads: activation
                .payloads
                .into_iter()
                .map(codec::payload_of)
                .collect(),
            tasks: all(activation.tasks, codec::task_of)?,
            timers: all(activation.timers, TryInto::try_into)?,
            head: codec::head_of(activation.head)?,
            summary: codec::summary_of(activation.summary)?,
            counters: all(activation.counters, TryInto::try_into)?,
            outbox: all(activation.outbox, codec::outbox_of)?,
        })
    }
}

impl From<&engine::TimerChange> for TimerChange {
    fn from(change: &engine::TimerChange) -> Self {
        match change {
            engine::TimerChange::Set(timer) => TimerChange::Set(codec::timer_record(timer)),
            engine::TimerChange::Cancel(kind) => TimerChange::Cancel {
                tenant: kind.tenant().to_string(),
                kind: codec::timer_kind_record(kind),
            },
        }
    }
}

impl TryFrom<TimerChange> for engine::TimerChange {
    type Error = Invalid;

    fn try_from(change: TimerChange) -> Result<Self, Invalid> {
        Ok(match change {
            TimerChange::Set(timer) => engine::TimerChange::Set(codec::timer_of(timer)?),
            TimerChange::Cancel { tenant: id, kind } => {
                engine::TimerChange::Cancel(codec::timer_kind_of(tenant(id)?, kind))
            }
        })
    }
}

impl From<&engine::CounterDelta> for CounterDelta {
    fn from(delta: &engine::CounterDelta) -> Self {
        match delta {
            engine::CounterDelta::Hour {
                tenant,
                namespace,
                hour,
                counts,
            } => CounterDelta::Hour {
                tenant: tenant.to_string(),
                namespace: namespace.to_string(),
                hour: hour.0,
                counts: Box::new(counts.into()),
            },
            engine::CounterDelta::Shape {
                tenant,
                namespace,
                tool,
                shape,
            } => CounterDelta::Shape {
                tenant: tenant.to_string(),
                namespace: namespace.to_string(),
                tool: tool.clone(),
                shape: record::Shape {
                    shape: shape.clone(),
                },
            },
        }
    }
}

impl TryFrom<CounterDelta> for engine::CounterDelta {
    type Error = Invalid;

    fn try_from(delta: CounterDelta) -> Result<Self, Invalid> {
        Ok(match delta {
            CounterDelta::Hour {
                tenant: id,
                namespace: name,
                hour,
                counts,
            } => engine::CounterDelta::Hour {
                tenant: tenant(id)?,
                namespace: namespace(name)?,
                hour: engine::Hour(hour),
                counts: (*counts).try_into()?,
            },
            CounterDelta::Shape {
                tenant: id,
                namespace: name,
                tool,
                shape,
            } => engine::CounterDelta::Shape {
                tenant: tenant(id)?,
                namespace: namespace(name)?,
                tool,
                shape: shape.shape,
            },
        })
    }
}
