//! The agent tools' definitions, as the SDK gives them to a model
//! (`sdk/typescript/src/agent-tools/definitions.ts`): the same names, descriptions and input
//! schemas, word for word. The keys of a schema's objects come sorted, as `serde_json` keeps
//! them; the SDK keeps the order it writes them in.

use serde_json::{Value, json};

/// The names of the agent tools, in the SDK's order. The evals' world always has a toolset,
/// so the tools that need one are always there, as the SDK gives them all with a toolset.
pub const AGENT_TOOLS: [&str; 12] = [
    "run_script",
    "check_script",
    "test_script",
    "get_run",
    "list_runs",
    "signal_run",
    "cancel_run",
    "retry_run",
    "search_tools",
    "save_workflow",
    "get_workflow",
    "record_tests",
];

/// The tools that need a toolset (the SDK's `NEEDS_TOOLSET`).
pub const NEEDS_TOOLSET: [&str; 4] = [
    "search_tools",
    "save_workflow",
    "get_workflow",
    "record_tests",
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
        "description": "The TypeScript source: one ES module whose default export is `async function main(input)`. It imports tools from \"rewind:tools\", workflows from \"rewind:workflows\", and `sleep`, `signal`, `webhook` from \"rewind\". Its input and output are JSON."
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

/// An answer of a test's world to the calls of one tool.
fn rule() -> Value {
    json!({
        "type": "object",
        "description": "An answer to the calls of the tool whose args contain `args`: the result in `returns`, or a failure in `error`.",
        "properties": {
            "args": {
                "type": "object",
                "description": "The call's args contain these (objects compared key by key). Any call of the tool when absent."
            },
            "returns": { "description": "The result of the call, any JSON value, null included." },
            "error": {
                "type": "object",
                "description": "The call fails with a ToolError instead.",
                "properties": {
                    "message": { "type": "string" },
                    "kind": {
                        "type": "string",
                        "enum": ["error", "unavailable", "timeout", "outcome_unknown"],
                        "description": "\"error\" when absent."
                    },
                    "data": { "description": "Any JSON value: the ToolError's `data`." }
                },
                "required": ["message"],
                "additionalProperties": false
            },
            "times": {
                "type": "integer",
                "minimum": 0,
                "description": "How many calls it answers; the next rule answers the calls after. Any number of calls when absent."
            }
        },
        "additionalProperties": false
    })
}

/// What must hold at the end of a test.
fn expect() -> Value {
    json!({
        "type": "object",
        "description": "What the outside must see at the end of the run. Anything left out is not checked, and a test never checks a variable, a helper or a log line.",
        "properties": {
            "status": {
                "type": "string",
                "enum": ["completed", "failed", "cancelled", "aborted", "active"],
                "description": "How the run ended; \"active\" for a run that can go no further in virtual time without the outside world (one waiting for a signal no test sends, for example)."
            },
            "output": {
                "description": "Values the output must contain: objects are compared key by key, recursively, so give only the keys that matter; other values must be equal."
            },
            "error": {
                "type": "object",
                "description": "The run failed with an error of this name (such as ToolError), whose message contains `message`.",
                "properties": { "name": { "type": "string" }, "message": { "type": "string" } },
                "required": ["name"],
                "additionalProperties": false
            },
            "calls": {
                "type": "array",
                "description": "Calls that must happen, or must not. The calls the world answers that are not listed are allowed: a harmless extra read breaks no test.",
                "items": {
                    "type": "object",
                    "properties": {
                        "tool": { "type": "string", "description": "The tool, such as \"github.close_issue\"." },
                        "args": {
                            "type": "object",
                            "description": "Only the calls whose args contain these. Every call of the tool when absent."
                        },
                        "times": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Exactly this many matching calls. At least one when absent; 0 says the call must not happen."
                        }
                    },
                    "required": ["tool"],
                    "additionalProperties": false
                }
            },
            "order": {
                "type": "string",
                "enum": ["any", "listed"],
                "description": "\"listed\": the first call matching each entry of `calls` comes in the order of the entries (those with times 0 aside). \"any\" when absent."
            }
        },
        "additionalProperties": false
    })
}

/// A test written in full.
fn inline_test() -> Value {
    json!({
        "type": "object",
        "description": "A test: the world the run meets and what must hold at the end. It never names the script, so the same test runs against the next version.",
        "properties": {
            "name": { "type": "string", "description": "What it checks, in a few words; unique among the tests." },
            "input": { "description": "The run's input, given to `main`. {} when absent." },
            "start": {
                "type": "string",
                "description": "When the run starts, in virtual time, as an RFC 3339 time; \"2026-01-01T00:00:00Z\" when absent. Date.now() starts there, and sleeps and timeouts take no real time."
            },
            "seed": {
                "type": "string",
                "description": "The run's seed, 64 lowercase hex digits: where its random values come from. A fixed seed when absent."
            },
            "world": {
                "type": "object",
                "description": "Everything the script may touch. A call that nothing here answers fails with a ToolError (no recorded response) and fails the test, even if the script catches the error.",
                "properties": {
                    "tools": {
                        "type": "object",
                        "description": "By tool name, such as \"github.list_issues\": one rule answering every call, or a list of rules tried in order. A start of a workflow, \"workflows.<name>.start\", is answered the same way: `returns` is the child's output, `error` its failure.",
                        "additionalProperties": { "anyOf": [rule(), { "type": "array", "items": rule() }] }
                    },
                    "mocks": {
                        "type": "string",
                        "description": "TypeScript mock handlers, for answers computed from the args: a module that imports nothing and default-exports its handlers by tool name or by namespace, such as `export default { \"github.get_issue\": ({ number }) => ({ number, state: \"open\" }) }`. What a handler throws fails the call. Tried after `tools`."
                    }
                },
                "additionalProperties": false
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
            "expect": expect()
        },
        "required": ["name"],
        "additionalProperties": false
    })
}

/// A test as the tools take it: in full, by digest, or a stored test with a new expectation.
fn test() -> Value {
    json!({
        "anyOf": [
            inline_test(),
            { "type": "string", "description": "The digest of a stored test, as record_tests or get_workflow gave it." },
            {
                "type": "object",
                "description": "A stored test with a new expectation: the same world (a recorded run's input, calls and signals), checked against `expect`. It turns a failed run into a regression test without reading its calls.",
                "properties": {
                    "from": { "type": "string", "description": "The digest of the stored test." },
                    "expect": expect()
                },
                "required": ["from", "expect"],
                "additionalProperties": false
            }
        ]
    })
}

fn tests() -> Value {
    json!({
        "type": "array",
        "description": "Tests, each written in full, the digest of a recorded test, or { from: <digest>, expect } for a recorded test with a new expectation.",
        "items": test()
    })
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
            description: "Run tests against a script, end to end, in virtual time: no tool is really called and sleeps take no time. Each test gives the world the run meets (its input, the answers of the tools it may call, the signals sent to it) and what must hold at the end (its status, its output or error, the calls it made). A call the world does not answer fails the test. With `workflow`, that workflow's suite runs too, against your script: this is how you check a fix or a change of a workflow before save_workflow. Returns whether each test passed and, for those that did not, why: stale (it names tools, answers or args your tools no longer have; not run), incompatible (the workflow's input or signal schemas refuse it; not run) or failed (an expectation does not hold), with the run's outcome and its trace.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": code(),
                    "tests": tests(),
                    "workflow": {
                        "type": "string",
                        "description": "Also run the suite of this workflow (its latest version, or name@version) against the script, and check every test against its input and signal schemas."
                    }
                },
                "required": ["code"],
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
            description: "Start a new run from a failed one with a corrected script. The calls the failed run already made are not made again: their recorded results are reused while the corrected script makes the same calls, and it continues live from the first difference. For a run of a workflow, the suite of its version runs against the corrected script first, and the retry is refused if a test does not pass: check the fix with test_script({ code, workflow }) before.",
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
        "save_workflow" => ToolDefinition {
            name: "save_workflow",
            description: "Save a script as a workflow: a named script with JSON Schemas for its input, its output and the signals it accepts. The script is type-checked against your tools and against these schemas: its default export must take the input and return the output, and `signal(name)` only takes a declared name and returns that signal's payload. Then its suite runs, and the workflow is saved only if every test passes. The suite is the tests of the version it derives from (`parent`, by default the latest version of this name), minus those you `retire`, with those you `update` replaced, plus your new `tests`. There is no way around a failing test. To change a behavior a test pins on purpose, retire the test when the behavior is deprecated, or update it when its state or its expected result is out of date, always with a reason: the reasons stay in the workflow's lineage for whoever reads it next. Read the suite with get_workflow, and check a change with test_script({ code, workflow }) first. Once saved, your later scripts can `import { <name> } from \"rewind:workflows\"`, start runs of it with `<name>.start(input)`, send them signals with `run.signal(name, payload)` and wait for their output with `run.result()`. Saving it again with another script, other schemas or other tests saves a new version, which your later scripts use.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "What scripts import from \"rewind:workflows\": a JavaScript identifier that is not a reserved word, such as triage."
                    },
                    "description": { "type": "string", "description": "What the workflow does, shown with its declaration." },
                    "code": with_description(
                        code(),
                        "The workflow's TypeScript source: one ES module whose default export is `async function main(input)`, taking the input and returning the output. It imports tools from \"rewind:tools\" and `sleep`, `signal`, `webhook` from \"rewind\"."
                    ),
                    "input": { "type": "object", "description": "The JSON Schema of the input of `main`." },
                    "output": { "type": "object", "description": "The JSON Schema of what `main` returns." },
                    "signals": {
                        "type": "object",
                        "description": "The signals its runs accept, by name, each with the JSON Schema of its payload, such as {\"approval\": {\"type\": \"object\", \"properties\": {\"close\": {\"type\": \"boolean\"}}, \"required\": [\"close\"]}}. None when absent; names starting with \"rewind:\" are reserved.",
                        "additionalProperties": { "type": "object" }
                    },
                    "tests": with_description(
                        tests(),
                        "New tests added to the suite: each written in full, the digest of a test record_tests recorded, or { from: <digest>, expect } for a recorded test with the expectation it should have met."
                    ),
                    "update": {
                        "type": "array",
                        "description": "Inherited tests to replace because their state or their expected result is out of date: by a whole new `test`, which takes the name, or by the same test with a new `expect`.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string", "description": "The name of an inherited test." },
                                "reason": { "type": "string", "description": "Why it changes: kept in the lineage." },
                                "test": with_description(test(), "The test that replaces it."),
                                "expect": with_description(expect(), "Its new expectation; the rest of the test is kept.")
                            },
                            "required": ["name", "reason"],
                            "additionalProperties": false
                        }
                    },
                    "retire": {
                        "type": "array",
                        "description": "Inherited tests to drop because the behavior they check is deprecated.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "names": { "type": "array", "items": { "type": "string" }, "description": "The names of inherited tests." },
                                "reason": { "type": "string", "description": "Why they no longer hold: kept in the lineage." }
                            },
                            "required": ["names", "reason"],
                            "additionalProperties": false
                        }
                    },
                    "parent": {
                        "type": "string",
                        "description": "The version it derives from, whose suite it inherits: name@version, or a version. The latest version of this name when absent."
                    }
                },
                "required": ["name", "description", "code", "input", "output"],
                "additionalProperties": false
            }),
        },
        "get_workflow" => ToolDefinition {
            name: "get_workflow",
            description: "Read a workflow: the source of its latest version, its input, output and signal schemas, its suite with every test in full, and its lineage: for each version, the tests it retired or updated, and why. Read it before you repair or extend a workflow you did not write: its tests say which behaviors it must keep, and its lineage which changes were made on purpose.",
            input_schema: json!({
                "type": "object",
                "properties": { "name": { "type": "string", "description": "The workflow's name, such as triage." } },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
        "record_tests" => ToolDefinition {
            name: "record_tests",
            description: "Record tests from past runs of a workflow, so a new version must keep doing what the old ones did on real inputs. Its completed and failed runs are grouped by the path they took (the tools they called in order, the signals they waited for, how they ended), and the newest run of each path becomes a test: its input, its calls and signals as they happened, and the same status, output or error and number of calls per tool as expected. The tests are stored but join no suite: pass their digests in save_workflow's `tests`, or { from: <digest>, expect } to give a failed run the expectation it should have met.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workflow": { "type": "string", "description": "The workflow's name, such as triage." },
                    "query": {
                        "type": "string",
                        "description": "Which runs, in the query language of the run lists, such as \"status:completed\". All the completed and failed runs of the workflow when absent."
                    },
                    "runs": { "type": "array", "items": { "type": "string" }, "description": "These runs, by id, rather than a query." },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "description": "At most this many tests, one per path. 20 when absent."
                    }
                },
                "required": ["workflow"],
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
        for name in NEEDS_TOOLSET {
            assert!(AGENT_TOOLS.contains(&name), "{name}");
        }
    }

    /// The same checks as the SDK's ("describe the test document: its world, its signals and its
    /// expectations, or a recorded test", `sdk/typescript/test/agent-tools.test.ts`), on sorted
    /// keys.
    #[test]
    fn they_describe_the_test_document_or_a_recorded_test() {
        let keys = |value: &Value| -> Vec<String> {
            let mut keys: Vec<String> = value
                .as_object()
                .map(|object| object.keys().cloned().collect())
                .unwrap_or_default();
            keys.sort();
            keys
        };
        let sorted = |names: &[&str]| -> Vec<String> {
            let mut names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
            names.sort();
            names
        };
        let test_script = definition("test_script").unwrap();
        assert_eq!(test_script.input_schema["required"], json!(["code"]));
        let any_of = &test_script.input_schema["properties"]["tests"]["items"]["anyOf"];
        let (inline, digest, from) = (&any_of[0], &any_of[1], &any_of[2]);
        assert_eq!(
            keys(&inline["properties"]),
            sorted(&[
                "name", "input", "start", "seed", "world", "signals", "expect"
            ])
        );
        assert_eq!(
            keys(&inline["properties"]["world"]["properties"]),
            sorted(&["tools", "mocks"])
        );
        let rule = &inline["properties"]["world"]["properties"]["tools"]["additionalProperties"]["anyOf"]
            [0];
        assert_eq!(
            keys(&rule["properties"]),
            sorted(&["args", "returns", "error", "times"])
        );
        assert_eq!(
            keys(&inline["properties"]["signals"]["items"]["properties"]),
            sorted(&["name", "payload", "when"])
        );
        let expectations = &inline["properties"]["expect"];
        assert_eq!(
            keys(&expectations["properties"]),
            sorted(&["status", "output", "error", "calls", "order"])
        );
        assert_eq!(
            keys(&expectations["properties"]["calls"]["items"]["properties"]),
            sorted(&["tool", "args", "times"])
        );
        assert_eq!(digest["type"], "string");
        assert_eq!(from["type"], "object");
        assert_eq!(from["required"], json!(["from", "expect"]));

        let save = definition("save_workflow").unwrap();
        assert_eq!(
            keys(&save.input_schema["properties"]),
            sorted(&[
                "name",
                "description",
                "code",
                "input",
                "output",
                "signals",
                "tests",
                "update",
                "retire",
                "parent",
            ])
        );
        assert!(save.description.contains("saved only if every test passes"));
        assert!(
            save.description
                .contains("retire the test when the behavior is deprecated")
        );
        assert!(
            save.description
                .contains("update it when its state or its expected result is out of date")
        );
    }
}
