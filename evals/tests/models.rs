//! The model clients against local fakes of the Anthropic and OpenAI APIs: the headers, the
//! tool loop on the wire, retries after an overload, and the tokens counted, through a whole
//! episode. No real model is called.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use evals::episode::{self, EpisodeConfig};
use evals::model::anthropic::{API_VERSION, Anthropic};
use evals::model::openai::OpenAi;
use evals::model::{Model, Usage};
use evals::prompt::Rendering;
use evals::tasks;
use rewind_check::TsgoChecker;
use serde_json::{Value, json};

static CHECKER: LazyLock<Arc<TsgoChecker>> = LazyLock::new(|| {
    Arc::new(TsgoChecker::new(
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("rewind-check-cache"),
    ))
});

const SOLUTION: &str = include_str!("solutions/customer_email.ts");

#[derive(Debug, Default)]
struct Seen {
    requests: Vec<(HeaderMap, Value)>,
}

type Shared = Arc<Mutex<Seen>>;

/// The text of the last tool result a request carries, whichever API it speaks.
fn last_result(body: &Value) -> Option<String> {
    if let Some(messages) = body["messages"].as_array() {
        return messages.last()?["content"]
            .as_array()?
            .iter()
            .find_map(|block| {
                (block["type"] == "tool_result")
                    .then(|| block["content"].as_str().unwrap_or_default().to_owned())
            });
    }
    body["input"].as_array()?.iter().rev().find_map(|item| {
        (item["type"] == "function_call_output")
            .then(|| item["output"].as_str().unwrap_or_default().to_owned())
    })
}

async fn messages(
    State(seen): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let count = {
        let mut seen = seen.lock().unwrap();
        seen.requests.push((headers, body.clone()));
        seen.requests.len()
    };
    match count {
        1 => (StatusCode::from_u16(529).unwrap(), [("retry-after", "0")], "overloaded").into_response(),
        2 => Json(json!({
            "content": [
                { "type": "thinking", "thinking": "", "signature": "sig-1" },
                { "type": "tool_use", "id": "toolu_1", "name": "run_script", "input": { "code": SOLUTION } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 1000, "output_tokens": 100, "cache_creation_input_tokens": 3000, "cache_read_input_tokens": 0 }
        }))
        .into_response(),
        _ => Json(json!({
            "content": [{ "type": "text", "text": format!("Done: {}", last_result(&body).unwrap_or_default()) }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 200, "output_tokens": 20, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 3000 }
        }))
        .into_response(),
    }
}

async fn responses(
    State(seen): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let count = {
        let mut seen = seen.lock().unwrap();
        seen.requests.push((headers, body.clone()));
        seen.requests.len()
    };
    match count {
        1 => (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "0")], "slow down").into_response(),
        2 => Json(json!({
            "status": "completed",
            "output": [
                { "type": "reasoning", "id": "rs_1", "encrypted_content": "e", "summary": [] },
                { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "run_script", "arguments": serde_json::to_string(&json!({ "code": SOLUTION })).unwrap() }
            ],
            "usage": { "input_tokens": 1500, "output_tokens": 300, "input_tokens_details": { "cached_tokens": 0 } }
        }))
        .into_response(),
        _ => Json(json!({
            "status": "completed",
            "output": [{ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": format!("Done: {}", last_result(&body).unwrap_or_default()) }] }],
            "usage": { "input_tokens": 2000, "output_tokens": 50, "input_tokens_details": { "cached_tokens": 1500 } }
        }))
        .into_response(),
    }
}

async fn serve(seen: Shared) -> String {
    let router = axum::Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .with_state(seen);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{address}")
}

async fn episode(model: &Model) -> episode::Episode {
    let task = tasks::select(&["customer_email".to_owned()])
        .unwrap()
        .remove(0);
    episode::run(
        &task,
        model,
        Rendering::Compact,
        CHECKER.clone(),
        &EpisodeConfig::default(),
        0,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_solves_a_task_through_the_messages_api() {
    let seen = Shared::default();
    let url = serve(seen.clone()).await;
    let model =
        Model::Anthropic(Anthropic::new("claude-test", "test-key", url, Some("high")).unwrap());
    let episode = episode(&model).await;
    let record = &episode.record;
    assert!(record.success, "{:?} {:?}", record.error, record.failure);
    assert_eq!(record.iterations, 2);
    assert_eq!(record.first_check, Some(true));
    assert_eq!(
        record.usage,
        Usage {
            input: 1200,
            output: 120,
            cache_read: 3000,
            cache_write: 3000
        }
    );

    let seen = seen.lock().unwrap();
    assert_eq!(seen.requests.len(), 3, "one retry after the 529");
    let (headers, body) = &seen.requests[2];
    assert_eq!(headers["x-api-key"], "test-key");
    assert_eq!(headers["anthropic-version"], API_VERSION);
    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["output_config"]["effort"], "high");
    assert_eq!(body["cache_control"]["type"], "ephemeral");
    assert_eq!(body["tools"].as_array().unwrap().len(), 9);
    assert!(
        body["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("crm.get_customer(")
    );
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    // The thinking block goes back unchanged.
    assert_eq!(messages[1]["content"][0]["signature"], "sig-1");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
    assert!(
        messages[2]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("completed."),
        "{}",
        messages[2]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_openai_model_solves_a_task_through_the_responses_api() {
    let seen = Shared::default();
    let url = serve(seen.clone()).await;
    let model = Model::OpenAi(OpenAi::new("gpt-test", "test-key", url, Some("high")).unwrap());
    let episode = episode(&model).await;
    let record = &episode.record;
    assert!(record.success, "{:?} {:?}", record.error, record.failure);
    assert_eq!(
        record.usage,
        Usage {
            input: 2000,
            output: 350,
            cache_read: 1500,
            cache_write: 0
        }
    );

    let seen = seen.lock().unwrap();
    assert_eq!(seen.requests.len(), 3, "one retry after the 429");
    let (headers, body) = &seen.requests[2];
    assert_eq!(headers["authorization"], "Bearer test-key");
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"]["effort"], "high");
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[1]["encrypted_content"], "e");
    assert_eq!(input[2]["call_id"], "call_1");
    assert!(input[2].get("id").is_none());
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_1");
}

#[test]
fn models_are_chosen_by_name() {
    assert!(matches!(
        Model::from_name("mistral-large", None),
        Err(evals::model::ModelError::Unknown(_))
    ));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("turns.json");
    std::fs::write(
        &path,
        r#"[{ "calls": [{ "name": "list_runs", "input": {} }] }, { "text": "done" }]"#,
    )
    .unwrap();
    let model = Model::from_name(&format!("scripted:{}", path.display()), None).unwrap();
    assert!(model.name().starts_with("scripted:"));
}
