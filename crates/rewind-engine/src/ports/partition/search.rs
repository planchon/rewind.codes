//! The run index of a partition and its search (`archi/05-testing-and-observability.md`, "Read
//! model", "Query language").
//!
//! Every run, live or ended, has one summary, written by the owner in the batch of the facts
//! it describes and kept until the run is erased. A search evaluates every term of a query on
//! the summaries of its scope, in the order asked, and reads at most its scan budget of them: a
//! page cut by the budget comes back short, with a cursor where it stopped.

use std::cmp::Reverse;
use std::collections::BTreeSet;

use rewind_core::{ActionFailureKind, Digest, RunId, RunStatus};

use crate::ports::read_model::{
    Predicate, RunOrder, RunQuery, RunSummary, SearchCursor, Wait, WaitKind, WaitingFor,
};

/// The summaries a partition reads at most for one page of a search.
pub const SCAN_BUDGET: usize = 10_000;

/// A run as the index holds it: its summary, and what the search reads beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedSummary {
    pub summary: RunSummary,
    /// The actions the run scheduled calls to (`action:`).
    pub actions: BTreeSet<String>,
    /// The kinds its failed calls failed with (`error:`).
    pub call_errors: BTreeSet<ActionFailureKind>,
}

impl IndexedSummary {
    /// A summary whose run called no action.
    pub fn new(summary: RunSummary) -> Self {
        Self {
            summary,
            actions: BTreeSet::new(),
            call_errors: BTreeSet::new(),
        }
    }

    /// Whether the run is the query's tenant's, and every term of the query holds: a negated
    /// term holds when its predicate does not.
    pub fn matches(&self, query: &RunQuery) -> bool {
        self.summary.run.tenant == query.tenant
            && query
                .terms
                .iter()
                .all(|term| self.holds(&term.predicate) != term.negated)
    }

    /// Whether the predicate holds for the run. A run without the value a predicate reads (the
    /// end of a run that has not ended) does not match it.
    pub fn holds(&self, predicate: &Predicate) -> bool {
        let summary = &self.summary;
        let hex_prefix = |digest: &Digest, prefixes: &[String]| {
            let hex = digest.to_hex();
            prefixes
                .iter()
                .any(|prefix| hex.starts_with(prefix.as_str()))
        };
        match predicate {
            Predicate::Namespace(names) => names.contains(&summary.namespace),
            Predicate::Id(prefixes) => {
                let id = summary.run.id.to_string();
                prefixes
                    .iter()
                    .any(|prefix| id.starts_with(prefix.as_str()))
            }
            Predicate::Status(statuses) => statuses.contains(&summary.status),
            Predicate::Label { key, values } => summary
                .labels
                .get(key)
                .is_some_and(|value| values.iter().any(|matcher| matcher.matches(value))),
            Predicate::Script(prefixes) => hex_prefix(&summary.code, prefixes),
            Predicate::ActionSet(prefixes) => hex_prefix(&summary.action_set, prefixes),
            Predicate::Action(matchers) => self
                .actions
                .iter()
                .any(|action| matchers.iter().any(|matcher| matcher.matches(action))),
            Predicate::Error { names, kinds } => {
                summary
                    .ending
                    .as_ref()
                    .is_some_and(|ending| names.contains(&ending.error))
                    || self.call_errors.iter().any(|kind| kinds.contains(kind))
            }
            Predicate::Waiting(kinds) => {
                let waiting = self.waiting();
                !waiting.is_empty()
                    && (kinds.is_empty() || waiting.iter().any(|kind| kinds.contains(kind)))
            }
            Predicate::Signal(matchers) => self
                .signals()
                .iter()
                .any(|name| matchers.iter().any(|matcher| matcher.matches(name))),
            Predicate::Started(range) => summary.started_at.is_some_and(|at| range.contains(&at)),
            Predicate::Ended(range) => summary.ended_at.is_some_and(|at| range.contains(&at)),
            Predicate::Activity(range) => range.contains(&summary.last_activity_at),
            Predicate::Duration(range) => summary
                .duration()
                .is_some_and(|duration| range.contains(&duration)),
            Predicate::Calls(range) => range.contains(&summary.calls),
            Predicate::Errors(range) => range.contains(&summary.errors),
            Predicate::Parent(parent) => summary.parent == Some(*parent),
            Predicate::Workflow(matchers) => summary
                .workflow
                .as_deref()
                .is_some_and(|name| matchers.iter().any(|matcher| matcher.matches(name))),
            Predicate::StartedBy(run) => summary.started_by == Some(*run),
            Predicate::Fingerprint(fingerprints) => summary
                .ending
                .as_ref()
                .is_some_and(|ending| fingerprints.contains(&ending.fingerprint)),
            Predicate::Quarantined => summary.quarantine.is_some(),
            Predicate::Mode(modes) => summary.mode.is_some_and(|mode| modes.contains(&mode)),
            Predicate::Text(text) => self
                .text()
                .iter()
                .any(|field| field.to_lowercase().contains(text.as_str())),
        }
    }

    /// What the run waits for: its waits' kinds, while it is active.
    pub fn waiting(&self) -> BTreeSet<WaitingFor> {
        if self.summary.status != RunStatus::Active {
            return BTreeSet::new();
        }
        self.summary.waits.iter().map(Wait::waiting_for).collect()
    }

    /// The signals the run waits for.
    pub fn signals(&self) -> BTreeSet<&str> {
        self.summary
            .waits
            .iter()
            .filter(|wait| wait.kind == WaitKind::Signal)
            .filter_map(|wait| wait.name.as_deref())
            .collect()
    }

    /// What free text is searched in: the run's id, the code's first line, the ending's error
    /// and message, the labels' values.
    fn text(&self) -> Vec<String> {
        let summary = &self.summary;
        let mut fields = vec![summary.run.id.to_string(), summary.headline.clone()];
        if let Some(ending) = &summary.ending {
            fields.push(ending.error.clone());
            fields.push(ending.message.clone());
        }
        fields.extend(summary.labels.values().cloned());
        fields
    }
}

/// A run's place in an order of the search: a run comes before another when its position is
/// smaller. The orders are [`RunOrder`]'s, ties broken by run id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SearchPosition(Position);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Position {
    Started(Reverse<RunId>),
    Oldest(RunId),
    Activity(Reverse<Option<u64>>, Reverse<RunId>),
    /// The runs without a wake last, then by run id.
    Wakes(bool, Option<u64>, RunId),
    /// The runs that have not ended last, then newest first.
    Duration(bool, Reverse<Option<u64>>, Reverse<RunId>),
    Calls(Reverse<Option<u64>>, Reverse<RunId>),
}

impl SearchPosition {
    /// The position a cursor names in `order`.
    pub fn of(order: RunOrder, cursor: &SearchCursor) -> Self {
        let (key, run) = (cursor.key, cursor.run);
        Self(match order {
            RunOrder::Started => Position::Started(Reverse(run)),
            RunOrder::Oldest => Position::Oldest(run),
            RunOrder::Activity => Position::Activity(Reverse(key), Reverse(run)),
            RunOrder::Wakes => Position::Wakes(key.is_none(), key, run),
            RunOrder::Duration => Position::Duration(key.is_none(), Reverse(key), Reverse(run)),
            RunOrder::Calls => Position::Calls(Reverse(key), Reverse(run)),
        })
    }

    /// The position of a run's summary in `order`.
    pub fn of_summary(order: RunOrder, summary: &RunSummary) -> Self {
        Self::of(order, &summary.cursor(order))
    }
}

/// Which page of a search to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    /// Read after this run, in the search's order; `None`: from the start.
    pub cursor: Option<SearchCursor>,
    /// Summaries returned at most.
    pub limit: usize,
    /// Summaries read at most, matching or not.
    pub budget: usize,
}

impl Page {
    /// The first page of `limit` summaries, read within [`SCAN_BUDGET`].
    pub fn first(limit: usize) -> Self {
        Self {
            cursor: None,
            limit,
            budget: SCAN_BUDGET,
        }
    }

    /// The page after `cursor`.
    pub fn after(cursor: SearchCursor, limit: usize) -> Self {
        Self {
            cursor: Some(cursor),
            ..Self::first(limit)
        }
    }
}

/// A page of a search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The summaries that match, in the search's order.
    pub summaries: Vec<RunSummary>,
    /// Where the next page starts: the last summary read, when the search stopped before the
    /// end, at its limit or its budget. Only a page without a cursor is the last.
    pub cursor: Option<SearchCursor>,
}
