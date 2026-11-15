//! The typed query of `search_runs`, `run_stats` and `failure_groups` in SQL, and the pages of
//! `search_runs`.
//!
//! A query is a condition on `run_summaries s`: its tenant, then each term. A term holds when
//! its predicate's SQL is true; a negated term holds when it is not true, so a run without the
//! value a predicate reads (its SQL is null) matches the negation. Predicates on what a run has
//! several of (labels, calls, waits, quarantine, its search text) are `EXISTS` subqueries over
//! the run's rows, which also name the run's tenant: the planner can then start from the rows'
//! own index, which starts with the tenant, rather than from every run of the tenant.
//!
//! Every value is bound. Prefixes and text are matched with `LIKE … ESCAPE`, their `%`, `_` and
//! `\` escaped, on columns in the "C" collation, so that a prefix is a range of the index. Id
//! and digest prefixes become ranges of the column too.

use std::collections::BTreeMap;
use std::ops::Bound;

use rewind_core::{Callsite, RunId, RunStatus, Seq, Timestamp};
use rewind_engine::ports::{
    Ending, Predicate, QuarantineNote, Range, RunOrder, RunQuery, RunRef, RunSummary, SearchCursor,
    Term, TextMatch, Wait,
};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::Error;
use crate::codec::{self, decode_labels};
use crate::sql::{Columns, int, limit, ts, uuid};

/// A statement being built, its values bound as they are pushed.
pub(super) type Sql<'a> = QueryBuilder<'a, Postgres>;

/// What joins the lowercase fields of `run_search.search_text`: an uppercase letter, which no
/// lowercased text holds. A text that holds it is in no field; one that does not is found in
/// the search text only inside a field.
pub(super) const SEARCH_SEPARATOR: char = 'A';

/// What a wait of `run_waits w` waits for, as `WaitingFor` names it, with [`WAIT_TASKS`]
/// joined: a call's task waiting for the harness names it, otherwise the wait's kind does.
pub(super) const WAITING_FOR: &str = "CASE WHEN w.kind IN ('call', 'host') \
     AND c.task_state IN ('approval', 'parked', 'deferred') THEN c.task_state ELSE w.kind END";

/// Joins the call of each wait of `run_waits w`, with its task's state, as `tool_calls c`.
pub(super) const WAIT_TASKS: &str =
    "LEFT JOIN tool_calls c ON c.run_id = w.run_id AND c.seq = w.seq";

/// Pushes the condition on `run_summaries s` of the runs a query matches.
pub(super) fn push_matching(sql: &mut Sql<'_>, query: &RunQuery) {
    sql.push("s.tenant = ")
        .push_bind(query.tenant.as_str().to_owned());
    for Term { negated, predicate } in &query.terms {
        sql.push(" AND (");
        push_predicate(sql, predicate);
        sql.push(if *negated { ") IS NOT TRUE" } else { ")" });
    }
}

fn push_predicate(sql: &mut Sql<'_>, predicate: &Predicate) {
    match predicate {
        Predicate::Id(prefixes) => {
            let ranges: Vec<_> = prefixes.iter().filter_map(|p| id_range(p)).collect();
            push_any(sql, &ranges, |sql, (low, high)| {
                sql.push("s.run_id BETWEEN ")
                    .push_bind(*low)
                    .push(" AND ")
                    .push_bind(*high);
            });
        }
        Predicate::Status(statuses) => {
            let names = statuses.iter().map(|status| status.as_str().to_owned());
            push_in(sql, "s.status", names.collect());
        }
        Predicate::Label { key, values } => {
            if values.is_empty() {
                sql.push("FALSE");
                return;
            }
            sql.push(
                "EXISTS (SELECT 1 FROM run_labels l WHERE l.run_id = s.run_id \
                 AND l.tenant = s.tenant AND l.key = ",
            )
            .push_bind(key.clone())
            .push(" AND ");
            push_matches(sql, "l.value", values);
            sql.push(")");
        }
        Predicate::Script(prefixes) => push_digest_prefixes(sql, "s.script", prefixes),
        Predicate::Toolset(prefixes) => push_digest_prefixes(sql, "s.toolset", prefixes),
        Predicate::Tool(matchers) => {
            if matchers.is_empty() {
                sql.push("FALSE");
                return;
            }
            sql.push(
                "EXISTS (SELECT 1 FROM tool_calls c WHERE c.run_id = s.run_id \
                 AND c.tenant = s.tenant AND c.tool IS NOT NULL AND ",
            );
            push_matches(sql, "c.tool", matchers);
            sql.push(")");
        }
        Predicate::Error { names, kinds } => {
            if names.is_empty() && kinds.is_empty() {
                sql.push("FALSE");
                return;
            }
            if !names.is_empty() {
                let names: Vec<String> = names.iter().cloned().collect();
                sql.push("s.error = ANY(").push_bind(names).push(")");
            }
            if !kinds.is_empty() {
                if !names.is_empty() {
                    sql.push(" OR ");
                }
                let kinds: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
                sql.push(
                    "EXISTS (SELECT 1 FROM tool_calls c WHERE c.run_id = s.run_id \
                     AND c.tenant = s.tenant AND c.outcome = 'failed' AND c.error_kind = ANY(",
                )
                .push_bind(kinds)
                .push("))");
            }
        }
        Predicate::Waiting(kinds) => {
            sql.push("s.status = 'active' AND EXISTS (SELECT 1 FROM run_waits w ")
                .push(WAIT_TASKS)
                .push(" WHERE w.run_id = s.run_id AND w.tenant = s.tenant");
            if !kinds.is_empty() {
                let kinds: Vec<&str> = kinds.iter().map(|kind| kind.as_str()).collect();
                sql.push(" AND ")
                    .push(WAITING_FOR)
                    .push(" = ANY(")
                    .push_bind(kinds)
                    .push(")");
            }
            sql.push(")");
        }
        Predicate::Signal(matchers) => {
            if matchers.is_empty() {
                sql.push("FALSE");
                return;
            }
            sql.push(
                "s.status = 'active' AND EXISTS (SELECT 1 FROM run_waits w \
                 WHERE w.run_id = s.run_id AND w.tenant = s.tenant AND w.kind = 'signal' \
                 AND w.name IS NOT NULL AND ",
            );
            push_matches(sql, "w.name", matchers);
            sql.push(")");
        }
        Predicate::Started(range) => push_range(sql, "s.started_at", times(range)),
        Predicate::Ended(range) => push_range(sql, "s.ended_at", times(range)),
        Predicate::Activity(range) => push_range(sql, "s.last_activity_at", times(range)),
        Predicate::Duration(range) => {
            sql.push("s.started_at IS NOT NULL AND s.ended_at IS NOT NULL AND ");
            push_range(sql, "GREATEST(s.ended_at - s.started_at, 0)", counts(range));
        }
        Predicate::Calls(range) => push_range(sql, "s.tool_calls", counts(range)),
        Predicate::Errors(range) => push_range(sql, "s.errors", counts(range)),
        Predicate::Parent(parent) => {
            sql.push("s.parent = ").push_bind(uuid(*parent));
        }
        Predicate::Workflow(matchers) => {
            if matchers.is_empty() {
                sql.push("FALSE");
                return;
            }
            sql.push("s.workflow IS NOT NULL AND ");
            push_matches(sql, "s.workflow", matchers);
        }
        Predicate::StartedBy(run) => {
            sql.push("s.started_by = ").push_bind(uuid(*run));
        }
        Predicate::Fingerprint(fingerprints) => {
            push_in(sql, "s.fingerprint", fingerprints.clone());
        }
        Predicate::Quarantined => {
            sql.push(
                "EXISTS (SELECT 1 FROM run_quarantine q \
                 WHERE q.run_id = s.run_id AND q.tenant = s.tenant AND q.kind IS NOT NULL)",
            );
        }
        Predicate::Mode(modes) => {
            let names = modes.iter().map(|mode| codec::mode(*mode).to_owned());
            push_in(sql, "s.mode", names.collect());
        }
        Predicate::Text(text) => {
            if text.contains(SEARCH_SEPARATOR) {
                sql.push("FALSE");
                return;
            }
            sql.push(
                "EXISTS (SELECT 1 FROM run_search t WHERE t.run_id = s.run_id \
                 AND t.search_text LIKE ",
            )
            .push_bind(format!("%{}%", like_literal(text)))
            .push(" ESCAPE '\\')");
        }
    }
}

/// `column = ANY(values)`; false without values.
fn push_in(sql: &mut Sql<'_>, column: &str, values: Vec<String>) {
    if values.is_empty() {
        sql.push("FALSE");
    } else {
        sql.push(column).push(" = ANY(").push_bind(values).push(")");
    }
}

/// Each item's condition, joined by `OR`; false without items.
fn push_any<T>(sql: &mut Sql<'_>, items: &[T], mut push: impl FnMut(&mut Sql<'_>, &T)) {
    if items.is_empty() {
        sql.push("FALSE");
        return;
    }
    sql.push("(");
    for (n, item) in items.iter().enumerate() {
        if n > 0 {
            sql.push(" OR ");
        }
        push(sql, item);
    }
    sql.push(")");
}

/// Whether `column` matches one of the matchers (at least one).
fn push_matches(sql: &mut Sql<'_>, column: &str, matchers: &[TextMatch]) {
    push_any(sql, matchers, |sql, matcher| match matcher {
        TextMatch::Any => {
            sql.push("TRUE");
        }
        TextMatch::Exact(value) => {
            sql.push(column).push(" = ").push_bind(value.clone());
        }
        TextMatch::Prefix(prefix) => {
            sql.push(column)
                .push(" LIKE ")
                .push_bind(format!("{}%", like_literal(prefix)))
                .push(" ESCAPE '\\'");
        }
    });
}

fn push_digest_prefixes(sql: &mut Sql<'_>, column: &str, prefixes: &[String]) {
    let ranges: Vec<_> = prefixes.iter().filter_map(|p| digest_range(p)).collect();
    push_any(sql, &ranges, |sql, (low, high)| {
        sql.push(column)
            .push(" BETWEEN ")
            .push_bind(low.clone())
            .push(" AND ")
            .push_bind(high.clone());
    });
}

/// `column` is set and within the bounds.
fn push_range(sql: &mut Sql<'_>, column: &str, (start, end): (Bound<i64>, Bound<i64>)) {
    sql.push(column).push(" IS NOT NULL");
    let mut bound = |op: &str, value: i64| {
        sql.push(" AND ").push(column).push(op).push_bind(value);
    };
    match start {
        Bound::Included(value) => bound(" >= ", value),
        Bound::Excluded(value) => bound(" > ", value),
        Bound::Unbounded => {}
    }
    match end {
        Bound::Included(value) => bound(" <= ", value),
        Bound::Excluded(value) => bound(" < ", value),
        Bound::Unbounded => {}
    }
}

fn times(range: &Range<Timestamp>) -> (Bound<i64>, Bound<i64>) {
    (range.start.map(ts), range.end.map(ts))
}

fn counts(range: &Range<u64>) -> (Bound<i64>, Bound<i64>) {
    (range.start.map(int), range.end.map(int))
}

/// A text as a `LIKE` pattern that matches it literally, with `\` as the escape.
pub(super) fn like_literal(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern
}

fn lower_hex(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        'a'..='f' => Some(c as u8 - b'a' + 10),
        _ => None,
    }
}

/// The lowest and highest values of `N` bytes whose hexadecimal starts with these digits.
fn nibble_range<const N: usize>(nibbles: &[u8]) -> ([u8; N], [u8; N]) {
    let mut low = [0x00; N];
    let mut high = [0xff; N];
    for (n, nibble) in nibbles.iter().enumerate() {
        let byte = n / 2;
        if n % 2 == 0 {
            low[byte] = nibble << 4;
            high[byte] = (nibble << 4) | 0x0f;
        } else {
            low[byte] |= nibble;
            high[byte] = (high[byte] & 0xf0) | nibble;
        }
    }
    (low, high)
}

/// The run ids whose hyphenated lowercase text starts with `prefix`, as a range; `None` when
/// no id does.
fn id_range(prefix: &str) -> Option<(Uuid, Uuid)> {
    if prefix.len() > 36 {
        return None;
    }
    let mut nibbles = Vec::with_capacity(32);
    for (at, c) in prefix.chars().enumerate() {
        if matches!(at, 8 | 13 | 18 | 23) {
            if c != '-' {
                return None;
            }
        } else {
            nibbles.push(lower_hex(c)?);
        }
    }
    let (low, high) = nibble_range::<16>(&nibbles);
    Some((Uuid::from_bytes(low), Uuid::from_bytes(high)))
}

/// The digests whose lowercase hexadecimal starts with `prefix`, as a range; `None` when no
/// digest does.
fn digest_range(prefix: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    if prefix.len() > 64 {
        return None;
    }
    let nibbles: Vec<u8> = prefix.chars().map(lower_hex).collect::<Option<_>>()?;
    let (low, high) = nibble_range::<32>(&nibbles);
    Some((low.to_vec(), high.to_vec()))
}

// -------------------------------------------------------------------------------------------
// Pages of runs

/// The columns of a summary, read by [`summary_from_row`]: `run_summaries s` with its
/// quarantine, `run_quarantine q`, joined.
const SUMMARY_COLUMNS: &str = "s.run_id, s.tenant, s.script, s.toolset, s.headline, s.status, \
     s.mode, s.labels::text, s.parent, s.workflow, s.started_by, s.started_at, s.ended_at, \
     s.last_activity_at, s.tool_calls, s.errors, s.history_len, s.wakes_at, s.error, s.message, \
     s.line, s.signature, s.fingerprint, q.kind, q.reason, q.since";

fn summary_from_row(row: &PgRow) -> Result<RunSummary, Error> {
    let ending = match row.opt_text(18)? {
        Some(error) => Some(Ending {
            error,
            message: row.text(19)?,
            line: row.opt_u32(20)?,
            signature: row.text(21)?,
            fingerprint: row.text(22)?,
        }),
        None => None,
    };
    let quarantine = match row.opt_text(23)? {
        Some(kind) => Some(QuarantineNote {
            kind: codec::parse_quarantine_kind(&kind)?,
            reason: row.text(24)?,
            since: row.ts(25)?,
        }),
        None => None,
    };
    Ok(RunSummary {
        run: RunRef::new(row.tenant(1)?, row.run_id(0)?),
        script: row.digest(2)?,
        toolset: row.digest(3)?,
        headline: row.text(4)?,
        status: codec::parse_status(&row.text(5)?)?,
        mode: row
            .opt_text(6)?
            .map(|m| codec::parse_mode(&m))
            .transpose()?,
        labels: decode_labels(&row.text(7)?)?,
        parent: row.opt_run_id(8)?,
        workflow: row.opt_text(9)?,
        started_by: row.opt_run_id(10)?,
        started_at: row.opt_ts(11)?,
        ended_at: row.opt_ts(12)?,
        last_activity_at: row.ts(13)?,
        tool_calls: row.u64(14)?,
        errors: row.u64(15)?,
        history_len: row.u64(16)?,
        waits: Vec::new(),
        wakes_at: row.opt_ts(17)?,
        ending,
        quarantine,
    })
}

/// A page of the runs a query matches, in `order`, after `after`.
pub(super) async fn search_runs(
    pool: &PgPool,
    query: &RunQuery,
    order: RunOrder,
    after: Option<&SearchCursor>,
    page: usize,
) -> Result<Vec<RunSummary>, Error> {
    if page == 0 {
        return Ok(Vec::new());
    }
    let mut sql = Sql::new("SELECT ");
    sql.push(SUMMARY_COLUMNS).push(
        " FROM run_summaries s \
         LEFT JOIN run_quarantine q ON q.run_id = s.run_id AND q.kind IS NOT NULL WHERE ",
    );
    push_matching(&mut sql, query);
    match (order, after) {
        (RunOrder::Started, Some(after)) => {
            sql.push(" AND s.run_id < ").push_bind(uuid(after.run));
        }
        (RunOrder::Activity, Some(after)) => {
            // Every run has an activity: a position without one comes after them all.
            let Some(key) = after.key else {
                return Ok(Vec::new());
            };
            sql.push(" AND (s.last_activity_at, s.run_id) < (")
                .push_bind(ts(key))
                .push(", ")
                .push_bind(uuid(after.run))
                .push(")");
        }
        (RunOrder::Wakes, Some(after)) => match after.key {
            Some(key) => {
                sql.push(" AND ((s.wakes_at, s.run_id) > (")
                    .push_bind(ts(key))
                    .push(", ")
                    .push_bind(uuid(after.run))
                    .push(") OR s.wakes_at IS NULL)");
            }
            None => {
                sql.push(" AND s.wakes_at IS NULL AND s.run_id > ")
                    .push_bind(uuid(after.run));
            }
        },
        (_, None) => {}
    }
    sql.push(match order {
        RunOrder::Started => " ORDER BY s.run_id DESC",
        RunOrder::Activity => " ORDER BY s.last_activity_at DESC, s.run_id DESC",
        RunOrder::Wakes => " ORDER BY s.wakes_at ASC NULLS LAST, s.run_id ASC",
    });
    sql.push(" LIMIT ").push_bind(limit(page));
    let rows = sql.build().persistent(false).fetch_all(pool).await?;
    let mut summaries: Vec<RunSummary> = rows
        .iter()
        .map(summary_from_row)
        .collect::<Result<_, _>>()?;
    // An active run shows its waits; any other waits for nothing.
    let active: Vec<Uuid> = summaries
        .iter()
        .filter(|summary| summary.status == RunStatus::Active)
        .map(|summary| uuid(summary.run.id))
        .collect();
    if !active.is_empty() {
        let mut waits = waits_of(pool, &active).await?;
        for summary in &mut summaries {
            if let Some(waits) = waits.remove(&summary.run.id) {
                summary.waits = waits;
            }
        }
    }
    Ok(summaries)
}

/// The waits of these runs, in their snapshot's order, each call's with its task's state.
async fn waits_of(pool: &PgPool, runs: &[Uuid]) -> Result<BTreeMap<RunId, Vec<Wait>>, Error> {
    let rows = sqlx::query(
        "SELECT w.run_id, w.kind, w.seq, w.name, w.since, w.until, w.callsite_line, \
             w.callsite_column, CASE WHEN w.kind IN ('call', 'host') THEN c.task_state END \
         FROM run_waits w LEFT JOIN tool_calls c ON c.run_id = w.run_id AND c.seq = w.seq \
         WHERE w.run_id = ANY($1) ORDER BY w.run_id, w.position",
    )
    .bind(runs)
    .fetch_all(pool)
    .await?;
    let mut waits: BTreeMap<RunId, Vec<Wait>> = BTreeMap::new();
    for row in &rows {
        let callsite = match (row.opt_u32(6)?, row.opt_u32(7)?) {
            (Some(line), Some(column)) => Some(Callsite { line, column }),
            _ => None,
        };
        let wait = Wait {
            kind: codec::parse_wait_kind(&row.text(1)?)?,
            seq: row.opt_u32(2)?.map(Seq),
            name: row.opt_text(3)?,
            since: row.ts(4)?,
            until: row.opt_ts(5)?,
            callsite,
            task: row
                .opt_text(8)?
                .map(|phase| codec::parse_task_phase(&phase))
                .transpose()?,
        };
        waits.entry(row.run_id(0)?).or_default().push(wait);
    }
    Ok(waits)
}

#[cfg(test)]
mod tests {
    use super::{digest_range, id_range, like_literal};
    use rewind_core::{Digest, RunId};

    #[test]
    fn prefixes_become_ranges() {
        let id = RunId::from_u128(0x0193_a5c4_0000_7000_8000_0000_0000_0004);
        let text = id.to_string();
        for len in 0..=text.len() {
            let (low, high) = id_range(&text[..len]).expect("a prefix of an id");
            assert!(
                low <= id.as_uuid() && id.as_uuid() <= high,
                "{}",
                &text[..len]
            );
        }
        assert_eq!(id_range(&text), Some((id.as_uuid(), id.as_uuid())));
        let (low, high) = id_range("0193a5c").expect("prefix");
        assert_eq!(low.to_string(), "0193a5c0-0000-0000-0000-000000000000");
        assert_eq!(high.to_string(), "0193a5cf-ffff-ffff-ffff-ffffffffffff");
        assert_eq!(id_range("0193A5C4"), None, "uppercase");
        assert_eq!(id_range("0193a5c40"), None, "a digit where the hyphen is");
        assert_eq!(id_range(&format!("{text}0")), None, "longer than an id");

        let digest = Digest::of(b"script");
        let hex = digest.to_hex();
        let (low, high) = digest_range(&hex[..9]).expect("prefix");
        let bytes = digest.as_bytes().as_slice();
        assert!(low.as_slice() <= bytes && bytes <= high.as_slice());
        assert_eq!(
            digest_range(&hex),
            Some((digest.as_bytes().to_vec(), digest.as_bytes().to_vec()))
        );
        assert_eq!(digest_range("not a digest"), None);
        assert_eq!(digest_range(&format!("{hex}0")), None);
    }

    #[test]
    fn like_patterns_match_literally() {
        assert_eq!(like_literal(r"100%_a\b"), r"100\%\_a\\b");
        assert_eq!(like_literal("plain"), "plain");
    }
}
