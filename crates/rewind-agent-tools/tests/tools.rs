//! The SDK's set over a fake API: what each tool sends to `/v1`, how a refusal the model can fix
//! becomes a result with `is_error` and a fault an error, the labels' scope, and the toolset
//! `save_workflow` derives.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rewind_agent_tools::{
    AgentError, AgentOptions, AgentTools, Api, ApiError, HandleOptions, ScriptVerdict,
};
use rewind_api::common::Duration;
use rewind_api::events::Entry;
use rewind_api::headers::IdempotencyKey;
use rewind_api::runs::{
    CancelRun, ForkRun, Run, RunFilter, RunPage, SignalAccepted, StartRun, Trace,
};
use rewind_api::scripts::{CheckRequest, CheckResult, Script, TestRequest};
use rewind_api::testing::{Test, TestRun};
use rewind_api::toolsets::{CreateToolset, SearchQuery, ToolSearchResults, Toolset};
use rewind_api::workflows::{
    CreateWorkflow, RecordTests, RecordedTests, Workflow, WorkflowVersions,
};
use rewind_api::{Problem, ProblemType};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const CODE: &str = "export default async () => 1;";

fn sha256(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn run(id: &str, labels: Value, script: &str, status: &str) -> Run {
    serde_json::from_value(json!({
        "id": id, "namespace": "alice", "status": status, "script": script, "toolset": "t",
        "epoch": 1, "mode": "live", "labels": labels, "traceId": "0", "historyLen": 3,
        "createdAt": "2026-11-19T10:00:00Z", "updatedAt": "2026-11-19T10:00:01Z",
        "expiresAt": "2026-12-19T10:00:00Z",
        "output": { "digest": "d", "bytes": 1, "preview": "1", "value": 1 }
    }))
    .unwrap()
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
    toolsets: Mutex<u32>,
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
}

fn problem(kind: ProblemType, detail: &str) -> ApiError {
    Problem::new(kind, detail).into()
}

impl Api for Fake {
    async fn start_run(
        &self,
        start: &StartRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, ApiError> {
        self.saw("start_run", start, key);
        if start.script.contains("refused") {
            return Err(Problem::new(
                ProblemType::InvalidScript,
                "the script does not type-check (1 error): fix the diagnostics and start it again",
            )
            .into());
        }
        if start.script.contains("busy") {
            return Err(problem(
                ProblemType::Unavailable,
                "the store cannot be reached: retry later",
            ));
        }
        if start.script.contains("quota") {
            return Err(problem(
                ProblemType::QuotaExceeded,
                "this namespace has 10 of its 10 active runs",
            ));
        }
        // A key seen before: the run of another script.
        let script = if start.script.contains("reused") {
            sha256("other")
        } else {
            sha256(&start.script)
        };
        Ok(run(
            "r1",
            serde_json::to_value(&start.labels).unwrap(),
            &script,
            "completed",
        ))
    }

    async fn get_run(&self, id: &str, wait: Option<Duration>) -> Result<Run, ApiError> {
        self.saw("get_run", json!({ "id": id, "wait": wait }), None);
        match id {
            "unlabelled" => Ok(run(id, json!({}), "s", "failed")),
            "missing" => Err(problem(
                ProblemType::NotFound,
                "no run missing: check the id (runs are listed by GET /v1/runs)",
            )),
            _ => Ok(run(id, json!({ "agent": "triage" }), "s", "failed")),
        }
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<RunPage, ApiError> {
        self.saw("list_runs", filter, None);
        Ok(RunPage {
            runs: Vec::new(),
            next_cursor: None,
        })
    }

    async fn run_trace(&self, run: &str) -> Result<Trace, ApiError> {
        self.saw("run_trace", json!({ "run": run }), None);
        Ok(serde_json::from_value(
            json!({ "runId": run, "format": "llm", "text": "[0] run_started" }),
        )
        .unwrap())
    }

    async fn run_entry(&self, _run: &str, _idx: u64) -> Result<Entry, ApiError> {
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
        ))
    }

    async fn fork_run(
        &self,
        run: &str,
        fork: &ForkRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, ApiError> {
        self.saw("fork_run", fork, key);
        let mut forked = self::run(
            "r2",
            json!({ "agent": "triage" }),
            &sha256(&fork.script),
            "completed",
        );
        forked.parent = Some(rewind_api::runs::ForkParent {
            run: run.into(),
            at: None,
        });
        Ok(forked)
    }

    async fn check(&self, check: &CheckRequest) -> Result<CheckResult, ApiError> {
        self.saw("check", check, None);
        Ok(CheckResult {
            ok: true,
            diagnostics: Vec::new(),
        })
    }

    async fn test(&self, test: &TestRequest) -> Result<TestRun, ApiError> {
        self.saw("test", test, None);
        Err(problem(
            ProblemType::InvalidTest,
            "test \"a\": `expect` is missing",
        ))
    }

    async fn get_script(&self, _digest: &str) -> Result<Script, ApiError> {
        Err(ApiError::fault("unused"))
    }

    async fn get_payload(&self, _digest: &str, _run: Option<&str>) -> Result<Value, ApiError> {
        Err(ApiError::fault("unused"))
    }

    async fn get_test(&self, _digest: &str) -> Result<Test, ApiError> {
        Err(ApiError::fault("unused"))
    }

    async fn create_toolset(&self, toolset: &CreateToolset) -> Result<Toolset, ApiError> {
        self.saw("create_toolset", toolset, None);
        let mut count = self.0.toolsets.lock().unwrap();
        *count += 1;
        Ok(serde_json::from_value(json!({
            "id": format!("derived-{count}"),
            "servers": [], "tools": [],
            "declarations": { "dts": "", "prompt": "" }
        }))
        .unwrap())
    }

    async fn search_tools(
        &self,
        toolset: &str,
        query: &SearchQuery,
    ) -> Result<ToolSearchResults, ApiError> {
        self.saw(
            "search_tools",
            json!({ "toolset": toolset, "query": query.query }),
            None,
        );
        Ok(ToolSearchResults { tools: Vec::new() })
    }

    async fn create_workflow(&self, workflow: &CreateWorkflow) -> Result<Workflow, ApiError> {
        self.saw("create_workflow", workflow, None);
        Ok(serde_json::from_value(json!({
            "name": workflow.name, "version": "9f2c4e1a07b3".repeat(5) + "9f2c", "toolset": workflow.toolset,
            "script": sha256(&workflow.script), "input": workflow.input, "output": workflow.output,
            "signals": {}, "tests": [], "retired": [], "updated": [],
            "labels": { "agent": "triage" }, "createdAt": "2026-11-19T10:00:00Z",
            "declarations": { "dts": "", "prompt": "" }
        }))
        .unwrap())
    }

    async fn workflow_versions(&self, _name: &str) -> Result<WorkflowVersions, ApiError> {
        Err(ApiError::fault("unused"))
    }

    async fn record_tests(
        &self,
        _workflow: &str,
        _record: &RecordTests,
    ) -> Result<RecordedTests, ApiError> {
        Err(ApiError::fault("unused"))
    }
}

fn tools(toolset: Option<&str>) -> (AgentTools<Fake>, Fake) {
    let fake = Fake::default();
    let options = AgentOptions {
        toolset: toolset.map(str::to_owned),
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
async fn run_script_starts_the_run_with_the_agent_s_toolset_labels_wait_and_key() {
    let (tools, fake) = tools(Some("ts1"));
    let result = tools
        .handle(
            "run_script",
            &json!({ "code": CODE, "wait": "1h" }),
            &call("call_0"),
        )
        .await
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.script, Some(ScriptVerdict::Passed));
    assert!(
        result.content.starts_with("Run r1 completed"),
        "{}",
        result.content
    );
    assert_eq!(result.run.unwrap().id, "r1");
    let seen = fake.seen();
    assert_eq!(seen[0].operation, "start_run");
    assert_eq!(
        seen[0].body,
        json!({
            "script": CODE, "toolset": "ts1", "input": {}, "wait": "2m",
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
async fn refusals_are_results_and_faults_are_errors() {
    let (tools, _) = tools(Some("ts1"));
    let refused = tools
        .handle("run_script", &json!({ "code": "// refused" }), &call("c1"))
        .await
        .unwrap();
    assert!(refused.is_error);
    assert_eq!(refused.script, Some(ScriptVerdict::Refused));
    assert_eq!(
        refused.content,
        "The script was refused before anything ran: the script does not type-check (1 error): fix the diagnostics and start it again"
    );

    let quota = tools
        .handle("run_script", &json!({ "code": "// quota" }), &call("c2"))
        .await
        .unwrap();
    assert!(quota.is_error);
    assert_eq!(quota.script, None);
    assert_eq!(
        quota.content,
        "run_script failed: this namespace has 10 of its 10 active runs"
    );

    let busy = tools
        .handle("run_script", &json!({ "code": "// busy" }), &call("c3"))
        .await
        .unwrap_err();
    assert!(matches!(busy, AgentError::Api(_)), "{busy:?}");

    let input = tools
        .handle("run_script", &json!({ "code": "" }), &call("c4"))
        .await
        .unwrap();
    assert_eq!(
        input.content,
        "run_script: `code` must be a non-empty string."
    );
    assert_eq!(input.script, None);

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
            "test_script",
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
            "test_script",
            &json!({ "code": CODE, "tests": [{ "input": 1 }] }),
            &call("c7"),
        )
        .await
        .unwrap();
    assert!(
        unreadable
            .content
            .starts_with("test_script failed: the body is not valid: "),
        "{}",
        unreadable.content
    );
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
        "Run r9 refused the signal \"nope\": the workflow declares no signal \"nope\""
    );
    // A signal's key is the tool call's id.
    assert_eq!(fake.seen().last().unwrap().key.as_deref(), Some("c2"));
}

#[tokio::test]
async fn a_key_that_answers_another_script_s_run_is_the_harness_s_fault() {
    let (tools, _) = tools(None);
    let error = tools
        .handle(
            "run_script",
            &json!({ "code": "// reused" }),
            &call("call_0"),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "the tool call id \"call_0\" is already the idempotency key of another run (of another script, or with other labels): give each tool call a unique id"
    );
}

#[tokio::test]
async fn each_saved_workflow_derives_the_toolset_the_next_calls_use() {
    let (tools, fake) = tools(Some("ts1"));
    let workflow = json!({
        "name": "triage", "description": "Triage issues.", "code": CODE,
        "input": { "type": "object" }, "output": { "type": "object" }
    });
    let saved = tools
        .handle("save_workflow", &workflow, &call("c1"))
        .await
        .unwrap();
    assert!(!saved.is_error, "{}", saved.content);
    assert_eq!(saved.toolset.unwrap().id, "derived-1");
    assert_eq!(tools.toolset().as_deref(), Some("derived-1"));
    tools
        .handle("save_workflow", &workflow, &call("c2"))
        .await
        .unwrap();
    assert_eq!(tools.toolset().as_deref(), Some("derived-2"));
    tools
        .handle("search_tools", &json!({ "query": "triage" }), &call("c3"))
        .await
        .unwrap();
    let seen = fake.seen();
    let derivations: Vec<&Value> = seen
        .iter()
        .filter(|seen| seen.operation == "create_toolset")
        .map(|seen| &seen.body)
        .collect();
    let version = format!("triage@{}9f2c", "9f2c4e1a07b3".repeat(5));
    assert_eq!(
        *derivations[0],
        json!({ "base": "ts1", "workflows": [version] })
    );
    assert_eq!(derivations[1]["base"], "derived-1");
    let created: Vec<&Value> = seen
        .iter()
        .filter(|seen| seen.operation == "create_workflow")
        .map(|seen| &seen.body)
        .collect();
    assert_eq!(created[1]["toolset"], "derived-1");
    assert_eq!(seen.last().unwrap().body["toolset"], "derived-2");
}

#[tokio::test]
async fn the_tools_are_those_given_and_some_need_a_toolset() {
    let (without, _) = tools(None);
    let names: Vec<&str> = without.definitions().iter().map(|tool| tool.name).collect();
    assert_eq!(
        names,
        [
            "run_script",
            "check_script",
            "test_script",
            "get_run",
            "list_runs",
            "signal_run",
            "cancel_run",
            "retry_run"
        ]
    );
    assert!(matches!(
        without.handle("save_workflow", &json!({}), &call("c")).await,
        Err(AgentError::UnknownTool(name)) if name == "save_workflow"
    ));
    let needs = AgentTools::new(
        Fake::default(),
        AgentOptions {
            tools: Some(vec!["search_tools".into()]),
            ..AgentOptions::default()
        },
    )
    .unwrap_err();
    assert_eq!(needs.to_string(), "search_tools needs a toolset");
    let unknown = AgentTools::new(
        Fake::default(),
        AgentOptions {
            tools: Some(vec!["list_orgs".into()]),
            ..AgentOptions::default()
        },
    )
    .unwrap_err();
    assert!(
        unknown
            .to_string()
            .starts_with("unknown agent tool \"list_orgs\""),
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
        retried
            .content
            .starts_with("Run r2 (a fork of run r1) completed"),
        "{}",
        retried.content
    );
    let fork = fake
        .seen()
        .into_iter()
        .find(|seen| seen.operation == "fork_run")
        .unwrap();
    assert_eq!(
        fork.body,
        json!({ "script": CODE, "labels": { "agent": "triage" }, "wait": "30s" })
    );
    let cancelled = tools
        .handle("cancel_run", &json!({ "run_id": "r1" }), &call("c2"))
        .await
        .unwrap();
    assert!(!cancelled.is_error, "cancel_run reports, it does not fail");
    let cancel = fake
        .seen()
        .into_iter()
        .find(|seen| seen.operation == "cancel_run")
        .unwrap();
    assert_eq!(cancel.body, json!({ "wait": "30s" }));
}
