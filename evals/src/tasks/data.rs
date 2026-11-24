//! Tasks over data: fan-out, pagination, failures to handle, aggregation, bulk updates, large
//! outputs.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::fake::{crm, office, ops, shop, tracker};
use crate::task::{Difficulty, Task, concurrent_calls, ensure};
use crate::world::WorldSpec;

fn world(servers: &[&'static str]) -> WorldSpec {
    WorldSpec {
        servers: servers.to_vec(),
        ..WorldSpec::default()
    }
}

/// The issues of a repository in the tracker's state.
fn issues(state: &Value, repo: &str) -> Vec<Value> {
    state[repo].as_array().cloned().unwrap_or_default()
}

pub(super) fn tasks() -> Vec<Task> {
    vec![
        Task {
            id: "lifetime_value_fan_out",
            difficulty: Difficulty::Medium,
            features: &["Promise.all", "ActionContent.json()"],
            world: || world(&["crm"]),
            setup: &[],
            prompt: |_| {
                "Fetch the customers C-1001 to C-1012 concurrently, with Promise.all, and return \
                 the sum of their lifetime values in cents as the script's output."
                    .into()
            },
            check: |outcome| {
                let expected: u64 = crm::customers()
                    .iter()
                    .take(12)
                    .map(|customer| customer.lifetime_value_cents)
                    .sum();
                let run = outcome
                    .agent_runs()
                    .find(|run| {
                        run.output.as_ref().is_some_and(|output| {
                            crate::task::holds_number(output, expected as f64)
                        })
                    })
                    .ok_or_else(|| format!("no run returned {expected}"))?;
                let concurrent = concurrent_calls(run, "crm.get_customer");
                ensure(concurrent >= 12, || {
                    format!("the run fetched {concurrent} customers concurrently, not 12")
                })
            },
        },
        Task {
            id: "count_open_issues",
            difficulty: Difficulty::Medium,
            features: &["pagination"],
            world: || world(&["tracker"]),
            setup: &[],
            prompt: |_| {
                "How many open issues does acme/api have, and how many of them are labeled customer?"
                    .into()
            },
            check: |outcome| {
                let issues = tracker::api_issues();
                let open: Vec<_> = issues
                    .iter()
                    .filter(|issue| issue.state == "open")
                    .collect();
                let customer = open
                    .iter()
                    .filter(|issue| issue.labels.iter().any(|label| label == "customer"))
                    .count();
                ensure(outcome.answer_has_number(open.len() as u64), || {
                    format!("the answer does not say {} open issues", open.len())
                })?;
                ensure(outcome.answer_has_number(customer as u64), || {
                    format!("the answer does not say {customer} customer issues")
                })
            },
        },
        Task {
            id: "missing_orders",
            difficulty: Difficulty::Medium,
            features: &["ActionError", "try/catch", "fan-out"],
            world: || world(&["crm"]),
            setup: &[],
            prompt: |_| {
                "Get the orders O-501 to O-512; some of them may not exist. Return \
                 { total_cents, missing } as the script's output: the sum of the totals of the \
                 orders that exist, and the ids of those that do not."
                    .into()
            },
            check: |outcome| {
                let ids: Vec<String> = (501..=512).map(|n| format!("O-{n}")).collect();
                let orders = crm::orders();
                let total: u64 = orders
                    .iter()
                    .filter(|order| ids.contains(&order.id))
                    .map(|order| order.total_cents)
                    .sum();
                let missing: BTreeSet<&str> = crm::MISSING_ORDERS.into_iter().collect();
                let found = outcome.outputs().into_iter().any(|output| {
                    let listed: BTreeSet<&str> = output["missing"]
                        .as_array()
                        .map(|ids| ids.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    output["total_cents"].as_u64() == Some(total) && listed == missing
                });
                ensure(found, || {
                    format!("no run returned {{ total_cents: {total}, missing: {missing:?} }}")
                })
            },
        },
        Task {
            id: "close_stale_issues",
            difficulty: Difficulty::Medium,
            features: &["Date.now()", "bulk update", "typed result"],
            world: || world(&["tracker"]),
            setup: &[],
            prompt: |_| {
                "In acme/web, close every open issue that has not been updated in the last 30 \
                 days. Comment \"Closing as stale.\" on each one before closing it."
                    .into()
            },
            check: |outcome| {
                let web = issues(outcome.state("tracker"), "acme/web");
                for issue in &web {
                    let number = issue["number"].as_u64().unwrap_or(0);
                    let stale = (8..=25).contains(&number);
                    let closed = issue["state"] == "closed";
                    let commented = issue["comments"].as_array().is_some_and(|comments| {
                        comments.iter().any(|comment| {
                            comment["body"]
                                .as_str()
                                .is_some_and(|body| body.contains("Closing as stale"))
                        })
                    });
                    ensure(closed == stale, || {
                        if stale {
                            format!("the stale issue #{number} is still open")
                        } else {
                            format!("the recent issue #{number} was closed")
                        }
                    })?;
                    ensure(commented == stale, || {
                        format!("issue #{number}: commented {commented}, stale {stale}")
                    })?;
                }
                Ok(())
            },
        },
        Task {
            id: "export_audit_log",
            difficulty: Difficulty::Medium,
            features: &["pagination", "output above 256 KB", "get_run offset"],
            world: || world(&["ops"]),
            setup: &[],
            prompt: |_| {
                "Return every audit event of February 2026 as the script's output: one array, in \
                 their original order (there are a few thousand). Then tell me the id of the \
                 last one."
                    .into()
            },
            check: |outcome| {
                let expected: Vec<String> = ops::audit_events().into_iter().map(|e| e.id).collect();
                let exported = outcome.outputs().into_iter().any(|output| {
                    output.as_array().is_some_and(|events| {
                        events.len() == expected.len()
                            && events
                                .iter()
                                .zip(&expected)
                                .all(|(event, id)| event["id"] == id.as_str())
                    })
                });
                ensure(exported, || {
                    format!("no run returned the {} events in order", expected.len())
                })?;
                let last = expected.last().cloned().unwrap_or_default();
                ensure(outcome.answer_has(&last), || {
                    format!("the answer does not give the last id, {last}")
                })
            },
        },
        Task {
            id: "revenue_by_region",
            difficulty: Difficulty::Medium,
            features: &["pagination", "aggregation", "typed result"],
            world: || world(&["shop"]),
            setup: &[],
            prompt: |_| {
                "Compute the total sales of February 2026 per region, in cents, and return an \
                 object such as { amer: 123, emea: 456, … } as the script's output."
                    .into()
            },
            check: |outcome| {
                let mut expected: BTreeMap<String, u64> = BTreeMap::new();
                for sale in shop::sales() {
                    *expected.entry(sale.region).or_default() += sale.amount_cents;
                }
                let found = outcome.outputs().into_iter().any(|output| {
                    expected
                        .iter()
                        .all(|(region, total)| output[region].as_u64() == Some(*total))
                });
                ensure(found, || format!("no run returned {expected:?}"))
            },
        },
        Task {
            id: "reorder_low_stock",
            difficulty: Difficulty::Medium,
            features: &["pagination", "bulk update", "typed result"],
            world: || world(&["shop"]),
            setup: &[],
            prompt: |_| {
                "For every product whose stock is below its reorder point, create a purchase \
                 order for its reorder quantity from its supplier."
                    .into()
            },
            check: |outcome| {
                let expected: BTreeSet<(String, u64, String)> = shop::products()
                    .into_iter()
                    .filter(|product| product.stock < product.reorder_point)
                    .map(|p| (p.sku, p.reorder_quantity, p.supplier))
                    .collect();
                let orders = outcome.state("shop")["purchase_orders"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let made: Vec<(String, u64, String)> = orders
                    .iter()
                    .map(|order| {
                        (
                            order["sku"].as_str().unwrap_or_default().to_owned(),
                            order["quantity"].as_u64().unwrap_or(0),
                            order["supplier"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect();
                ensure(made.len() == expected.len(), || {
                    format!("{} purchase orders, not {}", made.len(), expected.len())
                })?;
                let made: BTreeSet<_> = made.into_iter().collect();
                ensure(made == expected, || {
                    let wrong: Vec<_> = made.difference(&expected).collect();
                    format!("unexpected purchase orders: {wrong:?}")
                })
            },
        },
        Task {
            id: "unread_digest",
            difficulty: Difficulty::Medium,
            features: &["pagination", "aggregation"],
            world: || world(&["office"]),
            setup: &[],
            prompt: |_| {
                "Email cfo@acme.test a digest with the subject \"Inbox digest\": for each sender \
                 domain, the number of unread emails in the inbox, one line per domain written \
                 \"<domain>: <count>\", the largest count first."
                    .into()
            },
            check: |outcome| {
                let mut counts: BTreeMap<String, u64> = BTreeMap::new();
                for email in office::inbox().into_iter().filter(|email| email.unread) {
                    let domain = email.from.split('@').nth(1).unwrap_or_default().to_owned();
                    *counts.entry(domain).or_default() += 1;
                }
                let sent = outcome.state("office")["sent"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let digest = sent
                    .iter()
                    .find(|email| {
                        email["to"] == "cfo@acme.test" && email["subject"] == "Inbox digest"
                    })
                    .ok_or("no email \"Inbox digest\" to cfo@acme.test")?;
                let body = digest["body"].as_str().unwrap_or_default();
                let mut positions = Vec::new();
                for (domain, count) in &counts {
                    let line = format!("{domain}: {count}");
                    let at = body
                        .find(&line)
                        .ok_or_else(|| format!("the digest has no line {line:?}"))?;
                    positions.push((at, *count));
                }
                positions.sort_unstable();
                ensure(
                    positions.windows(2).all(|pair| pair[0].1 >= pair[1].1),
                    || "the lines are not in decreasing order of count".to_owned(),
                )
            },
        },
        Task {
            id: "schedule_meeting",
            difficulty: Difficulty::Medium,
            features: &["fan-out", "reasoning over data"],
            world: || world(&["office"]),
            setup: &[],
            prompt: |_| {
                "Find the first 30-minute slot on 2026-03-03 between 09:00 and 17:00 UTC when ana, \
                 ben and chloe are all free, and create a \"Design review\" event in that slot \
                 with the three of them."
                    .into()
            },
            check: |outcome| {
                let events = outcome.state("office")["events"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                ensure(events.len() == 1, || {
                    format!("{} events were created, not 1", events.len())
                })?;
                let event = &events[0];
                ensure(event["title"] == "Design review", || {
                    format!("the event is {}", event["title"])
                })?;
                ensure(
                    event["start"] == "2026-03-03T15:00:00Z"
                        && event["end"] == "2026-03-03T15:30:00Z",
                    || {
                        format!(
                            "the event is from {} to {}, not 15:00 to 15:30",
                            event["start"], event["end"]
                        )
                    },
                )?;
                let attendees: Vec<String> = event["attendees"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                ensure(
                    office::attendee_names(&attendees) == ["ana", "ben", "chloe"],
                    || format!("the attendees are {attendees:?}"),
                )
            },
        },
    ]
}
