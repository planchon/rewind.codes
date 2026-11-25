//! Reading across runs, for the dashboard: action statistics and payloads on demand
//! (`archi/05-testing-and-observability.md`, "Read model" and "Dashboard"). The list of
//! workflows is in [`crate::workflows`], the list of runs in [`crate::runs`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::common::Timestamp;

/// `GET /v1/actions/stats`: latency, errors and redeliveries per action, over the calls of a scope
/// scheduled in a window, from the hourly counters. A scope and a window, as `GET /v1/runs/stats`
/// takes them (`crate::runs::RunStatsQuery`), and the start of an action's name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct ActionStatsFilter {
    /// The scope, as a query: one term `namespace:<name>` of one value, or nothing for the
    /// organization. Any other term is refused with its position (`invalid-query`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Only the actions of this group, their name's prefix (`github` for `github.list_issues`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Only calls of runs of this action set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_set: Option<String>,
    /// Only this action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Calls scheduled at or after this time, rounded down to the hour. 24 hours ago by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub since: Option<Timestamp>,
    /// Calls scheduled before this time, rounded up to the hour. Now by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub until: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActionStatsPage {
    pub since: Timestamp,
    pub until: Timestamp,
    /// By action name.
    pub actions: Vec<ActionStats>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActionStats {
    pub action: String,
    /// Calls scheduled in the window.
    pub calls: u64,
    /// Calls that failed, whatever the kind.
    pub errors: u64,
    /// Failed calls by kind: `error`, `unavailable`, `timeout`, `outcome_unknown`.
    pub error_kinds: BTreeMap<String, u64>,
    /// Deliveries beyond the first, over every call.
    pub redeliveries: u64,
    /// From `schedule_call` to the completion, in milliseconds, over the calls that have an
    /// outcome; zeros when none has.
    pub latency_ms: Latency,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Latency {
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

/// `GET /v1/payloads/{digest}`: the run the payload is read through.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct PayloadQuery {
    /// The run whose history holds the payload: a payload belongs to its run, and is read
    /// through it.
    pub run: String,
}
