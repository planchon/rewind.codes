//! Workflows (`archi/01-product.md`, "Workflows"): the template of runs. A workflow has an id the
//! API gives it (`wf_` and 12 characters), a name when it is given one, unique in its scope, a
//! scope (the namespace whose run created it, or the organization once promoted), versions 1,
//! 2, 3…, and a suite. A run given code that its namespace has not run creates one; the writes
//! below name, describe, version, test, promote, fork and pull them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::action_sets::Declarations;
use crate::common::{Labels, RunStatus, Timestamp};
use crate::namespaces::NamespaceName;
use crate::testing::{Expectations, TestInput};

/// `POST /v1/workflows`: creates a workflow and its version 1, in the namespace
/// `Rewind-Namespace` names, else the organization's. Its code is checked against its action set
/// and, when it has one, its interface: its default export must take the input and return the
/// output, and `signal()` only takes the declared names. Its tests run through the gate. Without
/// a name, a code its scope already has answers that code's workflow, unchanged (200); a
/// creation sent again answers the workflow it created (200).
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
    /// What `rewind:workflows` exports and callers start it by: an identifier of at most 64
    /// characters that is not a reserved word of JavaScript and does not start with `wf_`,
    /// unique in its scope (`name-taken`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Shown, never part of its identity: at most 2,000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The id of the action set its runs call, already registered.
    pub action_set: String,
    /// The TypeScript source: a single ES module whose default export is the entry point.
    pub code: String,
    /// JSON Schema of the input of `main`. Given with `output` or `signals`, or alone, it gives
    /// the version an interface; a missing input or output then takes anything (`true`), missing
    /// signals declare none. Without any of the three, the version has no interface: any input,
    /// output and signal name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub input: Option<Value>,
    /// JSON Schema of what `main` returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub output: Option<Value>,
    /// The signals its runs accept, by name, each with the JSON Schema of its payload. Names
    /// starting with `rewind:` are reserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub signals: Option<BTreeMap<String, Value>>,

    /// The secrets its runs use, by MCP server or API (`org/<name>`, `ns/<name>`), part of
    /// the version: checked against the caller's rights now (an `ns/` secret by its name only),
    /// and read by its runs without checking their callers' (`secret-required`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: crate::secrets::ServerSecrets,
    /// Its first suite: inline tests, digests of recorded tests, or recorded tests with a new
    /// expectation. Each passes against version 1 first (`tests-failed`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestInput>,
}

/// The schemas of a version's interface, as a request gives them: `None` when it gives none of
/// them; else a missing input or output takes anything (`true`) and missing signals declare
/// none.
pub fn interface(
    input: &Option<Value>,
    output: &Option<Value>,
    signals: &Option<BTreeMap<String, Value>>,
) -> Option<(Value, Value, BTreeMap<String, Value>)> {
    if input.is_none() && output.is_none() && signals.is_none() {
        return None;
    }
    Some((
        input.clone().unwrap_or(Value::Bool(true)),
        output.clone().unwrap_or(Value::Bool(true)),
        signals.clone().unwrap_or_default(),
    ))
}

/// `PATCH /v1/workflows/{id}`: a JSON merge patch of its name and description. The same id, no
/// new version.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({ "name": "triage", "description": null }))]
pub struct PatchWorkflow {
    /// A name gives or changes it (`name-taken`, `invalid-workflow`); `null` removes it.
    #[serde(default, with = "patch", skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub name: Option<Option<String>>,
    /// At most 2,000 characters; `null` removes it.
    #[serde(default, with = "patch", skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
}

/// A field of a JSON merge patch: absent leaves it, `null` removes it.
mod patch {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Option<Option<String>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.as_ref().map(Option::as_deref).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Option<String>>, D::Error> {
        Option::<String>::deserialize(deserializer).map(Some)
    }
}

/// `POST /v1/workflows/{id}/versions`: makes its next version, through the gate: the workflow's
/// suite, minus `retire`, with `replace` applied, plus `tests`, runs against it, and the version
/// is stored only if each test passes (`tests-failed`). What it leaves out is its latest
/// version's: its code, its action set, its interface, its secrets. The same content without a
/// change of tests makes no version: the latest is answered (200).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateVersion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_set: Option<String>,
    /// JSON Schema of the input of `main`. Given with `output` or `signals`, or alone, it gives
    /// the version a new interface, as `CreateWorkflow.input` does; without any of the three, the
    /// version keeps its latest version's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub input: Option<Value>,
    /// JSON Schema of what `main` returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub output: Option<Value>,
    /// The signals its runs accept, by name, each with the JSON Schema of its payload. Names
    /// starting with `rewind:` are reserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub signals: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<crate::secrets::ServerSecrets>,
    /// New tests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestInput>,
    /// Tests of the suite replaced, each with a reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replace: Vec<TestReplacement>,
    /// Tests of the suite retired, with a reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retire: Vec<TestRetirement>,
}

/// `POST /v1/workflows/{id}/tests`: adds tests to its suite, replaces or retires some, with
/// their reasons, without a version: each test given and each replacement runs against its
/// latest version first (`tests-failed`). Its next version lists them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChangeTests {
    /// New tests: inline, by the digest of a recorded test, or a recorded test with a new
    /// expectation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<TestInput>,
    /// Tests of the suite replaced, each with a reason: their state or result is out of date.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replace: Vec<TestReplacement>,
    /// Tests of the suite retired, with a reason: they are deprecated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retire: Vec<TestRetirement>,
}

/// A test of the suite replaced: by a whole new test, which takes its name, or by itself with a
/// new expectation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestReplacement {
    pub name: String,
    /// Why: kept in the lineage.
    pub reason: String,
    /// A whole new test, which takes the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<TestInput>,
    /// Or only a new expectation for the test of the suite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expectations>,
}

/// Tests of the suite retired, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestRetirement {
    pub names: Vec<String>,
    /// Why: kept in the lineage.
    pub reason: String,
}

/// A test a version passed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SuiteTest {
    pub name: String,
    /// Its digest: `GET /v1/tests/{digest}`.
    pub digest: String,
}

/// A test of the workflow's suite a version does not keep.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RetiredTest {
    pub name: String,
    pub digest: String,
    pub reason: String,
}

/// A test of the workflow's suite a version replaced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplacedTest {
    pub name: String,
    pub old: String,
    pub new: String,
    pub reason: String,
}

/// A test the workflow's suite holds or held: when it joined, and when and why it left.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SuiteEntry {
    pub name: String,
    /// Its digest: `GET /v1/tests/{digest}`.
    pub digest: String,
    pub added_at: Timestamp,
    /// When it left the suite, retired or replaced; absent while it is in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<Timestamp>,
    /// Why it left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `POST /v1/workflows/{id}/record`: tests recorded from the workflow's past runs, one per
/// path. They are stored, and join a suite only when a version or a change of tests gives them.
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
    /// What a version or a change of tests takes in `tests`.
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

/// The comment at the top of a version's code: its first line, then the lines after it. Shown
/// for a workflow without a name or a description; never part of its identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodeComment {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A version of a workflow: immutable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowVersion {
    /// Its number among the versions of its workflow, from 1: `triage@v12` names it.
    #[schema(example = 12)]
    pub version: u32,
    /// The digest of its document (SHA-256, lowercase hex): `triage@<digest>` names it, as do
    /// its first 12 digits.
    pub digest: String,
    /// Digest of its code.
    pub code_digest: String,
    /// The comment at the top of its code, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<CodeComment>,
    /// The id of the action set its runs call.
    pub action_set: String,
    /// Its interface; absent for a version that accepts any input, output and signal name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub signals: Option<BTreeMap<String, Value>>,
    /// The labels of the grant that made it, part of the version: a caller limited to labels
    /// only sees the versions carrying them.
    pub labels: Labels,
    /// The secrets its runs use, part of the version.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: crate::secrets::ServerSecrets,
    pub created_at: Timestamp,
    /// How a code that starts it sees it, for a version with an interface and a workflow with a
    /// name: its declaration in `rewind:workflows`, and its line of the prompt rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declarations: Option<Declarations>,
    /// The version it derives from: its digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The suite it passed, by name.
    #[serde(default)]
    pub tests: Vec<SuiteTest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired: Vec<RetiredTest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaced: Vec<ReplacedTest>,
}

/// A fork's source: the organization's workflow it copies, and the version of it it last took.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSource {
    pub workflow: String,
    pub version: u32,
}

/// A workflow (`GET /v1/workflows/{id}`, and what each write answers).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    /// `wf_` and 12 characters: it never changes.
    #[schema(example = "wf_7k2m9q4x8d1c")]
    pub id: String,
    /// `wf-` and the first 6 characters of its id: how it shows without a name or a comment,
    /// and the origin of its runs' handles without a name.
    #[schema(example = "wf-7k2m9q")]
    pub short_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The description given; display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The namespace it belongs to; absent for the organization's, which every namespace's
    /// runs execute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<NamespaceName>,
    /// For a fork: the organization's workflow it copies, and the version it last took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WorkflowSource>,
    /// The labels of the grant that created it.
    pub labels: Labels,
    pub created_at: Timestamp,
    /// When it was promoted to the organization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_at: Option<Timestamp>,
    /// The version shown: its latest the caller sees, the one the reference names, or the one a
    /// write made.
    pub version: WorkflowVersion,
    /// Its code, the TypeScript source of `version`.
    pub code: String,
    /// Its versions the caller sees, newest first.
    pub versions: Vec<WorkflowVersion>,
    /// Every test its suite holds or held, with the reasons they left.
    pub suite: Vec<SuiteEntry>,
}

/// A workflow in a list (`GET /v1/workflows`), with what its runs did in the window, from the
/// hourly counters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSummary {
    pub id: String,
    pub short_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<NamespaceName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WorkflowSource>,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_at: Option<Timestamp>,
    /// Its latest version the caller sees, without its suite's tests.
    pub latest: WorkflowVersion,
    /// Its runs of the window, by status.
    pub runs: RunCounts,
    /// The actions its runs of the window called.
    pub actions: Vec<String>,
    /// When its first and last runs of the window started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_run_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<Timestamp>,
}

/// A number of runs per status, from the hourly counters: those started and not ended are
/// active (queued, running or waiting), the others by how they ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunCounts {
    pub active: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub aborted: u64,
}

/// `GET /v1/workflows`: the workflows the caller sees, by id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowPage {
    pub workflows: Vec<WorkflowSummary>,
    /// Pass it as `after` to read the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// Which workflows `GET /v1/workflows` lists, by scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowScope {
    /// The namespace's own: `Rewind-Namespace`'s or a token's, every namespace's without one.
    Namespace,
    /// The organization's.
    Organization,
}

/// `GET /v1/workflows`: the workflows the caller sees, by id; a caller of a namespace (a token,
/// or `Rewind-Namespace`) sees its namespace's and the organization's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowQuery {
    /// Only the namespace's workflows, or only the organization's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(inline)]
    pub scope: Option<WorkflowScope>,
    /// Only the workflows one of whose versions has this code: its digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The scope of the counts, as a query: one term `namespace:<name>` of one value, or
    /// nothing. Any other term is refused with its position (`invalid-query`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Count the runs started at or after this time, rounded down to the hour; the last 30 days
    /// by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub since: Option<Timestamp>,
    /// Count the runs started before this time, rounded up to the hour. Now by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = DateTime)]
    pub until: Option<Timestamp>,
    /// The workflows whose id comes after this one: the `next` of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// At most this many workflows (50 by default, 500 at most).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A workflow an action set pins, and the version it pins.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PinnedWorkflow {
    pub name: String,
    pub id: String,
    pub version: u32,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The workflow a run is a run of, and the version it executes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunWorkflow {
    #[schema(example = "wf_7k2m9q4x8d1c")]
    pub id: String,
    /// Its name when the run was created, if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The version's number (`WorkflowVersion.version`).
    pub version: u32,
    /// The version's digest.
    pub digest: String,
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
    fn a_workflow_reads_with_or_without_an_interface() {
        let workflow: CreateWorkflow = serde_json::from_value(json!({
            "name": "triage",
            "actionSet": "00",
            "code": "export default async () => 1;",
            "input": {},
            "output": { "type": "integer" },
        }))
        .unwrap();
        let (input, output, signals) =
            interface(&workflow.input, &workflow.output, &workflow.signals).unwrap();
        assert_eq!((input, output), (json!({}), json!({ "type": "integer" })));
        assert!(signals.is_empty());
        let without: CreateWorkflow = serde_json::from_value(json!({
            "actionSet": "00",
            "code": "export default async () => 1;",
        }))
        .unwrap();
        assert_eq!(
            interface(&without.input, &without.output, &without.signals),
            None
        );
        assert_eq!(without.name, None);
        assert!(
            serde_json::from_value::<CreateWorkflow>(json!({
                "actionSet": "00", "code": "", "base": "x",
            }))
            .is_err(),
            "unknown fields are refused"
        );
    }

    #[test]
    fn a_patch_tells_absent_from_null() {
        let patch: PatchWorkflow =
            serde_json::from_value(json!({ "name": "triage", "description": null })).unwrap();
        assert_eq!(patch.name, Some(Some("triage".into())));
        assert_eq!(patch.description, Some(None));
        let empty: PatchWorkflow = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, PatchWorkflow::default());
        assert_eq!(
            serde_json::to_value(&patch).unwrap(),
            json!({ "name": "triage", "description": null })
        );
        assert!(serde_json::from_value::<PatchWorkflow>(json!({ "title": "x" })).is_err());
    }

    #[test]
    fn a_version_takes_its_suite_changes_beside_its_fields() {
        let version: CreateVersion = serde_json::from_value(json!({
            "code": "export default async () => 2;",
            "retire": [{ "names": ["a"], "reason": "gone" }],
        }))
        .unwrap();
        assert_eq!(version.retire.len(), 1);
        assert_eq!(
            interface(&version.input, &version.output, &version.signals),
            None
        );
    }
}
