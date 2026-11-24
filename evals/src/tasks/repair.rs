//! Repair tasks (`archi/05-testing-and-observability.md`, "Evals"): the model gets a workflow
//! with its suite and something that broke it, a failed run or a server that changed. It
//! succeeds when its fix passes the gate and does what the workflow is for.

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{ActionSet, Command, Event, RunId, RunStatus};
use serde_json::{Value, json};

use crate::fake::{object_schema, shipping, tracker};
use crate::task::{Difficulty, Outcome, Setup, SetupWorkflow, Task, ensure};
use crate::world::{ProductionRun, WorkflowReport, WorldSpec};

// -------------------------------------------------------------------------------------------
// A regression: a run of a workflow failed on real data

/// Buys a label per order, and assumes every label has a tracking number: an order collected
/// in store fails the run, after every label was bought.
pub const SHIP_ORDERS: &str = r##"import { shipping } from "rewind:actions";

export default async function main(input: { orders: string[] }) {
  const labels = await Promise.all(
    input.orders.map((order_id) => shipping.create_label({ order_id })),
  );
  const shipped = labels.map((label) => {
    if (!label.tracking) {
      throw new Error(`the carrier gave no tracking number for order ${label.order_id}`);
    }
    return { order: label.order_id, tracking: label.tracking.number };
  });
  return { shipped };
}
"##;

/// The orders of the production run that completed, then of the one that failed.
const SHIPPED_ORDERS: [&str; 2] = ["A-1001", "A-1002"];
const FAILED_ORDERS: [&str; 3] = ["A-1003", "P-1004", "A-1005"];

/// The names of the tests `ship_orders` was created with.
const SHIP_ORDERS_TESTS: [&str; 3] = [
    "ships every order with its tracking number",
    "buys no label without orders",
    "fails when the carrier refuses a label",
];

fn label(order: &str, tracking: &str) -> Value {
    json!({
        "label_id": format!("L-{order}"),
        "order_id": order,
        "service": "ground",
        "postage_cents": shipping::GROUND_CENTS,
        "tracking": { "carrier": "UPS", "number": tracking },
        "created_at": "2026-01-01T00:00:00Z"
    })
}

fn ship_orders() -> SetupWorkflow {
    SetupWorkflow {
        name: "ship_orders",
        description: "Buy a shipping label for each order, and give each order's tracking number.",
        code: SHIP_ORDERS,
        input: object_schema(
            json!({ "orders": { "type": "array", "items": { "type": "string" } } }),
            &["orders"],
        ),
        output: object_schema(
            json!({
                "shipped": {
                    "type": "array",
                    "items": object_schema(
                        json!({
                            "order": { "type": "string" },
                            "tracking": {
                                "type": ["string", "null"],
                                "description": "null for an order collected in store."
                            }
                        }),
                        &["order", "tracking"],
                    )
                }
            }),
            &["shipped"],
        ),
        signals: BTreeMap::new(),
        tests: vec![
            json!({
                "name": SHIP_ORDERS_TESTS[0],
                "input": { "orders": ["A-1", "A-2"] },
                "world": { "actions": { "shipping.create_label": [
                    { "args": { "order_id": "A-1" }, "returns": label("A-1", "1Z0001") },
                    { "args": { "order_id": "A-2" }, "returns": label("A-2", "1Z0002") }
                ] } },
                "expect": {
                    "status": "completed",
                    "output": { "shipped": [
                        { "order": "A-1", "tracking": "1Z0001" },
                        { "order": "A-2", "tracking": "1Z0002" }
                    ] },
                    "calls": [{ "action": "shipping.create_label", "times": 2 }]
                }
            }),
            json!({
                "name": SHIP_ORDERS_TESTS[1],
                "input": { "orders": [] },
                "expect": {
                    "status": "completed",
                    "output": { "shipped": [] },
                    "calls": [{ "action": "shipping.create_label", "times": 0 }]
                }
            }),
            json!({
                "name": SHIP_ORDERS_TESTS[2],
                "input": { "orders": ["A-1"] },
                "world": { "actions": { "shipping.create_label": {
                    "error": { "message": "address_invalid: the address has no street" }
                } } },
                "expect": {
                    "status": "failed",
                    "error": { "name": "ActionError", "message": "address_invalid" }
                }
            }),
        ],
        action_set: None,
    }
}

fn orders(orders: &[&str]) -> Value {
    json!({ "orders": orders })
}

// -------------------------------------------------------------------------------------------
// An upgrade: the tracker's server changed its actions

/// Posts the open bugs of a repository, written for the tracker as it was before its upgrade:
/// `list_issues` took `labels` and returned at most `limit` issues, saying when there were more.
pub const BUG_DIGEST: &str = r##"import { chat, tracker } from "rewind:actions";

export default async function main(input: { repo: string }) {
  const { issues, truncated } = await tracker.list_issues({
    repo: input.repo,
    labels: "bug",
    limit: 100,
  });
  if (issues.length === 0) {
    return { posted: 0, truncated: false };
  }
  const lines = issues.map((issue) => `#${issue.number} ${issue.title}`);
  if (truncated) {
    lines.push("…and more: the list was truncated.");
  }
  await chat.post_message({
    channel: "#bugs",
    text: `Open bugs in ${input.repo}:\n${lines.join("\n")}`,
  });
  return { posted: issues.length, truncated: truncated ?? false };
}
"##;

/// The names of the tests `bug_digest` was created with: the first two pin what it is for, the
/// third a behavior of the tracker before its upgrade.
const BUG_DIGEST_TESTS: [&str; 3] = [
    "posts the open bugs",
    "posts nothing without open bugs",
    "says when the list is truncated",
];

/// The tracker's `list_issues` before its upgrade: `labels` rather than `label`, no cursor but
/// a `truncated` flag, and issues without their reporter.
fn tracker_before_upgrade(action_set: &ActionSet) -> Result<ActionSet, String> {
    let actions: Vec<rewind_core::Action> = action_set
        .actions()
        .iter()
        .map(|action| {
            if action.name != "tracker.list_issues" {
                return action.clone();
            }
            let mut old = action.clone();
            old.description = Some(
                "List the issues of a repository, most recently created first, up to `limit`; `truncated` is true when more issues match."
                    .into(),
            );
            old.input_schema = object_schema(
                json!({
                    "repo": { "type": "string", "description": "owner/name, such as acme/api" },
                    "state": { "type": "string", "enum": ["open", "closed", "all"], "description": "open by default" },
                    "labels": { "type": "string", "description": "Only issues with this label." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "50 by default, 100 at most" }
                }),
                &["repo"],
            );
            old.output_schema = Some(json!({
                "type": "object",
                "properties": {
                    "issues": { "type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "number": { "type": "integer" },
                            "title": { "type": "string" },
                            "state": { "type": "string", "enum": ["open", "closed"] },
                            "labels": { "type": "array", "items": { "type": "string" } },
                            "updated_at": { "type": "string", "format": "date-time" }
                        },
                        "required": ["number", "title", "state", "labels", "updated_at"]
                    } },
                    "truncated": { "type": "boolean" }
                },
                "required": ["issues"]
            }));
            old
        })
        .collect();
    ActionSet::new(action_set.servers().to_vec(), actions).map_err(|error| error.to_string())
}

fn old_issue(number: u64, title: &str) -> Value {
    json!({
        "number": number,
        "title": title,
        "state": "open",
        "labels": ["bug"],
        "updated_at": "2026-02-01T00:00:00Z"
    })
}

fn posted() -> Value {
    json!({ "returns": { "content": [{ "type": "text", "text": "{\"ok\":true}" }] } })
}

fn bug_digest() -> SetupWorkflow {
    SetupWorkflow {
        name: "bug_digest",
        description: "Post the open bugs of a repository to #bugs.",
        code: BUG_DIGEST,
        input: object_schema(json!({ "repo": { "type": "string" } }), &["repo"]),
        output: object_schema(
            json!({ "posted": { "type": "integer" }, "truncated": { "type": "boolean" } }),
            &["posted"],
        ),
        signals: BTreeMap::new(),
        tests: vec![
            json!({
                "name": BUG_DIGEST_TESTS[0],
                "input": { "repo": "acme/api" },
                "world": { "actions": {
                    "tracker.list_issues": {
                        "args": { "repo": "acme/api", "labels": "bug" },
                        "returns": { "issues": [
                            old_issue(3, "Login fails with SSO (3)"),
                            old_issue(6, "Slow dashboard (6)")
                        ] }
                    },
                    "chat.post_message": posted()
                } },
                "expect": {
                    "status": "completed",
                    "output": { "posted": 2 },
                    "calls": [{ "action": "chat.post_message", "args": { "channel": "#bugs" }, "times": 1 }]
                }
            }),
            json!({
                "name": BUG_DIGEST_TESTS[1],
                "input": { "repo": "acme/web" },
                "world": { "actions": {
                    "tracker.list_issues": { "args": { "labels": "bug" }, "returns": { "issues": [] } }
                } },
                "expect": {
                    "status": "completed",
                    "output": { "posted": 0 },
                    "calls": [{ "action": "chat.post_message", "times": 0 }]
                }
            }),
            json!({
                "name": BUG_DIGEST_TESTS[2],
                "input": { "repo": "acme/api" },
                "world": { "actions": {
                    "tracker.list_issues": {
                        "args": { "labels": "bug" },
                        "returns": { "issues": [old_issue(3, "Login fails with SSO (3)")], "truncated": true }
                    },
                    "chat.post_message": posted()
                } },
                "expect": { "status": "completed", "output": { "posted": 1, "truncated": true } }
            }),
        ],
        action_set: Some(tracker_before_upgrade),
    }
}

// -------------------------------------------------------------------------------------------
// Checks

/// The latest version of a workflow, which must derive from its first, and the first.
fn new_version<'o>(
    outcome: &'o Outcome,
    workflow: &str,
) -> Result<(&'o WorkflowReport, &'o WorkflowReport), String> {
    let versions = outcome.versions(workflow);
    let (Some(first), Some(latest)) = (versions.first(), versions.last()) else {
        return Err(format!("the workflow {workflow} does not exist"));
    };
    ensure(latest.version != first.version, || {
        format!("no new version of {workflow} was made")
    })?;
    // The lineage, from the latest version back.
    let mut seen = BTreeSet::new();
    let mut current = *latest;
    while let Some(parent) = current.base {
        if parent == first.version {
            return Ok((first, latest));
        }
        if !seen.insert(parent) {
            break;
        }
        match versions.iter().find(|version| version.version == parent) {
            Some(version) => current = version,
            None => break,
        }
    }
    Err(format!(
        "the latest version of {workflow} does not derive from the version it replaces: its \
         tests were not inherited"
    ))
}

/// Every test a version replaced or retired says why.
fn reasons_given(outcome: &Outcome, workflow: &str) -> Result<(), String> {
    for version in outcome.versions(workflow) {
        for replaced in &version.replaced {
            ensure(!replaced.reason.trim().is_empty(), || {
                format!("the replacement of {:?} gives no reason", replaced.name)
            })?;
        }
        for retired in &version.retired {
            ensure(!retired.reason.trim().is_empty(), || {
                format!("the retirement of {:?} gives no reason", retired.name)
            })?;
        }
    }
    Ok(())
}

fn check_regression(outcome: &Outcome) -> Result<(), String> {
    let failed = *outcome.setup.get(1).ok_or("no failed run in the setup")?;
    let (_, latest) = new_version(outcome, "ship_orders")?;
    for name in SHIP_ORDERS_TESTS {
        ensure(latest.test(name).is_some(), || {
            format!("the new version of ship_orders lost its test {name:?}")
        })?;
    }
    let regression = latest.tests.iter().any(|loaded| {
        loaded
            .test
            .as_ref()
            .and_then(|test| test.from.as_ref())
            .is_some_and(|origin| origin.run == failed)
    });
    ensure(regression, || {
        format!("no test of the new version's suite was recorded from the failed run {failed}")
    })?;
    reasons_given(outcome, "ship_orders")?;
    let fork = outcome
        .runs
        .iter()
        .find(|run| run.source == Some(failed) && run.status == RunStatus::Completed);
    ensure(fork.is_some(), || {
        format!("the failed run {failed} was not finished: no fork of it completed")
    })?;
    let labels = shipping::labels_in(outcome.state("shipping"));
    let mut per_order: BTreeMap<&str, usize> = BTreeMap::new();
    for label in &labels {
        *per_order.entry(label.order_id.as_str()).or_default() += 1;
    }
    for order in SHIPPED_ORDERS.iter().chain(&FAILED_ORDERS) {
        let count = per_order.remove(order).unwrap_or(0);
        ensure(count == 1, || {
            format!("{order} has {count} labels, not 1: a label was bought twice")
        })?;
    }
    ensure(per_order.is_empty(), || {
        format!(
            "labels were bought for other orders: {:?}",
            per_order.keys().collect::<Vec<_>>()
        )
    })
}

fn open_bugs() -> BTreeSet<u64> {
    tracker::api_issues()
        .iter()
        .filter(|issue| issue.state == "open" && issue.labels.iter().any(|label| label == "bug"))
        .map(|issue| issue.number)
        .collect()
}

/// The `#123` numbers of a text.
fn numbers(text: &str) -> BTreeSet<u64> {
    text.split('#')
        .skip(1)
        .filter_map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .collect()
}

fn check_upgrade(outcome: &Outcome) -> Result<(), String> {
    let (_, latest) = new_version(outcome, "bug_digest")?;
    for name in &BUG_DIGEST_TESTS[..2] {
        ensure(latest.test(name).is_some(), || {
            format!("the new version of bug_digest lost its test {name:?}")
        })?;
    }
    reasons_given(outcome, "bug_digest")?;
    let run: RunId = *outcome
        .production
        .first()
        .ok_or("production did not run bug_digest")?;
    let run = outcome.run(run).ok_or("the production run is missing")?;
    ensure(run.status == RunStatus::Completed, || {
        format!(
            "this morning's run of bug_digest is {}{}",
            run.status,
            run.error
                .as_ref()
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        )
    })?;
    ensure(
        run.workflow
            .as_ref()
            .is_some_and(|of| of.digest == latest.version),
        || "this morning's run is not a run of the new version".to_owned(),
    )?;
    let posts: Vec<&Value> = run
        .history
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Command(Command::ScheduleCall { action, args, .. })
                if action == "chat.post_message" =>
            {
                args.value()
            }
            _ => None,
        })
        .collect();
    ensure(posts.len() == 1, || {
        format!("this morning's run posted {} messages, not 1", posts.len())
    })?;
    let channel = posts[0]["channel"].as_str().unwrap_or_default();
    ensure(channel.trim_start_matches('#') == "bugs", || {
        format!("the digest was posted to {channel:?}, not #bugs")
    })?;
    let listed = numbers(posts[0]["text"].as_str().unwrap_or_default());
    let wanted = open_bugs();
    ensure(listed == wanted, || {
        format!("the digest lists the issues {listed:?}, not the open bugs {wanted:?}")
    })
}

pub(super) fn tasks() -> Vec<Task> {
    vec![
        Task {
            id: "fix_workflow_regression",
            difficulty: Difficulty::Hard,
            features: &[
                "get_workflow",
                "record_tests",
                "regression test",
                "gate",
                "retry_run",
            ],
            world: || WorldSpec {
                servers: vec!["shipping"],
                ..WorldSpec::default()
            },
            setup: &[
                Setup::Workflow(ship_orders),
                Setup::WorkflowRun {
                    workflow: "ship_orders",
                    input: || orders(&SHIPPED_ORDERS),
                },
                Setup::WorkflowRun {
                    workflow: "ship_orders",
                    input: || orders(&FAILED_ORDERS),
                },
            ],
            prompt: |_| {
                "The workflow ship_orders buys a shipping label for each order it is given. Its \
                 last run failed, and the orders of that run still have no tracking number in \
                 our system. Fix the workflow so that what broke that run cannot come back \
                 unnoticed: that run's case must be one of its tests. Then finish that run, \
                 without buying any label twice."
                    .into()
            },
            check: check_regression,
        },
        Task {
            id: "upgrade_workflow_suite",
            difficulty: Difficulty::Hard,
            features: &[
                "get_workflow",
                "stale tests",
                "replace with a reason",
                "retire with a reason",
                "pagination",
            ],
            world: || WorldSpec {
                servers: vec!["tracker", "chat"],
                production: vec![ProductionRun {
                    workflow: "bug_digest",
                    input: json!({ "repo": "acme/api" }),
                }],
                ..WorldSpec::default()
            },
            setup: &[Setup::Workflow(bug_digest)],
            prompt: |_| {
                "The tracker's MCP server was upgraded overnight, and some of its actions changed. \
                 The workflow bug_digest posts the open bugs of a repository to #bugs; its next \
                 run, for acme/api tomorrow morning, will call the new server. Make sure that \
                 run works, and keep the behaviors its tests check."
                    .into()
            },
            check: check_upgrade,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_lists_the_open_bugs() {
        let bugs = open_bugs();
        assert_eq!(bugs.len(), 16, "{bugs:?}");
        assert_eq!(
            numbers("Open bugs:\n#3 a\n#6 b (6)"),
            BTreeSet::from([3, 6])
        );
    }
}
