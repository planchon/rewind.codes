# 9. Plan for 0.1

Version 0.1 is the whole specification: every document in `archi/` describes 0.1. It is
built as one product: each part is built directly in its final shape, with no interim
version.

The contracts between the parts are frozen first. Then every workstream is built in
parallel against them. 0.1 is done when the [acceptance criteria](#01-is-done-when) pass.

## What the runtime spike proved

- **Concurrency**: `Promise.all` over 3 tools whose results arrive in reverse order gives
  the right result, and its replay is identical.
- **Determinism**: `Date`, `Math.random`, `crypto.randomUUID`, `performance.now` and
  `Intl` give identical results across runs with the same seed; a different seed gives
  different values.
- **Changed script detected**: a changed argument or an extra call fails replay at the
  exact diverging entry.
- **Durable time**: after `await sleep("10d")` the script sees exactly 10 days later;
  `setTimeout` uses the same durable timers.
- **Signals**: buffered when early, cancel their timeout timer on arrival, time out to
  `undefined`.
- **Sandbox**: no `fetch`, `Deno`, `WeakRef` or `process`; only `ptc` and `ptc:tools` are
  importable, dynamic `import()` included.
- **Callsites** point to TypeScript lines.
- **Errors and cancellation**: `ToolError` (with `outcomeUnknown`) and `CancelledError`
  reach the script and can be caught.

## Current state of the code

The layering is done. `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo test --workspace` pass.

| Crate | State |
| --- | --- |
| `ptc-core` | Pure domain, no serde: ids (`Seed` on 256 bits, `Epoch`), RFC 8785 canonical JSON (golden vectors of the RFC), payloads, the four event families (`signal_awaited` and `invalid_script` included), `RunState`, history validation, `ReplayCursor`; the pinned toolset (MCP servers and their transports; tools with route, description, input and output schemas, policy) and its digest; tool outcomes; cassettes derived from a history; observed result shapes. |
| `ptc-format` | Format v1: one event per row, journal documents (the run's id, script and pinned toolset, then its entries), cassettes, pinned toolsets (canonical, hashing to the toolset's digest), values in canonical form. Golden fixtures pin every variant's spelling; unknown versions, types and fields are refused. |
| `ptc-engine` | The final ports of 0.1, each with an in-memory adapter; the conformance suites of `JournalStore`, `BlobStore`, `ReadModel` and `SecretStore`, passed by the in-memory adapters. Use cases: start (script checks, idempotency key, TTL, the tenant's quota, credentials), signal with idempotency key, cancel, activate, fire timers, expire runs, quarantine and give up, replay, export of journal documents, forks (with the parent's calls reused, keys of calls left without an outcome, and the compatibility report). Activation with the commit invariant, limits and the cancellation grace; sticky execution in an LRU instance cache bounded by count and memory; the lease renewed every 10 s; quarantine after 5 failed activations, retried once by a new build. Tool tasks: policies and backoff, claims heartbeated up to the execution timeout, credentials resolved at dispatch, parked tasks and their webhooks, approvals, the host-tool worker's claim, heartbeat, deferral and answers by token, the `webhook` built-in, a routing dispatcher. Payloads in three tiers. The projector (read model, run telemetry with deterministic span ids, lifecycle webhooks), mock mode (cassettes, mock handlers, static responses, scenarios), retention and deletion, the LLM trace. Driver in real and virtual time, which jumps to timers, tool call retries and deadlines; use-case tests on a fake executor. |
| `ptc-runtime` | `V8Executor`: the checks at start (syntax, imported modules and names, default export), an isolate per instance on its own thread, started from the epoch's V8 snapshot, the epoch-1 glue, logical time, sfc32 and ChaCha20 keyed by the seed, `run.id` and `run.startedAt`, the web-standard globals of `deno_web`, how a script ends (forgotten `await`, unhandled rejection, `DeadlockError`), a CPU watchdog on the isolate thread's CPU clock and a heap limit; args validated against the pinned schema inside the op; `webhook()`; `fetch` with the `http` tool; `ptc:tools` generated from routes; TypeScript mock handlers as a `ToolDispatcher` (`TsMocks`); the memory each instance holds, from V8's heap statistics, for the sticky cache's bound. |
| `ptc-store-turso` | `JournalStore`, `ReadModel` and `SecretStore` on Turso Database (`turso` 0.8.3): the tables of 06, events in format v1, schema version 1 with migrations embedded and a newer schema refused, settings and virtual time in `meta`, secrets encrypted with a data key per tenant under the master key. Passes the three conformance suites, each check on a new database file; `examples/load.rs` measures commit latency and throughput ([08](08-decisions.md#taken-while-building-the-embedded-store-november-2026)). |
| `ptc-architecture-tests` | The dependency rules, telemetry and `anyhow` included, each with a test proving it catches its violation. |
| `ptc-e2e` | The epoch-1 corpus: 28 blue and 29 red cases, forks and tool policies included, recorded and replayed from their journal documents; tests proving the harness cannot pass silently; `cargo run -p ptc-e2e -- replay <case> [--script file.ts]`. |
| `ptc-api` | The HTTP API `/v1` as a contract: the DTOs of every endpoint, problem+json errors, `Idempotency-Key`, the event stream, webhook payloads signed following Standard Webhooks and their verification, the OpenAPI 3.1 document committed at `api/openapi.json` (a test fails when it is stale), and an async client over HTTP(S) and over the embedded server's unix socket. |
| `ptc-check` | `TsgoChecker`, the `TypeChecker` port: tsgo 7.0.2 embedded as its npm tarball, pinned by its integrity and extracted to a cache directory at first use; checks in strict mode against the sandbox lib and `ptc-tools.d.ts`, reading nothing outside its directory; diagnostics with a location, a hint and a code frame; the rules `floating-promise`, `ignored-async-callback` and `absent-global` on the swc syntax tree. About 50 ms per check. |
| Sandbox lib | `crates/ptc-runtime/lib/`: `ptc.d.ts`, `globals.d.ts`, `fetch.d.ts`, tsgo's `tsconfig.json` and `globals.json`, checked with TypeScript 7.0.2. A test checks that the isolate's globals are exactly the declared ones, with and without the `http` tool. |
| `dashboard` | Scripts, Runs, Run and Tools views over the API `/v1`, with types generated from `api/openapi.json`: filters kept in the URL, the source with each call, timer, signal wait and log next to its line, a timeline that compresses idle gaps, payloads on demand, live through the event stream, light and dark themes. Tested with vitest against a mocked API; built to `dashboard/dist/`, which `ptc-server` embeds ([07](07-code-architecture.md#dashboard)). |

Not there yet:

- The other crates: `ptc-store-postgres`, `ptc-mcp`, `ptc-codegen`, `ptc-otel`, `ptc-cli`,
  `ptc-server` and the evals.
- Starting a run does not type-check the script.
- The projector and its span derivation exist, but no OpenTelemetry exporter (`ptc-otel`).
  The read model is stored by `ptc-store-turso` only.

## Contracts frozen first

| Contract | Specified in | Implemented by | Consumed by |
| --- | --- | --- | --- |
| Engine ports (traits and conformance suites) | [07](07-code-architecture.md#ports) | The adapters: runtime, stores, blobs, MCP, checker, OpenTelemetry, secrets, webhooks | `ptc-engine` |
| Stored format v1 (events, journal documents, cassettes, pinned toolsets) | [02](02-journal.md#stored-format-v1) | `ptc-format` | The stores, `ptc test`, the e2e corpus, journal export and archive |
| Pinned toolset (schemas, policies, declarations) | [04](04-tools.md#toolsets-and-code-generation) | `ptc-codegen` | The runtime (`ptc:tools`), the checker, the dispatcher, the SDK, the model's prompt |
| HTTP API `/v1` (OpenAPI) | [07](07-code-architecture.md#http-api-v1) | `ptc-api` (DTOs, OpenAPI document, Rust client), `ptc-server` | The CLI, the SDK and its agent tools, the dashboard, the evals |
| Sandbox lib (`ptc.d.ts` and the checker's `lib`) | [04](04-tools.md#type-checking) | `ptc-runtime` (the globals that exist) | `ptc-check`, the prompt |

Each contract is reviewed by the owner, with diagrams, before the code that depends on it.
The five were approved in November 2026.

## Workstreams

| Workstream | Crates | What remains | Depends on (contracts) |
| --- | --- | --- | --- |
| Domain and format | `ptc-core`, `ptc-format` | Nothing beyond what the other workstreams need | Stored format v1, pinned toolset |
| Engine | `ptc-engine` | Nothing beyond what `ptc-server` wires: the worker, scheduler, projector and retention loops | Engine ports, stored format v1, pinned toolset |
| Runtime (V8) | `ptc-runtime` | Nothing beyond the load and limit tests of operations | Engine ports, pinned toolset, sandbox lib |
| Stores | `ptc-store-turso`, `ptc-store-postgres` | Postgres, through the same conformance suite, with tables ready to partition | Engine ports, stored format v1 |
| Blobs | `ptc-blob` | Files and S3; the `payloads` table with size and preview; workers' local cache; cleanup | Engine ports |
| Tools | `ptc-codegen`, `ptc-mcp` | `ptc-codegen`: JSON Schema → TypeScript, prompt rendering, observed types. `ptc-mcp`: stdio and HTTP, idempotency key and trace context in `_meta`. Built-ins `http` and `webhook`; credentials | Engine ports, pinned toolset |
| Type-checking | `ptc-check` | tsgo; the checker's own rules; the sandbox lib | Engine ports, pinned toolset, sandbox lib |
| Observability | `ptc-otel` | `tracing` subscriber; `SpanExporter` over OTLP; metrics | Engine ports, stored format v1, HTTP API `/v1` |
| Server | `ptc-server`, `ptc-api` | API `/v1`: DTOs, OpenAPI document and Rust client in `ptc-api`; SSE; webhooks signed with HMAC; host-tool long-poll; authentication; tenants; credentials; workers; scheduler; projector; roles `api` and `worker`; embedded mode | Engine ports, HTTP API `/v1` |
| CLI | `ptc-cli` | Client of the embedded or a remote server, through `ptc-api`; local commands: `check`, `test`, replay of an exported journal; exit codes | HTTP API `/v1`, stored format v1, sandbox lib |
| TypeScript SDK | `sdk/typescript` | Client; host-tool worker; agent tools; adapters for the Anthropic and OpenAI tool formats | HTTP API `/v1`, pinned toolset |
| Dashboard | `dashboard` | Scripts, Runs, Run and Tools views over the public API | HTTP API `/v1` |
| Evals | `evals` | Tasks; deterministic fake MCP servers; the loop generate → check → run → fix; reports per model | HTTP API `/v1`, pinned toolset, sandbox lib |
| Operations | deployment | Sandboxed workers; worker pools of previous epochs; retention and archiving; load and limit tests | Engine ports, HTTP API `/v1` |

## Designs settled (November 2026)

Each design below was reviewed by the owner, with diagrams. The list points to where it is
specified.

- **Embedded mode**: the CLI is an API client of `ptc server --embedded`
  ([07](07-code-architecture.md#embedded-server-and-cli)).
- **Sticky execution and a thread per instance** (was question 2)
  ([03](03-execution.md#sticky-execution)).
- **Payloads in three tiers** ([06](06-storage-and-scale.md#payloads)).
- **Fork semantics** (was question 9) ([03](03-execution.md#forks)).
- **Tool task lifecycle**: host tools that last days, the `approval` policy, tool
  concurrency, toolset change during a run ([04](04-tools.md#tool-task-lifecycle)).
- **Credentials** resolved at dispatch ([04](04-tools.md#credentials)).
- **API authentication and tenancy** ([06](06-storage-and-scale.md#tenancy-and-authentication)).
- **Egress of the `http` tool** ([04](04-tools.md#the-http-tool-and-egress)).
- **Retention and deletion** (was question 11)
  ([06](06-storage-and-scale.md#retention-and-deletion)).
- **Sandboxed workers** ([06](06-storage-and-scale.md#sandboxed-workers)).
- **tsgo in `ptc-check`** and the checker's own rules ([04](04-tools.md#type-checking)).
- **The JSON Schema → TypeScript converter**: ours, in Rust
  ([04](04-tools.md#toolsets-and-code-generation)).
- **The dashboard**: a React SPA served by `ptc-server`
  ([05](05-testing-and-observability.md#dashboard)).
- **Evals**: the models, run on demand ([05](05-testing-and-observability.md#evals)).

## Out of 0.1

- `ptc replay <run> --inspect`, which attaches the V8 inspector to step through a past run.
- `continueAsNew`. A run that reaches the history cap (50k entries) is aborted with
  `limit_exceeded`.
- A columnar read model. The read model lives in Postgres.
- Sharding over several Postgres clusters.

## 0.1 is done when

- A harness drives runs through the SDK, with several workers on one Postgres. Each run
  appears as one trace under the harness's span, and runs can be searched by label and tool.
- Locally, `ptc run`, `ptc signal`, `ptc replay` and `ptc trace` work on a run that sleeps,
  is interrupted, and resumes in another process. A forgotten `await`, an unhandled
  rejection and a deadlock each end as specified.
- A script calls a real MCP server, with types checked beforehand.
- The evals report, per model and prompt rendering, the first-try check pass rate, task
  success, iterations and tokens.
- `ptc test` replays a recorded real run as a scenario. `ptc fork` continues a failed run
  with a corrected script without repeating its recorded calls.
- Load tests and limit tests pass.
- An operator finds any run by label, tool or error in the dashboard, and sees its code next
  to its trace.

## Open questions

Questions 1, 2, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 and 17 were settled in November
2026: see [08-decisions.md](08-decisions.md#taken-after-the-layering-november-2026). Questions
2, 9 and 11 were settled with the [designs](#designs-settled-november-2026). Question 6
(`continueAsNew`) is [out of 0.1](#out-of-01). No other question is open.
