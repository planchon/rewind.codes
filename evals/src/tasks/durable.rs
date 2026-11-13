//! Durable tasks: sleeping, polling, waiting for signals with and without a timeout, webhooks.
//! The world goes on after the conversation, in virtual time, so a run that waits days does
//! what it was written to do before the checker looks.

use std::time::Duration;

use rewind_core::Timestamp;
use serde_json::{Value, json};

use crate::clock::{DAY, HOUR, T0, parse_rfc3339, rfc3339};
use crate::fake::{chat, ops, tracker};
use crate::task::{Difficulty, Task, ensure};
use crate::world::{PUBLIC_URL, Reaction, WorldSpec};

fn time_of(value: &Value) -> Option<Timestamp> {
    value.as_str().and_then(parse_rfc3339)
}

pub(super) fn tasks() -> Vec<Task> {
    vec![
        Task {
            id: "deploy_and_announce",
            difficulty: Difficulty::Medium,
            features: &["sleep", "polling"],
            world: || WorldSpec {
                servers: vec!["ops", "chat"],
                ..WorldSpec::default()
            },
            setup: &[],
            prompt: |_| {
                "Deploy version 2.4.1 of the api service. Deployments take up to an hour: check \
                 its status every 10 minutes until it has succeeded, then post \"api 2.4.1 is \
                 live\" to #deploys."
                    .into()
            },
            check: |outcome| {
                let deployments = outcome.state("ops")["deployments"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                ensure(deployments.len() == 1, || {
                    format!("{} deployments were started, not 1", deployments.len())
                })?;
                let deployment = &deployments[0];
                ensure(
                    deployment["service"] == "api" && deployment["version"] == "2.4.1",
                    || {
                        format!(
                            "the deployment is {} {}",
                            deployment["service"], deployment["version"]
                        )
                    },
                )?;
                let posted = chat::messages_in(outcome.state("chat"), "deploys");
                let live = posted
                    .iter()
                    .find(|message| message.text.contains("api 2.4.1 is live"))
                    .ok_or("nothing was posted to #deploys")?;
                let finished = time_of(&deployment["finished_at"]);
                ensure(parse_rfc3339(&live.ts) >= finished, || {
                    format!(
                        "the announcement was posted at {}, before the deployment succeeded at {}",
                        live.ts, deployment["finished_at"]
                    )
                })
            },
        },
        Task {
            id: "approve_refunds",
            difficulty: Difficulty::Medium,
            features: &["signal", "durable wait"],
            world: || WorldSpec {
                servers: vec!["crm"],
                reactions: vec![Reaction {
                    signal: "refund_approval",
                    payload: json!({ "approved": ["O-601", "O-603"] }),
                    after: Duration::from_millis(2 * HOUR),
                }],
                ..WorldSpec::default()
            },
            setup: &[],
            prompt: |_| {
                "Orders O-601, O-602 and O-603 are disputed. The finance team will send their \
                 decision to your run as the signal \"refund_approval\", with the payload \
                 { approved: string[] }: the ids of the orders to refund. Wait for it, then refund \
                 exactly those orders with the reason \"dispute\"."
                    .into()
            },
            check: |outcome| {
                let refunds = outcome.state("crm")["refunds"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                let mut refunded: Vec<&str> = refunds
                    .values()
                    .filter_map(|refund| refund["order_id"].as_str())
                    .collect();
                refunded.sort_unstable();
                ensure(refunded == ["O-601", "O-603"], || {
                    format!("the refunded orders are {refunded:?}, not O-601 and O-603")
                })?;
                ensure(
                    refunds.values().all(|refund| refund["reason"] == "dispute"),
                    || "a refund's reason is not \"dispute\"".to_owned(),
                )
            },
        },
        Task {
            id: "budget_timeout",
            difficulty: Difficulty::Medium,
            features: &["signal with timeout", "durable wait"],
            world: || WorldSpec {
                servers: vec!["chat", "office"],
                ..WorldSpec::default()
            },
            setup: &[],
            prompt: |_| {
                "Post \"Please approve the Q2 budget\" to #finance, then wait up to 24 hours for \
                 the signal \"budget_decision\", whose payload is { approved: boolean }. If the \
                 budget is approved, email cfo@acme.test with the subject \"Budget approved\". If \
                 no decision arrives within 24 hours, post \"No decision on the Q2 budget, \
                 escalating\" to #managers."
                    .into()
            },
            check: |outcome| {
                let chat_state = outcome.state("chat");
                let finance = chat::messages_in(chat_state, "finance");
                ensure(
                    finance
                        .iter()
                        .any(|m| m.text.contains("Please approve the Q2 budget")),
                    || "nothing asked #finance for the approval".to_owned(),
                )?;
                let managers = chat::messages_in(chat_state, "managers");
                let escalation = managers
                    .iter()
                    .find(|m| m.text.contains("No decision on the Q2 budget"))
                    .ok_or("nothing was escalated to #managers")?;
                let at = parse_rfc3339(&escalation.ts).unwrap_or(Timestamp(0));
                ensure(at.0 >= T0.0 + DAY - HOUR, || {
                    format!(
                        "the escalation was posted at {}, before the 24 hours ended",
                        escalation.ts
                    )
                })?;
                let sent = outcome.state("office")["sent"]
                    .as_array()
                    .map_or(0, Vec::len);
                ensure(sent == 0, || {
                    format!("{sent} emails were sent, though nothing was approved")
                })
            },
        },
        Task {
            id: "ci_then_deploy",
            difficulty: Difficulty::Hard,
            features: &["webhook", "signal", "durable wait"],
            world: || WorldSpec {
                servers: vec!["ops"],
                webhook: true,
                ..WorldSpec::default()
            },
            setup: &[],
            prompt: |_| {
                "Trigger a CI build of acme/api at commit 3f2a9c1. Give the CI a callback URL made \
                 with webhook(\"build_done\") so that it reports the result to your run, wait for \
                 that signal, and if the build passed, deploy version 2.5.0 of the api service."
                    .into()
            },
            check: |outcome| {
                let state = outcome.state("ops");
                let builds = state["builds"].as_array().cloned().unwrap_or_default();
                let build = builds
                    .iter()
                    .find(|build| {
                        build["commit"]
                            .as_str()
                            .is_some_and(|c| c.starts_with(ops::GREEN_COMMIT))
                    })
                    .ok_or("no build of 3f2a9c1 was triggered")?;
                ensure(
                    build["callback_url"]
                        .as_str()
                        .is_some_and(|url| url.starts_with(&format!("{PUBLIC_URL}/hooks/"))),
                    || {
                        format!(
                            "the build's callback is {}, not a webhook of the run",
                            build["callback_url"]
                        )
                    },
                )?;
                ensure(
                    outcome
                        .delivered
                        .iter()
                        .any(|d| d.signal == "build_done" && d.source == "webhook"),
                    || "the CI's callback reached no run".to_owned(),
                )?;
                let deployments = state["deployments"].as_array().cloned().unwrap_or_default();
                ensure(deployments.len() == 1, || {
                    format!("{} deployments were started, not 1", deployments.len())
                })?;
                let deployment = &deployments[0];
                ensure(
                    deployment["service"] == "api" && deployment["version"] == "2.5.0",
                    || {
                        format!(
                            "the deployment is {} {}",
                            deployment["service"], deployment["version"]
                        )
                    },
                )?;
                ensure(
                    time_of(&deployment["started_at"]) >= time_of(&build["finished_at"]),
                    || {
                        format!(
                            "the deployment started at {}, before the build ended at {}",
                            deployment["started_at"], build["finished_at"]
                        )
                    },
                )
            },
        },
        Task {
            id: "daily_standup_reports",
            difficulty: Difficulty::Hard,
            features: &["sleep", "loop over days", "Date"],
            world: || WorldSpec {
                servers: vec!["tracker", "chat"],
                ..WorldSpec::default()
            },
            setup: &[],
            prompt: |_| {
                "For the next three days, starting tomorrow, post every day at 09:00 UTC to \
                 #standup the number of open issues in acme/api at that time, as \"Open issues in \
                 acme/api: <n>\"."
                    .into()
            },
            check: |outcome| {
                let open = tracker::api_issues()
                    .iter()
                    .filter(|i| i.state == "open")
                    .count();
                let posted = chat::messages_in(outcome.state("chat"), "standup");
                ensure(posted.len() == 3, || {
                    format!("{} reports were posted, not 3", posted.len())
                })?;
                for (day, message) in (1..=3u64).zip(&posted) {
                    let expected = Timestamp(T0.0 + day * DAY);
                    let at = parse_rfc3339(&message.ts).unwrap_or(Timestamp(0));
                    ensure(at.0.abs_diff(expected.0) <= 5 * 60_000, || {
                        format!(
                            "report {day} was posted at {}, not {}",
                            message.ts,
                            rfc3339(expected)
                        )
                    })?;
                    ensure(
                        message
                            .text
                            .contains(&format!("Open issues in acme/api: {open}")),
                        || format!("report {day} says {:?}", message.text),
                    )?;
                }
                Ok(())
            },
        },
    ]
}
