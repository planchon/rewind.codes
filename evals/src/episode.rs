//! One episode: a model, a task, a prompt rendering. The model gets the system prompt and the
//! agent tools, and loops (generate → check → run → fix) until it answers or reaches the turn
//! limit; then the world goes on in virtual time, and the task's checker looks at it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use ptc_check::TsgoChecker;
use ptc_core::{RunId, Script};
use serde::Serialize;
use serde_json::json;

use crate::agent::{self, AgentError, AgentOptions, AgentTools, ScriptVerdict, definitions};
use crate::clock::rfc3339;
use crate::fake;
use crate::model::{AssistantTurn, Conversation, Model, Stop, ToolResult, Turn, Usage};
use crate::prompt::{self, Rendering};
use crate::task::{Difficulty, Outcome, Task};
use crate::world::{Delivered, World};

/// How episodes run.
#[derive(Debug, Clone)]
pub struct EpisodeConfig {
    /// Model turns at most.
    pub max_turns: usize,
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
    /// Whether the first script the model submitted passed the checks before anything ran
    /// (type-check, imports, default export); absent when it submitted none.
    pub first_check: Option<bool>,
    /// Model turns: requests to its API.
    pub iterations: usize,
    /// Scripts submitted (`run_script`, `check_script`, `test_script`, `retry_run`).
    pub scripts: usize,
    /// Scripts refused by the checks.
    pub refused: usize,
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
    let world = match World::new(&(task.world)(), checker, agent::labels(&episode_id)).await {
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

    let tools = AgentTools::new(world, config.agent.clone());
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
            let handled = match tools.handle(&call.name, &call.input, &call.id).await {
                Ok(handled) => handled,
                Err(AgentError::UnknownTool(name)) => agent::AgentToolResult {
                    content: format!("{name:?} is not one of the tools."),
                    is_error: true,
                    script: None,
                },
                Err(error) => return Err(format!("the harness failed: {error}")),
            };
            if let Some(verdict) = handled.script {
                record.scripts += 1;
                let passed = verdict == ScriptVerdict::Passed;
                if !passed {
                    record.refused += 1;
                }
                record.first_check.get_or_insert(passed);
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
    let outcome = outcome(world, answer, setup)
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

/// Starts the task's setup runs with the agent's labels, and drives them as far as they go.
async fn setup(task: &Task, world: &World) -> Result<Vec<RunId>, String> {
    let mut ids = Vec::new();
    for run in task.setup {
        let request = world.start_request(Script::new(run.script), json!({}));
        let created = world
            .engine
            .start_run(request)
            .await
            .map_err(|err| format!("a setup run was refused: {err}"))?;
        ids.push(created.run.id);
        world
            .drive(None, None)
            .await
            .map_err(|err| format!("a setup run could not run: {err}"))?;
    }
    Ok(ids)
}

async fn outcome(
    world: &World,
    answer: String,
    setup: Vec<RunId>,
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
