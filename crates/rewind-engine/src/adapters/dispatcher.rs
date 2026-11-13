use std::collections::BTreeMap;

use rewind_core::{JsonValue, ToolFailure, ToolFailureKind};

use crate::ports::{Credentials, Dispatched, ToolCall, ToolDispatcher};

/// Answers every call to a tool with the same response: the static responses of mock mode,
/// as the CLI reads them from `rewind.tools.json`.
///
/// In JSON, a tool maps to its result, or to `{"$error": "message"}` for a failure:
///
/// ```json
/// { "github.get_issue": { "number": 4, "title": "Crash" }, "slack.post": { "$error": "rate limited" } }
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StaticDispatcher {
    responses: BTreeMap<String, Result<JsonValue, ToolFailure>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StaticResponsesError {
    #[error("static responses must be a JSON object mapping tool names to responses")]
    NotAnObject,
    #[error("the \"$error\" of tool {0:?} must be a string")]
    InvalidError(String),
}

impl StaticDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Responses in the `rewind.tools.json` format.
    pub fn from_json(json: &JsonValue) -> Result<Self, StaticResponsesError> {
        let tools = json.as_object().ok_or(StaticResponsesError::NotAnObject)?;
        let mut dispatcher = Self::new();
        for (tool, response) in tools {
            match response.get("$error") {
                Some(JsonValue::String(message)) => dispatcher.insert_error(tool, message),
                Some(_) => return Err(StaticResponsesError::InvalidError(tool.clone())),
                None => dispatcher.insert(tool, response.clone()),
            }
        }
        Ok(dispatcher)
    }

    pub fn insert(&mut self, tool: impl Into<String>, result: JsonValue) {
        self.responses.insert(tool.into(), Ok(result));
    }

    pub fn insert_error(&mut self, tool: impl Into<String>, message: impl Into<String>) {
        self.responses.insert(
            tool.into(),
            Err(ToolFailure {
                kind: ToolFailureKind::Error,
                message: message.into(),
                data: None,
            }),
        );
    }

    fn respond(&self, tool: &str) -> Result<JsonValue, ToolFailure> {
        self.responses.get(tool).cloned().unwrap_or_else(|| {
            Err(ToolFailure {
                kind: ToolFailureKind::Unavailable,
                message: format!("no static response for {tool}"),
                data: None,
            })
        })
    }
}

impl ToolDispatcher for StaticDispatcher {
    /// The tool's response, or its failure. A tool without a response fails with
    /// `unavailable`, for good: retrying would not make one appear.
    async fn dispatch(&self, call: ToolCall, _credentials: Option<Credentials>) -> Dispatched {
        match self.respond(&call.tool.name) {
            Ok(result) => Dispatched::Result(result),
            Err(failure) => Dispatched::Failed(failure),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_the_static_responses_format() {
        let dispatcher = StaticDispatcher::from_json(&json!({
            "github.get_issue": { "number": 4 },
            "slack.post": { "$error": "rate limited" },
        }))
        .unwrap();
        assert_eq!(
            dispatcher.respond("github.get_issue"),
            Ok(json!({ "number": 4 }))
        );
        assert_eq!(
            dispatcher.respond("slack.post").unwrap_err().message,
            "rate limited"
        );
        assert_eq!(
            dispatcher.respond("other").unwrap_err().kind,
            ToolFailureKind::Unavailable
        );
        assert_eq!(
            StaticDispatcher::from_json(&json!([])),
            Err(StaticResponsesError::NotAnObject)
        );
        assert_eq!(
            StaticDispatcher::from_json(&json!({ "t": { "$error": 1 } })),
            Err(StaticResponsesError::InvalidError("t".into()))
        );
    }
}
