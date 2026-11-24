//! The names stored before the rename release, read once by `rewind server rename-data`
//! (`archi/06-storage-and-scale.md`, "Rewriting the stored names"; `archi/08-decisions.md`, R2
//! and R6).
//!
//! Each function takes a document or a record as it was written before the release, renames
//! what the release renamed, and hands back the new form, decoded by this crate's readers: what
//! cannot be read in the new names after the renaming is refused, never written. A document
//! already in the new names comes back as it is, so the command can run again and change
//! nothing.
//!
//! A content-addressed document whose bytes change (a pinned action set, a workflow version, a
//! test) gets a new digest, which its caller computes from what comes back; every reference to
//! another such document goes through [`Renames::digest`], which knows the new digests of those
//! rewritten before. What a renaming needs beyond the record itself (the workflow of each run,
//! the workflow of each code a namespace ran, whether a run has a trigger in its inbox) comes from
//! [`Renames`] too.
//!
//! | Old | New |
//! | --- | --- |
//! | `toolset`, `tools`, `tool`, `script` | `action_set`, `actions`, `action`, `code` |
//! | `schedule_tool_call`, `tool_call_completed`, `tool_call_failed` | `schedule_call`, `call_completed`, `call_failed` |
//! | route `host`, built-in `webhook` | route `harness`, the action `signalUrl` routed to the built-in `signal_url` |
//! | `on_interruption: retry` / `at_most_once`, `heartbeat_timeout_ms` | `idempotent: true` / `false`, `reconnect_timeout_ms` |
//! | a task's `attempt`, `next_attempt_at`; `retries`; the timer `retry_backoff` | `delivery`, `next_delivery_at`; `redeliveries`; `redelivery_backoff` |
//! | `entries`, `parent` (a fork's run, a version's base), `scope`, a head's `attempts` | `events`, `source` / `base`, `allowed_labels`, `unfinished` |
//! | mode `mock`, `world.mocks`, `origin`, `updated[{from, to}]` | `test`, `world.handlers`, `from`, `replaced[{old, new}]` |
//! | statuses `pending`, `active`; a test's `when: "blocked"` | `queued`, `running` or `waiting`; `"waiting"` |
//! | the outbox's `event`, `run_suspended`, `credentials_expired`, `tool_schema_changed` | `webhook`, `run_waiting`, `secret_expired`, `action_schema_changed` |
//! | an expectation's `ToolError`, `WorkflowError` | `ActionError`, `RunError` |
//! | counters and gauges per code (`scripts`) and per workflow name | per workflow id |

use std::collections::BTreeMap;

use rewind_core::{ActionSet, Digest, RunExport, RunId, Test, WorkflowVersion};
use serde_json::{Map, Value};

use crate::FormatError;

/// What a renaming needs beyond the record it renames.
pub trait Renames {
    /// The new digest of a content-addressed document (a pinned action set, a workflow version,
    /// a test) rewritten before, by its old digest; `None`: the digest is unchanged.
    fn digest(&self, old: &Digest) -> Option<Digest>;

    /// The workflow a pinned version belongs to, by the version's digest (old or new), for an
    /// action set of `tenant` that pins it under `name`.
    fn pinned(&self, tenant: &str, name: &str, version: &Digest) -> Option<PinnedVersion>;

    /// A run's workflow, as its head and summary now hold it (`{id, name?, version, digest,
    /// interface}`); `None`: the run has none, as a test run.
    fn run_workflow(&self, run: &RunId) -> Option<Value>;

    /// The workflow of a code that a namespace ran: what keys the counters.
    fn code_workflow(&self, tenant: &str, namespace: &str, code: &Digest) -> Option<String>;

    /// The run has a trigger in its inbox: a step of it is in progress, so it is `running`.
    fn stepping(&self, run: &RunId) -> bool;
}

/// A pinned workflow version, as an action set pins it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedVersion {
    pub id: String,
    pub version: u64,
    pub digest: Digest,
}

/// No renaming context: every digest kept, no workflow known. What a reader that only needs the
/// names (the e2e corpus's histories recorded before the release) uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoContext;

impl Renames for NoContext {
    fn digest(&self, _: &Digest) -> Option<Digest> {
        None
    }

    fn pinned(&self, _: &str, _: &str, _: &Digest) -> Option<PinnedVersion> {
        None
    }

    fn run_workflow(&self, _: &RunId) -> Option<Value> {
        None
    }

    fn code_workflow(&self, _: &str, _: &str, _: &Digest) -> Option<String> {
        None
    }

    fn stepping(&self, _: &RunId) -> bool {
        false
    }
}

// -------------------------------------------------------------------------------------------
// Documents

/// A pinned action set of `tenant`, renamed and read.
pub fn action_set<R: Renames>(
    json: &str,
    tenant: &str,
    renames: &R,
) -> Result<ActionSet, FormatError> {
    let mut value = parse(json)?;
    action_set_value(&mut value, tenant, renames);
    crate::decode_action_set(&value.to_string())
}

/// A workflow version, renamed and read: its workflow's name and description leave it, and the
/// digests it names (its action set, its base, its tests) are the new ones.
pub fn workflow_version<R: Renames>(
    json: &str,
    renames: &R,
) -> Result<WorkflowVersion, FormatError> {
    let mut value = parse(json)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("name");
        object.remove("description");
        rename(object, "script", "code");
        rename(object, "toolset", "action_set");
        rename(object, "parent", "base");
        map_digest_at(object, "action_set", renames);
        map_digest_at(object, "base", renames);
        for list in ["tests", "retired"] {
            for item in array_mut(object, list) {
                if let Some(item) = item.as_object_mut() {
                    map_digest_at(item, "test", renames);
                }
            }
        }
        if let Some(Value::Array(updated)) = object.remove("updated") {
            let replaced: Vec<Value> = updated
                .into_iter()
                .map(|mut item| {
                    if let Some(item) = item.as_object_mut() {
                        rename(item, "from", "old");
                        rename(item, "to", "new");
                    }
                    item
                })
                .collect();
            object.insert("replaced".into(), Value::Array(replaced));
        }
        for item in array_mut(object, "replaced") {
            if let Some(item) = item.as_object_mut() {
                map_digest_at(item, "old", renames);
                map_digest_at(item, "new", renames);
            }
        }
    }
    crate::decode_workflow_version(&value.to_string())
}

/// A test, renamed and read: its world's actions, handlers and cassette, its expectations, the
/// run it was recorded from (whose version's digest is the new one).
pub fn test<R: Renames>(json: &str, renames: &R) -> Result<Test, FormatError> {
    let mut value = parse(json)?;
    if let Some(object) = value.as_object_mut() {
        if let Some(Value::Object(world)) = object.get_mut("world") {
            rename(world, "tools", "actions");
            rename(world, "mocks", "handlers");
            if let Some(Value::Object(cassette)) = world.get_mut("cassette") {
                cassette_calls(cassette);
            }
        }
        if let Some(Value::Object(expect)) = object.get_mut("expect") {
            for call in array_mut(expect, "calls") {
                if let Some(call) = call.as_object_mut() {
                    rename(call, "tool", "action");
                    rename_action_value(call, "action");
                }
            }
            if let Some(Value::Object(error)) = expect.get_mut("error") {
                replace_string(
                    error,
                    "name",
                    &[("ToolError", "ActionError"), ("WorkflowError", "RunError")],
                );
            }
            replace_string(
                expect,
                "status",
                &[("pending", "queued"), ("active", "waiting")],
            );
        }
        for signal in array_mut(object, "signals") {
            if let Some(signal) = signal.as_object_mut() {
                replace_string(signal, "when", &[("blocked", "waiting")]);
            }
        }
        rename(object, "origin", "from");
        if let Some(Value::Object(from)) = object.get_mut("from") {
            map_digest_at(from, "version", renames);
        }
    }
    crate::decode_test(&value.to_string())
}

/// A run export (the archive's sealed attempt, a run's export, the e2e corpus's history) of
/// `tenant`, renamed and read: its code, its action set, its events; `run_started` names the
/// digest of the action set it holds.
pub fn run_export<R: Renames>(
    json: &str,
    tenant: &str,
    renames: &R,
) -> Result<RunExport, FormatError> {
    let mut value = parse(json)?;
    if let Some(object) = value.as_object_mut() {
        rename(object, "script", "code");
        rename(object, "toolset", "action_set");
        rename(object, "entries", "events");
        let mut action_set_digest = None;
        if let Some(action_set) = object.get_mut("action_set") {
            action_set_value(action_set, tenant, renames);
            if let Ok(decoded) = crate::decode_action_set(&action_set.to_string()) {
                action_set_digest = Some(decoded.digest());
            }
        }
        for entry in array_mut(object, "events") {
            if let Some(event) = entry.get_mut("event") {
                event_value(event, renames);
                if let (Some(digest), Some(event)) = (action_set_digest, event.as_object_mut())
                    && event.get("type").and_then(Value::as_str) == Some("run_started")
                {
                    event.insert("action_set".into(), Value::String(digest.to_hex()));
                }
            }
        }
    }
    crate::decode_run_export(&value.to_string())
}

/// An event as a history or an inbox stores it, renamed in place.
pub fn event_value<R: Renames>(event: &mut Value, renames: &R) {
    let Some(object) = event.as_object_mut() else {
        return;
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let new_kind = match kind.as_str() {
        "schedule_tool_call" => "schedule_call",
        "tool_call_completed" => "call_completed",
        "tool_call_failed" => "call_failed",
        other => other,
    }
    .to_owned();
    object.insert("type".into(), Value::String(new_kind.clone()));
    match new_kind.as_str() {
        "schedule_call" => {
            rename(object, "tool", "action");
            rename_action_value(object, "action");
        }
        "run_started" => {
            rename(object, "script", "code");
            rename(object, "toolset", "action_set");
            map_digest_at(object, "action_set", renames);
            replace_string(object, "mode", &[("mock", "test")]);
        }
        "run_aborted" => {
            if let Some(Value::Object(reason)) = object.get_mut("reason") {
                replace_string(reason, "kind", &[("invalid_script", "invalid_code")]);
            }
        }
        _ => {}
    }
}

/// A pinned action set's JSON, renamed in place.
fn action_set_value<R: Renames>(value: &mut Value, tenant: &str, renames: &R) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    rename(object, "tools", "actions");
    for action in array_mut(object, "actions") {
        let Some(action) = action.as_object_mut() else {
            continue;
        };
        route_value(action);
        if let Some(Value::Object(policy)) = action.get_mut("policy") {
            policy_value(policy);
        }
    }
    for workflow in array_mut(object, "workflows") {
        let Some(workflow) = workflow.as_object_mut() else {
            continue;
        };
        if workflow.contains_key("id") {
            continue;
        }
        let name = workflow
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Some(old) = workflow.get("version").and_then(digest_of) else {
            continue;
        };
        let pinned = renames
            .pinned(tenant, &name, &old)
            .unwrap_or_else(|| PinnedVersion {
                id: rewind_core::WorkflowId::of_saved(tenant, &name).to_string(),
                version: 1,
                digest: renames.digest(&old).unwrap_or(old),
            });
        workflow.insert("id".into(), Value::String(pinned.id));
        workflow.insert("version".into(), Value::from(pinned.version));
        workflow.insert("digest".into(), Value::String(pinned.digest.to_hex()));
    }
}

/// An action's route: `host` is `harness`; the built-in `webhook` is `signal_url`, and the
/// action routed to it is `signalUrl`.
fn route_value(action: &mut Map<String, Value>) {
    let Some(Value::Object(route)) = action.get_mut("route") else {
        return;
    };
    replace_string(route, "type", &[("host", "harness")]);
    let webhook = route.get("type").and_then(Value::as_str) == Some("builtin")
        && route.get("builtin").and_then(Value::as_str) == Some("webhook");
    if webhook {
        route.insert("builtin".into(), Value::String("signal_url".into()));
        action.insert("name".into(), Value::String("signalUrl".into()));
    }
}

/// An action's policy: `on_interruption` becomes `idempotent`, `heartbeat_timeout_ms`
/// `reconnect_timeout_ms`.
fn policy_value(policy: &mut Map<String, Value>) {
    if let Some(on_interruption) = policy.remove("on_interruption") {
        let idempotent = on_interruption.as_str() != Some("at_most_once");
        policy.insert("idempotent".into(), Value::Bool(idempotent));
    }
    rename(policy, "heartbeat_timeout_ms", "reconnect_timeout_ms");
}

/// The calls of a cassette: `tool` is `action`.
fn cassette_calls(cassette: &mut Map<String, Value>) {
    for call in array_mut(cassette, "calls") {
        if let Some(call) = call.as_object_mut() {
            rename(call, "tool", "action");
            rename_action_value(call, "action");
        }
    }
}

/// A standalone cassette (format v1), renamed and read.
pub fn cassette(json: &str) -> Result<rewind_core::Cassette, FormatError> {
    let mut value = parse(json)?;
    if let Some(object) = value.as_object_mut() {
        cassette_calls(object);
    }
    crate::decode_cassette(&value.to_string())
}

// -------------------------------------------------------------------------------------------
// Partition records

/// The kind of a partition record, as its key says (`rewind-store-slatedb` knows the keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind<'a> {
    RunHead,
    HistoryEvent,
    InboxItem,
    Task,
    Timer,
    Summary,
    /// The counters of a namespace's hour.
    Counters {
        tenant: &'a str,
        namespace: &'a str,
    },
    Outbox,
    /// The state of a tenant homed in the partition.
    TenantState {
        tenant: &'a str,
    },
    /// A record whose names did not change: a signal key, a payload, a watcher, a signal URL
    /// token, a start key, a shape, the gauges (counted again from the heads and summaries).
    Unchanged,
}

/// A partition record as it was stored, renamed, read back in the new names and encoded again:
/// what the store writes in its place.
pub fn record<R: Renames>(
    kind: RecordKind<'_>,
    bytes: &[u8],
    renames: &R,
) -> Result<Vec<u8>, FormatError> {
    use crate::partition::{self as p, decode, encode};
    let mut value: Value =
        serde_json::from_slice(bytes).map_err(|err| FormatError::Json(err.to_string()))?;
    let invalid = |record: &'static str, error: FormatError| FormatError::InvalidRecord {
        record,
        message: error.to_string(),
    };
    let renamed = |value: &Value| value.to_string().into_bytes();
    Ok(match kind {
        RecordKind::RunHead => {
            run_head(&mut value, renames);
            encode(&decode::<p::RunHead>(&renamed(&value)).map_err(|e| invalid("run head", e))?)
        }
        RecordKind::HistoryEvent => {
            if let Some(event) = value.get_mut("event") {
                event_value(event, renames);
            }
            encode(&decode::<p::HistoryEvent>(&renamed(&value))?)
        }
        RecordKind::InboxItem => {
            if let Some(trigger) = value.get_mut("trigger") {
                event_value(trigger, renames);
            }
            encode(&decode::<p::InboxItem>(&renamed(&value))?)
        }
        RecordKind::Task => {
            task(&mut value, renames);
            encode(&decode::<p::Task>(&renamed(&value))?)
        }
        RecordKind::Timer => {
            if let Some(Value::Object(kind)) = value.get_mut("kind") {
                replace_string(
                    kind,
                    "type",
                    &[("script", "code"), ("retry_backoff", "redelivery_backoff")],
                );
            }
            encode(&decode::<p::Timer>(&renamed(&value))?)
        }
        RecordKind::Summary => {
            summary(&mut value, renames);
            encode(&decode::<p::Summary>(&renamed(&value))?)
        }
        RecordKind::Counters { tenant, namespace } => {
            counters(&mut value, tenant, namespace, renames);
            encode(&decode::<p::Counters>(&renamed(&value))?)
        }
        RecordKind::Outbox => {
            outbox(&mut value);
            encode(&decode::<p::Outbox>(&renamed(&value))?)
        }
        RecordKind::TenantState { tenant } => {
            if let Some(object) = value.as_object_mut() {
                for admission in array_mut(object, "admissions") {
                    if let Some(admission) = admission.as_object_mut() {
                        admission_value(admission, tenant, renames);
                    }
                }
                for alert in array_mut(object, "alerts") {
                    if let Some(alert) = alert.as_object_mut() {
                        quota_value(alert);
                    }
                }
            }
            encode(&decode::<p::TenantState>(&renamed(&value))?)
        }
        RecordKind::Unchanged => bytes.to_vec(),
    })
}

/// The status of an active run from what its record holds: `waiting` once quarantined; `running`
/// while a step is in progress, a call Rewind makes itself is pending (a wait of kind `call`
/// whose task is not waiting for the harness), or nothing is waited for; `waiting` otherwise, as
/// `rewind_engine::ports::status_of` computes it.
fn active_status(object: &Map<String, Value>, stepping: bool) -> &'static str {
    if object.get("quarantine").is_some_and(|q| !q.is_null()) {
        return "waiting";
    }
    let waits = object.get("waits").and_then(Value::as_array);
    let ours = waits.is_some_and(|waits| {
        waits.iter().any(|wait| {
            wait.get("kind").and_then(Value::as_str) == Some("call")
                && !matches!(
                    wait.get("task").and_then(Value::as_str),
                    Some("approval" | "parked" | "deferred")
                )
        })
    });
    if stepping || ours || waits.is_none_or(Vec::is_empty) {
        "running"
    } else {
        "waiting"
    }
}

/// A head's or a summary's status: `pending` is `queued`, `active` is `running` or `waiting`.
fn status_value(object: &mut Map<String, Value>, run: Option<RunId>, renames: &impl Renames) {
    match object.get("status").and_then(Value::as_str) {
        Some("pending") => {
            object.insert("status".into(), Value::String("queued".into()));
        }
        Some("active") => {
            let stepping = run.is_some_and(|run| renames.stepping(&run));
            let status = active_status(object, stepping);
            object.insert("status".into(), Value::String(status.into()));
        }
        _ => {}
    }
}

fn waits_value(object: &mut Map<String, Value>) {
    for wait in array_mut(object, "waits") {
        if let Some(wait) = wait.as_object_mut() {
            replace_string(wait, "kind", &[("host", "harness")]);
            // A call's wait names its action; a signal's its signal, which keeps its name.
            if wait.get("kind").and_then(Value::as_str) == Some("call") {
                rename_action_value(wait, "name");
            }
        }
    }
}

fn run_of(object: &Map<String, Value>) -> Option<RunId> {
    object.get("run").and_then(Value::as_str)?.parse().ok()
}

fn run_head<R: Renames>(value: &mut Value, renames: &R) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let run = run_of(object);
    rename(object, "script", "code");
    rename(object, "toolset", "action_set");
    rename(object, "attempts", "unfinished");
    map_digest_at(object, "action_set", renames);
    waits_value(object);
    status_value(object, run, renames);
    if let Some(Value::Object(metadata)) = object.get_mut("metadata") {
        rename(metadata, "parent", "source");
        rename(metadata, "scope", "allowed_labels");
        workflow_value(metadata, run, renames);
    }
}

fn summary<R: Renames>(value: &mut Value, renames: &R) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let run = run_of(object);
    rename(object, "script", "code");
    rename(object, "toolset", "action_set");
    rename(object, "tool_calls", "calls");
    rename(object, "tools", "actions");
    rename(object, "parent", "source");
    map_digest_at(object, "action_set", renames);
    replace_string(object, "mode", &[("mock", "test")]);
    if let Some(Value::Array(actions)) = object.get_mut("actions") {
        for action in actions.iter_mut() {
            if action.as_str() == Some("webhook") {
                *action = Value::String("signalUrl".into());
            }
        }
        actions.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        actions.dedup();
    }
    waits_value(object);
    status_value(object, run, renames);
    workflow_value(object, run, renames);
}

/// A head's metadata or a summary: its `workflow`, which was a saved version's name and digest
/// (or a name alone), is the run's workflow as `rename::workflows` gave it.
fn workflow_value<R: Renames>(object: &mut Map<String, Value>, run: Option<RunId>, renames: &R) {
    let already = object
        .get("workflow")
        .and_then(Value::as_object)
        .is_some_and(|workflow| workflow.contains_key("id"));
    if already {
        return;
    }
    match run.and_then(|run| renames.run_workflow(&run)) {
        Some(workflow) => {
            object.insert("workflow".into(), workflow);
        }
        None => {
            object.remove("workflow");
        }
    }
}

fn task<R: Renames>(value: &mut Value, renames: &R) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    rename(object, "toolset", "action_set");
    rename(object, "tool", "action");
    rename(object, "attempt", "delivery");
    map_digest_at(object, "action_set", renames);
    let webhook = object
        .get("route")
        .and_then(|route| route.get("builtin"))
        .and_then(Value::as_str)
        == Some("webhook");
    route_value(object);
    if webhook {
        // `route_value` renamed the action's `name`, which a task does not have.
        object.remove("name");
        object.insert("action".into(), Value::String("signalUrl".into()));
    }
    if let Some(Value::Object(policy)) = object.get_mut("policy") {
        policy_value(policy);
    }
    if let Some(Value::Object(state)) = object.get_mut("state") {
        rename(state, "next_attempt_at", "next_delivery_at");
    }
}

fn counters<R: Renames>(value: &mut Value, tenant: &str, namespace: &str, renames: &R) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    // By action: `tools` is `actions`, a count of retries one of redeliveries.
    rename(object, "tools", "actions");
    if let Some(Value::Object(actions)) = object.get_mut("actions") {
        let renamed: Map<String, Value> = std::mem::take(actions)
            .into_iter()
            .map(|(name, mut counts)| {
                if let Some(counts) = counts.as_object_mut() {
                    rename(counts, "retries", "redeliveries");
                }
                (
                    if name == "webhook" {
                        "signalUrl".to_owned()
                    } else {
                        name
                    },
                    counts,
                )
            })
            .collect();
        *actions = renamed;
    }
    // By code, then by workflow: the old `workflows` counted saved versions by name, and the
    // `scripts` every code by its digest; both become the new `workflows`, by workflow id.
    let by_code = object.remove("scripts");
    let by_name = object.get("workflows").cloned();
    let already = by_code.is_none()
        && by_name
            .as_ref()
            .and_then(Value::as_object)
            .is_none_or(|workflows| {
                workflows
                    .values()
                    .all(|counts| counts.get("runs").is_some())
            });
    if !already {
        let mut workflows: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
        if let Some(Value::Object(by_code)) = by_code {
            for (code, counts) in by_code {
                let Some(id) = Digest::from_hex(&code)
                    .ok()
                    .and_then(|code| renames.code_workflow(tenant, namespace, &code))
                else {
                    continue;
                };
                let Value::Object(mut counts) = counts else {
                    continue;
                };
                rename(&mut counts, "tools", "actions");
                merge_workflow_counts(workflows.entry(id).or_default(), counts);
            }
        }
        if workflows.is_empty() {
            object.remove("workflows");
        } else {
            object.insert(
                "workflows".into(),
                Value::Object(
                    workflows
                        .into_iter()
                        .map(|(id, c)| (id, Value::Object(c)))
                        .collect(),
                ),
            );
        }
    }
    if let Some(Value::Object(failures)) = object.get_mut("failures") {
        for failure in failures.values_mut() {
            let Some(failure) = failure.as_object_mut() else {
                continue;
            };
            let Some(Value::Object(by_code)) = failure.remove("scripts") else {
                continue;
            };
            let mut workflows: BTreeMap<String, u64> = BTreeMap::new();
            for (code, runs) in by_code {
                if let Some(id) = Digest::from_hex(&code)
                    .ok()
                    .and_then(|code| renames.code_workflow(tenant, namespace, &code))
                {
                    *workflows.entry(id).or_default() += runs.as_u64().unwrap_or(0);
                }
            }
            if !workflows.is_empty() {
                failure.insert(
                    "workflows".into(),
                    Value::Object(
                        workflows
                            .into_iter()
                            .map(|(id, n)| (id, Value::from(n)))
                            .collect(),
                    ),
                );
            }
        }
    }
}

/// Adds the counts of one code to its workflow's.
fn merge_workflow_counts(into: &mut Map<String, Value>, from: Map<String, Value>) {
    for (key, value) in from {
        match (key.as_str(), into.get_mut(&key)) {
            (_, None) => {
                into.insert(key, value);
            }
            ("runs", Some(existing)) => {
                add_counts(existing, &value);
            }
            ("actions", Some(Value::Array(existing))) => {
                if let Value::Array(more) = value {
                    existing.extend(more);
                    existing.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
                    existing.dedup();
                }
            }
            ("first_started_at", Some(existing)) => {
                if value.as_u64() < existing.as_u64() {
                    *existing = value;
                }
            }
            ("last_started_at", Some(existing)) => {
                if value.as_u64() > existing.as_u64() {
                    *existing = value;
                }
            }
            (_, Some(_)) => {}
        }
    }
}

/// Adds numbers key by key, recursively.
fn add_counts(into: &mut Value, from: &Value) {
    match (into, from) {
        (Value::Object(into), Value::Object(from)) => {
            for (key, value) in from {
                match into.get_mut(key) {
                    Some(existing) => add_counts(existing, value),
                    None => {
                        into.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (into @ Value::Number(_), Value::Number(from)) => {
            let sum = into.as_u64().unwrap_or(0) + from.as_u64().unwrap_or(0);
            *into = Value::from(sum);
        }
        _ => {}
    }
}

fn outbox(value: &mut Value) {
    let Some(Value::Object(projection)) = value.get_mut("projection") else {
        return;
    };
    match projection.get("type").and_then(Value::as_str) {
        Some("webhooks") => {
            rename(projection, "event", "webhook");
            let Some(Value::Object(webhook)) = projection.get_mut("webhook") else {
                return;
            };
            let Some(Value::Object(kind)) = webhook.get_mut("kind") else {
                return;
            };
            replace_string(
                kind,
                "type",
                &[
                    ("run_suspended", "run_waiting"),
                    ("credentials_expired", "secret_expired"),
                    ("tool_schema_changed", "action_schema_changed"),
                ],
            );
            rename(kind, "tool", "action");
            rename_action_value(kind, "action");
            if let Some(Value::Array(waiting)) = kind.get_mut("waiting_for") {
                for item in waiting.iter_mut() {
                    if let Some(text) = item.as_str()
                        && let Some(rest) = text.strip_prefix("tool ")
                    {
                        *item = Value::String(format!("action {rest}"));
                    }
                }
            }
        }
        Some("otel") => {
            for span in array_mut(projection, "spans") {
                if let Some(Value::Object(attributes)) = span.get_mut("attributes") {
                    rename(attributes, "rewind.tool", "rewind.action");
                    rename(attributes, "rewind.script", "rewind.code");
                    rename(attributes, "rewind.toolset", "rewind.action_set");
                }
            }
        }
        Some("quota_alert") => {
            if let Some(Value::Object(alert)) = projection.get_mut("alert") {
                quota_value(alert);
            }
        }
        _ => {}
    }
}

/// An admission the tenant actor holds: the code it was for is the run's workflow.
fn admission_value<R: Renames>(admission: &mut Map<String, Value>, tenant: &str, renames: &R) {
    let Some(code) = admission.remove("script") else {
        return;
    };
    let from_run = run_of(admission)
        .and_then(|run| renames.run_workflow(&run))
        .and_then(|workflow| {
            workflow
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let namespace = admission
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let from_code =
        digest_of(&code).and_then(|code| renames.code_workflow(tenant, &namespace, &code));
    if let Some(workflow) = from_run.or(from_code) {
        admission.insert("workflow".into(), Value::String(workflow));
    }
}

/// A quota alert's quota: `scripts` is `workflows`, `max_script_lines` `max_code_lines`.
fn quota_value(alert: &mut Map<String, Value>) {
    replace_string(
        alert,
        "quota",
        &[
            ("scripts", "workflows"),
            ("max_script_lines", "max_code_lines"),
        ],
    );
}

// -------------------------------------------------------------------------------------------
// JSON helpers

fn parse(json: &str) -> Result<Value, FormatError> {
    serde_json::from_str(json).map_err(|err| FormatError::Json(err.to_string()))
}

/// Moves `old` to `new` when `old` is there and `new` is not.
fn rename(object: &mut Map<String, Value>, old: &str, new: &str) {
    if object.contains_key(new) {
        return;
    }
    if let Some(value) = object.remove(old) {
        object.insert(new.to_owned(), value);
    }
}

/// Replaces the string at `key` by its new spelling, if it has one.
fn replace_string(object: &mut Map<String, Value>, key: &str, pairs: &[(&str, &str)]) {
    let Some(current) = object.get(key).and_then(Value::as_str) else {
        return;
    };
    if let Some((_, new)) = pairs.iter().find(|(old, _)| *old == current) {
        object.insert(key.to_owned(), Value::String((*new).to_owned()));
    }
}

/// The built-in action `webhook` is `signalUrl`.
fn rename_action_value(object: &mut Map<String, Value>, key: &str) {
    replace_string(object, key, &[("webhook", "signalUrl")]);
}

fn array_mut<'a>(object: &'a mut Map<String, Value>, key: &str) -> std::slice::IterMut<'a, Value> {
    match object.get_mut(key) {
        Some(Value::Array(items)) => items.iter_mut(),
        _ => [].iter_mut(),
    }
}

fn digest_of(value: &Value) -> Option<Digest> {
    Digest::from_hex(value.as_str()?).ok()
}

/// Replaces the digest at `key` by its new one, if it has one.
fn map_digest_at<R: Renames>(object: &mut Map<String, Value>, key: &str, renames: &R) {
    let Some(old) = object.get(key).and_then(digest_of) else {
        return;
    };
    if let Some(new) = renames.digest(&old) {
        object.insert(key.to_owned(), Value::String(new.to_hex()));
    }
}
