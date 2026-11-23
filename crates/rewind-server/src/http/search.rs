//! Searching runs (`archi/05-testing-and-observability.md`, "Query language"): the query of
//! `q` and the filters of `GET /v1/runs`, turned into the typed query the partitions evaluate on
//! their summaries, within the caller's tenant and, for a key limited to labels, its labels; and
//! the summaries turned into the API's.

use std::collections::BTreeSet;
use std::ops::Bound;

use rewind_api::Problem;
use rewind_api::common::{Callsite, Labels, timestamp};
use rewind_api::query::{self, Comparison, Filter, Pattern, TimeValue, WaitKind};
use rewind_api::runs::{DurationStats, RunFilter, RunSort, RunWait, WaitCounts};
use rewind_core::{Digest, RunId, RunMode, ToolFailureKind};
use rewind_engine::ports::{
    self, Predicate, Range, RunOrder, RunQuery, Scope, SearchCursor, Term, TextMatch, WaitingFor,
};

use crate::auth::Caller;
use crate::error::ApiError;
use crate::http::runs::core_time;
use crate::http::{refuse_nul_in_filter, runs_status};

/// The query for `q`, and for the filters of `GET /v1/runs` when given, as the
/// caller may see it: every term of both, the namespace the request is bound to, and the
/// caller's labels.
pub(crate) fn run_query(
    caller: &Caller,
    q: Option<&str>,
    filter: Option<&RunFilter>,
    now: rewind_core::Timestamp,
) -> Result<RunQuery, ApiError> {
    let mut query = RunQuery::new(caller.tenant.clone());
    if let Some(namespace) = &caller.namespace {
        query = query.with(Predicate::Namespace(BTreeSet::from([namespace.clone()])));
    }
    for (key, value) in &caller.labels {
        query = query.with(Predicate::Label {
            key: key.clone(),
            values: vec![TextMatch::Exact(value.clone())],
        });
    }
    if let Some(filter) = filter {
        query.terms.extend(filter_terms(filter)?);
    }
    if let Some(text) = q {
        refuse_nul_in_filter("q", text)?;
        let parsed = query::parse(text)
            .map_err(|error| ApiError(Box::new(Problem::invalid_query(&error))))?;
        let now = timestamp(now);
        for term in parsed.terms {
            query.terms.push(self::term(term, now));
        }
    }
    Ok(query)
}

/// The terms of the separate filters of `GET /v1/runs`.
fn filter_terms(filter: &RunFilter) -> Result<Vec<Term>, ApiError> {
    let mut terms = Vec::new();
    let mut with = |predicate| {
        terms.push(Term {
            negated: false,
            predicate,
        });
    };
    if let Some(status) = filter.status {
        with(Predicate::Status(BTreeSet::from([runs_status(status)])));
    }
    for label in &filter.labels {
        refuse_nul_in_filter("label", label)?;
        let (key, value) = label
            .split_once(':')
            .ok_or_else(|| ApiError::invalid(format!("label {label:?}: expected key:value")))?;
        with(Predicate::Label {
            key: key.to_owned(),
            values: vec![TextMatch::Exact(value.to_owned())],
        });
    }
    if let Some(tool) = &filter.tool {
        refuse_nul_in_filter("tool", tool)?;
        with(Predicate::Tool(vec![TextMatch::Exact(tool.clone())]));
    }
    if let Some(script) = &filter.script {
        let digest = Digest::from_hex(script)
            .map_err(|_| ApiError::invalid(format!("script {script:?}: expected a digest")))?;
        with(Predicate::Script(vec![digest.to_hex()]));
    }
    if let Some(error) = &filter.error {
        refuse_nul_in_filter("error", error)?;
        with(error_predicate(std::slice::from_ref(error)));
    }
    if let Some(parent) = &filter.parent {
        let parent = parent.parse::<RunId>().map_err(|_| {
            ApiError::invalid(format!("parent {parent:?}: expected a run's id or handle"))
        })?;
        with(Predicate::Parent(parent));
    }
    if let Some(workflow) = &filter.workflow {
        refuse_nul_in_filter("workflow", workflow)?;
        with(Predicate::Workflow(vec![TextMatch::Exact(
            workflow.clone(),
        )]));
    }
    if let Some(started_by) = &filter.started_by {
        let started_by = started_by.parse::<RunId>().map_err(|_| {
            ApiError::invalid(format!(
                "startedBy {started_by:?}: expected a run's id or handle"
            ))
        })?;
        with(Predicate::StartedBy(started_by));
    }
    if filter.started_after.is_some() || filter.started_before.is_some() {
        with(Predicate::Started(Range {
            start: filter
                .started_after
                .map_or(Bound::Unbounded, |at| Bound::Included(core_time(at))),
            end: filter
                .started_before
                .map_or(Bound::Unbounded, |at| Bound::Excluded(core_time(at))),
        }));
    }
    if let Some(quarantined) = filter.quarantined {
        terms.push(Term {
            negated: !quarantined,
            predicate: Predicate::Quarantined,
        });
    }
    Ok(terms)
}

/// One term of a query, with `now` resolved.
fn term(term: query::Term, now: jiff::Timestamp) -> Term {
    let mut negated = term.negated;
    let predicate = match term.filter {
        Filter::Namespace(names) => Predicate::Namespace(
            names
                .iter()
                .filter_map(|name| rewind_engine::ports::NamespaceName::new(name.as_str()).ok())
                .collect(),
        ),
        Filter::Label { key, values } => Predicate::Label {
            key,
            values: patterns(values),
        },
        Filter::Status(statuses) => {
            Predicate::Status(statuses.into_iter().map(runs_status).collect())
        }
        Filter::Tool(tools) => Predicate::Tool(patterns(tools)),
        Filter::Error(errors) => error_predicate(&errors),
        Filter::Waiting(kinds) => Predicate::Waiting(kinds.into_iter().map(waiting_for).collect()),
        Filter::Signal(names) => Predicate::Signal(patterns(names)),
        Filter::Started(range) => Predicate::Started(times(range, now)),
        Filter::Ended(range) => Predicate::Ended(times(range, now)),
        Filter::Activity(range) => Predicate::Activity(times(range, now)),
        Filter::Duration(range) => Predicate::Duration(counts(range)),
        Filter::Calls(range) => Predicate::Calls(counts(range)),
        Filter::Errors(range) => Predicate::Errors(counts(range)),
        Filter::Script(prefixes) => Predicate::Script(prefixes),
        Filter::Toolset(prefixes) => Predicate::Toolset(prefixes),
        Filter::Id(prefixes) => Predicate::Id(prefixes),
        // The parser checked the form of the id.
        Filter::Parent(parent) => match parent.parse::<RunId>() {
            Ok(parent) => Predicate::Parent(parent),
            Err(_) => Predicate::Id(vec![parent]),
        },
        Filter::Workflow(names) => Predicate::Workflow(patterns(names)),
        Filter::StartedBy(run) => match run.parse::<RunId>() {
            Ok(run) => Predicate::StartedBy(run),
            Err(_) => Predicate::Id(vec![run]),
        },
        Filter::Fingerprint(fingerprints) => Predicate::Fingerprint(fingerprints),
        Filter::Quarantined(quarantined) => {
            negated ^= !quarantined;
            Predicate::Quarantined
        }
        Filter::Mode(modes) => Predicate::Mode(
            modes
                .into_iter()
                .map(|mode| match mode {
                    rewind_api::common::RunMode::Live => RunMode::Live,
                    rewind_api::common::RunMode::Mock => RunMode::Mock,
                })
                .collect(),
        ),
        Filter::Text(text) => Predicate::Text(text.to_lowercase()),
    };
    Term { negated, predicate }
}

/// `error:`: the name a run ended with (or its abort's kind), or the kind a call failed with.
fn error_predicate(errors: &[String]) -> Predicate {
    Predicate::Error {
        names: errors.iter().cloned().collect(),
        kinds: errors
            .iter()
            .filter_map(|error| failure_kind(error))
            .collect(),
    }
}

pub(crate) fn failure_kind(text: &str) -> Option<ToolFailureKind> {
    [
        ToolFailureKind::Error,
        ToolFailureKind::Unavailable,
        ToolFailureKind::Timeout,
        ToolFailureKind::OutcomeUnknown,
    ]
    .into_iter()
    .find(|kind| kind.as_str() == text)
}

fn patterns(patterns: Vec<Pattern>) -> Vec<TextMatch> {
    patterns
        .into_iter()
        .map(|pattern| match pattern {
            Pattern::Any => TextMatch::Any,
            Pattern::Exact(value) => TextMatch::Exact(value),
            Pattern::Prefix(prefix) => TextMatch::Prefix(prefix),
        })
        .collect()
}

fn times(range: Comparison<TimeValue>, now: jiff::Timestamp) -> Range<rewind_core::Timestamp> {
    let at = |value: TimeValue| core_time(value.resolve(now));
    Range {
        start: range.start.map(at),
        end: range.end.map(at),
    }
}

fn counts(range: Comparison<u64>) -> Range<u64> {
    Range {
        start: range.start,
        end: range.end,
    }
}

pub(crate) fn waiting_for(kind: WaitKind) -> WaitingFor {
    match kind {
        WaitKind::Signal => WaitingFor::Signal,
        WaitKind::Timer => WaitingFor::Timer,
        WaitKind::Call => WaitingFor::Call,
        WaitKind::Host => WaitingFor::Host,
        WaitKind::Approval => WaitingFor::Approval,
        WaitKind::Parked => WaitingFor::Parked,
        WaitKind::Deferred => WaitingFor::Deferred,
    }
}

/// What a run waits for, as the query language names it.
pub(crate) fn wait_name(waiting: WaitingFor) -> &'static str {
    match waiting {
        WaitingFor::Signal => "signal",
        WaitingFor::Timer => "timer",
        WaitingFor::Call => "call",
        WaitingFor::Host => "host",
        WaitingFor::Approval => "approval",
        WaitingFor::Parked => "parked",
        WaitingFor::Deferred => "deferred",
    }
}

fn wait_kind(waiting: WaitingFor) -> WaitKind {
    match waiting {
        WaitingFor::Signal => WaitKind::Signal,
        WaitingFor::Timer => WaitKind::Timer,
        WaitingFor::Call => WaitKind::Call,
        WaitingFor::Host => WaitKind::Host,
        WaitingFor::Approval => WaitKind::Approval,
        WaitingFor::Parked => WaitKind::Parked,
        WaitingFor::Deferred => WaitKind::Deferred,
    }
}

/// The scope a query reads: one namespace when the request is bound to it or one term
/// `namespace:` of one value names it, and the tenant otherwise (`archi/08-decisions.md`, "Taken
/// while writing the object-storage spec": a query goes to one partition when its scope is one
/// namespace).
pub(crate) fn scope_of(query: &RunQuery) -> Scope {
    let named = query.terms.iter().find_map(|term| match &term.predicate {
        Predicate::Namespace(names) if !term.negated && names.len() == 1 => names.first(),
        _ => None,
    });
    match named {
        Some(namespace) => Scope::Namespace {
            tenant: query.tenant.clone(),
            namespace: namespace.clone(),
        },
        None => Scope::Tenant(query.tenant.clone()),
    }
}

/// The run plane's order for the API's.
pub(crate) fn order(sort: Option<RunSort>) -> RunOrder {
    match sort.unwrap_or_default() {
        RunSort::Started => RunOrder::Started,
        RunSort::Oldest => RunOrder::Oldest,
        RunSort::Activity => RunOrder::Activity,
        RunSort::Wakes => RunOrder::Wakes,
        RunSort::Duration => RunOrder::Duration,
        RunSort::Calls => RunOrder::Calls,
    }
}

/// Whether a page's cursor in this order is the run id alone.
fn by_run_alone(order: RunOrder) -> bool {
    matches!(order, RunOrder::Started | RunOrder::Oldest)
}

/// A page's cursor as the API writes it: the run id for `started` and `oldest`, else the sort
/// key (milliseconds, or a number of calls; `-` without one), a dot and the run id.
pub(crate) fn encode_cursor(order: RunOrder, cursor: &SearchCursor) -> String {
    if by_run_alone(order) {
        return cursor.run.to_string();
    }
    match cursor.key {
        Some(key) => format!("{key}.{}", cursor.run),
        None => format!("-.{}", cursor.run),
    }
}

pub(crate) fn decode_cursor(order: RunOrder, text: &str) -> Result<SearchCursor, ApiError> {
    let invalid = || {
        ApiError::invalid(format!(
            "cursor {text:?} is not valid for this sort: pass the nextCursor of the previous page, with the same sort"
        ))
    };
    let (key, run) = if by_run_alone(order) {
        (None, text)
    } else {
        let (key, run) = text.split_once('.').ok_or_else(invalid)?;
        let key = match key {
            "-" => None,
            key => Some(key.parse().map_err(|_| invalid())?),
        };
        (key, run)
    };
    Ok(SearchCursor {
        key,
        run: run.parse().map_err(|_| invalid())?,
    })
}

/// A run's summary, as `GET /v1/runs` lists it and the change stream sends it.
pub(crate) fn summary(run: &ports::RunSummary) -> rewind_api::runs::RunSummary {
    let ending = run.ending.as_ref();
    rewind_api::runs::RunSummary {
        id: run.run.id.to_string(),
        handle: run.handle().to_string(),
        namespace: crate::views::namespace(&run.namespace),
        status: run.status.into(),
        quarantined: run.quarantine.is_some(),
        script: run.script.to_hex(),
        // The script's title is not the read model's: the caller gives it.
        script_title: None,
        headline: run.headline.clone(),
        toolset: run.toolset.to_hex(),
        labels: Labels::from(run.labels.clone()),
        started_at: timestamp(run.started_at.unwrap_or(run.last_activity_at)),
        ended_at: run.ended_at.map(timestamp),
        last_activity_at: timestamp(run.last_activity_at),
        tool_calls: run.tool_calls,
        errors: run.errors,
        history_len: run.history_len,
        error: ending.map(|ending| ending.error.clone()),
        error_message: ending.map(|ending| ending.message.clone()),
        error_line: ending.and_then(|ending| ending.line),
        fingerprint: ending.map(|ending| ending.fingerprint.clone()),
        waiting_for: run
            .waits
            .iter()
            .map(|wait| RunWait {
                kind: wait_kind(wait.waiting_for()),
                seq: wait.seq.map(|seq| seq.0),
                name: wait.name.clone(),
                since: timestamp(wait.since),
                until: wait.until.map(timestamp),
                callsite: wait.callsite.map(Callsite::from),
            })
            .collect(),
        wakes_at: run.wakes_at.map(timestamp),
        parent: run.parent.map(|parent| parent.to_string()),
        workflow: run.workflow.clone(),
        started_by: run.started_by.map(|run| run.to_string()),
    }
}

pub(crate) fn wait_counts<'a>(
    waiting: impl IntoIterator<Item = (&'a WaitingFor, &'a u64)>,
) -> WaitCounts {
    let mut counts = WaitCounts::default();
    for (kind, count) in waiting {
        let slot = match kind {
            WaitingFor::Signal => &mut counts.signal,
            WaitingFor::Timer => &mut counts.timer,
            WaitingFor::Call => &mut counts.call,
            WaitingFor::Host => &mut counts.host,
            WaitingFor::Approval => &mut counts.approval,
            WaitingFor::Parked => &mut counts.parked,
            WaitingFor::Deferred => &mut counts.deferred,
        };
        *slot += count;
    }
    counts
}

pub(crate) fn durations(latency: Option<ports::Latency>) -> Option<DurationStats> {
    latency.map(|latency| DurationStats {
        count: latency.count,
        p50: latency.p50_ms,
        p95: latency.p95_ms,
        p99: latency.p99_ms,
        max: latency.max_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_read_back_in_their_order() {
        let run: RunId = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10".parse().unwrap();
        for (order, key) in [
            (RunOrder::Started, None),
            (RunOrder::Oldest, None),
            (RunOrder::Activity, Some(42)),
            (RunOrder::Wakes, None),
            (RunOrder::Wakes, Some(7)),
            (RunOrder::Duration, None),
            (RunOrder::Duration, Some(90_000)),
            (RunOrder::Calls, Some(0)),
        ] {
            let cursor = SearchCursor { key, run };
            assert_eq!(
                decode_cursor(order, &encode_cursor(order, &cursor)).unwrap(),
                cursor
            );
        }
        assert!(decode_cursor(RunOrder::Wakes, &run.to_string()).is_err());
        assert!(decode_cursor(RunOrder::Calls, &format!("x.{run}")).is_err());
        assert_eq!(
            encode_cursor(RunOrder::Oldest, &SearchCursor { key: None, run }),
            run.to_string()
        );
    }

    #[test]
    fn quarantined_no_is_the_negation_of_quarantined() {
        let now = jiff::Timestamp::UNIX_EPOCH;
        let parsed = query::parse("quarantined:no -quarantined:no quarantined:yes").unwrap();
        let negations: Vec<bool> = parsed
            .terms
            .into_iter()
            .map(|parsed| term(parsed, now).negated)
            .collect();
        assert_eq!(negations, vec![true, false, false]);
    }

    #[test]
    fn errors_name_both_endings_and_failure_kinds() {
        let Predicate::Error { names, kinds } =
            error_predicate(&["timeout".into(), "TypeError".into()])
        else {
            panic!("an error predicate");
        };
        assert_eq!(names.len(), 2);
        assert_eq!(kinds, BTreeSet::from([ToolFailureKind::Timeout]));
    }
}
