//! A [`RunQuery`] as SQL: the `WHERE` clause over `run_summaries s`, with bound parameters,
//! and the orders of `search_runs` with their cursors.
//!
//! - **Negation.** Each term is `(CASE WHEN predicate THEN 1 ELSE 0 END)`, compared to 1, or to
//!   0 when negated. A predicate that reads a value the run lacks (`s.ended_at` of a run that
//!   has not ended, `s.mode` before `run_started`) is NULL in SQL: the `CASE` counts it as
//!   false, so the run matches the negation, where `NOT (…)` would be NULL and drop it. Lists
//!   are "any of": an empty one matches nothing.
//! - **Literal matches.** A prefix (of an id, a digest, a label's value, a tool, a signal, a
//!   workflow's name) is `instr(column, prefix) = 1`, and free text `instr(text, needle) > 0`
//!   over `run_texts`, lowercase on both sides: Turso's `instr` searches the whole strings as
//!   they are, so these are `str::starts_with` and `str::contains`. `%`, `_` and `\` are plain
//!   characters. Turso's `LIKE` would fold ASCII case (prefixes are case-sensitive), stop at
//!   the first NUL of the text or the pattern, and refuse a pattern over 50,000 bytes.
//! - **Orders.** Run ids are hyphenated lowercase UUIDs and digests lowercase hex, so text
//!   order is their order (`crate::sql`). A cursor is a position in the order, not a run: the
//!   page starts after it whether or not the run still matches.

use std::ops::Bound;

use rewind_engine::ports::{Predicate, Range, RunOrder, RunQuery, SearchCursor, TextMatch};
use turso::Value;

use crate::codec;
use crate::sql::{int, run_id, tenant, text, ts};

/// SQL text and its parameters, bound in the order of their `?`.
#[derive(Debug, Default)]
pub(super) struct Sql {
    pub(super) text: String,
    pub(super) params: Vec<Value>,
}

impl Sql {
    fn push(&mut self, sql: &str) -> &mut Self {
        self.text.push_str(sql);
        self
    }

    /// A `?` bound to `value`.
    fn bind(&mut self, value: Value) -> &mut Self {
        self.text.push('?');
        self.params.push(value);
        self
    }
}

/// What a wait of `run_waits w`, joined to its call's `tool_calls c`, waits for: the names of
/// [`rewind_engine::ports::WaitingFor`] (`Wait::waiting_for`).
const WAITING_FOR: &str = "CASE WHEN w.kind IN ('call', 'host') \
     AND c.task_phase IN ('approval', 'parked', 'deferred') THEN c.task_phase ELSE w.kind END";

/// From start to end, in milliseconds, at least 0; NULL until the run has both.
const DURATION: &str = "CASE WHEN s.ended_at >= s.started_at THEN s.ended_at - s.started_at \
     WHEN s.ended_at < s.started_at THEN 0 END";

/// `s.tenant = ?` and every term of the query.
pub(super) fn filter(query: &RunQuery) -> Sql {
    let mut sql = Sql::default();
    sql.push("s.tenant = ").bind(tenant(&query.tenant));
    for term in &query.terms {
        sql.push(" AND (CASE WHEN ");
        predicate(&mut sql, &term.predicate);
        sql.push(if term.negated {
            " THEN 1 ELSE 0 END) = 0"
        } else {
            " THEN 1 ELSE 0 END) = 1"
        });
    }
    sql
}

/// The `ORDER BY` of an order: `wakes` puts the runs without a wake last.
pub(super) fn order_by(order: RunOrder) -> &'static str {
    match order {
        RunOrder::Started => "s.run_id DESC",
        RunOrder::Activity => "s.last_activity_at DESC, s.run_id DESC",
        RunOrder::Wakes => "s.wakes_at IS NULL, s.wakes_at, s.run_id",
    }
}

/// Restricts `sql` to the runs after `cursor` in `order`.
pub(super) fn after(sql: &mut Sql, order: RunOrder, cursor: &SearchCursor) {
    let id = || run_id(cursor.run);
    match (order, cursor.key) {
        (RunOrder::Started, _) => {
            sql.push(" AND s.run_id < ").bind(id());
        }
        (RunOrder::Activity, Some(key)) => {
            sql.push(" AND (s.last_activity_at < ")
                .bind(ts(key))
                .push(" OR (s.last_activity_at = ")
                .bind(ts(key))
                .push(" AND s.run_id < ")
                .bind(id())
                .push("))");
        }
        // Every run has an activity, and a position without one is after all of them.
        (RunOrder::Activity, None) => {
            sql.push(" AND 0 = 1");
        }
        (RunOrder::Wakes, Some(key)) => {
            sql.push(" AND (s.wakes_at IS NULL OR s.wakes_at > ")
                .bind(ts(key))
                .push(" OR (s.wakes_at = ")
                .bind(ts(key))
                .push(" AND s.run_id > ")
                .bind(id())
                .push("))");
        }
        (RunOrder::Wakes, None) => {
            sql.push(" AND s.wakes_at IS NULL AND s.run_id > ")
                .bind(id());
        }
    }
}

fn predicate(sql: &mut Sql, predicate: &Predicate) {
    match predicate {
        Predicate::Id(prefixes) => any_prefix(sql, "s.run_id", prefixes),
        Predicate::Status(statuses) => {
            one_of(sql, "s.status", statuses.iter().map(|s| text(s.as_str())))
        }
        Predicate::Label { key, values } => {
            sql.push("EXISTS (SELECT 1 FROM run_labels l WHERE l.run_id = s.run_id AND l.key = ")
                .bind(text(key.as_str()))
                .push(" AND ");
            any_match(sql, "l.value", values);
            sql.push(")");
        }
        Predicate::Script(prefixes) => any_prefix(sql, "s.script", prefixes),
        Predicate::Toolset(prefixes) => any_prefix(sql, "s.toolset", prefixes),
        Predicate::Tool(matchers) => {
            sql.push(
                "EXISTS (SELECT 1 FROM tool_calls c WHERE c.run_id = s.run_id \
                 AND c.tool IS NOT NULL AND ",
            );
            any_match(sql, "c.tool", matchers);
            sql.push(")");
        }
        Predicate::Error { names, kinds } => {
            sql.push("(");
            one_of(
                sql,
                "s.ending_error",
                names.iter().map(|name| text(name.as_str())),
            );
            sql.push(" OR EXISTS (SELECT 1 FROM tool_calls c WHERE c.run_id = s.run_id AND ");
            one_of(
                sql,
                "c.error_kind",
                kinds.iter().map(|kind| text(kind.as_str())),
            );
            sql.push("))");
        }
        Predicate::Waiting(kinds) => {
            sql.push(
                "s.status = 'active' AND EXISTS (SELECT 1 FROM run_waits w \
                 LEFT JOIN tool_calls c ON c.run_id = w.run_id AND c.seq = w.seq \
                 WHERE w.run_id = s.run_id",
            );
            if !kinds.is_empty() {
                sql.push(" AND ");
                one_of(
                    sql,
                    WAITING_FOR,
                    kinds.iter().map(|kind| text(kind.as_str())),
                );
            }
            sql.push(")");
        }
        Predicate::Signal(matchers) => {
            sql.push(
                "s.status = 'active' AND EXISTS (SELECT 1 FROM run_waits w \
                 WHERE w.run_id = s.run_id AND w.kind = 'signal' AND w.name IS NOT NULL AND ",
            );
            any_match(sql, "w.name", matchers);
            sql.push(")");
        }
        Predicate::Started(range) => within(sql, "s.started_at", range, ts),
        Predicate::Ended(range) => within(sql, "s.ended_at", range, ts),
        Predicate::Activity(range) => within(sql, "s.last_activity_at", range, ts),
        Predicate::Duration(range) => within(sql, DURATION, range, int),
        Predicate::Calls(range) => within(sql, "s.tool_calls", range, int),
        Predicate::Errors(range) => within(sql, "s.errors", range, int),
        Predicate::Parent(parent) => {
            sql.push("s.parent = ").bind(run_id(*parent));
        }
        // `any` too needs a workflow.
        Predicate::Workflow(matchers) => {
            sql.push("s.workflow IS NOT NULL AND ");
            any_match(sql, "s.workflow", matchers);
        }
        Predicate::StartedBy(starter) => {
            sql.push("s.started_by = ").bind(run_id(*starter));
        }
        Predicate::Fingerprint(fingerprints) => one_of(
            sql,
            "s.fingerprint",
            fingerprints.iter().map(|fp| text(fp.as_str())),
        ),
        Predicate::Quarantined => {
            sql.push(
                "EXISTS (SELECT 1 FROM run_quarantine q \
                 WHERE q.run_id = s.run_id AND q.kind IS NOT NULL)",
            );
        }
        Predicate::Mode(modes) => one_of(
            sql,
            "s.mode",
            modes.iter().map(|mode| text(codec::run_mode(*mode))),
        ),
        Predicate::Text(needle) => {
            sql.push(
                "EXISTS (SELECT 1 FROM run_texts t WHERE t.run_id = s.run_id AND instr(t.text, ",
            )
            .bind(text(needle.as_str()))
            .push(") > 0)");
        }
    }
}

/// `expr IN (…)`; nothing for an empty list.
fn one_of(sql: &mut Sql, expr: &str, values: impl IntoIterator<Item = Value>) {
    let mut values = values.into_iter().peekable();
    if values.peek().is_none() {
        sql.push("0 = 1");
        return;
    }
    sql.push("(").push(expr).push(") IN (");
    for (i, value) in values.enumerate() {
        if i > 0 {
            sql.push(", ");
        }
        sql.bind(value);
    }
    sql.push(")");
}

/// `column` starts with one of `prefixes`.
fn any_prefix(sql: &mut Sql, column: &str, prefixes: &[String]) {
    either(sql, prefixes, |sql, prefix| {
        starts_with(sql, column, prefix)
    });
}

/// `column` matches one of `matchers`.
fn any_match(sql: &mut Sql, column: &str, matchers: &[TextMatch]) {
    either(sql, matchers, |sql, matcher| match matcher {
        TextMatch::Any => {
            sql.push("1 = 1");
        }
        TextMatch::Exact(value) => {
            sql.push(column).push(" = ").bind(text(value.as_str()));
        }
        TextMatch::Prefix(prefix) => starts_with(sql, column, prefix),
    });
}

/// The disjunction of one condition per item; nothing for no item.
fn either<T>(sql: &mut Sql, items: &[T], mut condition: impl FnMut(&mut Sql, &T)) {
    if items.is_empty() {
        sql.push("0 = 1");
        return;
    }
    sql.push("(");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            sql.push(" OR ");
        }
        condition(sql, item);
    }
    sql.push(")");
}

/// `column` starts with `prefix`, compared as is: its first occurrence is at the start.
fn starts_with(sql: &mut Sql, column: &str, prefix: &str) {
    sql.push("instr(")
        .push(column)
        .push(", ")
        .bind(text(prefix))
        .push(") = 1");
}

/// `expr` is set and within `range`.
fn within<T: Copy>(sql: &mut Sql, expr: &str, range: &Range<T>, value: fn(T) -> Value) {
    sql.push("(").push(expr).push(") IS NOT NULL");
    for (bound, included, excluded) in [(range.start, ">=", ">"), (range.end, "<=", "<")] {
        let (op, at) = match bound {
            Bound::Included(at) => (included, at),
            Bound::Excluded(at) => (excluded, at),
            Bound::Unbounded => continue,
        };
        sql.push(" AND (")
            .push(expr)
            .push(") ")
            .push(op)
            .push(" ")
            .bind(value(at));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rewind_core::{RunStatus, Timestamp};
    use rewind_engine::ports::{RunQuery, TenantId};

    use super::*;

    fn tenant_id() -> TenantId {
        TenantId::new("acme").expect("tenant")
    }

    #[test]
    fn terms_bind_their_values_in_order() {
        let query = RunQuery::new(tenant_id())
            .with(Predicate::Status(BTreeSet::from([RunStatus::Failed])))
            .without(Predicate::Started(Range {
                start: Bound::Included(Timestamp(10)),
                end: Bound::Unbounded,
            }));
        let sql = filter(&query);
        assert_eq!(
            sql.text,
            "s.tenant = ? AND (CASE WHEN (s.status) IN (?) THEN 1 ELSE 0 END) = 1 \
             AND (CASE WHEN (s.started_at) IS NOT NULL AND (s.started_at) >= ? \
             THEN 1 ELSE 0 END) = 0"
        );
        assert_eq!(
            sql.params,
            [
                Value::Text("acme".into()),
                Value::Text("failed".into()),
                Value::Integer(10),
            ]
        );
        assert_eq!(sql.text.matches('?').count(), sql.params.len());
    }

    #[test]
    fn empty_lists_match_nothing() {
        let query = RunQuery::new(tenant_id())
            .with(Predicate::Mode(Vec::new()))
            .with(Predicate::Tool(Vec::new()));
        let sql = filter(&query);
        assert_eq!(sql.text.matches("0 = 1").count(), 2, "{}", sql.text);
    }
}
