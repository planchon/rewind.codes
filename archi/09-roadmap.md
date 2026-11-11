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
| `ptc-core` | Pure domain, no serde: ids (`Seed` on 256 bits, `Epoch`), RFC 8785 canonical JSON (golden vectors of the RFC), payloads, the four event families (`signal_awaited` and `invalid_script` included), `RunState`, history validation, `ReplayCursor`. |
| `ptc-format` | Format v1: one event per row, journal documents, values in canonical form. Golden fixtures pin every variant's spelling; unknown versions, types and fields are refused. |
| `ptc-engine` | Ports; use cases (start with script checks, idempotency key and TTL; signal with idempotency key; cancel; activate; fire timers; expire runs; run tool tasks, an interrupted call retried with its idempotency key up to 5 attempts; quarantine and give up; replay; export); activation with the commit invariant, limits and the cancellation grace; payload offload above 1 KiB; driver in real and virtual time; in-memory adapters; the store conformance suite, passed by `MemoryJournal`; use-case tests on a fake executor. |
| `ptc-runtime` | `V8Executor`: the checks at start (syntax, imported modules and names, default export), an isolate per instance on its own thread, the epoch-1 glue, logical time, sfc32 and ChaCha20 keyed by the seed, `run.id` and `run.startedAt`, how a script ends (forgotten `await`, unhandled rejection, `DeadlockError`), CPU watchdog and heap limit. |
| `ptc-architecture-tests` | The dependency rules, telemetry and `anyhow` included, each with a test proving it catches its violation. |
| `ptc-e2e` | The epoch-1 corpus: 15 blue and 22 red cases, recorded and replayed from their journals; tests proving the harness cannot pass silently; `cargo run -p ptc-e2e -- replay <case> [--script file.ts]`. |

Not there yet:

- The other crates: `ptc-store-turso`, `ptc-store-postgres`, `ptc-blob`, `ptc-mcp`,
  `ptc-codegen`, `ptc-check`, `ptc-otel`, `ptc-cli`, `ptc-server`, the TypeScript SDK, the
  dashboard and the evals.
- Toolsets are only a sorted, deduplicated list of tool names with a digest: no schemas, no
  policies. Every tool is `retry`, and only the crash case is handled (a task claimed again
  after its claim expired); transient errors and their backoff are not.
- Starting a run does not type-check the script.
- Every activation replays the full history: there is no instance cache.
- An activation that fails 5 times in a row does not quarantine the run: it is retried every
  lease period, forever. The store does not count consecutive failures, nor record why a run
  is quarantined.
- The watchdog measures the wall-clock time of a call, not the thread's CPU clock, so a
  heavily loaded host can make a script exceed its budget (live, the run is aborted; during
  replay, the activation is retried).
- Leases are not renewed during an activation. A replay longer than the lease (30 s) lets
  another worker take the run over, and with several workers on a very long history they can
  fence each other off forever.
- No web-standard globals (`URL`, `TextEncoder`, `atob`…); `queueMicrotask` is present.
- No `webhook(name)`, no feed, no projector, no OpenTelemetry export, no read model.

## Contracts frozen first

| Contract | Implemented by | Consumed by |
| --- | --- | --- |
| Engine ports (traits and conformance suites) | The adapters: runtime, stores, blobs, MCP, checker, OpenTelemetry | `ptc-engine` |
| Stored format v1 (journal, cassettes) | `ptc-format` | The stores, `ptc test`, the e2e corpus, journal export |
| Pinned toolset (schemas, policies, declarations) | `ptc-codegen` | The runtime (`ptc:tools`), the checker, the dispatcher, the SDK, the model's prompt |
| HTTP API `/v1` (OpenAPI) | `ptc-server` | The CLI, the SDK and its agent tools, the dashboard, the evals |
| Sandbox lib (`ptc.d.ts` and the checker's `lib`) | `ptc-runtime` (the globals that exist) | `ptc-check`, the prompt |

Each contract is reviewed by the owner, with diagrams, before the code that depends on it.

## Workstreams

| Workstream | Crates | What remains | Depends on (contracts) |
| --- | --- | --- | --- |
| Domain and format | `ptc-core`, `ptc-format` | Toolset with schemas and policies; cassettes; a fork's link to its parent | Stored format v1, pinned toolset |
| Engine | `ptc-engine` | Policies and backoff; tool task lifecycle; sticky instance cache; lease heartbeat; quarantine after 5 failures; feed and projector; fork; toolset change; mock mode; tenants | Engine ports, stored format v1, pinned toolset |
| Runtime (V8) | `ptc-runtime` | Web-standard globals from deno crates; the thread's CPU clock; startup snapshots; args validated against schemas; `webhook()`; the `http` tool | Engine ports, pinned toolset, sandbox lib |
| Stores | `ptc-store-turso`, `ptc-store-postgres` | Turso and Postgres, through the same conformance suite: feed, failure counts, tenants, tables ready to partition | Engine ports, stored format v1 |
| Blobs | `ptc-blob` | Files and S3; the `payloads` table with size and preview; workers' local cache; cleanup | Engine ports |
| Tools | `ptc-codegen`, `ptc-mcp` | `ptc-codegen`: JSON Schema → TypeScript, prompt rendering, observed types. `ptc-mcp`: stdio and HTTP, idempotency key and trace context in `_meta`. Built-ins `http` and `webhook`; credentials | Engine ports, pinned toolset |
| Type-checking | `ptc-check` | tsgo; the checker's own rules; the sandbox lib | Engine ports, pinned toolset, sandbox lib |
| Observability | `ptc-otel` | `tracing` subscriber; `SpanExporter`; deterministic span ids; read model; LLM trace; metrics | Engine ports, stored format v1, HTTP API `/v1` |
| Server | `ptc-server` | API `/v1`; SSE; webhooks signed with HMAC; host-tool long-poll; authentication; tenants; workers; scheduler; projector; embedded mode | Engine ports, HTTP API `/v1` |
| CLI | `ptc-cli` | API client, embedded or remote; local commands: `check`, `test`, `replay`, `trace`, `fork` | HTTP API `/v1`, stored format v1, sandbox lib |
| TypeScript SDK | `sdk/typescript` | Client; host-tool worker; agent tools; adapters for the Anthropic and OpenAI tool formats | HTTP API `/v1`, pinned toolset |
| Dashboard | `dashboard` | Scripts, Runs, Run and Tools views over the public API | HTTP API `/v1` |
| Evals | `evals` | Tasks; deterministic fake MCP servers; the loop generate → check → run → fix; reports per model | HTTP API `/v1`, pinned toolset, sandbox lib |
| Operations | deployment | Sandboxed workers; worker pools of previous epochs; retention and archiving; load and limit tests | Engine ports, HTTP API `/v1` |

## Designs to settle before their code

Each design is reviewed by the owner, with diagrams, before the code that depends on it.

- **Process model of the embedded CLI.** Turso's multi-process access is experimental.
  Proposed: one `ptc server --embedded` process owns `.ptc/`, and the other `ptc` commands
  are its clients.
- **Sticky instance cache and thread per instance** (was question 2). What is kept between
  activations (instances, histories, payloads) and how a worker gets the run's next
  activation. Each live V8 instance holds a thread: is a thread per instance acceptable at
  the target scale, or does it need an isolate pool per thread?
- **Payloads of medium size in Postgres.** Kept in the `payloads` table, with S3 above a
  measured threshold instead of 1 KiB.
- **Fork semantics** (was question 9). How the new run references its parent, and which
  recorded results are reused. In particular: what happens at the first command that
  differs from the parent's (continue live, or reject the fork); which idempotency key the
  reused and the re-executed calls carry; and what a fork does with a call left without an
  outcome by a cancelled or aborted parent.
- **Host tools that last days**, such as `ask_user`: timeouts, heartbeats and asynchronous
  completion.
- **Tool concurrency.** A limit per run and per server in the dispatcher. It is invisible to
  the journal. To settle with it: whether calls still queued when a script fails are
  dispatched.
- **The `approval` policy.** It parks a task until the harness accepts it, so that scripts
  do not bypass the harness's permission gates.
- **Credentials resolved at dispatch** through a provider: a token expires during a long
  sleep.
- **API authentication and tenancy.** Tenancy (in the run metadata) is settled first: it
  touches every key, the quotas, and the fairness of `acquire_runnable`. Agent tools that
  take a `run_id` check the run's labels.
- **Egress of the `http` tool**: SSRF protection and an egress allow-list.
- **Toolset change during a run**: how the harness is warned when a call no longer matches
  ([tools](04-tools.md#when-a-toolset-changes-during-a-run)).
- **Retention durations and deletion** (was question 11). The shape is decided (finished
  journals archived to S3 and loaded on demand, read model per tenant retention). Still
  open: how long a finished journal stays in Postgres, the default tenant retention, and
  whether runs are ever deleted (tool arguments can hold personal data).
- **Sandboxed workers** for multi-tenant deployments
  ([security](06-storage-and-scale.md#security)).
- **tsgo in `ptc-check`**: how tsgo is shipped with the single `ptc` binary, and how the
  checker's own rules (floating promise, ignored async callback, absent global) get type
  information. TypeScript 7.0 has no programmatic API; one is announced for 7.1.
- **The JSON Schema → TypeScript converter.** No maintained Rust crate was found; the spec
  says it is ours.

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

Questions 1, 3, 4, 5, 7, 8, 10, 12, 13, 14, 15, 16 and 17 were settled in November 2026: see
[08-decisions.md](08-decisions.md#taken-after-the-layering-november-2026). Questions 2, 9 and
11 are [designs to settle](#designs-to-settle-before-their-code), and question 6
(`continueAsNew`) is [out of 0.1](#out-of-01). No other question is open.
