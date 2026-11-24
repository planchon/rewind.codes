//! Workflows: workflows with a name and JSON Schemas for their input, their output and the signals
//! they accept, that workflows start and signal (`archi/01-product.md`, "Workflows").

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::action_sets::Declarations;
use crate::common::{Labels, RunStatus, Timestamp};
use crate::testing::{Expectations, TestInput};

/// `POST /v1/workflows`: checks a workflow and saves it as a new version. Its code is checked
/// against its action set and against its own schemas: its default export must take the input and
/// return the output, and `signal()` only takes the declared names. Saving the same workflow
/// again under the same labels returns the version saved first (200): the labels of a key limited
/// to labels are part of the version, so such a key saves a version of its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({
    "name": "triage",
    "description": "Triage the stale issues of a repository, after approval.",
    "actionSet": "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4",
    "code": "import { signal } from \"rewind\";\nexport default async function main(input: { repo: string }) {\n  const { close } = await signal(\"approval\");\n  return { closed: close ? 1 : 0 };\n}\n",
    "input": { "type": "object", "properties": { "repo": { "type": "string" } }, "required": ["repo"] },
    "output": { "type": "object", "properties": { "closed": { "type": "integer" } }, "required": ["closed"] },
    "signals": { "approval": { "type": "object", "properties": { "close": { "type": "boolean" } }, "required": ["close"] } }
}))]
pub struct CreateWorkflow {
    /// What workflows import from `rewind:workflows`: an identifier that is not a reserved word of
    /// JavaScript.
    pub name: String,
    /// Shown to the model in the declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The id of the action set its runs call, already registered.
    pub action_set: String,
    /// The TypeScript source: a single ES module whose default export is the entry point.
    pub code: String,
    /// JSON Schema of the input of `main`.
    #[schema(value_type = Object)]
    pub input: Value,
    /// JSON Schema of what `main` returns.
    #[schema(value_type = Object)]
    pub output: Value,
    /// The signals its runs accept, by name, each with the JSON Schema of its payload. Names
    /// starting with `rewind:` are reserved.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schema(value_type = Object)]
    pub signals: BTreeMap<String, Value>,
    /// The version it derives from, `name@<version>`, `name@v<number>` or a version: the latest
    /// version of the same name saved under the caller's labels when absent. Its suite is
    /// inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// New tests: inline, by the digest of a recorded test, or a recorded test with a new
    /// expectation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestInput>,
    /// Inherited tests replaced, each with a reason: their state or result is out of date.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub update: Vec<TestUpdate>,
    /// Inherited tests dropped, with a reason: they are deprecated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retire: Vec<TestRetirement>,
    /// The secrets its runs attach, by MCP server or API (`org/<name>`, `ns/<name>`), part of
    /// the version: checked against the saver's rights now (an `ns/` secret by its name only),
    /// and read by its runs without checking the caller's again (`secret-required`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: crate::secrets::Attachments,
}

/// An inherited test replaced: by a whole new test, which takes its name, or by itself with a
/// new expectation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestUpdate {
    pub name: String,
    /// Why: kept in the lineage.
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<TestInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expectations>,
}

/// Inherited tests dropped, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestRetirement {
    pub names: Vec<String>,
    /// Why: kept in the lineage.
    pub reason: String,
}

/// A test of a version's suite.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SuiteTest {
    pub name: String,
    /// Its digest: `GET /v1/tests/{digest}`.
    pub digest: String,
}

/// A test of the parent a version does not keep.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RetiredTest {
    pub name: String,
    pub digest: String,
    pub reason: String,
}

/// A test of the parent a version replaced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdatedTest {
    pub name: String,
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// `POST /v1/workflows/{name}/record`: tests recorded from the workflow's past runs, one per
/// path. They are stored, and join a suite only when a save references them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({ "query": "status:completed", "limit": 20 }))]
pub struct RecordTests {
    /// Which runs, in the query language of the run lists; the completed and failed runs of the
    /// workflow, newest first, the 1,000 newest at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// These runs, rather than a query.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<String>,
    /// At most this many tests: 20 by default, 100 at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// What a recording stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordedTests {
    pub tests: Vec<RecordedTest>,
    /// The runs left out, and why.
    pub skipped: Vec<SkippedRun>,
}

/// A test recorded from a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RecordedTest {
    /// What `save_workflow` takes in `tests`.
    pub digest: String,
    pub name: String,
    /// The path the run took: `github.list_issues · signal approval · github.close_issue×n →
    /// completed`.
    pub path: String,
    pub run: String,
    pub status: RunStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkippedRun {
    pub run: String,
    pub reason: String,
}

/// A version of a workflow: immutable, identified by its digest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    pub name: String,
    /// The digest of the version (SHA-256, lowercase hex). `name@<version>` names it, as do its
    /// first 12 digits.
    pub version: String,
    /// The version's number among the versions of its workflow, from 1 in the order they were
    /// saved (`triage v12`): `name@v<number>` names it too.
    #[schema(example = 12)]
    pub number: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The id of the action set its runs call.
    pub action_set: String,
    /// Digest of its code (`GET /v1/scripts/{digest}`).
    pub code_digest: String,
    #[schema(value_type = Object)]
    pub input: Value,
    #[schema(value_type = Object)]
    pub output: Value,
    #[schema(value_type = Object)]
    pub signals: BTreeMap<String, Value>,
    /// The labels of the key that saved it, part of the version: a key limited to labels only
    /// sees the workflows carrying them. Empty for a version saved tenant-wide.
    pub labels: Labels,
    /// The secrets its runs attach, part of the version.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: crate::secrets::Attachments,
    pub created_at: Timestamp,
    /// How a code that starts it sees it: its declaration in `rewind:workflows`, and its line
    /// of the prompt rendering.
    pub declarations: Declarations,
    /// The version it derives from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Its suite: the tests it passed when it was saved, by name.
    #[serde(default)]
    pub tests: Vec<SuiteTest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired: Vec<RetiredTest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updated: Vec<UpdatedTest>,
}

/// `GET /v1/workflows`: the latest version of each workflow, by name; with `script`, every
/// version whose code it is, newest first, on one page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPage {
    pub workflows: Vec<Workflow>,
    /// Pass it as `after` to read the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// `GET /v1/workflows`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowQuery {
    /// The workflows whose name comes after this one: the `next` of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// At most this many workflows (50 by default, 500 at most).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// The digest of a code: every version of any workflow whose code it is, newest first,
    /// rather than the latest version of each workflow. `after` and `limit` do not apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

/// `GET /v1/workflows/{name}`: the versions of a workflow, newest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowVersions {
    pub name: String,
    pub versions: Vec<Workflow>,
}

/// A workflow an action set pins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PinnedWorkflow {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The workflow a run executes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunWorkflow {
    pub name: String,
    pub version: String,
    /// The version's number among the versions of its workflow (`Workflow.number`).
    pub number: u32,
}

/// The run that started a run, and the call that did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartedBy {
    /// The id of the run that started it.
    pub run: String,
    /// That run's handle; its id once it is erased.
    pub handle: String,
    /// The `seq` of the call of `workflows.<name>.start`.
    pub seq: u32,
    /// A detached run lives on its own; otherwise the run that started it does not end before
    /// it, and cancels it if it fails or is cancelled.
    pub detached: bool,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_workflow_reads_with_its_schemas() {
        let workflow: CreateWorkflow = serde_json::from_value(json!({
            "name": "triage",
            "actionSet": "00",
            "code": "export default async () => 1;",
            "input": {},
            "output": { "type": "integer" },
        }))
        .unwrap();
        assert!(workflow.signals.is_empty());
        assert!(
            serde_json::from_value::<CreateWorkflow>(json!({
                "name": "triage", "actionSet": "00", "code": "", "input": {}, "output": {}, "x": 1,
            }))
            .is_err(),
            "unknown fields are refused"
        );
    }
}
