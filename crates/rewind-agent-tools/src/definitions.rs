//! The agent tools' definitions.
//!
//! The SDK's set, as the SDK gives them to a model
//! (`sdk/typescript/src/agent-tools/definitions.ts`): the same names, descriptions and input
//! schemas, word for word. The keys of a schema's objects come sorted, as `serde_json` keeps
//! them; the SDK keeps the order it writes them in.
//!
//! The MCP set (`archi/07-code-architecture.md`, "MCP and OAuth"): the same tools but
//! `save_workflow`, with `list_orgs`, `list_namespaces` and `list_action_sets` to find what the
//! caller reaches, the organization, the namespace and the action set as arguments, and for each
//! tool a title, its annotations and the permission it needs.

use rewind_api::access::Permission;
use serde_json::{Value, json};

/// The names of the agent tools of the SDK's set, in the SDK's order.
pub const AGENT_TOOLS: [&str; 13] = [
    "run_script",
    "check_script",
    "test_script",
    "get_run",
    "list_runs",
    "signal_run",
    "cancel_run",
    "retry_run",
    "search_tools",
    "list_secrets",
    "save_workflow",
    "get_workflow",
    "record_tests",
];

/// The tools that need an action set (the SDK's `NEEDS_TOOLSET`).
pub const NEEDS_ACTION_SET: [&str; 4] = [
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
    json!({ "type": "string", "description": "The run, by its handle (such as dana/triage/06bqx3k9f2mt) or its id, as a previous tool result gave it." })
}

/// What `run_script` and `check_script` ask of a script's first lines: the comment its title and
/// description are read from.
macro_rules! title_comment {
    () => {
        "Start the script with a comment that says what it does:\n/** <title>\n *\n * <description> */\nIts title names what the script does in a few words, such as \"Triage new GitHub issues\": its runs are named after it."
    };
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

/// What a run reads: the secrets it attaches, by MCP server or API.
fn secrets(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": { "type": "string", "description": "org/<name> or ns/<name>." }
    })
}

/// The `secrets` of `run_script`.
const RUN_SECRETS: &str = "The secrets the run reads, by the MCP server or API whose calls need one: {\"github\": \"ns/github\"} for the end user's own secret, read in the namespace the run acts for, or \"org/<name>\" for the organization's. Names only, never a value: list_secrets gives those you may attach. A script that imports a server needing a secret is refused without one.";

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
            description: concat!(
                "Type-check a TypeScript script and run it. The script calls the tools declared in `rewind:tools` and runs durably: it can sleep for days and wait for signals. A script that does not type-check is refused with its diagnostics, before anything runs. When the run ends within `wait`, you get its output and last log lines, or its error with the failing line and the trace up to the failure. Otherwise you get the run's handle and what the run waits for: follow it with get_run, send it a signal with signal_run, or cancel it with cancel_run. ",
                title_comment!()
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": code(),
                    "input": { "description": "The input passed to `main`, any JSON value. {} when absent." },
                    "wait": wait(),
                    "secrets": secrets(RUN_SECRETS)
                },
                "required": ["code"],
                "additionalProperties": false
            }),
        },
        "check_script" => ToolDefinition {
            name: "check_script",
            description: concat!(
                "Type-check a TypeScript script against the sandbox and the tool declarations, without running it. Returns the diagnostics with their location and a code frame, and a note when the script has no title. ",
                title_comment!()
            ),
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
                    "code": with_description(code(), "The corrected TypeScript source."),
                    "secrets": secrets("The secrets the new run reads, by MCP server or API, as run_script takes them. The failed run's when absent; either way checked again, since the script is new code.")
                },
                "required": ["run_id", "code"],
                "additionalProperties": false
            }),
        },
        "search_tools" => ToolDefinition {
            name: "search_tools",
            description: "Find the tools of `rewind:tools` matching a query, with their TypeScript declarations: for action sets too large to be listed in full.",
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "Words to look for in tool names and descriptions." } },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        "list_secrets" => ToolDefinition {
            name: "list_secrets",
            description: "List the secrets you may attach to a run, by name, never a value: the end user's own (ns/<name>, read in the namespace the run acts for) and the organization's (org/<name>). Give them in `secrets` of run_script, retry_run or save_workflow, by the MCP server or API whose calls need one, such as {\"github\": \"ns/github\"}.",
            input_schema: json!({
                "type": "object",
                "properties": {},
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
                    },
                    "secrets": secrets("The secrets its runs read, by MCP server or API: {\"github\": \"ns/github\"}, the secret github of the namespace each run acts for, or \"org/<name>\", the organization's. Saved with the version: a run of it needs no right on them, only its namespace's ns/ secrets.")
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
                    "runs": { "type": "array", "items": { "type": "string" }, "description": "These runs, by handle or id, rather than a query." },
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

// -------------------------------------------------------------------------------------------
// The MCP set

/// The names of the agent tools of the MCP set, in the order `tools/list` gives them: from an
/// organization to a run.
pub const MCP_TOOLS: [&str; 15] = [
    "list_orgs",
    "list_namespaces",
    "list_action_sets",
    "search_tools",
    "list_secrets",
    "check_script",
    "test_script",
    "run_script",
    "get_run",
    "list_runs",
    "signal_run",
    "cancel_run",
    "retry_run",
    "get_workflow",
    "record_tests",
];

/// What a tool does to the world, for the client: MCP's tool annotations. The client asks the
/// person before a tool that is not read-only, and warns before a destructive one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Annotations {
    /// It changes nothing in Rewind or outside.
    pub read_only: bool,
    /// It may end or undo what exists.
    pub destructive: bool,
    /// Calling it again with the same arguments changes nothing more.
    pub idempotent: bool,
    /// What it starts reaches the world outside Rewind: the action set's servers.
    pub open_world: bool,
}

impl Annotations {
    /// Reads only.
    pub const READ: Self = Self {
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    };
}

/// A tool of the MCP set.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: &'static str,
    /// What a person reads in the client.
    pub title: &'static str,
    /// Written for the model: what the tool does, and where its arguments come from.
    pub description: String,
    pub input_schema: Value,
    pub annotations: Annotations,
    /// The permission it needs in the organization it acts in; none for `list_orgs`.
    pub permission: Option<Permission>,
}

fn org() -> Value {
    json!({
        "type": "string",
        "description": "The organization to act in: its id or its name, as list_orgs gives them. Leave it out when you reach one organization only."
    })
}

fn namespace_for(what: &str) -> Value {
    json!({
        "type": "string",
        "description": format!(
            "The namespace {what}: one end user of the organization, whose runs are kept apart from the others'. A name list_namespaces gives, or a new name (1 to 128 letters, digits, -, _, ., : and @), which creates the namespace at its first run."
        )
    })
}

fn namespace_filter() -> Value {
    json!({
        "type": "string",
        "description": "Only in this namespace, as list_namespaces gives them. Every namespace you may read when absent."
    })
}

fn action_set_required(what: &str) -> Value {
    json!({
        "type": "string",
        "description": format!("The action set {what}: its id, as list_action_sets gives it.")
    })
}

fn action_set_of_script() -> Value {
    json!({
        "type": "string",
        "description": "The action set whose tools the script imports from \"rewind:tools\": its id, as list_action_sets gives it. Without it, the script has only the built-in `webhook`."
    })
}

/// The wait of the MCP set: 30 s when absent, 200 s at most (Claude gives a call 240 s).
fn mcp_wait(schema: &Value) -> Value {
    let described = schema["description"].as_str().unwrap_or_default();
    with_description(
        schema.clone(),
        &format!("{described} 30s when absent, 200s at most."),
    )
}

/// The definition of an agent tool of the MCP set.
pub fn mcp_tool(name: &str) -> Option<McpTool> {
    let annotations = |destructive, idempotent, open_world| Annotations {
        read_only: false,
        destructive,
        idempotent,
        open_world,
    };
    // The tools of their own.
    match name {
        "list_orgs" => {
            return Some(McpTool {
                name: "list_orgs",
                title: "List organizations",
                description: "List the organizations you reach, with their id, their name and what you may do in each. Every other tool acts in one of them, given as `org`; leave `org` out when there is only one. Call it first when the person has not said which organization to use.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                annotations: Annotations::READ,
                permission: None,
            });
        }
        "list_namespaces" => {
            return Some(McpTool {
                name: "list_namespaces",
                title: "List namespaces",
                description: "List the namespaces of an organization, the most active first. A namespace is one end user of the organization: their runs, secrets and limits are kept apart from the others'. Its name is the organization's id for the end user; a display name (\"Acme Corp\") may follow it in parentheses, to find who is who. check_script, test_script and run_script act in the namespace whose name you give as `namespace`; get_run and list_runs read every namespace unless you name one.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "org": org(),
                        "prefix": { "type": "string", "description": "Only the namespaces whose name starts with this, or whose display name does, case aside." }
                    },
                    "additionalProperties": false
                }),
                annotations: Annotations::READ,
                permission: Some(Permission::NamespacesRead),
            });
        }
        "list_action_sets" => {
            return Some(McpTool {
                name: "list_action_sets",
                title: "List action sets",
                description: "List the action sets of an organization: each pins MCP servers and their tools, which scripts import from \"rewind:tools\", and the workflows they import from \"rewind:workflows\". Give an action set's id as `action_set` to search_tools, check_script, test_script and run_script, and read its tools' declarations with search_tools before you write a script.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "org": org() },
                    "additionalProperties": false
                }),
                annotations: Annotations::READ,
                permission: Some(Permission::ActionsRead),
            });
        }
        _ => {}
    }
    let sdk = definition(name)?;
    let mut schema = sdk.input_schema;
    let mut description = sdk.description.to_owned();
    let mut required: Vec<&str> = Vec::new();
    let mut extra = vec![("org", org())];
    let (title, permission, annotations, place) = match name {
        "search_tools" => {
            extra.push(("action_set", action_set_required("whose tools to search")));
            required.push("action_set");
            (
                "Search tools",
                Permission::ActionsRead,
                Annotations::READ,
                "Give the action set as `action_set`, as list_action_sets gives it.",
            )
        }
        "list_secrets" => {
            description = description.replace(
                "run_script, retry_run or save_workflow",
                "run_script or retry_run",
            );
            extra.push((
                "namespace",
                json!({
                    "type": "string",
                    "description": "The namespace whose own secrets (ns/<name>) to list besides the organization's: the end user the run will act for, as list_namespaces gives it. Only the organization's when absent."
                }),
            ));
            (
                "List secrets",
                Permission::SecretsRead,
                Annotations::READ,
                "Give the organization as `org`, and the namespace the run will act for as `namespace`.",
            )
        }
        "check_script" => {
            extra.push(("namespace", namespace_for("the script is checked for")));
            extra.push(("action_set", action_set_of_script()));
            required.push("namespace");
            (
                "Check a script",
                Permission::RunsStart,
                Annotations::READ,
                "Give the namespace it will run in as `namespace`, and its action set as `action_set`.",
            )
        }
        "test_script" => {
            extra.push(("namespace", namespace_for("the tests run for")));
            extra.push(("action_set", action_set_of_script()));
            required.push("namespace");
            description = description.replace(
                "this is how you check a fix or a change of a workflow before save_workflow",
                "this is how you check a fix of a workflow's run before retry_run",
            );
            (
                "Test a script",
                Permission::RunsStart,
                Annotations::READ,
                "Give the namespace as `namespace`, and the script's action set as `action_set`.",
            )
        }
        "run_script" => {
            extra.push(("namespace", namespace_for("the run acts for")));
            extra.push(("action_set", action_set_of_script()));
            required.push("namespace");
            schema["properties"]["wait"] = mcp_wait(&schema["properties"]["wait"]);
            (
                "Run a script",
                Permission::RunsStart,
                annotations(false, false, true),
                "Give the namespace the run acts for as `namespace` (list_namespaces), and the action set whose tools it imports as `action_set` (list_action_sets).",
            )
        }
        "get_run" => {
            extra.push(("namespace", namespace_filter()));
            schema["properties"]["wait"] = mcp_wait(&schema["properties"]["wait"]);
            (
                "Read a run",
                Permission::RunsRead,
                Annotations::READ,
                "Give the organization of the run as `org`.",
            )
        }
        "list_runs" => {
            extra.push(("namespace", namespace_filter()));
            description =
                "List the runs you may read in the organization, most recent first: with `namespace`, those of one end user only."
                    .into();
            (
                "List runs",
                Permission::RunsRead,
                Annotations::READ,
                "Give the organization as `org`.",
            )
        }
        "signal_run" => (
            "Signal a run",
            Permission::RunsControl,
            annotations(false, false, false),
            "Give the organization of the run as `org`.",
        ),
        "cancel_run" => (
            "Cancel a run",
            Permission::RunsControl,
            annotations(true, true, false),
            "Give the organization of the run as `org`.",
        ),
        "retry_run" => (
            "Retry a run",
            Permission::RunsStart,
            annotations(false, false, true),
            "Give the organization of the run as `org`: the new run takes its namespace and its action set.",
        ),
        "get_workflow" => (
            "Read a workflow",
            Permission::ActionsRead,
            Annotations::READ,
            "Give the organization as `org`.",
        ),
        "record_tests" => {
            description = description.replace(
                "pass their digests in save_workflow's `tests`, or { from: <digest>, expect } to give a failed run the expectation it should have met.",
                "pass their digests in test_script's `tests` to run them against a script, or { from: <digest>, expect } to give a failed run the expectation it should have met.",
            );
            (
                "Record tests",
                Permission::RunsRead,
                annotations(false, true, false),
                "Give the organization as `org`.",
            )
        }
        _ => return None,
    };
    for (key, value) in extra {
        schema["properties"][key] = value;
    }
    if !required.is_empty()
        && let Some(list) = schema["required"].as_array_mut()
    {
        list.extend(required.into_iter().map(Value::from));
    }
    Some(McpTool {
        name: sdk.name,
        title,
        description: format!("{description} {place}"),
        input_schema: schema,
        annotations,
        permission: Some(permission),
    })
}

/// Every agent tool of the MCP set.
pub fn mcp_tools() -> Vec<McpTool> {
    MCP_TOOLS.iter().filter_map(|name| mcp_tool(name)).collect()
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
        for name in NEEDS_ACTION_SET {
            assert!(AGENT_TOOLS.contains(&name), "{name}");
        }
    }

    #[test]
    fn the_mcp_set_has_titles_annotations_permissions_and_its_arguments() {
        let tools = mcp_tools();
        assert_eq!(tools.len(), MCP_TOOLS.len());
        assert!(tools.iter().all(|tool| tool.name != "save_workflow"));
        for tool in &tools {
            assert!(!tool.title.is_empty(), "{}", tool.name);
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            assert_eq!(tool.input_schema["additionalProperties"], false);
            assert!(
                !tool.description.contains("save_workflow"),
                "{}: {}",
                tool.name,
                tool.description
            );
            let org = tool.input_schema["properties"].get("org").is_some();
            assert_eq!(org, tool.name != "list_orgs", "{}", tool.name);
            assert_eq!(tool.permission.is_none(), tool.name == "list_orgs");
        }
        let tool = |name: &str| tools.iter().find(|tool| tool.name == name).unwrap();
        let required = |name: &str| tool(name).input_schema["required"].clone();
        assert_eq!(required("run_script"), json!(["code", "namespace"]));
        assert_eq!(required("check_script"), json!(["code", "namespace"]));
        assert_eq!(required("test_script"), json!(["code", "namespace"]));
        assert_eq!(required("search_tools"), json!(["query", "action_set"]));
        assert_eq!(required("get_run"), json!(["run_id"]));
        assert!(tool("get_run").input_schema["properties"]["namespace"].is_object());
        assert!(tool("list_runs").input_schema["properties"]["namespace"].is_object());
        assert!(tool("list_namespaces").input_schema["properties"]["prefix"].is_object());
        // `/v1` finds a workflow by its name alone: no action set to give.
        for name in ["get_workflow", "record_tests"] {
            assert!(
                tool(name).input_schema["properties"]
                    .get("action_set")
                    .is_none(),
                "{name}"
            );
        }
        assert_eq!(required("get_workflow"), json!(["name"]));
        assert!(
            tool("run_script").input_schema["properties"]["wait"]["description"]
                .as_str()
                .unwrap()
                .ends_with("30s when absent, 200s at most.")
        );

        let read_only: Vec<&str> = tools
            .iter()
            .filter(|tool| tool.annotations.read_only)
            .map(|tool| tool.name)
            .collect();
        assert_eq!(
            read_only,
            [
                "list_orgs",
                "list_namespaces",
                "list_action_sets",
                "search_tools",
                "list_secrets",
                "check_script",
                "test_script",
                "get_run",
                "list_runs",
                "get_workflow"
            ]
        );
        let flagged = |flag: fn(&Annotations) -> bool| -> Vec<&str> {
            tools
                .iter()
                .filter(|tool| flag(&tool.annotations))
                .map(|tool| tool.name)
                .collect()
        };
        assert_eq!(flagged(|a| a.destructive), ["cancel_run"]);
        assert_eq!(flagged(|a| a.open_world), ["run_script", "retry_run"]);
        assert_eq!(tool("signal_run").permission, Some(Permission::RunsControl));
        assert_eq!(
            tool("list_namespaces").permission,
            Some(Permission::NamespacesRead)
        );
        assert_eq!(tool("record_tests").permission, Some(Permission::RunsRead));
        assert_eq!(
            tool("list_secrets").permission,
            Some(Permission::SecretsRead)
        );
        assert!(tool("list_secrets").input_schema["properties"]["namespace"].is_object());
        assert!(
            !tool("list_namespaces").description.contains("credentials"),
            "a namespace keeps its secrets"
        );
        for name in ["run_script", "retry_run"] {
            assert_eq!(
                tool(name).input_schema["properties"]["secrets"]["type"],
                "object",
                "{name}"
            );
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
                "secrets",
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
