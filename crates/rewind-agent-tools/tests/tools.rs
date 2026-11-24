//! The SDK's set over a fake API: what each tool sends to `/v1`, how a refusal the model can fix
//! becomes a result with `is_error` and a fault an error, the labels' scope, and how a workflow
//! is run, checked, tested, read and changed by its id or its name.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rewind_agent_tools::{
    AgentError, AgentOptions, AgentTools, Api, ApiError, CodeVerdict, HandleOptions,
};
use rewind_api::action_sets::{ActionSearchResults, SearchQuery};
use rewind_api::checks::{CheckRequest, CheckResult, TestRequest};
use rewind_api::common::Duration;
use rewind_api::events::HistoryEvent;
use rewind_api::headers::IdempotencyKey;
use rewind_api::runs::{
    CancelRun, ForkRun, Run, RunFilter, RunPage, SignalAccepted, StartRun, Transcript,
};
use rewind_api::secrets::SecretList;
use rewind_api::testing::{Test, TestReport};
use rewind_api::workflows::{
    ChangeTests, CreateVersion, PatchWorkflow, RecordTests, RecordedTests, Workflow,
};
use rewind_api::{Problem, ProblemType};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const CODE: &str = "export default async () => 1;";

/// The workflow of the fake API: `triage`, at its version 3.
const ID: &str = "wf_7k2m9q4x8d1c";

fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A run of the fake API: its handle is `alice/wf-7k2m9q/<id>`, a run of `workflow`.
fn run(id: &str, labels: Value, code: &str, status: &str, workflow: Value) -> Run {
    serde_json::from_value(json!({
        "id": id, "handle": format!("alice/wf-7k2m9q/{id}"), "namespace": "alice",
        "status": status, "codeDigest": code, "actionSet": "t",
        "epoch": 1, "mode": "live", "labels": labels, "traceId": "0", "historyLen": 3,
        "createdAt": "2026-11-19T10:00:00Z", "updatedAt": "2026-11-19T10:00:01Z",
        "expiresAt": "2026-12-19T10:00:00Z",
        "output": { "digest": "d", "bytes": 1, "preview": "1", "value": 1 },
        "workflow": workflow
    }))
    .unwrap()
}

/// A workflow without a name, which a code's first run made.
fn unnamed(code: &str) -> Value {
    json!({ "id": "wf_c0de00000000", "version": 1, "digest": sha256(code) })
}

/// The version `number` of `triage`, with an interface.
fn version(number: u32, tests: usize) -> Value {
    json!({
        "version": number,
        "digest": format!("{number:02}").repeat(32),
        "codeDigest": sha256(CODE),
        "actionSet": "t",
        "input": { "type": "object" },
        "output": { "type": "integer" },
        "signals": { "approval": { "type": "object" } },
        "labels": { "agent": "triage" },
        "createdAt": "2026-11-19T10:00:00Z",
        "tests": (0..tests).map(|n| json!({ "name": format!("test {n}"), "digest": format!("{n:02}").repeat(32) })).collect::<Vec<_>>()
    })
}

/// `triage`, of the namespace alice, its versions `numbers` newest first, `name` its name.
fn workflow(name: Option<&str>, numbers: &[u32], suite: Value) -> Workflow {
    let versions: Vec<Value> = numbers.iter().map(|number| version(*number, 1)).collect();
    serde_json::from_value(json!({
        "id": ID, "shortId": "wf-7k2m9q", "name": name, "namespace": "alice",
        "labels": { "agent": "triage" }, "createdAt": "2026-11-19T10:00:00Z",
        "version": versions[0], "code": CODE, "versions": versions, "suite": suite
    }))
    .unwrap()
}

/// Its suite: one test, and one that left it.
fn suite() -> Value {
    json!([
        { "name": "test 0", "digest": "00".repeat(32), "addedAt": "2026-11-19T10:00:00Z" },
        {
            "name": "asks first", "digest": "ee".repeat(32), "addedAt": "2026-11-18T10:00:00Z",
            "retiredAt": "2026-11-19T10:00:00Z", "reason": "it no longer asks"
        }
    ])
}

/// A request the fake API saw: the operation and what it was given.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    operation: &'static str,
    body: Value,
    key: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    seen: Mutex<Vec<Seen>>,
}

/// The fake API: what it saw, shared with the test.
#[derive(Debug, Clone, Default)]
struct Fake(Arc<State>);

impl Fake {
    fn saw(
        &self,
        operation: &'static str,
        body: impl serde::Serialize,
        key: Option<&IdempotencyKey>,
    ) {
        self.0.seen.lock().unwrap().push(Seen {
            operation,
            body: serde_json::to_value(body).unwrap(),
            key: key.map(|key| key.as_str().to_owned()),
        });
    }

    fn seen(&self) -> Vec<Seen> {
        self.0.seen.lock().unwrap().clone()
    }

    fn operations(&self) -> Vec<&'static str> {
        self.seen().iter().map(|seen| seen.operation).collect()
    }

    fn body(&self, operation: &str) -> Value {
        self.seen()
            .into_iter()
            .find(|seen| seen.operation == operation)
            .unwrap()
            .body
    }
}

fn problem(kind: ProblemType, detail: &str) -> ApiError {
    Problem::new(kind, detail).into()
}

/// The gate's refusal: one test of the suite did not pass.
fn tests_failed(detail: &str) -> ApiError {
    let mut problem = Problem::new(ProblemType::TestsFailed, detail);
    problem.tests = serde_json::from_value(json!([{
        "name": "test 0", "passed": false, "problem": "failed", "status": "completed",
        "output": 2, "calls": {}, "failures": ["the output is 2, expected 1"], "transcript": ""
    }]))
    .unwrap();
    problem.into()
}

impl Api for Fake {
    async fn start_run(
        &self,
        start: &StartRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, ApiError> {
        self.saw("start_run", start, key);
        let labels = serde_json::to_value(&start.labels).unwrap();
        if let Some(workflow) = &start.workflow {
            // A key seen before: the run of another workflow.
            let name = if workflow == "reused" {
                "other"
            } else {
                "triage"
            };
            return Ok(run(
                "r1",
                labels,
                &sha256(CODE),
                "completed",
                json!({ "id": ID, "name": name, "version": 3, "digest": "03".repeat(32) }),
            ));
        }
        if start.code.contains("refused") {
            return Err(Problem::new(
                ProblemType::InvalidCode,
                "the code does not type-check (1 error): fix the diagnostics and start it again",
            )
            .into());
        }
        if start.code.contains("busy") {
            return Err(problem(
                ProblemType::Unavailable,
                "the store cannot be reached: retry later",
            ));
        }
        if start.code.contains("quota") {
            return Err(problem(
                ProblemType::QuotaExceeded,
                "this namespace has 10 of its 10 active runs",
            ));
        }
        // A key seen before: the run of another code.
        let code = if start.code.contains("reused") {
            sha256("other")
        } else {
            sha256(&start.code)
        };
        Ok(run("r1", labels, &code, "completed", unnamed(&start.code)))
    }

    async fn get_run(&self, id: &str, wait: Option<Duration>) -> Result<Run, ApiError> {
        self.saw("get_run", json!({ "id": id, "wait": wait }), None);
        match id {
            "unlabelled" => Ok(run(id, json!({}), "s", "failed", Value::Null)),
            "missing" => Err(problem(
                ProblemType::NotFound,
                "no run missing: check the id (runs are listed by GET /v1/runs)",
            )),
            // A handle, `alice/wf-7k2m9q/<id>`, names the run `<id>`.
            _ => Ok(run(
                id.rsplit('/').next().unwrap_or(id),
                json!({ "agent": "triage" }),
                "s",
                "failed",
                json!({ "id": ID, "name": "triage", "version": 3, "digest": "03".repeat(32) }),
            )),
        }
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<RunPage, ApiError> {
        self.saw("list_runs", filter, None);
        Ok(RunPage {
            runs: Vec::new(),
            next_cursor: None,
        })
    }

    async fn run_transcript(&self, run: &str) -> Result<Transcript, ApiError> {
        self.saw("run_transcript", json!({ "run": run }), None);
        Ok(Transcript {
            run_id: run.into(),
            text: "[0] run_started".into(),
        })
    }

    async fn run_event(&self, _run: &str, _idx: u64) -> Result<HistoryEvent, ApiError> {
        Err(ApiError::fault("unused"))
    }

    async fn signal_run(
        &self,
        run: &str,
        name: &str,
        payload: &Value,
        key: Option<&IdempotencyKey>,
    ) -> Result<SignalAccepted, ApiError> {
        self.saw(
            "signal_run",
            json!({ "run": run, "name": name, "payload": payload }),
            key,
        );
        if name == "nope" {
            return Err(problem(
                ProblemType::InvalidSignal,
                "the workflow declares no signal \"nope\"",
            ));
        }
        Ok(SignalAccepted {
            run_id: run.into(),
            name: name.into(),
            duplicate: false,
        })
    }

    async fn cancel_run(&self, run: &str, cancel: &CancelRun) -> Result<Run, ApiError> {
        self.saw("cancel_run", cancel, None);
        Ok(self::run(
            run,
            json!({ "agent": "triage" }),
            "s",
            "cancelled",
            Value::Null,
        ))
    }

    async fn fork_run(
        &self,
        run: &str,
        fork: &ForkRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, ApiError> {
        self.saw("fork_run", fork, key);
        if fork.code.contains("regression") {
            return Err(tests_failed(
                "1 test of the suite of triage did not pass against the corrected code",
            ));
        }
        let mut forked = self::run(
            "r2",
            json!({ "agent": "triage" }),
            &sha256(&fork.code),
            "completed",
            unnamed(&fork.code),
        );
        forked.source = Some(rewind_api::runs::ForkSource {
            run: run.into(),
            handle: format!("alice/wf-7k2m9q/{run}"),
            at: None,
        });
        Ok(forked)
    }

    async fn check(&self, check: &CheckRequest) -> Result<CheckResult, ApiError> {
        self.saw("check", check, None);
        // A code a workflow of the namespace already has.
        let workflow = check.code.contains("known").then(|| {
            serde_json::from_value(
                json!({ "id": ID, "name": "triage", "version": 3, "digest": "03".repeat(32) }),
            )
            .unwrap()
        });
        Ok(CheckResult {
            ok: true,
            diagnostics: Vec::new(),
            workflow,
        })
    }

    async fn test(&self, test: &TestRequest) -> Result<TestReport, ApiError> {
        self.saw("test", test, None);
        if test.code.is_none() {
            return Ok(TestReport {
                passed: true,
                tests: Vec::new(),
            });
        }
        Err(problem(
            ProblemType::InvalidTest,
            "test \"a\": `expect` is missing",
        ))
    }

    async fn get_payload(&self, _digest: &str, _run: &str) -> Result<Value, ApiError> {
        Err(ApiError::fault("unused"))
    }

    async fn get_test(&self, digest: &str) -> Result<Test, ApiError> {
        self.saw("get_test", json!({ "digest": digest }), None);
        Ok(serde_json::from_value(json!({
            "name": "test 0", "input": { "repo": "acme/api" }, "expect": { "output": 1 }
        }))
        .unwrap())
    }

    async fn search_actions(
        &self,
        action_set: &str,
        query: &SearchQuery,
    ) -> Result<ActionSearchResults, ApiError> {
        self.saw(
            "search_actions",
            json!({ "action_set": action_set, "query": query.query }),
            None,
        );
        Ok(ActionSearchResults {
            actions: Vec::new(),
        })
    }

    async fn get_workflow(&self, workflow: &str) -> Result<Workflow, ApiError> {
        self.saw("get_workflow", json!({ "workflow": workflow }), None);
        if workflow == "missing" {
            return Err(problem(ProblemType::NotFound, "no workflow missing"));
        }
        Ok(self::workflow(Some("triage"), &[3, 2, 1], suite()))
    }

    async fn update_workflow(
        &self,
        workflow: &str,
        patch: &PatchWorkflow,
    ) -> Result<Workflow, ApiError> {
        self.saw(
            "update_workflow",
            json!({ "workflow": workflow, "patch": patch }),
            None,
        );
        if patch.name == Some(Some("taken".into())) {
            return Err(problem(
                ProblemType::NameTaken,
                "the namespace alice already has a workflow taken",
            ));
        }
        let name = match &patch.name {
            Some(name) => name.clone(),
            None => Some("triage".to_owned()),
        };
        Ok(self::workflow(name.as_deref(), &[3, 2, 1], suite()))
    }

    async fn create_version(
        &self,
        workflow: &str,
        version: &CreateVersion,
    ) -> Result<Workflow, ApiError> {
        self.saw(
            "create_version",
            json!({ "workflow": workflow, "version": version }),
            None,
        );
        let code = version.code.as_deref().unwrap_or_default();
        if code.contains("refused") {
            return Err(Problem::new(
                ProblemType::InvalidCode,
                "the code does not type-check (1 error)",
            )
            .into());
        }
        if code.contains("regression") {
            return Err(tests_failed(
                "1 test of 1 did not pass: no version was made",
            ));
        }
        if code.contains("secret") {
            return Err(problem(
                ProblemType::SecretRequired,
                "github needs a secret: name one in secrets",
            ));
        }
        // The same content: the latest version, unchanged.
        let numbers: &[u32] = if code == CODE {
            &[3, 2, 1]
        } else {
            &[4, 3, 2, 1]
        };
        Ok(self::workflow(Some("triage"), numbers, suite()))
    }

    async fn change_tests(
        &self,
        workflow: &str,
        tests: &ChangeTests,
    ) -> Result<Workflow, ApiError> {
        self.saw(
            "change_tests",
            json!({ "workflow": workflow, "tests": tests }),
            None,
        );
        if tests
            .tests
            .iter()
            .any(|test| serde_json::to_value(test).unwrap()["name"] == "fails against v3")
        {
            return Err(tests_failed("1 test did not pass against the version v3"));
        }
        let mut suite = suite();
        suite.as_array_mut().unwrap().push(json!({
            "name": "test 1", "digest": "01".repeat(32), "addedAt": "2026-11-20T10:00:00Z"
        }));
        Ok(self::workflow(Some("triage"), &[3, 2, 1], suite))
    }

    async fn record_tests(
        &self,
        workflow: &str,
        record: &RecordTests,
    ) -> Result<RecordedTests, ApiError> {
        self.saw(
            "record_tests",
            json!({ "workflow": workflow, "record": record }),
            None,
        );
        Ok(serde_json::from_value(json!({
            "tests": [{
                "digest": "9f".repeat(32), "name": "recorded r1", "path": "→ completed",
                "run": "r1", "status": "completed"
            }],
            "skipped": []
        }))
        .unwrap())
    }

    async fn list_secrets(&self) -> Result<SecretList, ApiError> {
        self.saw("list_secrets", json!({}), None);
        Ok(secrets(&[
            ("org/zendesk", Some("prod")),
            ("org/linear", None),
        ]))
    }

    async fn namespace_secrets(&self, namespace: &str) -> Result<SecretList, ApiError> {
        self.saw("namespace_secrets", json!({ "namespace": namespace }), None);
        Ok(secrets(&[("ns/github", None)]))
    }

    fn namespace(&self) -> Option<String> {
        Some("alice".into())
    }
}

/// Secrets as the API lists them, with an `env` label for some.
fn secrets(references: &[(&str, Option<&str>)]) -> SecretList {
    let secrets = references
        .iter()
        .map(|(reference, env)| {
            let (scope, name) = reference.split_once('/').unwrap();
            let mut secret = json!({
                "name": name, "scope": scope, "reference": reference,
                "createdBy": "member:u", "createdAt": "2026-11-19T10:00:00Z",
                "updatedBy": "member:u", "updatedAt": "2026-11-19T10:00:00Z", "reads": []
            });
            if scope == "ns" {
                secret["namespace"] = json!("alice");
            }
            if let Some(env) = env {
                secret["labels"] = json!({ "env": env });
            }
            serde_json::from_value(secret).unwrap()
        })
        .collect();
    SecretList { secrets }
}

fn tools(action_set: Option<&str>) -> (AgentTools<Fake>, Fake) {
    let fake = Fake::default();
    let options = AgentOptions {
        action_set: action_set.map(str::to_owned),
        labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
        ..AgentOptions::default()
    };
    (AgentTools::new(fake.clone(), options).unwrap(), fake)
}

fn call(id: &str) -> HandleOptions {
    HandleOptions {
        tool_call_id: Some(id.to_owned()),
        ..HandleOptions::default()
    }
}

#[tokio::test]
async fn run_workflow_starts_code_with_the_agent_s_action_set_labels_wait_and_key() {
    let (tools, fake) = tools(Some("ts1"));
    let result = tools
        .handle(
            "run_workflow",
            &json!({ "code": CODE, "wait": "1h" }),
            &call("call_0"),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.code, Some(CodeVerdict::Passed));
    // The result names the workflow the code became, by its id and its version.
    assert!(
        result.content.starts_with(
            "Run alice/wf-7k2m9q/r1 (a run of the workflow wf_c0de00000000@v1) completed"
        ),
        "{}",
        result.content
    );
    assert_eq!(result.run.unwrap().id, "r1");
    let seen = fake.seen();
    assert_eq!(seen[0].operation, "start_run");
    assert_eq!(
        seen[0].body,
        json!({
            "code": CODE, "actionSet": "ts1", "input": {}, "wait": "2m",
            "labels": { "agent": "triage" }
        })
    );
    // The SDK's key: the SHA-256 of [labels, call id].
    assert_eq!(
        seen[0].key.as_deref(),
        rewind_agent_tools::run_key(
            &BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
            Some("call_0")
        )
        .as_ref()
        .map(IdempotencyKey::as_str)
    );
}

#[tokio::test]
async fn run_workflow_runs_a_workflow_by_its_name_with_its_version_s_action_set() {
    let (tools, fake) = tools(Some("ts1"));
    let result = tools
        .handle(
            "run_workflow",
            &json!({ "workflow": "triage@v3", "input": { "repo": "acme/api" } }),
            &call("call_1"),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.code, None, "no code was handed in");
    assert!(
        result.content.starts_with(&format!(
            "Run alice/wf-7k2m9q/r1 (a run of the workflow triage@v3, id {ID}) completed."
        )),
        "{}",
        result.content
    );
    assert_eq!(
        fake.body("start_run"),
        json!({
            "workflow": "triage@v3", "input": { "repo": "acme/api" }, "wait": "30s",
            "labels": { "agent": "triage" }
        })
    );

    let mistakes = [
        (
            json!({ "code": CODE, "workflow": "triage" }),
            "run_workflow: give `code` or `workflow`, not both: `code` runs new code, `workflow` an existing workflow by its id or its name.",
        ),
        (
            json!({ "input": {} }),
            "run_workflow: give the `code` to run, or a `workflow` by its id or its name.",
        ),
        (
            json!({ "workflow": "triage", "secrets": { "github": "ns/github" } }),
            "run_workflow: `secrets` goes with `code`: a run of a workflow uses the secrets its version names.",
        ),
    ];
    for (input, said) in mistakes {
        let refused = tools
            .handle("run_workflow", &input, &call("c"))
            .await
            .unwrap();
        assert!(refused.is_error);
        assert_eq!(refused.content, said);
    }
    assert_eq!(fake.operations(), ["start_run"], "nothing else was sent");
}

#[tokio::test]
async fn refusals_are_results_and_faults_are_errors() {
    let (tools, _) = tools(Some("ts1"));
    let refused = tools
        .handle(
            "run_workflow",
            &json!({ "code": "// refused" }),
            &call("c1"),
        )
        .await
        .unwrap();
    assert!(refused.is_error);
    assert_eq!(refused.code, Some(CodeVerdict::Refused));
    assert_eq!(
        refused.content,
        "The code was refused before anything ran: the code does not type-check (1 error): fix the diagnostics and start it again"
    );

    let quota = tools
        .handle("run_workflow", &json!({ "code": "// quota" }), &call("c2"))
        .await
        .unwrap();
    assert!(quota.is_error);
    assert_eq!(quota.code, None);
    assert_eq!(
        quota.content,
        "run_workflow failed: this namespace has 10 of its 10 active runs"
    );

    let busy = tools
        .handle("run_workflow", &json!({ "code": "// busy" }), &call("c3"))
        .await
        .unwrap_err();
    assert!(matches!(busy, AgentError::Api(_)), "{busy:?}");

    let input = tools
        .handle("run_workflow", &json!({ "code": "" }), &call("c4"))
        .await
        .unwrap();
    assert_eq!(
        input.content,
        "run_workflow: `code` must be a non-empty string."
    );
    assert_eq!(input.code, None);

    let missing = tools
        .handle("get_run", &json!({ "run_id": "missing" }), &call("c5"))
        .await
        .unwrap();
    assert_eq!(
        missing.content,
        "get_run failed: no run missing: check the id (runs are listed by GET /v1/runs)"
    );

    let invalid_test = tools
        .handle(
            "test_workflow",
            &json!({ "code": CODE, "tests": [{ "name": "a" }] }),
            &call("c6"),
        )
        .await
        .unwrap();
    assert!(invalid_test.is_error);
    assert_eq!(
        invalid_test.content,
        "The tests were refused before running: test \"a\": `expect` is missing"
    );
    let unreadable = tools
        .handle(
            "test_workflow",
            &json!({ "code": CODE, "tests": [{ "input": 1 }] }),
            &call("c7"),
        )
        .await
        .unwrap();
    assert!(
        unreadable
            .content
            .starts_with("test_workflow failed: the body is not valid: "),
        "{}",
        unreadable.content
    );
}

#[tokio::test]
async fn test_workflow_tests_code_or_a_workflow_s_version() {
    let (tools, fake) = tools(Some("ts1"));
    // A workflow alone: its version's code and action set.
    let suite = tools
        .handle("test_workflow", &json!({ "workflow": ID }), &call("c1"))
        .await
        .unwrap();
    assert!(!suite.is_error, "{}", suite.content);
    assert_eq!(suite.code, None);
    assert_eq!(fake.body("test"), json!({ "workflow": ID }));

    let nothing = tools
        .handle("test_workflow", &json!({ "tests": [] }), &call("c2"))
        .await
        .unwrap();
    assert_eq!(
        nothing.content,
        "test_workflow: give the `code` to test, or a `workflow` whose version the tests run against."
    );
    let no_tests = tools
        .handle("test_workflow", &json!({ "code": CODE }), &call("c3"))
        .await
        .unwrap();
    assert_eq!(
        no_tests.content,
        "test_workflow: give `tests`, or a `workflow` whose suite runs against the code, or both."
    );
    assert_eq!(fake.operations(), ["test"]);
}

#[tokio::test]
async fn a_run_without_the_agent_s_labels_is_not_one_of_its_runs() {
    let (tools, fake) = tools(None);
    let result = tools
        .handle(
            "signal_run",
            &json!({ "run_id": "unlabelled", "name": "go" }),
            &call("c1"),
        )
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "signal_run: Run unlabelled does not exist, or is not one of the runs you started."
    );
    assert_eq!(fake.seen().len(), 1, "no signal was sent");

    let refused = tools
        .handle(
            "signal_run",
            &json!({ "run_id": "r9", "name": "nope" }),
            &call("c2"),
        )
        .await
        .unwrap();
    assert_eq!(
        refused.content,
        "Run alice/wf-7k2m9q/r9 refused the signal \"nope\": the workflow declares no signal \"nope\""
    );
    // A signal's key is the tool call's id.
    assert_eq!(fake.seen().last().unwrap().key.as_deref(), Some("c2"));
}

#[tokio::test]
async fn a_key_that_answers_another_run_is_the_harness_s_fault() {
    let (tools, _) = tools(None);
    let said = "the tool call id \"call_0\" is already the idempotency key of another run (of another code or workflow, or with other labels): give each tool call a unique id";
    let code = tools
        .handle(
            "run_workflow",
            &json!({ "code": "// reused" }),
            &call("call_0"),
        )
        .await
        .unwrap_err();
    assert_eq!(code.to_string(), said);
    let workflow = tools
        .handle(
            "run_workflow",
            &json!({ "workflow": "reused" }),
            &call("call_0"),
        )
        .await
        .unwrap_err();
    assert_eq!(workflow.to_string(), said);
    // A workflow is named by its id too, and a version by its number or its digest.
    for reference in [
        ID.to_owned(),
        "triage".to_owned(),
        format!("{ID}@v3"),
        "triage@030303".to_owned(),
    ] {
        let ran = tools
            .handle(
                "run_workflow",
                &json!({ "workflow": reference }),
                &call("call_1"),
            )
            .await;
        assert!(ran.is_ok(), "{reference}: {ran:?}");
    }
    let other_version = tools
        .handle(
            "run_workflow",
            &json!({ "workflow": "triage@v2" }),
            &call("call_2"),
        )
        .await;
    assert!(
        matches!(other_version, Err(AgentError::Harness(_))),
        "{other_version:?}"
    );
}

#[tokio::test]
async fn update_workflow_names_and_describes_a_workflow_without_a_version() {
    let (tools, fake) = tools(Some("ts1"));
    let named = tools
        .handle(
            "update_workflow",
            &json!({ "workflow": ID, "name": "triage", "description": null }),
            &call("c1"),
        )
        .await
        .unwrap();
    assert!(!named.is_error, "{}", named.content);
    assert_eq!(named.code, None);
    assert_eq!(
        named.content,
        [
            &format!(
                "Updated the workflow triage (id {ID}): named it triage and removed its description."
            ),
            "Its version stays v3.",
            "Run it with run_workflow({\"workflow\":\"triage\"}).",
        ]
        .join("\n")
    );
    assert_eq!(named.workflow.unwrap().id, ID);
    assert_eq!(fake.operations(), ["update_workflow"]);
    // A JSON merge patch: `null` removes.
    assert_eq!(
        fake.body("update_workflow"),
        json!({ "workflow": ID, "patch": { "name": "triage", "description": null } })
    );

    let taken = tools
        .handle(
            "update_workflow",
            &json!({ "workflow": ID, "name": "taken", "code": "// new" }),
            &call("c2"),
        )
        .await
        .unwrap();
    assert!(taken.is_error);
    assert_eq!(
        taken.content,
        "update_workflow failed: the namespace alice already has a workflow taken"
    );
    assert_eq!(
        fake.operations(),
        ["update_workflow", "update_workflow"],
        "no version after a refused name"
    );

    let nothing = tools
        .handle("update_workflow", &json!({ "workflow": ID }), &call("c3"))
        .await
        .unwrap();
    assert_eq!(
        nothing.content,
        "update_workflow: give what changes: a `name` or a `description`; `code`, an interface (`input`, `output`, `signals`) or `secrets` for a new version; or `tests`, `replace` and `retire` for its suite."
    );
}

#[tokio::test]
async fn update_workflow_makes_a_version_through_the_gate() {
    let (tools, fake) = tools(Some("ts1"));
    let made = tools
        .handle(
            "update_workflow",
            &json!({
                "workflow": "triage",
                "code": "// new",
                "secrets": { "github": "ns/github" },
                "tests": ["9f".repeat(32)],
                "retire": [{ "names": ["asks first"], "reason": "it no longer asks" }]
            }),
            &call("c1"),
        )
        .await
        .unwrap();
    assert!(!made.is_error, "{}", made.content);
    assert_eq!(made.code, Some(CodeVerdict::Passed));
    assert_eq!(
        made.content,
        [
            &format!(
                "Made the version v4 (digest 040404040404) of the workflow triage (id {ID}): its suite, 1 test, passed against it."
            ),
            "Run it with run_workflow({\"workflow\":\"triage\"}).",
        ]
        .join("\n")
    );
    // Read first, by the name given; then written by its id, with the tools' action set.
    assert_eq!(fake.operations(), ["get_workflow", "create_version"]);
    assert_eq!(
        fake.body("create_version"),
        json!({
            "workflow": ID,
            "version": {
                "code": "// new", "actionSet": "ts1",
                "secrets": { "github": "ns/github" },
                "tests": ["9f".repeat(32)],
                "retire": [{ "names": ["asks first"], "reason": "it no longer asks" }]
            }
        })
    );

    // The same content makes no version.
    let same = tools
        .handle(
            "update_workflow",
            &json!({ "workflow": "triage", "code": CODE }),
            &call("c2"),
        )
        .await
        .unwrap();
    assert!(
        same.content.starts_with(&format!(
            "The workflow triage (id {ID}) already has this content as its version v3: no version was made."
        )),
        "{}",
        same.content
    );

    // A name and a version in one call: the patch first, then the version by the id.
    let fake = {
        let (tools, fake) = self::tools(None);
        let both = tools
            .handle(
                "update_workflow",
                &json!({ "workflow": "wf_7k2m9q4x8d1c", "name": "triage", "output": { "type": "string" } }),
                &call("c3"),
            )
            .await
            .unwrap();
        assert!(!both.is_error, "{}", both.content);
        assert_eq!(both.code, None, "no code was handed in");
        assert!(
            both.content.starts_with(&format!(
                "Updated the workflow triage (id {ID}): named it triage.\nMade the version v4"
            )),
            "{}",
            both.content
        );
        fake
    };
    assert_eq!(fake.operations(), ["update_workflow", "create_version"]);
    // The interface's parts left out keep the latest version's; no action set without code.
    assert_eq!(
        fake.body("create_version")["version"],
        json!({
            "input": { "type": "object" },
            "output": { "type": "string" },
            "signals": { "approval": { "type": "object" } }
        })
    );
}

#[tokio::test]
async fn update_workflow_says_what_its_gate_refused() {
    let (tools, _) = tools(Some("ts1"));
    let failed = tools
        .handle(
            "update_workflow",
            &json!({ "workflow": "triage", "code": "// regression" }),
            &call("c1"),
        )
        .await
        .unwrap();
    assert!(failed.is_error);
    assert_eq!(
        failed.code,
        Some(CodeVerdict::Passed),
        "the code passed its checks"
    );
    assert_eq!(failed.tests.as_ref().map(Vec::len), Some(1));
    assert!(
        failed.content.starts_with(
            "No version was made: 1 test of 1 did not pass: no version was made\n1 of 1 test did not pass.\nFAIL \"test 0\""
        ),
        "{}",
        failed.content
    );
    assert!(
        failed.content.ends_with("with a reason."),
        "{}",
        failed.content
    );

    let refused = tools
        .handle(
            "update_workflow",
            &json!({ "workflow": "triage", "code": "// refused" }),
            &call("c2"),
        )
        .await
        .unwrap();
    assert_eq!(refused.code, Some(CodeVerdict::Refused));
    assert_eq!(
        refused.content,
        "No version was made: the code was refused before anything ran: the code does not type-check (1 error)"
    );

    // What the patch changed stays changed: the model reads it first.
    let partly = tools
        .handle(
            "update_workflow",
            &json!({ "workflow": ID, "description": "Triage.", "code": "// secret" }),
            &call("c3"),
        )
        .await
        .unwrap();
    assert!(partly.is_error);
    assert_eq!(partly.code, None, "refused beside the checks");
    assert_eq!(
        partly.content,
        format!(
            "Updated the workflow triage (id {ID}): gave it a new description.\nNo version was made: github needs a secret: name one in secrets"
        )
    );
}

#[tokio::test]
async fn update_workflow_changes_a_suite_alone_without_a_version() {
    let (tools, fake) = tools(None);
    let test = json!({ "name": "test 1", "input": {}, "expect": { "output": 1 } });
    let changed = tools
        .handle(
            "update_workflow",
            &json!({
                "workflow": "triage",
                "tests": [test],
                "replace": [{ "name": "test 0", "reason": "it returns 1 now", "expect": { "output": 1 } }]
            }),
            &call("c1"),
        )
        .await
        .unwrap();
    assert!(!changed.is_error, "{}", changed.content);
    assert_eq!(changed.code, None);
    assert!(
        changed.content.starts_with(&format!(
            "Changed the suite of the workflow triage (id {ID}): it holds 2 tests, each passing against its version v3, which stays its latest. Its next version keeps them."
        )),
        "{}",
        changed.content
    );
    assert_eq!(fake.operations(), ["get_workflow", "change_tests"]);
    assert_eq!(
        fake.body("change_tests"),
        json!({
            "workflow": ID,
            "tests": {
                "tests": [test],
                "replace": [{ "name": "test 0", "reason": "it returns 1 now", "expect": { "output": 1 } }]
            }
        })
    );

    let failed = tools
        .handle(
            "update_workflow",
            &json!({
                "workflow": "triage",
                "tests": [{ "name": "fails against v3", "expect": { "output": 2 } }]
            }),
            &call("c2"),
        )
        .await
        .unwrap();
    assert!(failed.is_error);
    assert!(
        failed
            .content
            .starts_with("The suite was not changed: 1 test did not pass against the version v3\n"),
        "{}",
        failed.content
    );
    assert!(
        failed.content.ends_with(
            "A test joins the suite only once it passes against the latest version, v3. If the test is wrong, fix it. If the workflow must change to pass it, give its new `code` with the test: the version is made with it once the whole suite passes."
        ),
        "{}",
        failed.content
    );
}

#[tokio::test]
async fn get_workflow_and_record_tests_name_a_workflow_by_its_id_or_its_name() {
    let (tools, fake) = tools(None);
    let read = tools
        .handle(
            "get_workflow",
            &json!({ "workflow": "triage" }),
            &call("c1"),
        )
        .await
        .unwrap();
    assert!(!read.is_error, "{}", read.content);
    assert!(
        read.content.starts_with(&format!(
            "The workflow triage (id {ID}), of the namespace alice.\nIts version v3 of 3 versions"
        )),
        "{}",
        read.content
    );
    assert!(
        read.content
            .contains("\nCode:\nexport default async () => 1;\n"),
        "{}",
        read.content
    );
    assert!(
        read.content
            .contains("\nTests that left its suite, and why:\n- \"asks first\" (eeeeeeeeeeee), left 2026-11-19T10:00:00Z: it no longer asks\n"),
        "{}",
        read.content
    );
    // Only the tests in its suite are read in full.
    assert_eq!(fake.operations(), ["get_workflow", "get_test"]);
    assert_eq!(read.workflow.unwrap().id, ID);

    let missing = tools
        .handle(
            "get_workflow",
            &json!({ "workflow": "missing" }),
            &call("c2"),
        )
        .await
        .unwrap();
    assert_eq!(missing.content, "get_workflow failed: no workflow missing");

    let recorded = tools
        .handle(
            "record_tests",
            &json!({ "workflow": ID, "limit": 5 }),
            &call("c3"),
        )
        .await
        .unwrap();
    assert!(
        recorded
            .content
            .ends_with(&format!("pass their digests in update_workflow's `tests` to add them to the suite of {ID}, or {{ \"test\": \"<digest>\", \"expect\": {{…}} }} to give a failed run the expectation it should have met.")),
        "{}",
        recorded.content
    );
    assert_eq!(
        fake.body("record_tests"),
        json!({ "workflow": ID, "record": { "limit": 5 } })
    );
}

#[tokio::test]
async fn check_workflow_says_which_workflow_has_the_code_and_when_it_has_no_comment() {
    let (tools, _) = tools(None);
    let untitled = tools
        .handle("check_workflow", &json!({ "code": CODE }), &call("c1"))
        .await
        .unwrap();
    assert_eq!(
        untitled.content,
        format!(
            "The code type-checks.\n{}",
            rewind_agent_tools::render::COMMENT_NOTE
        )
    );
    assert!(!untitled.is_error, "a note, not an error");
    assert_eq!(untitled.code, Some(CodeVerdict::Passed));

    let commented = format!("/** Count to one\n *\n * Returns 1. */\n{CODE}");
    let titled = tools
        .handle("check_workflow", &json!({ "code": commented }), &call("c2"))
        .await
        .unwrap();
    assert_eq!(titled.content, "The code type-checks.");

    let known = tools
        .handle(
            "check_workflow",
            &json!({ "code": format!("// Count to one, known\n{CODE}") }),
            &call("c3"),
        )
        .await
        .unwrap();
    assert_eq!(
        known.content,
        format!(
            "The code type-checks.\nThis code is already the version v3 of the workflow triage (id {ID}): running it is a run of that workflow."
        )
    );
}

#[tokio::test]
async fn the_tools_are_those_given_and_search_needs_an_action_set() {
    let (without, _) = tools(None);
    let names: Vec<&str> = without.definitions().iter().map(|tool| tool.name).collect();
    assert_eq!(
        names,
        [
            "run_workflow",
            "check_workflow",
            "test_workflow",
            "get_run",
            "list_runs",
            "signal_run",
            "cancel_run",
            "retry_run",
            "list_secrets",
            "update_workflow",
            "get_workflow",
            "record_tests"
        ]
    );
    assert!(matches!(
        without.handle("search_actions", &json!({}), &call("c")).await,
        Err(AgentError::UnknownTool(name)) if name == "search_actions"
    ));
    assert!(matches!(
        without.handle("save_workflow", &json!({}), &call("c")).await,
        Err(AgentError::UnknownTool(name)) if name == "save_workflow"
    ));
    let needs = AgentTools::new(
        Fake::default(),
        AgentOptions {
            tools: Some(vec!["search_actions".into()]),
            ..AgentOptions::default()
        },
    )
    .unwrap_err();
    assert_eq!(needs.to_string(), "search_actions needs an action set");
    let unknown = AgentTools::new(
        Fake::default(),
        AgentOptions {
            tools: Some(vec!["list_organizations".into()]),
            ..AgentOptions::default()
        },
    )
    .unwrap_err();
    assert!(
        unknown
            .to_string()
            .starts_with("unknown agent tool \"list_organizations\""),
        "{unknown}"
    );
}

#[tokio::test]
async fn a_retry_carries_the_agent_s_labels_and_the_default_wait() {
    let (tools, fake) = tools(None);
    let retried = tools
        .handle(
            "retry_run",
            &json!({ "run_id": "r1", "code": CODE }),
            &call("c1"),
        )
        .await
        .unwrap();
    assert!(!retried.is_error, "{}", retried.content);
    assert!(
        retried.content.starts_with(
            "Run alice/wf-7k2m9q/r2 (a fork of run alice/wf-7k2m9q/r1; a run of the workflow wf_c0de00000000@v1) completed"
        ),
        "{}",
        retried.content
    );
    assert_eq!(
        fake.body("fork_run"),
        json!({ "code": CODE, "labels": { "agent": "triage" }, "wait": "30s" })
    );
    let cancelled = tools
        .handle("cancel_run", &json!({ "run_id": "r1" }), &call("c2"))
        .await
        .unwrap();
    assert!(!cancelled.is_error, "cancel_run reports, it does not fail");
    assert_eq!(fake.body("cancel_run"), json!({ "wait": "30s" }));

    // The suite of the run's version refused the fix: the next step is a version.
    let refused = tools
        .handle(
            "retry_run",
            &json!({ "run_id": "r1", "code": "// regression" }),
            &call("c3"),
        )
        .await
        .unwrap();
    assert!(refused.is_error);
    assert!(
        refused.content.contains("check it with test_workflow({\"code\":\"…\",\"workflow\":\"triage@v3\"})")
            && refused.content.ends_with("make the workflow's next version with update_workflow, retiring or replacing its tests with a reason, and run it."),
        "{}",
        refused.content
    );
}

#[tokio::test]
async fn list_secrets_lists_what_the_agent_may_use_never_a_value() {
    let (tools, fake) = tools(None);
    let listed = tools
        .handle("list_secrets", &json!({}), &call("c"))
        .await
        .unwrap();
    assert!(!listed.is_error);
    assert_eq!(
        listed.content,
        "The secrets you may use, never a value:\n- org/zendesk, the organization's (labels env=prod)\n- org/linear, the organization's\n- ns/github, the end user's in the namespace alice\nName one in `secrets`, by the MCP server or API whose calls need it, such as {\"github\": \"ns/github\"}."
    );
    assert_eq!(fake.operations(), ["list_secrets", "namespace_secrets"]);
}

#[tokio::test]
async fn the_secrets_a_run_reads_go_with_the_start_the_retry_and_the_version() {
    let (tools, fake) = tools(Some("ts1"));
    let named = json!({ "github": "ns/github", "zendesk": "org/zendesk" });
    tools
        .handle(
            "run_workflow",
            &json!({ "code": CODE, "secrets": named }),
            &call("c1"),
        )
        .await
        .unwrap();
    tools
        .handle(
            "retry_run",
            &json!({ "run_id": "r1", "code": CODE, "secrets": { "github": "org/github" } }),
            &call("c2"),
        )
        .await
        .unwrap();
    tools
        .handle(
            "update_workflow",
            &json!({ "workflow": "triage", "secrets": { "github": "ns/github" } }),
            &call("c3"),
        )
        .await
        .unwrap();
    assert_eq!(fake.body("start_run")["secrets"], named);
    assert_eq!(
        fake.body("fork_run")["secrets"],
        json!({ "github": "org/github" })
    );
    assert_eq!(
        fake.body("create_version")["version"],
        json!({ "secrets": { "github": "ns/github" } })
    );

    let refused = tools
        .handle(
            "run_workflow",
            &json!({ "code": CODE, "secrets": { "github": 1 } }),
            &call("c4"),
        )
        .await
        .unwrap();
    assert!(refused.is_error);
    assert!(
        refused.content.starts_with(
            "run_workflow: `secrets` must be an object of secrets by MCP server or API"
        ),
        "{}",
        refused.content
    );
}

#[tokio::test]
async fn a_run_is_named_by_its_handle() {
    let (tools, fake) = tools(None);
    // The handle the model gives goes to the API as it is; the answer names the run by its
    // handle.
    let read = tools
        .handle(
            "get_run",
            &json!({ "run_id": "alice/wf-7k2m9q/r7" }),
            &call("c1"),
        )
        .await
        .unwrap();
    assert_eq!(fake.seen()[0].body["id"], "alice/wf-7k2m9q/r7");
    assert!(
        read.content.starts_with(&format!(
            "Run alice/wf-7k2m9q/r7 (a run of the workflow triage@v3, id {ID}) failed."
        )),
        "{}",
        read.content
    );
}
