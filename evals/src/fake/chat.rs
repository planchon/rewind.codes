//! `chat`: a team chat. No tool declares an output schema: results are JSON in text, as most
//! MCP servers answer, read with `.json()`.

use std::sync::Mutex;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{Args, CallContext, Reply, Service, ToolSpec, lock, object_schema};
use crate::clock::rfc3339;

/// The channels, without their `#`.
pub const CHANNELS: [(&str, &str); 10] = [
    ("ops", "Operations and incidents"),
    ("bugs", "The open bugs, every morning"),
    ("deploys", "Deployment announcements"),
    ("standup", "Daily standup"),
    ("finance", "Budget and spending"),
    ("managers", "Engineering managers"),
    ("escalations", "Customer escalations"),
    ("support", "Customer support"),
    ("general", "Everything else"),
    ("random", "Off topic"),
];

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Message {
    pub channel: String,
    pub text: String,
    pub ts: String,
    pub author: String,
}

#[derive(Debug)]
pub struct Chat {
    messages: Mutex<Vec<Message>>,
}

impl Default for Chat {
    fn default() -> Self {
        Self::new()
    }
}

impl Chat {
    pub fn new() -> Self {
        Self {
            messages: Mutex::new(Vec::new()),
        }
    }
}

/// `#ops` and `ops` name the same channel.
fn channel_of(name: &str) -> Option<&'static str> {
    let name = name.trim().trim_start_matches('#');
    CHANNELS
        .iter()
        .find(|(channel, _)| *channel == name)
        .map(|(channel, _)| *channel)
}

impl Service for Chat {
    fn name(&self) -> &'static str {
        "chat"
    }

    fn tools(&self) -> Vec<ToolSpec> {
        let channel =
            json!({ "type": "string", "description": "The channel's name, such as #ops." });
        vec![
            ToolSpec::new(
                "list_channels",
                "List the channels, with their topics.",
                object_schema(json!({}), &[]),
            ),
            ToolSpec::new(
                "post_message",
                "Post a message to a channel. Returns the message's timestamp `ts`.",
                object_schema(
                    json!({ "channel": channel, "text": { "type": "string" } }),
                    &["channel", "text"],
                ),
            ),
            ToolSpec::new(
                "list_messages",
                "The latest messages of a channel, newest first.",
                object_schema(
                    json!({ "channel": channel, "limit": { "type": "integer", "minimum": 1, "maximum": 100 } }),
                    &["channel"],
                ),
            ),
        ]
    }

    fn call(&self, context: &CallContext, tool: &str, args: &Map<String, Value>) -> Reply {
        let args = Args(args);
        let mut messages = lock(&self.messages);
        match tool {
            "list_channels" => Reply::Json(json!(
                CHANNELS
                    .iter()
                    .map(|(name, topic)| json!({ "name": format!("#{name}"), "topic": topic }))
                    .collect::<Vec<_>>()
            )),
            "post_message" | "list_messages" => {
                let name = match args.str("channel") {
                    Ok(name) => name,
                    Err(reply) => return reply,
                };
                let Some(channel) = channel_of(name) else {
                    return Reply::error(format!("channel_not_found: no channel {name}"));
                };
                if tool == "post_message" {
                    let text = match args.str("text") {
                        Ok(text) => text.to_owned(),
                        Err(reply) => return reply,
                    };
                    let ts = rfc3339(context.now);
                    messages.push(Message {
                        channel: channel.to_owned(),
                        text,
                        ts: ts.clone(),
                        author: "rewind-bot".to_owned(),
                    });
                    Reply::Json(json!({ "ok": true, "channel": format!("#{channel}"), "ts": ts }))
                } else {
                    let limit = args.opt_u64("limit").unwrap_or(20) as usize;
                    let listed: Vec<&Message> = messages
                        .iter()
                        .rev()
                        .filter(|message| message.channel == channel)
                        .take(limit)
                        .collect();
                    Reply::Json(json!(listed))
                }
            }
            other => Reply::error(format!("unknown tool {other}")),
        }
    }

    fn snapshot(&self) -> Value {
        json!({ "messages": *lock(&self.messages) })
    }
}

/// The messages of the chat's state posted to `channel` (without `#`), in order.
pub fn messages_in(state: &Value, channel: &str) -> Vec<Message> {
    state["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["channel"] == channel)
                .map(|message| Message {
                    channel: channel.to_owned(),
                    text: message["text"].as_str().unwrap_or_default().to_owned(),
                    ts: message["ts"].as_str().unwrap_or_default().to_owned(),
                    author: message["author"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::T0;

    #[test]
    fn a_posted_message_is_listed_with_its_time() {
        let chat = Chat::new();
        let context = CallContext {
            now: T0,
            idempotency_key: None,
        };
        let posted = chat.call(
            &context,
            "post_message",
            json!({ "channel": "#ops", "text": "hello" })
                .as_object()
                .unwrap(),
        );
        assert_eq!(
            posted,
            Reply::Json(json!({ "ok": true, "channel": "#ops", "ts": "2026-03-02T09:00:00Z" }))
        );
        assert_eq!(messages_in(&chat.snapshot(), "ops")[0].text, "hello");
        assert!(matches!(
            chat.call(
                &context,
                "post_message",
                json!({ "channel": "#nope", "text": "x" })
                    .as_object()
                    .unwrap()
            ),
            Reply::Error { .. }
        ));
    }
}
