//! Reading across runs, for the dashboard: tool statistics and payloads on demand
//! (`archi/05-testing-and-observability.md`, "Read model" and "Dashboard"). The list of
//! scripts is in [`crate::scripts`], the list of runs in [`crate::runs`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::common::Timestamp;

/// `GET /v1/tools/stats`: latency, errors and retries per tool, over the calls scheduled in a
/// time window.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct ToolStatsFilter {
    /// Only calls of runs of this toolset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolset: Option<String>,
    /// Only this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Calls scheduled at or after this time. 24 hours ago by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub since: Option<Timestamp>,
    /// Calls scheduled before this time. Now by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub until: Option<Timestamp>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatsPage {
    pub since: Timestamp,
    pub until: Timestamp,
    /// By tool name.
    pub tools: Vec<ToolStats>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolStats {
    pub tool: String,
    /// Calls scheduled in the window.
    pub calls: u64,
    /// Calls that failed, whatever the kind.
    pub errors: u64,
    /// Failed calls by kind: `error`, `unavailable`, `timeout`, `outcome_unknown`.
    pub error_kinds: BTreeMap<String, u64>,
    /// Attempts beyond the first, over every call.
    pub retries: u64,
    /// From `schedule_tool_call` to the completion, in milliseconds, over the calls that have an
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
