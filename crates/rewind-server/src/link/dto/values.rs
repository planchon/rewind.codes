//! What the messages of the node link carry: a run, a task, a quota refused, a query of the run
//! index and a page of its search, counters and a run's view.

use std::collections::{BTreeMap, BTreeSet};
use std::ops;
use std::time::Duration;

use rewind_core::Timestamp;
use rewind_engine::ports as engine;
use rewind_format::partition as record;
use rewind_store_slatedb::codec;
use serde::{Deserialize, Serialize};

use super::{Id, Invalid, namespace, tenant};

/// `{"tenant":"acme","run":"0196…"}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRef {
    pub tenant: String,
    pub run: Id,
}

impl From<&engine::RunRef> for RunRef {
    fn from(run: &engine::RunRef) -> Self {
        Self {
            tenant: run.tenant.to_string(),
            run: Id(run.id),
        }
    }
}

impl TryFrom<RunRef> for engine::RunRef {
    type Error = Invalid;

    fn try_from(run: RunRef) -> Result<Self, Invalid> {
        Ok(engine::RunRef::new(tenant(run.tenant)?, run.run.0))
    }
}

/// `{"run":{…},"seq":3}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRef {
    pub run: RunRef,
    pub seq: u32,
}

impl From<&engine::TaskRef> for TaskRef {
    fn from(task: &engine::TaskRef) -> Self {
        Self {
            run: (&task.run).into(),
            seq: task.seq.0,
        }
    }
}

impl TryFrom<TaskRef> for engine::TaskRef {
    type Error = Invalid;

    fn try_from(task: TaskRef) -> Result<Self, Invalid> {
        Ok(engine::TaskRef {
            run: task.run.try_into()?,
            seq: rewind_core::Seq(task.seq),
        })
    }
}

/// `{"quota":"active_runs","scope":"namespace","limit":10,"used":10,"resets_at":…}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaExceeded {
    pub quota: QuotaKind,
    pub scope: QuotaScope,
    pub limit: u64,
    pub used: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaKind {
    CpuPerMonth,
    ActiveRuns,
    RunsPerMonth,
    Workflows,
    MaxTtl,
    MaxCodeLines,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaScope {
    Namespace,
    Tenant,
}

impl From<&engine::QuotaExceeded> for QuotaExceeded {
    fn from(exceeded: &engine::QuotaExceeded) -> Self {
        Self {
            quota: match exceeded.quota {
                engine::QuotaKind::CpuPerMonth => QuotaKind::CpuPerMonth,
                engine::QuotaKind::ActiveRuns => QuotaKind::ActiveRuns,
                engine::QuotaKind::RunsPerMonth => QuotaKind::RunsPerMonth,
                engine::QuotaKind::Workflows => QuotaKind::Workflows,
                engine::QuotaKind::MaxTtl => QuotaKind::MaxTtl,
                engine::QuotaKind::MaxCodeLines => QuotaKind::MaxCodeLines,
            },
            scope: match exceeded.scope {
                engine::QuotaScope::Namespace => QuotaScope::Namespace,
                engine::QuotaScope::Tenant => QuotaScope::Tenant,
            },
            limit: exceeded.limit,
            used: exceeded.used,
            resets_at: exceeded.resets_at.map(|at| at.0),
        }
    }
}

impl From<QuotaExceeded> for engine::QuotaExceeded {
    fn from(exceeded: QuotaExceeded) -> Self {
        Self {
            quota: match exceeded.quota {
                QuotaKind::CpuPerMonth => engine::QuotaKind::CpuPerMonth,
                QuotaKind::ActiveRuns => engine::QuotaKind::ActiveRuns,
                QuotaKind::RunsPerMonth => engine::QuotaKind::RunsPerMonth,
                QuotaKind::Workflows => engine::QuotaKind::Workflows,
                QuotaKind::MaxTtl => engine::QuotaKind::MaxTtl,
                QuotaKind::MaxCodeLines => engine::QuotaKind::MaxCodeLines,
            },
            scope: match exceeded.scope {
                QuotaScope::Namespace => engine::QuotaScope::Namespace,
                QuotaScope::Tenant => engine::QuotaScope::Tenant,
            },
            limit: exceeded.limit,
            used: exceeded.used,
            resets_at: exceeded.resets_at.map(Timestamp),
        }
    }
}

// -------------------------------------------------------------------------------------------
// Reads of a partition

/// `{"tenant":"acme"}` or `{"namespace":{"tenant":"acme","namespace":"user-1"}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Scope {
    Tenant(String),
    Namespace { tenant: String, namespace: String },
}

impl From<&engine::Scope> for Scope {
    fn from(scope: &engine::Scope) -> Self {
        match scope {
            engine::Scope::Tenant(tenant) => Scope::Tenant(tenant.to_string()),
            engine::Scope::Namespace { tenant, namespace } => Scope::Namespace {
                tenant: tenant.to_string(),
                namespace: namespace.to_string(),
            },
        }
    }
}

impl TryFrom<Scope> for engine::Scope {
    type Error = Invalid;

    fn try_from(scope: Scope) -> Result<Self, Invalid> {
        Ok(match scope {
            Scope::Tenant(id) => engine::Scope::Tenant(tenant(id)?),
            Scope::Namespace {
                tenant: id,
                namespace: name,
            } => engine::Scope::Namespace {
                tenant: tenant(id)?,
                namespace: namespace(name)?,
            },
        })
    }
}

/// Whole hours since the Unix epoch, `from` included, `to` excluded: `{"from":…,"to":…}`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub from: u32,
    pub to: u32,
}

impl From<engine::Window> for Window {
    fn from(window: engine::Window) -> Self {
        Self {
            from: window.from.0,
            to: window.to.0,
        }
    }
}

impl From<Window> for engine::Window {
    fn from(window: Window) -> Self {
        engine::Window::hours(engine::Hour(window.from), engine::Hour(window.to))
    }
}

/// `"started"`, `"oldest"`, `"activity"`, `"wakes"`, `"duration"` or `"calls"`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOrder {
    Started,
    Oldest,
    Activity,
    Wakes,
    Duration,
    Calls,
}

impl From<engine::RunOrder> for RunOrder {
    fn from(order: engine::RunOrder) -> Self {
        match order {
            engine::RunOrder::Started => RunOrder::Started,
            engine::RunOrder::Oldest => RunOrder::Oldest,
            engine::RunOrder::Activity => RunOrder::Activity,
            engine::RunOrder::Wakes => RunOrder::Wakes,
            engine::RunOrder::Duration => RunOrder::Duration,
            engine::RunOrder::Calls => RunOrder::Calls,
        }
    }
}

impl From<RunOrder> for engine::RunOrder {
    fn from(order: RunOrder) -> Self {
        match order {
            RunOrder::Started => engine::RunOrder::Started,
            RunOrder::Oldest => engine::RunOrder::Oldest,
            RunOrder::Activity => engine::RunOrder::Activity,
            RunOrder::Wakes => engine::RunOrder::Wakes,
            RunOrder::Duration => engine::RunOrder::Duration,
            RunOrder::Calls => engine::RunOrder::Calls,
        }
    }
}

/// `{"key":…,"run":"0196…"}`, the key left out when there is none.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<u64>,
    pub run: Id,
}

impl From<&engine::SearchCursor> for SearchCursor {
    fn from(cursor: &engine::SearchCursor) -> Self {
        Self {
            key: cursor.key,
            run: Id(cursor.run),
        }
    }
}

impl From<SearchCursor> for engine::SearchCursor {
    fn from(cursor: SearchCursor) -> Self {
        Self {
            key: cursor.key,
            run: cursor.run.0,
        }
    }
}

/// `{"cursor":{…},"limit":50,"budget":10000}`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<SearchCursor>,
    pub limit: u64,
    pub budget: u64,
}

impl From<&engine::Page> for Page {
    fn from(page: &engine::Page) -> Self {
        Self {
            cursor: page.cursor.as_ref().map(Into::into),
            limit: u64::try_from(page.limit).unwrap_or(u64::MAX),
            budget: u64::try_from(page.budget).unwrap_or(u64::MAX),
        }
    }
}

impl TryFrom<Page> for engine::Page {
    type Error = Invalid;

    fn try_from(page: Page) -> Result<Self, Invalid> {
        Ok(engine::Page {
            cursor: page.cursor.map(Into::into),
            limit: usize::try_from(page.limit).map_err(Invalid::new)?,
            budget: usize::try_from(page.budget).map_err(Invalid::new)?,
        })
    }
}

/// A page of a search: `{"summaries":[<summary record>,…],"cursor":{…}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Found {
    pub summaries: Vec<record::Summary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<SearchCursor>,
}

impl From<&engine::Found> for Found {
    fn from(found: &engine::Found) -> Self {
        Self {
            summaries: found.summaries.iter().map(summary_record).collect(),
            cursor: found.cursor.as_ref().map(Into::into),
        }
    }
}

impl TryFrom<Found> for engine::Found {
    type Error = Invalid;

    fn try_from(found: Found) -> Result<Self, Invalid> {
        Ok(engine::Found {
            summaries: found
                .summaries
                .into_iter()
                .map(summary_of)
                .collect::<Result<_, _>>()?,
            cursor: found.cursor.map(Into::into),
        })
    }
}

/// A run's summary, as the summary record of the index holds it, without the actions and errors
/// only the index reads.
pub(crate) fn summary_record(summary: &engine::RunSummary) -> record::Summary {
    codec::summary_record(&engine::IndexedSummary::new(summary.clone()))
}

pub(crate) fn summary_of(record: record::Summary) -> Result<engine::RunSummary, Invalid> {
    Ok(codec::summary_of(record)?.summary)
}

/// A run as its owner knows it: `{"head":<run head record>,"summary":<summary record>,
/// "source":"dana/triage/06bqx3k9f2mt","started_by":…}`, the handles of the runs it names as
/// their text.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunView {
    pub head: record::RunHead,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<record::Summary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<String>,
}

impl From<&engine::link::RunView> for RunView {
    fn from(view: &engine::link::RunView) -> Self {
        Self {
            head: codec::head_record(&view.head),
            summary: view.summary.as_ref().map(summary_record),
            source: view.source.as_ref().map(ToString::to_string),
            started_by: view.started_by.as_ref().map(ToString::to_string),
        }
    }
}

impl TryFrom<RunView> for engine::link::RunView {
    type Error = Invalid;

    fn try_from(view: RunView) -> Result<Self, Invalid> {
        Ok(engine::link::RunView {
            head: codec::head_of(view.head)?,
            summary: view.summary.map(summary_of).transpose()?,
            source: view
                .source
                .map(|handle| handle.parse().map_err(Invalid::new))
                .transpose()?,
            started_by: view
                .started_by
                .map(|handle| handle.parse().map_err(Invalid::new))
                .transpose()?,
        })
    }
}

/// Counters summed over a scope and a window: `{"counts":<counters record>,"cpu":{"secs":…,
/// "nanos":…},"namespaces":{"user-1":<run counts record>}}`. The CPU time is written exactly
/// beside the record's milliseconds, and the runs by namespace, which a record leaves to its
/// store, are written too.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counters {
    pub counts: record::Counters,
    pub cpu: Duration,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub namespaces: BTreeMap<String, record::RunCounts>,
}

impl From<&engine::Counters> for Counters {
    fn from(counters: &engine::Counters) -> Self {
        Self {
            counts: codec::counters_record(counters),
            cpu: counters.cpu,
            namespaces: counters
                .namespaces
                .iter()
                .map(|(name, runs)| (name.to_string(), codec::runs_record(runs)))
                .collect(),
        }
    }
}

impl TryFrom<Counters> for engine::Counters {
    type Error = Invalid;

    fn try_from(counters: Counters) -> Result<Self, Invalid> {
        let mut decoded = codec::counters_of(counters.counts)?;
        decoded.cpu = counters.cpu;
        decoded.namespaces = counters
            .namespaces
            .into_iter()
            .map(|(name, runs)| Ok((namespace(name)?, codec::runs_of(runs))))
            .collect::<Result<_, Invalid>>()?;
        Ok(decoded)
    }
}

/// A calendar month, in UTC: `{"year":2026,"month":11}`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Month {
    pub year: i16,
    pub month: i8,
}

impl From<engine::Month> for Month {
    fn from(month: engine::Month) -> Self {
        Self {
            year: month.year(),
            month: month.month(),
        }
    }
}

impl TryFrom<Month> for engine::Month {
    type Error = Invalid;

    fn try_from(month: Month) -> Result<Self, Invalid> {
        engine::Month::new(month.year, month.month)
            .ok_or_else(|| Invalid(format!("no month {}-{}", month.year, month.month)))
    }
}

/// What a namespace used in a month: `{"cpu":{"secs":…,"nanos":…},"runs_started":4,
/// "active_runs":1,"workflows":2,"days":[{"day":20412,"cpu":{…},"runs_started":3},…]}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub cpu: Duration,
    pub runs_started: u64,
    pub active_runs: u64,
    pub workflows: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<DayUsage>,
}

/// What a namespace used in a day, counted in days since the Unix epoch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DayUsage {
    pub day: u32,
    pub cpu: Duration,
    pub runs_started: u64,
}

impl From<&engine::Usage> for Usage {
    fn from(usage: &engine::Usage) -> Self {
        Self {
            cpu: usage.cpu,
            runs_started: usage.runs_started,
            active_runs: usage.active_runs,
            workflows: usage.workflows,
            days: usage
                .days
                .iter()
                .map(|(day, used)| DayUsage {
                    day: day.0,
                    cpu: used.cpu,
                    runs_started: used.runs_started,
                })
                .collect(),
        }
    }
}

impl From<Usage> for engine::Usage {
    fn from(usage: Usage) -> Self {
        Self {
            cpu: usage.cpu,
            runs_started: usage.runs_started,
            active_runs: usage.active_runs,
            workflows: usage.workflows,
            days: usage
                .days
                .into_iter()
                .map(|used| {
                    (
                        engine::Day(used.day),
                        engine::DayUsage {
                            cpu: used.cpu,
                            runs_started: used.runs_started,
                        },
                    )
                })
                .collect(),
        }
    }
}

// -------------------------------------------------------------------------------------------
// Queries of the run index

/// `{"tenant":"acme","terms":[{"negated":false,"predicate":{…}},…]}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunQuery {
    pub tenant: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terms: Vec<Term>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Term {
    pub negated: bool,
    pub predicate: Predicate,
}

/// One predicate on a run, named as its variant: `{"status":["active"]}`, `"quarantined"`, …
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    Namespace(BTreeSet<String>),
    Id(Vec<String>),
    Status(Vec<record::Status>),
    Label {
        key: String,
        values: Vec<TextMatch>,
    },
    ActionSet(Vec<String>),
    Action(Vec<TextMatch>),
    Error {
        names: BTreeSet<String>,
        kinds: Vec<record::FailureKind>,
    },
    Waiting(Vec<record::WaitingFor>),
    Signal(Vec<TextMatch>),
    Started(Range<u64>),
    Ended(Range<u64>),
    Activity(Range<u64>),
    Duration(Range<u64>),
    Calls(Range<u64>),
    Errors(Range<u64>),
    Source(Id),
    Workflow(Vec<TextMatch>),
    StartedBy(Id),
    Fingerprint(Vec<String>),
    Quarantined,
    Mode(Vec<record::Mode>),
    Text(String),
}

/// `"any"`, `{"exact":"…"}` or `{"prefix":"…"}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum TextMatch {
    Any,
    Exact(String),
    Prefix(String),
}

/// `{"start":{"included":…},"end":"unbounded"}`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range<T> {
    pub start: Bound<T>,
    pub end: Bound<T>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Bound<T> {
    Included(T),
    Excluded(T),
    Unbounded,
}

fn bound<T: Copy, U>(bound: &ops::Bound<T>, into: impl Fn(T) -> U) -> Bound<U> {
    match bound {
        ops::Bound::Included(value) => Bound::Included(into(*value)),
        ops::Bound::Excluded(value) => Bound::Excluded(into(*value)),
        ops::Bound::Unbounded => Bound::Unbounded,
    }
}

fn bound_of<T, U>(bound: Bound<T>, into: impl Fn(T) -> U) -> ops::Bound<U> {
    match bound {
        Bound::Included(value) => ops::Bound::Included(into(value)),
        Bound::Excluded(value) => ops::Bound::Excluded(into(value)),
        Bound::Unbounded => ops::Bound::Unbounded,
    }
}

fn range<T: Copy, U>(range: &engine::Range<T>, into: impl Fn(T) -> U + Copy) -> Range<U> {
    Range {
        start: bound(&range.start, into),
        end: bound(&range.end, into),
    }
}

fn range_of<T, U>(range: Range<T>, into: impl Fn(T) -> U + Copy) -> engine::Range<U> {
    engine::Range {
        start: bound_of(range.start, into),
        end: bound_of(range.end, into),
    }
}

fn times(range: &engine::Range<Timestamp>) -> Range<u64> {
    self::range(range, |at: Timestamp| at.0)
}

fn times_of(range: Range<u64>) -> engine::Range<Timestamp> {
    range_of(range, Timestamp)
}

fn counts(range: &engine::Range<u64>) -> Range<u64> {
    self::range(range, |count| count)
}

fn counts_of(range: Range<u64>) -> engine::Range<u64> {
    range_of(range, |count| count)
}

impl From<&engine::TextMatch> for TextMatch {
    fn from(matcher: &engine::TextMatch) -> Self {
        match matcher {
            engine::TextMatch::Any => TextMatch::Any,
            engine::TextMatch::Exact(text) => TextMatch::Exact(text.clone()),
            engine::TextMatch::Prefix(text) => TextMatch::Prefix(text.clone()),
        }
    }
}

impl From<TextMatch> for engine::TextMatch {
    fn from(matcher: TextMatch) -> Self {
        match matcher {
            TextMatch::Any => engine::TextMatch::Any,
            TextMatch::Exact(text) => engine::TextMatch::Exact(text),
            TextMatch::Prefix(text) => engine::TextMatch::Prefix(text),
        }
    }
}

fn matchers(matchers: &[engine::TextMatch]) -> Vec<TextMatch> {
    matchers.iter().map(Into::into).collect()
}

fn matchers_of(matchers: Vec<TextMatch>) -> Vec<engine::TextMatch> {
    matchers.into_iter().map(Into::into).collect()
}

impl From<&engine::Predicate> for Predicate {
    fn from(predicate: &engine::Predicate) -> Self {
        use engine::Predicate as P;
        match predicate {
            P::Namespace(names) => {
                Predicate::Namespace(names.iter().map(ToString::to_string).collect())
            }
            P::Id(prefixes) => Predicate::Id(prefixes.clone()),
            P::Status(statuses) => {
                Predicate::Status(statuses.iter().map(|&status| status.into()).collect())
            }
            P::Label { key, values } => Predicate::Label {
                key: key.clone(),
                values: matchers(values),
            },
            P::ActionSet(prefixes) => Predicate::ActionSet(prefixes.clone()),
            P::Action(actions) => Predicate::Action(matchers(actions)),
            P::Error { names, kinds } => Predicate::Error {
                names: names.clone(),
                kinds: kinds.iter().map(|&kind| kind.into()).collect(),
            },
            P::Waiting(kinds) => Predicate::Waiting(
                kinds
                    .iter()
                    .map(|&kind| codec::waiting_record(kind))
                    .collect(),
            ),
            P::Signal(names) => Predicate::Signal(matchers(names)),
            P::Started(range) => Predicate::Started(times(range)),
            P::Ended(range) => Predicate::Ended(times(range)),
            P::Activity(range) => Predicate::Activity(times(range)),
            P::Duration(range) => Predicate::Duration(counts(range)),
            P::Calls(range) => Predicate::Calls(counts(range)),
            P::Errors(range) => Predicate::Errors(counts(range)),
            P::Source(run) => Predicate::Source(Id(*run)),
            P::Workflow(names) => Predicate::Workflow(matchers(names)),
            P::StartedBy(run) => Predicate::StartedBy(Id(*run)),
            P::Fingerprint(fingerprints) => Predicate::Fingerprint(fingerprints.clone()),
            P::Quarantined => Predicate::Quarantined,
            P::Mode(modes) => Predicate::Mode(modes.iter().map(|&mode| mode.into()).collect()),
            P::Text(text) => Predicate::Text(text.clone()),
        }
    }
}

impl TryFrom<Predicate> for engine::Predicate {
    type Error = Invalid;

    fn try_from(predicate: Predicate) -> Result<Self, Invalid> {
        use engine::Predicate as P;
        Ok(match predicate {
            Predicate::Namespace(names) => {
                P::Namespace(names.into_iter().map(namespace).collect::<Result<_, _>>()?)
            }
            Predicate::Id(prefixes) => P::Id(prefixes),
            Predicate::Status(statuses) => {
                P::Status(statuses.into_iter().map(Into::into).collect())
            }
            Predicate::Label { key, values } => P::Label {
                key,
                values: matchers_of(values),
            },
            Predicate::ActionSet(prefixes) => P::ActionSet(prefixes),
            Predicate::Action(actions) => P::Action(matchers_of(actions)),
            Predicate::Error { names, kinds } => P::Error {
                names,
                kinds: kinds.into_iter().map(Into::into).collect(),
            },
            Predicate::Waiting(kinds) => {
                P::Waiting(kinds.into_iter().map(codec::waiting_of).collect())
            }
            Predicate::Signal(names) => P::Signal(matchers_of(names)),
            Predicate::Started(range) => P::Started(times_of(range)),
            Predicate::Ended(range) => P::Ended(times_of(range)),
            Predicate::Activity(range) => P::Activity(times_of(range)),
            Predicate::Duration(range) => P::Duration(counts_of(range)),
            Predicate::Calls(range) => P::Calls(counts_of(range)),
            Predicate::Errors(range) => P::Errors(counts_of(range)),
            Predicate::Source(run) => P::Source(run.0),
            Predicate::Workflow(names) => P::Workflow(matchers_of(names)),
            Predicate::StartedBy(run) => P::StartedBy(run.0),
            Predicate::Fingerprint(fingerprints) => P::Fingerprint(fingerprints),
            Predicate::Quarantined => P::Quarantined,
            Predicate::Mode(modes) => P::Mode(modes.into_iter().map(Into::into).collect()),
            Predicate::Text(text) => P::Text(text),
        })
    }
}

impl From<&engine::RunQuery> for RunQuery {
    fn from(query: &engine::RunQuery) -> Self {
        Self {
            tenant: query.tenant.to_string(),
            terms: query
                .terms
                .iter()
                .map(|term| Term {
                    negated: term.negated,
                    predicate: (&term.predicate).into(),
                })
                .collect(),
        }
    }
}

impl TryFrom<RunQuery> for engine::RunQuery {
    type Error = Invalid;

    fn try_from(query: RunQuery) -> Result<Self, Invalid> {
        Ok(engine::RunQuery {
            tenant: tenant(query.tenant)?,
            terms: query
                .terms
                .into_iter()
                .map(|term| {
                    Ok(engine::Term {
                        negated: term.negated,
                        predicate: term.predicate.try_into()?,
                    })
                })
                .collect::<Result<_, Invalid>>()?,
        })
    }
}
