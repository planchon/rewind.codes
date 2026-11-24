# @rewind.codes/sdk

The TypeScript SDK of Rewind, for agent harness developers. It has four parts:

- **the client** of the HTTP API `/v1`
  ([`api/openapi.json`](https://github.com/planchon/rewind.codes/blob/main/api/openapi.json)): action sets, workflows,
  runs, signals, forks, transcripts, histories, secrets, webhooks, and the end users of the harness (namespaces, their
  tokens and their quotas);
- **`verifyRewindCall`**, for the harness's own APIs and MCP servers: it checks the calls Rewind signs with the
  organization's key, and says which end user each acts for;
- **the worker**, which executes the harness actions, the harness's own functions, when workflows call them;
- **the agent tools**, ready-made tools for the model (`run_workflow`, `get_run`, `update_workflow`…), with adapters
  for the Anthropic and OpenAI tool formats.

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

const actionSet = await rewind.actionSets.create({
  mcp: [{ name: "github", url: "https://mcp.example.com/github" }],
  harness: [
    {
      name: "ask_user",
      description: "Ask the end user a question and return their answer.",
      inputSchema: { type: "object", properties: { question: { type: "string" } }, required: ["question"] },
      policy: { idempotent: false },
    },
  ],
});
// actionSet.declarations.prompt or .dts → the model's system prompt

// Every run acts for one end user of the harness: its namespace (below).
const run = await rewind.namespace(user.id).runs.start({
  actionSet: actionSet.id,
  code,
  input: {},
  wait: "30s",
  idempotencyKey: toolCall.id, // a retried request returns the same run
  labels: { agent: "triage", conversation: conversation.id },
  traceparent, // the LLM call that wrote the code
  ttl: "7d",
});
// run.workflow: { id: "wf_7k2m9q4x8d1c", name: null, version: 1, digest }: code new to the namespace is a new workflow
```

A **workflow** is what the model writes and Rewind runs: its code, one TypeScript module, and its versions, numbered
from 1. Every code a namespace runs is already a workflow of that namespace, with an id (`wf_…`); the same code run
again is a run of the same workflow. Create one yourself to give it a name, a description, an interface (JSON Schemas
for its input, its output and the signals it accepts) and a suite from its first version. Without a namespace it is
the organization's, which every namespace's runs may execute; an action set that names it lets code
`import { triage } from "rewind:workflows"`, start runs of it, signal them and wait for their output.

```ts
const triage = await rewind.workflows.create({
  name: "triage",
  description: "Triage the stale issues of a repository, after approval.",
  actionSet: actionSet.id,
  code: triageCode,
  input: { type: "object", properties: { repo: { type: "string" } }, required: ["repo"] },
  output: { type: "object", properties: { closed: { type: "integer" } }, required: ["closed"] },
  signals: { approval: { type: "object", properties: { close: { type: "boolean" } }, required: ["close"] } },
});
// triage.id: "wf_…"; triage.version: its version 1 (`version`, `digest`, `declarations`, `tests`…)

// An action set whose code starts it: derived from `actionSet`, without listing its servers again.
const withTriage = await rewind.actionSets.create({ base: actionSet.id, workflows: ["triage"] });

// The harness can start a run of it too, for an end user, by its id or its name (its latest version), or one version.
const triageRun = await rewind.namespace(user.id).runs.start({ workflow: "triage@v1", input: { repo: "acme/api" } });
// triageRun.workflow: { id, name, version, digest }; a run code started has `startedBy: { run, handle, seq, detached }`

await rewind.workflows.update(triage.id, { name: "repo_triage", description: null }); // same id, no new version
```

A workflow carries a **suite**: the tests every new version must pass. A test gives the world a run meets (its input,
the answers of the actions it may call, the signals sent to it) and what the outside must see at the end; it never names
its code. A new version (new code, an interface, another action set or other secrets) runs the suite, minus those it
`retire`s, with those it `replace`s replaced, plus its new `tests`, and is stored only if every test passes
(otherwise `tests-failed`, whose `error.tests` holds each outcome). Tests can also be recorded from past runs, one per
path, and referenced by digest.

```ts
const recorded = await rewind.workflows.record("triage", { query: "status:completed", limit: 20 });

// Check a candidate first: the given tests, plus the suite of triage's latest version, against the new code.
const report = await rewind.namespace(user.id).workflows.test({ code: fixedCode, workflow: "triage" });

const next = await rewind.workflows.createVersion("triage", {
  code: fixedCode,
  tests: [
    ...recorded.tests.map((test) => test.digest),
    {
      name: "closes nothing without approval",
      input: { repo: "acme/api" },
      world: { actions: { "github.list_issues": { returns: [{ number: 4 }] } } },
      signals: [{ name: "approval", payload: { close: false } }],
      expect: { status: "completed", output: { closed: 0 }, calls: [{ action: "github.close_issue", times: 0 }] },
    },
  ],
  retire: [{ names: ["asks before closing"], reason: "under 5 stale issues, it closes without asking" }],
});
// next.version.version: 2. Tests added alone make no version: workflows.addTests("triage", { tests, replace, retire })
```

A workflow belongs to the namespace whose run created it, or to the organization. `workflows.promote(id)` moves a
namespace's workflow to the organization; `rewind.namespace(name).workflows.fork("triage")` forks an organization's
workflow into a namespace, which keeps its name, so that namespace's runs get the fork; `workflows.pull(id)` makes the
source's latest version the fork's next one, once the fork's own suite passes. Each needs `workflows:write`, which an end
user's token never holds.

| Resource                     | Calls                                                                                                                                                                                                                                                                                                                                                                                                                              |
| ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `rewind.actionSets`          | `create` (`mcp`, `apis` from OpenAPI documents, `harness`, `http`; `workflows` pins the organization's workflows, `base` derives from an action set), `declarations`, `search`                                                                                                                                                                                                                                                     |
| `rewind.workflows`           | `check` (type-checks code), `test` (tests in virtual time, and a workflow's suite), `create`, `list` and `listAll` (the latest version of each, with the counts of its runs; `code`: the workflows one of whose versions has it), `get` (by id or name), `update` (name, description), `createVersion` and `addTests` (through the gate: `tests`, `replace`, `retire`), `promote`, `fork`, `pull`, `record` (tests from past runs) |
| `rewind.tests`               | `get`: a stored test, by digest                                                                                                                                                                                                                                                                                                                                                                                                    |
| `rewind.runs`                | `start` (`code`, or a `workflow`), `get`, `list` and `listAll` (`q`: the query language, `sort`, `status`, `workflow`, `startedBy`, `source`, `action`), `stats`, `failures`, `signal`, `cancel`, `fork`, `transcript`, `event`, `events`, `export` (`as: "cassette"`), `release`, `giveUp`, `delete`; `start` and `fork` take `secrets`                                                                                           |
| `rewind.calls`               | `stream` (the call stream), `defer`, `complete`, `fail`, `approve`, `reject`                                                                                                                                                                                                                                                                                                                                                       |
| `rewind.approvals`           | `pending(run)`, `args`, `approve`, `reject`                                                                                                                                                                                                                                                                                                                                                                                        |
| `rewind.payloads`            | `get(digest, { run })`: a value left out above 256 KiB, read through its run                                                                                                                                                                                                                                                                                                                                                       |
| `rewind.namespaces`          | `list` and `listAll` (`prefix`, `above`, `order`, `month`), `get`, `put` (exceptions), `rename`, `delete`, `alerts`                                                                                                                                                                                                                                                                                                                |
| `rewind.namespaces.secrets`  | `list`, `put`, `delete`, `usage`: an end user's own secrets, used as `ns/<name>`                                                                                                                                                                                                                                                                                                                                                   |
| `rewind.namespaces.defaults` | `get`, `put`: the default limits of every namespace, and the ceiling                                                                                                                                                                                                                                                                                                                                                               |
| `rewind.namespaces.tokens`   | `create` (its answer holds the token, once), `revoke`, `revokeAll`                                                                                                                                                                                                                                                                                                                                                                 |
| `rewind.tokenKeys`           | `create`, `list`, `delete`: the keys the harness signs end users' tokens with                                                                                                                                                                                                                                                                                                                                                      |
| `rewind.secrets`             | `list`, `put`, `delete`, `usage`: the organization's secrets, used as `org/<name>`                                                                                                                                                                                                                                                                                                                                                 |
| `rewind.signingKeys`         | `list` (with the `issuer` and the `jwksUrl`), `rotate`, `jwks`: the keys Rewind signs its calls with                                                                                                                                                                                                                                                                                                                               |
| `rewind.subscriptions`       | `create` (`types`; its answer holds the secret, once), `list`, `delete`                                                                                                                                                                                                                                                                                                                                                            |
| `rewind.webhooks`            | `verify`                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `rewind.actions`             | `stats`: calls, latency, errors and redeliveries per action (`group`, `action`, `actionSet`)                                                                                                                                                                                                                                                                                                                                       |
| `rewind.apiKeys`             | `create`, `revoke`                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `rewind.server`              | `info` (the version and epochs, the node that answered, the partitions and nodes), `stop`                                                                                                                                                                                                                                                                                                                                          |

Every call takes an `AbortSignal` (`signal`). The types are those of the API, generated from the OpenAPI document and
exported under their names (`Run`, `Workflow`, `ActionSet`, `HandedCall`, `Webhook`…).

**Statuses.** A run is `queued` until its first step is committed, `running` while a step is in progress or Rewind is
making one of its calls, and `waiting` when nothing Rewind does can move it until something arrives: `run.waitingFor`
says what (a signal, a timer, a call with its `state`, a quarantine). These three are active; a run then ends
`completed`, `failed`, `cancelled` or `aborted` (`ENDED_STATUSES`, `isEnded(status)`).

**Errors.** The API answers errors as `application/problem+json`. The client throws them as `RewindApiError`, with
`status`, `slug` (`"run-ended"`, `"invalid-code"`, `"invalid-workflow"`, `"name-taken"`, `"tests-failed"`,
`"invalid-signal"`…), `detail` (written for a model), `diagnostics` (for refused code), `tests` (for `tests-failed`:
each test's outcome), `secrets` (for `secret-required`: each secret at fault, why, and the secrets the caller may use
instead, `usable`), `query` (for a query that cannot be read: where the term at fault is) and `quota` (for
`quota-exceeded`: which quota, at which scope, its limit, the use and when it frees). `isProblem(error, "call-gone")`
tests the kind; the slugs also include `invalid-action-set`, `not-forkable`, `not-a-fork`, `call-not-waiting` and
`action-set-unavailable`. A key's client that starts a run, checks or tests code, or forks a workflow without naming a
namespace gets the server's 400 as a `NamespaceRequiredError`. A server that cannot be reached gives a
`RewindConnectionError`.

**Retries.** A request that is safe to repeat is sent again after a network error or a 503, twice by default
(`maxRetries`): reads, `PUT`, `DELETE`, action set registration, workflow creation and its other writes that find what
they already made, checks and tests, and the requests that carry an idempotency key. Starting a run, forking it or
sending a signal without a key is never repeated, nor is minting a token, adding tests or forking a workflow.

**Waiting.** `runs.start`, `runs.fork`, `runs.cancel` and `runs.get` take `wait`: one request, which the server holds
until the run ends or is quarantined, or until `wait` passes, however long it is. Nothing is asked again once the server
has answered. A connection cut while the server holds it (a proxy, or the HTTP client's own timeout: Node's `fetch`
waits 5 minutes for an answer) is sent again with the time left; a start or a fork that waits carries an idempotency
key for that, the caller's or one the client draws.

**Pages and statistics.** A page of `runs.list` may hold fewer runs than `limit`, or none, and still have a
`nextCursor`: the server stopped at its scan budget; `listAll` follows the cursor to the last page. What a waiting run
waits for is a term of `q` (`waiting:approval`), as are the active runs (`status:active`). `runs.stats`,
`runs.failures` (by fingerprint), the counts of `workflows.list` and `actions.stats` answer from hourly counters: a
scope (`q: "namespace:<name>"`, the client's own namespace, or every namespace it reaches) and a window of whole hours,
stepped by `interval`; another term of `q` is refused with `invalid-query`, and a key or a token limited to labels with
`forbidden`. `actions.stats` takes `group`, an MCP server, an API or a set of harness actions (`github`), and `action`.

**The event stream.** `runs.events(id, { follow: true })` is an async iterator over the events of the run's history as
they are committed (`{ type: "event", idx, event }`), ending with `{ type: "end" }`. A dropped connection resumes after
the last event received. `runs.event(id, idx)` reads one event in full, `runs.transcript(id)` the run as text for a
model, and `runs.export(id)` the run export, which replays on its own.

## End users: namespaces and tokens

A harness serves its own users, the **end users** of its product. Each one has a **namespace**: their runs, their
workflows, their secrets and their use live there, and nothing of one end user is visible to another. A namespace's
name is the harness's own id for the end user: 1 to 128 ASCII letters, digits, `-`, `_`, `.`, `:` and `@` (an opaque
id is better than an email: it appears in webhooks, the dashboard and the service's logs). It is created at its first
use.

**The harness's backend** holds its API key, which reaches every namespace, and names the end user of each request with
`rewind.namespace(name)`: a client whose requests carry the header `Rewind-Namespace`. A key needs it to start a run,
check or test code, or fork a workflow (there is no default namespace); on a read, it narrows the answer to that
namespace, a workflow's name resolves in it first, and a run of another end user is missing (404). Without it, a read
covers every namespace.

```ts
const asAlice = rewind.namespace(user.id); // cheap: one per request is fine
const run = await asAlice.runs.start({ actionSet: actionSet.id, code, wait: "30s" });
const runs = await asAlice.runs.list({ status: "failed" }); // Alice's runs only
await rewind.runs.start({ code }); // NamespaceRequiredError: the server's 400
```

**The agent acting for one end user** holds a token of their namespace. With it the agent, and every workflow its model
writes, reaches that namespace only. The backend mints an opaque token (`rewind_nt_…`; 1 hour by default, 30 days at
most), or signs a JWT itself with a key it registered.

```ts
// In the harness's backend, with its key:
const { token } = await rewind.namespaces.tokens.create(user.id, {
  ttl: "1h",
  permissions: ["runs:read", "runs:start", "runs:control", "actions:read"], // the default
  labels: { conversation: conversation.id }, // optional: the runs it reaches carry them
});

// In the agent acting for that user:
const asUser = new Rewind({ url: process.env.REWIND_URL, token });
const agentTools = asUser.agentTools({ actionSet: actionSet.id });

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
only), active runs, runs per month, distinct workflows, the longest TTL and the longest code. The harness sets the
default limits of every namespace and the exceptions of one (a paying end user, a blocked one), under its ceiling.

```ts
await rewind.namespaces.defaults.put({ defaults: { activeRuns: 5, runsPerMonth: 1_000, maxTtl: "7d" } });
await rewind.namespaces.put("user_2fQ81", { exceptions: { activeRuns: 0 } }); // blocked

const nearLimits = await rewind.namespaces.list({ above: 0.8, order: "runs" }); // at 80 % of a limit or more
const alice = await rewind.namespaces.get(user.id); // limits, use per day, alerts, tokens in use
const { alerts } = await rewind.namespaces.alerts({ month: "2026-11" });

try {
  await asAlice.runs.start({ code });
} catch (error) {
  if (isProblem(error, "quota-exceeded")) console.log(error.quota); // { quota: "runs_per_month", scope, limit, used, resetsAt }
}
```

- An end user's own secrets (their GitHub token): `rewind.namespaces.secrets.put(user.id, "github", { value })`, which
  their runs use as `ns/github` ([secrets](#secrets-and-signed-calls)).
- `rewind.namespaces.delete(user.id)` erases the end user in the background: their tokens, runs, workflows, secrets and
  use.
- Action sets and webhook subscriptions stay the harness's: register the actions once, receive every end user's
  webhooks, each naming its `namespace`.

## Secrets and signed calls

Each MCP server and API of an action set says in its `auth` how its calls prove who calls: nothing, a **secret** the
run uses, or a **signature** of the organization's key. An action set holds no secret.

```ts
const actionSet = await rewind.actionSets.create({
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

// A run reads only the secrets it uses, server by server, named in the request, never in the code.
const run = await rewind.namespace(user.id).runs.start({
  actionSet: actionSet.id,
  code,
  secrets: { github: "ns/github" },
});
```

- `org/<name>` is the organization's secret, `ns/<name>` the run's namespace's. Using an organization's secret needs
  `secrets:use`, and a key limited to labels sees, writes and uses only the secrets carrying them; a namespace's secret
  is used by whoever starts a run in it.
- A workflow version carries its secrets (`workflows.create({ …, secrets })`, `workflows.createVersion`), checked
  against whoever makes it; a run of it gives none. A fork keeps its source's unless it gives its own
  (`runs.fork(id, { code, secrets })`).
- A start that misses a secret, names one that does not exist, or one the caller may not use is refused before any run
  exists: `secret-required`, whose `error.secrets` say which server and what the caller may use (`usable`).
- `secrets.usage(name)` says what deleting a secret would stop: the active runs that use it, those waiting on a call it
  serves, the workflows whose latest version uses it. A call whose secret was deleted or refused is parked (webhook
  `secret.expired`); putting the secret again sends it again, with the same idempotency key.

**Checking a signed call.** With `auth: { type: "rewind" }`, every request carries a JWT signed with the organization's
Ed25519 key. `verifyRewindCall` does every check the receiving server must do: the signature against
`{issuer}/jwks.json` (cached, an unknown `kid` fetched again at most every 10 s), the issuer and the audience, `exp` and
`iat` with 5 s of leeway, the method, the URL and the SHA-256 of the raw body, and the run's labels when given. It
takes a Fetch `Request`, or `{ method, url, headers, body }` with the raw body for Express or NestJS:

```ts
import express from "express";
import { RewindSignatureError, verifyRewindCall } from "@rewind.codes/sdk";

const { issuer } = await rewind.signingKeys.list(); // https://api.rewind.codes/organizations/org_…, or rewindIssuer(url, organizationId)

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
  // call.namespace is the end user, call.action the action called; call.idempotencyKey is the same for every delivery.
  res.json(await createMeasure(call.namespace, JSON.parse(req.body.toString()), call.idempotencyKey));
});
```

It answers `{ namespace, labels, run, action, idempotencyKey }`: the JWT's claim `action` (Rewind also sends it as
`tool`, for verifiers of SDK 0.3 to 0.5). One key signs every run of the organization: an organization whose
development and production share it gives `labels: { env: "prod" }`, so a production server refuses the calls of a
development run. A `RewindSignatureError` names the check that failed (`check`: `signature`, `kid`, `iss`, `aud`, `exp`,
`iat`, `htm`, `htu`, `body`, `labels`…). An MCP server passes `session: true`: the requests of the session outside a
call (`initialize`, the listing of the tools) are answered `null`. `signingKeys.rotate()` replaces the key; the old one
stays published 24 hours.

## Harness actions and the worker

A harness action is a function of the harness that workflows call like any action. The worker holds a call stream open
(`GET /v1/calls/stream`); the server hands it each call as it becomes ready (a `call` message, with its token), the
worker runs the handler and answers with the result. Nothing is sent while a handler runs: the open stream is the
liveness of every call handed on it, and a `gone` message says a call no longer waits for its answer.

```ts
import { HarnessActionError } from "@rewind.codes/sdk";

const worker = rewind.worker({
  actionSet: actionSet.id,
  concurrency: 10,
  actions: {
    "crm.find_customer": async ({ email }, ctx) => {
      // ctx.namespace is the end user the run acts for: act on their data only.
      // ctx.idempotencyKey is the same on every delivery: use it to recognize a call already made.
      const customer = await crm.find(ctx.namespace, email, { signal: ctx.signal });
      if (!customer) throw new HarnessActionError(`no customer ${email}`); // the code gets an ActionError
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
await rewind.calls.complete(token, { answer: "yes" });

await worker.stop(); // no new call; waits for the handlers in flight to answer
```

- One worker with the harness's key serves every end user: each call names the namespace of its run
  (`ctx.namespace`). A worker holding an end user's token is handed only the calls of that namespace's runs.
- The context (`HarnessActionContext`) has the call as handed (`ctx.call`), its id (`ctx.callId`), `runId`, `seq`,
  `delivery` (from 1; above 1, an earlier delivery was interrupted and the call may already have happened),
  `idempotencyKey`, `labels`, `traceparent`, `signal` and `defer()`.
- `concurrency` (10 by default, 100 at most) is the number of slots of the stream: the server hands at most that many
  calls at once. A deferred call frees its slot.
- After a drop, the worker opens its stream again with the same id (`worker.id`), after a backoff (500 ms, doubling up
  to 30 s, starting over once a stream lived); the server keeps the calls handed to it for the action's
  `reconnectTimeout` (60 s by default), and the handlers in flight go on. After it, a call is delivered again to
  another worker under `idempotent: true`, and fails with `outcomeUnknown` under `idempotent: false`. A stream silent
  for 45 s (the server writes a comment at least every 15 s) is taken as broken. `stop()` keeps the stream open with no
  slot until the handlers in flight have answered.
- A thrown `HarnessActionError` is the action's failure: the code gets an `ActionError` with its `message` and `data`.
  With `retryable: true`, the call is delivered again later under `idempotent: true`. Any other exception fails the
  call too, with `Name: message`, and is reported to `onError`.
- `ctx.signal` aborts when the call no longer waits for its answer: the stream says it is gone (its run ended, the call
  timed out), or the server refused an answer as `call-gone`. The result is then dropped, as the server would drop it.
- After `ctx.defer()` (a `DeferredCall`, with its `token` and `callId`), the handler's return value is not an answer:
  whoever holds the token answers. If the handler throws after deferring, the worker fails the call.
- A refused key (401, 403), an invalid request or an unknown action set (400, 404) stops the worker: `worker.done`
  rejects. Other failures are reported to `onError` (`console.error` by default), and the stream is opened again.

## Approvals

An action with the `approval` policy waits for the harness before each call is sent, and so does a call parked because
its action changed during the run. The run is then `waiting`; its state says which call waits (`Run.waitingFor`, an
item of kind `call` with its `state`, also in the `run.waiting` webhook).

```ts
for (const approval of rewind.approvals.pending(run)) {
  const args = await rewind.approvals.args(approval); // from the run's state, or by its digest when large
  if (await askAHuman(approval.action, args)) await rewind.approvals.approve(approval);
  else await rewind.approvals.reject(approval, "the user refused"); // the code gets an ActionError
}
```

A `PendingApproval` has the run (`runId`, `runHandle`), the call (`callId`, `action`, `seq`, `delivery`, `callsite`),
its arguments (`payload`) and its `reason`: `approval`, or `schema_changed`. `rewind.calls.approve(callId)` and
`reject` do the same by the call's id.

## The agent tools

```ts
// The model's runs belong to the end user: a client bound to their namespace, or holding their token.
const agentTools = rewind.namespace(user.id).agentTools({ actionSet: actionSet.id, labels: { conversation: convId } });

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

| Tool                                                                                                                        | Does                                                                                                                                                      |
| --------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `run_workflow({ code?, workflow?, input?, wait?, secrets? })`                                                               | Runs code, type-checked first, or a workflow by its id or name; names the workflow and its version, with the output, the error, or what the run waits for |
| `check_workflow({ code })`                                                                                                  | Type-check only; also the workflow and version that already have this code. Creates nothing                                                               |
| `test_workflow({ code?, workflow?, tests? })`                                                                               | Runs tests as test runs, in virtual time, and the suite of `workflow`; each test's outcome. Creates and changes nothing                                   |
| `get_run({ run_id, event?, offset?, wait? })`                                                                               | The run's state and transcript; one event of its history in full; the next page of a long output                                                          |
| `list_runs({ status? })`                                                                                                    | The runs started with the same labels                                                                                                                     |
| `signal_run({ run_id, name, payload? })`                                                                                    | Sends a signal                                                                                                                                            |
| `cancel_run({ run_id })`                                                                                                    | Cancels a run                                                                                                                                             |
| `retry_run({ run_id, code, secrets? })`                                                                                     | Forks the run with corrected code: the calls already made are not made again; a workflow's suite must pass first                                          |
| `search_actions({ query })`                                                                                                 | The declarations of the matching actions (only with an action set)                                                                                        |
| `list_secrets({})`                                                                                                          | The names of the secrets the model's runs may use, never a value                                                                                          |
| `update_workflow({ workflow, name?, description?, code?, input?, output?, signals?, secrets?, tests?, replace?, retire? })` | Names or describes a workflow, makes its next version once its suite passes, or adds tests to its suite                                                   |
| `get_workflow({ workflow })`                                                                                                | A workflow's code, schemas, versions, suite and source, with the reasons its tests were replaced or retired                                               |
| `record_tests({ workflow, query?, runs?, limit? })`                                                                         | Records tests from past runs, one per path, and returns their digests for `update_workflow`                                                               |

- Results are plain text written for the model, and bounded: `run_workflow` returns at most 256 KiB of output, with the
  call that reads the rest (`get_run` with `offset`); a failure shows the error with its line and code frame, the call
  that failed, and the transcript up to the failure. Messages, diagnostics and transcripts are cut too, whatever their
  size.
- The model's tool call id makes the idempotency key of `run_workflow`, `retry_run` and `signal_run`: handling the same
  tool call twice starts one run, or delivers one signal. A run's key is the SHA-256 of the agent's `labels` and the id
  (`runIdempotencyKey`), so agents with other labels never share one, however their harness numbers its tool calls. A
  run the server answers for a key already used by another code, workflow or labels is never shown to the model, and a
  `RewindError` is thrown.
- `labels` are set on every run the model starts. They filter `list_runs`, and the tools that take a run id answer for
  a run without them as for a run that does not exist. Give the harness an API key limited to these labels so the
  server holds the scope too.
- Mistakes the model can fix (a bad argument, refused code, a run that has ended) come back as a result with
  `isError`, and so does a quota reached, in words written for the model. Faults of the platform or of the harness (an
  unreachable server, a 401, a 5xx, a key's client that names no namespace) are thrown.
- `result.run` holds the run the call started or read, for the harness: `rewind.approvals.pending(result.run)` says what
  waits for it. `result.workflow` holds the workflow `update_workflow` changed or `get_workflow` read, `result.tests`
  the outcomes of the tests that ran, and `result.code` whether the checks passed the code the call handed in
  (`passed` or `refused`).
- `update_workflow`, `get_workflow` and `record_tests` name a workflow by its id or its name; `update_workflow` patches
  its name and description first, then makes a version (new code, an interface or secrets) or changes its suite. It
  and `retry_run` (of a run of a workflow) run the suite first. When a test does not pass, the model reads each test's
  problem (`stale`, `incompatible` or `failed`), its failures and the transcript of its run, and fixes the code, or
  retires or replaces the test with a reason. No agent tool promotes, forks or pulls a workflow.
- Options: `actionSet`, `labels`, `tools` (a subset), `wait` (30 s), `maxWait` (2 min, the longest wait the model may
  ask for), `ttl`, `traceparent` (a string, or a function giving the current one), `outputLimit`.

## Webhooks

```ts
import { verifyWebhook, WebhookVerificationFailed } from "@rewind.codes/sdk";

await rewind.subscriptions.create({
  url: "https://harness.example/rewind-webhooks",
  types: ["run.completed", "run.waiting"],
});

app.post("/rewind-webhooks", express.raw({ type: "application/json" }), (req, res) => {
  let webhook;
  try {
    webhook = verifyWebhook(req.body, req.headers, process.env.REWIND_WEBHOOK_SECRET!);
  } catch (error) {
    if (error instanceof WebhookVerificationFailed) return res.sendStatus(400);
    throw error;
  }
  // Deduplicate on the webhook-id header: a retried delivery keeps it.
  if (webhook.type === "run.completed") resumeConversation(webhook.data.run); // webhook.data.run.namespace: the end user
  if (webhook.type === "quota.alert") warnCustomer(webhook.data); // namespace (null: the ceiling), quota, threshold, used, limit
  res.sendStatus(204);
});
```

A webhook is `{ type, timestamp, data }` (`Webhook`), of a type a subscription lists: `run.completed`, `run.failed`,
`run.cancelled`, `run.aborted`, `run.waiting`, `run.quarantined`, `action.schema_changed`, `secret.expired` or
`quota.alert`. Deliveries follow Standard Webhooks: an HMAC-SHA256 signature over `{id}.{timestamp}.{body}` with the
subscription's secret `whsec_…`, and 5 minutes of tolerance on the timestamp. Pass the body exactly as received.

## Versions

The package follows [Semantic Versioning](https://semver.org). Before 1.0, a minor version (0.5 → 0.6) may break the
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
