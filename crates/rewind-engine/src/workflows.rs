//! Workflows across runs (`archi/01-product.md`, "Workflows"; `archi/03-execution.md`,
//! "Workflows across runs").
//!
//! ```text
//! Edge::create_workflow ─── checks (name, schemas, code, tsgo), the gate ──► create_workflow
//! Edge::make_version ────── checks, the gate on the workflow's suite ─────► put_version
//! Edge::start_workflow ──── input checked against the version ────────────► the owner creates the run
//!
//! a code's workflows.<name>.start  ──► call executor ──► the child's owner creates it
//!                                                          (started_by, the call's key)
//!          workflows.<name>.signal ──► call executor ──► a signal to the target (the call's key)
//! the target's terminal commit ──────────────────────────► rewind:ended:<id> to its watchers
//! a fork's copied starts ──► the fork watches the runs they started, their end if it is there
//! ```
//!
//! In a test run, a start is answered by the test sources: their answer is the child's end, which
//! follows the start at once, and no child runs.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rewind_core::{
    Code, Command, Completion, Digest, Event, HistoryEvent, InvalidWorkflow, JsonValue, Payload,
    RunId, Seq, ServerSecrets, WorkflowOp, WorkflowSchemas, ended_run, parse_duration,
};

use crate::ports::{
    BlobError, CheckerError, Diagnostic, InvalidCode, Labels, NamespaceName, RunMetadata,
    SecretError, Severity, StoreError, StoredVersion, StoredWorkflow, TenantId, WorkflowScope,
};
use crate::schema::Refused;
use crate::secrets::{RefusedSecrets, SecretRights};
use crate::suites::{SuiteChanges, SuiteError, SuiteLoadError, TestInput};
use crate::testing::{TestError, TestReport};
use crate::text::NulCharacter;

/// The longest description a workflow takes, in characters (`archi/04-tools.md`, "Workflow
/// names").
pub const MAX_DESCRIPTION_CHARS: usize = 2_000;

/// Who writes a workflow: the labels of their grant, what they may use, and who they are.
#[derive(Debug, Clone)]
pub struct WorkflowMaker {
    /// The labels of the grant: a version made under them carries them, which only the
    /// principals that see them see. Empty for a tenant-wide grant.
    pub labels: Labels,
    /// What the maker may use: the secrets a version names are checked against it
    /// (`archi/04-tools.md`, "Secrets": a workflow version is the permission).
    pub secret_rights: SecretRights,
    /// A principal, as the audit log names it.
    pub principal: String,
}

/// A workflow to create, with its version 1 (`POST /v1/workflows`): in a namespace, or in the
/// organization.
#[derive(Debug, Clone)]
pub struct CreateWorkflow {
    pub tenant: TenantId,
    pub scope: WorkflowScope,
    pub name: Option<String>,
    pub description: Option<String>,
    pub code: Code,
    /// The action set its runs call, already registered.
    pub action_set: Digest,
    /// Its interface; without one, any input, output and signal name.
    pub interface: Option<WorkflowSchemas>,
    /// The secrets its runs use (`archi/04-tools.md`, "Secrets"): part of the version, checked
    /// against the action set and the maker's rights; an `ns/` secret only by its name, since a
    /// version has no namespace yet.
    pub secrets: ServerSecrets,
    /// Its first suite, through the gate.
    pub tests: Vec<TestInput>,
    pub maker: WorkflowMaker,
}

/// The next version of a workflow (`POST /v1/workflows/{id}/versions`, `update_workflow` given
/// code, an interface, an action set or secrets). What it leaves out is its latest version's.
#[derive(Debug, Clone)]
pub struct MakeVersion {
    pub tenant: TenantId,
    pub workflow: StoredWorkflow,
    pub code: Option<Code>,
    pub action_set: Option<Digest>,
    /// `None` keeps the latest version's interface.
    pub interface: Option<WorkflowSchemas>,
    pub secrets: Option<ServerSecrets>,
    pub suite: SuiteChanges,
    pub maker: WorkflowMaker,
}

/// Tests added to a workflow's suite, replaced or retired, without a version
/// (`POST /v1/workflows/{id}/tests`).
#[derive(Debug, Clone)]
pub struct ChangeTests {
    pub tenant: TenantId,
    pub workflow: StoredWorkflow,
    pub suite: SuiteChanges,
    pub maker: WorkflowMaker,
}

/// An organization's workflow forked into a namespace (`POST /v1/workflows/{id}/fork`).
#[derive(Debug, Clone)]
pub struct ForkWorkflow {
    pub tenant: TenantId,
    pub source: StoredWorkflow,
    /// The version forked: the source's latest the maker sees when `None`.
    pub version: Option<StoredVersion>,
    pub namespace: NamespaceName,
    pub maker: WorkflowMaker,
}

/// A fork taking its source's latest version (`POST /v1/workflows/{id}/pull`).
#[derive(Debug, Clone)]
pub struct PullWorkflow {
    pub tenant: TenantId,
    pub fork: StoredWorkflow,
    pub maker: WorkflowMaker,
}

/// A workflow written, and the version a call made or found: `created` when it made one.
#[derive(Debug, Clone, PartialEq)]
pub struct WrittenWorkflow {
    pub workflow: StoredWorkflow,
    pub version: StoredVersion,
    pub created: bool,
}

/// Why a write of a workflow was refused. The messages are written for a model.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WorkflowError {
    #[error(transparent)]
    Invalid(#[from] InvalidWorkflow),
    /// A schema cannot be compiled.
    #[error("the {which} schema cannot be used: {message}")]
    Schema { which: String, message: String },
    #[error(
        "the description is {0} characters, above the limit of {MAX_DESCRIPTION_CHARS}: shorten it"
    )]
    DescriptionTooLong(usize),
    /// Its scope already has a workflow of that name: 409 `name-taken`.
    #[error("{0}")]
    NameTaken(String),
    #[error("{0}")]
    NotFound(String),
    /// A namespace's workflow cannot be forked: 409 `not-forkable`.
    #[error("{0}")]
    NotForkable(String),
    /// Pulling into a workflow that is no fork: 409 `not-a-fork`.
    #[error("{0}")]
    NotAFork(String),
    #[error("action set {0} not found")]
    ActionSetNotFound(Digest),
    /// A secret is missing, takes no secret, names an organization's secret that does not
    /// exist or one the maker may not use: 422 `secret-required`.
    #[error("{0}")]
    SecretRequired(RefusedSecrets),
    /// The code cannot run: it does not compile, imports what does not exist…
    #[error("the code is invalid: {0}")]
    InvalidCode(InvalidCode),
    /// The code does not type-check against its action set and its own schemas.
    #[error("the code does not type-check: {}", first_error(.0))]
    TypeErrors(Vec<Diagnostic>),
    #[error(transparent)]
    Checker(#[from] CheckerError),
    #[error(transparent)]
    NulCharacter(#[from] NulCharacter),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    /// The suite cannot be composed: a name, a reason or a test is wrong.
    #[error(transparent)]
    Suite(#[from] SuiteError),
    /// A stored test cannot be read.
    #[error("{0}")]
    CorruptTest(String),
    /// Tests of the suite did not pass: nothing was stored. Every report, in the suite's
    /// order.
    #[error("{} of {} tests did not pass", not_passed(.0), .0.len())]
    TestsFailed(Vec<TestReport>),
    /// The tests could not run, for a reason that is not theirs.
    #[error(transparent)]
    Tests(#[from] TestError),
}

impl From<crate::ports::CatalogError> for WorkflowError {
    fn from(error: crate::ports::CatalogError) -> Self {
        WorkflowError::Store(error.into())
    }
}

fn not_passed(reports: &[TestReport]) -> usize {
    reports.iter().filter(|report| !report.passed).count()
}

impl From<SuiteLoadError> for WorkflowError {
    fn from(error: SuiteLoadError) -> Self {
        match error {
            SuiteLoadError::Suite(error) => WorkflowError::Suite(error),
            SuiteLoadError::Store(error) => WorkflowError::Store(error),
            SuiteLoadError::Blob(error) => WorkflowError::Blob(error),
            corrupt @ SuiteLoadError::Corrupt { .. } => {
                WorkflowError::CorruptTest(corrupt.to_string())
            }
        }
    }
}

/// A run of a workflow to start (`POST /v1/runs { workflow }`): its id or its name, `@v12` or
/// `@<digest>` for a version, else its latest.
#[derive(Debug, Clone)]
pub struct StartWorkflow {
    pub tenant: TenantId,
    /// The end user the run acts for (`archi/06-storage-and-scale.md`, "Namespaces"): a name
    /// resolves in it first.
    pub namespace: NamespaceName,
    pub reference: String,
    pub input: JsonValue,
    /// The run's TTL; the namespace's default
    /// ([`NodeConfig::default_ttl`](crate::plane::NodeConfig::default_ttl) within its bounds) when
    /// `None`.
    pub ttl: Option<Duration>,
    /// Its `workflow` is set from the version; its `allowed_labels` are the caller's, under
    /// which the reference resolves.
    pub metadata: RunMetadata,
    pub idempotency_key: Option<String>,
}

/// The runs started by the calls of `events` whose end `events` does not hold, with the `seq`
/// of their call.
pub(crate) fn started_runs(events: &[HistoryEvent]) -> Vec<(Seq, RunId)> {
    let mut starts = BTreeSet::new();
    let mut ended = BTreeSet::new();
    let mut runs = Vec::new();
    for entry in events {
        match &entry.event {
            Event::Command(Command::ScheduleCall { seq, action, .. })
                if matches!(WorkflowOp::of_action(action), Some((_, WorkflowOp::Start))) =>
            {
                starts.insert(*seq);
            }
            Event::Completion(Completion::CallCompleted {
                seq,
                result: Payload::Value(result),
            }) if starts.contains(seq) => {
                if let Some(run) = result
                    .get("runId")
                    .and_then(JsonValue::as_str)
                    .and_then(|run| run.parse::<RunId>().ok())
                {
                    runs.push((*seq, run));
                }
            }
            Event::Completion(Completion::SignalReceived { name, .. }) => {
                if let Some(run) = ended_run(name) {
                    ended.insert(run);
                }
            }
            _ => {}
        }
    }
    runs.retain(|(_, run)| !ended.contains(run));
    runs
}

/// Why a signal was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SignalRefused {
    #[error("signal names starting with \"rewind:\" are reserved for Rewind: {0:?}")]
    Reserved(String),
    /// The run is a workflow's that does not declare it.
    #[error("{0}")]
    Undeclared(String),
    /// The payload does not match the signal's schema.
    #[error("{0}")]
    Payload(String),
    #[error(transparent)]
    Store(StoreError),
}

/// The args of `workflows.<name>.start`, as the op checked them.
pub(crate) struct StartArgs {
    pub(crate) input: JsonValue,
    pub(crate) detached: bool,
    pub(crate) labels: BTreeMap<String, String>,
    pub(crate) ttl: Option<Duration>,
}

impl StartArgs {
    pub(crate) fn parse(args: &JsonValue) -> Result<Self, String> {
        let input = args
            .get("input")
            .cloned()
            .ok_or("the start of a workflow takes { input }")?;
        let detached = args
            .get("detached")
            .map(|detached| detached.as_bool().ok_or("detached is a boolean"))
            .transpose()?
            .unwrap_or(false);
        let labels = match args.get("labels") {
            None => BTreeMap::new(),
            Some(JsonValue::Object(labels)) => labels
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key.clone(), value.to_owned()))
                        .ok_or_else(|| format!("the label {key:?} is not a string"))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err("labels is an object of strings".into()),
        };
        let ttl = match args.get("ttl") {
            None => None,
            Some(JsonValue::Number(ms)) => Some(Duration::from_millis(
                ms.as_u64().ok_or("ttl is a whole number of milliseconds")?,
            )),
            Some(JsonValue::String(text)) => Some(
                parse_duration(text).map_err(|error| format!("invalid ttl {text:?}: {error}"))?,
            ),
            Some(_) => return Err("ttl is a duration".into()),
        };
        Ok(Self {
            input,
            detached,
            labels,
            ttl,
        })
    }
}

/// The args of `workflows.<name>.signal`, as the op checked them.
pub(crate) struct SignalArgs {
    pub(crate) run: RunId,
    pub(crate) name: String,
    pub(crate) payload: JsonValue,
}

impl SignalArgs {
    pub(crate) fn parse(args: &JsonValue) -> Result<Self, String> {
        let run = args
            .get("run")
            .and_then(JsonValue::as_str)
            .ok_or("the signal of a workflow run takes { run, name, payload }")?;
        let run = run
            .parse::<RunId>()
            .map_err(|_| format!("{run:?} is not a run id"))?;
        let name = args
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or("the signal's name is a string")?
            .to_owned();
        let payload = args.get("payload").cloned().unwrap_or(JsonValue::Null);
        Ok(Self { run, name, payload })
    }
}

/// A refused value, as a sentence: `{what} refuses it: …`.
pub(crate) fn refused_message(what: &str, refused: Refused) -> String {
    match refused {
        Refused::Unusable(error) => format!("{what} cannot be used: {error}"),
        Refused::Invalid(problems) => format!("{what} refuses it: {problems}"),
    }
}

fn first_error(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .find(|diagnostic| diagnostic.severity == Severity::Error)
        .map_or_else(String::new, ToString::to_string)
}

#[cfg(test)]
mod tests {
    use rewind_core::{ActionOutcome, Timestamp, end_signal_name};
    use serde_json::json;

    use super::*;

    fn entry(index: u64, event: Event) -> HistoryEvent {
        HistoryEvent {
            index,
            recorded_at: Timestamp(0),
            event,
        }
    }

    #[test]
    fn a_copy_lists_the_runs_it_started_whose_end_it_lacks() {
        let a: RunId = "0193a5c4-0000-7000-8000-000000000001".parse().unwrap();
        let b: RunId = "0193a5c4-0000-7000-8000-000000000002".parse().unwrap();
        let start = |seq: u32| {
            Event::Command(Command::ScheduleCall {
                seq: Seq(seq),
                action: "workflows.triage.start".into(),
                args: json!({ "input": {} }).into(),
                callsite: None,
            })
        };
        let started = |seq: u32, run: RunId| {
            Event::Completion(
                ActionOutcome::Result(json!({ "runId": run.to_string() }))
                    .into_completion(Seq(seq)),
            )
        };
        let events = [
            entry(1, start(1)),
            entry(2, start(2)),
            entry(3, started(1, a)),
            entry(4, started(2, b)),
            entry(
                5,
                Event::Completion(Completion::SignalReceived {
                    name: end_signal_name(a),
                    payload: json!({ "status": "completed", "output": 1 }).into(),
                }),
            ),
        ];
        assert_eq!(started_runs(&events), [(Seq(2), b)]);
    }

    #[test]
    fn start_args_are_read_as_the_op_wrote_them() {
        let args = StartArgs::parse(&json!({
            "input": { "repo": "a" },
            "detached": true,
            "labels": { "team": "core" },
            "ttl": "2d",
        }))
        .unwrap();
        assert!(args.detached);
        assert_eq!(args.ttl, Some(Duration::from_secs(2 * 86_400)));
        assert_eq!(args.labels.get("team").map(String::as_str), Some("core"));
        assert_eq!(
            StartArgs::parse(&json!({ "input": 1, "ttl": 1500 }))
                .unwrap()
                .ttl,
            Some(Duration::from_millis(1500))
        );
        assert!(StartArgs::parse(&json!({})).is_err());
        assert!(SignalArgs::parse(&json!({ "run": "nope", "name": "a" })).is_err());
    }
}
