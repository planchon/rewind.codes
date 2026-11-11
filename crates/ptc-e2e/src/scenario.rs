//! `scenario.json`: what a corpus case runs, and what must happen.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// One case of the corpus. Every field but `description` is optional.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// What the case shows, in one or two sentences.
    pub description: String,
    /// The run's input.
    #[serde(default = "empty_object")]
    pub input: Value,
    /// The toolset, and how each tool answers (see [`crate::tools::ScenarioTools`]).
    #[serde(default)]
    pub tools: BTreeMap<String, Value>,
    /// The order in which tool calls answer.
    #[serde(default)]
    pub tool_order: ToolOrder,
    /// Base of the deterministic entropy: the run's id and seed derive from it.
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// The run's TTL, such as `"1h"`. The engine's default (30 days) otherwise.
    #[serde(default)]
    pub ttl: Option<String>,
    #[serde(default)]
    pub limits: Limits,
    /// What happens after the run starts, in order. Each step is followed by a pass of the
    /// engine at the current time; a last pass in virtual time follows the last step.
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub expect: Expect,
    /// A check of the recorded journal against another script, or a tampered journal.
    #[serde(default)]
    pub replay: Option<ReplayCheck>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn default_seed() -> u64 {
    1
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOrder {
    #[default]
    Scheduled,
    Reversed,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub cpu_per_trigger_ms: Option<u64>,
    pub heap_mb: Option<usize>,
    pub max_commands: Option<usize>,
    pub max_payload_bytes: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// Sends a signal.
    Signal {
        name: String,
        #[serde(default)]
        payload: Value,
        #[serde(default)]
        key: Option<String>,
    },
    /// Asks the run to cancel.
    Cancel {
        #[serde(default)]
        message: Option<String>,
    },
    /// Moves the virtual clock forward, such as `"2h"`.
    Advance(String),
}

/// What must be true once the run is driven. Everything not given is not checked.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The start is refused, with a message containing this text: no run exists.
    pub start_error: Option<String>,
    /// `pending`, `active`, `completed`, `failed`, `cancelled` or `aborted`.
    pub status: Option<String>,
    /// The output contains these values (objects are compared key by key, recursively).
    pub output: Option<Value>,
    /// The run failed with this error.
    pub error: Option<ExpectedError>,
    /// The host aborted the run.
    pub abort: Option<ExpectedAbort>,
    /// How many times each tool was called.
    pub calls: Option<BTreeMap<String, usize>>,
    /// The names of the journal's events, in order.
    pub events: Option<Vec<String>>,
    /// The signals the run is waiting for at the end.
    pub awaiting: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedError {
    pub name: String,
    /// The message contains this text.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedAbort {
    pub kind: String,
    /// The message contains this text.
    pub message: String,
}

/// Replays the recorded journal with a change, and says how it must fail.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayCheck {
    /// Another script, next to `scenario.json`, replayed against the recorded journal.
    pub script: Option<String>,
    /// Removes this entry from the recorded journal before replaying it.
    pub drop_entry: Option<usize>,
    /// The replay diverges with exactly this message.
    pub diverges: Option<String>,
    /// The replay fails with exactly this message (an invalid history).
    pub error: Option<String>,
}
