//! Scripts: type-checking (`POST /v1/check`), scenarios in mock mode (`POST /v1/test`), and
//! the distinct scripts executed on the platform (`GET /v1/scripts`).

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::common::Timestamp;
use crate::testing::TestInput;

/// `POST /v1/check`: type-checks a script against the sandbox lib and an action set's
/// declarations, in strict mode, before anything runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckRequest {
    /// The TypeScript source: a single ES module whose default export is the entry point.
    pub script: String,
    /// The action set whose `rewind:actions` the script imports. Without one, `rewind:actions` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_set: Option<String>,
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
    /// `TS<n>` for tsgo; `rewind/<rule>` for the checks at start and the checker's own rules
    /// (`rewind/floating-promise`, `rewind/ignored-async-callback`, `rewind/absent-global`…).
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
// Tests

/// `POST /v1/test`: runs tests in mock mode, in virtual time (`archi/05-testing-and-observability.md`,
/// "Running tests"). The given tests run against the script; with `workflow`, that version's
/// suite runs too, and its interface checks every test. This is how a candidate is checked
/// before it is saved: a corrected script, or the version on a new action set.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({
    "script": "import { github } from \"rewind:actions\";\nexport default async function main(input: { repo: string }) {\n  const issues = await github.list_issues({ repo: input.repo, state: \"open\" });\n  return { open: issues.length };\n}\n",
    "tests": [{
        "name": "counts the open issues",
        "input": { "repo": "acme/api" },
        "world": { "actions": { "github.list_issues": { "returns": [{ "number": 4 }] } } },
        "expect": { "status": "completed", "output": { "open": 1 } }
    }]
}))]
pub struct TestRequest {
    /// The TypeScript source. With `workflow`, the version's own when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// The action set the script imports from `rewind:actions`. With `workflow`, the version's own
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_set: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestInput>,
    /// A workflow, `triage` (its latest version), `triage@<version>` or `triage@v12`: its suite
    /// runs too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
}

// -------------------------------------------------------------------------------------------
// Scripts executed on the platform

/// `GET /v1/scripts`: every distinct script the runs of a scope ran in a window, most recently run
/// first, each with its runs by status and the actions it called, from the hourly counters. A scope
/// and a window, as `GET /v1/runs/stats` takes them (`crate::runs::RunStatsQuery`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct ScriptFilter {
    /// The scope, as a query: one term `namespace:<name>` of one value, or nothing for the
    /// tenant. Any other term is refused with its position (`invalid-query`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Runs started at or after this time, rounded down to the hour. No lower bound by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub since: Option<Timestamp>,
    /// Runs started before this time, rounded up to the hour. Now by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub until: Option<Timestamp>,
    /// Only this script: its digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Only scripts that called this action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
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
    /// The script's title: the one a person gave it, else the first line of the comment at its
    /// top. Absent when it has neither.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The description that goes with the title: the person's with theirs, else the rest of
    /// the comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where the title comes from; absent without a title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_source: Option<TitleSource>,
    /// The first line of the source that is not blank, for display.
    pub headline: String,
    pub first_run_at: Timestamp,
    pub last_run_at: Timestamp,
    /// Runs of this script, by status.
    pub runs: RunCounts,
    /// The actions its runs called.
    pub actions: Vec<String>,
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

/// `GET /v1/scripts/{digest}`: one script's source, and what its runs did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Script {
    pub digest: String,
    /// The script's title, as in `ScriptSummary.title`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where the title comes from; absent without a title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_source: Option<TitleSource>,
    /// The TypeScript source, as submitted.
    pub source: String,
    /// The first line of the source that is not blank, for display.
    pub headline: String,
    /// Runs of this script, by status, as the read model keeps them.
    pub runs: RunCounts,
    /// The actions its runs called.
    pub actions: Vec<String>,
    /// When its first and last runs started; absent when no run is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_run_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<Timestamp>,
}

/// Where a script's title comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    /// A person gave it (`PATCH /v1/scripts/{digest}`), for the whole organization.
    Person,
    /// The first line of the comment at the top of the source (`/** Title … */` or `// Title`).
    Comment,
}

/// `PATCH /v1/scripts/{digest}`: the title and description of a script, for the whole
/// organization, over those of the comment at its top. The source does not change: a script is
/// its digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({
    "title": "Triage new GitHub issues",
    "description": "Labels each new issue, and asks for an approval before closing duplicates."
}))]
pub struct SetScriptTitle {
    /// The title, trimmed: 1 to 120 characters. `null` removes the title and the description a
    /// person gave: those of the comment show again.
    #[serde(deserialize_with = "nullable")]
    #[schema(required = true)]
    pub title: Option<String>,
    /// The description, trimmed: at most 2,000 characters. Absent, `null` or `""`: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A field that must be present, and may be `null`.
fn nullable<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

/// The first line of a source that is not blank, trimmed, cut at 120 characters: how a script
/// is shown in a list.
pub fn headline(source: &str) -> String {
    let line = source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    match line.char_indices().nth(120) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}
