//! The query language of `GET /v1/runs`, `GET /v1/runs/stats` and `GET /v1/runs/failures`
//! (`archi/05-testing-and-observability.md`, "Query language").
//!
//! A query is terms separated by spaces, all of which must hold. A term is a label
//! (`@agent:triage`), a field (`status:failed,aborted`, `started:>now-24h`) or text (`refund`,
//! `"rate limit"`); a leading `-` excludes, a comma inside a term means "or". [`parse`] reads a
//! query into typed terms, or says which term is wrong and where, for a client to underline it.
//! The server resolves `now` and limits the query to the caller's tenant and labels.

use std::fmt;
use std::ops::Bound;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use winnow::combinator::{alt, opt};
use winnow::prelude::*;
use winnow::token::take_while;

use crate::common::{Duration, RunMode, RunStatus, Timestamp};
use crate::namespaces::NamespaceName;

/// The fields of the language, in the order error messages list them.
pub const FIELDS: [&str; 21] = [
    "status",
    "action",
    "error",
    "waiting",
    "signal",
    "started",
    "ended",
    "activity",
    "duration",
    "calls",
    "errors",
    "script",
    "action_set",
    "id",
    "parent",
    "workflow",
    "started_by",
    "fingerprint",
    "quarantined",
    "mode",
    "namespace",
];

/// A query: every term must hold. Empty, it matches every run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub terms: Vec<Term>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    /// Written with a leading `-`: the term must not hold.
    pub negated: bool,
    pub filter: Filter,
    /// Where the term is in the query.
    pub span: QuerySpan,
}

/// A position in a query, in characters (Unicode scalar values) from 0, `end` excluded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuerySpan {
    pub start: u32,
    pub end: u32,
}

/// What one term asks of a run. A list means "any of".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    /// In one of these namespaces (exact names).
    Namespace(Vec<NamespaceName>),
    /// `@key:value`, `@key:prefix*`, `@key:*`.
    Label {
        key: String,
        values: Vec<Pattern>,
    },
    Status(Vec<RunStatus>),
    /// Scheduled a call to an action one of these matches.
    Action(Vec<Pattern>),
    /// A call failed with this kind, or the run ended with this error name or abort kind.
    Error(Vec<String>),
    /// Waits for one of these; empty (`waiting:*`): for anything.
    Waiting(Vec<WaitKind>),
    /// Waits for a signal one of these matches.
    Signal(Vec<Pattern>),
    Started(Comparison<TimeValue>),
    Ended(Comparison<TimeValue>),
    Activity(Comparison<TimeValue>),
    /// In milliseconds.
    Duration(Comparison<u64>),
    Calls(Comparison<u64>),
    Errors(Comparison<u64>),
    /// Lowercase hexadecimal prefixes of the script's digest.
    Script(Vec<String>),
    /// Lowercase hexadecimal prefixes of the action set's digest.
    ActionSet(Vec<String>),
    /// Lowercase prefixes of the run's id, hyphenated as a UUID is.
    Id(Vec<String>),
    /// Forks of this run (a full run id, lowercase).
    Parent(String),
    /// Runs of a workflow one of these matches.
    Workflow(Vec<Pattern>),
    /// Runs this run started (a full run id, lowercase).
    StartedBy(String),
    /// Lowercase fingerprints of 16 hexadecimal digits.
    Fingerprint(Vec<String>),
    Quarantined(bool),
    Mode(Vec<RunMode>),
    /// Text, as written: matched case-insensitively.
    Text(String),
}

/// How a value of a label, an action or a signal is matched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// `*`: any value.
    Any,
    Exact(String),
    /// `prefix*`.
    Prefix(String),
}

/// What a waiting run waits for (`archi/05-testing-and-observability.md`, "Read model").
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    /// `await signal(name)` with an empty mailbox.
    Signal,
    /// A durable timer.
    Timer,
    /// A call Rewind sends itself.
    Call,
    /// A harness action's call, answered by the harness's worker.
    Harness,
    /// A call under the `approval` policy, not approved yet.
    Approval,
    /// A call parked by a schema change, a refused call or a missing secret.
    Parked,
    /// A harness action's call whose handler answers later.
    Deferred,
}

impl WaitKind {
    pub const ALL: [WaitKind; 7] = [
        WaitKind::Signal,
        WaitKind::Timer,
        WaitKind::Call,
        WaitKind::Harness,
        WaitKind::Approval,
        WaitKind::Parked,
        WaitKind::Deferred,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            WaitKind::Signal => "signal",
            WaitKind::Timer => "timer",
            WaitKind::Call => "call",
            WaitKind::Harness => "harness",
            WaitKind::Approval => "approval",
            WaitKind::Parked => "parked",
            WaitKind::Deferred => "deferred",
        }
    }
}

/// A range of values: `>v`, `>=v`, `<v`, `<=v`, `a..b` (`a` included, `b` excluded, either
/// may be left out), or a value alone: exactly it for a count, that day (UTC) for a date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Comparison<T> {
    pub start: Bound<T>,
    pub end: Bound<T>,
}

/// A time in a query: an RFC 3339 time or a date (midnight UTC), or `now` minus a duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeValue {
    At(Timestamp),
    /// `now`, `now-24h`: this long before the server's now.
    Ago(Duration),
}

impl TimeValue {
    /// The time this value names, given the server's now.
    pub fn resolve(self, now: Timestamp) -> Timestamp {
        match self {
            TimeValue::At(at) => at,
            TimeValue::Ago(ago) => now
                .checked_sub(jiff::SignedDuration::from_millis(
                    i64::try_from(ago.as_millis()).unwrap_or(i64::MAX),
                ))
                .unwrap_or(Timestamp::MIN),
        }
    }
}

/// Why a query cannot be read, and the term at fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    /// Written for a person or a model: what is wrong, and what is accepted instead.
    pub message: String,
    pub span: QuerySpan,
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (characters {} to {})",
            self.message, self.span.start, self.span.end
        )
    }
}

impl std::error::Error for QueryError {}

impl FromStr for Query {
    type Err = QueryError;

    fn from_str(text: &str) -> Result<Self, QueryError> {
        parse(text)
    }
}

/// Reads a query.
pub fn parse(text: &str) -> Result<Query, QueryError> {
    let mut terms = Vec::new();
    for (start, end) in split_terms(text)? {
        let span = QuerySpan {
            start: chars(text, start),
            end: chars(text, end),
        };
        let fail = |message: String| QueryError { message, span };
        let raw = &text[start..end];
        let (negated, body) =
            match raw.strip_prefix('-') {
                Some("") => return Err(fail(
                    "a lone \"-\": write the term to exclude right after it, as -status:completed"
                        .into(),
                )),
                Some(rest) => (true, rest),
                None => (false, raw),
            };
        let filter = term(body).map_err(fail)?;
        terms.push(Term {
            negated,
            filter,
            span,
        });
    }
    Ok(Query { terms })
}

/// The byte ranges of the terms: runs of characters that are not whitespace, where whitespace
/// inside double quotes does not count.
fn split_terms(text: &str) -> Result<Vec<(usize, usize)>, QueryError> {
    let mut terms = Vec::new();
    let mut start = None;
    let mut quote: Option<usize> = None;
    let mut escaped = false;
    for (at, c) in text.char_indices() {
        if quote.is_some() {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quote = None;
            }
            continue;
        }
        if c.is_whitespace() {
            if let Some(from) = start.take() {
                terms.push((from, at));
            }
            continue;
        }
        if start.is_none() {
            start = Some(at);
        }
        if c == '"' {
            quote = Some(at);
        }
    }
    if let Some(opened) = quote {
        return Err(QueryError {
            message: "a quote is not closed: end the text with \"".into(),
            span: QuerySpan {
                start: chars(text, opened),
                end: chars(text, text.len()),
            },
        });
    }
    if let Some(from) = start {
        terms.push((from, text.len()));
    }
    Ok(terms)
}

fn chars(text: &str, byte: usize) -> u32 {
    u32::try_from(text[..byte].chars().count()).unwrap_or(u32::MAX)
}

/// One term, without its `-`.
fn term(body: &str) -> Result<Filter, String> {
    if body.starts_with('"') {
        let (text, rest) = quoted(body)?;
        if !rest.is_empty() {
            return Err(format!(
                "unexpected {rest:?} after the quoted text: separate terms with a space"
            ));
        }
        if text.trim().is_empty() {
            return Err("empty text: write something between the quotes".into());
        }
        return Ok(Filter::Text(text));
    }
    if let Some(label) = body.strip_prefix('@') {
        let Some((key, values)) = label.split_once(':') else {
            return Err(format!(
                "a label is written @key:value, @key:prefix* or @key:*, not @{label}"
            ));
        };
        if key.is_empty() || key.contains('"') {
            return Err("a label needs a key: @key:value".into());
        }
        return Ok(Filter::Label {
            key: key.to_owned(),
            values: patterns(values)?,
        });
    }
    let quote = body.find('"').unwrap_or(body.len());
    let Some(colon) = body[..quote].find(':') else {
        if quote < body.len() {
            return Err("a quote inside a word: quote the whole text, as \"rate limit\"".into());
        }
        return Ok(Filter::Text(body.to_owned()));
    };
    let (name, operand) = (&body[..colon], &body[colon + 1..]);
    let mut input = name;
    let known: Result<&str, winnow::error::ContextError> =
        take_while(1.., ('a'..='z', '_')).parse_next(&mut input);
    if known.is_err() || !input.is_empty() || !FIELDS.contains(&name) {
        return Err(format!(
            "unknown field {name:?}: the fields are {}; a label is written @key:value; quote text that contains a colon",
            FIELDS.join(", ")
        ));
    }
    field(name, operand)
}

fn field(name: &str, operand: &str) -> Result<Filter, String> {
    if operand.is_empty() {
        return Err(format!("{name}: expected a value after the colon"));
    }
    Ok(match name {
        "namespace" => Filter::Namespace(
            exact(name, operand)?
                .into_iter()
                .map(|value| {
                    NamespaceName::new(value.clone()).map_err(|_| {
                        format!(
                            "namespace: {value:?} is not a namespace: 1 to 128 letters, digits, -, _, ., : and @"
                        )
                    })
                })
                .collect::<Result<_, _>>()?,
        ),
        "status" => Filter::Status(enumerated(name, operand, &STATUSES)?),
        "action" => Filter::Action(patterns(operand)?),
        "signal" => Filter::Signal(patterns(operand)?),
        "error" => Filter::Error(exact(name, operand)?),
        "waiting" if operand == "*" => Filter::Waiting(Vec::new()),
        "waiting" => Filter::Waiting(enumerated(name, operand, &WAITS)?),
        "started" => Filter::Started(comparison(name, operand, time_value, Alone::Day)?),
        "ended" => Filter::Ended(comparison(name, operand, time_value, Alone::Day)?),
        "activity" => Filter::Activity(comparison(name, operand, time_value, Alone::Day)?),
        "duration" => Filter::Duration(comparison(name, operand, duration_value, Alone::Refused)?),
        "calls" => Filter::Calls(comparison(name, operand, count_value, Alone::Exact)?),
        "errors" => Filter::Errors(comparison(name, operand, count_value, Alone::Exact)?),
        "script" | "action_set" => {
            let prefixes = hex_prefixes(name, operand, false)?;
            if name == "script" {
                Filter::Script(prefixes)
            } else {
                Filter::ActionSet(prefixes)
            }
        }
        "id" => Filter::Id(hex_prefixes(name, operand, true)?),
        "parent" => Filter::Parent(run_id(operand)?),
        "workflow" => Filter::Workflow(patterns(operand)?),
        "started_by" => Filter::StartedBy(
            run_id(operand).map_err(|error| error.replacen("parent:", "started_by:", 1))?,
        ),
        "fingerprint" => {
            let values = exact(name, operand)?;
            for value in &values {
                if value.len() != 16 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!(
                        "fingerprint: {value:?} is not a fingerprint: 16 hexadecimal digits"
                    ));
                }
            }
            Filter::Fingerprint(values.iter().map(|v| v.to_ascii_lowercase()).collect())
        }
        "quarantined" => Filter::Quarantined(match operand {
            "yes" | "true" => true,
            "no" | "false" => false,
            other => return Err(format!("quarantined: expected yes or no, not {other:?}")),
        }),
        "mode" => Filter::Mode(enumerated(name, operand, &MODES)?),
        _ => unreachable!("a field of FIELDS"),
    })
}

const STATUSES: [(&str, RunStatus); 6] = [
    ("pending", RunStatus::Pending),
    ("active", RunStatus::Active),
    ("completed", RunStatus::Completed),
    ("failed", RunStatus::Failed),
    ("cancelled", RunStatus::Cancelled),
    ("aborted", RunStatus::Aborted),
];

const WAITS: [(&str, WaitKind); 7] = [
    ("signal", WaitKind::Signal),
    ("timer", WaitKind::Timer),
    ("call", WaitKind::Call),
    ("harness", WaitKind::Harness),
    ("approval", WaitKind::Approval),
    ("parked", WaitKind::Parked),
    ("deferred", WaitKind::Deferred),
];

const MODES: [(&str, RunMode); 2] = [("live", RunMode::Live), ("mock", RunMode::Mock)];

/// The values of a list, split on commas outside quotes.
fn values(operand: &str) -> Result<Vec<(String, bool)>, String> {
    let mut values = Vec::new();
    let mut rest = operand;
    loop {
        let (value, was_quoted, after) = if rest.starts_with('"') {
            let (text, after) = quoted(rest)?;
            (text, true, after)
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let value = &rest[..end];
            if value.contains('"') {
                return Err(
                    "a quote inside a value: quote the whole value, as action:\"a b\"".into(),
                );
            }
            (value.to_owned(), false, &rest[end..])
        };
        if value.is_empty() && !was_quoted {
            return Err("an empty value: write a value between the commas".into());
        }
        values.push((value, was_quoted));
        match after.strip_prefix(',') {
            Some(next) => rest = next,
            None if after.is_empty() => return Ok(values),
            None => return Err(format!("unexpected {after:?}: separate values with commas")),
        }
    }
}

fn patterns(operand: &str) -> Result<Vec<Pattern>, String> {
    if operand.is_empty() {
        return Err("expected a value after the colon, or * for any value".into());
    }
    values(operand)?
        .into_iter()
        .map(|(value, was_quoted)| {
            Ok(match value.strip_suffix('*') {
                _ if was_quoted => Pattern::Exact(value),
                Some("") => Pattern::Any,
                Some(prefix) if prefix.contains('*') => {
                    return Err(format!("{value:?}: * only ends a value, as github.*"));
                }
                Some(prefix) => Pattern::Prefix(prefix.to_owned()),
                None if value.contains('*') => {
                    return Err(format!("{value:?}: * only ends a value, as github.*"));
                }
                None => Pattern::Exact(value),
            })
        })
        .collect()
}

fn exact(name: &str, operand: &str) -> Result<Vec<String>, String> {
    values(operand)?
        .into_iter()
        .map(|(value, was_quoted)| {
            if !was_quoted && value.contains('*') {
                Err(format!("{name}: * is not accepted here"))
            } else {
                Ok(value)
            }
        })
        .collect()
}

fn enumerated<T: Copy>(name: &str, operand: &str, allowed: &[(&str, T)]) -> Result<Vec<T>, String> {
    exact(name, operand)?
        .into_iter()
        .map(|value| {
            allowed
                .iter()
                .find(|(spelled, _)| *spelled == value)
                .map(|(_, parsed)| *parsed)
                .ok_or_else(|| {
                    let names: Vec<&str> = allowed.iter().map(|(spelled, _)| *spelled).collect();
                    format!("{name}: {value:?} is not one of {}", names.join(", "))
                })
        })
        .collect()
}

fn hex_prefixes(name: &str, operand: &str, hyphens: bool) -> Result<Vec<String>, String> {
    exact(name, operand)?
        .into_iter()
        .map(|value| {
            let prefix = value
                .strip_suffix('*')
                .unwrap_or(&value)
                .to_ascii_lowercase();
            let digits = prefix.bytes().filter(u8::is_ascii_hexdigit).count();
            let valid = prefix
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || (hyphens && b == b'-'));
            if !valid || digits < 8 {
                return Err(format!(
                    "{name}: {value:?} is not a prefix of at least 8 hexadecimal digits"
                ));
            }
            Ok(if hyphens { hyphenate(&prefix) } else { prefix })
        })
        .collect()
}

/// A prefix of a run id, hyphenated as a UUID: `0193a5c4b2` → `0193a5c4-b2`.
fn hyphenate(prefix: &str) -> String {
    let digits: String = prefix.chars().filter(|c| *c != '-').collect();
    let mut out = String::with_capacity(digits.len() + 4);
    for (i, c) in digits.chars().enumerate() {
        if matches!(i, 8 | 12 | 16 | 20) {
            out.push('-');
        }
        out.push(c);
    }
    out
}

fn run_id(operand: &str) -> Result<String, String> {
    let id = operand.to_ascii_lowercase();
    let digits: String = id.chars().filter(|c| *c != '-').collect();
    if digits.len() == 32 && digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hyphenate(&digits))
    } else {
        Err(format!("parent: {operand:?} is not a run id"))
    }
}

/// `"..."`, with `\"` and `\\` escaped. Returns the text and what follows the closing quote.
fn quoted(input: &str) -> Result<(String, &str), String> {
    let mut text = String::new();
    let mut chars = input.char_indices().skip(1);
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => return Ok((text, &input[at + 1..])),
            '\\' => match chars.next() {
                Some((_, escaped @ ('"' | '\\'))) => text.push(escaped),
                Some((_, other)) => {
                    text.push('\\');
                    text.push(other);
                }
                None => break,
            },
            other => text.push(other),
        }
    }
    Err("a quote is not closed: end the text with \"".into())
}

/// Reads a value of a comparison; for a date, also the next day, which ends the day it names.
type ValueParser<T> = fn(&str) -> Result<(T, Option<T>), String>;

/// What a value alone, without a comparison, means.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Alone {
    /// Exactly it: a count.
    Exact,
    /// A date: that day. A time alone is refused.
    Day,
    /// Refused: a duration is compared.
    Refused,
}

/// `>v`, `>=v`, `<v`, `<=v`, `a..b`, `a..`, `..b`, or `v` alone as `alone` says.
fn comparison<T: Copy>(
    name: &str,
    operand: &str,
    value: ValueParser<T>,
    alone: Alone,
) -> Result<Comparison<T>, String> {
    let mut input = operand;
    let op: Result<Option<&str>, winnow::error::ContextError> =
        opt(alt((">=", "<=", ">", "<"))).parse_next(&mut input);
    let op = op.unwrap_or(None);
    let bad = |error: String| format!("{name}: {error}");
    if let Some(op) = op {
        let (v, _) = value(input).map_err(bad)?;
        return Ok(match op {
            ">" => Comparison {
                start: Bound::Excluded(v),
                end: Bound::Unbounded,
            },
            ">=" => Comparison {
                start: Bound::Included(v),
                end: Bound::Unbounded,
            },
            "<" => Comparison {
                start: Bound::Unbounded,
                end: Bound::Excluded(v),
            },
            _ => Comparison {
                start: Bound::Unbounded,
                end: Bound::Included(v),
            },
        });
    }
    if let Some((from, to)) = input.split_once("..") {
        let start = if from.is_empty() {
            Bound::Unbounded
        } else {
            Bound::Included(value(from).map_err(bad)?.0)
        };
        let end = if to.is_empty() {
            Bound::Unbounded
        } else {
            Bound::Excluded(value(to).map_err(bad)?.0)
        };
        if from.is_empty() && to.is_empty() {
            return Err(format!("{name}: a range needs a start or an end, as a..b"));
        }
        return Ok(Comparison { start, end });
    }
    match (value(input).map_err(bad)?, alone) {
        ((v, Some(next)), Alone::Day) => Ok(Comparison {
            start: Bound::Included(v),
            end: Bound::Excluded(next),
        }),
        ((v, _), Alone::Exact) => Ok(Comparison {
            start: Bound::Included(v),
            end: Bound::Included(v),
        }),
        (_, Alone::Day) => Err(format!(
            "{name}: compare a time with >, >=, <, <= or a range a..b, as {name}:>now-24h, or give a date alone"
        )),
        (_, _) => Err(format!(
            "{name}: compare with >, >=, <, <= or a range a..b, as {name}:>5m"
        )),
    }
}

/// A time; for a date, also the next day, which ends the day a date alone names.
fn time_value(text: &str) -> Result<(TimeValue, Option<TimeValue>), String> {
    if text == "now" {
        return Ok((TimeValue::Ago(Duration::from_millis(0)), None));
    }
    if let Some(ago) = text.strip_prefix("now-") {
        let ago: Duration = ago
            .parse()
            .map_err(|_| format!("{ago:?} is not a duration, as 24h, 30m or 7d"))?;
        return Ok((TimeValue::Ago(ago), None));
    }
    if let Ok(date) = text.parse::<jiff::civil::Date>() {
        let start = date
            .to_zoned(jiff::tz::TimeZone::UTC)
            .map_err(|error| error.to_string())?;
        let next = start
            .checked_add(jiff::Span::new().days(1))
            .map_err(|error| error.to_string())?;
        return Ok((
            TimeValue::At(start.timestamp()),
            Some(TimeValue::At(next.timestamp())),
        ));
    }
    text.parse::<Timestamp>()
        .map(|at| (TimeValue::At(at), None))
        .map_err(|_| {
            format!(
                "{text:?} is not a time: write a date (2026-11-01), an RFC 3339 time (2026-11-01T10:00:00Z) or now-24h"
            )
        })
}

fn duration_value(text: &str) -> Result<(u64, Option<u64>), String> {
    text.parse::<Duration>()
        .map(|duration| (duration.as_millis(), None))
        .map_err(|_| format!("{text:?} is not a duration, as 500ms, 30s, 5m, 1h30m or 2d"))
}

fn count_value(text: &str) -> Result<(u64, Option<u64>), String> {
    text.parse::<u64>()
        .map(|count| (count, None))
        .map_err(|_| format!("{text:?} is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filters(text: &str) -> Vec<(bool, Filter)> {
        parse(text)
            .unwrap()
            .terms
            .into_iter()
            .map(|term| (term.negated, term.filter))
            .collect()
    }

    fn error(text: &str) -> QueryError {
        parse(text).unwrap_err()
    }

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn an_empty_query_has_no_terms() {
        assert!(parse("").unwrap().terms.is_empty());
        assert!(parse("   ").unwrap().terms.is_empty());
    }

    #[test]
    fn labels_match_a_value_a_prefix_or_presence() {
        assert_eq!(
            filters("@agent:triage @customer:acme* @conversation:* @env:prod,staging"),
            vec![
                (
                    false,
                    Filter::Label {
                        key: "agent".into(),
                        values: vec![Pattern::Exact("triage".into())]
                    }
                ),
                (
                    false,
                    Filter::Label {
                        key: "customer".into(),
                        values: vec![Pattern::Prefix("acme".into())]
                    }
                ),
                (
                    false,
                    Filter::Label {
                        key: "conversation".into(),
                        values: vec![Pattern::Any]
                    }
                ),
                (
                    false,
                    Filter::Label {
                        key: "env".into(),
                        values: vec![
                            Pattern::Exact("prod".into()),
                            Pattern::Exact("staging".into())
                        ]
                    }
                ),
            ]
        );
        assert_eq!(
            filters(r#"@team:"data platform""#),
            vec![(
                false,
                Filter::Label {
                    key: "team".into(),
                    values: vec![Pattern::Exact("data platform".into())]
                }
            )]
        );
    }

    #[test]
    fn a_namespace_is_named_exactly() {
        assert_eq!(
            filters("namespace:alice,user_2f:x@y.z -namespace:bob"),
            vec![
                (
                    false,
                    Filter::Namespace(vec![
                        NamespaceName::new("alice").unwrap(),
                        NamespaceName::new("user_2f:x@y.z").unwrap()
                    ])
                ),
                (
                    true,
                    Filter::Namespace(vec![NamespaceName::new("bob").unwrap()])
                ),
            ]
        );
        assert!(parse("namespace:al*").is_err());
        assert!(parse("namespace:a/b").is_err());
    }

    #[test]
    fn fields_take_lists_and_a_minus_excludes() {
        assert_eq!(
            filters("status:failed,aborted -status:completed action:github.* -@env:test"),
            vec![
                (
                    false,
                    Filter::Status(vec![RunStatus::Failed, RunStatus::Aborted])
                ),
                (true, Filter::Status(vec![RunStatus::Completed])),
                (
                    false,
                    Filter::Action(vec![Pattern::Prefix("github.".into())])
                ),
                (
                    true,
                    Filter::Label {
                        key: "env".into(),
                        values: vec![Pattern::Exact("test".into())]
                    }
                ),
            ]
        );
        assert_eq!(
            filters("waiting:* waiting:signal,approval signal:approval error:timeout,TypeError"),
            vec![
                (false, Filter::Waiting(vec![])),
                (
                    false,
                    Filter::Waiting(vec![WaitKind::Signal, WaitKind::Approval])
                ),
                (
                    false,
                    Filter::Signal(vec![Pattern::Exact("approval".into())])
                ),
                (
                    false,
                    Filter::Error(vec!["timeout".into(), "TypeError".into()])
                ),
            ]
        );
        assert_eq!(
            filters("quarantined:yes -quarantined:no mode:mock"),
            vec![
                (false, Filter::Quarantined(true)),
                (true, Filter::Quarantined(false)),
                (false, Filter::Mode(vec![RunMode::Mock])),
            ]
        );
    }

    #[test]
    fn comparisons_read_times_durations_and_counts() {
        assert_eq!(
            filters(
                "started:>2026-11-01T10:00:00Z ended:<=now-24h duration:>5m calls:>=20 errors:3"
            ),
            vec![
                (
                    false,
                    Filter::Started(Comparison {
                        start: Bound::Excluded(TimeValue::At(at("2026-11-01T10:00:00Z"))),
                        end: Bound::Unbounded
                    })
                ),
                (
                    false,
                    Filter::Ended(Comparison {
                        start: Bound::Unbounded,
                        end: Bound::Included(TimeValue::Ago(Duration::from_millis(86_400_000)))
                    })
                ),
                (
                    false,
                    Filter::Duration(Comparison {
                        start: Bound::Excluded(300_000),
                        end: Bound::Unbounded
                    })
                ),
                (
                    false,
                    Filter::Calls(Comparison {
                        start: Bound::Included(20),
                        end: Bound::Unbounded
                    })
                ),
                (
                    false,
                    Filter::Errors(Comparison {
                        start: Bound::Included(3),
                        end: Bound::Included(3)
                    })
                ),
            ]
        );
        assert_eq!(
            filters("started:2026-11-01 activity:2026-11-01..2026-11-08 calls:5.."),
            vec![
                (
                    false,
                    Filter::Started(Comparison {
                        start: Bound::Included(TimeValue::At(at("2026-11-01T00:00:00Z"))),
                        end: Bound::Excluded(TimeValue::At(at("2026-11-02T00:00:00Z")))
                    })
                ),
                (
                    false,
                    Filter::Activity(Comparison {
                        start: Bound::Included(TimeValue::At(at("2026-11-01T00:00:00Z"))),
                        end: Bound::Excluded(TimeValue::At(at("2026-11-08T00:00:00Z")))
                    })
                ),
                (
                    false,
                    Filter::Calls(Comparison {
                        start: Bound::Included(5),
                        end: Bound::Unbounded
                    })
                ),
            ]
        );
        let now = at("2026-11-12T12:00:00Z");
        assert_eq!(
            TimeValue::Ago(Duration::from_millis(3_600_000)).resolve(now),
            at("2026-11-12T11:00:00Z")
        );
    }

    #[test]
    fn digests_and_ids_are_prefixes_of_eight_digits_at_least() {
        assert_eq!(
            filters(
                "script:9F2C4E1A id:0193a5c4b2 parent:0193A5C4-B2E1-7C3D-8E4F-0A1B2C3D4E5F fingerprint:00112233445566aa"
            ),
            vec![
                (false, Filter::Script(vec!["9f2c4e1a".into()])),
                (false, Filter::Id(vec!["0193a5c4-b2".into()])),
                (
                    false,
                    Filter::Parent("0193a5c4-b2e1-7c3d-8e4f-0a1b2c3d4e5f".into())
                ),
                (false, Filter::Fingerprint(vec!["00112233445566aa".into()])),
            ]
        );
        assert!(
            error("script:9f2c")
                .message
                .contains("8 hexadecimal digits")
        );
        assert!(error("parent:abc").message.contains("not a run id"));
    }

    #[test]
    fn runs_of_workflows_and_the_runs_a_run_started() {
        assert_eq!(
            filters("workflow:triage,dig* -started_by:0193A5C4-B2E1-7C3D-8E4F-0A1B2C3D4E5F"),
            vec![
                (
                    false,
                    Filter::Workflow(vec![
                        Pattern::Exact("triage".into()),
                        Pattern::Prefix("dig".into()),
                    ])
                ),
                (
                    true,
                    Filter::StartedBy("0193a5c4-b2e1-7c3d-8e4f-0a1b2c3d4e5f".into())
                ),
            ]
        );
        let refused = error("started_by:abc").message;
        assert!(refused.starts_with("started_by:"), "{refused}");
        assert!(refused.contains("not a run id"), "{refused}");
    }

    #[test]
    fn text_is_a_word_or_quoted() {
        assert_eq!(
            filters(r#"refund "rate limit" -"timed out" "say \"hi\"""#),
            vec![
                (false, Filter::Text("refund".into())),
                (false, Filter::Text("rate limit".into())),
                (true, Filter::Text("timed out".into())),
                (false, Filter::Text(r#"say "hi""#.into())),
            ]
        );
        assert_eq!(
            filters(r#""a:b""#),
            vec![(false, Filter::Text("a:b".into()))]
        );
    }

    #[test]
    fn errors_name_the_term_and_where_it_is() {
        let e = error("@agent:triage stauts:failed");
        assert_eq!(e.span, QuerySpan { start: 14, end: 27 });
        assert!(
            e.message
                .starts_with("unknown field \"stauts\": the fields are status, action")
        );
        let e = error("status:faild");
        assert!(
            e.message
                .contains("\"faild\" is not one of pending, active")
        );
        let e = error("started:2026-13-45");
        assert!(
            e.message
                .starts_with("started: \"2026-13-45\" is not a time")
        );
        let e = error("started:now-2x");
        assert!(e.message.contains("is not a duration"));
        let e = error("duration:5m");
        assert!(e.message.contains("compare"), "{e}");
        let e = error("é \"open");
        assert_eq!(e.span, QuerySpan { start: 2, end: 7 });
        assert!(error("-").message.contains("lone"));
        assert!(error("@agent").message.contains("@key:value"));
        assert!(error("status:").message.contains("expected a value"));
        assert!(error("action:a,,b").message.contains("empty value"));
        assert!(error("action:a*b").message.contains("* only ends a value"));
        assert!(
            error("waiting:soon")
                .message
                .contains("signal, timer, call, harness, approval, parked, deferred")
        );
        assert!(error("ab\"c\"").message.contains("quote inside a word"));
        assert!(error("ab\"c").message.contains("not closed"));
    }

    #[test]
    fn spans_count_characters_not_bytes() {
        let query = parse("été action:x").unwrap();
        assert_eq!(query.terms[1].span, QuerySpan { start: 4, end: 12 });
    }
}
