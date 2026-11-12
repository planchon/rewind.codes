//! A scripted model: it replays given assistant turns, one per request, so the harness runs
//! end to end without a model API. Tests write turns in code, or as a function of the
//! conversation; `scripted:<file.json>` reads them from a file.
//!
//! In a call's input, the string `{{run_id}}` stands for the last run id the tool results
//! gave, so turns can follow a run they started.

use std::fmt;

use serde_json::Value;

use super::{AssistantTurn, Conversation, ModelError, Reply, Stop, Turn, Usage};

type Respond = dyn Fn(&Conversation) -> Option<AssistantTurn> + Send + Sync;

/// A model that answers from a script.
pub struct Scripted {
    pub name: String,
    respond: Box<Respond>,
}

impl fmt::Debug for Scripted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scripted")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Scripted {
    /// Answers with `turns`, in order; after the last one, it ends the conversation.
    pub fn turns(name: impl Into<String>, turns: Vec<AssistantTurn>) -> Self {
        Self::from_fn(name, move |conversation| {
            let answered = conversation
                .turns
                .iter()
                .filter(|turn| matches!(turn, Turn::Assistant(_)))
                .count();
            turns.get(answered).cloned()
        })
    }

    /// Answers with what `respond` returns; `None` ends the conversation.
    pub fn from_fn(
        name: impl Into<String>,
        respond: impl Fn(&Conversation) -> Option<AssistantTurn> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            respond: Box::new(respond),
        }
    }

    /// Turns from a JSON file: an array of `{ "text"?, "calls"?: [{ "id"?, "name", "input" }] }`.
    pub fn from_file(path: &str) -> Result<Self, ModelError> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| ModelError::Script(format!("{path}: {err}")))?;
        let raw: Vec<Value> = serde_json::from_str(&text)
            .map_err(|err| ModelError::Script(format!("{path}: {err}")))?;
        let mut turns = Vec::new();
        for (index, turn) in raw.into_iter().enumerate() {
            let mut turn = turn;
            if let Some(calls) = turn.get_mut("calls").and_then(Value::as_array_mut) {
                for (n, call) in calls.iter_mut().enumerate() {
                    if call.get("id").is_none() {
                        call["id"] = Value::from(format!("call_{index}_{n}"));
                    }
                }
            }
            let turn: AssistantTurn = serde_json::from_value(turn)
                .map_err(|err| ModelError::Script(format!("{path}: turn {index}: {err}")))?;
            turns.push(turn);
        }
        let stem = std::path::Path::new(path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(path);
        Ok(Self::turns(format!("scripted:{stem}"), turns))
    }

    pub fn reply(&self, conversation: &Conversation) -> Result<Reply, ModelError> {
        let Some(mut turn) = (self.respond)(conversation) else {
            return Ok(Reply {
                turn: AssistantTurn::default(),
                usage: Usage::default(),
                stop: Stop::EndTurn,
            });
        };
        if let Some(run_id) = last_run_id(conversation) {
            for call in &mut turn.calls {
                substitute(&mut call.input, &run_id);
            }
        }
        let stop = if turn.calls.is_empty() {
            Stop::EndTurn
        } else {
            Stop::ToolUse
        };
        Ok(Reply {
            turn,
            usage: Usage::default(),
            stop,
        })
    }
}

fn substitute(value: &mut Value, run_id: &str) {
    match value {
        Value::String(text) if text.contains("{{run_id}}") => {
            *text = text.replace("{{run_id}}", run_id);
        }
        Value::Array(items) => items.iter_mut().for_each(|item| substitute(item, run_id)),
        Value::Object(fields) => fields
            .values_mut()
            .for_each(|item| substitute(item, run_id)),
        _ => {}
    }
}

/// The last run id in the tool results of a conversation.
pub fn last_run_id(conversation: &Conversation) -> Option<String> {
    conversation.turns.iter().rev().find_map(|turn| match turn {
        Turn::ToolResults { results } => results
            .iter()
            .rev()
            .find_map(|result| first_uuid(&result.content)),
        _ => None,
    })
}

/// The first UUID of a text, such as `019a7b3c-…`.
pub fn first_uuid(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let shape = [8, 4, 4, 4, 12];
    let length = 36;
    (0..bytes.len().saturating_sub(length - 1)).find_map(|start| {
        let candidate = text.get(start..start + length)?;
        let mut parts = candidate.split('-');
        let fits = shape.iter().all(|len| {
            parts.next().is_some_and(|part| {
                part.len() == *len && part.bytes().all(|b| b.is_ascii_hexdigit())
            })
        });
        let bounded = start == 0 || !bytes[start - 1].is_ascii_hexdigit();
        (fits && bounded).then(|| candidate.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ToolCall, ToolResult};
    use serde_json::json;

    #[test]
    fn turns_follow_the_run_they_started() {
        let model = Scripted::turns(
            "scripted",
            vec![
                AssistantTurn {
                    text: String::new(),
                    calls: vec![ToolCall {
                        id: "1".into(),
                        name: "run_script".into(),
                        input: json!({ "code": "x" }),
                    }],
                    raw: Value::Null,
                },
                AssistantTurn {
                    text: String::new(),
                    calls: vec![ToolCall {
                        id: "2".into(),
                        name: "get_run".into(),
                        input: json!({ "run_id": "{{run_id}}" }),
                    }],
                    raw: Value::Null,
                },
            ],
        );
        let mut conversation = Conversation {
            system: String::new(),
            tools: vec![],
            turns: vec![Turn::User { text: "go".into() }],
        };
        let first = model.reply(&conversation).unwrap();
        assert_eq!(first.stop, Stop::ToolUse);
        conversation.turns.push(Turn::Assistant(first.turn));
        conversation.turns.push(Turn::ToolResults {
            results: vec![ToolResult {
                call_id: "1".into(),
                content: "Run 019a7b3c-0000-7000-8000-00000000abcd is suspended".into(),
                is_error: false,
            }],
        });
        let second = model.reply(&conversation).unwrap();
        assert_eq!(
            second.turn.calls[0].input["run_id"],
            "019a7b3c-0000-7000-8000-00000000abcd"
        );
        conversation.turns.push(Turn::Assistant(second.turn));
        assert_eq!(model.reply(&conversation).unwrap().stop, Stop::EndTurn);
    }

    #[test]
    fn uuids_are_found_whole() {
        assert_eq!(first_uuid("no id"), None);
        assert_eq!(
            first_uuid("run 019a7b3c-0000-7000-8000-00000000abcd."),
            Some("019a7b3c-0000-7000-8000-00000000abcd".into())
        );
    }
}
