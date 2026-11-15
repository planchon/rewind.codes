# 1. Product

## What Rewind is

Rewind lets the model inside an agent harness solve a task by writing a TypeScript
script that calls tools (MCP servers and harness-defined tools), instead of calling
tools one at a time through the conversation.

Scripts run in a deterministic V8 sandbox. Every interaction with the outside world
goes through a journal, and everything else is derived from it:

| Feature | How the journal provides it |
| --- | --- |
| Durable execution | A run can `await sleep("10d")` or wait for a signal, survive crashes and deploys, and resume by re-executing the script against its journal. |
| Replay | Any run can be re-executed from its journal, locally, with identical results. |
| Testing | A journal recorded from a real run becomes a cassette of mocks; time is virtual. |
| Observability | Every tool call, timer, signal and log is an event linked to the script line that caused it. Exported as OpenTelemetry, and shown in a dashboard of every run and all the code executed on the platform. |

Priorities:

1. **Durable workflows first.** Short "run this and give me the result" scripts are the
   same thing, started with a `wait` timeout.
2. **Horizontal scale from day one.** Stateless workers, Postgres as the source of truth.
   Turso, an embedded SQLite-compatible database, only for the embedded engine (CLI, local
   dev, tests).
3. **The journal is the only source of truth.**

## Who uses it

Rewind is a tool for **agent harness developers**. A harness embeds it through an HTTP
API ([`/v1`](07-code-architecture.md#http-api-v1)) and a TypeScript SDK. The `rewind` CLI is a
client of the same API, for local development: of a server it starts itself on a local
directory (Turso, local files), or of a remote one
([embedded server](07-code-architecture.md#embedded-server-and-cli)).

```
┌──────────── agent harness ────────────┐       ┌──────────────── Rewind ────────────────┐
│                                        │       │                                         │
│  model ── run_script(code) ──► SDK ────┼─HTTP─►│  API server ──► Postgres (journal, …)   │
│    ▲                            │      │       │                    ▲          S3 (payloads)
│    └── result | "suspended" ◄───┘      │       │  workers (V8) ─────┤──► MCP servers      │
│                                        │       │  scheduler (timers)┘                    │
│  host-tool worker ◄──── long-poll ─────┼───────│  tool dispatcher                        │
│  (harness functions, LLM calls, …)     │       │                                         │
└────────────────────────────────────────┘       └─────────────────────────────────────────┘
```

1. The harness **registers a toolset**: MCP servers and, optionally, its own *host tools*
   (JSON Schema + handler) and the [workflows](#workflows) its scripts may start. Rewind
   connects to the MCP servers, pins their schemas and the workflows' versions, and returns the
   generated TypeScript declarations ([routes](04-tools.md#routes)).
2. The harness puts those declarations in the system prompt and exposes the **agent
   tools** to its model (below).
3. When the model calls `run_script`, the harness calls
   `runs.start({ script, toolset, input, wait: "30s" })` in the namespace of the end user the
   agent acts for ([below](#end-users-and-namespaces)). If the run finishes within
   `wait`, the result is returned inline. Otherwise the harness gets a run id and the
   current state ("suspended, waiting for signal `approval`"). The harness can attach
   `labels` (agent, conversation…) and its `traceparent`, so the run is
   searchable in the dashboard and appears under the agent's span in its traces.
   It also passes an `idempotencyKey`, so a retried request returns the same run instead
   of starting a second one, and may set a `ttl` (30 days by default, see
   [execution](03-execution.md#run-ttl)).
4. Host tools are executed by the harness through an SDK worker (long-poll), like
   Temporal activity workers. This is how a script calls harness functions, another
   LLM, a sub-agent, or a human. Each task says the namespace of its run, and the handler
   acts for that end user.
5. The harness receives lifecycle webhooks (`run.completed`, `run.failed`,
   `run.suspended`), each naming its namespace, to resume the agent conversation when a long
   run ends, and `quota.alert` when an end user nears or passes a limit.

```ts
import { Rewind } from "@rewind.codes/sdk";

const rewind = new Rewind({ url: process.env.REWIND_URL });

const toolset = await rewind.toolsets.create({
  mcp: [{ name: "github", url: "https://mcp.example.com/github" }],
  host: [{
    name: "ask_user",
    description: "Ask the end user a question and return their answer.",
    inputSchema: { type: "object", properties: { question: { type: "string" } }, required: ["question"] },
    policy: { onInterruption: "at_most_once" },
  }],
});

// toolset.declarations → system prompt

rewind.worker({ toolset: toolset.id, tools: { ask_user: async ({ question }, ctx) => { /* … */ } } });

const run = await rewind.namespace(user.id).runs.start({ // the end user's namespace
  toolset: toolset.id,
  script: code,
  input: {},
  wait: "30s",
  idempotencyKey: toolCall.id, // a retried request returns the same run
  labels: { agent: "triage", conversation: conversation.id },
  traceparent, // the LLM call that wrote the script
});
```

### End users and namespaces

A harness serves its own users, the **end users** of its product. Each one has a
**namespace**: their runs, the scripts those runs ran, their credentials and their consumption
live there, and nothing of one end user is visible to another
([namespaces](06-storage-and-scale.md#namespaces)).

- The harness's backend holds its API key, which reaches every namespace, and names the
  namespace of each request (the header `Rewind-Namespace`, set by `rewind.namespace(name)`).
- An agent acting for one end user gets a **token** of that end user's namespace, minted by the
  harness's backend, or signed by it with a key it registered. With it the agent, and every
  script its model writes, can only reach that namespace: an id of another end user's run
  answers 404.
- **Quotas** per namespace (CPU per month, active runs, runs per month, distinct scripts, the
  longest TTL, the longest script) protect the platform and the harness from an end user who
  abuses its product ([quotas](06-storage-and-scale.md#quotas)).

```ts
// In the harness's backend, with its key:
const { token } = await rewind.namespaces.tokens.create(user.id, { ttl: "1h" });

// In the agent acting for that user:
const asUser = new Rewind({ url: process.env.REWIND_URL, token });
const agentTools = asUser.agentTools({ toolset: toolset.id });
```

## Agent tools

The model needs more than `run_script`: it must be able to check, test, follow and steer
its runs. The SDK ships ready-made tool definitions and handlers, so the harness doesn't
write them. The harness picks the ones it wants and plugs them into its model API
(adapters for the Anthropic and OpenAI tool formats).

```ts
const agentTools = rewind.agentTools({ toolset: toolset.id, labels: { conversation: convId } });
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
| `retry_run({ run_id, code })` | Starts a new run from a failed one with a corrected script. The calls that already happened are not made again: their recorded results are reused ([forks](05-testing-and-observability.md#replay-debugging-and-forks)). |
| `search_tools({ query })` | Returns the declarations matching a query, for toolsets too large for the prompt. |
| `save_workflow({ name, description, code, input, output, signals })` | Checks and saves a [workflow](#workflows): its script, with JSON Schemas for its input, its output and the signals it accepts. The agent's later scripts can start it and signal its runs. |

`labels` are set by the harness on every run the agent starts. They scope `list_runs` and
the lifecycle webhooks. A token or an API key limited to these labels makes the scope hold for
every tool that takes a `run_id`, within the namespace the agent acts in ([access](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)).

### What `run_script` returns

The result goes into the model's context, so it is bounded by design:

- **Completed**: the output, truncated above 256 KB, with the run id so the model can read
  the rest through `get_run`; plus the last log lines.
- **Suspended**: the run id and what the run is waiting for.
- **Failed**: the error with its TypeScript location and a code frame, the trace up to
  the failure, and the tool call that failed, if any.

## What the model writes

A script is a single ES module. Its default export is the entry point; its input and
output are JSON.

```ts
import { github, slack } from "rewind:tools";
import { sleep, signal } from "rewind";

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

- `rewind:tools`: generated per toolset. One namespace per MCP server or host-tool group.
- `rewind:workflows`: generated per toolset. One export per [workflow](#workflows) the toolset
  pins.
- `rewind`: the runtime standard library.

```ts
declare module "rewind" {
  type Duration = number | `${number}${"ms" | "s" | "m" | "h" | "d" | "w"}` | string; // "1h30m"
  export function sleep(duration: Duration): Promise<void>;
  // In a run of a workflow, the name is one of its declared signals and the payload has its type.
  export function signal<T = unknown>(name: string): Promise<T>;
  export function signal<T = unknown>(name: string, opts: { timeout: Duration }): Promise<T | undefined>;
  export function webhook(name: string): Promise<{ url: string }>;
  export const run: { readonly id: string; readonly startedAt: Date };
  export class ToolError extends Error { tool: string; outcomeUnknown: boolean; data?: unknown }
  export class CancelledError extends Error {}
  export class WorkflowError extends Error {
    workflow: string; runId: string; status: "failed" | "cancelled" | "aborted" | "invalid_output";
  }
}
```

Nothing else is importable in 0.1: no npm, no relative imports, no dynamic `import()`. A
curated set of deterministic utility libraries can be bundled later.

### Signals

A signal is a named message sent to a run from the outside. Events can come from any
source: the harness, a human through a UI, another agent (with `signal_run`), another
run (through [`rewind:workflows`](#workflows)), or an external system through a webhook.

```
POST /v1/runs/{run_id}/signals/approval   { "close": true }
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

A signal can carry an idempotency key (the `Idempotency-Key` header). The same key sent
twice to a run delivers one signal. Without it, a retried request leaves a second
`approval` in the mailbox, and that one approves the script's next `signal("approval")`.

External systems don't know run ids. For them, the script asks for a webhook:

```ts
const hook = await webhook("pr_merged");          // built-in tool call, journaled
await github.create_webhook({ repo, url: hook.url });
const event = await signal("pr_merged");          // anything POSTed to hook.url
```

`webhook(name)` returns a unique URL bound to this run and this signal name. Every
request posted to it becomes a `pr_merged` signal on the run. When the sender provides a
delivery id, it is used as the signal's idempotency key.

Signal names starting with `rewind:` are reserved for Rewind. A run of a [workflow](#workflows)
accepts only the signals its workflow declares, with a payload that matches their schema:
anything else is refused (422), whoever sends it. Other runs accept any name, and
`signal<T>(name)` only says what the script expects.

### Workflows

A **workflow** is a script with a name and a typed interface: JSON Schemas for its input, its
output and each signal it accepts. A script can start a run of a workflow, signal it and wait
for its result, and it can signal a run of a workflow that already exists, by its id. tsgo
checks every one of these calls before the script runs.

The harness registers workflows (`POST /v1/workflows`, `rewind.workflows.create`), and so does
the model, with the agent tool `save_workflow`. Registering one checks its script against its
toolset and against its own schemas: its default export must accept the input and return the
output, and `signal(name)` only takes a declared name and returns that signal's type.

```ts
await rewind.workflows.create({
  name: "triage",
  description: "Triage the stale issues of a repository, after approval.",
  toolset: github.id,
  script: triageSource, // export default async function main(input: { repo: string }) { … }
  input: { type: "object", properties: { repo: { type: "string" } }, required: ["repo"] },
  output: { type: "object", properties: { closed: { type: "integer" } }, required: ["closed"] },
  signals: { approval: { type: "object", properties: { close: { type: "boolean" } }, required: ["close"] } },
});
```

Each registration is a new immutable **version**, identified by its digest
(`triage@9f2c4e1a07b3`). A toolset lists the workflows its scripts may start, and pins their
latest versions when it is registered, as it pins the schemas of MCP tools
([toolsets](04-tools.md#toolsets-and-code-generation)). Its scripts import them from
`rewind:workflows`:

```ts
import { triage } from "rewind:workflows";

export default async function main(input: { repos: string[] }) {
  const runs = await Promise.all(input.repos.map((repo) => triage.start({ repo })));
  await Promise.all(runs.map((run) => run.signal("approval", { close: true })));
  const results = await Promise.all(runs.map((run) => run.result()));
  return { closed: results.reduce((sum, result) => sum + result.closed, 0) };
}
```

| Call | Does |
| --- | --- |
| `triage.start(input, opts?)` | Starts a run of the pinned version, and returns its handle once the run exists. `opts`: `detached`, `labels` (added to the parent's), `ttl`. |
| `run.signal(name, payload)` | Sends one of the workflow's signals to the run. Resolves once the signal is in the run's mailbox. |
| `run.result()`, `run.result({ timeout })` | The run's output. Rejects with `WorkflowError` if the run failed, was cancelled or aborted. With a timeout, `undefined` if the run has not ended in time. |
| `triage.get(runId)` | A handle on an existing run of `triage`, which can be signalled. |

A run started by a script is **attached** to it unless it is started with `detached: true`. A
parent does not end before its attached children, as it does not end with a tool call in
flight, so a forgotten `result()` is harmless. A parent that fails, is cancelled or is aborted
cancels its attached children still running. A detached child lives on its own: use it to
start a long workflow and move on.

The harness can also start a run of a workflow: `POST /v1/runs { workflow: "triage", input }`.
How all of this lives in the journal: [execution](03-execution.md#workflows-across-runs).

### What exists in the sandbox

| Global | Behavior |
| --- | --- |
| `Date.now()`, `new Date()`, `performance.now()`, `Temporal.Now`, `Intl.DateTimeFormat` | Logical time (see [execution](03-execution.md)). |
| `Math.random()` | Seeded PRNG (sfc32). |
| `crypto.getRandomValues()`, `crypto.randomUUID()` | Seeded CSPRNG (ChaCha20). Scripts use them to make tokens and passwords, so their output must not be predictable. |
| `setTimeout`, `setInterval` | Durable timers, same as `sleep`. |
| `console.*` | Journaled as log annotations. |
| `URL`, `URLSearchParams`, `TextEncoder`, `TextDecoder`, `atob`, `btoa`, `structuredClone`, `queueMicrotask` | Present: pure and deterministic, and models use them by habit. They come from existing deno crates ([decided](08-decisions.md#taken-after-the-layering-november-2026)). |
| `ArrayBuffer`, typed arrays | Present. Their memory is bounded per instance, 128 MiB by default: past it, an allocation throws `RangeError: Array buffer allocation failed` ([limits](03-execution.md#resource-limits)). A resizable buffer counts its maximum length. |
| `fetch` | Absent, unless the toolset enables the built-in `http` tool with an allow-list of hosts. It is then a journaled tool call like any other, sent by the dispatcher ([egress](04-tools.md#the-http-tool-and-egress)). |
| `WeakRef`, `FinalizationRegistry`, `SharedArrayBuffer`, `Atomics`, `WebAssembly` | Removed (they expose GC timing or threads, or run code the sandbox cannot see the source of). |
| File system, env, process, network, `Deno`, Node APIs | Absent. No file system, ever. |

Both random generators are keyed by the run's 256-bit seed, recorded in `run_started`.

The isolate runs with `TZ=UTC` and the `en-US` ICU locale, so `Intl` output does not
depend on the host. The sandbox refuses to start if `TZ` is not UTC.

**What type-checks is what exists.** The checker's `lib` declares exactly the globals of
this table and the ECMAScript built-ins, and nothing else (no DOM lib). A name that models
reach for by habit but that is absent (`fetch`, `require`, `process`, `Buffer`) gets a
hint in `rewind check` diagnostics and in the LLM trace, saying what to use instead. Adding
or changing a global changes what scripts can observe, so it bumps the
[determinism epoch](03-execution.md#determinism-epoch).
