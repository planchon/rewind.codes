//! One episode: a model, a task, a prompt rendering. The model gets the system prompt and the
//! agent tools, and loops (generate → check → run → fix) until it answers or reaches the turn
//! limit; then the world goes on in virtual time, and the task's checker looks at it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use rewind_check::TsgoChecker;
use rewind_core::{Code, RunId, WorkflowSchemas};
use rewind_engine::RegisterWorkflow;
use rewind_engine::ports::{TenantId, WorkflowDeclarations};
use rewind_engine::suites::{SuiteChanges, TestInput};
use rewind_runtime::TsHandlerLoader;
use serde::Serialize;
use serde_json::json;

use rewind_agent_tools::{
    AgentError, AgentOptions, AgentToolResult, AgentTools, CodeVerdict, HandleOptions, definitions,
};

use crate::clock::rfc3339;
use crate::fake;
use crate::model::{AssistantTurn, Conversation, Model, Stop, ToolResult, Turn, Usage};
use crate::prompt::{self, Rendering};
use crate::task::{Difficulty, Outcome, Setup, SetupWorkflow, Task};
use crate::world::{Delivered, World};

/// How episodes run.
#[derive(Debug, Clone)]
pub struct EpisodeConfig {
    /// Model turns at most.
    pub max_turns: usize,
    /// How the agent tools behave: the SDK's defaults, a wait of 30 s and 2 minutes at most, in
    /// virtual time. Each episode gives them its action set and its labels.
    pub agent: AgentOptions,
}

impl Default for EpisodeConfig {
    fn default() -> Self {
        Self {
            max_turns: 30,
            agent: AgentOptions::default(),
        }
    }
}

/// Labels of an episode's runs.
pub fn labels(episode: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("eval".to_owned(), episode.to_owned())])
}

/// The tests a tool call gives: `test_workflow`'s and `save_workflow`'s `tests`, and the tests
/// `save_workflow` replacements.
fn tests_given(name: &str, input: &serde_json::Value) -> usize {
    let input = match input {
        serde_json::Value::String(text) => serde_json::from_str(text).unwrap_or_default(),
        other => other.clone(),
    };
    let count = |key: &str| {
        input
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len)
    };
    match name {
        "test_workflow" => count("tests"),
        "save_workflow" => count("tests") + count("replace"),
        _ => 0,
    }
}

/// What an episode measured.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EpisodeRecord {
    pub task: String,
    pub difficulty: Difficulty,
    pub model: String,
    pub rendering: Rendering,
    pub repetition: u32,
    /// The task's checker passed.
    pub success: bool,
    /// Why the checker failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// The episode could not be measured: the model's API or the harness failed. Such episodes
    /// are left out of the rates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether the first code the model submitted passed the checks before anything ran
    /// (type-check, imports, default export); absent when it submitted none.
    pub first_check: Option<bool>,
    /// Model turns: requests to its API.
    pub iterations: usize,
    /// Workflows submitted (`run_workflow`, `check_workflow`, `test_workflow`, `retry_run`,
    /// `save_workflow`).
    pub scripts: usize,
    /// Workflows refused by the checks.
    pub refused: usize,
    /// Tests the model wrote, unprompted or not: those it gave `test_workflow` and
    /// `save_workflow`, replacements included.
    pub tests_written: usize,
    /// Fixes the gate refused because a test of a workflow's suite did not pass
    /// (`save_workflow`, `retry_run`).
    pub gate_refusals: usize,
    /// Runs the agent started, forks included.
    pub runs: usize,
    /// Agent tool calls, by tool.
    pub tool_calls: BTreeMap<String, usize>,
    pub usage: Usage,
    /// Why the model stopped last.
    pub stop: Option<Stop>,
    /// The model was still calling tools at the turn limit.
    pub turn_limit: bool,
    pub wall_ms: u64,
    /// The virtual time when the checker looked.
    pub virtual_end: String,
    /// The signals the world sent.
    pub delivered: Vec<Delivered>,
}

/// An episode: its record, and the conversation for the transcript.
#[derive(Debug, Clone)]
pub struct Episode {
    pub record: EpisodeRecord,
    pub system: String,
    pub turns: Vec<Turn>,
}

impl Episode {
    /// The transcript as JSON: the system prompt and every turn.
    pub fn transcript(&self) -> serde_json::Value {
        json!({ "record": self.record, "system": self.system, "turns": self.turns })
    }
}

/// Runs a task with a model and a rendering.
pub async fn run(
    task: &Task,
    model: &Model,
    rendering: Rendering,
    checker: Arc<TsgoChecker>,
    config: &EpisodeConfig,
    repetition: u32,
) -> Episode {
    let started = Instant::now();
    let mut record = EpisodeRecord {
        task: task.id.to_owned(),
        difficulty: task.difficulty,
        model: model.name().to_owned(),
        rendering,
        repetition,
        success: false,
        failure: None,
        error: None,
        first_check: None,
        iterations: 0,
        scripts: 0,
        refused: 0,
        tests_written: 0,
        gate_refusals: 0,
        runs: 0,
        tool_calls: BTreeMap::new(),
        usage: Usage::default(),
        stop: None,
        turn_limit: false,
        wall_ms: 0,
        virtual_end: String::new(),
        delivered: Vec::new(),
    };
    let episode_id = format!("{}-{}-{rendering}-{repetition}", task.id, model.name());
    let world = match World::new(&(task.world)(), checker, labels(&episode_id)).await {
        Ok(world) => world,
        Err(error) => {
            record.error = Some(format!("the world could not start: {error}"));
            return Episode {
                record,
                system: String::new(),
                turns: Vec::new(),
            };
        }
    };
    let system = prompt::system(&world.declarations, rendering);
    let mut conversation = Conversation {
        system: system.clone(),
        tools: definitions::definitions(),
        turns: Vec::new(),
    };
    match converse(task, model, &world, config, &mut conversation, &mut record).await {
        Ok(()) => {}
        Err(error) => record.error = Some(error),
    }
    record.wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Episode {
        record,
        system,
        turns: conversation.turns,
    }
}

async fn converse(
    task: &Task,
    model: &Model,
    world: &World,
    config: &EpisodeConfig,
    conversation: &mut Conversation,
    record: &mut EpisodeRecord,
) -> Result<(), String> {
    let setup = setup(task, world).await?;
    conversation.turns.push(Turn::User {
        text: (task.prompt)(&setup),
    });

    let options = AgentOptions {
        action_set: Some(world.action_set.digest().to_hex()),
        labels: world.labels.clone(),
        ..config.agent.clone()
    };
    let tools =
        AgentTools::new(world, options).map_err(|err| format!("the harness failed: {err}"))?;
    let mut answer = String::new();
    let mut ended = false;
    for _ in 0..config.max_turns {
        let reply = model
            .reply(conversation)
            .await
            .map_err(|err| format!("the model failed: {err}"))?;
        record.iterations += 1;
        record.usage.add(reply.usage);
        record.stop = Some(reply.stop.clone());
        if !reply.turn.text.trim().is_empty() {
            answer = reply.turn.text.clone();
        }
        let calls = reply.turn.calls.clone();
        conversation.turns.push(Turn::Assistant(reply.turn));
        if calls.is_empty() || matches!(reply.stop, Stop::Refusal) {
            ended = true;
            break;
        }
        let mut results = Vec::new();
        for call in &calls {
            *record.tool_calls.entry(call.name.clone()).or_default() += 1;
            let call_options = HandleOptions {
                tool_call_id: Some(call.id.clone()),
                ..HandleOptions::default()
            };
            let handled = match tools.handle(&call.name, &call.input, &call_options).await {
                Ok(handled) => handled,
                Err(AgentError::UnknownTool(name)) => AgentToolResult {
                    content: format!("{name:?} is not one of the tools."),
                    is_error: true,
                    run: None,
                    workflow: None,
                    action_set: None,
                    tests: None,
                    code: None,
                },
                Err(error) => return Err(format!("the harness failed: {error}")),
            };
            if let Some(verdict) = handled.code {
                record.scripts += 1;
                let passed = verdict == CodeVerdict::Passed;
                if !passed {
                    record.refused += 1;
                }
                record.first_check.get_or_insert(passed);
            }
            record.tests_written += tests_given(&call.name, &call.input);
            // The gate refused a fix: a test of a workflow's suite did not pass, so
            // `save_workflow` saved nothing, or `retry_run` started no fork.
            let gated = matches!(call.name.as_str(), "save_workflow" | "retry_run");
            if gated && handled.is_error && handled.tests.is_some() {
                record.gate_refusals += 1;
            }
            results.push(ToolResult {
                call_id: call.id.clone(),
                content: handled.content,
                is_error: handled.is_error,
            });
        }
        conversation.turns.push(Turn::ToolResults { results });
    }
    record.turn_limit = !ended;

    world
        .settle()
        .await
        .map_err(|err| format!("the world could not settle: {err}"))?;
    let production = world
        .run_production()
        .await
        .map_err(|err| format!("production could not run: {err}"))?;
    let outcome = outcome(world, answer, setup, production)
        .await
        .map_err(|err| format!("the world could not be read: {err}"))?;
    record.runs = outcome.agent_runs().count();
    record.virtual_end = rfc3339(world.now());
    record.delivered = outcome.delivered.clone();
    match (task.check)(&outcome) {
        Ok(()) => record.success = true,
        Err(failure) => record.failure = Some(failure),
    }
    Ok(())
}

/// Plays the task's setup with the agent's labels: saves its workflows, starts its runs and
/// drives each as far as it goes. The ids of the runs.
async fn setup(task: &Task, world: &World) -> Result<Vec<RunId>, String> {
    let mut ids = Vec::new();
    for step in task.setup {
        let run = match step {
            Setup::Run(code) => {
                let request = world.start_request(Code::new(*code), json!({}));
                world
                    .start(request, true)
                    .await
                    .map_err(|err| format!("a setup run was refused: {err}"))?
                    .run
            }
            Setup::Workflow(workflow) => {
                save_workflow(world, &workflow()).await?;
                continue;
            }
            Setup::WorkflowRun { workflow, input } => world
                .start_workflow(workflow, input())
                .await
                .map_err(|err| format!("a setup run was refused: {err}"))?,
        };
        ids.push(run.id);
        world
            .drive(None, None)
            .await
            .map_err(|err| format!("a setup run could not run: {err}"))?;
    }
    Ok(ids)
}

/// Saves a workflow of the setup through the gate, with the agent's labels, on the action set it
/// was written for.
async fn save_workflow(world: &World, workflow: &SetupWorkflow) -> Result<(), String> {
    let refused = |error: String| {
        format!(
            "the setup's workflow {} was refused: {error}",
            workflow.name
        )
    };
    let action_set = match workflow.action_set {
        Some(action_set) => action_set(&world.action_set).map_err(refused)?,
        None => world.action_set.clone(),
    };
    world
        .register_action_set(&action_set)
        .await
        .map_err(|err| refused(err.to_string()))?;
    let mut tests = Vec::with_capacity(workflow.tests.len());
    for test in &workflow.tests {
        let test: rewind_api::testing::Test =
            serde_json::from_value(test.clone()).map_err(|err| refused(err.to_string()))?;
        let test = test
            .into_core(crate::api::decode_cassette)
            .map_err(refused)?;
        tests.push(TestInput::Inline(Box::new(test)));
    }
    let schemas = WorkflowSchemas {
        input: workflow.input.clone(),
        output: workflow.output.clone(),
        signals: workflow.signals.clone(),
    };
    let generated = rewind_codegen::workflow_declarations(workflow.name, &schemas);
    let loader = TsHandlerLoader::default();
    world
        .edge
        .register_workflow(
            RegisterWorkflow {
                tenant: TenantId::local(),
                name: workflow.name.to_owned(),
                description: Some(workflow.description.to_owned()),
                code: Code::new(workflow.code),
                action_set: action_set.digest(),
                schemas,
                labels: world.labels.clone(),
                secrets: Default::default(),
                attach: rewind_engine::attachments::AttachRights::Organization {
                    labels: Default::default(),
                },
                declarations: WorkflowDeclarations {
                    signals: generated.signals,
                    entry: generated.entry,
                },
                suite: SuiteChanges {
                    tests,
                    ..SuiteChanges::default()
                },
            },
            &*world.checker,
            &world.test_kit(&loader),
        )
        .await
        .map_err(|err| refused(err.to_string()))?;
    Ok(())
}

async fn outcome(
    world: &World,
    answer: String,
    setup: Vec<RunId>,
    production: Vec<RunId>,
) -> Result<Outcome, crate::world::WorldError> {
    let calls = world
        .servers
        .iter()
        .map(|server| (server.service.name().to_owned(), server.calls()))
        .collect();
    Ok(Outcome {
        answer,
        runs: world.run_reports().await?,
        setup,
        production,
        workflows: world.workflow_reports().await?,
        state: fake::snapshots(&world.servers),
        calls,
        delivered: world.delivered(),
    })
}

/// An assistant turn that only answers, for scripted models.
pub fn answer(text: &str) -> AssistantTurn {
    AssistantTurn {
        text: text.to_owned(),
        calls: Vec::new(),
        raw: serde_json::Value::Null,
    }
}
