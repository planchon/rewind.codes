//! The whole harness, driven by the scripted model: every task is solved by a reference
//! solution (`tests/solutions/<task>.ts`), through the agent tools, on the real engine against
//! the fake MCP servers, so each checker is shown to accept a correct solution. Other tests
//! show that wrong solutions fail, and what the metrics and the agent tools' texts are.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::{Arc, LazyLock};

use evals::episode::{self, Episode, EpisodeConfig};
use evals::model::scripted::{Scripted, first_run, first_uuid};
use evals::model::{AssistantTurn, Call, Conversation, Model, Turn};
use evals::prompt::Rendering;
use evals::task::Task;
use evals::tasks;
use rewind_check::TsgoChecker;
use serde_json::{Value, json};

/// One checker for the tests, extracted where `rewind-check`'s own tests extract it.
static CHECKER: LazyLock<Arc<TsgoChecker>> = LazyLock::new(|| {
    Arc::new(TsgoChecker::new(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("rewind-check-cache"),
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
        calls: vec![Call {
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

/// The first digest (64 lowercase hex digits) of a text.
fn digest_in(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(63)).find_map(|start| {
        let candidate = text.get(start..start + 64)?;
        let hex = candidate
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        let bounded = start == 0 || !bytes[start - 1].is_ascii_hexdigit();
        (hex && bounded).then(|| candidate.to_owned())
    })
}

/// The run of a line of `list_runs` that names `needle`.
fn run_named(listing: &str, needle: &str) -> Option<String> {
    first_run(listing.lines().find(|line| line.contains(needle))?)
}

/// `ship_orders` as `fix_workflow_regression` saves it, with `code` and these suite changes.
fn ship_orders(code: &str, suite: Value) -> Value {
    let mut workflow = json!({
        "name": "ship_orders",
        "description": "Buy a shipping label for each order, and give each order's tracking number.",
        "code": code,
        "input": {
            "type": "object",
            "properties": { "orders": { "type": "array", "items": { "type": "string" } } },
            "required": ["orders"],
            "additionalProperties": false
        },
        "output": {
            "type": "object",
            "properties": { "shipped": { "type": "array", "items": {
                "type": "object",
                "properties": {
                    "order": { "type": "string" },
                    "tracking": { "type": ["string", "null"] }
                },
                "required": ["order", "tracking"]
            } } },
            "required": ["shipped"]
        }
    });
    for (key, value) in suite.as_object().cloned().unwrap_or_default() {
        workflow[key] = value;
    }
    workflow
}

/// `bug_digest` as `upgrade_workflow_suite` saves it, with `code` and these suite changes.
fn bug_digest(code: &str, suite: Value) -> Value {
    let mut workflow = json!({
        "name": "bug_digest",
        "description": "Post the open bugs of a repository to #bugs.",
        "code": code,
        "input": {
            "type": "object",
            "properties": { "repo": { "type": "string" } },
            "required": ["repo"],
            "additionalProperties": false
        },
        "output": {
            "type": "object",
            "properties": { "posted": { "type": "integer" } },
            "required": ["posted"]
        }
    });
    for (key, value) in suite.as_object().cloned().unwrap_or_default() {
        workflow[key] = value;
    }
    workflow
}

/// An open bug as the upgraded tracker lists it.
fn bug(number: u64, title: &str) -> Value {
    json!({
        "number": number,
        "title": title,
        "state": "open",
        "labels": ["bug"],
        "reporter": "dana",
        "updated_at": "2026-02-01T00:00:00Z"
    })
}

/// The suite changes of the reference solution of `upgrade_workflow_suite`: the tests of the
/// old tracker updated for the new one, the one about truncated lists retired, and a test of
/// the new paging.
fn upgraded_suite() -> Value {
    let posted = json!({ "returns": { "content": [{ "type": "text", "text": "{\"ok\":true}" }] } });
    json!({
        "update": [
            {
                "name": "posts the open bugs",
                "reason": "the tracker renamed `labels` to `label`, and its issues now carry their reporter",
                "test": {
                    "name": "posts the open bugs",
                    "input": { "repo": "acme/api" },
                    "world": { "actions": {
                        "tracker.list_issues": {
                            "args": { "repo": "acme/api", "label": "bug" },
                            "returns": { "issues": [bug(3, "Login fails with SSO (3)"), bug(6, "Slow dashboard (6)")] }
                        },
                        "chat.post_message": posted
                    } },
                    "expect": {
                        "status": "completed",
                        "output": { "posted": 2 },
                        "calls": [{ "action": "chat.post_message", "args": { "channel": "#bugs" }, "times": 1 }]
                    }
                }
            },
            {
                "name": "posts nothing without open bugs",
                "reason": "the tracker renamed `labels` to `label`",
                "test": {
                    "name": "posts nothing without open bugs",
                    "input": { "repo": "acme/web" },
                    "world": { "actions": {
                        "tracker.list_issues": { "args": { "label": "bug" }, "returns": { "issues": [] } }
                    } },
                    "expect": {
                        "status": "completed",
                        "output": { "posted": 0 },
                        "calls": [{ "action": "chat.post_message", "times": 0 }]
                    }
                }
            }
        ],
        "retire": [{
            "names": ["says when the list is truncated"],
            "reason": "the tracker pages through the issues with a cursor instead of truncating the list"
        }],
        "tests": [{
            "name": "posts the bugs of every page",
            "input": { "repo": "acme/api" },
            "world": { "actions": {
                "tracker.list_issues": [
                    { "args": { "cursor": "page_25" }, "returns": { "issues": [bug(60, "Pagination skips items (60)")] } },
                    { "args": { "label": "bug" }, "returns": { "issues": [bug(3, "Login fails with SSO (3)")], "next_cursor": "page_25" } }
                ],
                "chat.post_message": posted
            } },
            "expect": {
                "status": "completed",
                "output": { "posted": 2 },
                "calls": [{ "action": "tracker.list_issues", "times": 2 }]
            }
        }]
    })
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

/// A model that solves `id` with its reference solution: it runs the code, pages through an
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
                    Some(call("get_run", json!({ "run_id": first_run(line)? })))
                }
                ("cancel_stuck_run", 2) => {
                    let run_id = first_run(&last.unwrap_or_default())?;
                    Some(call("cancel_run", json!({ "run_id": run_id })))
                }
                ("fix_failed_run", 0) => Some(call(
                    "retry_run",
                    json!({ "run_id": first_uuid(user_text(conversation))?, "code": solution(id) }),
                )),
                // Find the failed run, record it, save the fix with the run as a regression
                // test, then finish the run.
                ("fix_workflow_regression", 0) => {
                    Some(call("list_runs", json!({ "status": "failed" })))
                }
                ("fix_workflow_regression", 1) => {
                    let failed = run_named(results(conversation)[0], "ship_orders")?;
                    Some(call(
                        "record_tests",
                        json!({ "workflow": "ship_orders", "runs": [failed] }),
                    ))
                }
                ("fix_workflow_regression", 2) => {
                    let digest = digest_in(&last.unwrap_or_default())?;
                    let expect = json!({
                        "status": "completed",
                        "calls": [{ "action": "shipping.create_label", "times": 3 }]
                    });
                    Some(call(
                        "save_workflow",
                        ship_orders(
                            &solution(id),
                            json!({ "tests": [{ "from": digest, "expect": expect }] }),
                        ),
                    ))
                }
                ("fix_workflow_regression", 3) => Some(call(
                    "retry_run",
                    json!({
                        "run_id": run_named(results(conversation)[0], "ship_orders")?,
                        "code": solution(id)
                    }),
                )),
                // Read the workflow, see its tests go stale against the new server, then save
                // the adapted code with its tests updated or retired, each with a reason.
                ("upgrade_workflow_suite", 0) => {
                    Some(call("get_workflow", json!({ "name": "bug_digest" })))
                }
                ("upgrade_workflow_suite", 1) => Some(call(
                    "test_workflow",
                    json!({ "code": solution(id), "workflow": "bug_digest" }),
                )),
                ("upgrade_workflow_suite", 2) => Some(call(
                    "save_workflow",
                    bug_digest(&solution(id), upgraded_suite()),
                )),
                (_, 0) => Some(call(
                    "run_workflow",
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
    fix_workflow_regression,
    upgrade_workflow_suite,
);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_refused_first_counts_against_the_first_try() {
    let wrong = solution("customer_email").replace("\"C-1007\"", "1007");
    let model = Model::Scripted(Scripted::turns(
        "scripted",
        vec![
            call("run_workflow", json!({ "code": wrong })),
            call(
                "run_workflow",
                json!({ "code": solution("customer_email") }),
            ),
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
            "The code was refused before anything ran: the code does not type-check (1 error): fix the diagnostics and start it again"
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
        vec![call("run_workflow", json!({ "code": naive, "wait": "2m" }))],
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
    let code = "import { crm } from \"rewind:actions\";\n\nexport default async function main() {\n  return (await crm.get_order({ id: \"O-504\" })).json();\n}\n";
    let model = Model::Scripted(Scripted::turns(
        "scripted",
        vec![call("run_workflow", json!({ "code": code }))],
    ));
    let episode = run(&task("missing_orders"), &model).await;
    let Turn::ToolResults { results } = &episode.turns[2] else {
        panic!("tool results");
    };
    let text = &results[0].content;
    assert!(results[0].is_error);
    assert!(text.contains(" failed.\nActionError: "), "{text}");
    assert!(
        text.contains("The call that failed: crm.get_order (call 1 at workflow.ts:4:"),
        "{text}"
    );
    assert!(text.contains("{\"id\":\"O-504\"}"), "{text}");
    assert!(text.contains("error: not_found: no order O-504"), "{text}");
    assert!(text.contains("Transcript up to the failure:"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_run_is_reported_as_suspended_and_goes_on_after_the_conversation() {
    let model = Model::Scripted(Scripted::turns(
        "scripted",
        vec![call(
            "run_workflow",
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
        text.contains("- the signal \"refund_approval\" at workflow.ts:5:"),
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
                call("search_actions", json!({ "query": "customer" })),
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

/// A turn calling several tools, each with its own id.
fn calls(list: Vec<(&str, Value)>) -> AssistantTurn {
    AssistantTurn {
        text: String::new(),
        calls: list
            .into_iter()
            .enumerate()
            .map(|(n, (name, input))| Call {
                id: format!("call_{n}_{name}"),
                name: name.into(),
                input,
            })
            .collect(),
        raw: Value::Null,
    }
}

/// The results of the episode's turns, in order.
fn all_results(episode: &Episode) -> Vec<&evals::model::ToolResult> {
    episode
        .turns
        .iter()
        .filter_map(|turn| match turn {
            Turn::ToolResults { results } => Some(results.iter()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// A fix of `ship_orders` that hides the carrier's refusals, which a test of its suite pins.
fn swallowing_fix() -> String {
    solution("fix_workflow_regression").replace(
        "shipping.create_label({ order_id })",
        "shipping.create_label({ order_id }).catch(() => ({ order_id, tracking: undefined }))",
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gate_refuses_a_fix_that_breaks_the_suite_and_counts_it() {
    let model = Model::Scripted(Scripted::from_fn("scripted", |conversation| {
        let failed = || run_named(results(conversation).first()?, "ship_orders");
        match answered(conversation) {
            0 => Some(call("list_runs", json!({ "status": "failed" }))),
            1 => Some(call(
                "save_workflow",
                ship_orders(&swallowing_fix(), json!({})),
            )),
            2 => Some(call(
                "retry_run",
                json!({ "run_id": failed()?, "code": swallowing_fix() }),
            )),
            _ => Some(episode::answer("done")),
        }
    }));
    let episode = run(&task("fix_workflow_regression"), &model).await;
    let record = &episode.record;
    assert!(record.error.is_none(), "{:?}", record.error);
    let results = all_results(&episode);
    let saved = &results[1];
    assert!(saved.is_error);
    assert!(
        saved.content.starts_with(
            "The workflow was not saved: 1 of 3 tests of the suite did not pass: \"fails when the carrier refuses a label\" (failed): "
        ),
        "{}",
        saved.content
    );
    assert!(
        saved.content.contains(
            "\nFAIL \"fails when the carrier refuses a label\" (failed): it ran, and expectations do not hold:\n"
        ),
        "{}",
        saved.content
    );
    assert!(
        saved.content.ends_with("with a reason."),
        "{}",
        saved.content
    );
    let retried = &results[2];
    assert!(retried.is_error);
    let failed = first_run(&retried.content).unwrap();
    assert!(
        retried.content.starts_with(&format!(
            "Run {failed} was not retried: 1 of 3 tests of the suite of ship_orders@"
        )),
        "{}",
        retried.content
    );
    assert!(
        retried
            .content
            .contains("A fix must keep every behavior the suite of ship_orders@"),
        "{}",
        retried.content
    );
    assert_eq!(record.gate_refusals, 2);
    assert_eq!(record.tests_written, 0);
    assert_eq!(record.runs, 0, "the refused retry started nothing");
    assert_eq!(
        record.failure.as_deref(),
        Some("no new version of ship_orders was saved")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fix_run_anew_instead_of_finishing_the_failed_run_fails_the_task() {
    let model = Model::Scripted(Scripted::from_fn("scripted", |conversation| {
        let last = results(conversation).last().map(|text| (*text).to_owned());
        match answered(conversation) {
            0 => Some(call("list_runs", json!({ "status": "failed" }))),
            1 => Some(call(
                "record_tests",
                json!({ "workflow": "ship_orders", "runs": [run_named(results(conversation)[0], "ship_orders")?] }),
            )),
            2 => Some(call(
                "save_workflow",
                ship_orders(
                    &solution("fix_workflow_regression"),
                    json!({ "tests": [{ "from": digest_in(&last?)?, "expect": { "status": "completed" } }] }),
                ),
            )),
            3 => Some(call(
                "run_workflow",
                json!({
                    "code": solution("fix_workflow_regression"),
                    "input": { "orders": ["A-1003", "P-1004", "A-1005"] }
                }),
            )),
            _ => Some(episode::answer("done")),
        }
    }));
    let episode = run(&task("fix_workflow_regression"), &model).await;
    let record = &episode.record;
    assert!(record.error.is_none(), "{:?}", record.error);
    assert_eq!(record.tests_written, 1);
    let failure = record.failure.as_deref().unwrap_or_default();
    assert!(failure.contains("was not finished"), "{failure}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_test_cannot_be_updated_by_its_expectation_alone() {
    let model = Model::Scripted(Scripted::from_fn(
        "scripted",
        |conversation| match answered(conversation) {
            0 => Some(call(
                "save_workflow",
                bug_digest(
                    &solution("upgrade_workflow_suite"),
                    json!({
                        "update": [{
                            "name": "posts the open bugs",
                            "reason": "the server changed",
                            "expect": { "status": "completed" }
                        }],
                        "retire": [{
                            "names": ["posts nothing without open bugs", "says when the list is truncated"],
                            "reason": "the server changed"
                        }]
                    }),
                ),
            )),
            _ => Some(episode::answer("done")),
        },
    ));
    let episode = run(&task("upgrade_workflow_suite"), &model).await;
    let record = &episode.record;
    assert!(record.error.is_none(), "{:?}", record.error);
    let results = all_results(&episode);
    assert!(results[0].is_error);
    assert!(
        results[0].content.contains(
            "FAIL \"posts the open bugs\" (stale): it names actions, answers or args the action set no longer has; it was not run.\n  - rule 1 of tracker.list_issues answers what its output schema refuses"
        ),
        "{}",
        results[0].content
    );
    assert_eq!(record.gate_refusals, 1);
    assert_eq!(
        record.failure.as_deref(),
        Some("no new version of bug_digest was saved")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_workflow_tools_say_what_is_wrong_with_their_arguments() {
    let code = solution("fix_workflow_regression");
    let workflow = ship_orders(&code, json!({}));
    let with = |key: &str, value: Value| {
        let mut workflow = workflow.clone();
        workflow[key] = value;
        workflow
    };
    let model = Model::Scripted(Scripted::turns(
        "scripted",
        vec![
            calls(vec![
                ("test_workflow", json!({ "code": code })),
                ("test_workflow", json!({ "code": code, "tests": {} })),
                ("test_workflow", json!({ "code": code, "workflow": 3 })),
                ("test_workflow", json!({ "code": code, "tests": ["00"] })),
                ("save_workflow", with("input", json!("string"))),
                ("save_workflow", with("signals", json!([]))),
                ("save_workflow", with("retire", json!([{ "names": ["a"] }]))),
                (
                    "save_workflow",
                    with("update", json!([{ "name": "a", "expect": {} }])),
                ),
                ("save_workflow", with("parent", json!(4))),
                (
                    "save_workflow",
                    with(
                        "retire",
                        json!([{ "names": ["asks first"], "reason": "deprecated" }]),
                    ),
                ),
                ("get_workflow", json!({ "name": "nope" })),
                ("get_workflow", json!({})),
                (
                    "record_tests",
                    json!({ "workflow": "ship_orders", "limit": 0 }),
                ),
                (
                    "record_tests",
                    json!({ "workflow": "ship_orders", "runs": "r" }),
                ),
                ("record_tests", json!({ "workflow": "nope" })),
                ("record_tests", json!({ "workflow": "ship_orders" })),
                (
                    "test_workflow",
                    json!({ "code": code, "workflow": "ship_orders" }),
                ),
            ]),
            episode::answer("done"),
        ],
    ));
    let episode = run(&task("fix_workflow_regression"), &model).await;
    assert!(episode.record.error.is_none(), "{:?}", episode.record.error);
    let texts: Vec<&str> = all_results(&episode)
        .iter()
        .map(|result| result.content.as_str())
        .collect();
    let expected = [
        "test_workflow: give `tests`, or a `workflow` whose suite runs against the code, or both.",
        "test_workflow: `tests` must be an array of tests: each one an object, or the digest of a recorded test as a string.",
        "test_workflow: `workflow` must be a non-empty string.",
        "The tests were refused before running: \"00\" is not a test's digest: 64 lowercase hex digits, as record_tests returns them",
        "save_workflow: `input` must be a JSON Schema: an object.",
        "save_workflow: `signals` must be an object of JSON Schemas by signal name.",
        "save_workflow: `retire` must be an array of { names, reason }: the names of inherited tests, and why they no longer hold.",
        "save_workflow: `update` must be an array of { name, reason, test?, expect? }: the name of an inherited test, why it changes, and the test that replaces it or its new expectation.",
        "save_workflow: `parent` must be a non-empty string.",
        "The workflow was not saved: retire names \"asks first\", which the parent's suite does not have (it has \"buys no label without orders\", \"fails when the carrier refuses a label\", \"ships every order with its tracking number\")",
        "get_workflow failed: no workflow nope for this API key: save it with POST /v1/workflows, or list them with GET /v1/workflows",
        "get_workflow: `name` must be a non-empty string.",
        "record_tests: `limit` must be an integer, 1 or more.",
        "record_tests: `runs` must be an array of non-empty strings.",
        "record_tests failed: no workflow nope for this caller: list them with GET /v1/workflows",
    ];
    for (n, text) in expected.iter().enumerate() {
        assert_eq!(texts[n], *text, "call {n}");
    }
    // Both runs of the workflow, newest first: two paths.
    let recorded = texts[15];
    assert!(
        recorded.starts_with("Recorded 2 tests from the runs of ship_orders, one per path:\n"),
        "{recorded}"
    );
    assert!(
        recorded.contains("(failed): shipping.create_label×n → failed"),
        "{recorded}"
    );
    assert!(
        recorded.contains("(completed): shipping.create_label×n → completed"),
        "{recorded}"
    );
    assert_eq!(
        texts[16],
        "All 3 tests passed.\nPASS \"buys no label without orders\"\nPASS \"fails when the carrier refuses a label\"\nPASS \"ships every order with its tracking number\""
    );
}
