//! What the script receives from an MCP tool (`archi/04-tools.md`, "Toolsets and code
//! generation"):
//!
//! - A tool pinned with an output schema returns its `structuredContent`: the value its types
//!   describe. A server that sent none, but put the JSON in a text block as the specification
//!   asks for compatibility, gets that JSON parsed.
//! - A tool pinned without one returns its content: `{ "content": [blocks], "structuredContent"?:
//!   value }`. Blocks keep the fields `rewind-tools.d.ts` declares (`type`, `text`, `data`,
//!   `mimeType`, `uri`, `name`, `title`, `description`, `size`, `resource`); annotations and
//!   `_meta` are dropped. The runtime adds the `text()` and `json()` helpers.
//! - `isError: true` is a failure of kind `error`: its message is the text of the content, its
//!   data the `structuredContent`, if any.

use rewind_core::{Tool, ToolFailureKind};
use rewind_engine::ports::Dispatched;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value, json};

use crate::failure::{Failure, bounded, failure};

pub(crate) fn outcome(tool: &Tool, result: CallToolResult) -> Dispatched {
    if result.is_error == Some(true) {
        let text = text(&result.content);
        let message = if text.trim().is_empty() {
            format!("{} reported an error", tool.name)
        } else {
            bounded(text.trim())
        };
        return Dispatched::Failed(failure(
            ToolFailureKind::Error,
            message,
            result.structured_content,
        ));
    }
    if tool.output_schema.is_some() {
        if let Some(structured) = result.structured_content {
            return Dispatched::Result(structured);
        }
        return match serde_json::from_str::<Value>(text(&result.content).trim()) {
            Ok(value) => Dispatched::Result(value),
            Err(_) => Failure::permanent(
                ToolFailureKind::Error,
                format!(
                    "{} declares an output schema, but returned no structured content",
                    tool.name
                ),
                None,
            )
            .into_dispatched(),
        };
    }
    let mut value = Map::new();
    value.insert(
        "content".into(),
        Value::Array(result.content.iter().map(block).collect()),
    );
    if let Some(structured) = result.structured_content {
        value.insert("structuredContent".into(), structured);
    }
    Dispatched::Result(Value::Object(value))
}

/// The text blocks, joined by newlines: what `text()` returns.
fn text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A content block with the fields the declarations give it.
fn block(block: &ContentBlock) -> Value {
    let serialized = serde_json::to_value(block).unwrap_or_else(|_| json!({}));
    let Value::Object(fields) = serialized else {
        return json!({});
    };
    let keep: &[&str] = match fields.get("type").and_then(Value::as_str) {
        Some("text") => &["type", "text"],
        Some("image" | "audio") => &["type", "data", "mimeType"],
        Some("resource_link") => &[
            "type",
            "uri",
            "name",
            "title",
            "description",
            "mimeType",
            "size",
        ],
        Some("resource") => &["type", "resource"],
        _ => &[],
    };
    let mut kept = Map::new();
    for (key, value) in fields {
        if keep.is_empty() && key != "annotations" && key != "_meta" || keep.contains(&key.as_str())
        {
            let value = if key == "resource" {
                resource(value)
            } else {
                value
            };
            kept.insert(key, value);
        }
    }
    Value::Object(kept)
}

/// An embedded resource: its URI, its MIME type, and its text or its bytes in base64.
fn resource(value: Value) -> Value {
    let Value::Object(fields) = value else {
        return value;
    };
    Value::Object(
        fields
            .into_iter()
            .filter(|(key, _)| matches!(key.as_str(), "uri" | "mimeType" | "text" | "blob"))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use rewind_core::ToolFailure;
    use rewind_core::ToolRoute;
    use rmcp::model::{Resource, ResourceContents};

    use super::*;

    fn tool(output: Option<Value>) -> Tool {
        let mut tool = Tool::new(
            "crm.lookup",
            ToolRoute::Mcp {
                server: "crm".into(),
            },
            json!({}),
        );
        tool.output_schema = output;
        tool
    }

    #[test]
    fn a_tool_without_an_output_schema_returns_its_content() {
        let result = CallToolResult::success(vec![
            ContentBlock::text("{\"name\":\"Ada\"}"),
            ContentBlock::image("aGVsbG8=", "image/png"),
            ContentBlock::resource(ResourceContents::text("notes", "file:///notes.txt")),
            ContentBlock::resource_link(Resource::new("file:///a.csv", "a.csv")),
        ]);
        assert_eq!(
            outcome(&tool(None), result),
            Dispatched::Result(json!({
                "content": [
                    { "type": "text", "text": "{\"name\":\"Ada\"}" },
                    { "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" },
                    {
                        "type": "resource",
                        "resource": { "uri": "file:///notes.txt", "mimeType": "text/plain", "text": "notes" }
                    },
                    { "type": "resource_link", "uri": "file:///a.csv", "name": "a.csv" },
                ]
            }))
        );
        let mut structured = CallToolResult::success(vec![ContentBlock::text("Ada")]);
        structured.structured_content = Some(json!({ "name": "Ada" }));
        assert_eq!(
            outcome(&tool(None), structured),
            Dispatched::Result(json!({
                "content": [{ "type": "text", "text": "Ada" }],
                "structuredContent": { "name": "Ada" }
            }))
        );
    }

    #[test]
    fn a_tool_with_an_output_schema_returns_its_structured_content() {
        let schema = Some(json!({ "type": "object" }));
        let mut result = CallToolResult::success(vec![ContentBlock::text("ignored")]);
        result.structured_content = Some(json!({ "name": "Ada" }));
        assert_eq!(
            outcome(&tool(schema.clone()), result),
            Dispatched::Result(json!({ "name": "Ada" }))
        );
        let text_only = CallToolResult::success(vec![ContentBlock::text(" {\"name\":\"Ada\"} ")]);
        assert_eq!(
            outcome(&tool(schema.clone()), text_only),
            Dispatched::Result(json!({ "name": "Ada" }))
        );
        let Dispatched::Failed(failure) = outcome(
            &tool(schema),
            CallToolResult::success(vec![ContentBlock::text("not json")]),
        ) else {
            panic!("a result without structured content fails");
        };
        assert_eq!(failure.kind, ToolFailureKind::Error);
    }

    #[test]
    fn an_error_result_is_a_failure() {
        let mut result = CallToolResult::error(vec![ContentBlock::text("no such customer")]);
        result.structured_content = Some(json!({ "code": 404 }));
        assert_eq!(
            outcome(&tool(None), result),
            Dispatched::Failed(ToolFailure {
                kind: ToolFailureKind::Error,
                message: "no such customer".into(),
                data: Some(json!({ "code": 404 })),
            })
        );
        assert_eq!(
            outcome(&tool(None), CallToolResult::error(vec![])),
            Dispatched::Failed(ToolFailure {
                kind: ToolFailureKind::Error,
                message: "crm.lookup reported an error".into(),
                data: None,
            })
        );
    }
}
