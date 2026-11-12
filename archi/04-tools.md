# 4. Tools

## Routes

A toolset has three kinds of tools:

| Route | Called by | Used for |
| --- | --- | --- |
| MCP tools | The dispatcher of ai-ptc, an MCP client (`rmcp`, stdio or streamable HTTP) | Every tool of the toolset's MCP servers |
| Built-ins: `http`, `webhook` | The dispatcher of ai-ptc | `http` is the only way a script reaches the network ([egress](#the-http-tool-and-egress)); `webhook(name)` returns a URL bound to the run and the signal name ([signals](01-product.md#signals)) |
| Host tools | The harness's SDK worker, which long-polls `GET /v1/tasks` | Functions that live in the harness: ask the end user, call another LLM, start a sub-agent |

ai-ptc calls MCP tools and built-ins directly. Their results go back to the script, never to
the agent. The harness is on the path only for its host tools, and they are optional: a
toolset may have only MCP servers. There is no other kind of tool in 0.1.

Every call carries an **idempotency key** `"{run_id}/{seq}"`, passed in MCP `_meta`
(`io.ai-ptc/idempotency-key`) and to host tool handlers. Calls also carry the W3C trace
context (`traceparent`, `tracestate`) in `_meta`, with the tool call span as parent, so
the MCP server's own spans join the run's trace.

This key protects the way out. The way in (starting a run, sending a signal) has its
own keys: see [storage](06-storage-and-scale.md#idempotency-of-the-api). A fork's calls
carry other keys: see [forks](03-execution.md#forks).

A host tool can last days. The most complete case, a host tool that outlasts the run's
`wait`:

1. The agent starts the run with `POST /v1/runs` and `wait=30s`. The script calls `ask_user`.
2. The harness's worker long-polls `GET /v1/tasks?tools=ask_user` and gets the task, a token
   and the idempotency key.
3. While it works, it sends `POST /v1/tasks/{token}/heartbeat`.
4. After 30 s, the agent gets `suspended`, waiting for `ask_user`.
5. When it has the answer, even days later, the worker sends
   `POST /v1/tasks/{token}/complete`. The completion goes to the run's inbox, the run resumes
   and ends, and the harness gets the signed webhook `run.completed`.

## Tool task lifecycle

Each `schedule_tool_call` creates a **tool task**, executed outside the isolate. Attempts,
backoff, leases, approvals and tokens live in the tool task, not in the journal. The journal
only gets the final outcome.

One state machine covers every route:

| State | The task | Leaves it |
| --- | --- | --- |
| `approval` | Waits for the harness. Only with the `approval` policy | approve → `ready`; reject → `done`, the call fails with a `ToolError` |
| `ready` | Can be claimed once `next_attempt_at ≤ now` and a concurrency slot is free | claim → `claimed` |
| `claimed` | Attempt n, under a lease kept alive by heartbeats | See the next table |
| `deferred` | Holds a token until the handler answers | `/complete` or `/fail` → `done`; a timeout, bounded by the run's TTL → `done` |
| `parked` | Waits for the harness after a schema change or a 401 | continue, or credentials renewed → `ready`; cancel → `done` |
| `done` | Its completion is in the run's inbox: a result, a failure, or `outcomeUnknown` | |

From `claimed`:

| Event | Next state |
| --- | --- |
| The tool answered | `done`, with its result or its failure |
| Transient failure | `ready` with backoff (policy `retry`); `done` with the failure (`at_most_once`) |
| The claim expired: the dispatcher or the worker died mid-call | `ready` with attempt + 1, up to 5 attempts, then `done` with `outcomeUnknown` (policy `retry`); `done` with `outcomeUnknown` at once (`at_most_once`) |
| The handler answers later | `deferred` |
| The schema changed at dispatch | `parked`, and the harness gets the webhook `tool.schema_changed` ([toolset change](#when-a-toolset-changes-during-a-run)) |
| The server answered 401 | `parked`, and the harness gets the webhook `credentials.expired` ([credentials](#credentials)) |

The harness answers a task in `approval`, and a task `parked` by a schema change, with
`POST /v1/tasks/{id}/approve` (→ `ready`) or `POST /v1/tasks/{id}/reject` (→ `done`, the call
fails with a `ToolError`). A run with a task in `approval` or `parked` is suspended, and its
state says which call waits.

When a run ends, its tasks are deleted: a call still queued is never sent. Results arriving
for a run that already ended are dropped.

The backoff curve is an implementation choice, recorded in 08.

## Policies

Each tool has a policy, pinned in the toolset. The harness sets it per tool when it
registers the toolset.

| Setting | Values | Default |
| --- | --- | --- |
| After an interruption | `retry` or `at_most_once` | `retry` |
| `approval` | The task waits for the harness before it is sent | Off |
| Execution timeout | A duration | 5 min for MCP tools |
| Heartbeat timeout | A duration | 60 s for host tools |
| Concurrency | Calls in flight | 16 per run; 8 per MCP server on each worker |

| Policy | On transient error | Claim expired (outcome unknown) |
| --- | --- | --- |
| `retry` | Retry with backoff | Retry with the same idempotency key, up to 5 attempts, then fail with `outcomeUnknown: true` |
| `at_most_once` | Fail | Fail with `outcomeUnknown: true` |

An interrupted call is called again with the same idempotency key, so the tool can recognize
it and not repeat its effect. A tool that ignores the key may then run twice: such a tool is
declared `at_most_once` in the toolset.

The `approval` policy lets the harness keep its permission gates: a script cannot call such a
tool without the harness's consent.

Concurrency limits are invisible to the journal: a task waiting for a slot stays `ready`.

The script sees failures as a thrown `ToolError` and decides what to do.

## Toolsets and code generation

A toolset is a list of MCP servers and host tools, with a policy per tool. Registering it
(`POST /v1/toolsets`) connects to the servers, lists their tools, and **pins** the schemas.
The pinned toolset is a document of the [stored format](02-journal.md#stored-format-v1): its
servers, and for each tool its name, route, schemas and policy. The toolset is
content-addressed, and every run records the digest it was started with.

From each pinned tool, `ptc-codegen` generates three outputs. For `github.list_issues`:

| Output | Read by | Example |
| --- | --- | --- |
| The `ptc:tools` runtime module: thin wrappers over the tool op | The runtime | `list_issues: (args) => call("github.list_issues", args)` |
| `ptc-tools.d.ts`: types for args (from `inputSchema`) and results (from `outputSchema` when present, otherwise the MCP content type with `.text()` and `.json()` helpers), with tool descriptions as JSDoc | tsgo | `list_issues(args: { repo: string; state?: "open" \| "closed" }): Promise<Issue[]>` |
| A **compact prompt rendering** of the same declarations | The model | `github.list_issues({ repo, state? }) → Issue[] · List the issues` |

Codegen is deterministic. Without it, nothing could check a script before it runs: tsgo would
not know that `repo` is a string. With it, `github.list_issues({ repo: 42 })` is refused at
start, before any run exists: `Type 'number' is not assignable to type 'string'
(script.ts:3:28)`.

Tool names are kept as the server defines them (`github.list_issues`) so they match the
descriptions the model reads. Args are also validated at run time, against the same JSON
Schema, inside the op, before any command is recorded. Invalid args throw a `TypeError` at
the call site. This is deterministic, since the schema is pinned.

The JSON Schema → TypeScript converter is ours, written in Rust in `ptc-codegen`: no
maintained crate exists. It covers objects, `required`, enums, `anyOf`/`oneOf`/`allOf`,
`$ref`/`$defs`, arrays and `additionalProperties`.

### When a toolset changes during a run

A run keeps the toolset it started with, but the server behind it can change during a long
pause: a tool renamed, removed, or with a new schema. The dispatcher checks each call against
the server's current schema at dispatch. A call that no longer matches is not failed
silently: its task is `parked`, and the harness gets the webhook `tool.schema_changed`. The
agent or the developer decides what happens to the run:

- continue against the new schema: `approve`, and the task is `ready` again;
- [fork](03-execution.md#forks) it with a corrected script;
- cancel it, or `reject` the call, which fails with a `ToolError`.

### Observed result types

Most MCP servers declare no `outputSchema`, so the model would write code against a
result it cannot see. But every result ever returned is in a journal. A projection
infers, per tool, the **shape observed** in recorded results, and codegen uses it when
the server declares nothing.

- Shapes only: keys and types, never values. Observations stay within a tenant.
- A field missing from some results is optional. A tool never observed keeps the MCP
  content type and its helpers.
- The declaration says it is observed, not declared (`@observed` in its JSDoc), so the
  model knows it may be incomplete.
- Observed types serve type-checking and the prompt only. Results are never validated
  against them, and they are not part of the toolset digest, so replay does not depend
  on them.

## Credentials

| Step | What happens |
| --- | --- |
| Sent | By the harness, per toolset or per run: `PUT /v1/toolsets/{id}/credentials`, `PUT /v1/runs/{id}/credentials` |
| Stored | Encrypted in the `SecretStore`, with a data key per tenant, itself encrypted by a master key from the deployment's configuration |
| Resolved | By the dispatcher, at call time: the run's credentials if it has some, else the toolset's |
| Used | Injected into the MCP transport |

Credentials never enter the journal, the isolate, a cassette or OpenTelemetry.

A token can expire during a long sleep. A 401 from the server parks the task, and the
harness gets the webhook `credentials.expired`. Once the harness has renewed the
credentials, the task is `ready` again, and the call is sent with the same idempotency key.

## The `http` tool and egress

The `http` tool's requests leave from the dispatcher, never from the isolate. A toolset
enables the tool with an allow-list of hosts. Without an allow-list there is no `http` tool,
and no `fetch` in the script.

Each request goes through these steps, in order:

1. The host must be in the toolset's allow-list.
2. The name is resolved once.
3. Private, loopback, link-local and cloud-metadata IPs are refused.
4. The connection goes to the IP that was checked. A DNS answer that changes between the
   check and the connection cannot reach the internal network.
5. Each redirect is checked again from step 1, up to 5 redirects.
6. The response is bounded to 5 MB and 30 s.

A refused request fails with a `ToolError`. In production, egress also goes through an
egress proxy ([sandboxed workers](06-storage-and-scale.md#sandboxed-workers)).

## Type-checking

`ptc check` (and `POST /v1/check`) type-checks the script against the sandbox lib and the
toolset's declarations, in strict mode, before anything runs. Diagnostics are formatted
for a model: location, message, and a short code frame.

What tsgo sees:

| Declarations | Contents |
| --- | --- |
| ECMAScript lib | ES2025 and the ESNext pieces V8 ships (`Temporal`, explicit resource management…), no DOM, without the shared memory and weak references the sandbox removes |
| Sandbox globals | `globals.d.ts`, declaring exactly the globals the [sandbox](01-product.md#what-exists-in-the-sandbox) has; `fetch.d.ts` only when the toolset enables the `http` tool |
| `ptc.d.ts` | The `ptc` module: `sleep`, `signal`, `webhook`, `run`, `ToolError`, `CancelledError` |
| `ptc-tools.d.ts` | Generated by `ptc-codegen` for the toolset |

What type-checks exists at run time, and the reverse. The sandbox lib lives in
`crates/ptc-runtime/lib/`, with `tsconfig.json` (tsgo's options) and `globals.json` (the
declared names); a test in CI checks that the isolate's globals and the sandbox lib declare the
same names ([decided](08-decisions.md#taken-while-freezing-the-http-api-and-the-sandbox-lib-november-2026)).

`ptc-check` drives tsgo, the native TypeScript 7 compiler, as a CLI, behind the `TypeChecker`
port: TypeScript 7.0 has no programmatic API. The tsgo binary is embedded in the `ptc` binary
and extracted to a cache directory at its first launch, so ai-ptc stays a single binary.

The checker also runs rules of its own. They run on the swc syntax tree that the checks at
start already parse (`deno_ast`), without type information. They are syntactic, and so
slightly less precise than rules based on types.

| Rule | Flags | Fix |
| --- | --- | --- |
| Floating promise | A call to a tool, to a `ptc` function or to an async function of the script, used as a statement: neither awaited, returned nor assigned | `await`; `void expr` says it is intended |
| Ignored async callback | An `async` function passed to `forEach` and the like, which ignore its result | `for … of` with `await`, or `Promise.all` over `map` |
| Absent global | `fetch`, `require`, `process`, `Buffer` | A hint saying what to use instead |

Their diagnostics join tsgo's. These rules are the first line. At run time, a forgotten
`await` is still harmless ([how a script ends](03-execution.md#how-a-script-ends)).

`runs.start` checks by default; the harness can skip it.

Transpilation is `deno_ast` (swc: type stripping and source maps).
