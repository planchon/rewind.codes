//! `office`: a mailbox and calendars. Results are JSON in text.

use std::sync::Mutex;

use rewind_core::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{Args, CallContext, Fixed, Reply, Service, ToolSpec, lock, object_schema, page};
use crate::clock::{HOUR, MINUTE, T0, parse_date, parse_rfc3339, rfc3339};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Email {
    pub id: String,
    pub from: String,
    pub subject: String,
    pub unread: bool,
    pub received_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sent {
    pub id: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub event_id: String,
    pub title: String,
    pub start: String,
    pub end: String,
    pub attendees: Vec<String>,
}

/// The people whose calendars exist.
pub const PEOPLE: [&str; 5] = ["ana", "ben", "chloe", "dev", "eva"];

const DOMAINS: [&str; 5] = [
    "customers.test",
    "acme.test",
    "vendor.test",
    "newsletter.test",
    "bank.test",
];

/// 120 emails received over the week before the start, newest first.
pub fn inbox() -> Vec<Email> {
    let mut fixed = Fixed::new(43);
    (1..=120u64)
        .map(|n| {
            let domain = DOMAINS[usize::try_from(fixed.below(13) % 5).unwrap_or(0)];
            let sender = fixed.pick(&["info", "sam", "lee", "billing", "noreply", "kim"]);
            Email {
                id: format!("m_{n:03}"),
                from: format!("{sender}@{domain}"),
                subject: format!("Message {n}"),
                unread: fixed.below(3) != 0,
                received_at: rfc3339(Timestamp(T0.0 - n * 83 * MINUTE)),
            }
        })
        .collect()
}

/// The busy intervals of a person on a date, as (start, end) in minutes after midnight UTC.
pub fn busy(person: &str, date: &str) -> Vec<(u64, u64)> {
    let at = |h: u64, m: u64| h * 60 + m;
    if date == "2026-03-03" {
        return match person {
            "ana" => vec![
                (at(9, 0), at(10, 30)),
                (at(11, 0), at(12, 0)),
                (at(14, 0), at(15, 0)),
            ],
            "ben" => vec![(at(9, 30), at(11, 15)), (at(12, 0), at(13, 0))],
            "chloe" => vec![
                (at(10, 30), at(11, 30)),
                (at(13, 0), at(13, 45)),
                (at(15, 30), at(16, 0)),
            ],
            _ => vec![(at(10, 0), at(11, 0))],
        };
    }
    let seed = person
        .bytes()
        .chain(date.bytes())
        .fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)));
    let mut fixed = Fixed::new(seed);
    let start = at(9, 0) + 30 * fixed.below(10);
    vec![(start, start + 30 * (1 + fixed.below(3)))]
}

#[derive(Debug, Default)]
struct State {
    sent: Vec<Sent>,
    events: Vec<Event>,
}

#[derive(Debug)]
pub struct Office {
    inbox: Vec<Email>,
    state: Mutex<State>,
}

impl Default for Office {
    fn default() -> Self {
        Self::new()
    }
}

impl Office {
    pub fn new() -> Self {
        Self {
            inbox: inbox(),
            state: Mutex::new(State::default()),
        }
    }
}

/// `ana` and `ana@acme.test` are the same person.
fn person_of(name: &str) -> Option<&'static str> {
    let name = name.split('@').next().unwrap_or(name).trim().to_lowercase();
    PEOPLE.iter().find(|person| **person == name).copied()
}

impl Service for Office {
    fn name(&self) -> &'static str {
        "office"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                "list_inbox",
                "The emails of the inbox, newest first, 25 per page. Returns { emails: [{ id, from, subject, unread, received_at }], next_cursor? }.",
                object_schema(json!({ "cursor": { "type": "string" } }), &[]),
            ),
            ToolSpec::new(
                "send_email",
                "Send an email.",
                object_schema(
                    json!({ "to": { "type": "string" }, "subject": { "type": "string" }, "body": { "type": "string" } }),
                    &["to", "subject", "body"],
                ),
            ),
            ToolSpec::new(
                "get_availability",
                "The busy intervals of a person (ana, ben, chloe, dev, eva) on a date (YYYY-MM-DD), as RFC 3339 times in UTC. Returns { person, date, busy: [{ start, end }] }.",
                object_schema(
                    json!({ "person": { "type": "string" }, "date": { "type": "string", "format": "date" } }),
                    &["person", "date"],
                ),
            ),
            ToolSpec::new(
                "create_event",
                "Create a calendar event. Times are RFC 3339.",
                object_schema(
                    json!({
                        "title": { "type": "string" },
                        "start": { "type": "string", "format": "date-time" },
                        "end": { "type": "string", "format": "date-time" },
                        "attendees": { "type": "array", "items": { "type": "string" } }
                    }),
                    &["title", "start", "end", "attendees"],
                ),
            ),
        ]
    }

    fn call(&self, context: &CallContext, tool: &str, args: &Map<String, Value>) -> Reply {
        let args = Args(args);
        let mut state = lock(&self.state);
        match tool {
            "list_inbox" => match page(&self.inbox, args.opt_str("cursor"), 25) {
                Ok((emails, next)) => {
                    let mut result = json!({ "emails": emails });
                    if let Some(next) = next {
                        result["next_cursor"] = json!(next);
                    }
                    Reply::Json(result)
                }
                Err(reply) => reply,
            },
            "send_email" => {
                let (to, subject, body) =
                    match (args.str("to"), args.str("subject"), args.str("body")) {
                        (Ok(to), Ok(subject), Ok(body)) => {
                            (to.to_owned(), subject.to_owned(), body.to_owned())
                        }
                        (Err(reply), _, _) | (_, Err(reply), _) | (_, _, Err(reply)) => {
                            return reply;
                        }
                    };
                let id = format!("sent_{}", state.sent.len() + 1);
                state.sent.push(Sent {
                    id: id.clone(),
                    to,
                    subject,
                    body,
                    at: rfc3339(context.now),
                });
                Reply::Json(json!({ "id": id }))
            }
            "get_availability" => {
                let (name, date) = match (args.str("person"), args.str("date")) {
                    (Ok(name), Ok(date)) => (name, date),
                    (Err(reply), _) | (_, Err(reply)) => return reply,
                };
                let Some(person) = person_of(name) else {
                    return Reply::error(format!("not_found: no calendar for {name}"));
                };
                let Some(midnight) = parse_date(date) else {
                    return Reply::error(format!("invalid date {date:?}: expected YYYY-MM-DD"));
                };
                let busy: Vec<Value> = busy(person, date)
                    .into_iter()
                    .map(|(start, end)| {
                        json!({
                            "start": rfc3339(Timestamp(midnight.0 + start * MINUTE)),
                            "end": rfc3339(Timestamp(midnight.0 + end * MINUTE)),
                        })
                    })
                    .collect();
                Reply::Json(json!({ "person": person, "date": date, "busy": busy }))
            }
            "create_event" => {
                let (title, start, end) =
                    match (args.str("title"), args.str("start"), args.str("end")) {
                        (Ok(title), Ok(start), Ok(end)) => (title.to_owned(), start, end),
                        (Err(reply), _, _) | (_, Err(reply), _) | (_, _, Err(reply)) => {
                            return reply;
                        }
                    };
                let (Some(from), Some(to)) = (parse_rfc3339(start), parse_rfc3339(end)) else {
                    return Reply::error("invalid time: start and end are RFC 3339 times");
                };
                if to <= from || to.0 - from.0 > 12 * HOUR {
                    return Reply::error("invalid event: end must follow start, within 12 hours");
                }
                let event_id = format!("ev_{}", state.events.len() + 1);
                state.events.push(Event {
                    event_id: event_id.clone(),
                    title,
                    start: rfc3339(from),
                    end: rfc3339(to),
                    attendees: args.strings("attendees"),
                });
                Reply::Json(json!({ "event_id": event_id }))
            }
            other => Reply::error(format!("unknown tool {other}")),
        }
    }

    fn snapshot(&self) -> Value {
        let state = lock(&self.state);
        json!({ "sent": state.sent, "events": state.events })
    }
}

/// The people of an attendee list, by name.
pub fn attendee_names(attendees: &[String]) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = attendees.iter().filter_map(|a| person_of(a)).collect();
    names.sort_unstable();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_is_given_in_utc_times() {
        let office = Office::new();
        let context = CallContext {
            now: T0,
            idempotency_key: None,
        };
        let reply = office.call(
            &context,
            "get_availability",
            json!({ "person": "ana@acme.test", "date": "2026-03-03" })
                .as_object()
                .unwrap(),
        );
        let Reply::Json(view) = reply else {
            panic!("{reply:?}");
        };
        assert_eq!(view["busy"][0]["start"], "2026-03-03T09:00:00Z");
        assert_eq!(view["busy"][0]["end"], "2026-03-03T10:30:00Z");
        assert_eq!(
            attendee_names(&["Ben".into(), "ana@acme.test".into()]),
            ["ana", "ben"]
        );
    }
}
