//! The agent tools' definitions, as the SDK gives them to a model
//! (`sdk/typescript/src/agent-tools/definitions.ts`): the same names, descriptions and input
//! schemas, word for word.

use serde_json::{Value, json};

/// The names of the agent tools, in the SDK's order.
pub const AGENT_TOOLS: [&str; 9] = [
    "run_script",
    "check_script",
    "test_script",
    "get_run",
    "list_runs",
    "signal_run",
    "cancel_run",
    "retry_run",
    "search_tools",
];

/// A tool for a model, independent of any model API.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

fn run_id() -> Value {
    json!({ "type": "string", "description": "The run id, as a previous tool result gave it." })
}

fn code() -> Value {
    json!({
        "type": "string",
        "description": "The TypeScript source: one ES module whose default export is `async function main(input)`. It imports tools from \"rewind:tools\" and `sleep`, `signal`, `webhook` from \"rewind\". Its input and output are JSON."
    })
}

fn wait() -> Value {
    json!({
        "type": "string",
        "description": "How long to wait for the run to end before answering, such as \"30s\" or \"2m\". If the run is still going then, you get its id and what it waits for."
    })
}

fn status() -> Value {
    json!({
        "type": "string",
        "enum": ["pending", "active", "completed", "failed", "cancelled", "aborted"]
    })
}

fn with_description(mut schema: Value, description: &str) -> Value {
    schema["description"] = json!(description);
    schema
}

/// The definition of an agent tool.
pub fn definition(name: &str) -> Option<ToolDefinition> {
    Some(match name {
        "run_script" => ToolDefinition {
            name: "run_script",
            description: "Type-check a TypeScript script and run it. The script calls the tools declared in `rewind:tools` and runs durably: it can sleep for days and wait for signals. A script that does not type-check is refused with its diagnostics, before anything runs. When the run ends within `wait`, you get its output and last log lines, or its error with the failing line and the trace up to the failure. Otherwise you get the run id and what the run waits for: follow it with get_run, send it a signal with signal_run, or cancel it with cancel_run.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": code(),
                    "input": { "description": "The input passed to `main`, any JSON value. {} when absent." },
                    "wait": wait()
                },
                "required": ["code"],
                "additionalProperties": false
            }),
        },
        "check_script" => ToolDefinition {
            name: "check_script",
            description: "Type-check a TypeScript script against the sandbox and the tool declarations, without running it. Returns the diagnostics with their location and a code frame.",
            input_schema: json!({
                "type": "object",
                "properties": { "code": code() },
                "required": ["code"],
                "additionalProperties": false
            }),
        },
        "test_script" => ToolDefinition {
            name: "test_script",
            description: "Run a script against mocks, in virtual time: no tool is really called and sleeps take no time. Tool results come from the calls recorded by an earlier run (`scenario.from_run`), then TypeScript mock handlers (`scenario.mocks`), then static responses (`scenario.tools`). Returns whether the expectations hold, the outcome, the calls made and the trace.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": code(),
                    "scenario": {
                        "type": "object",
                        "description": "What the script runs against, and what must hold at the end.",
                        "properties": {
                            "input": { "description": "The input passed to `main`. {} when absent." },
                            "tools": {
                                "type": "object",
                                "description": "Static responses by tool name, such as {\"github.list_issues\": []}. {\"$error\": \"message\"} makes the call fail with a ToolError.",
                                "additionalProperties": true
                            },
                            "mocks": {
                                "type": "string",
                                "description": "TypeScript mock handlers, as a module run in a separate isolate. Tried after the calls recorded by `from_run`, before the static responses."
                            },
                            "from_run": {
                                "type": "string",
                                "description": "A run id: the tool results recorded by that run are replayed as a cassette, before mocks and static responses."
                            },
                            "signals": {
                                "type": "array",
                                "description": "Signals sent to the run, in order.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "name": { "type": "string" },
                                        "payload": { "description": "Any JSON value." },
                                        "when": {
                                            "type": "string",
                                            "description": "\"blocked\" (the default): once the run can go no further without it. Or a duration of virtual time after the start, such as \"2h\"."
                                        }
                                    },
                                    "required": ["name"],
                                    "additionalProperties": false
                                }
                            },
                            "expect": {
                                "type": "object",
                                "description": "What must hold at the end. Anything left out is not checked.",
                                "properties": {
                                    "status": status(),
                                    "output": { "description": "Values the output must contain; objects are compared key by key." },
                                    "error": {
                                        "type": "object",
                                        "properties": { "name": { "type": "string" }, "message": { "type": "string" } },
                                        "required": ["name"],
                                        "additionalProperties": false
                                    },
                                    "calls": {
                                        "type": "object",
                                        "description": "How many times each tool must be called, by tool name.",
                                        "additionalProperties": { "type": "integer", "minimum": 0 }
                                    }
                                },
                                "additionalProperties": false
                            }
                        },
                        "additionalProperties": false
                    }
                },
                "required": ["code", "scenario"],
                "additionalProperties": false
            }),
        },
        "get_run" => ToolDefinition {
            name: "get_run",
            description: "Read a run: its status, what it waits for or how it ended, and its trace. With `entry`, one journal entry in full instead. With `offset`, the next page of an output (or entry) too large for one answer.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "run_id": run_id(),
                    "entry": { "type": "integer", "minimum": 0, "description": "The index of a journal entry, as the trace shows it." },
                    "offset": { "type": "integer", "minimum": 0, "description": "Where the page starts, in bytes, as the previous answer said." },
                    "wait": { "type": "string", "description": "Wait up to this long for the run to end first, such as \"30s\"." }
                },
                "required": ["run_id"],
                "additionalProperties": false
            }),
        },
        "list_runs" => ToolDefinition {
            name: "list_runs",
            description: "List the runs you started, most recent first.",
            input_schema: json!({
                "type": "object",
                "properties": { "status": with_description(status(), "Only runs with this status.") },
                "additionalProperties": false
            }),
        },
        "signal_run" => ToolDefinition {
            name: "signal_run",
            description: "Send a signal to a run: it resolves the script's `signal(name)`, now or the next time the script waits for that name. A run that has ended refuses it.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "run_id": run_id(),
                    "name": { "type": "string", "description": "The signal's name, as the script waits for it." },
                    "payload": { "description": "The signal's payload, any JSON value. null when absent." }
                },
                "required": ["run_id", "name"],
                "additionalProperties": false
            }),
        },
        "cancel_run" => ToolDefinition {
            name: "cancel_run",
            description: "Cancel a run: every pending await of the script rejects with CancelledError, and the script gets one chance to clean up.",
            input_schema: json!({
                "type": "object",
                "properties": { "run_id": run_id() },
                "required": ["run_id"],
                "additionalProperties": false
            }),
        },
        "retry_run" => ToolDefinition {
            name: "retry_run",
            description: "Start a new run from a failed one with a corrected script. The calls the failed run already made are not made again: their recorded results are reused while the corrected script makes the same calls, and it continues live from the first difference.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "run_id": with_description(run_id(), "The run to correct."),
                    "code": with_description(code(), "The corrected TypeScript source.")
                },
                "required": ["run_id", "code"],
                "additionalProperties": false
            }),
        },
        "search_tools" => ToolDefinition {
            name: "search_tools",
            description: "Find the tools of `rewind:tools` matching a query, with their TypeScript declarations: for toolsets too large to be listed in full.",
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "Words to look for in tool names and descriptions." } },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        _ => return None,
    })
}

/// Every agent tool's definition.
pub fn definitions() -> Vec<ToolDefinition> {
    AGENT_TOOLS
        .iter()
        .filter_map(|name| definition(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_an_object_schema() {
        let definitions = definitions();
        assert_eq!(definitions.len(), AGENT_TOOLS.len());
        for definition in definitions {
            assert_eq!(
                definition.input_schema["type"], "object",
                "{}",
                definition.name
            );
            assert_eq!(definition.input_schema["additionalProperties"], false);
        }
        assert_eq!(
            definition("list_runs").unwrap().input_schema["properties"]["status"]["description"],
            "Only runs with this status."
        );
    }
}
