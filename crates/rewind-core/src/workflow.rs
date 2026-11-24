//! Workflows: workflows with a name and a typed interface, that workflows start and signal
//! (`archi/01-product.md`, "Workflows"; `archi/03-execution.md`, "Workflows across runs").
//!
//! ```text
//! Workflow (a version: immutable, content-addressed)
//! ├── name, description?
//! ├── code, action set            digests: what its runs execute
//! ├── schemas                    { input, output, signals: { <name>: schema } }
//! ├── suite                      base?, tests, retired, replaced: the tests it passed
//! ├── labels                     the scope it was saved under; {} for a tenant-wide save
//! └── secrets                    its secrets: { <server>: org/<name> | ns/<name> }
//!
//! PinnedWorkflow (what an action set pins: the interface, without the code)
//! └── name, version (the Workflow's digest), description?, schemas
//! ```
//!
//! A code starts and signals runs of a pinned workflow through two built-ins, and the end of
//! a run comes back to the runs that watch it as the reserved signal `rewind:ended:<run id>`,
//! whose payload is a [`RunEnd`].

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value, json};

use crate::action_set::{is_identifier, is_reserved};
use crate::canonical::canonical_json;
use crate::digest::Digest;
use crate::event::{
    AbortKind, AbortReason, ActionFailure, ActionFailureKind, ActionOutcome, CancelReason,
    ErrorInfo,
};
use crate::ids::RunId;
use crate::payload::Payload;
use crate::secret::ServerSecrets;
use crate::state::RunOutcome;

/// Signal names starting with this prefix are Rewind's: no one else sends them, and no
/// workflow declares them (`archi/01-product.md`, "Signals").
pub const RESERVED_SIGNAL_PREFIX: &str = "rewind:";

/// The prefix of the signal that carries a run's end to the runs that watch it.
const ENDED_PREFIX: &str = "rewind:ended:";

/// The name of the signal that carries the end of `run`: `rewind:ended:<run id>`.
pub fn end_signal_name(run: RunId) -> String {
    format!("{ENDED_PREFIX}{run}")
}

/// The run whose end a signal named `name` carries, if it is such a signal.
pub fn ended_run(name: &str) -> Option<RunId> {
    name.strip_prefix(ENDED_PREFIX)?.parse().ok()
}

/// Why a run is cancelled when the run that started it attached ends
/// (`archi/03-execution.md`, "Workflows across runs").
pub fn starter_ended(starter: RunId) -> CancelReason {
    CancelReason::Requested(Some(format!("the run {starter} that started it ended")))
}

/// Whether a signal name is reserved for Rewind.
pub fn is_reserved_signal(name: &str) -> bool {
    name.starts_with(RESERVED_SIGNAL_PREFIX)
}

/// The JSON Schemas of a workflow: its input, its output, and each signal it accepts.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowSchemas {
    pub input: Value,
    pub output: Value,
    /// By signal name.
    pub signals: BTreeMap<String, Value>,
}

impl WorkflowSchemas {
    fn to_json(&self) -> Value {
        json!({ "input": self.input, "output": self.output, "signals": self.signals })
    }
}

/// A version of a workflow: a code with a name and a typed interface. Immutable and
/// content-addressed: its [`version`](Self::version) is the digest of its document, the workflow
/// version document of the stored format v1 (`archi/02-history.md`).
///
/// A version saved under a grant limited to labels carries them, and they are part of its
/// document: the same workflow saved under other labels, or tenant-wide, is another version,
/// which the other scopes do not see (`archi/06-storage-and-scale.md`, "Tenancy"). A version
/// without labels has the document, and so the digest, it had before labels were part of it.
///
/// A version also carries the secrets its runs use (`archi/04-tools.md`, "Secrets"): saved
/// with it and checked against the saver's rights, they are part of its document, and a run of
/// it reads them without the caller's rights being checked again. A version without secrets
/// has the document it had before secrets were part of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Workflow {
    name: String,
    description: Option<String>,
    code: Digest,
    action_set: Digest,
    schemas: WorkflowSchemas,
    suite: Suite,
    labels: BTreeMap<String, String>,
    secrets: ServerSecrets,
}

/// The tests a version passed when it was saved, and how they derive from its base's
/// (`archi/05-testing-and-observability.md`, "Suites and the gate"). Tests are referenced by
/// the digest of their document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Suite {
    /// The version it derives from.
    pub base: Option<Digest>,
    /// Every test of the suite, by name.
    pub tests: Vec<SuiteTest>,
    /// The base's tests it does not keep.
    pub retired: Vec<RetiredTest>,
    /// The base's tests it replaced.
    pub replaced: Vec<ReplacedTest>,
}

impl Suite {
    pub fn is_empty(&self) -> bool {
        self.base.is_none()
            && self.tests.is_empty()
            && self.retired.is_empty()
            && self.replaced.is_empty()
    }

    /// The digest of the test named `name`.
    pub fn test(&self, name: &str) -> Option<Digest> {
        self.tests
            .iter()
            .find(|test| test.name == name)
            .map(|test| test.test)
    }

    /// Sorts every list by name, so that the same suite always writes the same document, and
    /// checks that names are unique and reasons given.
    fn normalize(mut self) -> Result<Self, InvalidWorkflow> {
        self.tests.sort_by(|a, b| a.name.cmp(&b.name));
        self.retired.sort_by(|a, b| a.name.cmp(&b.name));
        self.replaced.sort_by(|a, b| a.name.cmp(&b.name));
        for pair in self.tests.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(InvalidWorkflow::DuplicateTest(pair[0].name.clone()));
            }
        }
        let reasons = self
            .retired
            .iter()
            .map(|retired| (&retired.name, &retired.reason))
            .chain(
                self.replaced
                    .iter()
                    .map(|replaced| (&replaced.name, &replaced.reason)),
            );
        for (name, reason) in reasons {
            if reason.trim().is_empty() {
                return Err(InvalidWorkflow::NoReason(name.clone()));
            }
        }
        Ok(self)
    }

    fn insert_into(&self, object: &mut Map<String, Value>) {
        if let Some(base) = &self.base {
            object.insert("base".into(), json!(base.to_hex()));
        }
        if !self.tests.is_empty() {
            let tests: Vec<Value> = self
                .tests
                .iter()
                .map(|test| json!({ "name": test.name, "test": test.test.to_hex() }))
                .collect();
            object.insert("tests".into(), Value::Array(tests));
        }
        if !self.retired.is_empty() {
            let retired: Vec<Value> = self
                .retired
                .iter()
                .map(|retired| {
                    json!({
                        "name": retired.name,
                        "test": retired.test.to_hex(),
                        "reason": retired.reason,
                    })
                })
                .collect();
            object.insert("retired".into(), Value::Array(retired));
        }
        if !self.replaced.is_empty() {
            let replaced: Vec<Value> = self
                .replaced
                .iter()
                .map(|replaced| {
                    json!({
                        "name": replaced.name,
                        "old": replaced.old.to_hex(),
                        "new": replaced.new.to_hex(),
                        "reason": replaced.reason,
                    })
                })
                .collect();
            object.insert("replaced".into(), Value::Array(replaced));
        }
    }
}

/// A test of a suite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuiteTest {
    pub name: String,
    pub test: Digest,
}

/// A test of the base a version does not keep, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetiredTest {
    pub name: String,
    pub test: Digest,
    pub reason: String,
}

/// A test of the base a version replaced, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplacedTest {
    pub name: String,
    pub old: Digest,
    pub new: Digest,
    pub reason: String,
}

/// Why a test of the base is left out without being retired by the version: its namespace
/// was erased with the end user's data.
pub const ERASED_TEST_REASON: &str = "recorded from an erased namespace";

/// A workflow that breaks an invariant. The messages are written for whoever registers it, a
/// harness developer or a model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidWorkflow {
    #[error(
        "invalid workflow name {0:?}: expected an identifier (letters, digits, _ and $) that is \
         not a reserved word of JavaScript, since workflows import it from rewind:workflows"
    )]
    Name(String),
    #[error("invalid signal name {0:?}: expected a name without control characters")]
    SignalName(String),
    #[error("invalid signal name {0:?}: names starting with \"rewind:\" are reserved")]
    ReservedSignalName(String),
    #[error("the {0} schema must be a JSON Schema: an object or a boolean")]
    Schema(String),
    #[error("the suite has two tests named {0:?}: names are unique in a suite")]
    DuplicateTest(String),
    #[error("the test {0:?} is retired or replaced without a reason: say why")]
    NoReason(String),
}

impl Workflow {
    /// A version of the workflow `name`, whose runs execute `code` with `action_set`.
    pub fn new(
        name: impl Into<String>,
        description: Option<String>,
        code: Digest,
        action_set: Digest,
        schemas: WorkflowSchemas,
    ) -> Result<Self, InvalidWorkflow> {
        let name = name.into();
        check_interface(&name, &schemas)?;
        Ok(Self {
            name,
            description,
            code,
            action_set,
            schemas,
            suite: Suite::default(),
            labels: BTreeMap::new(),
            secrets: ServerSecrets::new(),
        })
    }

    /// The same version with a suite.
    pub fn with_suite(mut self, suite: Suite) -> Result<Self, InvalidWorkflow> {
        self.suite = suite.normalize()?;
        Ok(self)
    }

    pub fn suite(&self) -> &Suite {
        &self.suite
    }

    /// The same version saved under `labels`: the scope of a grant limited to labels. Another
    /// version unless `labels` is empty.
    #[must_use]
    pub fn with_labels(mut self, labels: BTreeMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    /// The same version giving `secrets` to its runs. Another version unless `secrets` is
    /// empty.
    #[must_use]
    pub fn with_secrets(mut self, secrets: ServerSecrets) -> Self {
        self.secrets = secrets;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The secrets its runs use, by server or API name.
    pub fn secrets(&self) -> &ServerSecrets {
        &self.secrets
    }

    /// Digest of the code its runs execute.
    pub fn code(&self) -> Digest {
        self.code
    }

    /// Digest of the action set its runs call.
    pub fn action_set(&self) -> Digest {
        self.action_set
    }

    pub fn schemas(&self) -> &WorkflowSchemas {
        &self.schemas
    }

    /// The labels it was saved under: only the principals that see them see it.
    pub fn labels(&self) -> &BTreeMap<String, String> {
        &self.labels
    }

    /// The version: SHA-256 of the canonical workflow version document, `"v": 1` included. The
    /// document `rewind-format` writes hashes to it.
    pub fn version(&self) -> Digest {
        Digest::of(canonical_json(&self.document()).as_bytes())
    }

    /// What an action set pins of this version.
    pub fn pinned(&self) -> PinnedWorkflow {
        PinnedWorkflow {
            name: self.name.clone(),
            version: self.version(),
            description: self.description.clone(),
            schemas: self.schemas.clone(),
        }
    }

    /// The workflow version document v1, as a JSON value.
    fn document(&self) -> Value {
        let mut object = Map::new();
        object.insert("v".into(), json!(1));
        object.insert("name".into(), json!(self.name));
        if let Some(description) = &self.description {
            object.insert("description".into(), json!(description));
        }
        object.insert("code".into(), json!(self.code.to_hex()));
        object.insert("action_set".into(), json!(self.action_set.to_hex()));
        object.insert("schemas".into(), self.schemas.to_json());
        self.suite.insert_into(&mut object);
        if !self.labels.is_empty() {
            object.insert("labels".into(), json!(self.labels));
        }
        if !self.secrets.is_empty() {
            let secrets: Map<String, Value> = self
                .secrets
                .iter()
                .map(|(server, secret)| (server.to_owned(), json!(secret.to_string())))
                .collect();
            object.insert("secrets".into(), Value::Object(secrets));
        }
        Value::Object(object)
    }
}

impl fmt::Display for Workflow {
    /// `triage@9f2c4e1a07b3`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version().short())
    }
}

/// A workflow as an action set pins it: its name, version and interface. Workflows of that action set
/// start runs of exactly this version, and are checked against these schemas.
#[derive(Clone, Debug, PartialEq)]
pub struct PinnedWorkflow {
    pub name: String,
    /// The [`Workflow::version`] pinned.
    pub version: Digest,
    pub description: Option<String>,
    pub schemas: WorkflowSchemas,
}

impl PinnedWorkflow {
    /// Checks what [`Workflow::new`] checks of the interface.
    pub(crate) fn check(&self) -> Result<(), InvalidWorkflow> {
        check_interface(&self.name, &self.schemas)
    }

    /// The name of its built-in: `workflows.<name>.start` or `workflows.<name>.signal`.
    pub fn action_name(&self, op: WorkflowOp) -> String {
        format!("{WORKFLOWS_GROUP}.{}.{}", self.name, op.as_str())
    }

    /// Its event in the pinned action set document v1.
    pub(crate) fn document(&self) -> Value {
        let mut object = Map::new();
        object.insert("name".into(), json!(self.name));
        object.insert("version".into(), json!(self.version.to_hex()));
        if let Some(description) = &self.description {
            object.insert("description".into(), json!(description));
        }
        object.insert("schemas".into(), self.schemas.to_json());
        Value::Object(object)
    }
}

impl fmt::Display for PinnedWorkflow {
    /// `triage@9f2c4e1a07b3`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version.short())
    }
}

/// The namespace of the workflows' built-ins, reserved in every action set.
pub const WORKFLOWS_GROUP: &str = "workflows";

/// What a workflow's built-in does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WorkflowOp {
    /// `workflows.<name>.start`: starts a run, args `{ input, detached?, labels?, ttl? }`,
    /// result `{ runId }`.
    Start,
    /// `workflows.<name>.signal`: signals a run, args `{ run, name, payload }`, result `{}`.
    Signal,
}

impl WorkflowOp {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkflowOp::Start => "start",
            WorkflowOp::Signal => "signal",
        }
    }

    /// The workflow and the operation of a workflow's built-in, by its name:
    /// `workflows.triage.start` is `("triage", Start)`. The namespace is reserved, so no other
    /// action has such a name.
    pub fn of_action(action: &str) -> Option<(&str, WorkflowOp)> {
        let rest = action.strip_prefix(WORKFLOWS_GROUP)?.strip_prefix('.')?;
        let (workflow, op) = rest.rsplit_once('.')?;
        let op = match op {
            "start" => WorkflowOp::Start,
            "signal" => WorkflowOp::Signal,
            _ => return None,
        };
        is_identifier(workflow).then_some((workflow, op))
    }
}

fn check_interface(name: &str, schemas: &WorkflowSchemas) -> Result<(), InvalidWorkflow> {
    if !is_identifier(name) || is_reserved(name) {
        return Err(InvalidWorkflow::Name(name.to_owned()));
    }
    let is_schema = |schema: &Value| matches!(schema, Value::Object(_) | Value::Bool(_));
    if !is_schema(&schemas.input) {
        return Err(InvalidWorkflow::Schema("input".into()));
    }
    if !is_schema(&schemas.output) {
        return Err(InvalidWorkflow::Schema("output".into()));
    }
    for (signal, schema) in &schemas.signals {
        if signal.is_empty() || signal.chars().any(char::is_control) {
            return Err(InvalidWorkflow::SignalName(signal.clone()));
        }
        if is_reserved_signal(signal) {
            return Err(InvalidWorkflow::ReservedSignalName(signal.clone()));
        }
        if !is_schema(schema) {
            return Err(InvalidWorkflow::Schema(format!("{signal:?} signal")));
        }
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// The end of a run, as the runs that watch it receive it

/// How a run ended, as the payload of its `rewind:ended:<run id>` signal:
/// `{ "status": "completed", "output" }`, `{ "status": "failed" | "cancelled", "error" }` or
/// `{ "status": "aborted", "reason" }` (`archi/02-history.md`).
#[derive(Clone, Debug, PartialEq)]
pub enum RunEnd {
    Completed {
        output: Value,
    },
    Failed {
        error: ErrorInfo,
    },
    /// A failure with `CancelledError`.
    Cancelled {
        error: ErrorInfo,
    },
    Aborted {
        reason: AbortReason,
    },
}

impl RunEnd {
    /// The end of a run that ended so. `None` when its output is held by digest only: the
    /// caller loads its value first.
    pub fn from_outcome(outcome: &RunOutcome) -> Option<Self> {
        Some(match outcome {
            RunOutcome::Completed { output } => match output {
                Payload::Value(output) => RunEnd::Completed {
                    output: output.clone(),
                },
                Payload::Digest(_) => return None,
            },
            RunOutcome::Failed { error } if error.name == ErrorInfo::CANCELLED => {
                RunEnd::Cancelled {
                    error: error.clone(),
                }
            }
            RunOutcome::Failed { error } => RunEnd::Failed {
                error: error.clone(),
            },
            RunOutcome::Aborted { reason } => RunEnd::Aborted {
                reason: reason.clone(),
            },
        })
    }

    pub fn status(&self) -> &'static str {
        match self {
            RunEnd::Completed { .. } => "completed",
            RunEnd::Failed { .. } => "failed",
            RunEnd::Cancelled { .. } => "cancelled",
            RunEnd::Aborted { .. } => "aborted",
        }
    }

    /// The signal's payload.
    pub fn to_json(&self) -> Value {
        let error_json = |error: &ErrorInfo| {
            let mut object = Map::new();
            object.insert("name".into(), json!(error.name));
            object.insert("message".into(), json!(error.message));
            if let Some(stack) = &error.stack {
                object.insert("stack".into(), json!(stack));
            }
            Value::Object(object)
        };
        match self {
            RunEnd::Completed { output } => json!({ "status": "completed", "output": output }),
            RunEnd::Failed { error } | RunEnd::Cancelled { error } => {
                json!({ "status": self.status(), "error": error_json(error) })
            }
            RunEnd::Aborted { reason } => json!({
                "status": "aborted",
                "reason": { "kind": reason.kind.as_str(), "message": reason.message },
            }),
        }
    }

    /// What a cassette records for the start of a run that ended so, and what a test run makes
    /// of it again ([`from_test`](Self::from_test)): its output, or a failure that says how it
    /// ended.
    pub fn to_test(&self) -> ActionOutcome {
        let failure = |message: String| {
            ActionOutcome::Failure(ActionFailure {
                kind: ActionFailureKind::Error,
                message,
                data: None,
            })
        };
        match self {
            RunEnd::Completed { output } => ActionOutcome::Result(output.clone()),
            RunEnd::Failed { error } => failure(format!("{}: {}", error.name, error.message)),
            RunEnd::Cancelled { error } => {
                failure(format!("cancelled: {}: {}", error.name, error.message))
            }
            RunEnd::Aborted { reason } => failure(format!("aborted: {reason}")),
        }
    }

    /// The end of a run that test runs answered in place of starting it: a result is its
    /// output, a failure its error (`archi/03-execution.md`, "Workflows across runs").
    pub fn from_test(outcome: ActionOutcome) -> Self {
        match outcome {
            ActionOutcome::Result(output) => RunEnd::Completed { output },
            ActionOutcome::Failure(failure) => RunEnd::Failed {
                error: ErrorInfo {
                    name: "Error".into(),
                    message: failure.message,
                    stack: None,
                },
            },
        }
    }

    /// Reads a signal's payload written by [`to_json`](Self::to_json).
    pub fn from_json(value: &Value) -> Option<Self> {
        let error = |value: &Value| {
            Some(ErrorInfo {
                name: value.get("name")?.as_str()?.to_owned(),
                message: value.get("message")?.as_str()?.to_owned(),
                stack: value
                    .get("stack")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        };
        match value.get("status")?.as_str()? {
            "completed" => Some(RunEnd::Completed {
                output: value.get("output")?.clone(),
            }),
            "failed" => Some(RunEnd::Failed {
                error: error(value.get("error")?)?,
            }),
            "cancelled" => Some(RunEnd::Cancelled {
                error: error(value.get("error")?)?,
            }),
            "aborted" => {
                let reason = value.get("reason")?;
                Some(RunEnd::Aborted {
                    reason: AbortReason {
                        kind: AbortKind::parse(reason.get("kind")?.as_str()?)?,
                        message: reason.get("message")?.as_str()?.to_owned(),
                    },
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schemas() -> WorkflowSchemas {
        WorkflowSchemas {
            input: json!({ "type": "object", "properties": { "repo": { "type": "string" } } }),
            output: json!({ "type": "object" }),
            signals: BTreeMap::from([("approval".to_owned(), json!({ "type": "object" }))]),
        }
    }

    fn triage() -> Workflow {
        Workflow::new(
            "triage",
            Some("Triage the stale issues".into()),
            Digest::of(b"script"),
            Digest::of(b"action_set"),
            schemas(),
        )
        .unwrap()
    }

    #[test]
    fn the_version_is_the_digest_of_the_document() {
        let workflow = triage();
        let document = format!(
            concat!(
                r#"{{"action_set":"{}","code":"{}","description":"Triage the stale issues","#,
                r#""name":"triage","schemas":{{"input":{{"properties":{{"repo":{{"type":"string"}}}},"#,
                r#""type":"object"}},"output":{{"type":"object"}},"signals":{{"approval":{{"type":"object"}}}}}},"#,
                r#""v":1}}"#
            ),
            Digest::of(b"action_set").to_hex(),
            Digest::of(b"script").to_hex(),
        );
        assert_eq!(workflow.version(), Digest::of(document.as_bytes()));
        assert_eq!(
            workflow.to_string(),
            format!("triage@{}", workflow.version().short())
        );
        let pinned = workflow.pinned();
        assert_eq!(pinned.version, workflow.version());
        assert_eq!(
            pinned.action_name(WorkflowOp::Start),
            "workflows.triage.start"
        );
    }

    #[test]
    fn secrets_are_part_of_the_version_only_when_there_are_some() {
        let base = triage();
        assert_eq!(
            base.clone().with_secrets(ServerSecrets::new()).version(),
            base.version(),
            "no secrets: the digest of the document without them"
        );
        let attach = |secret: &str| {
            let secret = crate::SecretRef::parse(secret).unwrap();
            triage().with_secrets([("github".to_owned(), secret)].into_iter().collect())
        };
        let of_user = attach("ns/github");
        let of_org = attach("org/github");
        assert_ne!(of_user.version(), base.version());
        assert_ne!(of_user.version(), of_org.version());
        assert_eq!(
            of_user.secrets().get("github").map(ToString::to_string),
            Some("ns/github".to_owned())
        );
        assert_eq!(
            canonical_json(&of_user.document()["secrets"]),
            r#"{"github":"ns/github"}"#
        );
    }

    #[test]
    fn labels_are_part_of_the_version_only_when_there_are_some() {
        let tenant_wide = triage();
        assert_eq!(
            tenant_wide.clone().with_labels(BTreeMap::new()).version(),
            tenant_wide.version(),
            "no labels: the digest of the document without them"
        );
        let label = |key: &str, value: &str| BTreeMap::from([(key.to_owned(), value.to_owned())]);
        let dev = triage().with_labels(label("env", "dev"));
        let prod = triage().with_labels(label("env", "prod"));
        assert_ne!(dev.version(), tenant_wide.version());
        assert_ne!(dev.version(), prod.version());
        assert_eq!(dev.labels(), &label("env", "dev"));
        assert_eq!(dev.pinned().version, dev.version());
    }

    #[test]
    fn every_field_changes_the_version() {
        let base = triage().version();
        let with = |name: &str, description: Option<&str>, code: &[u8], signals: &[&str]| {
            let mut schemas = schemas();
            schemas.signals = signals
                .iter()
                .map(|name| ((*name).to_owned(), json!({})))
                .collect();
            Workflow::new(
                name,
                description.map(str::to_owned),
                Digest::of(code),
                Digest::of(b"action_set"),
                schemas,
            )
            .unwrap()
            .version()
        };
        let versions = [
            base,
            with(
                "other",
                Some("Triage the stale issues"),
                b"script",
                &["approval"],
            ),
            with("triage", None, b"script", &["approval"]),
            with(
                "triage",
                Some("Triage the stale issues"),
                b"other",
                &["approval"],
            ),
            with("triage", Some("Triage the stale issues"), b"script", &[]),
        ];
        let distinct: std::collections::BTreeSet<_> = versions.iter().collect();
        assert_eq!(distinct.len(), versions.len());
    }

    #[test]
    fn names_and_schemas_are_checked() {
        let workflow = |name: &str, schemas: WorkflowSchemas| {
            Workflow::new(name, None, Digest::of(b"s"), Digest::of(b"t"), schemas)
        };
        for name in ["", "1st", "my-flow", "default", "eval", "a.b"] {
            assert_eq!(
                workflow(name, schemas()),
                Err(InvalidWorkflow::Name(name.into())),
                "{name}"
            );
        }
        let mut reserved = schemas();
        reserved.signals.insert("rewind:ended:x".into(), json!({}));
        assert!(matches!(
            workflow("triage", reserved),
            Err(InvalidWorkflow::ReservedSignalName(_))
        ));
        let mut control = schemas();
        control.signals.insert("a\nb".into(), json!({}));
        assert!(matches!(
            workflow("triage", control),
            Err(InvalidWorkflow::SignalName(_))
        ));
        let mut not_a_schema = schemas();
        not_a_schema.output = json!("string");
        assert_eq!(
            workflow("triage", not_a_schema),
            Err(InvalidWorkflow::Schema("output".into()))
        );
        let mut any = schemas();
        any.input = json!(true);
        assert!(workflow("$flow_2", any).is_ok());
    }

    #[test]
    fn builtins_are_found_by_name() {
        assert_eq!(
            WorkflowOp::of_action("workflows.triage.start"),
            Some(("triage", WorkflowOp::Start))
        );
        assert_eq!(
            WorkflowOp::of_action("workflows.triage.signal"),
            Some(("triage", WorkflowOp::Signal))
        );
        for action in [
            "workflows.triage",
            "workflows.triage.stop",
            "workflows.a.b.start",
            "github.triage.start",
            "workflows..start",
        ] {
            assert_eq!(WorkflowOp::of_action(action), None, "{action}");
        }
    }

    #[test]
    fn test_outcomes_are_ends() {
        let end = RunEnd::from_test(ActionOutcome::Result(json!({ "closed": 1 })));
        assert_eq!(
            end,
            RunEnd::Completed {
                output: json!({ "closed": 1 })
            }
        );
        assert_eq!(end.to_test(), ActionOutcome::Result(json!({ "closed": 1 })));
        let failed = RunEnd::Failed {
            error: ErrorInfo {
                name: "TypeError".into(),
                message: "x".into(),
                stack: None,
            },
        };
        let ActionOutcome::Failure(failure) = failed.to_test() else {
            panic!("a failed run is a failure");
        };
        assert_eq!(failure.message, "TypeError: x");
        assert_eq!(
            RunEnd::from_test(ActionOutcome::Failure(failure)).status(),
            "failed"
        );
    }

    #[test]
    fn end_signals_name_their_run() {
        let run: RunId = "0193a5c4-0000-7000-8000-000000000001".parse().unwrap();
        let name = end_signal_name(run);
        assert_eq!(name, format!("rewind:ended:{run}"));
        assert_eq!(ended_run(&name), Some(run));
        assert_eq!(ended_run("approval"), None);
        assert_eq!(ended_run("rewind:ended:nope"), None);
        assert!(is_reserved_signal(&name));
        assert!(!is_reserved_signal("approval"));
    }

    #[test]
    fn ends_round_trip_through_their_payload() {
        let error = ErrorInfo {
            name: "TypeError".into(),
            message: "x is undefined".into(),
            stack: Some("at main (workflow.ts:3:7)".into()),
        };
        let cancelled = ErrorInfo {
            name: ErrorInfo::CANCELLED.into(),
            message: "the run was cancelled".into(),
            stack: None,
        };
        let ends = [
            RunEnd::Completed {
                output: json!({ "closed": 3 }),
            },
            RunEnd::Failed {
                error: error.clone(),
            },
            RunEnd::Cancelled {
                error: cancelled.clone(),
            },
            RunEnd::Aborted {
                reason: AbortReason {
                    kind: AbortKind::LimitExceeded,
                    message: "cpu".into(),
                },
            },
        ];
        for end in &ends {
            assert_eq!(RunEnd::from_json(&end.to_json()).as_ref(), Some(end));
        }
        assert_eq!(
            ends[0].to_json(),
            json!({ "status": "completed", "output": { "closed": 3 } })
        );
        assert_eq!(
            RunEnd::from_outcome(&RunOutcome::Failed { error: cancelled }).map(|end| end.status()),
            Some("cancelled")
        );
        assert_eq!(
            RunEnd::from_outcome(&RunOutcome::Failed { error }).map(|end| end.status()),
            Some("failed")
        );
        assert_eq!(
            RunEnd::from_outcome(&RunOutcome::Completed {
                output: Payload::Digest(Digest::of(b"x"))
            }),
            None
        );
        assert_eq!(RunEnd::from_json(&json!({ "status": "lost" })), None);
    }
}
