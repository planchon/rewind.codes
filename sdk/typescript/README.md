# @rewind.codes/sdk

The TypeScript SDK of Rewind, for agent harness developers. It has three parts:

- **the client** of the HTTP API `/v1`
  ([`api/openapi.json`](https://github.com/planchon/rewind.codes/blob/main/api/openapi.json)): toolsets, workflows,
  runs, signals, forks, traces, journals, credentials, webhooks, and the end users of the harness (namespaces, their
  tokens and their quotas);
- **the host-tool worker**, which executes the harness's own tools when scripts call them;
- **the agent tools**, ready-made tools for the model (`run_script`, `get_run`, `save_workflow`…), with adapters for
  the Anthropic and OpenAI tool formats.

```sh
npm install @rewind.codes/sdk
```

Node 22.12 or later. ESM only. The package depends on no model SDK.

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

| Resource                     | Calls                                                                                                                                                                                                                                                                                   |
| ---------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rewind.toolsets`            | `create` (`workflows` pins workflows, `base` derives from a toolset), `declarations`, `search`, `setCredentials`                                                                                                                                                                        |
| `rewind.workflows`           | `create`, `list` and `listAll` (the latest version of each), `versions` (newest first)                                                                                                                                                                                                  |
| `rewind.scripts`             | `check`, `test` (mock mode, virtual time), `list`, `listAll`, `get`                                                                                                                                                                                                                     |
| `rewind.runs`                | `start` (a script, or a `workflow`), `get`, `list` and `listAll` (`q`: the query language, `sort`, `workflow`, `startedBy`), `stats`, `failures`, `signal`, `cancel`, `fork`, `trace`, `entry`, `events`, `journal` (`as: "cassette"`), `release`, `giveUp`, `setCredentials`, `delete` |
| `rewind.tasks`               | `claim`, `heartbeat`, `defer`, `complete`, `fail`, `approve`, `reject`                                                                                                                                                                                                                  |
| `rewind.approvals`           | `pending(run)`, `args`, `approve`, `reject`                                                                                                                                                                                                                                             |
| `rewind.payloads`            | `get`: a value left out above 256 KiB                                                                                                                                                                                                                                                   |
| `rewind.namespaces`          | `list` and `listAll` (`prefix`, `above`, `order`, `month`), `get`, `put` (exceptions), `delete`, `setCredentials`, `alerts`                                                                                                                                                             |
| `rewind.namespaces.defaults` | `get`, `put`: the default limits of every namespace, and the ceiling                                                                                                                                                                                                                    |
| `rewind.namespaces.tokens`   | `create` (its answer holds the token, once), `revoke`, `revokeAll`                                                                                                                                                                                                                      |
| `rewind.tokenKeys`           | `create`, `list`, `delete`: the keys the harness signs end users' tokens with                                                                                                                                                                                                           |
| `rewind.subscriptions`       | `create` (its answer holds the secret, once), `list`, `delete`                                                                                                                                                                                                                          |
| `rewind.webhooks`            | `verify`                                                                                                                                                                                                                                                                                |
| `rewind.tools`               | `stats`                                                                                                                                                                                                                                                                                 |
| `rewind.apiKeys`             | `create`, `revoke`                                                                                                                                                                                                                                                                      |
| `rewind.server`              | `info`, `stop`                                                                                                                                                                                                                                                                          |

Every call takes an `AbortSignal` (`signal`). The types are those of the API, generated from the OpenAPI document and
exported under their names (`Run`, `Toolset`, `Task`, `WebhookEvent`…).

**Errors.** The API answers errors as `application/problem+json`. The client throws them as `RewindApiError`, with
`status`, `slug` (`"run-ended"`, `"invalid-script"`, `"invalid-workflow"`, `"invalid-input"`, `"invalid-signal"`…),
`detail` (written for a model), `diagnostics` (for a refused script), `query` (for a query that cannot be read: where
the term at fault is) and `quota` (for `quota-exceeded`: which quota, at which scope, its limit, the use and when it
frees). `isProblem(error, "task-gone")` tests the kind. A key's client that starts a run, checks or tests a script
without naming a namespace gets the server's 400 as a `NamespaceRequiredError`. A server that cannot be reached gives a
`RewindConnectionError`.

**Retries.** A request that is safe to repeat is sent again after a network error or a 503, twice by default
(`maxRetries`): reads, `PUT`, `DELETE`, the content-addressed toolset registration and workflow save, checks and
tests, and the requests that carry an idempotency key. Starting a run, forking it or sending a signal without a key is
never repeated, nor is minting a token.

**Waiting.** `runs.start`, `runs.fork`, `runs.cancel` and `runs.get` take `wait`: they answer when the run ends or
`wait` passes. A wait longer than 50 seconds is split into several requests, so proxies and HTTP timeouts never cut it.

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

- An end user's own credentials for a toolset (their GitHub token): `rewind.namespaces.setCredentials(user.id,
toolset.id, credentials)`. A run uses its own credentials if it has some, else its namespace's, else the toolset's.
- `rewind.namespaces.delete(user.id)` erases the end user in the background: their tokens, runs, credentials and use.
- Toolsets and webhook subscriptions stay the harness's: register the tools once, receive every end user's events,
  each naming its `namespace`.

## The host-tool worker

A script calls a host tool like any tool. The server queues a task; the worker claims it (long-poll), runs the handler,
keeps the claim alive with heartbeats while it runs, and answers with the result.

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

await worker.stop(); // lets the claim in flight finish, then waits for the handlers
```

- One worker with the harness's key serves every end user: each task names the namespace of its run
  (`ctx.namespace`). A worker holding an end user's token claims only the tasks of that namespace's runs.
- A thrown `HostToolError` is the tool's failure: the script gets a `ToolError` with its `message` and `data`. With
  `retryable: true`, a tool under the `retry` policy is called again later. Any other exception fails the call too,
  with `Name: message`, and is reported to `onError`.
- `ctx.signal` aborts when the claim is lost (the run ended, or the claim expired and another worker took the task).
  The result is then dropped, as the server would drop it.
- After `ctx.defer()`, the handler's return value is not an answer: whoever holds the token answers. If the handler
  throws after deferring, the worker fails the task.
- A refused key (401, 403) or an invalid claim stops the worker: `worker.done` rejects. Other failures are reported to
  `onError` (`console.error` by default) and retried.

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

| Tool                                                                  | Does                                                                                                |
| --------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| `run_script({ code, input?, wait? })`                                 | Type-checks and starts a run; returns its output, its error, or what it waits for                   |
| `check_script({ code })`                                              | Type-check only                                                                                     |
| `test_script({ code, scenario })`                                     | Mock mode, virtual time; `scenario.from_run` replays the calls a run recorded                       |
| `get_run({ run_id, entry?, offset?, wait? })`                         | The run's state and trace; one journal entry in full; the next page of a long output                |
| `list_runs({ status? })`                                              | The runs started with the same labels                                                               |
| `signal_run({ run_id, name, payload? })`                              | Sends a signal                                                                                      |
| `cancel_run({ run_id })`                                              | Cancels a run                                                                                       |
| `retry_run({ run_id, code })`                                         | Forks the run with a corrected script: the calls already made are not made again                    |
| `search_tools({ query })`                                             | The declarations of the matching tools (only with a toolset)                                        |
| `save_workflow({ name, description, code, input, output, signals? })` | Saves a workflow; the model's later scripts import it from `rewind:workflows` (only with a toolset) |

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
  toolset has another id: a worker that claims tasks `toolset: <id>` does not claim those of its runs (start it
  without `toolset`, with its `tools`), and credentials set on the first toolset are not carried over (set them on the
  derived one with `toolsets.setCredentials`). A refused script comes back with its diagnostics.
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
