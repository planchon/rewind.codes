//! Workflows across runs in the sandbox: `rewind:workflows`, the handles of the runs a code
//! starts or signals, `result()`, the attached runs a code waits for, and the checks of the
//! args inside the op (`archi/03-execution.md`, "Workflows across runs").

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use rewind_core::{
    ActionFailure, ActionSet, Annotation, Code, Command, Completion, Event, HistoryEvent,
    PinnedWorkflow, RunMode, RunOutcome, RunState, RunStatus, WorkflowId, WorkflowSchemas,
    WorkflowVersion,
};
use rewind_engine::ports::{
    ActionDeclarations, ActionDispatcher, Call, CallAuth, CatalogStore, CodeExecutor, Dispatched,
    NewWorkflow, Predicate, RunRef, TenantId, WorkflowCreated, WorkflowScope,
};
use rewind_engine::{ReplayVerdict, StartError, StartRun};
use rewind_runtime::{SandboxLimits, V8Executor};
use serde_json::{Value, json};

use crate::support;

use support::World;

type Handler = dyn Fn(&Call) -> Result<Value, ActionFailure> + Send + Sync;

#[derive(Clone)]
struct Actions(Arc<Handler>);

impl ActionDispatcher for Actions {
    async fn dispatch(&self, call: Call, _auth: CallAuth) -> Dispatched {
        match (self.0)(&call) {
            Ok(result) => Dispatched::Result(result),
            Err(failure) => Dispatched::Failed(failure),
        }
    }
}

/// `triage` waits for `approval`, then closes issues, unless its input asks it to fail or to
/// return what its output schema refuses.
const TRIAGE: &str = r#"
import { signal } from "rewind";
import { notify } from "rewind:actions";

export default async function main(input: { repo: string; fail?: boolean; bad?: boolean }) {
  if (input.fail) throw new TypeError(`cannot triage ${input.repo}`);
  const approval = await signal<{ close: boolean }>("approval");
  await notify({ text: `triage of ${input.repo}` });
  if (input.bad) return { closed: "three" };
  return { closed: approval.close ? 3 : 0, repo: input.repo };
}
"#;

fn schemas() -> WorkflowSchemas {
    WorkflowSchemas {
        input: json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string" },
                "fail": { "type": "boolean" },
                "bad": { "type": "boolean" },
            },
            "required": ["repo"],
        }),
        output: json!({
            "type": "object",
            "properties": { "closed": { "type": "integer" } },
            "required": ["closed"],
        }),
        signals: BTreeMap::from([(
            "approval".to_owned(),
            json!({
                "type": "object",
                "properties": { "close": { "type": "boolean" } },
                "required": ["close"],
            }),
        )]),
    }
}

struct Harness {
    world: World<Actions>,
    /// The action set that pins `triage`.
    action_set: ActionSet,
}

async fn harness() -> Harness {
    harness_with(|_| Ok(json!({ "ok": true }))).await
}

async fn harness_with(
    actions: impl Fn(&Call) -> Result<Value, ActionFailure> + Send + Sync + 'static,
) -> Harness {
    let world = World::new(
        V8Executor::new(SandboxLimits::default()).expect("TZ=UTC"),
        Actions(Arc::new(actions)),
        7,
        |_| {},
    )
    .await;
    // `triage`'s version in the catalog, its suite empty: nothing gates it.
    let tenant = TenantId::local();
    let catalog = &world.plane.catalog;
    let child_action_set = ActionSet::from_harness_actions(["notify"]).unwrap();
    catalog
        .put_action_set(&tenant, &child_action_set, &ActionDeclarations::default())
        .await
        .unwrap();
    let code = Code::new(TRIAGE);
    let compiled = world
        .plane
        .executor
        .compile(&code, &child_action_set)
        .unwrap();
    catalog.put_code(&tenant, &code, &compiled).await.unwrap();
    let version = WorkflowVersion::new(code.digest(), child_action_set.digest())
        .with_interface(Some(schemas()))
        .unwrap();
    let id = WorkflowId::from_bits(7);
    let created = catalog
        .create_workflow(
            &tenant,
            &NewWorkflow {
                id: id.clone(),
                scope: WorkflowScope::Organization,
                name: Some("triage".into()),
                description: None,
                labels: BTreeMap::new(),
                source: None,
                created_by: "key:test".into(),
                version: version.clone(),
            },
            world.now(),
        )
        .await
        .unwrap();
    assert!(matches!(created, WorkflowCreated::Created(_)));
    let action_set = ActionSet::from_harness_actions(["notify"])
        .unwrap()
        .with_workflows([PinnedWorkflow {
            name: "triage".into(),
            id,
            version: 1,
            digest: version.digest(),
            description: None,
            schemas: schemas(),
        }])
        .unwrap();
    Harness { world, action_set }
}

struct Run {
    id: RunRef,
    history: Vec<HistoryEvent>,
    state: RunState,
}

impl Run {
    fn output(&self) -> &Value {
        match self.state.outcome() {
            Some(RunOutcome::Completed { output }) => output.value().unwrap(),
            other => panic!(
                "expected a completed run, got {other:?}\n{:#?}",
                self.history
            ),
        }
    }

    fn events(&self) -> Vec<&'static str> {
        self.history
            .iter()
            .map(|entry| entry.event.name())
            .collect()
    }
}

impl Harness {
    async fn try_start(
        &self,
        source: &str,
        input: Value,
        mode: RunMode,
    ) -> Result<RunRef, StartError> {
        let mut request = StartRun::new(Code::new(source), self.action_set.clone(), input);
        request.mode = mode;
        self.world.start(request).await
    }

    /// Drives the node in virtual time until only the outside world can wake a run; no
    /// activation failed, no run is quarantined.
    async fn drive(&self) {
        self.world.idle().await;
    }

    async fn get(&self, id: &RunRef) -> Run {
        let history = self.world.history(id).await;
        let state = RunState::from_history(&history).unwrap();
        Run {
            id: id.clone(),
            history,
            state,
        }
    }

    async fn run(&self, source: &str, input: Value) -> Run {
        let id = self.try_start(source, input, RunMode::Live).await.unwrap();
        self.drive().await;
        self.get(&id).await
    }

    async fn assert_replays(&self, run: &Run) {
        let verdict = self.world.replay(&run.id, None).await;
        assert!(matches!(verdict, ReplayVerdict::Matches(_)), "{verdict:?}");
    }

    /// The run a start's result names.
    fn child(&self, run: &Run) -> RunRef {
        let id = run
            .history
            .iter()
            .find_map(|entry| match &entry.event {
                Event::Completion(Completion::CallCompleted { result, .. }) => result
                    .value()
                    .and_then(|result| result.get("runId"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                _ => None,
            })
            .expect("a start with a result");
        RunRef::new(TenantId::local(), id.parse().unwrap())
    }
}

#[tokio::test]
async fn a_script_starts_signals_and_awaits_a_run_of_a_workflow() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { triage } from "rewind:workflows";

export default async function main(input: { repos: string[] }) {
  const runs = await Promise.all(input.repos.map((repo) => triage.start({ repo })));
  await Promise.all(runs.map((run) => run.signal("approval", { close: true })));
  const results = await Promise.all(runs.map((run) => run.result()));
  const again = await runs[0].result();
  return { results, again, ids: runs.map((run) => typeof run.id) };
}
"#,
            json!({ "repos": ["acme/api", "acme/web"] }),
        )
        .await;
    assert_eq!(
        run.output(),
        &json!({
            "results": [
                { "closed": 3, "repo": "acme/api" },
                { "closed": 3, "repo": "acme/web" },
            ],
            "again": { "closed": 3, "repo": "acme/api" },
            "ids": ["string", "string"],
        })
    );
    let awaited: Vec<String> = run
        .history
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Annotation(Annotation::SignalAwaited { name, .. }) => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert!(
        awaited.iter().all(|name| name.starts_with("rewind:ended:")),
        "{awaited:?}"
    );
    harness.assert_replays(&run).await;
    let child = harness.get(&harness.child(&run)).await;
    assert_eq!(
        RunStatus::at_rest(child.state.phase()),
        RunStatus::Completed
    );
}

#[tokio::test]
async fn a_run_that_ends_badly_rejects_result_with_a_run_error() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { triage } from "rewind:workflows";
import { RunError, WorkflowError } from "rewind";

export default async function main() {
  const failed = await triage.start({ repo: "acme/api", fail: true });
  const bad = await triage.start({ repo: "acme/web", bad: true });
  await bad.signal("approval", { close: true });
  const errors = [];
  for (const run of [failed, bad]) {
    try {
      await run.result();
    } catch (error) {
      errors.push({
        name: error.name,
        workflow: error.workflow,
        status: error.status,
        own: error instanceof RunError,
        // Code written before the error was named RunError catches the same class (R1).
        old: error instanceof WorkflowError,
        same: error.runId === run.id,
        message: error.message.replace(run.id, "<id>"),
      });
    }
  }
  return errors;
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(
        run.output(),
        &json!([
            {
                "name": "RunError",
                "workflow": "triage",
                "status": "failed",
                "own": true,
                "old": true,
                "same": true,
                "message": "workflow triage (run <id>) failed: TypeError: cannot triage acme/api",
            },
            {
                "name": "RunError",
                "workflow": "triage",
                "status": "invalid_output",
                "own": true,
                "old": true,
                "same": true,
                "message": "workflow triage (run <id>) returned an output its schema refuses: output.closed: \"three\" is not of type \"integer\"",
            },
        ])
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn a_script_does_not_end_before_the_runs_it_started_attached() {
    let harness = harness().await;
    let id = harness
        .try_start(
            r#"
import { triage } from "rewind:workflows";

export default async function main() {
  await triage.start({ repo: "acme/api" });                    // attached, never awaited
  await triage.start({ repo: "acme/web" }, { detached: true }); // lives on its own
  return "done";
}
"#,
            json!({}),
            RunMode::Live,
        )
        .await
        .unwrap();
    harness.drive().await;
    let waiting = harness.get(&id).await;
    assert_eq!(
        RunStatus::at_rest(waiting.state.phase()),
        RunStatus::Waiting,
        "{:?}",
        waiting.events()
    );

    let children = harness.world.runs(vec![Predicate::StartedBy(id.id)]).await;
    assert_eq!(children.len(), 2);
    for child in &children {
        harness
            .world
            .signal(&child.run, "approval", json!({ "close": false }))
            .await
            .unwrap();
    }
    harness.drive().await;
    let run = harness.get(&id).await;
    assert_eq!(run.output(), &json!("done"));
    // It ended once its attached run had: its end is in the history before `complete_run`.
    let events = run.events();
    assert_eq!(events.last(), Some(&"complete_run"));
    assert!(events.contains(&"signal_received"), "{events:?}");
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn args_are_checked_inside_the_op_and_names_are_reserved() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { signal } from "rewind";
import { triage } from "rewind:workflows";

export default async function main() {
  const messages: string[] = [];
  const attempt = async (call: () => Promise<unknown>) => {
    try {
      await call();
      messages.push("ok");
    } catch (error) {
      messages.push(`${error.name}: ${error.message}`);
    }
  };
  // @ts-ignore: what a code checked by tsgo cannot write
  await attempt(() => triage.start({ repo: 42 }));
  // @ts-ignore
  await attempt(() => triage.start({ repo: "a" }, { retries: 2 }));
  await attempt(() => triage.start({ repo: "a" }, { ttl: "soon" }));
  // @ts-ignore
  await attempt(() => triage.get("not-an-id").signal("approval", { close: true }));
  // @ts-ignore
  await attempt(() => triage.get("0193a5c4-0000-7000-8000-000000000001").signal("aproval", {}));
  // @ts-ignore
  await attempt(() => triage.get("0193a5c4-0000-7000-8000-000000000001").signal("approval", { close: "yes" }));
  await attempt(() => signal("rewind:ended:x"));
  return messages;
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(
        run.output(),
        &json!([
            "TypeError: invalid input for workflow triage: input.repo: 42 is not of type \"string\"",
            "TypeError: triage.start() takes no option \"retries\": its options are detached, labels and ttl",
            "TypeError: invalid ttl \"soon\": a number of milliseconds, or a duration such as \"2d\"",
            "TypeError: \"not-an-id\" is not a run id",
            "TypeError: workflow triage declares no signal \"aproval\" (it declares \"approval\")",
            "TypeError: invalid payload for the signal \"approval\" of workflow triage: payload.close: \"yes\" is not of type \"boolean\"",
            "TypeError: signal names starting with \"rewind:\" are reserved: rewind:ended:x",
        ])
    );
    // Nothing was recorded for the refused calls.
    assert!(
        !run.history
            .iter()
            .any(|entry| matches!(entry.event, Event::Command(Command::ScheduleCall { .. }))),
        "{:?}",
        run.events()
    );
}

#[tokio::test]
async fn a_signal_to_a_run_that_has_ended_is_an_action_error_and_result_can_time_out() {
    let harness = harness().await;
    let run = harness
        .run(
            r#"
import { triage } from "rewind:workflows";

export default async function main() {
  const run = await triage.start({ repo: "acme/api" }, { detached: true });
  const early = await run.result({ timeout: "1h" });
  await run.signal("approval", { close: true });
  const done = await run.result({ timeout: "1h" });
  let late = "";
  try {
    await run.signal("approval", { close: true });
  } catch (error) {
    late = `${error.name}: ${error.message.replace(run.id, "<id>")}`;
  }
  return { early: early ?? null, done, late };
}
"#,
            json!({}),
        )
        .await;
    assert_eq!(
        run.output(),
        &json!({
            "early": null,
            "done": { "closed": 3, "repo": "acme/api" },
            "late": "ActionError: workflows.triage.signal failed: run <id> has ended",
        })
    );
    harness.assert_replays(&run).await;
}

#[tokio::test]
async fn importing_a_workflow_the_action_set_does_not_pin_is_refused_at_start() {
    let harness = harness().await;
    let error = harness
        .try_start(
            "import { digest } from \"rewind:workflows\";\nexport default async () => digest;",
            json!({}),
            RunMode::Live,
        )
        .await
        .unwrap_err();
    let StartError::InvalidCode(invalid) = error else {
        panic!("expected an invalid code, got {error:?}");
    };
    assert!(
        invalid
            .message
            .contains("\"rewind:workflows\" has no export named \"digest\""),
        "{}",
        invalid.message
    );
}

#[tokio::test]
async fn in_a_test_run_the_world_answers_with_the_end_of_the_run() {
    let harness = harness_with(|call| match call.action.name.as_str() {
        "workflows.triage.start" => {
            Ok(json!({ "closed": call.args["input"]["repo"].as_str().unwrap().len() }))
        }
        other => Ok(json!({ "unexpected": other })),
    })
    .await;
    let id = harness
        .try_start(
            r#"
import { triage } from "rewind:workflows";

export default async function main() {
  const run = await triage.start({ repo: "acme/api" });
  return run.result();
}
"#,
            json!({}),
            RunMode::Test,
        )
        .await
        .unwrap();
    harness.drive().await;
    let run = harness.get(&id).await;
    assert_eq!(run.output(), &json!({ "closed": 8 }));
    harness.assert_replays(&run).await;
}
