//! The models under evaluation, behind one interface: Claude through the Anthropic Messages
//! API, an OpenAI model through the Responses API, and a scripted model that replays given
//! turns, so the whole harness runs in tests without keys.

pub mod anthropic;
pub mod openai;
pub mod scripted;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use rewind_agent_tools::definitions::ToolDefinition;

/// A conversation with a model: the system prompt, the agent tools, and the turns so far.
#[derive(Debug, Clone)]
pub struct Conversation {
    pub system: String,
    pub tools: Vec<ToolDefinition>,
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Turn {
    User {
        text: String,
    },
    Assistant(AssistantTurn),
    /// The results of the tool calls of the assistant turn before.
    ToolResults {
        results: Vec<ToolResult>,
    },
}

/// What a model said: its text, its tool calls, and its answer as its API gave it, sent back
/// unchanged on the next request (thinking and reasoning items included).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AssistantTurn {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub calls: Vec<Call>,
    /// The provider's raw content (`anthropic` content blocks, `openai` output items).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

/// Tokens of one request, or of an episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens not read from the cache.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    /// Every input token, cached or not.
    pub fn total_input(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

/// Why a model stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    /// It called tools.
    ToolUse,
    /// It answered.
    EndTurn,
    /// It reached its output limit.
    MaxTokens,
    /// It declined the request.
    Refusal,
    Other(String),
}

/// A model's answer to a conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub turn: AssistantTurn,
    pub usage: Usage,
    pub stop: Stop,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("{0} is not set")]
    MissingKey(&'static str),
    #[error("unknown model {0:?}: expected claude-…, gpt-… or scripted:<file>")]
    Unknown(String),
    #[error("the request to {provider} failed: {message}")]
    Http {
        provider: &'static str,
        message: String,
    },
    #[error("{provider} answered {status}: {body}")]
    Status {
        provider: &'static str,
        status: u16,
        body: String,
    },
    #[error("{provider} answered something unexpected: {message}")]
    Unexpected {
        provider: &'static str,
        message: String,
    },
    #[error("the scripted model has no turn {0}")]
    ScriptEnded(usize),
    #[error("the scripted turns cannot be read: {0}")]
    Code(String),
}

/// A model under evaluation.
#[derive(Debug)]
pub enum Model {
    Anthropic(anthropic::Anthropic),
    OpenAi(openai::OpenAi),
    Scripted(scripted::Scripted),
}

/// The model of the reports: `claude-opus-5-5`, `gpt-6-astra`, `scripted:file.json`.
impl Model {
    /// A model by name: `claude-…` through the Anthropic API (`ANTHROPIC_API_KEY`), `gpt-…`,
    /// `o…` through the OpenAI API (`OPENAI_API_KEY`), `scripted:<file.json>` from turns in a
    /// file. `effort` is the reasoning effort, when the model takes one.
    pub fn from_name(name: &str, effort: Option<&str>) -> Result<Self, ModelError> {
        if let Some(path) = name.strip_prefix("scripted:") {
            return Ok(Model::Scripted(scripted::Scripted::from_file(path)?));
        }
        if name.starts_with("claude-") {
            return Ok(Model::Anthropic(anthropic::Anthropic::from_env(
                name, effort,
            )?));
        }
        if name.starts_with("gpt-") || name.starts_with('o') {
            return Ok(Model::OpenAi(openai::OpenAi::from_env(name, effort)?));
        }
        Err(ModelError::Unknown(name.to_owned()))
    }

    pub fn name(&self) -> &str {
        match self {
            Model::Anthropic(model) => &model.model,
            Model::OpenAi(model) => &model.model,
            Model::Scripted(model) => &model.name,
        }
    }

    pub async fn reply(&self, conversation: &Conversation) -> Result<Reply, ModelError> {
        match self {
            Model::Anthropic(model) => model.reply(conversation).await,
            Model::OpenAi(model) => model.reply(conversation).await,
            Model::Scripted(model) => model.reply(conversation),
        }
    }
}

/// The models evaluated by default (`archi/08-decisions.md`): Claude Opus, Sonnet and Haiku,
/// and one OpenAI model.
pub const DEFAULT_MODELS: [&str; 4] = [
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "gpt-6-astra",
];

/// Sends a JSON request, again after a network error, a 429 or a 5xx (529 included), with
/// exponential backoff or the server's `retry-after`, up to `attempts` times.
pub(crate) async fn post_json(
    provider: &'static str,
    request: impl Fn() -> reqwest::RequestBuilder,
    attempts: u32,
) -> Result<Value, ModelError> {
    let mut delay = Duration::from_secs(2);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let sent = request().send().await;
        let retry_after = match sent {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return response
                        .json::<Value>()
                        .await
                        .map_err(|err| ModelError::Http {
                            provider,
                            message: err.to_string(),
                        });
                }
                let retryable = status.as_u16() == 429 || status.is_server_error();
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs);
                let body = response.text().await.unwrap_or_default();
                if !retryable || attempt >= attempts {
                    return Err(ModelError::Status {
                        provider,
                        status: status.as_u16(),
                        body: body.chars().take(2_000).collect(),
                    });
                }
                retry_after
            }
            Err(err) => {
                if attempt >= attempts {
                    return Err(ModelError::Http {
                        provider,
                        message: err.to_string(),
                    });
                }
                None
            }
        };
        tokio::time::sleep(retry_after.unwrap_or(delay).min(Duration::from_secs(120))).await;
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}

/// The HTTP client of the model APIs: requests may take minutes.
pub(crate) fn http_client() -> Result<reqwest::Client, ModelError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|err| ModelError::Http {
            provider: "the HTTP client",
            message: err.to_string(),
        })
}
