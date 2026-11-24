//! How the actions of a scenario answer.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use rewind_core::{ActionFailure, ActionFailureKind, JsonValue};
use rewind_engine::ports::{ActionDispatcher, Call, CallAuth, Dispatched, ParkKind, ParkReason};
use serde_json::Value;

/// Answers calls from the `actions` of a scenario. A superset of `rewind.actions.json`:
///
/// | Response | Answer |
/// | --- | --- |
/// | any value | that value |
/// | `{"$echo": true}` | the call's arguments |
/// | `{"$error": "message", "$kind": "timeout", "$data": …}` | a failure (`$kind` defaults to `error`) |
/// | `{"$transient": "message", "$kind": …}` | a transient failure: retried with backoff under `retry` |
/// | `{"$unauthorized": "message"}` | the server answers 401: the call is parked (`credentials_refused`) until the secret is put again or the harness approves it |
/// | `{"$crash": true}` | the worker dies with the call (with `harness_worker` only): its stream closes, and another worker opens one |
/// | `{"$sequence": [r1, r2, …]}` | `r1` to the first call, `r2` to the second… |
/// | `{"$defer": true}` | no answer: the call stays without an outcome |
#[derive(Debug)]
pub struct ScenarioActions {
    responses: Mutex<BTreeMap<String, Responses>>,
    /// The calls dispatched, by action, and the idempotency keys they carried.
    dispatched: Mutex<Vec<(String, String)>>,
    /// How each call the dispatcher answered authenticated: the secret's value, `signed` or
    /// `none`.
    auth: Mutex<Vec<String>>,
}

#[derive(Debug)]
enum Responses {
    Always(Value),
    Sequence(VecDeque<Value>),
}

/// What a scenario's action does with one call.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Result(JsonValue),
    Failed(ActionFailure),
    Transient(ActionFailure),
    Unauthorized(String),
    Crash,
    Deferred,
}

impl ScenarioActions {
    pub fn new(actions: &BTreeMap<String, Value>) -> Self {
        let responses = actions
            .iter()
            .map(|(action, response)| {
                let responses = match response.get("$sequence") {
                    Some(Value::Array(sequence)) => {
                        Responses::Sequence(sequence.iter().cloned().collect())
                    }
                    _ => Responses::Always(response.clone()),
                };
                (action.clone(), responses)
            })
            .collect();
        Self {
            responses: Mutex::new(responses),
            dispatched: Mutex::new(Vec::new()),
            auth: Mutex::new(Vec::new()),
        }
    }

    /// How each call the dispatcher answered authenticated, in order: the secret's value,
    /// `signed`, or `none`.
    pub fn auth(&self) -> Vec<String> {
        self.auth
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many times each action was dispatched.
    pub fn dispatched(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for (action, _) in self
            .dispatched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
        {
            *counts.entry(action.clone()).or_default() += 1;
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

    /// The next answer of `action` to a call with these args, which carries this idempotency
    /// key. The call is counted as dispatched.
    pub fn answer(&self, action: &str, args: &JsonValue, key: &str) -> Answer {
        self.dispatched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((action.to_owned(), key.to_owned()));
        let mut responses = self
            .responses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let response = match responses.get_mut(action) {
            Some(Responses::Always(response)) => response.clone(),
            Some(Responses::Sequence(sequence)) => match sequence.pop_front() {
                Some(response) => response,
                None => {
                    return Answer::Failed(failure(
                        ActionFailureKind::Unavailable,
                        format!("{action} has no response left"),
                        None,
                    ));
                }
            },
            None => {
                return Answer::Failed(failure(
                    ActionFailureKind::Unavailable,
                    format!("{action} has no response in the scenario"),
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
        Some("unavailable") => ActionFailureKind::Unavailable,
        Some("timeout") => ActionFailureKind::Timeout,
        Some("outcome_unknown") => ActionFailureKind::OutcomeUnknown,
        _ => ActionFailureKind::Error,
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

fn failure(kind: ActionFailureKind, message: String, data: Option<Value>) -> ActionFailure {
    ActionFailure {
        kind,
        message,
        data,
    }
}

impl ActionDispatcher for ScenarioActions {
    async fn dispatch(&self, call: Call, auth: CallAuth) -> Dispatched {
        let carried = match &auth {
            CallAuth::None => "none".to_owned(),
            CallAuth::Secret { value, .. } => value.expose().to_owned(),
            CallAuth::Signed { .. } => "signed".to_owned(),
        };
        self.auth
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(carried);
        match self.answer(&call.action.name, &call.args, &call.idempotency_key) {
            Answer::Result(result) => Dispatched::Result(result),
            Answer::Deferred => Dispatched::Deferred,
            Answer::Failed(failure) => Dispatched::Failed(failure),
            Answer::Transient(failure) => Dispatched::Transient(failure),
            Answer::Unauthorized(message) => Dispatched::Park(ParkReason {
                kind: ParkKind::CredentialsRefused,
                message,
            }),
            Answer::Crash => Dispatched::Failed(failure(
                ActionFailureKind::Error,
                format!(
                    "{}: a $crash response needs a harness's worker (\"harness_worker\": true)",
                    call.action.name
                ),
                None,
            )),
        }
    }
}
