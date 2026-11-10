# 1. Product

## What ai-ptc is

ai-ptc lets the model inside an agent harness solve a task by writing a TypeScript
script that calls tools (MCP servers and harness-defined tools), instead of calling
tools one at a time through the conversation.

Scripts run in a deterministic V8 sandbox. Every interaction with the outside world
goes through a journal, and everything else is derived from it:

| Feature | How the journal provides it |
| --- | --- |
| Durable execution | A run can `await sleep("10d")` or wait for a signal, survive crashes and deploys, and resume by re-executing the script against its journal. |
| Replay | Any run can be re-executed from its journal, locally, with identical results. |
| Testing | A journal recorded from a real run becomes a cassette of mocks; time is virtual. |
| Observability | Every tool call, timer, signal and log is an event linked to the script line that caused it. Exported as OpenTelemetry, and shown (later) in a dashboard of every run and all the code executed on the platform. |

V1 priorities:

1. **Durable workflows first.** Short "run this and give me the result" scripts are the
   same thing, started with a `wait` timeout.
2. **Horizontal scale from day one.** Stateless workers, Postgres as the source of truth.
   SQLite only for the embedded engine (CLI, local dev, tests).
3. **The journal is the only source of truth.**

## Who uses it

ai-ptc is a tool for **agent harness developers**. A harness embeds it through an HTTP
API and a TypeScript SDK. The `ptc` CLI covers the same surface for local development,
either with an embedded engine (SQLite, local files) or as a client of a remote server.

```
┌──────────── agent harness ────────────┐       ┌──────────────── ai-ptc ────────────────┐
│                                        │       │                                         │
│  model ── run_script(code) ──► SDK ────┼─HTTP─►│  API server ──► Postgres (journal, …)   │
│    ▲                            │      │       │                    ▲          S3 (payloads)
│    └── result | "suspended" ◄───┘      │       │  workers (V8) ─────┤──► MCP servers      │
│                                        │       │  scheduler (timers)┘                    │
│  host-tool worker ◄──── long-poll ─────┼───────│  tool dispatcher                        │
│  (harness functions, LLM calls, …)     │       │                                         │
└────────────────────────────────────────┘       └─────────────────────────────────────────┘
```

1. The harness **registers a toolset**: MCP servers plus its own *host tools* (JSON
   Schema + handler). ai-ptc connects to the MCP servers, pins their schemas, and returns
   the generated TypeScript declarations.
2. The harness puts those declarations in the system prompt and exposes the **agent
   tools** to its model (below).
3. When the model calls `run_script`, the harness calls
   `runs.start({ script, toolset, input, wait: "30s" })`. If the run finishes within
   `wait`, the result is returned inline. Otherwise the harness gets a run id and the
   current state ("suspended, waiting for signal `approval`"). The harness can attach
   `labels` (agent, conversation, end user…) and its `traceparent`, so the run is
   searchable in the dashboard and appears under the agent's span in its traces.
4. Host tools are executed by the harness through an SDK worker (long-poll), like
   Temporal activity workers. This is how a script calls harness functions, another
   LLM, a sub-agent, or a human.
5. The harness receives lifecycle webhooks (`completed`, `failed`, `suspended`) to resume
   the agent conversation when a long run ends.

```ts
import { Ptc } from "@ai-ptc/sdk";

const ptc = new Ptc({ url: process.env.PTC_URL });

const toolset = await ptc.toolsets.create({
  mcp: [{ name: "github", url: "https://mcp.example.com/github" }],
  host: [{
    name: "ask_user",
    description: "Ask the end user a question and return their answer.",
    inputSchema: { type: "object", properties: { question: { type: "string" } }, required: ["question"] },
    policy: { retry: "never" },
  }],
});

// toolset.declarations → system prompt

ptc.worker({ toolset: toolset.id, tools: { ask_user: async ({ question }, ctx) => { /* … */ } } });

const run = await ptc.runs.start({
  toolset: toolset.id,
  script: code,
  input: {},
  wait: "30s",
  labels: { agent: "triage", conversation: conversation.id },
  traceparent, // the LLM call that wrote the script
});
```

## Agent tools

The model needs more than `run_script`: it must be able to check, test, follow and steer
its runs. The SDK ships ready-made tool definitions and handlers, so the harness doesn't
write them. The harness picks the ones it wants and plugs them into its model API
(adapters for the Anthropic and OpenAI tool formats).

```ts
const agentTools = ptc.agentTools({ toolset: toolset.id, labels: { conversation: convId } });
// agentTools.definitions → model API; agentTools.handle(name, input) → tool result
```

| Tool | What it does |
| --- | --- |
| `run_script({ code, input?, wait? })` | Type-checks, starts the run, returns the output or the run's state. |
| `check_script({ code })` | Type-check only. |
| `test_script({ code, scenario })` | Runs against mocks and cassettes with virtual time. |
| `get_run({ run_id, entry? })` | Status and LLM trace, or one journal entry in full. |
| `list_runs({ status? })` | Runs started with the same labels (for example the same conversation). |
| `signal_run({ run_id, name, payload })` | Sends a signal. |
| `cancel_run({ run_id })` | Cancels a run. |
| `search_tools({ query })` | Returns the declarations matching a query, for toolsets too large for the prompt. |

`labels` are set by the harness on every run the agent starts. They scope `list_runs` and
the lifecycle webhooks.

## What the model writes

A script is a single ES module. Its default export is the entry point; its input and
output are JSON.

```ts
import { github, slack } from "ptc:tools";
import { sleep, signal } from "ptc";

export default async function main(input: { repo: string }) {
  const issues = await github.list_issues({ repo: input.repo, state: "open" });
  const stale = issues.filter((i) => Date.now() - Date.parse(i.updated_at) > 30 * 86_400_000);

  await slack.post_message({ channel: "#triage", text: `${stale.length} stale issues in ${input.repo}` });

  const decision = await signal<{ close: boolean }>("approval", { timeout: "3d" });
  if (!decision?.close) return { closed: 0 };

  await Promise.all(stale.map((i) => github.close_issue({ repo: input.repo, number: i.number })));

  await sleep("10d");
  const reopened = await github.list_issues({ repo: input.repo, state: "open", labels: "reopened" });
  return { closed: stale.length, reopened: reopened.length };
}
```

### Modules available to scripts

- `ptc:tools`: generated per toolset. One namespace per MCP server or host-tool group.
- `ptc`: the runtime standard library.

```ts
declare module "ptc" {
  type Duration = number | `${number}${"ms" | "s" | "m" | "h" | "d" | "w"}` | string; // "1h30m"
  export function sleep(duration: Duration): Promise<void>;
  export function signal<T = unknown>(name: string): Promise<T>;
  export function signal<T = unknown>(name: string, opts: { timeout: Duration }): Promise<T | undefined>;
  export function webhook(name: string): Promise<{ url: string }>;
  export const run: { readonly id: string; readonly startedAt: Date };
  export class ToolError extends Error { tool: string; outcomeUnknown: boolean; data?: unknown }
  export class CancelledError extends Error {}
}
```

Nothing else is importable in V1: no npm, no relative imports, no dynamic `import()`. A
curated set of deterministic utility libraries can be bundled later.

### Signals

A signal is a named message sent to a run from the outside. Events can come from any
source: the harness, a human through a UI, another agent (with `signal_run`), another
run (through a tool), or an external system through a webhook.

```
POST /runs/{run_id}/signals/approval   { "close": true }
```

```ts
const decision = await signal<{ close: boolean }>("approval");
```

Each run has one mailbox per signal name, first in first out:

- If the script is already waiting on `signal("approval")`, the signal resolves it.
- If it isn't waiting yet, the signal is kept in the `approval` mailbox. The next
  `signal("approval")` returns it immediately. Nothing is lost if the human approves
  before the script reaches that line.
- Two signals with the same name are returned by two successive `signal()` calls, in
  arrival order.
- With `{ timeout }`, `signal()` returns `undefined` if nothing arrived in time.

External systems don't know run ids. For them, the script asks for a webhook:

```ts
const hook = await webhook("pr_merged");          // built-in tool call, journaled
await github.create_webhook({ repo, url: hook.url });
const event = await signal("pr_merged");          // anything POSTed to hook.url
```

`webhook(name)` returns a unique URL bound to this run and this signal name. Every
request posted to it becomes a `pr_merged` signal on the run. (Designed, not implemented
yet.)

### What exists in the sandbox

| Global | Behavior |
| --- | --- |
| `Date.now()`, `new Date()`, `performance.now()`, `Temporal.Now`, `Intl.DateTimeFormat` | Logical time (see [execution](03-execution.md)). |
| `Math.random()`, `crypto.getRandomValues()`, `crypto.randomUUID()` | Seeded PRNG (sfc32); the seed is in the journal. |
| `setTimeout`, `setInterval` | Durable timers, same as `sleep`. |
| `console.*` | Journaled as log annotations. |
| `fetch` | Absent, unless the toolset enables the built-in `http` tool. It is then a journaled tool call like any other. |
| `WeakRef`, `FinalizationRegistry`, `SharedArrayBuffer`, `Atomics` | Removed (they expose GC timing or threads). |
| File system, env, process, network, `Deno`, Node APIs | Absent. No file system, ever. |

The isolate runs with `TZ=UTC` and the `en-US` ICU locale, so `Intl` output does not
depend on the host. The sandbox refuses to start if `TZ` is not UTC.
