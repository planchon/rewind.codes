/** The names of the agent tools. */
export type AgentToolName =
  | "run_workflow"
  | "check_workflow"
  | "test_workflow"
  | "get_run"
  | "list_runs"
  | "signal_run"
  | "cancel_run"
  | "retry_run"
  | "search_actions"
  | "list_secrets"
  | "update_workflow"
  | "get_workflow"
  | "record_tests";

export const AGENT_TOOL_NAMES: readonly AgentToolName[] = [
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

/** The tools that need an action set: the others reach a workflow by its id or its name. */
export const NEEDS_ACTION_SET: ReadonlySet<AgentToolName> = new Set(["search_actions"]);

/** The JSON Schema of a tool's arguments, as the model APIs take it: always an object. */
export interface JsonSchema {
  type: "object";
  properties?: Record<string, unknown>;
  required?: string[];
  [keyword: string]: unknown;
}

/** A tool for a model, independent of any model API. The adapters turn it into each API's format. */
export interface AgentToolDefinition {
  name: AgentToolName;
  description: string;
  inputSchema: JsonSchema;
}

const RUN_ID = {
  type: "string",
  description:
    "The run, by its handle (such as dana/triage/06bqx3k9f2mt) or its id, as a previous tool result gave it.",
};

/** What `run_workflow` and `check_workflow` ask of a code's first lines: the comment a workflow without a name shows as. */
const TOP_COMMENT =
  'Start the code with a comment that says what it does:\n/** <what it does, in a few words>\n *\n * <more, if needed> */\nIts first line, such as "Triage new GitHub issues", is how the workflow shows until it has a name; the lines after it describe it.';

const CODE = {
  type: "string",
  description:
    'The TypeScript source: one ES module whose default export is `async function main(input)`. It imports actions from "rewind:actions", workflows from "rewind:workflows", and `sleep`, `signal`, `signalUrl` from "rewind". Its input and output are JSON.',
};

/** A workflow named in a call. */
function workflow(description: string): Record<string, unknown> {
  return { type: "string", description };
}

const WAIT = {
  type: "string",
  description:
    'How long to wait for the run to end before answering, such as "30s" or "2m". If the run is still going then, you get its id and what it waits for.',
};

const STATUS = {
  type: "string",
  enum: ["queued", "running", "waiting", "completed", "failed", "cancelled", "aborted"],
};

/** What a run reads: the secrets it uses, by MCP server or API. */
function secrets(description: string): Record<string, unknown> {
  return {
    type: "object",
    description,
    additionalProperties: { type: "string", description: "org/<name> or ns/<name>." },
  };
}

/** The `secrets` of `run_workflow`. */
const RUN_SECRETS =
  'With `code`, the secrets the run reads, by the MCP server or API whose calls need one: {"github": "ns/github"} for the end user\'s own secret, read in the namespace the run acts for, or "org/<name>" for the organization\'s. Names only, never a value: list_secrets gives those you may use. A code that imports a server needing a secret is refused without one. A run of a `workflow` takes none: it uses those its version names.';

/** An answer of a test's world to the calls of one action. */
const RULE = {
  type: "object",
  description:
    "An answer to the calls of the action whose args contain `args`: the result in `returns`, or a failure in `error`.",
  properties: {
    args: {
      type: "object",
      description: "The call's args contain these (objects compared key by key). Any call of the action when absent.",
    },
    returns: { description: "The result of the call, any JSON value, null included." },
    error: {
      type: "object",
      description: "The call fails with an ActionError instead.",
      properties: {
        message: { type: "string" },
        kind: {
          type: "string",
          enum: ["error", "unavailable", "timeout", "outcome_unknown"],
          description: '"error" when absent.',
        },
        data: { description: "Any JSON value: the ActionError's `data`." },
      },
      required: ["message"],
      additionalProperties: false,
    },
    times: {
      type: "integer",
      minimum: 0,
      description: "How many calls it answers; the next rule answers the calls after. Any number of calls when absent.",
    },
  },
  additionalProperties: false,
};

/** What must hold at the end of a test. */
const EXPECT = {
  type: "object",
  description:
    "What the outside must see at the end of the run. Anything left out is not checked, and a test never checks a variable, a helper or a log line.",
  properties: {
    status: {
      type: "string",
      enum: ["completed", "failed", "cancelled", "aborted", "waiting"],
      description:
        'How the run ended; "waiting" for a run that can go no further in virtual time without the outside world (one waiting for a signal no test sends, for example).',
    },
    output: {
      description:
        "Values the output must contain: objects are compared key by key, recursively, so give only the keys that matter; other values must be equal.",
    },
    error: {
      type: "object",
      description: "The run failed with an error of this name (such as ActionError), whose message contains `message`.",
      properties: { name: { type: "string" }, message: { type: "string" } },
      required: ["name"],
      additionalProperties: false,
    },
    calls: {
      type: "array",
      description:
        "Calls that must happen, or must not. The calls the world answers that are not listed are allowed: a harmless extra read breaks no test.",
      items: {
        type: "object",
        properties: {
          action: { type: "string", description: 'The action, such as "github.close_issue".' },
          args: {
            type: "object",
            description: "Only the calls whose args contain these. Every call of the action when absent.",
          },
          times: {
            type: "integer",
            minimum: 0,
            description: "Exactly this many matching calls. At least one when absent; 0 says the call must not happen.",
          },
        },
        required: ["action"],
        additionalProperties: false,
      },
    },
    order: {
      type: "string",
      enum: ["any", "listed"],
      description:
        '"listed": the first call matching each entry of `calls` comes in the order of the entries (those with times 0 aside). "any" when absent.',
    },
  },
  additionalProperties: false,
};

/** A test written in full. */
const INLINE_TEST = {
  type: "object",
  description:
    "A test: the world the run meets and what must hold at the end. It never names the code, so the same test runs against the next version.",
  properties: {
    name: { type: "string", description: "What it checks, in a few words; unique among the tests." },
    input: { description: "The run's input, given to `main`. {} when absent." },
    start: {
      type: "string",
      description:
        'When the run starts, in virtual time, as an RFC 3339 time; "2026-01-01T00:00:00Z" when absent. Date.now() starts there, and sleeps and timeouts take no real time.',
    },
    seed: {
      type: "string",
      description:
        "The run's seed, 64 lowercase hex digits: where its random values come from. A fixed seed when absent.",
    },
    world: {
      type: "object",
      description:
        "Everything the code may touch. A call that nothing here answers fails with an ActionError (no recorded response) and fails the test, even if the code catches the error.",
      properties: {
        actions: {
          type: "object",
          description:
            'By action name, such as "github.list_issues": one rule answering every call, or a list of rules tried in order. A start of a workflow, "workflows.<name>.start", is answered the same way: `returns` is the child\'s output, `error` its failure.',
          additionalProperties: { anyOf: [RULE, { type: "array", items: RULE }] },
        },
        handlers: {
          type: "string",
          description:
            'TypeScript handlers, for answers computed from the args: a module that imports nothing and default-exports its handlers by action name or by group, such as `export default { "github.get_issue": ({ number }) => ({ number, state: "open" }) }`. What a handler throws fails the call. Tried after `actions`.',
        },
      },
      additionalProperties: false,
    },
    signals: {
      type: "array",
      description: "Signals sent to the run, in order.",
      items: {
        type: "object",
        properties: {
          name: { type: "string" },
          payload: { description: "Any JSON value." },
          when: {
            type: "string",
            description:
              '"waiting" (the default): once the run waits for something other than a timer, which virtual time moves by itself. Or a duration of virtual time after the start, such as "2h".',
          },
        },
        required: ["name"],
        additionalProperties: false,
      },
    },
    expect: EXPECT,
  },
  required: ["name"],
  additionalProperties: false,
};

/** A test as the tools take it: in full, by digest, or a stored test with a new expectation. */
const TEST = {
  anyOf: [
    INLINE_TEST,
    { type: "string", description: "The digest of a stored test, as record_tests or get_workflow gave it." },
    {
      type: "object",
      description:
        "A stored test with a new expectation: the same world (a recorded run's input, calls and signals), checked against `expect`. It turns a failed run into a regression test without reading its calls.",
      properties: {
        test: { type: "string", description: "The digest of the stored test." },
        expect: EXPECT,
      },
      required: ["test", "expect"],
      additionalProperties: false,
    },
  ],
};

const TESTS = {
  type: "array",
  description:
    "Tests, each written in full, the digest of a recorded test, or { test: <digest>, expect } for a recorded test with a new expectation.",
  items: TEST,
};

/** The changes of a suite `update_workflow` takes beside new tests. */
const REPLACE = {
  type: "array",
  description:
    "Tests of its suite to replace because their state or their expected result is out of date: by a whole `new` test, which takes the name, or by the same test with a new `expect`.",
  items: {
    type: "object",
    properties: {
      name: { type: "string", description: "The name of a test of its suite." },
      reason: { type: "string", description: "Why it changes: kept with the workflow." },
      new: { ...TEST, description: "The test that replaces it." },
      expect: { ...EXPECT, description: "Its new expectation; the rest of the test is kept." },
    },
    required: ["name", "reason"],
    additionalProperties: false,
  },
};

const RETIRE = {
  type: "array",
  description: "Tests of its suite to drop because the behavior they check is deprecated.",
  items: {
    type: "object",
    properties: {
      names: { type: "array", items: { type: "string" }, description: "The names of tests of its suite." },
      reason: { type: "string", description: "Why they no longer hold: kept with the workflow." },
    },
    required: ["names", "reason"],
    additionalProperties: false,
  },
};

const DEFINITIONS: Record<AgentToolName, AgentToolDefinition> = {
  run_workflow: {
    name: "run_workflow",
    description:
      "Run a workflow: TypeScript code, type-checked first, or an existing workflow by its id or its name. The code calls the actions declared in `rewind:actions` and runs durably: it can sleep for days and wait for signals. Code that no workflow of the namespace or the organization has becomes a new workflow, with an id (wf_…) and its version v1; the same code run again is a run of the same workflow, and changed code is another workflow: to make a workflow's next version, give its new code to update_workflow. Code that does not type-check is refused with its diagnostics, before anything runs. The result names the workflow by its id and its version. When the run ends within `wait`, you get its output and last log lines, or its error with the failing line and the transcript up to the failure. Otherwise you get the run's handle and what the run waits for: follow it with get_run, send it a signal with signal_run, or cancel it with cancel_run. " +
      TOP_COMMENT,
    inputSchema: {
      type: "object",
      properties: {
        code: {
          ...CODE,
          description:
            'The TypeScript source to run: one ES module whose default export is `async function main(input)`. It imports actions from "rewind:actions", workflows from "rewind:workflows", and `sleep`, `signal`, `signalUrl` from "rewind". Its input and output are JSON. Give `code` or `workflow`, not both.',
        },
        workflow: workflow(
          "An existing workflow to run instead of code: its id (wf_…) or its name, such as triage, for its latest version, or triage@v12 for one version.",
        ),
        input: { description: "The input passed to `main`, any JSON value. {} when absent." },
        wait: WAIT,
        secrets: secrets(RUN_SECRETS),
      },
      additionalProperties: false,
    },
  },
  check_workflow: {
    name: "check_workflow",
    description:
      "Type-check TypeScript code against the sandbox and the action declarations, without running it. Returns the diagnostics with their location and a code frame. It also says which workflow and version already have this code, if one does: running the code is then a run of that workflow. It says when the code has no comment at its top. It creates nothing. " +
      TOP_COMMENT,
    inputSchema: {
      type: "object",
      properties: { code: CODE },
      required: ["code"],
      additionalProperties: false,
    },
  },
  test_workflow: {
    name: "test_workflow",
    description:
      "Run tests end to end, in virtual time: no action is really called and sleeps take no time. Each test gives the world the run meets (its input, the answers of the actions it may call, the signals sent to it) and what must hold at the end (its status, its output or error, the calls it made). A call the world does not answer fails the test. Give `code` with `tests`; with `workflow`, that workflow's suite runs too, against your code: this is how you check a fix or a change of a workflow before update_workflow. A `workflow` without `code` runs its suite and your tests against its version. It creates and changes nothing. Returns whether each test passed and, for those that did not, why: stale (it names actions, answers or args your actions no longer have; not run), incompatible (the workflow's input or signal schemas refuse it; not run) or failed (an expectation does not hold), with the run's outcome and its transcript.",
    inputSchema: {
      type: "object",
      properties: {
        code: {
          ...CODE,
          description: "The TypeScript source to test. The version's code when absent, with `workflow`.",
        },
        tests: TESTS,
        workflow: workflow(
          "A workflow whose suite runs too: its id (wf_…) or its name, such as triage, for its latest version, or triage@v12 for one version. Its interface checks every test against its input and signal schemas.",
        ),
      },
      additionalProperties: false,
    },
  },
  get_run: {
    name: "get_run",
    description:
      "Read a run: its status, what it waits for or how it ended, and its transcript. With `event`, one event in full instead. With `offset`, the next page of an output (or event) too large for one answer.",
    inputSchema: {
      type: "object",
      properties: {
        run_id: RUN_ID,
        event: {
          type: "integer",
          minimum: 0,
          description: "The index of an event of the history, as the transcript shows it.",
        },
        offset: {
          type: "integer",
          minimum: 0,
          description: "Where the page starts, in bytes, as the previous answer said.",
        },
        wait: {
          type: "string",
          description: 'Wait up to this long for the run to end first, such as "30s".',
        },
      },
      required: ["run_id"],
      additionalProperties: false,
    },
  },
  list_runs: {
    name: "list_runs",
    description: "List the runs you started, most recent first.",
    inputSchema: {
      type: "object",
      properties: { status: { ...STATUS, description: "Only runs with this status." } },
      additionalProperties: false,
    },
  },
  signal_run: {
    name: "signal_run",
    description:
      "Send a signal to a run: it resolves the code's `signal(name)`, now or the next time the code waits for that name. A run that has ended refuses it.",
    inputSchema: {
      type: "object",
      properties: {
        run_id: RUN_ID,
        name: { type: "string", description: "The signal's name, as the code waits for it." },
        payload: { description: "The signal's payload, any JSON value. null when absent." },
      },
      required: ["run_id", "name"],
      additionalProperties: false,
    },
  },
  cancel_run: {
    name: "cancel_run",
    description:
      "Cancel a run: every pending await of the code rejects with CancelledError, and the code gets one chance to clean up.",
    inputSchema: {
      type: "object",
      properties: { run_id: RUN_ID },
      required: ["run_id"],
      additionalProperties: false,
    },
  },
  retry_run: {
    name: "retry_run",
    description:
      "Start a new run from a failed one with corrected code. The calls the failed run already made are not made again: their recorded results are reused while the corrected code makes the same calls, and it continues live from the first difference. The suite of the run's version runs against the corrected code first, and the retry is refused if a test does not pass: check the fix with test_workflow({ code, workflow }) before. The correction does not become a version of the run's workflow: update_workflow makes one.",
    inputSchema: {
      type: "object",
      properties: {
        run_id: { ...RUN_ID, description: "The run to correct." },
        code: { ...CODE, description: "The corrected TypeScript source." },
        secrets: secrets(
          "The secrets the new run reads, by MCP server or API, as run_workflow takes them with `code`. The failed run's when absent; either way checked again, since the code is new code.",
        ),
      },
      required: ["run_id", "code"],
      additionalProperties: false,
    },
  },
  search_actions: {
    name: "search_actions",
    description:
      "Find the actions of `rewind:actions` matching a query, with their TypeScript declarations: for action sets too large to be listed in full.",
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string", description: "Words to look for in action names and descriptions." },
      },
      required: ["query"],
      additionalProperties: false,
    },
  },
  list_secrets: {
    name: "list_secrets",
    description:
      'List the secrets your runs may use, by name, never a value: the end user\'s own (ns/<name>, read in the namespace the run acts for) and the organization\'s (org/<name>). Give them in `secrets` of run_workflow, retry_run or update_workflow, by the MCP server or API whose calls need one, such as {"github": "ns/github"}.',
    inputSchema: {
      type: "object",
      properties: {},
      additionalProperties: false,
    },
  },
  update_workflow: {
    name: "update_workflow",
    description:
      'Change a workflow, named by its id or its name. A `name` and a `description` keep its version; null removes them. New `code`, an interface (`input`, `output`, `signals`: JSON Schemas) or the `secrets` its runs use make its next version. Its code is type-checked against your actions and its interface: its default export must take the input and return the output, and `signal(name)` only takes a declared name and returns that signal\'s payload. Then its suite runs, and the version is made only if every test passes. The suite is its tests, minus those you `retire`, with those you `replace` replaced, plus your new `tests`. There is no way around a failing test. To change a behavior a test pins on purpose, retire the test when the behavior is deprecated, or replace it when its state or its expected result is out of date, always with a reason: the reasons stay with the workflow for whoever reads it next. `tests`, `replace` and `retire` alone change its suite without a version, once each new test passes against its latest version. Read the suite with get_workflow, and check a change with test_workflow({ code, workflow }) first. Then run it by name with run_workflow({ workflow }); code starts it from "rewind:workflows" once an action set pins it.',
    inputSchema: {
      type: "object",
      properties: {
        workflow: workflow("The workflow to change: its id (wf_…) or its name, such as triage."),
        name: {
          type: ["string", "null"],
          description:
            'Its new name: what code imports from "rewind:workflows" and runs start it by, a JavaScript identifier that is not a reserved word and does not start with wf_, such as triage, unique among the workflows of its namespace or organization. null removes it.',
        },
        description: {
          type: ["string", "null"],
          description: "What the workflow does, shown with it; at most 2,000 characters. null removes it.",
        },
        code: {
          ...CODE,
          description:
            'Its new TypeScript source: one ES module whose default export is `async function main(input)`, taking the input and returning the output. It imports actions from "rewind:actions" and `sleep`, `signal`, `signalUrl` from "rewind". Its latest version\'s when absent.',
        },
        input: {
          type: ["object", "boolean"],
          description:
            "The JSON Schema of the input of `main`. Each of `input`, `output` and `signals` you leave out keeps its latest version's.",
        },
        output: { type: ["object", "boolean"], description: "The JSON Schema of what `main` returns." },
        signals: {
          type: "object",
          description:
            'The signals its runs accept, by name, each with the JSON Schema of its payload, such as {"approval": {"type": "object", "properties": {"close": {"type": "boolean"}}, "required": ["close"]}}. {} for none; names starting with "rewind:" are reserved.',
          additionalProperties: { type: ["object", "boolean"] },
        },
        secrets: secrets(
          'The secrets its runs read, by MCP server or API: {"github": "ns/github"}, the secret github of the namespace each run acts for, or "org/<name>", the organization\'s. Part of the version: a run of it needs no right on them, only its namespace\'s ns/ secrets. Its latest version\'s when absent.',
        ),
        tests: {
          ...TESTS,
          description:
            "New tests for its suite: each written in full, the digest of a test record_tests recorded, or { test: <digest>, expect } for a recorded test with the expectation it should have met.",
        },
        replace: REPLACE,
        retire: RETIRE,
      },
      required: ["workflow"],
      additionalProperties: false,
    },
  },
  get_workflow: {
    name: "get_workflow",
    description:
      "Read a workflow, by its id or its name: its name and description, whether it belongs to the namespace or to the organization, the organization's workflow it was forked from, the code of its latest version, its interface (input, output and signal schemas), the secrets its runs use, its versions, its suite with every test in full, and the tests that left it, retired or replaced, with the reason of each. Read it before you repair or extend a workflow you did not write: its tests say which behaviors it must keep, and their reasons which changes were made on purpose.",
    inputSchema: {
      type: "object",
      properties: {
        workflow: workflow(
          "The workflow: its id (wf_…) or its name, such as triage; triage@v12 reads its version v12.",
        ),
      },
      required: ["workflow"],
      additionalProperties: false,
    },
  },
  record_tests: {
    name: "record_tests",
    description:
      "Record tests from past runs of a workflow, so a new version must keep doing what the old ones did on real inputs. Its completed and failed runs are grouped by the path they took (the actions they called in order, the signals they waited for, how they ended), and the newest run of each path becomes a test: its input, its calls and signals as they happened, and the same status, output or error and number of calls per action as expected. The tests are stored but join no suite: pass their digests in update_workflow's `tests`, or { test: <digest>, expect } to give a failed run the expectation it should have met.",
    inputSchema: {
      type: "object",
      properties: {
        workflow: workflow("The workflow: its id (wf_…) or its name, such as triage."),
        query: {
          type: "string",
          description:
            'Which runs, in the query language of the run lists, such as "status:completed". All the completed and failed runs of the workflow when absent.',
        },
        runs: {
          type: "array",
          items: { type: "string" },
          description: "These runs, by handle or id, rather than a query.",
        },
        limit: {
          type: "integer",
          minimum: 1,
          maximum: 100,
          description: "At most this many tests, one per path. 20 when absent.",
        },
      },
      required: ["workflow"],
      additionalProperties: false,
    },
  },
};

/** The definition of an agent tool. */
export function definitionOf(name: AgentToolName): AgentToolDefinition {
  return structuredClone(DEFINITIONS[name]);
}

export function isAgentToolName(name: string): name is AgentToolName {
  return (AGENT_TOOL_NAMES as readonly string[]).includes(name);
}
