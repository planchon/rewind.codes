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
    /// The action set, and how each action answers (see [`crate::actions::ScenarioActions`]). An
    /// action is a harness action (`group.action` puts it in a group), except `http` and `webhook`,
    /// the built-ins, and the actions of `mcp_servers`.
    #[serde(default)]
    pub actions: BTreeMap<String, Value>,
    /// MCP servers: an action `server.action` of one of them is an MCP action. Pinned without an
    /// output schema (unless `output_schemas` gives one), it answers with MCP content, as
    /// `rewind-mcp` returns it: `{"content": [blocks], "structuredContent"?: value}`.
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    /// The pinned output schema of some MCP actions, by name.
    #[serde(default)]
    pub output_schemas: BTreeMap<String, Value>,
    /// How the calls of some MCP servers authenticate, by server: `secret` (the attached secret
    /// as a bearer token) or `rewind` (signed with the tenant's key). None for the others.
    #[serde(default)]
    pub auth: BTreeMap<String, String>,
    /// The secrets the run attaches, by server: `org/<name>` or `ns/<name>`
    /// (`archi/04-tools.md`, "Secrets").
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
    /// The secrets that exist before the run starts, by reference (`ns/<name>` in the run's
    /// namespace, `org/<name>`), with their values.
    #[serde(default)]
    pub put_secrets: BTreeMap<String, String>,
    /// What the caller that starts the run may attach: `namespace` (the default: its
    /// namespace's secrets only) or `organization` (`secrets:use`).
    #[serde(default)]
    pub attach: Option<String>,
    /// The policies that differ from the default, by action.
    #[serde(default)]
    pub policies: BTreeMap<String, Policy>,
    /// The actions are harness actions answered by a harness's worker, handed their tasks on an SDK
    /// stream and answering through their tokens, instead of handlers called by the engine. A
    /// `$crash` response is a worker that dies with the call: its stream closes.
    #[serde(default)]
    pub harness_worker: bool,
    /// The run is live rather than in a test run: its actions still answer from `actions`, but the
    /// engine serves its built-ins itself, so the runs of `workflows` it starts really run.
    #[serde(default)]
    pub live: bool,
    /// Workflows the run's action set pins, by name. Each is registered with the scenario's actions
    /// as its action set, before the run starts (`archi/01-product.md`, "Workflows").
    #[serde(default)]
    pub workflows: BTreeMap<String, WorkflowCase>,
    /// The pinned input schema of some actions, by name. The other host and MCP actions accept any
    /// arguments; the built-ins get the schemas of `rewind_mcp::http_action` and
    /// `rewind_runtime::builtins`.
    #[serde(default)]
    pub schemas: BTreeMap<String, Value>,
    /// The hosts the built-in `http` action allows, when `actions` has it.
    #[serde(default)]
    pub http_hosts: Vec<String>,
    /// The order in which calls answer.
    #[serde(default)]
    pub action_order: ActionOrder,
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
    /// Forks the run once its steps are done: the expectations, the recorded history and the
    /// replays are then the fork's.
    #[serde(default)]
    pub fork: Option<Fork>,
    #[serde(default)]
    pub expect: Expect,
    /// A check of the recorded history against another code, or a tampered history.
    #[serde(default)]
    pub replay: Option<ReplayCheck>,
}

/// A workflow of a case.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCase {
    #[serde(default)]
    pub description: Option<String>,
    /// Its code, next to `scenario.json`.
    pub code: String,
    /// Its JSON Schemas. `{}` (anything) when absent.
    #[serde(default = "empty_object")]
    pub input: Value,
    #[serde(default = "empty_object")]
    pub output: Value,
    #[serde(default)]
    pub signals: BTreeMap<String, Value>,
    /// The secrets its runs attach, by server, saved with the version.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
}

/// A fork of the case's run (`archi/03-execution.md`, "Forks").
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fork {
    /// The corrected code, next to `scenario.json`.
    pub code: String,
    /// Go live from this event of the parent's history.
    #[serde(default)]
    pub at: Option<u64>,
    /// The fork's input; the parent's when absent.
    #[serde(default)]
    pub input: Option<Value>,
    /// What happens after the fork starts, as the case's steps.
    #[serde(default)]
    pub steps: Vec<Step>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

fn default_seed() -> u64 {
    1
}

/// An action's policy, where it differs from the default (`archi/04-tools.md`, "Policies").
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// The call waits for the harness's approval (an `approve` or `reject` step).
    #[serde(default)]
    pub approval: bool,
    /// Whether an interrupted call may be delivered again; `true` by default.
    #[serde(default)]
    pub idempotent: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOrder {
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
    /// The harness approves the call of this action that waits for it.
    Approve(String),
    /// The harness rejects the call of this action that waits for it.
    Reject {
        action: String,
        #[serde(default)]
        message: Option<String>,
    },
    /// The harness makes these MCP servers accept the run's calls again, the value by server (a
    /// map of one header, as the corpus wrote it when runs had credentials): it puts the secret
    /// the run attached for the server again, or approves the calls the server refused.
    Credentials(BTreeMap<String, BTreeMap<String, String>>),
    /// Deletes the secret the run attached for this server.
    DeleteSecret(String),
    /// The call of this action waits for the harness, parked for this reason
    /// (`credentials_refused`, `secret_missing`, `schema_changed`).
    Parked { action: String, reason: String },
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
    /// How many times each action was called.
    pub calls: Option<BTreeMap<String, usize>>,
    /// The names of the history's events, in order.
    pub events: Option<Vec<String>>,
    /// The signals the run is waiting for at the end.
    pub awaiting: Option<Vec<String>>,
    /// How many times each action really answered, across the case: a fork that reuses its
    /// parent's results calls the actions fewer times than its history schedules calls.
    pub dispatched: Option<BTreeMap<String, usize>>,
    /// The statuses of the runs of workflows the run started, in the order of its calls.
    pub children: Option<Vec<String>>,
    /// How each call the actions answered authenticated, in order, across the case: the value of
    /// the secret it carried, `signed`, or `none`.
    pub auth: Option<Vec<String>>,
    /// The idempotency keys of the calls the actions answered, in order, across the case; `{run}`
    /// stands for the recorded run's id.
    pub keys: Option<Vec<String>>,
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

/// Replays the recorded history with a change, and says how it must fail.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayCheck {
    /// Another code, next to `scenario.json`, replayed against the recorded history.
    pub code: Option<String>,
    /// Removes this event from the recorded history before replaying it.
    pub drop_event: Option<usize>,
    /// The replay diverges with exactly this message.
    pub diverges: Option<String>,
    /// The replay fails with exactly this message (an invalid history).
    pub error: Option<String>,
}
