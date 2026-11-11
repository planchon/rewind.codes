//! Scripts: type-checking (`POST /v1/check`), scenarios in mock mode (`POST /v1/test`), and
//! the distinct scripts executed on the platform (`GET /v1/scripts`).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::common::{AbortReason, RunStatus, ScriptError, Timestamp};

/// `POST /v1/check`: type-checks a script against the sandbox lib and a toolset's
/// declarations, in strict mode, before anything runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckRequest {
    /// The TypeScript source: a single ES module whose default export is the entry point.
    pub script: String,
    /// The toolset whose `ptc:tools` the script imports. Without one, `ptc:tools` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolset: Option<String>,
}

/// The diagnostics of a check. A script with only warnings passes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    /// No diagnostic of severity `error`.
    pub ok: bool,
    pub diagnostics: Vec<Diagnostic>,
}

/// One finding of the checks: tsgo's type errors, the checks at start, and the checker's own
/// rules. Written for a model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(example = json!({
    "severity": "error",
    "code": "TS2322",
    "message": "Type 'number' is not assignable to type 'string'.",
    "location": { "line": 3, "column": 28, "endLine": 3, "endColumn": 30 },
    "frame": "3 | const issues = await github.list_issues({ repo: 42 });\n  |                            ^^"
}))]
pub struct Diagnostic {
    pub severity: Severity,
    /// `TS<n>` for tsgo; `ptc/<rule>` for the checks at start and the checker's own rules
    /// (`ptc/floating-promise`, `ptc/ignored-async-callback`, `ptc/absent-global`…).
    pub code: String,
    pub message: String,
    /// Where in `script.ts`; absent for a diagnostic about the script as a whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
    /// A few lines of the source around the location, with the span underlined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
    /// What to do instead, when the checker knows (the hint of an absent global, for example).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.location {
            Some(location) => write!(f, "script.ts:{}:{}", location.line, location.column)?,
            None => f.write_str("script.ts")?,
        }
        write!(
            f,
            " {} {}: {}",
            self.severity.as_str(),
            self.code,
            self.message
        )?;
        if let Some(frame) = &self.frame {
            write!(f, "\n{frame}")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, "\nhint: {hint}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// A span in `script.ts`, 1-based, the end exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub line: u32,
    pub column: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
}

// -------------------------------------------------------------------------------------------
// Scenarios

/// `POST /v1/test`: runs a script in mock mode, in virtual time, against a cassette, mock
/// handlers and static responses, and checks expectations (`archi/05-testing-and-observability.md`,
/// "Testing and mocks"). It is what `ptc test scenario.json` runs, with the files inlined.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestRequest {
    /// The TypeScript source.
    pub script: String,
    /// The toolset the script imports from `ptc:tools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolset: Option<String>,
    /// The run's input. `{}` when absent.
    #[serde(default = "empty_object")]
    pub input: Value,
    /// A cassette of the stored format v1, recorded from a real run
    /// (`GET /v1/runs/{id}/journal?as=cassette`). Tried first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub cassette: Option<Value>,
    /// TypeScript mock handlers, run in a separate isolate. Tried after the cassette.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mocks: Option<String>,
    /// Static responses per tool, tried last: the result, or `{"$error": "message"}` for a
    /// failure (as in `ptc.tools.json`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tools: BTreeMap<String, Value>,
    /// Signals sent to the run, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<ScenarioSignal>,
    #[serde(default)]
    pub expect: Expectations,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// A signal sent during a scenario.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScenarioSignal {
    pub name: String,
    #[serde(default)]
    pub payload: Value,
    /// When it is sent: `"blocked"` (the default) once the run can advance no further without
    /// the outside world; or a duration of virtual time after the start, such as `"2h"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

/// What must be true at the end of a scenario. Everything not given is not checked.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Expectations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RunStatus>,
    /// The output contains these values (objects are compared key by key, recursively).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// The run failed with an error of this name, whose message contains `message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ExpectedError>,
    /// How many times each tool was called.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calls: Option<BTreeMap<String, u64>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectedError {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The outcome of a scenario.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    /// Every expectation holds.
    pub passed: bool,
    /// The expectations that do not hold, one sentence each, written for a model.
    pub failures: Vec<String>,
    pub status: RunStatus,
    #[serde(
        default,
        deserialize_with = "crate::common::present",
        skip_serializing_if = "Option::is_none"
    )]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ScriptError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort: Option<AbortReason>,
    /// How many times each tool was called.
    pub calls: BTreeMap<String, u64>,
    /// The LLM trace of the run (as `GET /v1/runs/{id}/trace?format=llm`).
    pub trace: String,
}

// -------------------------------------------------------------------------------------------
// Scripts executed on the platform

/// `GET /v1/scripts`: every distinct script, most recently run first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct ScriptFilter {
    /// Only scripts that ran with this label, as `key:value`. Repeat to require several.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[param(rename = "label")]
    pub labels: Vec<String>,
    /// Only scripts that called this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// At most this many scripts (50 by default, 500 at most).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// The `nextCursor` of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScriptPage {
    pub scripts: Vec<ScriptSummary>,
    /// Pass it as `cursor` to read the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// A distinct script: content-addressed, so every run of the same source counts here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScriptSummary {
    /// SHA-256 of the TypeScript source, lowercase hex.
    pub digest: String,
    /// The first line of the source that is not blank, for display.
    pub headline: String,
    pub first_run_at: Timestamp,
    pub last_run_at: Timestamp,
    /// Runs of this script, by status.
    pub runs: RunCounts,
    /// The tools its runs called.
    pub tools: Vec<String>,
    /// The labels its runs carried, with the number of runs for each `key:value`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, u64>,
}

/// A number of runs per status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunCounts {
    pub pending: u64,
    pub active: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub aborted: u64,
}

/// `GET /v1/scripts/{digest}`: one script's source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Script {
    pub digest: String,
    /// The TypeScript source, as submitted.
    pub source: String,
}
