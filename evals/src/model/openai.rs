//! An OpenAI model through the Responses API (`POST /v1/responses`), over HTTP.
//!
//! Requests are stateless (`store: false`): the output items of each answer are sent back as
//! input on the next request, reasoning items with their encrypted content, as OpenAI
//! documents for multi-turn tool use without stored responses.

use serde_json::{Value, json};

use super::{
    AssistantTurn, Call, Conversation, ModelError, Reply, Stop, Turn, Usage, http_client, post_json,
};

const PROVIDER: &str = "OpenAI";
/// Output tokens per request, reasoning included.
const MAX_OUTPUT_TOKENS: u32 = 32_000;

#[derive(Debug)]
pub struct OpenAi {
    pub model: String,
    key: String,
    base_url: String,
    effort: Option<String>,
    client: reqwest::Client,
}

impl OpenAi {
    /// With `OPENAI_API_KEY`, and `OPENAI_BASE_URL` when set.
    pub fn from_env(model: &str, effort: Option<&str>) -> Result<Self, ModelError> {
        let key = std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|key| !key.is_empty())
            .ok_or(ModelError::MissingKey("OPENAI_API_KEY"))?;
        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com".to_owned());
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
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": false,
                })
            })
            .collect();
        let mut input = Vec::new();
        for turn in &conversation.turns {
            items(turn, &mut input);
        }
        let mut body = json!({
            "model": self.model,
            "instructions": conversation.system,
            "input": input,
            "tools": tools,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "max_output_tokens": MAX_OUTPUT_TOKENS,
        });
        if let Some(effort) = &self.effort {
            body["reasoning"] = json!({ "effort": effort });
        }
        body
    }

    pub async fn reply(&self, conversation: &Conversation) -> Result<Reply, ModelError> {
        let body = self.body(conversation);
        let url = format!("{}/v1/responses", self.base_url);
        let response = post_json(
            PROVIDER,
            || self.client.post(&url).bearer_auth(&self.key).json(&body),
            6,
        )
        .await?;
        parse(&response)
    }
}

fn items(turn: &Turn, input: &mut Vec<Value>) {
    match turn {
        Turn::User { text } => input.push(json!({ "role": "user", "content": text })),
        Turn::Assistant(turn) => match &turn.raw {
            Value::Array(raw) if !raw.is_empty() => {
                for item in raw {
                    let mut item = item.clone();
                    // Messages and calls are input items without their ids: nothing is stored.
                    if matches!(item["type"].as_str(), Some("message" | "function_call"))
                        && let Some(fields) = item.as_object_mut()
                    {
                        fields.remove("id");
                        fields.remove("status");
                    }
                    input.push(item);
                }
            }
            _ => {
                if !turn.text.is_empty() {
                    input.push(json!({ "role": "assistant", "content": turn.text }));
                }
                for call in &turn.calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.input.to_string(),
                    }));
                }
            }
        },
        Turn::ToolResults { results } => {
            for result in results {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": result.call_id,
                    "output": result.content,
                }));
            }
        }
    }
}

/// Reads a Responses API response.
pub fn parse(response: &Value) -> Result<Reply, ModelError> {
    let unexpected = |message: &str| ModelError::Unexpected {
        provider: PROVIDER,
        message: format!(
            "{message}: {}",
            response.to_string().chars().take(500).collect::<String>()
        ),
    };
    let output = response["output"]
        .as_array()
        .ok_or_else(|| unexpected("no output"))?;
    let mut text = Vec::new();
    let mut calls = Vec::new();
    let mut refused = false;
    for item in output {
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"].as_array().into_iter().flatten() {
                    match part["type"].as_str() {
                        Some("output_text") => {
                            text.push(part["text"].as_str().unwrap_or_default().to_owned());
                        }
                        Some("refusal") => refused = true,
                        _ => {}
                    }
                }
            }
            Some("function_call") => {
                let arguments = item["arguments"].as_str().unwrap_or("{}");
                calls.push(Call {
                    id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                    name: item["name"].as_str().unwrap_or_default().to_owned(),
                    // Invalid JSON is left as text: the agent tool says so to the model.
                    input: serde_json::from_str(arguments)
                        .unwrap_or_else(|_| Value::String(arguments.to_owned())),
                });
            }
            _ => {}
        }
    }
    let usage = &response["usage"];
    let input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
    let cached = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    let stop = match response["status"].as_str() {
        Some("incomplete") => match response["incomplete_details"]["reason"].as_str() {
            Some("max_output_tokens") => Stop::MaxTokens,
            Some("content_filter") => Stop::Refusal,
            other => Stop::Other(other.unwrap_or("incomplete").to_owned()),
        },
        _ if refused => Stop::Refusal,
        _ if !calls.is_empty() => Stop::ToolUse,
        Some("completed") => Stop::EndTurn,
        Some(other) => Stop::Other(other.to_owned()),
        None => return Err(unexpected("no status")),
    };
    Ok(Reply {
        turn: AssistantTurn {
            text: text.join("\n"),
            calls,
            raw: Value::Array(output.clone()),
        },
        usage: Usage {
            input: input_tokens.saturating_sub(cached),
            output: usage["output_tokens"].as_u64().unwrap_or(0),
            cache_read: cached,
            cache_write: 0,
        },
        stop,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolResult;
    use rewind_agent_tools::definitions;

    #[test]
    fn output_items_go_back_as_input() {
        let model = OpenAi::new("gpt-6-astra", "k", "https://x", Some("high")).unwrap();
        let raw = json!([
            { "type": "reasoning", "id": "rs_1", "encrypted_content": "abc", "summary": [] },
            { "type": "function_call", "id": "fc_1", "status": "completed", "call_id": "call_1", "name": "list_runs", "arguments": "{}" }
        ]);
        let conversation = Conversation {
            system: "system".into(),
            tools: vec![definitions::definition("list_runs").unwrap()],
            turns: vec![
                Turn::User { text: "go".into() },
                Turn::Assistant(AssistantTurn {
                    text: String::new(),
                    calls: vec![],
                    raw,
                }),
                Turn::ToolResults {
                    results: vec![ToolResult {
                        call_id: "call_1".into(),
                        content: "No runs.".into(),
                        is_error: false,
                    }],
                },
            ],
        };
        let body = model.body(&conversation);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["store"], false);
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[1]["id"], "rs_1");
        assert!(input[2].get("id").is_none());
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["output"], "No runs.");
    }

    #[test]
    fn responses_give_text_calls_and_tokens() {
        let reply = parse(&json!({
            "status": "completed",
            "output": [
                { "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Done." }] },
                { "type": "function_call", "call_id": "call_2", "name": "get_run", "arguments": "{\"run_id\":\"r\"}" }
            ],
            "usage": { "input_tokens": 120, "output_tokens": 30, "input_tokens_details": { "cached_tokens": 100 } }
        }))
        .unwrap();
        assert_eq!(reply.stop, Stop::ToolUse);
        assert_eq!(reply.turn.calls[0].input["run_id"], "r");
        assert_eq!(reply.usage.input, 20);
        assert_eq!(reply.usage.cache_read, 100);
        assert_eq!(reply.turn.text, "Done.");
    }
}
