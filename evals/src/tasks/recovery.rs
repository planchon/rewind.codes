//! Hard tasks: retries that must not duplicate effects, several servers joined, and runs of an
//! earlier turn to find, cancel or correct with `retry_run`.

use std::collections::{BTreeMap, BTreeSet};

use crate::fake::{chat, crm, tracker};
use crate::task::{Difficulty, SetupRun, Task, ensure};
use crate::world::WorldSpec;
use ptc_core::{RunId, RunStatus};

/// A run that posts and completes, and a run stuck waiting for the signal `go`.
const POSTED: SetupRun = SetupRun {
    script: r##"import { chat } from "ptc:tools";

export default async function main() {
  await chat.post_message({ channel: "#general", text: "hello" });
  return "posted";
}
"##,
};

const STUCK: SetupRun = SetupRun {
    script: r##"import { chat } from "ptc:tools";
import { signal } from "ptc";

export default async function main() {
  await signal("go");
  await chat.post_message({ channel: "#ops", text: "went" });
  return "went";
}
"##,
};

/// Opens one onboarding issue per customer, and fails at `C-1004`, which has no company.
pub const ONBOARDING: &str = r##"import { crm, tracker } from "ptc:tools";

export default async function main() {
  const ids = ["C-1001", "C-1002", "C-1003", "C-1004", "C-1005", "C-1006", "C-1007", "C-1008"];
  for (const id of ids) {
    const customer = (await crm.get_customer({ id })).json() as any;
    await tracker.create_issue({
      repo: "acme/support",
      title: `Onboard ${customer.company.name} (${id})`,
    });
  }
  return ids.length;
}
"##;

const ONBOARDING_RUN: SetupRun = SetupRun { script: ONBOARDING };

fn setup_run(setup: &[RunId], index: usize) -> String {
    setup
        .get(index)
        .map(ToString::to_string)
        .unwrap_or_default()
}

pub(super) fn tasks() -> Vec<Task> {
    vec![
        Task {
            id: "ship_orders_with_retries",
            difficulty: Difficulty::Hard,
            features: &["rate limit", "idempotency", "ToolError.data", "sleep"],
            world: || WorldSpec {
                servers: vec!["shop"],
                ..WorldSpec::default()
            },
            setup: &[],
            prompt: |_| {
                "Ship the orders O-601 to O-608 with shop.create_shipment. Each order must get \
                 exactly one shipment."
                    .into()
            },
            check: |outcome| {
                let shipments = outcome.state("shop")["shipments"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let mut per_order: BTreeMap<String, usize> = BTreeMap::new();
                for shipment in &shipments {
                    *per_order
                        .entry(shipment["order_id"].as_str().unwrap_or_default().to_owned())
                        .or_default() += 1;
                }
                for n in 601..=608 {
                    let order = format!("O-{n}");
                    let count = per_order.remove(&order).unwrap_or(0);
                    ensure(count == 1, || {
                        format!("{order} has {count} shipments, not 1")
                    })?;
                }
                ensure(per_order.is_empty(), || {
                    format!(
                        "other orders were shipped: {:?}",
                        per_order.keys().collect::<Vec<_>>()
                    )
                })
            },
        },
        Task {
            id: "escalate_customer_issues",
            difficulty: Difficulty::Hard,
            features: &["several servers", "pagination", "fan-out"],
            world: || WorldSpec {
                servers: vec!["tracker", "crm", "chat"],
                ..WorldSpec::default()
            },
            setup: &[],
            prompt: |_| {
                "Find the open issues of acme/api labeled customer; their reporter is a CRM \
                 customer id. Post one message to #escalations listing the issues reported by \
                 enterprise customers, one per line, written \"#<number> <title> (<customer \
                 name>)\"."
                    .into()
            },
            check: |outcome| {
                let customers: BTreeMap<String, crm::Customer> = crm::customers()
                    .into_iter()
                    .map(|customer| (customer.id.clone(), customer))
                    .collect();
                let issues = tracker::api_issues();
                let expected: Vec<(u64, String)> = issues
                    .iter()
                    .filter(|issue| {
                        issue.state == "open" && issue.labels.iter().any(|l| l == "customer")
                    })
                    .filter_map(|issue| {
                        let customer = customers.get(&issue.reporter)?;
                        (customer.tier == "enterprise")
                            .then(|| (issue.number, customer.name.clone()))
                    })
                    .collect();
                let posted = chat::messages_in(outcome.state("chat"), "escalations");
                ensure(posted.len() == 1, || {
                    format!("{} messages in #escalations, not 1", posted.len())
                })?;
                let text = &posted[0].text;
                let listed: BTreeSet<u64> = text
                    .split('#')
                    .skip(1)
                    .filter_map(|part| {
                        let digits: String =
                            part.chars().take_while(char::is_ascii_digit).collect();
                        digits.parse().ok()
                    })
                    .collect();
                let wanted: BTreeSet<u64> = expected.iter().map(|(number, _)| *number).collect();
                ensure(listed == wanted, || {
                    format!("the message lists the issues {listed:?}, not {wanted:?}")
                })?;
                for (number, name) in &expected {
                    let line = text
                        .lines()
                        .find(|line| line.contains(&format!("#{number} ")))
                        .unwrap_or_default();
                    ensure(line.contains(name.as_str()), || {
                        format!("the line of #{number} does not name {name}")
                    })?;
                }
                Ok(())
            },
        },
        Task {
            id: "cancel_stuck_run",
            difficulty: Difficulty::Medium,
            features: &["list_runs", "get_run", "cancel_run"],
            world: || WorldSpec {
                servers: vec!["chat"],
                ..WorldSpec::default()
            },
            setup: &[POSTED, STUCK],
            prompt: |_| {
                "One of the runs you started earlier is stuck. Find it, tell me what it is waiting \
                 for, and cancel it."
                    .into()
            },
            check: |outcome| {
                let stuck = outcome
                    .setup
                    .get(1)
                    .and_then(|id| outcome.run(*id))
                    .ok_or("no stuck run")?;
                ensure(stuck.status == RunStatus::Cancelled, || {
                    format!("the stuck run is {}, not cancelled", stuck.status)
                })?;
                let posted = outcome
                    .setup
                    .first()
                    .and_then(|id| outcome.run(*id))
                    .ok_or("no first run")?;
                ensure(posted.status == RunStatus::Completed, || {
                    format!("the run that had completed is {}", posted.status)
                })?;
                ensure(outcome.answer_has("go"), || {
                    "the answer does not say the run waited for the signal \"go\"".to_owned()
                })?;
                ensure(
                    chat::messages_in(outcome.state("chat"), "ops").is_empty(),
                    || "the stuck run went on and posted to #ops".to_owned(),
                )
            },
        },
        Task {
            id: "fix_failed_run",
            difficulty: Difficulty::Hard,
            features: &["retry_run", "fork", "no duplicate effects"],
            world: || WorldSpec {
                servers: vec!["crm", "tracker"],
                ..WorldSpec::default()
            },
            setup: &[ONBOARDING_RUN],
            prompt: |setup| {
                format!(
                    "Run {} failed halfway: it opens one onboarding issue in acme/support per \
                     customer. Fix it so that every customer gets exactly one issue, titled \
                     \"Onboard <company name> (<id>)\", or with the customer's own name when the \
                     customer has no company, without creating any duplicate. Its script was:\n\n\
                     ```ts\n{ONBOARDING}```",
                    setup_run(setup, 0)
                )
            },
            check: |outcome| {
                let support = outcome.state("tracker")["acme/support"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let titles: Vec<&str> = support
                    .iter()
                    .filter_map(|issue| issue["title"].as_str())
                    .collect();
                let customers = crm::customers();
                for customer in customers.iter().take(8) {
                    let tag = format!("({})", customer.id);
                    let matching: Vec<&&str> =
                        titles.iter().filter(|title| title.contains(&tag)).collect();
                    ensure(matching.len() == 1, || {
                        format!(
                            "{} has {} onboarding issues, not 1",
                            customer.id,
                            matching.len()
                        )
                    })?;
                    let name = customer
                        .company
                        .as_ref()
                        .map_or(customer.name.as_str(), |company| company.name.as_str());
                    ensure(matching[0].contains(name), || {
                        format!(
                            "the issue of {} is {:?}, without {name}",
                            customer.id, matching[0]
                        )
                    })?;
                }
                ensure(titles.len() == 8, || {
                    format!("acme/support has {} issues, not 8", titles.len())
                })
            },
        },
    ]
}
