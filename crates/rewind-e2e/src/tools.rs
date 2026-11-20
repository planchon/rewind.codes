//! How the tools of a scenario answer.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use rewind_core::{JsonValue, ToolFailure, ToolFailureKind};
use rewind_engine::ports::{CallAuth, Dispatched, ParkKind, ParkReason, ToolCall, ToolDispatcher};
use serde_json::Value;

/// Answers tool calls from the `tools` of a scenario. A superset of `rewind.tools.json`:
///
/// | Response | Answer |
/// | --- | --- |
/// | any value | that value |
/// | `{"$echo": true}` | the call's arguments |
/// | `{"$error": "message", "$kind": "timeout", "$data": …}` | a failure (`$kind` defaults to `error`) |
/// | `{"$transient": "message", "$kind": …}` | a transient failure: retried with backoff under `retry` |
/// | `{"$unauthorized": "message"}` | the server answers 401: the call is parked (`credentials_refused`) until the secret is put again or the harness approves it |
/// | `{"$crash": true}` | the worker dies with the call (with `host_worker` only): its claim expires |
/// | `{"$sequence": [r1, r2, …]}` | `r1` to the first call, `r2` to the second… |
/// | `{"$defer": true}` | no answer: the call stays without an outcome |
#[derive(Debug)]
pub struct ScenarioTools {
    responses: Mutex<BTreeMap<String, Responses>>,
    /// The calls dispatched, by tool, and the idempotency keys they carried.
    dispatched: Mutex<Vec<(String, String)>>,
}

#[derive(Debug)]
enum Responses {
    Always(Value),
    Sequence(VecDeque<Value>),
}

/// What a scenario's tool does with one call.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Result(JsonValue),
    Failed(ToolFailure),
    Transient(ToolFailure),
    Unauthorized(String),
    Crash,
    Deferred,
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
            dispatched: Mutex::new(Vec::new()),
        }
    }

    /// How many times each tool was dispatched.
    pub fn dispatched(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for (tool, _) in self
            .dispatched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            *counts.entry(tool.clone()).or_default() += 1;
        }
        counts
    }

    /// The idempotency keys of the calls dispatched, in order.
    pub fn keys(&self) -> Vec<String> {
        self.dispatched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, key)| key.clone())
            .collect()
    }

    /// The next answer of `tool` to a call with these args, which carries this idempotency
    /// key. The call is counted as dispatched.
    pub fn answer(&self, tool: &str, args: &JsonValue, key: &str) -> Answer {
        self.dispatched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((tool.to_owned(), key.to_owned()));
        let mut responses = self
            .responses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let response = match responses.get_mut(tool) {
            Some(Responses::Always(response)) => response.clone(),
            Some(Responses::Sequence(sequence)) => match sequence.pop_front() {
                Some(response) => response,
                None => {
                    return Answer::Failed(failure(
                        ToolFailureKind::Unavailable,
                        format!("{tool} has no response left"),
                        None,
                    ));
                }
            },
            None => {
                return Answer::Failed(failure(
                    ToolFailureKind::Unavailable,
                    format!("{tool} has no response in the scenario"),
                    None,
                ));
            }
        };
        answer(&response, args)
    }
}

fn answer(response: &Value, args: &JsonValue) -> Answer {
    let text = |value: &Value| {
        value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned)
    };
    let kind = match response.get("$kind").and_then(Value::as_str) {
        Some("unavailable") => ToolFailureKind::Unavailable,
        Some("timeout") => ToolFailureKind::Timeout,
        Some("outcome_unknown") => ToolFailureKind::OutcomeUnknown,
        _ => ToolFailureKind::Error,
    };
    let data = response.get("$data").cloned();
    if response.get("$defer") == Some(&Value::Bool(true)) {
        Answer::Deferred
    } else if response.get("$echo") == Some(&Value::Bool(true)) {
        Answer::Result(args.clone())
    } else if response.get("$crash") == Some(&Value::Bool(true)) {
        Answer::Crash
    } else if let Some(message) = response.get("$error") {
        Answer::Failed(failure(kind, text(message), data))
    } else if let Some(message) = response.get("$transient") {
        Answer::Transient(failure(kind, text(message), data))
    } else if let Some(message) = response.get("$unauthorized") {
        Answer::Unauthorized(text(message))
    } else {
        Answer::Result(response.clone())
    }
}

fn failure(kind: ToolFailureKind, message: String, data: Option<Value>) -> ToolFailure {
    ToolFailure {
        kind,
        message,
        data,
    }
}

impl ToolDispatcher for ScenarioTools {
    async fn dispatch(&self, call: ToolCall, _auth: CallAuth) -> Dispatched {
        match self.answer(&call.tool.name, &call.args, &call.idempotency_key) {
            Answer::Result(result) => Dispatched::Result(result),
            Answer::Deferred => Dispatched::Deferred,
            Answer::Failed(failure) => Dispatched::Failed(failure),
            Answer::Transient(failure) => Dispatched::Transient(failure),
            Answer::Unauthorized(message) => Dispatched::Park(ParkReason {
                kind: ParkKind::CredentialsRefused,
                message,
            }),
            Answer::Crash => Dispatched::Failed(failure(
                ToolFailureKind::Error,
                format!(
                    "{}: a $crash response needs a harness's worker (\"host_worker\": true)",
                    call.tool.name
                ),
                None,
            )),
        }
    }
}
