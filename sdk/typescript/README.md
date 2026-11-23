# @rewind.codes/sdk

The TypeScript SDK of Rewind, for agent harness developers. It has three parts:

- **the client** of the HTTP API `/v1`
  ([`api/openapi.json`](https://github.com/planchon/rewind.codes/blob/main/api/openapi.json)): toolsets, workflows,
  runs, signals, forks, traces, journals, secrets, webhooks, and the end users of the harness (namespaces, their
  tokens and their quotas);
- **`verifyRewindCall`**, for the harness's own APIs and MCP servers: it checks the calls Rewind signs with the
  organization's key, and says which end user each acts for;
- **the host-tool worker**, which executes the harness's own tools when scripts call them;
- **the agent tools**, ready-made tools for the model (`run_script`, `get_run`, `save_workflow`…), with adapters for
  the Anthropic and OpenAI tool formats.

```sh
npm install @rewind.codes/sdk
```

Node 22.12 or later. ESM only. The package depends on no model SDK. The guides are at
[docs.rewind.codes](https://docs.rewind.codes), and runnable programs in
[`examples/`](https://github.com/planchon/rewind.codes/tree/main/examples).

## The client

```ts
import { Rewind } from "@rewind.codes/sdk";

const rewind = new Rewind({ url: process.env.REWIND_URL, apiKey: process.env.REWIND_API_KEY }); // the defaults

const toolset = await rewind.toolsets.create({
  mcp: [{ name: "github", url: "https://mcp.example.com/github" }],
  host: [
    {
      name: "ask_user",
      description: "Ask the end user a question and return their answer.",
      inputSchema: { type: "object", properties: { question: { type: "string" } }, required: ["question"] },
      policy: { onInterruption: "at_most_once" },
    },
  ],
});
// toolset.declarations.prompt or .dts → the model's system prompt

// Every run acts for one end user of the harness: its namespace (below).
const run = await rewind.namespace(user.id).runs.start({
  toolset: toolset.id,
  script: code,
  input: {},
  wait: "30s",
  idempotencyKey: toolCall.id, // a retried request returns the same run
  labels: { agent: "triage", conversation: conversation.id },
  traceparent, // the LLM call that wrote the script
  ttl: "7d",
});
```

A workflow is a script with a name and JSON Schemas for its input, its output and the signals it accepts. Each save is
a new immutable version; a toolset that names it lets its scripts `import { triage } from "rewind:workflows"`, start
runs of it, signal them and wait for their output.

```ts
const triage = await rewind.workflows.create({
  name: "triage",
  description: "Triage the stale issues of a repository, after approval.",
  toolset: toolset.id,
  script: triageSource,
  input: { type: "object", properties: { repo: { type: "string" } }, required: ["repo"] },
  output: { type: "object", properties: { closed: { type: "integer" } }, required: ["closed"] },
  signals: { approval: { type: "object", properties: { close: { type: "boolean" } }, required: ["close"] } },
});
// triage.declarations.prompt: how a script starts it, signals its runs and reads their result

// A toolset whose scripts start it: derived from `toolset`, without listing its servers again.
const withTriage = await rewind.toolsets.create({ base: toolset.id, workflows: ["triage"] });

// The harness can start a run of it too, for an end user, by name (its latest version) or by version.
const triageRun = await rewind.namespace(user.id).runs.start({
  workflow: `triage@${triage.version.slice(0, 12)}`,
  input: { repo: "acme/api" },
});
// triageRun.workflow: { name, version }; a run a script started has `startedBy: { run, seq, detached }`
```

A version carries a **suite**: the tests it passed, which every version derived from it must pass too. A test gives the
world a run meets (its input, the answers of the tools it may call, the signals sent to it) and what the outside must
see at the end; it never names its script. Saving a version runs the parent's tests, minus those it `retire`s, with
those it `update`s replaced, plus its new `tests`, and saves it only if every test passes (otherwise `tests-failed`,
whose `error.tests` holds each outcome). Tests can also be recorded from past runs, one per path, and referenced by
digest.

```ts
const recorded = await rewind.workflows.record("triage", { query: "status:completed", limit: 20 });

const next = await rewind.workflows.create({
  name: "triage",
  description: triage.description,
  toolset: toolset.id,
  script: fixedSource,
  input: triage.input,
  output: triage.output,
  signals: triage.signals,
  tests: [
    ...recorded.tests.map((test) => test.digest),
    {
      name: "closes nothing without approval",
      input: { repo: "acme/api" },
      world: { tools: { "github.list_issues": { returns: [{ number: 4 }] } } },
      signals: [{ name: "approval", payload: { close: false } }],
      expect: { status: "completed", output: { closed: 0 }, calls: [{ tool: "github.close_issue", times: 0 }] },
    },
  ],
  retire: [{ names: ["asks before closing"], reason: "under 5 stale issues, it closes without asking" }],
});

// Check a candidate first: the given tests, plus the suite of the latest version of triage, against the new script.
const outcome = await rewind.namespace(user.id).scripts.test({ script: fixedSource, workflow: "triage" });
```

| Resource                     | Calls                                                                                                                                                                                                                                                                                                    |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rewind.toolsets`            | `create` (`mcp`, `apis` from OpenAPI documents, `host`, `http`; `workflows` pins workflows, `base` derives from a toolset), `declarations`, `search`                                                                                                                                                     |
| `rewind.workflows`           | `create` (through the gate of its suite: `parent`, `tests`, `update`, `retire`), `list` and `listAll` (the latest version of each; `script`: the versions whose script it is), `versions` (newest first), `record` (tests from past runs)                                                                |
| `rewind.tests`               | `get`: a stored test, by digest                                                                                                                                                                                                                                                                          |
| `rewind.scripts`             | `check`, `test` (tests in mock mode, virtual time, and the suite of a `workflow`), `list`, `listAll`, `get`                                                                                                                                                                                              |
| `rewind.runs`                | `start` (a script, or a `workflow`), `get`, `list` and `listAll` (`q`: the query language, `sort`, `workflow`, `startedBy`), `stats`, `failures`, `signal`, `cancel`, `fork`, `trace`, `entry`, `events`, `journal` (`as: "cassette"`), `release`, `giveUp`, `delete`; `start` and `fork` take `secrets` |
| `rewind.tasks`               | `stream` (the task stream), `defer`, `complete`, `fail`, `approve`, `reject`                                                                                                                                                                                                                             |
| `rewind.approvals`           | `pending(run)`, `args`, `approve`, `reject`                                                                                                                                                                                                                                                              |
| `rewind.payloads`            | `get(digest, { run })`: a value left out above 256 KiB, read through its run                                                                                                                                                                                                                             |
| `rewind.namespaces`          | `list` and `listAll` (`prefix`, `above`, `order`, `month`), `get`, `put` (exceptions), `rename`, `delete`, `alerts`                                                                                                                                                                                      |
| `rewind.namespaces.secrets`  | `list`, `put`, `delete`, `usage`: an end user's own secrets, attached as `ns/<name>`                                                                                                                                                                                                                     |
| `rewind.namespaces.defaults` | `get`, `put`: the default limits of every namespace, and the ceiling                                                                                                                                                                                                                                     |
| `rewind.namespaces.tokens`   | `create` (its answer holds the token, once), `revoke`, `revokeAll`                                                                                                                                                                                                                                       |
| `rewind.tokenKeys`           | `create`, `list`, `delete`: the keys the harness signs end users' tokens with                                                                                                                                                                                                                            |
| `rewind.secrets`             | `list`, `put`, `delete`, `usage`: the organization's secrets, attached as `org/<name>`                                                                                                                                                                                                                   |
| `rewind.signingKeys`         | `list` (with the `issuer` and the `jwksUrl`), `rotate`, `jwks`: the keys Rewind signs its calls with                                                                                                                                                                                                     |
| `rewind.subscriptions`       | `create` (its answer holds the secret, once), `list`, `delete`                                                                                                                                                                                                                                           |
| `rewind.webhooks`            | `verify`                                                                                                                                                                                                                                                                                                 |
| `rewind.tools`               | `stats`                                                                                                                                                                                                                                                                                                  |
| `rewind.apiKeys`             | `create`, `revoke`                                                                                                                                                                                                                                                                                       |
| `rewind.server`              | `info` (the version and epochs, the node that answered, the partitions and nodes), `stop`                                                                                                                                                                                                                |

Every call takes an `AbortSignal` (`signal`). The types are those of the API, generated from the OpenAPI document and
exported under their names (`Run`, `Toolset`, `HandedTask`, `WebhookEvent`…).

**Errors.** The API answers errors as `application/problem+json`. The client throws them as `RewindApiError`, with
`status`, `slug` (`"run-ended"`, `"invalid-script"`, `"invalid-workflow"`, `"tests-failed"`, `"invalid-signal"`…),
`detail` (written for a model), `diagnostics` (for a refused script), `tests` (for `tests-failed`: each test's outcome),
`attachments` (for `secret-required`: each attachment at fault, why, and the secrets that may be attached instead),
`query` (for a query that cannot be read: where the term at fault is) and `quota` (for `quota-exceeded`: which quota,
at which scope, its limit, the use and when it frees). `isProblem(error, "task-gone")` tests the kind. A key's client that starts a run, checks or tests a script
without naming a namespace gets the server's 400 as a `NamespaceRequiredError`. A server that cannot be reached gives a
`RewindConnectionError`.

**Retries.** A request that is safe to repeat is sent again after a network error or a 503, twice by default
(`maxRetries`): reads, `PUT`, `DELETE`, the content-addressed toolset registration and workflow save, checks and
tests, and the requests that carry an idempotency key. Starting a run, forking it or sending a signal without a key is
never repeated, nor is minting a token.

**Waiting.** `runs.start`, `runs.fork`, `runs.cancel` and `runs.get` take `wait`: one request, which the server holds
until the run ends or is quarantined, or until `wait` passes, however long it is. Nothing is asked again once the server
has answered. A connection cut while the server holds it (a proxy, or the HTTP client's own timeout: Node's `fetch`
waits 5 minutes for an answer) is sent again with the time left; a start or a fork that waits carries an idempotency
key for that, the caller's or one the client draws.

**Pages and statistics.** A page of `runs.list` may hold fewer runs than `limit`, or none, and still have a
`nextCursor`: the server stopped at its scan budget; `listAll` follows the cursor to the last page. `runs.stats`,
`runs.failures`, `scripts.list` and `tools.stats` answer from hourly counters: a scope (`q: "namespace:<name>"`, the
client's own namespace, or every namespace it reaches) and a window of whole hours; another term of `q` is refused
with `invalid-query`, and a key or a token limited to labels with `forbidden`. `scripts.list` also takes `tool` and
`digest`; `tools.stats` takes `prefix`, the start of a tool's name, and `tool`.

**The event stream.** `runs.events(id, { follow: true })` is an async iterator over the run's journal entries as they
are committed, ending with `{ type: "end" }`. A dropped connection resumes after the last entry received.

## End users: namespaces and tokens

A harness serves its own users, the **end users** of its product. Each one has a **namespace**: their runs, the scripts
those runs ran, their credentials and their use live there, and nothing of one end user is visible to another. A
namespace's name is the harness's own id for the end user: 1 to 128 ASCII letters, digits, `-`, `_`, `.`, `:` and `@`
(an opaque id is better than an email: it appears in webhooks, the dashboard and the service's logs). It is created at
its first use.

**The harness's backend** holds its API key, which reaches every namespace, and names the end user of each request with
`rewind.namespace(name)`: a client whose requests carry the header `Rewind-Namespace`. A key needs it to start a run,
check or test a script (there is no default namespace); on a read, it narrows the answer to that namespace, and a run
of another end user is missing (404). Without it, a read covers every namespace.

```ts
const asAlice = rewind.namespace(user.id); // cheap: one per request is fine
const run = await asAlice.runs.start({ toolset: toolset.id, script: code, wait: "30s" });
const runs = await asAlice.runs.list({ status: "failed" }); // Alice's runs only
await rewind.runs.start({ script: code }); // NamespaceRequiredError: the server's 400
```

**The agent acting for one end user** holds a token of their namespace. With it the agent, and every script its model
writes, reaches that namespace only. The backend mints an opaque token (`rewind_nt_…`; 1 hour by default, 30 days at
most), or signs a JWT itself with a key it registered.

```ts
// In the harness's backend, with its key:
const { token } = await rewind.namespaces.tokens.create(user.id, {
  ttl: "1h",
  permissions: ["runs:read", "runs:start", "runs:control", "toolsets:read"], // the default
  labels: { conversation: conversation.id }, // optional: the runs it reaches carry them
});

// In the agent acting for that user:
const asUser = new Rewind({ url: process.env.REWIND_URL, token });
const agentTools = asUser.agentTools({ toolset: toolset.id });

// Revoking: one token by its id, or every token of the namespace (signed ones issued before now included).
await rewind.namespaces.tokens.revoke(id);
await rewind.namespaces.tokens.revokeAll(user.id);
```

A token the harness signs needs no call to Rewind: register the public key once, then sign with any JOSE library, such
as [`jose`](https://github.com/panva/jose). The header names the key (`kid`), `sub` is the namespace, `aud` is
`rewind`, `iat` and `exp` are at most 30 days apart; `permissions`, `labels` and `jti` are optional
(`SignedTokenClaims`).

```ts
import { SignJWT, exportJWK, generateKeyPair } from "jose";

const { publicKey, privateKey } = await generateKeyPair("EdDSA", { crv: "Ed25519" });
const { kid } = await rewind.tokenKeys.create({ name: "backend", jwk: await exportJWK(publicKey) });

const token = await new SignJWT({ permissions: ["runs:read", "runs:start"] })
  .setProtectedHeader({ alg: "EdDSA", kid })
  .setSubject(user.id)
  .setAudience("rewind")
  .setIssuedAt()
  .setExpirationTime("1h")
  .sign(privateKey);
// Deleting the key (rewind.tokenKeys.delete(kid)) refuses every token it signed.
```

**Quotas** protect the platform and the harness from an end user who abuses its product: CPU time per month (alerts
only), active runs, runs per month, distinct scripts, the longest TTL and the longest script. The harness sets the
default limits of every namespace and the exceptions of one (a paying end user, a blocked one), under its ceiling.

```ts
await rewind.namespaces.defaults.put({ defaults: { activeRuns: 5, runsPerMonth: 1_000, maxTtl: "7d" } });
await rewind.namespaces.put("user_2fQ81", { exceptions: { activeRuns: 0 } }); // blocked

const nearLimits = await rewind.namespaces.list({ above: 0.8, order: "runs" }); // at 80 % of a limit or more
const alice = await rewind.namespaces.get(user.id); // limits, use per day, alerts, tokens in use
const { alerts } = await rewind.namespaces.alerts({ month: "2026-11" });

try {
  await asAlice.runs.start({ script: code });
} catch (error) {
  if (isProblem(error, "quota-exceeded")) console.log(error.quota); // { quota: "runs_per_month", scope, limit, used, resetsAt }
}
```

- An end user's own secrets (their GitHub token): `rewind.namespaces.secrets.put(user.id, "github", { value })`, which
  their runs attach as `ns/github` ([secrets](#secrets-and-signed-calls)).
- `rewind.namespaces.delete(user.id)` erases the end user in the background: their tokens, runs, secrets and use.
- Toolsets and webhook subscriptions stay the harness's: register the tools once, receive every end user's events,
  each naming its `namespace`.

## Secrets and signed calls

Each MCP server and API of a toolset says in its `auth` how its calls prove who calls: nothing, a **secret** the run
attaches, or a **signature** of the organization's key. A toolset holds no secret.

```ts
const toolset = await rewind.toolsets.create({
  mcp: [
    {
      name: "github",
      url: "https://api.githubcopilot.com/mcp/",
      auth: { type: "secret", header: "Authorization", format: "Bearer {secret}" },
    },
  ],
  apis: [{ name: "dax", openapi: "https://api.dax.example/openapi.json", auth: { type: "rewind" } }],
});

// Secrets are written, never read back: the organization's, and each end user's own.
await rewind.secrets.put("zendesk", { value: process.env.ZENDESK_TOKEN!, labels: { env: "prod" } });
await rewind.namespaces.secrets.put(user.id, "github", { value: githubTokenOf(user) });

// A run reads only the secrets it attaches, server by server, in the request, never in the script.
const run = await rewind.namespace(user.id).runs.start({
  toolset: toolset.id,
  script,
  secrets: { github: "ns/github" },
});
```

- `org/<name>` is the organization's secret, `ns/<name>` the run's namespace's. Attaching an organization's secret
  needs `secrets:use`, and a key limited to labels sees, writes and attaches only the secrets carrying them; a
  namespace's secret is attached by whoever starts a run in it.
- A workflow version carries its attachments (`workflows.create({ …, secrets })`), checked against the saver; its runs
  give none. A fork keeps the parent's unless it gives its own (`runs.fork(id, { script, secrets })`).
- A start that misses an attachment, names a secret that does not exist, or one the caller may not attach is refused
  before any run exists: `secret-required`, whose `error.attachments` say which server and what may be attached.
- `secrets.usage(name)` says what deleting a secret would stop: the active runs that attach it, those waiting on a call
  it serves, the workflows whose latest version attaches it. A call whose secret was deleted or refused is parked
  (webhook `credentials.expired`); putting the secret again sends it again, with the same idempotency key.

**Checking a signed call.** With `auth: { type: "rewind" }`, every request carries a JWT signed with the organization's
Ed25519 key. `verifyRewindCall` does every check the receiving server must do: the signature against
`{issuer}/jwks.json` (cached, an unknown `kid` fetched again at most every 10 s), the issuer and the audience, `exp` and
`iat` with 5 s of leeway, the method, the URL and the SHA-256 of the raw body, and the run's labels when given. It
takes a Fetch `Request`, or `{ method, url, headers, body }` with the raw body for Express or NestJS:

```ts
import express from "express";
import { RewindSignatureError, verifyRewindCall } from "@rewind.codes/sdk";

const { issuer } = await rewind.signingKeys.list(); // https://api.rewind.codes/organizations/org_…

app.set("trust proxy", true);
app.post("/v1/measures", express.raw({ type: "application/json" }), async (req, res) => {
  let call;
  try {
    call = await verifyRewindCall(
      {
        method: req.method,
        url: `${req.protocol}://${req.get("host")}${req.originalUrl}`,
        headers: req.headers,
        body: req.body,
      },
      { issuer, audience: "https://api.dax.example", labels: { env: "prod" } },
    );
  } catch (error) {
    if (error instanceof RewindSignatureError) return res.status(401).json({ error: error.check }); // Rewind parks the call
    throw error; // the keys could not be fetched: a 5xx, which Rewind retries
  }
  // call.namespace is the end user; call.idempotencyKey is the same for every attempt of the call.
  res.json(await createMeasure(call.namespace, JSON.parse(req.body.toString()), call.idempotencyKey));
});
```

One key signs every run of the organization: an organization whose development and production share it gives
`labels: { env: "prod" }`, so a production server refuses the calls of a development run. A `RewindSignatureError` names
the check that failed (`check`: `signature`, `kid`, `iss`, `aud`, `exp`, `iat`, `htm`, `htu`, `body`, `labels`…). An
MCP server passes `session: true`: the requests of the session outside a call (`initialize`, the listing of the tools)
are answered `null`. `signingKeys.rotate()` replaces the key; the old one stays published 24 hours.

## The host-tool worker

A script calls a host tool like any tool. The worker holds a task stream open (`GET /v1/tasks/stream`); the server hands
it each task as it becomes ready, the worker runs the handler and answers with the result. Nothing is sent while a
handler runs: the open stream is the liveness of every task handed on it.

```ts
import { HostToolError } from "@rewind.codes/sdk";

const worker = rewind.worker({
  toolset: toolset.id,
  concurrency: 10,
  tools: {
    lookup_customer: async ({ email }, ctx) => {
      // ctx.namespace is the end user the run acts for: act on their data only.
      // ctx.idempotencyKey is the same on every attempt: use it to recognize a call already made.
      const customer = await crm.find(ctx.namespace, email, { signal: ctx.signal });
      if (!customer) throw new HostToolError(`no customer ${email}`); // the script gets a ToolError
      return customer;
    },
    ask_user: async ({ question }, ctx) => {
      // The answer may take days: hand the token over and answer later.
      const later = await ctx.defer({ timeout: "3d" });
      await inbox.ask(question, later.token);
    },
  },
});

// Later, maybe in another process, when the user answers:
await rewind.tasks.complete(token, { answer: "yes" });

await worker.stop(); // no new task; waits for the handlers in flight to answer
```

- One worker with the harness's key serves every end user: each task names the namespace of its run
  (`ctx.namespace`). A worker holding an end user's token is handed only the tasks of that namespace's runs.
- `concurrency` (10 by default, 100 at most) is the number of slots of the stream: the server hands at most that many
  tasks at once. A deferred task frees its slot.
- After a drop, the worker opens its stream again with the same id (`worker.id`), after a backoff (500 ms, doubling up
  to 30 s, starting over once a stream lived); the server keeps the tasks handed to it for their tool's grace (60 s by
  default), and the handlers in flight go on. A stream silent for 45 s (the server writes a comment at least every
  15 s) is taken as broken. `stop()` keeps the stream open with no slot until the handlers in flight have answered.
- A thrown `HostToolError` is the tool's failure: the script gets a `ToolError` with its `message` and `data`. With
  `retryable: true`, a tool under the `retry` policy is called again later. Any other exception fails the call too,
  with `Name: message`, and is reported to `onError`.
- `ctx.signal` aborts when the task no longer waits for its answer: the stream says it is gone (its run ended, its call
  timed out), or the server refused an answer as `task-gone`. The result is then dropped, as the server would drop it.
- After `ctx.defer()`, the handler's return value is not an answer: whoever holds the token answers. If the handler
  throws after deferring, the worker fails the task.
- A refused key (401, 403), an invalid request or an unknown toolset (400, 404) stops the worker: `worker.done`
  rejects. Other failures are reported to `onError` (`console.error` by default), and the stream is opened again.

## Approvals

A tool with the `approval` policy waits for the harness before it is sent, and so does a call parked because its tool
changed during the run. The run is then suspended; its state says which call waits (`Run.waitingFor`, also in the
`run.suspended` webhook).

```ts
for (const approval of rewind.approvals.pending(run)) {
  const args = await rewind.approvals.args(approval); // from the run's state, or by its digest when large
  if (await askAHuman(approval.tool, args)) await rewind.approvals.approve(approval);
  else await rewind.approvals.reject(approval, "the user refused");
}
```

## The agent tools

```ts
// The model's runs belong to the end user: a client bound to their namespace, or holding their token.
const agentTools = rewind.namespace(user.id).agentTools({ toolset: toolset.id, labels: { conversation: convId } });

// Anthropic Messages API
const answer = await anthropic.messages.create({ model, messages, tools: agentTools.anthropic.tools });
const results = await Promise.all(
  answer.content.filter((block) => block.type === "tool_use").map((block) => agentTools.anthropic.handle(block)),
);
messages.push({ role: "assistant", content: answer.content }, { role: "user", content: results });

// OpenAI Chat Completions: agentTools.openai.tools and agentTools.openai.handle(toolCall)
// OpenAI Responses:        agentTools.openai.responses.tools and agentTools.openai.responses.handle(functionCall)
// Any other API:           agentTools.definitions and agentTools.handle(name, input, { toolCallId })
```

| Tool                                                                                                     | Does                                                                                                                      |
| -------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `run_script({ code, input?, wait? })`                                                                    | Type-checks and starts a run; returns its output, its error, or what it waits for                                         |
| `check_script({ code })`                                                                                 | Type-check only                                                                                                           |
| `test_script({ code, tests?, workflow? })`                                                               | Runs tests against the script in mock mode, in virtual time, and the suite of `workflow`; each test's outcome             |
| `get_run({ run_id, entry?, offset?, wait? })`                                                            | The run's state and trace; one journal entry in full; the next page of a long output                                      |
| `list_runs({ status? })`                                                                                 | The runs started with the same labels                                                                                     |
| `signal_run({ run_id, name, payload? })`                                                                 | Sends a signal                                                                                                            |
| `cancel_run({ run_id })`                                                                                 | Cancels a run                                                                                                             |
| `retry_run({ run_id, code })`                                                                            | Forks the run with a corrected script: the calls already made are not made again; a workflow's suite must pass first      |
| `search_tools({ query })`                                                                                | The declarations of the matching tools (only with a toolset)                                                              |
| `save_workflow({ name, description, code, input, output, signals?, tests?, update?, retire?, parent? })` | Saves a workflow once its suite passes; the model's later scripts import it from `rewind:workflows` (only with a toolset) |
| `get_workflow({ name })`                                                                                 | The latest version's source and schemas, its suite with each test, and the lineage with its reasons (only with a toolset) |
| `record_tests({ workflow, query?, runs?, limit? })`                                                      | Records tests from past runs, one per path, and returns their digests for `save_workflow` (only with a toolset)           |

- Results are plain text written for the model, and bounded: `run_script` returns at most 256 KiB of output, with the
  call that reads the rest (`get_run` with `offset`); a failure shows the error with its line and code frame, the call
  that failed, and the trace up to the failure. Messages, diagnostics and traces are cut too, whatever their size.
- The model's tool call id makes the idempotency key of `run_script`, `retry_run` and `signal_run`: handling the same
  tool call twice starts one run, or delivers one signal. A run's key is the SHA-256 of the agent's `labels` and the id
  (`runIdempotencyKey`), so agents with other labels never share one, however their harness numbers its tool calls. A
  run the server answers for a key already used by another script or other labels is never shown to the model, and a
  `RewindError` is thrown.
- `labels` are set on every run the model starts. They filter `list_runs`, and the tools that take a run id answer for
  a run without them as for a run that does not exist. Give the harness an API key limited to these labels so the
  server holds the scope too.
- Mistakes the model can fix (a bad argument, a refused script, a run that has ended) come back as a result with
  `isError`, and so does a quota reached, in words written for the model. Faults of the platform or of the harness (an
  unreachable server, a 401, a 5xx, a key's client that names no namespace) are thrown.
- `result.run` holds the run the call started or read, for the harness: `rewind.approvals.pending(result.run)` says what
  waits for it.
- `save_workflow` saves the workflow with the agent's toolset, then derives a toolset from it that pins this version
  (`toolsets.create({ base, workflows: ["triage@<version>"] })`). The later `run_script`, `check_script`,
  `test_script` and `search_tools` of these agent tools use the derived toolset: `agentTools.toolset` is its id, and
  `result.workflow` and `result.toolset` hold what was saved and derived. A harness that creates the agent tools for
  each turn keeps `agentTools.toolset` with the conversation and passes it as `toolset` the next time. The derived
  toolset has another id: a worker started with `toolset: <id>` is not handed the tasks of its runs (start it without
  `toolset`, with its `tools`); the secrets its runs attach do not depend on the toolset. A refused script
  comes back with its diagnostics.
- `save_workflow` and `retry_run` (of a run of a workflow) run the suite first. When a test does not pass, the model
  reads each test's problem (`stale`, `incompatible` or `failed`), its failures and the trace of the run, and is told
  that there is no way around the gate: it fixes the script, or retires or updates the test with a reason.
  `result.tests` holds the outcomes, as it does for `test_script`. `get_workflow` reads each test of the suite in full,
  a recorded run's calls summarized; a test recorded from a namespace whose runs the agent cannot read is said so.
- Options: `tools` (a subset), `wait` (30 s), `maxWait` (2 min, the longest wait the model may ask for), `ttl`,
  `traceparent` (a string, or a function giving the current one), `outputLimit`.

## Webhooks

```ts
import { verifyWebhook, WebhookVerificationFailed } from "@rewind.codes/sdk";

app.post("/rewind-events", express.raw({ type: "application/json" }), (req, res) => {
  let event;
  try {
    event = verifyWebhook(req.body, req.headers, process.env.REWIND_WEBHOOK_SECRET!);
  } catch (error) {
    if (error instanceof WebhookVerificationFailed) return res.sendStatus(400);
    throw error;
  }
  // Deduplicate on the webhook-id header: a retried delivery keeps it.
  if (event.type === "run.completed") resumeConversation(event.data.run); // event.data.run.namespace: the end user
  if (event.type === "quota.alert") warnCustomer(event.data); // namespace (null: the ceiling), quota, threshold, used, limit
  res.sendStatus(204);
});
```

Deliveries follow Standard Webhooks: an HMAC-SHA256 signature over `{id}.{timestamp}.{body}` with the subscription's
secret `whsec_…`, and 5 minutes of tolerance on the timestamp. Pass the body exactly as received.

## Versions

The package follows [Semantic Versioning](https://semver.org). Before 1.0, a minor version (0.1 → 0.2) may break the
API and a patch version does not. Each version's changes are in
[`CHANGELOG.md`](https://github.com/planchon/rewind.codes/blob/main/sdk/typescript/CHANGELOG.md). A version speaks
the HTTP API `/v1`.

## Development

```sh
pnpm install
pnpm check          # prettier, tsc (TypeScript 7), vitest
pnpm build          # dist/
pnpm generate:api   # src/generated/api.ts from api/openapi.json; commit the diff
```

The tests run against a mocked HTTP server (`test/mock-server.ts`): a real server on a free local port, with handlers
per route.

## Releasing

1. Set the new version, following SemVer: `pnpm version <patch|minor|major|x.y.z-rc.n> --no-git-tag-version`.
2. Add its section to `CHANGELOG.md`, then commit on `main`.
3. Tag the commit `sdk-v<version>` and push the tag.

The `Release the SDK` workflow (`.github/workflows/release-sdk.yml`) checks that the tag matches `package.json`, runs
the checks, and publishes to npm with provenance through trusted publishing (no token). A prerelease (`-rc.1`) goes
to the `next` dist-tag, any other version to `latest`.
