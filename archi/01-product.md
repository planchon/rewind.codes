# 1. Product

## What Rewind is

Rewind lets the model inside an agent harness solve a task by writing a **workflow**: TypeScript
code that calls actions (the tools of MCP servers, the operations of APIs and harness-defined
actions), instead of calling tools one at a time through the conversation.

A workflow's code runs in a deterministic V8 sandbox. Every interaction with the outside world
goes through the run's history, and everything else is derived from it:

| Feature | How the history provides it |
| --- | --- |
| Durable execution | A run can `await sleep("10d")` or wait for a signal, survive crashes and deploys, and resume by re-executing its code against its history. |
| Replay | Any run can be re-executed from its history, locally, with identical results. |
| Testing | A history recorded from a real run becomes a test, whose cassette answers the run's calls; time is virtual. |
| Observability | Every call, timer, signal and log is an event linked to the line of code that caused it. Exported as OpenTelemetry, and shown in a dashboard of every run and every workflow executed on the platform. |

Priorities:

1. **Durable workflows first.** Short "run this and give me the result" runs are the
   same thing, started with a `wait` timeout.
2. **Horizontal scale from day one.** Compute and storage are separate. Runs live in
   partitions on object storage (AWS S3 in production), each written by one node, and adding nodes adds compute.
   Postgres holds only the control plane: organizations, keys, action sets, workflows, secrets.
   Turso, an embedded SQLite-compatible database, holds it for the embedded engine (CLI, local
   dev, tests), whose partition lives in local files
   ([partitions](06-storage-and-scale.md#partitions-owners-and-nodes)).
3. **The history is the only source of truth.**

## Who uses it

Rewind is a tool for **agent harness developers**. A harness embeds it through an HTTP
API ([`/v1`](07-code-architecture.md#http-api-v1)) and a TypeScript SDK. The `rewind` CLI is a
client of the same API, for local development: of a server it starts itself on a local
directory (Turso, local files), or of a remote one
([embedded server](07-code-architecture.md#embedded-server-and-cli)).

```
┌──────────── agent harness ─────────────┐       ┌────────────────── Rewind ──────────────────┐
│                                        │       │  Postgres (control plane)                  │
│  model ── run_workflow(code) ─► SDK ───┼─HTTP─►│  edge ──► partition owners ──► S3: runs,   │
│    ▲                            │      │       │             │    timer wheel     archive,  │
│    └── result | "waiting" ◄─────┘      │       │             ▼                    blobs     │
│                                        │       │  executors (V8) ──► MCP servers, APIs      │
│  worker ◄────────── call stream ───────┼───────│  tenant actors (harness calls)             │
│  (harness functions, LLM calls, …)     │       │                                            │
└────────────────────────────────────────┘       └────────────────────────────────────────────┘
```

1. The harness **registers an action set**: MCP servers, APIs described by their OpenAPI
   documents and, optionally, its own *harness actions* (JSON Schema + handler) and the
   organization's [workflows](#workflows) its code may start. Rewind connects to the MCP servers
   and lists their tools, reads the APIs' documents, pins their schemas and the workflows'
   versions, and returns the generated TypeScript declarations ([routes](04-tools.md#routes)).
   Each server says how its calls authenticate: with a secret, or with Rewind's signature, which
   the harness's own APIs check against the organization's public key
   ([authenticating calls](04-tools.md#authenticating-calls)).
2. The harness puts those declarations in the system prompt and exposes the **agent
   tools** to its model (below).
3. When the model calls `run_workflow`, the harness calls
   `runs.start({ code, actionSet, input, wait: "30s" })` in the namespace of the end user the
   agent acts for ([below](#end-users-and-namespaces)). Code that namespace has not run before
   becomes one of its workflows ([workflows](#workflows)). If the run ends within `wait`, the
   result is returned inline. Otherwise the harness gets a run id and the run's state
   ("waiting for signal `approval`"). The harness can set `labels` (agent, conversation…) and
   its `traceparent`, so the run is searchable in the dashboard and appears under the agent's
   span in its traces. It also passes an `idempotencyKey`, so a retried request returns the same
   run instead of starting a second one, and may set a `ttl` (30 days by default, see
   [execution](03-execution.md#run-ttl)). The run reads only the secrets the request names, by
   name, for the servers that need one: the secrets it uses ([secrets](04-tools.md#secrets)).
4. Harness actions are executed by the harness through an SDK worker, like Temporal activity
   workers. The worker holds one open call stream, and Rewind pushes each call to it as soon as
   it is ready. This is how a workflow calls harness functions, another LLM, a sub-agent, or a
   human. Each call says the namespace of its run, and the handler acts for that end user.
5. The harness receives webhooks (`run.completed`, `run.failed`, `run.waiting`), each naming
   its namespace, to resume the agent conversation when a long run ends or waits, and
   `quota.alert` when an end user nears or passes a limit.

```ts
import { Rewind } from "@rewind.codes/sdk";

const rewind = new Rewind({ url: process.env.REWIND_URL });

const actionSet = await rewind.actionSets.create({
  mcp: [{
    name: "github",
    url: "https://mcp.example.com/github",
    auth: { type: "secret", header: "Authorization", format: "Bearer {secret}" },
  }],
  apis: [{ name: "crm", openapi: "https://api.example.com/openapi.json", auth: { type: "rewind" } }],
  harness: [{
    name: "ask_user",
    description: "Ask the end user a question and return their answer.",
    inputSchema: { type: "object", properties: { question: { type: "string" } }, required: ["question"] },
    policy: { idempotent: false },
  }],
});

// actionSet.declarations → system prompt

rewind.worker({ actionSet: actionSet.id, actions: { ask_user: async ({ question }, ctx) => { /* … */ } } });

const run = await rewind.namespace(user.id).runs.start({ // the end user's namespace
  actionSet: actionSet.id,
  code,
  input: {},
  wait: "30s",
  secrets: { github: "ns/github" }, // the end user's own GitHub token, put on their namespace
  idempotencyKey: toolCall.id, // a retried request returns the same run
  labels: { agent: "triage", conversation: conversation.id },
  traceparent, // the LLM call that wrote the code
});
```

### End users and namespaces

A harness serves its own users, the **end users** of its product. Each one has a
**namespace**: their runs, the workflows those runs created, their secrets and their consumption
live there, and nothing of one end user is visible to another
([namespaces](06-storage-and-scale.md#namespaces)).

- The harness's backend holds its API key, which reaches every namespace, and names the
  namespace of each request (the header `Rewind-Namespace`, set by `rewind.namespace(name)`).
- An agent acting for one end user gets a **token** of that end user's namespace, minted by the
  harness's backend, or signed by it with a key it registered. With it the agent, and every
  workflow its model writes, can only reach that namespace and the organization's workflows: an
  id of another end user's run or workflow answers 404.
- **Quotas** per namespace (CPU per month, active runs, runs per month, workflows, the longest
  TTL, the longest code) protect the platform and the harness from an end user who abuses its
  product ([quotas](06-storage-and-scale.md#quotas)).

```ts
// In the harness's backend, with its key:
const { token } = await rewind.namespaces.tokens.create(user.id, { ttl: "1h" });

// In the agent acting for that end user:
const asEndUser = new Rewind({ url: process.env.REWIND_URL, token });
const agentTools = asEndUser.agentTools({ actionSet: actionSet.id });
```

### Claude as the harness

A person can also use Rewind straight from Claude: claude.ai, Claude Desktop, the mobile apps or
Claude Code. Rewind is an MCP server at `https://api.rewind.codes/mcp`, which the person adds as
a custom connector; Claude is then the harness. It writes the workflows, checks and tests them,
runs them, follows their runs and answers their signals, through the same agent tools as a
harness's model ([below](#agent-tools)), served as MCP tools.

1. The dashboard's **Connect Claude** page gives the URL, the steps for each client (claude.ai,
   Team and Enterprise, Claude Code, other MCP clients) and the good practices, and sees the
   connection arrive.
2. Claude opens Rewind's **consent page**. The person, signed in, ticks the organizations Claude
   may open and gives each an access, "Read" or "Run"; Claude never gets more than the person
   holds, and never administers anything
   ([connections](06-storage-and-scale.md#connections)). Runs started from Claude carry the label
   `via: claude`, unless the person unticks it.
3. In the conversation, the person names the organization, the namespace and the action set, or
   lets Claude find them (`list_organizations`, `list_namespaces`, `list_action_sets`), and the
   secrets a run uses (`list_secrets`). Nothing is pinned at connection.

The person revokes the connection, or changes what it opens, from the dashboard, without
reconnecting Claude.

## Agent tools

The model needs more than `run_workflow`: it must be able to check, test, follow and steer
its runs. The SDK ships ready-made tool definitions and handlers, so the harness doesn't
write them. The harness picks the ones it wants and plugs them into its model API
(adapters for the Anthropic and OpenAI tool formats).

```ts
const agentTools = rewind.agentTools({ actionSet: actionSet.id, labels: { conversation: convId } });
// agentTools.definitions → model API; agentTools.handle(name, input) → tool result
```

| Tool | What it does |
| --- | --- |
| `run_workflow({ code?, workflow?, input?, wait?, secrets? })` | Runs a workflow, with the secrets its run uses: `code`, type-checked first, which becomes a workflow of the namespace the first time it runs there; or `workflow`, an existing one by its id or name (`triage`, `triage@v12`). Returns the workflow's id and version, with the output or the run's state. |
| `check_workflow({ code })` | Type-check only; also gives the workflow's id and version when the namespace already has that code. Creates nothing. |
| `test_workflow({ code?, workflow?, tests? })` | Runs [tests](05-testing-and-observability.md#testing) as test runs, in virtual time: the tests given, and the suite of `workflow` when it names one, against `code`, else against that workflow's version. Creates and changes nothing. |
| `get_run({ run_id, event? })` | Status and transcript, or one event of its history in full. |
| `list_runs({ status? })` | Runs started with the same labels (for example the same conversation). |
| `signal_run({ run_id, name, payload })` | Sends a signal. |
| `cancel_run({ run_id })` | Cancels a run. |
| `retry_run({ run_id, code, secrets? })` | Starts a new run from a failed one with corrected code. The calls that already happened are not made again: their recorded results are reused ([forks](05-testing-and-observability.md#replay-debugging-and-forks)). When the run's version has a suite, the suite must pass first. The correction does not become a version of the run's workflow: `update_workflow` makes one. |
| `search_actions({ query })` | Returns the declarations matching a query, for action sets too large for the prompt. |
| `list_secrets({})` | The names of the secrets the agent's runs may use, the organization's and its namespace's, never a value. |
| `update_workflow({ workflow, code?, name?, description?, input?, output?, signals?, secrets?, tests?, update?, retire? })` | Changes a [workflow](#workflows), named by its id or its name. A name and a description keep its version. New code, an interface (JSON Schemas for its input, its output and the signals it accepts) or the secrets its runs use make its next version, stored only if its suite passes (its tests, updated and retired with reasons, plus the new ones). Tests alone join its suite once they pass against its latest version. The agent's later code can start it by name and signal its runs. |
| `get_workflow({ workflow })` | A workflow's code, schemas, versions, suite and source, with the reasons its tests were updated or retired: what the model reads to repair or extend a workflow it did not write. |
| `record_tests({ workflow, query?, runs?, limit? })` | Records tests from past runs of a workflow, one per path, and returns their digests for `update_workflow` ([recorded tests](05-testing-and-observability.md#tests-recorded-from-runs)). |

Over MCP ([Claude](#claude-as-the-harness)), the same tools but `update_workflow`, with three
more to find what the connection reaches (`list_organizations`, `list_namespaces`,
`list_action_sets`), and the organization, the namespace and the action set as arguments of
each call ([07](07-code-architecture.md#mcp-and-oauth)). No agent tool promotes, forks or pulls a
workflow: the harness and the dashboard do.

`labels` are set by the harness on every run the agent starts. They select the runs of
`list_runs` and the webhooks the harness gets. A token or an API key whose allowed labels are
these makes them hold for every tool that takes a `run_id`, within the namespace the agent acts
in ([access](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)).

### What `run_workflow` returns

The result goes into the model's context, so it is bounded by design. It always names the
workflow, by its id and version, and:

- **Completed**: the output, truncated above 256 KB, with the run id so the model can read
  the rest through `get_run`; plus the last log lines.
- **Waiting**, or still queued or running when `wait` ran out: the run id, its status and what
  the run is waiting for.
- **Failed**: the error with its TypeScript location and a code frame, the transcript up to
  the failure, and the call that failed, if any.

## What the model writes

A workflow's code is a single ES module. Its default export is the entry point; its input and
output are JSON.

```ts
import { github, slack } from "rewind:actions";
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

### Modules available to workflows

- `rewind:actions`: generated per action set. One object per group: an MCP server, an API or a
  group of harness actions.
- `rewind:workflows`: generated per action set. One export per organization [workflow](#workflows)
  the action set pins.
- `rewind`: the runtime standard library.

```ts
declare module "rewind" {
  type Duration = number | `${number}${"ms" | "s" | "m" | "h" | "d" | "w"}` | string; // "1h30m"
  export function sleep(duration: Duration): Promise<void>;
  // In a run of a version that declares its signals, the name is one of them and the payload has its type.
  export function signal<T = unknown>(name: string): Promise<T>;
  export function signal<T = unknown>(name: string, opts: { timeout: Duration }): Promise<T | undefined>;
  export function signalUrl(name: string): Promise<{ url: string }>;
  export const run: { readonly id: string; readonly startedAt: Date };
  export class ActionError extends Error { action: string; outcomeUnknown: boolean; data?: unknown }
  export class CancelledError extends Error {}
  export class RunError extends Error {
    workflow: string; runId: string; status: "failed" | "cancelled" | "aborted" | "invalid_output";
  }
}
```

Nothing else is importable in 0.1: no npm, no relative imports, no dynamic `import()`. A
curated set of deterministic utility libraries can be bundled later.

Code written with the names Rewind used before November 2026 keeps working: the sandbox still
resolves `rewind:tools`, `webhook()`, `ToolError` and `WorkflowError` for code that uses them, as
`rewind:actions`, `signalUrl()`, `ActionError` and `RunError`. They are declared nowhere:
declarations, prompts and docs show only the names above
([decided](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)).

### Signals

A signal is a named message sent to a run from the outside. Signals can come from any
source: the harness, a human through a UI, another agent (with `signal_run`), another
run (through [`rewind:workflows`](#workflows)), or an external system through a signal URL.

```
POST /v1/runs/{run_id}/signals/approval   { "close": true }
```

```ts
const decision = await signal<{ close: boolean }>("approval");
```

Each run has one mailbox per signal name, first in first out:

- If the code is already waiting on `signal("approval")`, the signal resolves it.
- If it isn't waiting yet, the signal is kept in the `approval` mailbox. The next
  `signal("approval")` returns it immediately. Nothing is lost if the human approves
  before the code reaches that line.
- Two signals with the same name are returned by two successive `signal()` calls, in
  arrival order.
- With `{ timeout }`, `signal()` returns `undefined` if nothing arrived in time.

A signal can carry an idempotency key (the `Idempotency-Key` header). The same key sent
twice to a run delivers one signal. Without it, a retried request leaves a second
`approval` in the mailbox, and that one approves the code's next `signal("approval")`.

External systems don't know run ids. For them, the code asks for a signal URL:

```ts
const hook = await signalUrl("pr_merged");        // built-in action, recorded
await github.create_webhook({ repo, url: hook.url });
const merged = await signal("pr_merged");         // anything POSTed to hook.url
```

`signalUrl(name)` returns a unique URL bound to this run and this signal name. Every
request posted to it becomes a `pr_merged` signal on the run. When the sender gives its
request an id (GitHub's delivery id, for one), it is used as the signal's idempotency key.

Signal names starting with `rewind:` are reserved for Rewind. A run of a version that declares
its signals ([workflows](#workflows)) accepts only those, with a payload that matches their
schema: anything else is refused (422), whoever sends it. A run of a version without an
interface accepts any name, and `signal<T>(name)` only says what the code expects.

### Workflows

A **workflow** is the template of its runs: the model writes it, and running it gives a run.
Every run is a run of a workflow, and the runs it starts are its child runs.

- **Its id.** The API gives a workflow its id, `wf_` and 12 characters (`wf_7k2m9q4x8d1c`), the
  first time it sees its code in a namespace: a run given code that neither its namespace nor the
  organization has creates a workflow in its namespace, with that code as its version 1. The
  same code sent again without naming a workflow is the same workflow: its id does not change
  while its code does not. A check or a test reads workflows, and creates or changes none.
- **Its versions.** A workflow has versions 1, 2, 3…, each a code, an interface, a pinned action
  set and the secrets its runs use. A version is immutable: its `version` is its number, its
  `digest` the hash of its document, and `triage@v12` and `triage@<digest>` both name it. New
  code becomes a workflow's next version only when the call names that workflow, by its id or
  its name; code sent without one is another workflow.
- **Its name and description.** A name is optional. It is an identifier (`rewind:workflows`
  exports it) that does not start with `wf_`, unique in the workflow's scope; a namespace's name
  shadows the organization's: a run or a harness in a namespace that names `gym` gets the
  namespace's `gym`, else the organization's. With a name, callers start the workflow by name and
  read it in its runs' handles. A workflow shows as its name, else the first line of the comment
  at the top of its latest version's code, else its short id (`wf-7k2m9q`); its description is the
  one given, else the rest of that comment. Both are display only: never its identity, its
  version or its runs' handles ([names](04-tools.md#workflow-names)).
- **Its interface.** JSON Schemas for its input, its output and each signal it accepts. A
  version without one accepts any input, output and signal name, as the version a run creates
  does. Giving one makes the next version, checked against its code: the default export must
  accept the input and return the output, and `signal(name)` only takes a declared name and
  returns that signal's type. tsgo checks every call to a workflow's runs before the code runs.
- **Its scope.** A workflow belongs to the namespace whose run created it: end users are
  isolated by their namespace, their code too. A member or an API key with `workflows:write`
  promotes it to the organization, or creates one there directly (a request without
  `Rewind-Namespace`). The organization's workflows are generic: any namespace's run executes
  them as they are.
- **Its forks.** A namespace changes an organization workflow by forking it. A fork is a
  namespace workflow whose `source` is the organization's workflow and one of its versions. It
  keeps its source's name, so the namespace's runs that name the workflow get the fork from then
  on; it starts with a copy of its source's suite, without the tests recorded from another
  namespace's runs, which then evolves on its own; and it pulls
  its source's latest version (its code, interface and action set), which becomes its next
  version once the fork's own suite passes, else nothing changes (422 `tests-failed`). Members and
  API keys with `workflows:write` fork and pull, so a harness forks for its end users; an end
  user's token does not. A namespace's workflow is not forkable.

```ts
const triage = await rewind.workflows.create({ // no namespace: an organization's workflow
  name: "triage",
  description: "Triage the stale issues of a repository, after approval.",
  actionSet: github.id,
  code: triageCode, // export default async function main(input: { repo: string }) { … }
  input: { type: "object", properties: { repo: { type: "string" } }, required: ["repo"] },
  output: { type: "object", properties: { closed: { type: "integer" } }, required: ["closed"] },
  signals: { approval: { type: "object", properties: { close: { type: "boolean" } }, required: ["close"] } },
});
// triage.id: "wf_…", triage.version: 1
```

| Call | Reads | Creates or changes |
| --- | --- | --- |
| A run with code, no workflow named | Its namespace, then the organization's workflows | A workflow in its namespace and its version 1, when the code is new there; the run |
| A run of a workflow (its id or name, a version or the latest) | That version | The run only |
| A check of code | The code, the action set | Nothing: diagnostics |
| A test of code or of a workflow's version | The version, the tests | A test report; the workflow is unchanged |
| New code, an interface, an action set or secrets for a workflow it names | The workflow, its suite | Its next version, once its suite passes; the same id |
| A test added to its suite | The workflow | The test, once it passes against the latest version; the same id, no new version |
| A name, a description | The workflow | Those fields; the same id, no new version |
| Promoting it | The workflow | It moves from its namespace to the organization |
| Forking an organization workflow | The workflow, its version | A namespace workflow with its name, its version as version 1 and a copy of its suite |
| Pulling into a fork | The fork, its source | The fork's next version, once the fork's suite passes |

Each workflow carries a **suite**: end-to-end tests that give its runs a world (input, answers
to their calls, signals) and check what leaves them (how they end, the calls they make). A new
version is stored only if every test of the suite passes; it retires or updates a test only with
a reason, and each version records the suite it passed. Past runs of a workflow can be recorded
as tests ([testing](05-testing-and-observability.md#testing)).

An action set lists the organization's workflows its code may start, and pins their latest
versions (or the version it names: `triage@v12`) when it is registered, as it pins the schemas
of MCP tools ([action sets](04-tools.md#action-sets-and-code-generation)). The code imports them
from `rewind:workflows`:

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
| `run.result()`, `run.result({ timeout })` | The run's output. Rejects with `RunError` if the run failed, was cancelled or aborted. With a timeout, `undefined` if the run has not ended in time. |
| `triage.get(runId)` | A handle on an existing run of `triage`, which can be signalled. |

A child run started through `rewind:workflows` runs the pinned version, whatever its namespace
has forked since.

A run started by a workflow's code is **attached** to its parent unless it is started with
`detached: true`. A parent does not end before its attached children, as it does not end with a
call in flight, so a forgotten `result()` is harmless. A parent that fails, is cancelled or is
aborted cancels its attached children still active. A detached child lives on its own: use it to
start a long workflow and move on.

The harness can also start a run of a workflow: `POST /v1/runs { workflow: "triage", input }`,
by its id or its name, with `triage@v12` for a version, else its latest; in a namespace, a name
is the namespace's workflow, else the organization's. How all of this lives in the history:
[execution](03-execution.md#workflows-across-runs).

### What exists in the sandbox

| Global | Behavior |
| --- | --- |
| `Date.now()`, `new Date()`, `performance.now()`, `Temporal.Now`, `Intl.DateTimeFormat` | Logical time (see [execution](03-execution.md)). |
| `Math.random()` | Seeded PRNG (sfc32). |
| `crypto.getRandomValues()`, `crypto.randomUUID()` | Seeded CSPRNG (ChaCha20). Workflows use them to make tokens and passwords, so their output must not be predictable. |
| `setTimeout`, `setInterval` | Durable timers, same as `sleep`. |
| `console.*` | Recorded as log annotations. |
| `URL`, `URLSearchParams`, `TextEncoder`, `TextDecoder`, `atob`, `btoa`, `structuredClone`, `queueMicrotask` | Present: pure and deterministic, and models use them by habit. They come from existing deno crates ([decided](08-decisions.md#taken-after-the-layering-november-2026)). |
| `ArrayBuffer`, typed arrays | Present. Their memory is bounded per instance, 128 MiB by default: past it, an allocation throws `RangeError: Array buffer allocation failed` ([limits](03-execution.md#resource-limits)). A resizable buffer counts its maximum length. |
| `fetch` | Absent, unless the action set enables the built-in `http` action with an allow-list of hosts. It is then a recorded call like any other, sent by the dispatcher ([egress](04-tools.md#the-http-action-and-egress)). |
| `WeakRef`, `FinalizationRegistry`, `SharedArrayBuffer`, `Atomics`, `WebAssembly` | Removed (they expose GC timing or threads, or run code the sandbox cannot see the source of). |
| File system, env, process, network, `Deno`, Node APIs | Absent. No file system, ever. |

Both random generators are keyed by the run's 256-bit seed, recorded in `run_started`.

The isolate runs with `TZ=UTC` and the `en-US` ICU locale, so `Intl` output does not
depend on the machine. The sandbox refuses to start if `TZ` is not UTC.

**What type-checks is what exists.** The checker's `lib` declares exactly the globals of
this table and the ECMAScript built-ins, and nothing else (no DOM lib). A name that models
reach for by habit but that is absent (`fetch`, `require`, `process`, `Buffer`) gets a
hint in `rewind check` diagnostics and in the transcript, saying what to use instead. Adding
or changing a global changes what code can observe, so it bumps the
[determinism epoch](03-execution.md#determinism-epoch).
