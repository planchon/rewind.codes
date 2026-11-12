//! `tracker`: an issue tracker with typed results (every tool declares an output schema).
//!
//! Repositories: `acme/api` (60 issues, labels `bug`, `customer`, `enhancement`, reporters that
//! are CRM customers), `acme/web` (25 issues updated over the last 90 days), `acme/support`
//! (empty).

use std::collections::BTreeMap;
use std::sync::Mutex;

use ptc_core::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{Args, CallContext, Fixed, Reply, Service, ToolSpec, lock, object_schema, page};
use crate::clock::{DAY, HOUR, T0, rfc3339};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comment {
    pub body: String,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    /// `open` or `closed`.
    pub state: String,
    pub labels: Vec<String>,
    /// A CRM customer id (`C-1003`) or a user name.
    pub reporter: String,
    pub created_at: String,
    pub updated_at: String,
    pub comments: Vec<Comment>,
}

const SUBJECTS: [&str; 12] = [
    "Login fails with SSO",
    "Timeout on export",
    "Wrong currency in invoice",
    "Webhook retries twice",
    "Rate limit too strict",
    "Search ignores accents",
    "Slow dashboard",
    "Missing audit entries",
    "CSV import drops rows",
    "Password reset email late",
    "API returns 500 on empty body",
    "Pagination skips items",
];

/// `acme/api`: issue `n` (1 to 60) is closed when `n % 5 == 0`; labeled `bug` when
/// `n % 3 == 0`, `customer` when `n % 4 == 1` (reported by customer `C-10xx`), `enhancement`
/// otherwise.
pub fn api_issues() -> Vec<Issue> {
    let mut fixed = Fixed::new(11);
    (1..=60u64)
        .map(|n| {
            let mut labels = Vec::new();
            if n % 3 == 0 {
                labels.push("bug".to_owned());
            }
            if n % 4 == 1 {
                labels.push("customer".to_owned());
            }
            if labels.is_empty() {
                labels.push("enhancement".to_owned());
            }
            let reporter = if n % 4 == 1 {
                format!("C-{}", 1001 + (n * 7) % 40)
            } else {
                fixed.pick(&["dana", "eli", "farah", "gus"]).to_string()
            };
            let created = Timestamp(T0.0 - (120 - n) * DAY);
            let updated = Timestamp(created.0 + fixed.below(10) * DAY + fixed.below(20) * HOUR);
            Issue {
                number: n,
                title: format!("{} ({})", fixed.pick(&SUBJECTS), n),
                body: format!("Reported in ticket #{}.", 4000 + n),
                state: if n % 5 == 0 { "closed" } else { "open" }.to_owned(),
                labels,
                reporter,
                created_at: rfc3339(created),
                updated_at: rfc3339(updated.min(T0)),
                comments: Vec::new(),
            }
        })
        .collect()
}

/// `acme/web`: 25 open issues; issue `n` was last updated `4 * n - 1` days before the start,
/// so the issues 8 to 25 have not been updated for more than 30 days.
pub fn web_issues() -> Vec<Issue> {
    (1..=25u64)
        .map(|n| {
            let updated = Timestamp(T0.0 - (4 * n - 1) * DAY);
            Issue {
                number: n,
                title: format!("UI glitch #{n}"),
                body: String::new(),
                state: "open".to_owned(),
                labels: vec!["ui".to_owned()],
                reporter: "hana".to_owned(),
                created_at: rfc3339(Timestamp(updated.0 - 5 * DAY)),
                updated_at: rfc3339(updated),
                comments: Vec::new(),
            }
        })
        .collect()
}

#[derive(Debug)]
pub struct Tracker {
    repos: Mutex<BTreeMap<String, Vec<Issue>>>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Self {
        let repos = BTreeMap::from([
            ("acme/api".to_owned(), api_issues()),
            ("acme/web".to_owned(), web_issues()),
            ("acme/support".to_owned(), Vec::new()),
        ]);
        Self {
            repos: Mutex::new(repos),
        }
    }
}

fn summary_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "number": { "type": "integer" },
            "title": { "type": "string" },
            "state": { "type": "string", "enum": ["open", "closed"] },
            "labels": { "type": "array", "items": { "type": "string" } },
            "reporter": { "type": "string", "description": "A CRM customer id such as C-1003, or a user name." },
            "updated_at": { "type": "string", "format": "date-time" }
        },
        "required": ["number", "title", "state", "labels", "reporter", "updated_at"]
    })
}

fn summary(issue: &Issue) -> Value {
    json!({
        "number": issue.number,
        "title": issue.title,
        "state": issue.state,
        "labels": issue.labels,
        "reporter": issue.reporter,
        "updated_at": issue.updated_at,
    })
}

fn repo_arg() -> Value {
    json!({ "type": "string", "description": "owner/name, such as acme/api" })
}

impl Service for Tracker {
    fn name(&self) -> &'static str {
        "tracker"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec::new(
                "list_issues",
                "List the issues of a repository, most recently created first, one page at a time. Pass `next_cursor` back as `cursor` to read the next page; it is absent on the last page.",
                object_schema(
                    json!({
                        "repo": repo_arg(),
                        "state": { "type": "string", "enum": ["open", "closed", "all"], "description": "open by default" },
                        "label": { "type": "string", "description": "Only issues with this label." },
                        "cursor": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 25, "description": "20 by default, 25 at most" }
                    }),
                    &["repo"],
                ),
            )
            .output(json!({
                "type": "object",
                "properties": {
                    "issues": { "type": "array", "items": summary_schema() },
                    "next_cursor": { "type": "string" }
                },
                "required": ["issues"]
            })),
            ToolSpec::new(
                "get_issue",
                "Get one issue with its body and comments.",
                object_schema(json!({ "repo": repo_arg(), "number": { "type": "integer" } }), &["repo", "number"]),
            )
            .output(json!({
                "type": "object",
                "properties": {
                    "number": { "type": "integer" },
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "state": { "type": "string", "enum": ["open", "closed"] },
                    "labels": { "type": "array", "items": { "type": "string" } },
                    "reporter": { "type": "string" },
                    "created_at": { "type": "string", "format": "date-time" },
                    "updated_at": { "type": "string", "format": "date-time" },
                    "comments": { "type": "array", "items": { "type": "object", "properties": { "body": { "type": "string" }, "at": { "type": "string" } }, "required": ["body", "at"] } }
                },
                "required": ["number", "title", "body", "state", "labels", "reporter", "created_at", "updated_at", "comments"]
            })),
            ToolSpec::new(
                "create_issue",
                "Open an issue.",
                object_schema(
                    json!({
                        "repo": repo_arg(),
                        "title": { "type": "string" },
                        "body": { "type": "string" },
                        "labels": { "type": "array", "items": { "type": "string" } }
                    }),
                    &["repo", "title"],
                ),
            )
            .output(object_schema(json!({ "number": { "type": "integer" }, "url": { "type": "string" } }), &["number", "url"])),
            ToolSpec::new(
                "close_issue",
                "Close an issue.",
                object_schema(json!({ "repo": repo_arg(), "number": { "type": "integer" } }), &["repo", "number"]),
            )
            .output(object_schema(json!({ "number": { "type": "integer" }, "state": { "type": "string" } }), &["number", "state"])),
            ToolSpec::new(
                "add_comment",
                "Comment on an issue.",
                object_schema(
                    json!({ "repo": repo_arg(), "number": { "type": "integer" }, "body": { "type": "string" } }),
                    &["repo", "number", "body"],
                ),
            )
            .output(object_schema(json!({ "id": { "type": "string" } }), &["id"])),
        ]
    }

    fn call(&self, context: &CallContext, tool: &str, args: &Map<String, Value>) -> Reply {
        let args = Args(args);
        let mut repos = lock(&self.repos);
        let repo_name = match args.str("repo") {
            Ok(repo) => repo.to_owned(),
            Err(reply) => return reply,
        };
        let Some(issues) = repos.get_mut(&repo_name) else {
            return Reply::error(format!("no repository {repo_name}"));
        };
        let now = rfc3339(context.now);
        let number = args.opt_u64("number");
        let find = |issues: &mut Vec<Issue>| -> Result<usize, Reply> {
            let number = number.ok_or_else(|| Reply::error("`number` is required"))?;
            issues
                .iter()
                .position(|issue| issue.number == number)
                .ok_or_else(|| Reply::error(format!("no issue #{number} in {repo_name}")))
        };
        match tool {
            "list_issues" => {
                let state = args.opt_str("state").unwrap_or("open");
                let label = args.opt_str("label");
                let selected: Vec<Value> = issues
                    .iter()
                    .rev()
                    .filter(|issue| state == "all" || issue.state == state)
                    .filter(|issue| {
                        label.is_none_or(|label| issue.labels.iter().any(|l| l == label))
                    })
                    .map(summary)
                    .collect();
                let limit = args.opt_u64("limit").unwrap_or(20).clamp(1, 25) as usize;
                let (items, next) = match page(&selected, args.opt_str("cursor"), limit) {
                    Ok(page) => page,
                    Err(reply) => return reply,
                };
                let mut result = json!({ "issues": items });
                if let Some(next) = next {
                    result["next_cursor"] = json!(next);
                }
                Reply::Structured(result)
            }
            "get_issue" => match find(issues) {
                Ok(index) => Reply::Structured(json!(issues[index])),
                Err(reply) => reply,
            },
            "create_issue" => {
                let title = match args.str("title") {
                    Ok(title) => title.to_owned(),
                    Err(reply) => return reply,
                };
                let number = issues.iter().map(|issue| issue.number).max().unwrap_or(0) + 1;
                issues.push(Issue {
                    number,
                    title,
                    body: args.opt_str("body").unwrap_or_default().to_owned(),
                    state: "open".to_owned(),
                    labels: args.strings("labels"),
                    reporter: "ptc-bot".to_owned(),
                    created_at: now.clone(),
                    updated_at: now,
                    comments: Vec::new(),
                });
                Reply::Structured(json!({
                    "number": number,
                    "url": format!("https://tracker.example/{repo_name}/issues/{number}"),
                }))
            }
            "close_issue" => match find(issues) {
                Ok(index) => {
                    let issue = &mut issues[index];
                    issue.state = "closed".to_owned();
                    issue.updated_at = now;
                    Reply::Structured(json!({ "number": issue.number, "state": "closed" }))
                }
                Err(reply) => reply,
            },
            "add_comment" => match (find(issues), args.str("body")) {
                (Ok(index), Ok(body)) => {
                    let issue = &mut issues[index];
                    issue.comments.push(Comment {
                        body: body.to_owned(),
                        at: now.clone(),
                    });
                    issue.updated_at = now;
                    Reply::Structured(
                        json!({ "id": format!("{}-{}", issue.number, issue.comments.len()) }),
                    )
                }
                (Err(reply), _) | (_, Err(reply)) => reply,
            },
            other => Reply::error(format!("unknown tool {other}")),
        }
    }

    fn snapshot(&self) -> Value {
        json!(*lock(&self.repos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(tracker: &Tracker, tool: &str, args: Value) -> Reply {
        let context = CallContext {
            now: T0,
            idempotency_key: None,
        };
        tracker.call(&context, tool, args.as_object().unwrap())
    }

    #[test]
    fn listing_pages_through_open_issues() {
        let tracker = Tracker::new();
        let mut cursor: Option<String> = None;
        let mut seen = 0;
        loop {
            let mut args = json!({ "repo": "acme/api", "limit": 20 });
            if let Some(cursor) = &cursor {
                args["cursor"] = json!(cursor);
            }
            let Reply::Structured(result) = call(&tracker, "list_issues", args) else {
                panic!("a page");
            };
            seen += result["issues"].as_array().unwrap().len();
            match result.get("next_cursor") {
                Some(next) => cursor = Some(next.as_str().unwrap().to_owned()),
                None => break,
            }
        }
        let open = api_issues().iter().filter(|i| i.state == "open").count();
        assert_eq!(seen, open);
        assert_eq!(open, 48);
    }

    #[test]
    fn creating_and_closing_change_the_state() {
        let tracker = Tracker::new();
        let Reply::Structured(created) = call(
            &tracker,
            "create_issue",
            json!({ "repo": "acme/support", "title": "Hello" }),
        ) else {
            panic!("created");
        };
        assert_eq!(created["number"], 1);
        call(
            &tracker,
            "close_issue",
            json!({ "repo": "acme/support", "number": 1 }),
        );
        assert_eq!(tracker.snapshot()["acme/support"][0]["state"], "closed");
        assert!(matches!(
            call(
                &tracker,
                "get_issue",
                json!({ "repo": "acme/nope", "number": 1 })
            ),
            Reply::Error { .. }
        ));
    }
}
