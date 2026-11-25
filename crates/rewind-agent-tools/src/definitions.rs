//! The agent tools' definitions.
//!
//! The SDK's set (`archi/01-product.md`, "Agent tools"): the names, descriptions and input
//! schemas a model reads. The keys of a schema's objects come sorted, as `serde_json` keeps
//! them.
//!
//! The MCP set (`archi/07-code-architecture.md`, "MCP and OAuth"): the same tools but
//! `update_workflow`, with `list_organizations`, `list_namespaces` and `list_action_sets` to find
//! what the caller reaches, the organization, the namespace and the action set as arguments, and
//! for each tool a title, its annotations and the permission it needs.

use rewind_api::access::Permission;
use serde_json::{Value, json};

/// The names of the agent tools of the SDK's set, in the SDK's order.
pub const AGENT_TOOLS: [&str; 13] = [
    "run_workflow",
    "check_workflow",
    "test_workflow",
    "get_run",
    "list_runs",
    "signal_run",
    "cancel_run",
    "retry_run",
    "search_actions",
    "list_secrets",
    "update_workflow",
    "get_workflow",
    "record_tests",
];

/// The tools that need an action set: the others reach a workflow by its id or its name.
pub const NEEDS_ACTION_SET: [&str; 1] = ["search_actions"];

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

/// What `run_workflow` and `check_workflow` ask of a code's first lines: the comment a workflow
/// without a name shows as (`archi/04-tools.md`, "Workflow names").
macro_rules! top_comment {
    () => {
        "Start the code with a comment that says what it does:\n/** <what it does, in a few words>\n *\n * <more, if needed> */\nIts first line, such as \"Triage new GitHub issues\", is how the workflow shows until it has a name; the lines after it describe it."
    };
}

/// What the SDK's `run_workflow` says of changed code, which the MCP set, without
/// `update_workflow`, leaves out.
const NEXT_VERSION: &str =
    ": to make a workflow's next version, give its new code to update_workflow";

/// What the SDK's `retry_run` says of a correction, which the MCP set leaves out.
const RETRY_VERSION: &str = ": update_workflow makes one";

fn code() -> Value {
    json!({
        "type": "string",
        "description": "The TypeScript source: one ES module whose default export is `async function main(input)`. It imports actions from \"rewind:actions\", workflows from \"rewind:workflows\", and `sleep`, `signal`, `signalUrl` from \"rewind\". Its input and output are JSON."
    })
}

/// A workflow named in a call.
fn workflow(description: &str) -> Value {
    json!({ "type": "string", "description": description })
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
        "enum": ["queued", "running", "waiting", "completed", "failed", "cancelled", "aborted"]
    })
}

/// What a run reads: the secrets it uses, by MCP server or API.
fn secrets(description: &str) -> Value {
    json!({
        "type": "object",
        "description": description,
        "additionalProperties": { "type": "string", "description": "org/<name> or ns/<name>." }
    })
}

/// The `secrets` of `run_workflow`.
const RUN_SECRETS: &str = "With `code`, the secrets the run reads, by the MCP server or API whose calls need one: {\"github\": \"ns/github\"} for the end user's own secret, read in the namespace the run acts for, or \"org/<name>\" for the organization's. Names only, never a value: list_secrets gives those you may use. A code that imports a server needing a secret is refused without one. A run of a `workflow` takes none: it uses those its version names.";

fn with_description(mut schema: Value, description: &str) -> Value {
    schema["description"] = json!(description);
    schema
}

/// An answer of a test's world to the calls of one action.
fn rule() -> Value {
    json!({
        "type": "object",
        "description": "An answer to the calls of the action whose args contain `args`: the result in `returns`, or a failure in `error`.",
        "properties": {
            "args": {
                "type": "object",
                "description": "The call's args contain these (objects compared key by key). Any call of the action when absent."
            },
            "returns": { "description": "The result of the call, any JSON value, null included." },
            "error": {
                "type": "object",
                "description": "The call fails with an ActionError instead.",
                "properties": {
                    "message": { "type": "string" },
                    "kind": {
                        "type": "string",
                        "enum": ["error", "unavailable", "timeout", "outcome_unknown"],
                        "description": "\"error\" when absent."
                    },
                    "data": { "description": "Any JSON value: the ActionError's `data`." }
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
                "enum": ["completed", "failed", "cancelled", "aborted", "waiting"],
                "description": "How the run ended; \"waiting\" for a run that can go no further in virtual time without the outside world (one waiting for a signal no test sends, for example)."
            },
            "output": {
                "description": "Values the output must contain: objects are compared key by key, recursively, so give only the keys that matter; other values must be equal."
            },
            "error": {
                "type": "object",
                "description": "The run failed with an error of this name (such as ActionError), whose message contains `message`.",
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
                        "action": { "type": "string", "description": "The action, such as \"github.close_issue\"." },
                        "args": {
                            "type": "object",
                            "description": "Only the calls whose args contain these. Every call of the action when absent."
                        },
                        "times": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Exactly this many matching calls. At least one when absent; 0 says the call must not happen."
                        }
                    },
                    "required": ["action"],
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
        "description": "A test: the world the run meets and what must hold at the end. It never names the code, so the same test runs against the next version.",
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
                "description": "Everything the code may touch. A call that nothing here answers fails with an ActionError (no recorded response) and fails the test, even if the code catches the error.",
                "properties": {
                    "actions": {
                        "type": "object",
                        "description": "By action name, such as \"github.list_issues\": one rule answering every call, or a list of rules tried in order. A start of a workflow, \"workflows.<name>.start\", is answered the same way: `returns` is the child's output, `error` its failure.",
                        "additionalProperties": { "anyOf": [rule(), { "type": "array", "items": rule() }] }
                    },
                    "handlers": {
                        "type": "string",
                        "description": "TypeScript handlers, for answers computed from the args: a module that imports nothing and default-exports its handlers by action name or by group, such as `export default { \"github.get_issue\": ({ number }) => ({ number, state: \"open\" }) }`. What a handler throws fails the call. Tried after `actions`."
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
                            "description": "\"waiting\" (the default): once the run waits for something other than a timer, which virtual time moves by itself. Or a duration of virtual time after the start, such as \"2h\"."
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
                    "test": { "type": "string", "description": "The digest of the stored test." },
                    "expect": expect()
                },
                "required": ["test", "expect"],
                "additionalProperties": false
            }
        ]
    })
}

fn tests() -> Value {
    json!({
        "type": "array",
        "description": "Tests, each written in full, the digest of a recorded test, or { test: <digest>, expect } for a recorded test with a new expectation.",
        "items": test()
    })
}

/// The changes of a suite `update_workflow` takes beside new tests.
fn replace() -> Value {
    json!({
        "type": "array",
        "description": "Tests of its suite to replace because their state or their expected result is out of date: by a whole `new` test, which takes the name, or by the same test with a new `expect`.",
        "items": {
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The name of a test of its suite." },
                "reason": { "type": "string", "description": "Why it changes: kept with the workflow." },
                "new": with_description(test(), "The test that replaces it."),
                "expect": with_description(expect(), "Its new expectation; the rest of the test is kept.")
            },
            "required": ["name", "reason"],
            "additionalProperties": false
        }
    })
}

fn retire() -> Value {
    json!({
        "type": "array",
        "description": "Tests of its suite to drop because the behavior they check is deprecated.",
        "items": {
            "type": "object",
            "properties": {
                "names": { "type": "array", "items": { "type": "string" }, "description": "The names of tests of its suite." },
                "reason": { "type": "string", "description": "Why they no longer hold: kept with the workflow." }
            },
            "required": ["names", "reason"],
            "additionalProperties": false
        }
    })
}

/// The definition of an agent tool.
pub fn definition(name: &str) -> Option<ToolDefinition> {
    Some(match name {
        "run_workflow" => ToolDefinition {
            name: "run_workflow",
            description: concat!(
                "Run a workflow: TypeScript code, type-checked first, or an existing workflow by its id or its name. The code calls the actions declared in `rewind:actions` and runs durably: it can sleep for days and wait for signals. Code that no workflow of the namespace or the organization has becomes a new workflow, with an id (wf_…) and its version v1; the same code run again is a run of the same workflow, and changed code is another workflow: to make a workflow's next version, give its new code to update_workflow. Code that does not type-check is refused with its diagnostics, before anything runs. The result names the workflow by its id and its version. When the run ends within `wait`, you get its output and last log lines, or its error with the failing line and the transcript up to the failure. Otherwise you get the run's handle and what the run waits for: follow it with get_run, send it a signal with signal_run, or cancel it with cancel_run. ",
                top_comment!()
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": with_description(code(), "The TypeScript source to run: one ES module whose default export is `async function main(input)`. It imports actions from \"rewind:actions\", workflows from \"rewind:workflows\", and `sleep`, `signal`, `signalUrl` from \"rewind\". Its input and output are JSON. Give `code` or `workflow`, not both."),
                    "workflow": workflow("An existing workflow to run instead of code: its id (wf_…) or its name, such as triage, for its latest version, or triage@v12 for one version."),
                    "input": { "description": "The input passed to `main`, any JSON value. {} when absent." },
                    "wait": wait(),
                    "secrets": secrets(RUN_SECRETS)
                },
                "additionalProperties": false
            }),
        },
        "check_workflow" => ToolDefinition {
            name: "check_workflow",
            description: concat!(
                "Type-check TypeScript code against the sandbox and the action declarations, without running it. Returns the diagnostics with their location and a code frame. It also says which workflow and version already have this code, if one does: running the code is then a run of that workflow. It says when the code has no comment at its top. It creates nothing. ",
                top_comment!()
            ),
            input_schema: json!({
                "type": "object",
                "properties": { "code": code() },
                "required": ["code"],
                "additionalProperties": false
            }),
        },
        "test_workflow" => ToolDefinition {
            name: "test_workflow",
            description: "Run tests end to end, in virtual time: no action is really called and sleeps take no time. Each test gives the world the run meets (its input, the answers of the actions it may call, the signals sent to it) and what must hold at the end (its status, its output or error, the calls it made). A call the world does not answer fails the test. Give `code` with `tests`; with `workflow`, that workflow's suite runs too, against your code: this is how you check a fix or a change of a workflow before update_workflow. A `workflow` without `code` runs its suite and your tests against its version. It creates and changes nothing. Returns whether each test passed and, for those that did not, why: stale (it names actions, answers or args your actions no longer have; not run), incompatible (the workflow's input or signal schemas refuse it; not run) or failed (an expectation does not hold), with the run's outcome and its transcript.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "code": with_description(code(), "The TypeScript source to test. The version's code when absent, with `workflow`."),
                    "tests": tests(),
                    "workflow": workflow("A workflow whose suite runs too: its id (wf_…) or its name, such as triage, for its latest version, or triage@v12 for one version. Its interface checks every test against its input and signal schemas.")
                },
                "additionalProperties": false
            }),
        },
        "get_run" => ToolDefinition {
            name: "get_run",
            description: "Read a run: its status, what it waits for or how it ended, and its transcript. With `event`, one event in full instead. With `offset`, the next page of an output (or event) too large for one answer.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "run_id": run_id(),
                    "event": { "type": "integer", "minimum": 0, "description": "The index of an event of the history, as the transcript shows it." },
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
            description: "Send a signal to a run: it resolves the code's `signal(name)`, now or the next time the code waits for that name. A run that has ended refuses it.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "run_id": run_id(),
                    "name": { "type": "string", "description": "The signal's name, as the code waits for it." },
                    "payload": { "description": "The signal's payload, any JSON value. null when absent." }
                },
                "required": ["run_id", "name"],
                "additionalProperties": false
            }),
        },
        "cancel_run" => ToolDefinition {
            name: "cancel_run",
            description: "Cancel a run: every pending await of the code rejects with CancelledError, and the code gets one chance to clean up.",
            input_schema: json!({
                "type": "object",
                "properties": { "run_id": run_id() },
                "required": ["run_id"],
                "additionalProperties": false
            }),
        },
        "retry_run" => ToolDefinition {
            name: "retry_run",
            description: "Start a new run from a failed one with corrected code. The calls the failed run already made are not made again: their recorded results are reused while the corrected code makes the same calls, and it continues live from the first difference. The suite of the run's version runs against the corrected code first, and the retry is refused if a test does not pass: check the fix with test_workflow({ code, workflow }) before. The correction does not become a version of the run's workflow: update_workflow makes one.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "run_id": with_description(run_id(), "The run to correct."),
                    "code": with_description(code(), "The corrected TypeScript source."),
                    "secrets": secrets("The secrets the new run reads, by MCP server or API, as run_workflow takes them with `code`. The failed run's when absent; either way checked again, since the code is new code.")
                },
                "required": ["run_id", "code"],
                "additionalProperties": false
            }),
        },
        "search_actions" => ToolDefinition {
            name: "search_actions",
            description: "Find the actions of `rewind:actions` matching a query, with their TypeScript declarations: for action sets too large to be listed in full.",
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "Words to look for in action names and descriptions." } },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        "list_secrets" => ToolDefinition {
            name: "list_secrets",
            description: "List the secrets your runs may use, by name, never a value: the end user's own (ns/<name>, read in the namespace the run acts for) and the organization's (org/<name>). Give them in `secrets` of run_workflow, retry_run or update_workflow, by the MCP server or API whose calls need one, such as {\"github\": \"ns/github\"}.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        "update_workflow" => ToolDefinition {
            name: "update_workflow",
            description: "Change a workflow, named by its id or its name. A `name` and a `description` keep its version; null removes them. New `code`, an interface (`input`, `output`, `signals`: JSON Schemas) or the `secrets` its runs use make its next version. Its code is type-checked against your actions and its interface: its default export must take the input and return the output, and `signal(name)` only takes a declared name and returns that signal's payload. Then its suite runs, and the version is made only if every test passes. The suite is its tests, minus those you `retire`, with those you `replace` replaced, plus your new `tests`. There is no way around a failing test. To change a behavior a test pins on purpose, retire the test when the behavior is deprecated, or replace it when its state or its expected result is out of date, always with a reason: the reasons stay with the workflow for whoever reads it next. `tests`, `replace` and `retire` alone change its suite without a version, once each new test passes against its latest version. Read the suite with get_workflow, and check a change with test_workflow({ code, workflow }) first. Then run it by name with run_workflow({ workflow }); code starts it from \"rewind:workflows\" once an action set pins it.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workflow": workflow("The workflow to change: its id (wf_…) or its name, such as triage."),
                    "name": {
                        "type": ["string", "null"],
                        "description": "Its new name: what code imports from \"rewind:workflows\" and runs start it by, a JavaScript identifier that is not a reserved word and does not start with wf_, such as triage, unique among the workflows of its namespace or organization. null removes it."
                    },
                    "description": {
                        "type": ["string", "null"],
                        "description": "What the workflow does, shown with it; at most 2,000 characters. null removes it."
                    },
                    "code": with_description(
                        code(),
                        "Its new TypeScript source: one ES module whose default export is `async function main(input)`, taking the input and returning the output. It imports actions from \"rewind:actions\" and `sleep`, `signal`, `signalUrl` from \"rewind\". Its latest version's when absent."
                    ),
                    "input": { "type": ["object", "boolean"], "description": "The JSON Schema of the input of `main`. Each of `input`, `output` and `signals` you leave out keeps its latest version's." },
                    "output": { "type": ["object", "boolean"], "description": "The JSON Schema of what `main` returns." },
                    "signals": {
                        "type": "object",
                        "description": "The signals its runs accept, by name, each with the JSON Schema of its payload, such as {\"approval\": {\"type\": \"object\", \"properties\": {\"close\": {\"type\": \"boolean\"}}, \"required\": [\"close\"]}}. {} for none; names starting with \"rewind:\" are reserved.",
                        "additionalProperties": { "type": ["object", "boolean"] }
                    },
                    "secrets": secrets("The secrets its runs read, by MCP server or API: {\"github\": \"ns/github\"}, the secret github of the namespace each run acts for, or \"org/<name>\", the organization's. Part of the version: a run of it needs no right on them, only its namespace's ns/ secrets. Its latest version's when absent."),
                    "tests": with_description(
                        tests(),
                        "New tests for its suite: each written in full, the digest of a test record_tests recorded, or { test: <digest>, expect } for a recorded test with the expectation it should have met."
                    ),
                    "replace": replace(),
                    "retire": retire()
                },
                "required": ["workflow"],
                "additionalProperties": false
            }),
        },
        "get_workflow" => ToolDefinition {
            name: "get_workflow",
            description: "Read a workflow, by its id or its name: its name and description, whether it belongs to the namespace or to the organization, the organization's workflow it was forked from, the code of its latest version, its interface (input, output and signal schemas), the secrets its runs use, its versions, its suite with every test in full, and the tests that left it, retired or replaced, with the reason of each. Read it before you repair or extend a workflow you did not write: its tests say which behaviors it must keep, and their reasons which changes were made on purpose.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workflow": workflow("The workflow: its id (wf_…) or its name, such as triage; triage@v12 reads its version v12.")
                },
                "required": ["workflow"],
                "additionalProperties": false
            }),
        },
        "record_tests" => ToolDefinition {
            name: "record_tests",
            description: "Record tests from past runs of a workflow, so a new version must keep doing what the old ones did on real inputs. Its completed and failed runs are grouped by the path they took (the actions they called in order, the signals they waited for, how they ended), and the newest run of each path becomes a test: its input, its calls and signals as they happened, and the same status, output or error and number of calls per action as expected. The tests are stored but join no suite: pass their digests in update_workflow's `tests`, or { test: <digest>, expect } to give a failed run the expectation it should have met.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "workflow": workflow("The workflow: its id (wf_…) or its name, such as triage."),
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
    "list_organizations",
    "list_namespaces",
    "list_action_sets",
    "search_actions",
    "list_secrets",
    "check_workflow",
    "test_workflow",
    "run_workflow",
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
    /// The permission it needs in the organization it acts in; none for `list_organizations`.
    pub permission: Option<Permission>,
}

fn organization() -> Value {
    json!({
        "type": "string",
        "description": "The organization to act in: its id or its name, as list_organizations gives them. Leave it out when you reach one organization only."
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

/// Where `get_workflow` and `record_tests` resolve a workflow's name first.
fn namespace_of_names() -> Value {
    json!({
        "type": "string",
        "description": "The namespace whose workflows a name finds first, before the organization's: the end user, as list_namespaces gives them. Only the organization's workflows by name when absent; an id finds a workflow either way."
    })
}

fn action_set_required(what: &str) -> Value {
    json!({
        "type": "string",
        "description": format!("The action set {what}: its id, as list_action_sets gives it.")
    })
}

fn action_set_of_code() -> Value {
    json!({
        "type": "string",
        "description": "With `code`, the action set whose actions the code imports from \"rewind:actions\": its id, as list_action_sets gives it. Without it, the code has only the built-in `signalUrl`. A `workflow` runs with its version's action set."
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
        "list_organizations" => {
            return Some(McpTool {
                name: "list_organizations",
                title: "List organizations",
                description: "List the organizations you reach, with their id, their name and what you may do in each. Every other tool acts in one of them, given as `organization`; leave `organization` out when there is only one. Call it first when the person has not said which organization to use.".into(),
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
                description: "List the namespaces of an organization, the most active first. A namespace is one end user of the organization: their runs, workflows, secrets and limits are kept apart from the others'. Its name is the organization's id for the end user; a display name (\"Acme Corp\") may follow it in parentheses, to find who is who. check_workflow, test_workflow and run_workflow act in the namespace whose name you give as `namespace`; get_run and list_runs read every namespace unless you name one.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "organization": organization(),
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
                description: "List the action sets of an organization: each pins MCP servers and their actions, which code imports from \"rewind:actions\", and the organization's workflows it imports from \"rewind:workflows\". Give an action set's id as `action_set` to search_actions, check_workflow, test_workflow and run_workflow, and read its actions' declarations with search_actions before you write code.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "organization": organization() },
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
    let mut extra = vec![("organization", organization())];
    let (title, permission, annotations, place) = match name {
        "search_actions" => {
            extra.push(("action_set", action_set_required("whose actions to search")));
            required.push("action_set");
            (
                "Search actions",
                Permission::ActionsRead,
                Annotations::READ,
                "Give the action set as `action_set`, as list_action_sets gives it.",
            )
        }
        "list_secrets" => {
            description = description.replace(
                "run_workflow, retry_run or update_workflow",
                "run_workflow or retry_run",
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
                "Give the organization as `organization`, and the namespace the run will act for as `namespace`.",
            )
        }
        "check_workflow" => {
            extra.push(("namespace", namespace_for("the code is checked for")));
            extra.push(("action_set", action_set_of_code()));
            required.push("namespace");
            (
                "Check a workflow",
                Permission::RunsStart,
                Annotations::READ,
                "Give the namespace it will run in as `namespace`, and its action set as `action_set`.",
            )
        }
        "test_workflow" => {
            extra.push(("namespace", namespace_for("the tests run for")));
            extra.push(("action_set", action_set_of_code()));
            required.push("namespace");
            description = description.replace(
                "this is how you check a fix or a change of a workflow before update_workflow",
                "this is how you check a fix of a workflow's run before retry_run",
            );
            (
                "Test a workflow",
                Permission::RunsStart,
                Annotations::READ,
                "Give the namespace as `namespace`, and with `code` its action set as `action_set`.",
            )
        }
        "run_workflow" => {
            extra.push(("namespace", namespace_for("the run acts for")));
            extra.push(("action_set", action_set_of_code()));
            required.push("namespace");
            description = description.replace(NEXT_VERSION, "");
            schema["properties"]["wait"] = mcp_wait(&schema["properties"]["wait"]);
            (
                "Run a workflow",
                Permission::RunsStart,
                annotations(false, false, true),
                "Give the namespace the run acts for as `namespace` (list_namespaces), and with `code` the action set whose actions it imports as `action_set` (list_action_sets).",
            )
        }
        "get_run" => {
            extra.push(("namespace", namespace_filter()));
            schema["properties"]["wait"] = mcp_wait(&schema["properties"]["wait"]);
            (
                "Read a run",
                Permission::RunsRead,
                Annotations::READ,
                "Give the organization of the run as `organization`.",
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
                "Give the organization as `organization`.",
            )
        }
        "signal_run" => (
            "Signal a run",
            Permission::RunsControl,
            annotations(false, false, false),
            "Give the organization of the run as `organization`.",
        ),
        "cancel_run" => (
            "Cancel a run",
            Permission::RunsControl,
            annotations(true, true, false),
            "Give the organization of the run as `organization`.",
        ),
        "retry_run" => {
            description = description.replace(RETRY_VERSION, "");
            (
                "Retry a run",
                Permission::RunsStart,
                annotations(false, false, true),
                "Give the organization of the run as `organization`: the new run takes its namespace and its action set.",
            )
        }
        "get_workflow" => {
            extra.push(("namespace", namespace_of_names()));
            (
                "Read a workflow",
                Permission::RunsRead,
                Annotations::READ,
                "Give the organization as `organization`, and the namespace of a namespace's workflow as `namespace`.",
            )
        }
        "record_tests" => {
            extra.push(("namespace", namespace_of_names()));
            description = description.replace(
                "pass their digests in update_workflow's `tests`, or { test: <digest>, expect } to give a failed run the expectation it should have met.",
                "pass their digests in test_workflow's `tests` to run them against a code, or { test: <digest>, expect } to give a failed run the expectation it should have met.",
            );
            (
                "Record tests",
                Permission::RunsRead,
                annotations(false, true, false),
                "Give the organization as `organization`, and the namespace of a namespace's workflow as `namespace`.",
            )
        }
        _ => return None,
    };
    for (key, value) in extra {
        schema["properties"][key] = value;
    }
    if !required.is_empty() {
        match schema["required"].as_array_mut() {
            Some(list) => list.extend(required.into_iter().map(Value::from)),
            None => schema["required"] = json!(required),
        }
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
        // The words of the workflow model only.
        for definition in super::definitions() {
            let text = format!("{} {}", definition.description, definition.input_schema);
            let words: Vec<&str> = text
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .collect();
            for word in [
                "save_workflow",
                "script",
                "scripts",
                "title",
                "webhook",
                "toolset",
                "save",
                "saved",
            ] {
                assert!(!words.contains(&word), "{}: {word}", definition.name);
            }
            assert!(!text.contains("{ from"), "{}", definition.name);
        }
    }

    #[test]
    fn a_workflow_is_named_by_its_id_or_its_name() {
        let required = |name: &str| definition(name).unwrap().input_schema["required"].clone();
        // Code or a workflow: the handler says which is missing.
        assert_eq!(required("run_workflow"), Value::Null);
        assert_eq!(required("test_workflow"), Value::Null);
        assert_eq!(required("check_workflow"), json!(["code"]));
        for name in ["update_workflow", "get_workflow", "record_tests"] {
            assert_eq!(required(name), json!(["workflow"]), "{name}");
        }
        for name in [
            "run_workflow",
            "test_workflow",
            "update_workflow",
            "get_workflow",
            "record_tests",
        ] {
            let described =
                definition(name).unwrap().input_schema["properties"]["workflow"]["description"]
                    .as_str()
                    .unwrap()
                    .to_owned();
            assert!(described.contains("its id (wf_…) or its name"), "{name}");
        }
        // The comment a workflow without a name shows as.
        for name in ["run_workflow", "check_workflow"] {
            assert!(
                definition(name)
                    .unwrap()
                    .description
                    .contains("is how the workflow shows until it has a name"),
                "{name}"
            );
        }
        let update = definition("update_workflow").unwrap();
        assert_eq!(
            update.input_schema["properties"]["name"]["type"],
            json!(["string", "null"])
        );
        assert!(
            update
                .description
                .contains("the version is made only if every test passes")
        );
        assert!(
            update
                .description
                .contains("retire the test when the behavior is deprecated")
        );
        assert!(
            update
                .description
                .contains("replace it when its state or its expected result is out of date")
        );
    }

    #[test]
    fn the_mcp_set_has_titles_annotations_permissions_and_its_arguments() {
        let tools = mcp_tools();
        assert_eq!(tools.len(), MCP_TOOLS.len());
        assert!(tools.iter().all(|tool| tool.name != "update_workflow"));
        for tool in &tools {
            assert!(!tool.title.is_empty(), "{}", tool.name);
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            assert_eq!(tool.input_schema["additionalProperties"], false);
            // No tool of the MCP set changes a workflow.
            assert!(
                !tool.description.contains("update_workflow"),
                "{}: {}",
                tool.name,
                tool.description
            );
            let org = tool.input_schema["properties"]
                .get("organization")
                .is_some();
            assert_eq!(org, tool.name != "list_organizations", "{}", tool.name);
            assert_eq!(tool.permission.is_none(), tool.name == "list_organizations");
        }
        let tool = |name: &str| tools.iter().find(|tool| tool.name == name).unwrap();
        let required = |name: &str| tool(name).input_schema["required"].clone();
        assert_eq!(required("run_workflow"), json!(["namespace"]));
        assert_eq!(required("check_workflow"), json!(["code", "namespace"]));
        assert_eq!(required("test_workflow"), json!(["namespace"]));
        assert_eq!(required("search_actions"), json!(["query", "action_set"]));
        assert_eq!(required("get_run"), json!(["run_id"]));
        assert!(tool("get_run").input_schema["properties"]["namespace"].is_object());
        assert!(tool("list_runs").input_schema["properties"]["namespace"].is_object());
        assert!(tool("list_namespaces").input_schema["properties"]["prefix"].is_object());
        // `/v1` finds a workflow by its id or its name, a name in the namespace first: no action
        // set to give.
        for name in ["get_workflow", "record_tests"] {
            let properties = &tool(name).input_schema["properties"];
            assert!(properties.get("action_set").is_none(), "{name}");
            assert!(properties["namespace"].is_object(), "{name}");
            assert_eq!(required(name), json!(["workflow"]), "{name}");
            assert_eq!(tool(name).permission, Some(Permission::RunsRead), "{name}");
        }
        assert!(
            tool("run_workflow").input_schema["properties"]["wait"]["description"]
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
                "list_organizations",
                "list_namespaces",
                "list_action_sets",
                "search_actions",
                "list_secrets",
                "check_workflow",
                "test_workflow",
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
        assert_eq!(flagged(|a| a.open_world), ["run_workflow", "retry_run"]);
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
        for name in ["run_workflow", "retry_run"] {
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
        let test_workflow = definition("test_workflow").unwrap();
        let any_of = &test_workflow.input_schema["properties"]["tests"]["items"]["anyOf"];
        let (inline, digest, from) = (&any_of[0], &any_of[1], &any_of[2]);
        assert_eq!(
            keys(&inline["properties"]),
            sorted(&[
                "name", "input", "start", "seed", "world", "signals", "expect"
            ])
        );
        assert_eq!(
            keys(&inline["properties"]["world"]["properties"]),
            sorted(&["actions", "handlers"])
        );
        let rule = &inline["properties"]["world"]["properties"]["actions"]["additionalProperties"]
            ["anyOf"][0];
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
            sorted(&["action", "args", "times"])
        );
        assert_eq!(digest["type"], "string");
        assert_eq!(from["type"], "object");
        assert_eq!(from["required"], json!(["test", "expect"]));

        let update = definition("update_workflow").unwrap();
        assert_eq!(
            keys(&update.input_schema["properties"]),
            sorted(&[
                "workflow",
                "name",
                "description",
                "code",
                "input",
                "output",
                "signals",
                "tests",
                "replace",
                "retire",
                "secrets",
            ])
        );
        let replace = &update.input_schema["properties"]["replace"]["items"];
        assert_eq!(
            keys(&replace["properties"]),
            sorted(&["name", "reason", "new", "expect"])
        );
        assert_eq!(replace["required"], json!(["name", "reason"]));
    }
}
