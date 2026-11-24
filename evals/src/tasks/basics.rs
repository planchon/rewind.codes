//! Easy tasks: one or a few calls, typed and untyped results, a state change.

use crate::fake::{chat, crm, ops};
use crate::task::{Difficulty, Task, ensure, holds_number};
use crate::world::WorldSpec;

fn world(servers: &[&'static str]) -> WorldSpec {
    WorldSpec {
        servers: servers.to_vec(),
        ..WorldSpec::default()
    }
}

pub(super) fn tasks() -> Vec<Task> {
    vec![
        Task {
            id: "customer_email",
            difficulty: Difficulty::Easy,
            features: &["single call", "ActionContent.json()"],
            world: || world(&["crm"]),
            setup: &[],
            prompt: |_| "What is the email address of customer C-1007?".into(),
            check: |outcome| {
                let email = &crm::customers()[6].email;
                ensure(outcome.answer_has(email), || {
                    format!("the answer does not give {email}")
                })
            },
        },
        Task {
            id: "post_announcement",
            difficulty: Difficulty::Easy,
            features: &["single call", "state change"],
            world: || world(&["chat"]),
            setup: &[],
            prompt: |_| {
                "Post the message \"Deploy freeze starts Friday 18:00 UTC\" to the #ops channel."
                    .into()
            },
            check: |outcome| {
                let state = outcome.state("chat");
                let ops = chat::messages_in(state, "ops");
                ensure(ops.len() == 1, || {
                    format!("#ops has {} messages, not 1", ops.len())
                })?;
                ensure(
                    ops[0]
                        .text
                        .contains("Deploy freeze starts Friday 18:00 UTC"),
                    || format!("#ops says {:?}", ops[0].text),
                )?;
                let total = state["messages"].as_array().map_or(0, Vec::len);
                ensure(total == 1, || {
                    format!("{total} messages were posted, not 1")
                })
            },
        },
        Task {
            id: "open_issue",
            difficulty: Difficulty::Easy,
            features: &["single call", "typed result", "state change"],
            world: || world(&["tracker"]),
            setup: &[],
            prompt: |_| {
                "Open an issue in acme/web titled \"Login button misaligned on Safari\" with the label bug."
                    .into()
            },
            check: |outcome| {
                let issues = outcome.state("tracker")["acme/web"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let created: Vec<_> = issues
                    .iter()
                    .filter(|issue| issue["number"].as_u64() > Some(25))
                    .collect();
                ensure(created.len() == 1, || {
                    format!("{} issues were opened in acme/web, not 1", created.len())
                })?;
                let issue = created[0];
                ensure(
                    issue["title"] == "Login button misaligned on Safari",
                    || format!("the issue is titled {}", issue["title"]),
                )?;
                ensure(
                    issue["labels"]
                        .as_array()
                        .is_some_and(|labels| labels.iter().any(|label| label == "bug")),
                    || format!("the issue's labels are {}", issue["labels"]),
                )
            },
        },
        Task {
            id: "average_latency",
            difficulty: Difficulty::Easy,
            features: &["typed result", "fan-out"],
            world: || world(&["ops"]),
            setup: &[],
            prompt: |_| {
                "What is the average p95 latency, in ms, of the services api, web and worker? \
                 Return it as the script's output."
                    .into()
            },
            check: |outcome| {
                let expected = ["api", "web", "worker"]
                    .iter()
                    .filter_map(|service| ops::metric(service, "latency_p95"))
                    .map(|(value, _)| value)
                    .sum::<f64>()
                    / 3.0;
                ensure(
                    outcome
                        .outputs()
                        .iter()
                        .any(|output| holds_number(output, expected)),
                    || format!("no run returned {expected}"),
                )
            },
        },
    ]
}
