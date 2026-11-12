//! The whole harness, driven by the scripted model: every task is solved by a reference
//! solution (`tests/solutions/<task>.ts`), through the agent tools, on the real engine against
//! the fake MCP servers, so each checker is shown to accept a correct solution. Other tests
//! show that wrong solutions fail, and what the metrics and the agent tools' texts are.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::{Arc, LazyLock};

use evals::episode::{self, Episode, EpisodeConfig};
use evals::model::scripted::{Scripted, first_uuid};
use evals::model::{AssistantTurn, Conversation, Model, ToolCall, Turn};
use evals::prompt::Rendering;
use evals::task::Task;
use evals::tasks;
use ptc_check::TsgoChecker;
use serde_json::{Value, json};

/// One checker for the tests, extracted where `ptc-check`'s own tests extract it.
static CHECKER: LazyLock<Arc<TsgoChecker>> = LazyLock::new(|| {
    Arc::new(TsgoChecker::new(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("ptc-check-cache"),
    ))
});

fn task(id: &str) -> Task {
    tasks::select(&[id.to_owned()]).unwrap().remove(0)
}

fn solution(id: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/solutions")
        .join(format!("{id}.ts"));
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn call(name: &str, input: Value) -> AssistantTurn {
    AssistantTurn {
        text: String::new(),
        calls: vec![ToolCall {
            id: format!("call_{name}"),
            name: name.into(),
            input,
        }],
        raw: Value::Null,
    }
}

/// The tool results the model has read so far, latest last.
fn results(conversation: &Conversation) -> Vec<&str> {
    conversation
        .turns
        .iter()
        .filter_map(|turn| match turn {
            Turn::ToolResults { results } => Some(results.iter().map(|r| r.content.as_str())),
            _ => None,
        })
        .flatten()
        .collect()
}

fn answered(conversation: &Conversation) -> usize {
    conversation
        .turns
        .iter()
        .filter(|turn| matches!(turn, Turn::Assistant(_)))
        .count()
}

fn user_text(conversation: &Conversation) -> &str {
    match conversation.turns.first() {
        Some(Turn::User { text }) => text,
        _ => "",
    }
}

/// `"offset":123` in a text.
fn next_offset(text: &str) -> Option<u64> {
    let (_, rest) = text.rsplit_once("\"offset\":")?;
    rest.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// A model that solves `id` with its reference solution: it runs the script, pages through an
/// output too large for one answer, and answers with what it read.
fn reference(id: &'static str) -> Model {
    Model::Scripted(Scripted::from_fn(
        format!("reference:{id}"),
        move |conversation| {
            let last = results(conversation).last().map(|text| (*text).to_owned());
            match (id, answered(conversation)) {
                ("cancel_stuck_run", 0) => Some(call("list_runs", json!({}))),
                ("cancel_stuck_run", 1) => {
                    let listing = last.unwrap_or_default();
                    let line = listing.lines().find(|line| line.contains(": active"))?;
                    Some(call("get_run", json!({ "run_id": first_uuid(line)? })))
                }
                ("cancel_stuck_run", 2) => {
                    let run_id = first_uuid(&last.unwrap_or_default())?;
                    Some(call("cancel_run", json!({ "run_id": run_id })))
                }
                ("fix_failed_run", 0) => Some(call(
                    "retry_run",
                    json!({ "run_id": first_uuid(user_text(conversation))?, "code": solution(id) }),
                )),
                (_, 0) => Some(call(
                    "run_script",
                    json!({ "code": solution(id), "wait": "2m" }),
                )),
                _ => {
                    let last = last.unwrap_or_default();
                    match next_offset(&last) {
                        Some(offset)
                            if last.contains("Read the rest") || last.contains("read the rest") =>
                        {
                            Some(call(
                                "get_run",
                                json!({ "run_id": "{{run_id}}", "offset": offset }),
                            ))
                        }
                        _ if id == "cancel_stuck_run" => Some(episode::answer(
                            "The run was waiting for the signal \"go\"; I cancelled it.",
                        )),
                        _ => Some(episode::answer(&last)),
                    }
                }
            }
        },
    ))
}

async fn run(task: &Task, model: &Model) -> Episode {
    episode::run(
        task,
        model,
        Rendering::Compact,
        CHECKER.clone(),
        &EpisodeConfig::default(),
        0,
    )
    .await
}

async fn solve(id: &'static str) {
    let task = task(id);
    let episode = run(&task, &reference(id)).await;
    let record = &episode.record;
    assert!(
        record.success && record.error.is_none(),
        "{id}: {:?} {:?}\n{}",
        record.error,
        record.failure,
        serde_json::to_string_pretty(&episode.transcript()).unwrap()
    );
    if !matches!(id, "cancel_stuck_run") {
        assert_eq!(record.first_check, Some(true), "{id}");
    }
}

macro_rules! reference_solutions {
    ($($id:ident),* $(,)?) => {
        mod reference {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $id() {
                    super::solve(stringify!($id)).await;
                }
            )*
        }

        #[test]
        fn every_task_has_a_reference_solution() {
            let solved = [$(stringify!($id)),*];
            for task in tasks::all() {
                assert!(solved.contains(&task.id), "{} has no reference solution", task.id);
            }
        }
    };
}

reference_solutions!(
    customer_email,
    post_announcement,
    open_issue,
    average_latency,
    lifetime_value_fan_out,
    count_open_issues,
    missing_orders,
    close_stale_issues,
    export_audit_log,
    revenue_by_region,
    reorder_low_stock,
    unread_digest,
    schedule_meeting,
    deploy_and_announce,
    approve_refunds,
    budget_timeout,
    ci_then_deploy,
    daily_standup_reports,
    ship_orders_with_retries,
    escalate_customer_issues,
    cancel_stuck_run,
    fix_failed_run,
);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_refused_first_counts_against_the_first_try() {
    let wrong = solution("customer_email").replace("\"C-1007\"", "1007");
    let model = Model::Scripted(Scripted::turns(
        "scripted",
        vec![
            call("run_script", json!({ "code": wrong })),
            call("run_script", json!({ "code": solution("customer_email") })),
            episode::answer("crm says: see above"),
        ],
    ));
    let episode = run(&task("customer_email"), &model).await;
    let record = &episode.record;
    assert_eq!(record.first_check, Some(false));
    assert_eq!(
        (record.scripts, record.refused, record.iterations),
        (2, 1, 3)
    );
    let Turn::ToolResults { results } = &episode.turns[2] else {
        panic!("tool results");
    };
    assert!(results[0].is_error);
    assert!(
        results[0].content.starts_with(
            "The script was refused before anything ran: it does not type-check (1 error)"
        ),
        "{}",
        results[0].content
    );
    assert!(
        results[0].content.contains("error TS2322"),
        "{}",
        results[0].content
    );
    // The answer did not give the email: the task failed, and says why.
    assert!(!record.success);
    assert!(record.failure.as_deref().unwrap().contains("does not give"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retrying_with_new_request_ids_duplicates_shipments() {
    let naive = solution("ship_orders_with_retries")
        .replace("const request_id = `ship-${order_id}`;", "let attempt = 0;")
        .replace(
            "create_shipment({ order_id, request_id })",
            "create_shipment({ order_id, request_id: `${order_id}-${attempt++}` })",
        );
    let model = Model::Scripted(Scripted::turns(
        "scripted",
        vec![call("run_script", json!({ "code": naive, "wait": "2m" }))],
    ));
    let episode = run(&task("ship_orders_with_retries"), &model).await;
    assert!(episode.record.error.is_none(), "{:?}", episode.record.error);
    assert_eq!(
        episode.record.failure.as_deref(),
        Some("O-603 has 2 shipments, not 1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_run_shows_the_call_that_failed_with_its_line() {
    let code = "import { crm } from \"ptc:tools\";\n\nexport default async function main() {\n  return (await crm.get_order({ id: \"O-504\" })).json();\n}\n";
    let model = Model::Scripted(Scripted::turns(
        "scripted",
        vec![call("run_script", json!({ "code": code }))],
    ));
    let episode = run(&task("missing_orders"), &model).await;
    let Turn::ToolResults { results } = &episode.turns[2] else {
        panic!("tool results");
    };
    let text = &results[0].content;
    assert!(results[0].is_error);
    assert!(text.contains(" failed.\nToolError: "), "{text}");
    assert!(
        text.contains("The call that failed: crm.get_order (call 1 at script.ts:4:"),
        "{text}"
    );
    assert!(text.contains("{\"id\":\"O-504\"}"), "{text}");
    assert!(text.contains("error: not_found: no order O-504"), "{text}");
    assert!(text.contains("Trace up to the failure:"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_run_is_reported_as_suspended_and_goes_on_after_the_conversation() {
    let model = Model::Scripted(Scripted::turns(
        "scripted",
        vec![call(
            "run_script",
            json!({ "code": solution("approve_refunds"), "wait": "30s" }),
        )],
    ));
    let episode = run(&task("approve_refunds"), &model).await;
    let Turn::ToolResults { results } = &episode.turns[2] else {
        panic!("tool results");
    };
    let text = &results[0].content;
    assert!(text.contains("is suspended, waiting for:"), "{text}");
    assert!(
        text.contains("- the signal \"refund_approval\" at script.ts:5:"),
        "{text}"
    );
    assert!(episode.record.success, "{:?}", episode.record.failure);
    assert_eq!(episode.record.delivered.len(), 1);
    assert_eq!(episode.record.delivered[0].source, "reaction");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_rendering_gives_the_model_what_it_needs() {
    for rendering in Rendering::ALL {
        let task = task("customer_email");
        let model = Model::Scripted(Scripted::turns(
            "scripted",
            vec![
                call("search_tools", json!({ "query": "customer" })),
                episode::answer("done"),
            ],
        ));
        let episode = episode::run(
            &task,
            &model,
            rendering,
            CHECKER.clone(),
            &EpisodeConfig::default(),
            0,
        )
        .await;
        assert!(episode.system.contains("get_customer"), "{rendering}");
        let Turn::ToolResults { results } = &episode.turns[2] else {
            panic!("tool results");
        };
        assert!(
            results[0].content.contains("get_customer(args: {"),
            "{}",
            results[0].content
        );
    }
}
