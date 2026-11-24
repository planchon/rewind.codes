//! Claude through the Anthropic Messages API (`POST /v1/messages`), over HTTP.
//!
//! The assistant's content blocks are sent back unchanged on the next request, thinking
//! blocks included. The prefix of the conversation is cached (top-level `cache_control`), as
//! a harness would do in a long tool loop. Thinking is the models' default (adaptive); the
//! effort is set when given.

use serde_json::{Value, json};

use super::{
    AssistantTurn, Call, Conversation, ModelError, Reply, Stop, Turn, Usage, http_client, post_json,
};

const PROVIDER: &str = "Anthropic";
/// The API version the requests are written for.
pub const API_VERSION: &str = "2026-10-21";
/// Output tokens per request.
const MAX_TOKENS: u32 = 16_000;

#[derive(Debug)]
pub struct Anthropic {
    pub model: String,
    key: String,
    base_url: String,
    effort: Option<String>,
    client: reqwest::Client,
}

impl Anthropic {
    /// With `ANTHROPIC_API_KEY`, and `ANTHROPIC_BASE_URL` when set.
    pub fn from_env(model: &str, effort: Option<&str>) -> Result<Self, ModelError> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|key| !key.is_empty())
            .ok_or(ModelError::MissingKey("ANTHROPIC_API_KEY"))?;
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_owned());
        Self::new(model, key, base_url, effort)
    }

    pub fn new(
        model: &str,
        key: impl Into<String>,
        base_url: impl Into<String>,
        effort: Option<&str>,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            model: model.to_owned(),
            key: key.into(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            effort: effort.map(str::to_owned),
            client: http_client()?,
        })
    }

    /// The request's body.
    pub fn body(&self, conversation: &Conversation) -> Value {
        let tools: Vec<Value> = conversation
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
            })
            .collect();
        let messages: Vec<Value> = conversation.turns.iter().map(message).collect();
        let mut body = json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": [{ "type": "text", "text": conversation.system }],
            "tools": tools,
            "messages": messages,
            "cache_control": { "type": "ephemeral" },
        });
        if let Some(effort) = &self.effort {
            body["output_config"] = json!({ "effort": effort });
        }
        body
    }

    pub async fn reply(&self, conversation: &Conversation) -> Result<Reply, ModelError> {
        let body = self.body(conversation);
        let url = format!("{}/v1/messages", self.base_url);
        let response = post_json(
            PROVIDER,
            || {
                self.client
                    .post(&url)
                    .header("x-api-key", &self.key)
                    .header("anthropic-version", API_VERSION)
                    .json(&body)
            },
            6,
        )
        .await?;
        parse(&response)
    }
}

fn message(turn: &Turn) -> Value {
    match turn {
        Turn::User { text } => json!({ "role": "user", "content": text }),
        Turn::Assistant(turn) => {
            let content = match &turn.raw {
                Value::Array(blocks) if !blocks.is_empty() => Value::Array(blocks.clone()),
                _ => {
                    let mut blocks = Vec::new();
                    if !turn.text.is_empty() {
                        blocks.push(json!({ "type": "text", "text": turn.text }));
                    }
                    for call in &turn.calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": call.input,
                        }));
                    }
                    Value::Array(blocks)
                }
            };
            json!({ "role": "assistant", "content": content })
        }
        Turn::ToolResults { results } => {
            let blocks: Vec<Value> = results
                .iter()
                .map(|result| {
                    let mut block = json!({
                        "type": "tool_result",
                        "tool_use_id": result.call_id,
                        "content": result.content,
                    });
                    if result.is_error {
                        block["is_error"] = json!(true);
                    }
                    block
                })
                .collect();
            json!({ "role": "user", "content": blocks })
        }
    }
}

/// Reads a Messages API response.
pub fn parse(response: &Value) -> Result<Reply, ModelError> {
    let unexpected = |message: &str| ModelError::Unexpected {
        provider: PROVIDER,
        message: format!("{message}: {}", truncated(response)),
    };
    let content = response["content"]
        .as_array()
        .ok_or_else(|| unexpected("no content"))?;
    let mut text = Vec::new();
    let mut calls = Vec::new();
    for block in content {
        match block["type"].as_str() {
            Some("text") => text.push(block["text"].as_str().unwrap_or_default().to_owned()),
            Some("tool_use") => calls.push(Call {
                id: block["id"].as_str().unwrap_or_default().to_owned(),
                name: block["name"].as_str().unwrap_or_default().to_owned(),
                input: block["input"].clone(),
            }),
            _ => {}
        }
    }
    let usage = &response["usage"];
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let stop = match response["stop_reason"].as_str() {
        Some("tool_use") => Stop::ToolUse,
        Some("end_turn" | "stop_sequence") => Stop::EndTurn,
        Some("max_tokens") => Stop::MaxTokens,
        Some("refusal") => Stop::Refusal,
        Some(other) => Stop::Other(other.to_owned()),
        None => return Err(unexpected("no stop_reason")),
    };
    Ok(Reply {
        turn: AssistantTurn {
            text: text.join("\n"),
            calls,
            raw: Value::Array(content.clone()),
        },
        usage: Usage {
            input: count("input_tokens"),
            output: count("output_tokens"),
            cache_read: count("cache_read_input_tokens"),
            cache_write: count("cache_creation_input_tokens"),
        },
        stop,
    })
}

fn truncated(value: &Value) -> String {
    value.to_string().chars().take(500).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolResult;
    use rewind_agent_tools::definitions;

    #[test]
    fn a_tool_loop_is_written_as_the_api_takes_it() {
        let model = Anthropic::new("claude-opus-5-5", "k", "https://x", Some("high")).unwrap();
        let thinking = json!([
            { "type": "thinking", "thinking": "", "signature": "sig" },
            { "type": "tool_use", "id": "toolu_1", "name": "list_runs", "input": {} }
        ]);
        let conversation = Conversation {
            system: "system".into(),
            tools: vec![definitions::definition("list_runs").unwrap()],
            turns: vec![
                Turn::User { text: "go".into() },
                Turn::Assistant(AssistantTurn {
                    text: String::new(),
                    calls: vec![],
                    raw: thinking.clone(),
                }),
                Turn::ToolResults {
                    results: vec![ToolResult {
                        call_id: "toolu_1".into(),
                        content: "No runs.".into(),
                        is_error: true,
                    }],
                },
            ],
        };
        let body = model.body(&conversation);
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["tools"][0]["name"], "list_runs");
        assert_eq!(body["messages"][1]["content"], thinking);
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][2]["content"][0]["is_error"], true);
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn responses_give_text_calls_and_tokens() {
        let reply = parse(&json!({
            "content": [
                { "type": "text", "text": "Checking." },
                { "type": "tool_use", "id": "toolu_9", "name": "run_workflow", "input": { "code": "x" } }
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 7 }
        }))
        .unwrap();
        assert_eq!(reply.stop, Stop::ToolUse);
        assert_eq!(reply.turn.text, "Checking.");
        assert_eq!(reply.turn.calls[0].input["code"], "x");
        assert_eq!(reply.usage.total_input(), 117);
        assert!(parse(&json!({ "error": {} })).is_err());
    }
}
