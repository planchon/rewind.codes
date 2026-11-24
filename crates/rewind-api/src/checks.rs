//! Checks and tests of a workflow's code: type-checking (`POST /v1/check`) and tests in a test
//! run (`POST /v1/test`). Neither creates nor changes a workflow.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::testing::TestInput;

/// `POST /v1/check`: type-checks a code against the sandbox lib and an action set's
/// declarations, in strict mode, before anything runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckRequest {
    /// The TypeScript source: a single ES module whose default export is the entry point.
    pub code: String,
    /// The action set whose `rewind:actions` the code imports. Without one, `rewind:actions` is
    /// empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_set: Option<String>,
}

/// The diagnostics of a check. A code with only warnings passes. With `Rewind-Namespace`, the
/// workflow that has the code, if its namespace or the organization has one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    /// No diagnostic of severity `error`.
    pub ok: bool,
    pub diagnostics: Vec<Diagnostic>,
    /// The workflow a run of this code would be a run of, and its version with that code: the
    /// namespace's, else the organization's. Absent for code no workflow has yet: its first run
    /// creates one. A check creates nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<crate::workflows::RunWorkflow>,
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
    /// Where in `workflow.ts`; absent for a diagnostic about the code as a whole.
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
            Some(location) => write!(f, "workflow.ts:{}:{}", location.line, location.column)?,
            None => f.write_str("workflow.ts")?,
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

/// A span in `workflow.ts`, 1-based, the end exclusive.
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

/// `POST /v1/test`: runs tests in a test run, in virtual time (`archi/05-testing-and-observability.md`,
/// "Running tests"). The given tests run against the code; with `workflow`, that workflow's suite
/// runs too, and its version's interface checks every test. This is how a candidate is checked
/// before it becomes a version: a corrected code, or the version on a new action set. A test
/// creates and changes nothing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({
    "code": "import { github } from \"rewind:actions\";\nexport default async function main(input: { repo: string }) {\n  const issues = await github.list_issues({ repo: input.repo, state: \"open\" });\n  return { open: issues.length };\n}\n",
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
    pub code: Option<String>,
    /// The action set the code imports from `rewind:actions`. With `workflow`, the version's own
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_set: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestInput>,
    /// A workflow, by its id or its name (`triage`, its latest version), or one version
    /// (`triage@v12`, `triage@<digest>`): its suite runs too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
}
