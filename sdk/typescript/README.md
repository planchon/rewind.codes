# @ai-ptc/sdk

The TypeScript SDK of ai-ptc, for agent harness developers. It has three parts:

- **the client** of the HTTP API `/v1` ([`api/openapi.json`](../../api/openapi.json)): toolsets, runs, signals,
  forks, traces, journals, credentials, webhooks;
- **the host-tool worker**, which executes the harness's own tools when scripts call them;
- **the agent tools**, ready-made tools for the model (`run_script`, `get_run`…), with adapters for the Anthropic
  and OpenAI tool formats.

Node 22.12 or later. ESM only. The package depends on no model SDK.

## The client

```ts
import { Ptc } from "@ai-ptc/sdk";

const ptc = new Ptc({ url: process.env.PTC_URL, apiKey: process.env.PTC_API_KEY }); // the defaults

const toolset = await ptc.toolsets.create({
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

const run = await ptc.runs.start({
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

| Namespace           | Calls                                                                                                                                                                    |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `ptc.toolsets`      | `create`, `declarations`, `search`, `setCredentials`                                                                                                                     |
| `ptc.scripts`       | `check`, `test` (mock mode, virtual time), `list`, `listAll`, `get`                                                                                                      |
| `ptc.runs`          | `start`, `get`, `list`, `listAll`, `signal`, `cancel`, `fork`, `trace`, `entry`, `events`, `journal` (`as: "cassette"`), `release`, `giveUp`, `setCredentials`, `delete` |
| `ptc.tasks`         | `claim`, `heartbeat`, `defer`, `complete`, `fail`, `approve`, `reject`                                                                                                   |
| `ptc.approvals`     | `pending(run)`, `args`, `approve`, `reject`                                                                                                                              |
| `ptc.payloads`      | `get`: a value left out above 256 KiB                                                                                                                                    |
| `ptc.subscriptions` | `create` (its answer holds the secret, once), `list`, `delete`                                                                                                           |
| `ptc.webhooks`      | `verify`                                                                                                                                                                 |
| `ptc.tools`         | `stats`                                                                                                                                                                  |
| `ptc.apiKeys`       | `create`, `revoke`                                                                                                                                                       |
| `ptc.server`        | `info`, `stop`                                                                                                                                                           |

Every call takes an `AbortSignal` (`signal`). The types are those of the API, generated from the OpenAPI document and
exported under their names (`Run`, `Toolset`, `Task`, `WebhookEvent`…).

**Errors.** The API answers errors as `application/problem+json`. The client throws them as `PtcApiError`, with
`status`, `slug` (`"run-ended"`, `"invalid-script"`…), `detail` (written for a model) and `diagnostics` (for a refused
script). `isProblem(error, "task-gone")` tests the kind. A server that cannot be reached gives a `PtcConnectionError`.

**Retries.** A request that is safe to repeat is sent again after a network error or a 503, twice by default
(`maxRetries`): reads, `PUT`, `DELETE`, the content-addressed toolset registration, checks and tests, and the requests
that carry an idempotency key. Starting a run, forking it or sending a signal without a key is never repeated.

**Waiting.** `runs.start`, `runs.fork`, `runs.cancel` and `runs.get` take `wait`: they answer when the run ends or
`wait` passes. A wait longer than 50 seconds is split into several requests, so proxies and HTTP timeouts never cut it.

**The event stream.** `runs.events(id, { follow: true })` is an async iterator over the run's journal entries as they
are committed, ending with `{ type: "end" }`. A dropped connection resumes after the last entry received.

## The host-tool worker

A script calls a host tool like any tool. The server queues a task; the worker claims it (long-poll), runs the handler,
keeps the claim alive with heartbeats while it runs, and answers with the result.

```ts
import { HostToolError } from "@ai-ptc/sdk";

const worker = ptc.worker({
  toolset: toolset.id,
  concurrency: 10,
  tools: {
    lookup_customer: async ({ email }, ctx) => {
      // ctx.idempotencyKey is the same on every attempt: use it to recognize a call already made.
      const customer = await crm.find(email, { signal: ctx.signal });
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
await ptc.tasks.complete(token, { answer: "yes" });

await worker.stop(); // lets the claim in flight finish, then waits for the handlers
```

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
for (const approval of ptc.approvals.pending(run)) {
  const args = await ptc.approvals.args(approval); // from the run's state, or by its digest when large
  if (await askAHuman(approval.tool, args)) await ptc.approvals.approve(approval);
  else await ptc.approvals.reject(approval, "the user refused");
}
```

## The agent tools

```ts
const agentTools = ptc.agentTools({ toolset: toolset.id, labels: { conversation: convId } });

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

| Tool                                          | Does                                                                                 |
| --------------------------------------------- | ------------------------------------------------------------------------------------ |
| `run_script({ code, input?, wait? })`         | Type-checks and starts a run; returns its output, its error, or what it waits for    |
| `check_script({ code })`                      | Type-check only                                                                      |
| `test_script({ code, scenario })`             | Mock mode, virtual time; `scenario.from_run` replays the calls a run recorded        |
| `get_run({ run_id, entry?, offset?, wait? })` | The run's state and trace; one journal entry in full; the next page of a long output |
| `list_runs({ status? })`                      | The runs started with the same labels                                                |
| `signal_run({ run_id, name, payload? })`      | Sends a signal                                                                       |
| `cancel_run({ run_id })`                      | Cancels a run                                                                        |
| `retry_run({ run_id, code })`                 | Forks the run with a corrected script: the calls already made are not made again     |
| `search_tools({ query })`                     | The declarations of the matching tools (only with a toolset)                         |

- Results are plain text written for the model, and bounded: `run_script` returns at most 256 KiB of output, with the
  call that reads the rest (`get_run` with `offset`); a failure shows the error with its line and code frame, the call
  that failed, and the trace up to the failure. Messages, diagnostics and traces are cut too, whatever their size.
- The model's tool call id is the idempotency key of `run_script`, `retry_run` and `signal_run`: handling the same tool
  call twice starts one run, or delivers one signal. Ids must be unique across conversations: a run the server answers
  for an id already used by another script or other labels is never shown to the model, and a `PtcError` is thrown.
- `labels` are set on every run the model starts. They filter `list_runs`, and the tools that take a run id answer for
  a run without them as for a run that does not exist. Give the harness an API key limited to these labels so the
  server holds the scope too.
- Mistakes the model can fix (a bad argument, a refused script, a run that has ended) come back as a result with
  `isError`. Faults of the platform (an unreachable server, a 401, a 5xx) are thrown.
- `result.run` holds the run the call started or read, for the harness: `ptc.approvals.pending(result.run)` says what
  waits for it.
- Options: `tools` (a subset), `wait` (30 s), `maxWait` (2 min, the longest wait the model may ask for), `ttl`,
  `traceparent` (a string, or a function giving the current one), `outputLimit`.

## Webhooks

```ts
import { verifyWebhook, WebhookVerificationFailed } from "@ai-ptc/sdk";

app.post("/ptc-events", express.raw({ type: "application/json" }), (req, res) => {
  let event;
  try {
    event = verifyWebhook(req.body, req.headers, process.env.PTC_WEBHOOK_SECRET!);
  } catch (error) {
    if (error instanceof WebhookVerificationFailed) return res.sendStatus(400);
    throw error;
  }
  // Deduplicate on the webhook-id header: a retried delivery keeps it.
  if (event.type === "run.completed") resumeConversation(event.data.run);
  res.sendStatus(204);
});
```

Deliveries follow Standard Webhooks: an HMAC-SHA256 signature over `{id}.{timestamp}.{body}` with the subscription's
secret `whsec_…`, and 5 minutes of tolerance on the timestamp. Pass the body exactly as received.

## Development

```sh
pnpm install
pnpm check          # prettier, tsc (TypeScript 7), vitest
pnpm build          # dist/
pnpm generate:api   # src/generated/api.ts from api/openapi.json; commit the diff
```

The tests run against a mocked HTTP server (`test/mock-server.ts`): a real server on a free local port, with handlers
per route.
