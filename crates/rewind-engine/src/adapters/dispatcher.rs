use std::collections::BTreeMap;

use rewind_core::{ActionFailure, ActionFailureKind, JsonValue};

use crate::ports::{ActionDispatcher, Call, CallAuth, Dispatched};

/// Answers every call to an action with the same response: the static responses of mock mode,
/// as the CLI reads them from `rewind.actions.json`.
///
/// In JSON, an action maps to its result, or to `{"$error": "message"}` for a failure:
///
/// ```json
/// { "github.get_issue": { "number": 4, "title": "Crash" }, "slack.post": { "$error": "rate limited" } }
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StaticDispatcher {
    responses: BTreeMap<String, Result<JsonValue, ActionFailure>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StaticResponsesError {
    #[error("static responses must be a JSON object mapping action names to responses")]
    NotAnObject,
    #[error("the \"$error\" of action {0:?} must be a string")]
    InvalidError(String),
}

impl StaticDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Responses in the `rewind.actions.json` format.
    pub fn from_json(json: &JsonValue) -> Result<Self, StaticResponsesError> {
        let actions = json.as_object().ok_or(StaticResponsesError::NotAnObject)?;
        let mut dispatcher = Self::new();
        for (action, response) in actions {
            match response.get("$error") {
                Some(JsonValue::String(message)) => dispatcher.insert_error(action, message),
                Some(_) => return Err(StaticResponsesError::InvalidError(action.clone())),
                None => dispatcher.insert(action, response.clone()),
            }
        }
        Ok(dispatcher)
    }

    pub fn insert(&mut self, action: impl Into<String>, result: JsonValue) {
        self.responses.insert(action.into(), Ok(result));
    }

    pub fn insert_error(&mut self, action: impl Into<String>, message: impl Into<String>) {
        self.responses.insert(
            action.into(),
            Err(ActionFailure {
                kind: ActionFailureKind::Error,
                message: message.into(),
                data: None,
            }),
        );
    }

    fn respond(&self, action: &str) -> Result<JsonValue, ActionFailure> {
        self.responses.get(action).cloned().unwrap_or_else(|| {
            Err(ActionFailure {
                kind: ActionFailureKind::Unavailable,
                message: format!("no static response for {action}"),
                data: None,
            })
        })
    }
}

impl ActionDispatcher for StaticDispatcher {
    /// The action's response, or its failure. An action without a response fails with
    /// `unavailable`, for good: retrying would not make one appear.
    async fn dispatch(&self, call: Call, _auth: CallAuth) -> Dispatched {
        match self.respond(&call.action.name) {
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
            ActionFailureKind::Unavailable
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
