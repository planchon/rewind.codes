/** The names of the agent tools. */
export type AgentToolName =
  | "run_script"
  | "check_script"
  | "test_script"
  | "get_run"
  | "list_runs"
  | "signal_run"
  | "cancel_run"
  | "retry_run"
  | "search_tools";

export const AGENT_TOOL_NAMES: readonly AgentToolName[] = [
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

const RUN_ID = { type: "string", description: "The run id, as a previous tool result gave it." };
const CODE = {
  type: "string",
  description:
    'The TypeScript source: one ES module whose default export is `async function main(input)`. It imports tools from "rewind:tools" and `sleep`, `signal`, `webhook` from "rewind". Its input and output are JSON.',
};
const WAIT = {
  type: "string",
  description:
    'How long to wait for the run to end before answering, such as "30s" or "2m". If the run is still going then, you get its id and what it waits for.',
};
const STATUS = {
  type: "string",
  enum: ["pending", "active", "completed", "failed", "cancelled", "aborted"],
};

const DEFINITIONS: Record<AgentToolName, AgentToolDefinition> = {
  run_script: {
    name: "run_script",
    description:
      "Type-check a TypeScript script and run it. The script calls the tools declared in `rewind:tools` and runs durably: it can sleep for days and wait for signals. A script that does not type-check is refused with its diagnostics, before anything runs. When the run ends within `wait`, you get its output and last log lines, or its error with the failing line and the trace up to the failure. Otherwise you get the run id and what the run waits for: follow it with get_run, send it a signal with signal_run, or cancel it with cancel_run.",
    inputSchema: {
      type: "object",
      properties: {
        code: CODE,
        input: { description: "The input passed to `main`, any JSON value. {} when absent." },
        wait: WAIT,
      },
      required: ["code"],
      additionalProperties: false,
    },
  },
  check_script: {
    name: "check_script",
    description:
      "Type-check a TypeScript script against the sandbox and the tool declarations, without running it. Returns the diagnostics with their location and a code frame.",
    inputSchema: {
      type: "object",
      properties: { code: CODE },
      required: ["code"],
      additionalProperties: false,
    },
  },
  test_script: {
    name: "test_script",
    description:
      "Run a script against mocks, in virtual time: no tool is really called and sleeps take no time. Tool results come from the calls recorded by an earlier run (`scenario.from_run`), then TypeScript mock handlers (`scenario.mocks`), then static responses (`scenario.tools`). Returns whether the expectations hold, the outcome, the calls made and the trace.",
    inputSchema: {
      type: "object",
      properties: {
        code: CODE,
        scenario: {
          type: "object",
          description: "What the script runs against, and what must hold at the end.",
          properties: {
            input: { description: "The input passed to `main`. {} when absent." },
            tools: {
              type: "object",
              description:
                'Static responses by tool name, such as {"github.list_issues": []}. {"$error": "message"} makes the call fail with a ToolError.',
              additionalProperties: true,
            },
            mocks: {
              type: "string",
              description:
                "TypeScript mock handlers, as a module run in a separate isolate. Tried after the calls recorded by `from_run`, before the static responses.",
            },
            from_run: {
              type: "string",
              description:
                "A run id: the tool results recorded by that run are replayed as a cassette, before mocks and static responses.",
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
                      '"blocked" (the default): once the run can go no further without it. Or a duration of virtual time after the start, such as "2h".',
                  },
                },
                required: ["name"],
                additionalProperties: false,
              },
            },
            expect: {
              type: "object",
              description: "What must hold at the end. Anything left out is not checked.",
              properties: {
                status: STATUS,
                output: { description: "Values the output must contain; objects are compared key by key." },
                error: {
                  type: "object",
                  properties: { name: { type: "string" }, message: { type: "string" } },
                  required: ["name"],
                  additionalProperties: false,
                },
                calls: {
                  type: "object",
                  description: "How many times each tool must be called, by tool name.",
                  additionalProperties: { type: "integer", minimum: 0 },
                },
              },
              additionalProperties: false,
            },
          },
          additionalProperties: false,
        },
      },
      required: ["code", "scenario"],
      additionalProperties: false,
    },
  },
  get_run: {
    name: "get_run",
    description:
      "Read a run: its status, what it waits for or how it ended, and its trace. With `entry`, one journal entry in full instead. With `offset`, the next page of an output (or entry) too large for one answer.",
    inputSchema: {
      type: "object",
      properties: {
        run_id: RUN_ID,
        entry: { type: "integer", minimum: 0, description: "The index of a journal entry, as the trace shows it." },
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
      "Send a signal to a run: it resolves the script's `signal(name)`, now or the next time the script waits for that name. A run that has ended refuses it.",
    inputSchema: {
      type: "object",
      properties: {
        run_id: RUN_ID,
        name: { type: "string", description: "The signal's name, as the script waits for it." },
        payload: { description: "The signal's payload, any JSON value. null when absent." },
      },
      required: ["run_id", "name"],
      additionalProperties: false,
    },
  },
  cancel_run: {
    name: "cancel_run",
    description:
      "Cancel a run: every pending await of the script rejects with CancelledError, and the script gets one chance to clean up.",
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
      "Start a new run from a failed one with a corrected script. The calls the failed run already made are not made again: their recorded results are reused while the corrected script makes the same calls, and it continues live from the first difference.",
    inputSchema: {
      type: "object",
      properties: {
        run_id: { ...RUN_ID, description: "The run to correct." },
        code: { ...CODE, description: "The corrected TypeScript source." },
      },
      required: ["run_id", "code"],
      additionalProperties: false,
    },
  },
  search_tools: {
    name: "search_tools",
    description:
      "Find the tools of `rewind:tools` matching a query, with their TypeScript declarations: for toolsets too large to be listed in full.",
    inputSchema: {
      type: "object",
      properties: { query: { type: "string", description: "Words to look for in tool names and descriptions." } },
      required: ["query"],
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
