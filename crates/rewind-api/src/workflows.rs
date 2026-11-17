//! Workflows: scripts with a name and JSON Schemas for their input, their output and the signals
//! they accept, that scripts start and signal (`archi/01-product.md`, "Workflows").

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};

use crate::common::{Labels, Timestamp};
use crate::toolsets::Declarations;

/// `POST /v1/workflows`: checks a workflow and saves it as a new version. Its script is checked
/// against its toolset and against its own schemas: its default export must take the input and
/// return the output, and `signal()` only takes the declared names. Saving the same workflow
/// again under the same labels returns the version saved first (200): the labels of a key limited
/// to labels are part of the version, so such a key saves a version of its own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({
    "name": "triage",
    "description": "Triage the stale issues of a repository, after approval.",
    "toolset": "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4",
    "script": "import { signal } from \"rewind\";\nexport default async function main(input: { repo: string }) {\n  const { close } = await signal(\"approval\");\n  return { closed: close ? 1 : 0 };\n}\n",
    "input": { "type": "object", "properties": { "repo": { "type": "string" } }, "required": ["repo"] },
    "output": { "type": "object", "properties": { "closed": { "type": "integer" } }, "required": ["closed"] },
    "signals": { "approval": { "type": "object", "properties": { "close": { "type": "boolean" } }, "required": ["close"] } }
}))]
pub struct CreateWorkflow {
    /// What scripts import from `rewind:workflows`: an identifier that is not a reserved word of
    /// JavaScript.
    pub name: String,
    /// Shown to the model in the declarations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The id of the toolset its runs call, already registered.
    pub toolset: String,
    /// The TypeScript source: a single ES module whose default export is the entry point.
    pub script: String,
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
}

/// A version of a workflow: immutable, identified by its digest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    pub name: String,
    /// The digest of the version (SHA-256, lowercase hex). `name@<version>` names it, as do its
    /// first 12 digits.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The id of the toolset its runs call.
    pub toolset: String,
    /// Digest of its script (`GET /v1/scripts/{digest}`).
    pub script: String,
    #[schema(value_type = Object)]
    pub input: Value,
    #[schema(value_type = Object)]
    pub output: Value,
    #[schema(value_type = Object)]
    pub signals: BTreeMap<String, Value>,
    /// The labels of the key that saved it, part of the version: a key limited to labels only
    /// sees the workflows carrying them. Empty for a version saved tenant-wide.
    pub labels: Labels,
    pub created_at: Timestamp,
    /// How a script that starts it sees it: its declaration in `rewind:workflows`, and its line
    /// of the prompt rendering.
    pub declarations: Declarations,
}

/// `GET /v1/workflows`: the latest version of each workflow, by name.
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
}

/// `GET /v1/workflows/{name}`: the versions of a workflow, newest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowVersions {
    pub name: String,
    pub versions: Vec<Workflow>,
}

/// A workflow a toolset pins.
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
}

/// The run that started a run, and the call that did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartedBy {
    pub run: String,
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
            "toolset": "00",
            "script": "export default async () => 1;",
            "input": {},
            "output": { "type": "integer" },
        }))
        .unwrap();
        assert!(workflow.signals.is_empty());
        assert!(
            serde_json::from_value::<CreateWorkflow>(json!({
                "name": "triage", "toolset": "00", "script": "", "input": {}, "output": {}, "x": 1,
            }))
            .is_err(),
            "unknown fields are refused"
        );
    }
}
