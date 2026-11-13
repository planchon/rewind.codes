//! A task: what the model is asked, the world it acts in, and the checker of what it did.

use std::collections::BTreeMap;

use rewind_core::{Command, Completion, Event, RunId, RunStatus};
use serde::Serialize;
use serde_json::Value;

use crate::fake::CallRecord;
use crate::world::{Delivered, RunReport, WorldSpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Difficulty {
    Easy,
    Medium,
    Hard,
}

/// A run started before the conversation, with the agent's labels: the history the task
/// talks about.
#[derive(Debug, Clone, Copy)]
pub struct SetupRun {
    pub script: &'static str,
}

/// A task of the suite.
#[derive(Debug, Clone)]
pub struct Task {
    /// `snake_case`, unique.
    pub id: &'static str,
    pub difficulty: Difficulty,
    /// What the task exercises: `pagination`, `Promise.all`, `signal`…
    pub features: &'static [&'static str],
    /// The fake servers, built-ins and reactions of its world.
    pub world: fn() -> WorldSpec,
    pub setup: &'static [SetupRun],
    /// The user's message, given the ids of the setup runs.
    pub prompt: fn(&[RunId]) -> String,
    /// Whether the task was done, from the world at the end. `Err` says what is wrong.
    pub check: fn(&Outcome) -> Result<(), String>,
}

/// The world at the end of an episode, as a checker sees it.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The model's last answer.
    pub answer: String,
    /// Every run, oldest first: the setup's, then the agent's.
    pub runs: Vec<RunReport>,
    /// The ids of the setup runs.
    pub setup: Vec<RunId>,
    /// Each fake server's state.
    pub state: BTreeMap<String, Value>,
    /// The calls each fake server received.
    pub calls: BTreeMap<String, Vec<CallRecord>>,
    /// The signals the world sent.
    pub delivered: Vec<Delivered>,
}

impl Outcome {
    /// The runs the agent started during the conversation.
    pub fn agent_runs(&self) -> impl Iterator<Item = &RunReport> {
        self.runs.iter().filter(|run| !self.setup.contains(&run.id))
    }

    /// The outputs of the agent's completed runs, oldest first.
    pub fn outputs(&self) -> Vec<&Value> {
        self.agent_runs()
            .filter(|run| run.status == RunStatus::Completed)
            .filter_map(|run| run.output.as_ref())
            .collect()
    }

    pub fn run(&self, id: RunId) -> Option<&RunReport> {
        self.runs.iter().find(|run| run.id == id)
    }

    /// A server's state.
    pub fn state(&self, server: &str) -> &Value {
        self.state.get(server).unwrap_or(&Value::Null)
    }

    /// The calls a server received for one of its tools.
    pub fn calls_to(&self, server: &str, tool: &str) -> Vec<&CallRecord> {
        self.calls
            .get(server)
            .map(|calls| calls.iter().filter(|call| call.tool == tool).collect())
            .unwrap_or_default()
    }

    /// The answer contains `text`, ignoring case.
    pub fn answer_has(&self, text: &str) -> bool {
        self.answer.to_lowercase().contains(&text.to_lowercase())
    }

    /// The answer contains the integer `n`, written with or without thousands separators, and
    /// not as part of a longer number.
    pub fn answer_has_number(&self, n: u64) -> bool {
        let plain = n.to_string();
        let grouped = group(&plain);
        number_tokens(&self.answer)
            .iter()
            .any(|token| *token == plain || *token == grouped)
    }
}

/// `1234567` → `1,234,567`.
fn group(digits: &str) -> String {
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The numbers of a text, as written (digits and inner commas).
fn number_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let inner_comma =
            *c == ',' && !current.is_empty() && chars.get(i + 1).is_some_and(char::is_ascii_digit);
        if c.is_ascii_digit() || inner_comma {
            current.push(*c);
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// `Ok` if `condition`, else the failure.
pub fn ensure(condition: bool, failure: impl FnOnce() -> String) -> Result<(), String> {
    if condition { Ok(()) } else { Err(failure()) }
}

/// A value holds `expected`, a number within 0.01, at any depth.
pub fn holds_number(value: &Value, expected: f64) -> bool {
    match value {
        Value::Number(n) => n.as_f64().is_some_and(|n| (n - expected).abs() < 0.01),
        Value::Array(items) => items.iter().any(|item| holds_number(item, expected)),
        Value::Object(fields) => fields.values().any(|item| holds_number(item, expected)),
        _ => false,
    }
}

/// How many calls to `tool` a run scheduled before any call of the run got its outcome: the
/// calls it made concurrently from its start.
pub fn concurrent_calls(run: &RunReport, tool: &str) -> usize {
    let mut scheduled = 0;
    for entry in &run.history {
        match &entry.event {
            Event::Command(Command::ScheduleToolCall { tool: called, .. }) if called == tool => {
                scheduled += 1;
            }
            Event::Completion(Completion::ToolCallCompleted { .. })
            | Event::Completion(Completion::ToolCallFailed { .. }) => break,
            _ => {}
        }
    }
    scheduled
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn outcome(answer: &str) -> Outcome {
        Outcome {
            answer: answer.into(),
            runs: vec![],
            setup: vec![],
            state: BTreeMap::new(),
            calls: BTreeMap::new(),
            delivered: vec![],
        }
    }

    #[test]
    fn numbers_are_found_whole_with_or_without_separators() {
        let answer = outcome("There are 48 open issues, 12 from customers; total 1,234,567.");
        assert!(answer.answer_has_number(48));
        assert!(answer.answer_has_number(12));
        assert!(answer.answer_has_number(1_234_567));
        assert!(!answer.answer_has_number(4));
        assert!(!answer.answer_has_number(234));
        assert!(answer.answer_has("OPEN ISSUES"));
    }

    #[test]
    fn numbers_are_found_at_any_depth() {
        assert!(holds_number(&json!({ "avg": 157.5 }), 157.5));
        assert!(holds_number(&json!(157.5), 157.5));
        assert!(!holds_number(&json!({ "avg": "157.5" }), 157.5));
    }
}
