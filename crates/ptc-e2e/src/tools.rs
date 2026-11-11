//! How the tools of a scenario answer.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use ptc_core::{JsonValue, ToolFailure, ToolFailureKind};
use ptc_engine::ports::{Credentials, Dispatched, ToolCall, ToolDispatcher};
use serde_json::Value;

/// Answers tool calls from the `tools` of a scenario. A superset of `ptc.tools.json`:
///
/// | Response | Answer |
/// | --- | --- |
/// | any value | that value |
/// | `{"$echo": true}` | the call's arguments |
/// | `{"$error": "message", "$kind": "timeout", "$data": …}` | a failure (`$kind` defaults to `error`) |
/// | `{"$sequence": [r1, r2, …]}` | `r1` to the first call, `r2` to the second… |
#[derive(Debug)]
pub struct ScenarioTools {
    responses: Mutex<BTreeMap<String, Responses>>,
}

#[derive(Debug)]
enum Responses {
    Always(Value),
    Sequence(VecDeque<Value>),
}

impl ScenarioTools {
    pub fn new(tools: &BTreeMap<String, Value>) -> Self {
        let responses = tools
            .iter()
            .map(|(tool, response)| {
                let responses = match response.get("$sequence") {
                    Some(Value::Array(sequence)) => {
                        Responses::Sequence(sequence.iter().cloned().collect())
                    }
                    _ => Responses::Always(response.clone()),
                };
                (tool.clone(), responses)
            })
            .collect();
        Self {
            responses: Mutex::new(responses),
        }
    }

    fn respond(&self, call: &ToolCall) -> Result<JsonValue, ToolFailure> {
        let mut responses = self
            .responses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let response = match responses.get_mut(&call.tool.name) {
            Some(Responses::Always(response)) => response.clone(),
            Some(Responses::Sequence(sequence)) => sequence.pop_front().ok_or_else(|| {
                failure(
                    ToolFailureKind::Unavailable,
                    format!("{} has no response left", call.tool.name),
                    None,
                )
            })?,
            None => {
                return Err(failure(
                    ToolFailureKind::Unavailable,
                    format!("{} has no response in the scenario", call.tool.name),
                    None,
                ));
            }
        };
        answer(&response, call)
    }
}

fn answer(response: &Value, call: &ToolCall) -> Result<JsonValue, ToolFailure> {
    if response.get("$echo") == Some(&Value::Bool(true)) {
        return Ok(call.args.clone());
    }
    if let Some(message) = response.get("$error") {
        let kind = match response.get("$kind").and_then(Value::as_str) {
            Some("unavailable") => ToolFailureKind::Unavailable,
            Some("timeout") => ToolFailureKind::Timeout,
            Some("outcome_unknown") => ToolFailureKind::OutcomeUnknown,
            _ => ToolFailureKind::Error,
        };
        let message = message
            .as_str()
            .map_or_else(|| message.to_string(), str::to_owned);
        return Err(failure(kind, message, response.get("$data").cloned()));
    }
    Ok(response.clone())
}

fn failure(kind: ToolFailureKind, message: String, data: Option<Value>) -> ToolFailure {
    ToolFailure {
        kind,
        message,
        data,
    }
}

impl ToolDispatcher for ScenarioTools {
    async fn dispatch(&self, call: ToolCall, _credentials: Option<Credentials>) -> Dispatched {
        match self.respond(&call) {
            Ok(result) => Dispatched::Result(result),
            Err(failure) => Dispatched::Failed(failure),
        }
    }
}
