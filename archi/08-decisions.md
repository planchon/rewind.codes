# 8. Decisions

Three kinds of decisions, kept apart so it is clear which ones are open:

- **Product**: taken by the project owner.
- **Architecture (approved)**: proposed during design and approved for 0.1.
- **Technical (open to challenge)**: taken during design or implementation, delegated.

## Product

| Decision | Notes |
| --- | --- |
| ai-ptc is a tool for **agent harness developers** | Server + TypeScript SDK + ready-made agent tools; the CLI is for local development. |
| **Durable workflows are the priority** | A short run is the same mechanism started with `wait`. |
| **Built to scale** | Postgres in production. Turso only for the embedded engine (CLI, local dev, tests). |
| **Payloads live in S3**, except small ones | Three tiers by size: in the event, in the `payloads` table, in S3 ([storage](06-storage-and-scale.md#payloads)). |
| **Mocks are recorded from real runs** | Cassettes from journals; TypeScript mock handlers as a complement. |
| **Events can come from any source** | Harness, humans, other agents, other runs, external systems through webhooks. |
| **No `fetch`, no file system** | Network only through the opt-in, journaled `http` tool. No file system at all. |
| **The agent gets several tools**, not only `run_script` | `check_script`, `test_script`, `get_run`, `list_runs`, `signal_run`, `cancel_run`, `retry_run`, `search_tools`. |
| **Everything goes through the journal** | Replay re-executes against the journal, never restores a snapshot. |
| **Type-checking is in 0.1** | Generate → check → fix is the agent's main loop. |
| **`deno_core` + `deno_ast`** | Instead of raw `rusty_v8`. |
| **CLI surface for 0.1** | `run`, `check`, `test`, `replay`, `fork`, `trace`, plus `signal`, `cancel`, `server`. `ptc server` replaced `ptc worker` with the embedded mode (below). |
| **Concurrency, side effects, randomness**: handled as proposed | `seq` in emission order; tool policies and idempotency keys; seeded PRNG. |
| **OpenTelemetry for everything in the service** | Service telemetry and run telemetry, exported over OTLP. |
| **A dashboard of every run and all the code executed on the platform** | Built on the journal, not on an OTel backend. |
| **Every run has a TTL, 30 days by default** | `expires_at` in `run_started`; at the deadline the run is cancelled through the normal path. It bounds signal waits, tool calls that never answer, quarantine, and the life of a previous epoch. |
| **The system is measured by evals** | Models run end to end on the embedded engine against fake MCP servers. Choices that are cheap to change are decided on the numbers. |
| **A failed run can be forked with a corrected script** | `ptc fork` and `retry_run({ run_id, code })` as an agent tool. Fixing step 7 without redoing steps 1–6 is what the journal is for. |
| **Result types are inferred from the journal** when a server declares no `outputSchema` | Observed shapes, for type-checking and the prompt only. |
| **`run_script` returns a bounded result** | It lands in the model's context: output truncated above 256 KB, last log lines, errors with a code frame. |
| **What type-checks is what exists** | A baseline of deterministic web-standard globals; the checker's `lib` declares exactly the sandbox's globals; hints for absent ones such as `fetch`. |

### Taken after the layering (November 2026)

Answers to the questions raised once the layering was done.

| Decision | Notes |
| --- | --- |
| **An invalid script fails fast** | Starting a run compiles, checks and type-checks the script (imports, names, default export, types) and refuses it before any run exists. The agent learns in the same call; it does not wait for an execution. |
| **An interrupted tool call is retried with its idempotency key** | `retry` is the default policy of every tool; `at_most_once` is an opt-out per tool in the toolset. After 5 attempts the call fails with `outcomeUnknown`. |
| **A toolset that changed during a run is reported, not failed silently** | The task is parked and the harness gets the webhook `tool.schema_changed`; the agent or the developer decides what happens to the run (closes question 3; [tools](04-tools.md#when-a-toolset-changes-during-a-run)). |
| **The cancellation grace is one delivery** | The script ends within the delivery of `cancel_requested`, or it is aborted (closes question 5). |
| **The CPU budget applies to each call into the script**, measured as CPU time | The watchdog reads the CPU clock of the isolate's thread, not wall-clock time. |
| **An activation that fails 5 times in a row quarantines the run** | With the last error as reason. The failure is logged and reported through OpenTelemetry and webhooks (closes question 16). |
| **Web-standard globals come from existing crates** | `deno_webidl`, `deno_url`, `deno_web` and the like, with their non-deterministic parts replaced (closes question 13). In general, take a maintained crate rather than rewrite one. |
| **A signal or a cancellation sent to a finished run is refused** | 409 in the API, an error in the CLI, instead of being dropped (closes question 4). |
| **The tenant lives in the run metadata**, never in the core | Closes question 7. |
| **Payload size and preview are blob metadata** | A `payloads (digest, bytes, preview)` table kept by the store (closes question 1). |
| **Webhooks: an unguessable URL and an HMAC signature** | Closes question 8. |
| **Run labels: a bounded map set at start**, exported to OpenTelemetry for allow-listed keys only | Closes question 10. |
| **TTL: a maximum per deployment** (365 days by default) | A run past its deadline ends `cancelled` with the reason `ttl`; no status of its own (closes question 15). |
| **No `LISTEN/NOTIFY`** | Wake-ups do not rely on Postgres notifications. |
| **The corpus lives in `ptc-e2e`** | A tooling crate allowed to depend on adapters. |
| **The embedded store is Turso Database** | The `turso` crate, the Rust rewrite of SQLite, rather than SQLite through rusqlite or libSQL (closes question 17). It is in beta and its multi-process access is experimental, so only the embedded server opens the database (below). The store stays behind `JournalStore` and its conformance suite, so the choice can change. |
| **`run_script` returns at most 256 KB of output** | Above it the output is truncated and the model pages through the rest (closes question 12). |
| **A quarantined run is released by hand, or retried when a new build is deployed** | A replay that fails again writes nothing, so the automatic retry is safe; a run nobody fixes ends at its TTL (closes question 14). |
| **Retention: active runs are never touched; finished runs are archived, then deleted** | Defaults, configurable per tenant: a finished run's journal stays 30 days in Postgres (or Turso), then is archived to S3 as a journal document for 1 year and loaded on demand; the read model keeps 90 days; `DELETE /v1/runs/{id}` erases a run at any time; payloads unreferenced for 24 h are deleted, using a `payload_refs` table filled at commit (closes question 11; [storage](06-storage-and-scale.md#retention-and-deletion)). |
| **Leases are renewed by a heartbeat during an activation** | The lease stays short (30 s) and the worker renews it about every 10 s while it lives, during the replay as during the execution. A long replay keeps its lease; a dead worker frees the run at most 30 s after its last renewal. The fence still refuses the commit of a worker that lost its lease. |
| **The latency of the polling path is accepted** | A script of 5 sequential calls makes about ten store round trips. Gains come from caching (instances, histories, payloads), not from a separate short path. |
| **The embedded CLI** | Schema versioned with migrations in the binary, refusing a newer schema; `--json` on every command and one exit code per outcome; a first Ctrl-C finishes the call in flight and commits it, a second one exits at once. |
| **No milestones: 0.1 is built as one product** | Version 0.1 is the whole specification. Everything is coded directly in its final shape, with no interim version of a feature. The plan is in [09](09-roadmap.md). |
| **Tools are called by ai-ptc, except host tools** | MCP tools and the built-ins `http` and `webhook` are dispatched by ai-ptc, and their results go back to the script, never to the agent. The harness is on the path only for its host tools, long-polled by the SDK worker. Host tools are optional: a toolset may have only MCP servers. No other kind of tool in 0.1 ([tools](04-tools.md#routes)). |
| **`ptc-codegen` turns each pinned tool into three outputs** | The `ptc:tools` module, `ptc-tools.d.ts` and the compact prompt rendering. The JSON Schema → TypeScript converter is ours, in Rust: no maintained crate exists. Args are also validated at run time against the same schema, inside the op ([tools](04-tools.md#toolsets-and-code-generation)). |
| **The engine ports of 0.1** | `JournalStore`, `BlobStore` and `ToolDispatcher` extended; `TypeChecker`, `ReadModel`, `SpanExporter`, `SecretStore` and `WebhookSender` added; `ScriptExecutor`, `Clock` and `Entropy` unchanged, the instance cache living in the engine. Every port has a conformance suite in `ptc-engine` that all its adapters pass ([07](07-code-architecture.md#ports)). |
| **The stored format v1 covers four documents** | The event, the journal document, the cassette and the pinned toolset. Labels, idempotency keys, tenant, trace context, credentials, quarantine, lease, fence, sticky worker, feed and a fork's link to its parent stay out: table columns, never replayed, exported or put in a cassette ([02](02-journal.md#stored-format-v1)). |
| **HTTP API `/v1`** | An API key per tenant as Bearer; `Idempotency-Key` on run creation and signals; errors as `application/problem+json` with a message written for a model; the OpenAPI document generated from the DTOs (utoipa), and the SDK's types from the OpenAPI ([07](07-code-architecture.md#http-api-v1)). |
| **Embedded mode: the CLI is an API client of `ptc server --embedded`** | The CLI never opens `journal.db`. The embedded server is started on demand, holds `.ptc/server.lock`, listens on `.ptc/server.sock` (mode 0600, no key, one tenant `local`), and stops after 10 minutes idle. `ptc check`, `ptc test` and the replay of an exported journal run in the CLI process ([07](07-code-architecture.md#embedded-server-and-cli)). |
| **A thread per live instance** | With the bounded sticky cache. An isolate pool per thread only if the load tests fail ([execution](03-execution.md#sticky-execution)). |
| **Sticky execution** | After a commit the worker keeps the instance in an LRU cache (500 instances and 4 GiB per worker by default). The run's sticky worker gets its next activation first; other workers take it after 5 s. A hit delivers only the new trigger; a miss starts from a V8 startup snapshot and replays (closes question 2; [execution](03-execution.md#sticky-execution)). |
| **Payloads in three tiers** | Up to 1 KiB in the event; up to 64 KiB in the `payloads` table (Postgres, Turso); above, in S3 or files, with size and preview in `payloads`. Thresholds configurable, defaults confirmed by load tests ([storage](06-storage-and-scale.md#payloads)). |
| **Fork semantics** | A fork is a new run with its own id, the parent's seed, input (overridable) and toolset, and a new TTL. While its commands match, the parent's entries are copied and no tool is called again; at the first difference, at `at`, or at the end of the parent's history, it continues live with keys `{fork_run_id}/{seq}`. A call the parent left without an outcome is dispatched again with the parent's key under `retry`, and fails with `outcomeUnknown` under `at_most_once`. The link to the parent is run metadata (closes question 9; [execution](03-execution.md#forks)). |
| **One state machine for tool tasks** | `approval` → `ready` → `claimed` → `done`, with `deferred` (a token, for host tools that answer later) and `parked` (schema changed, 401). Per-tool policy: `retry` or `at_most_once`, `approval`, execution timeout (5 min for MCP), heartbeat timeout (60 s for host tools), concurrency (16 per run, 8 per MCP server on each worker). A terminal run's tasks are deleted ([tools](04-tools.md#tool-task-lifecycle)). |
| **Credentials are resolved by the dispatcher at call time** | Sent per toolset or per run, stored encrypted in the `SecretStore` (a data key per tenant, encrypted by a master key from the deployment's configuration), injected into the MCP transport. Never in the journal, the isolate, a cassette or OpenTelemetry. A 401 parks the task and sends `credentials.expired` ([tools](04-tools.md#credentials)). |
| **API keys and tenancy** | Keys `ptc_sk_…`, stored hashed, each with a tenant and a scope (a role, and optionally allowed labels). The tenant is part of every storage key. A quota of active runs per tenant (429 above it). Workers draw a tenant at random among those with runnable work, then take its oldest runnable run ([storage](06-storage-and-scale.md#tenancy-and-authentication)). |
| **Egress of the `http` tool** | Requests leave from the dispatcher. No allow-list, no `http` tool and no `fetch`. Each request: allow-list, one DNS resolution, private and metadata IPs refused, connection to the checked IP, each redirect checked again (5 at most), response bounded to 5 MB and 30 s; an egress proxy in production ([tools](04-tools.md#the-http-tool-and-egress)). |
| **Sandboxed workers** | One binary, two roles: `--role api` (no V8) and `--role worker`. Workers run under gVisor, in pools per tenant when needed, reach the outside only through the egress proxy, and have no route to the cloud metadata endpoint. Three walls between two tenants: V8, gVisor, separate pools ([storage](06-storage-and-scale.md#sandboxed-workers)). |
| **The dashboard is a React SPA served by `ptc-server`** | It uses only the public API ([05](05-testing-and-observability.md#dashboard)). |
| **Evals: Claude Opus, Sonnet and Haiku, and one OpenAI model** | Run on demand, not in CI: they cost model tokens ([05](05-testing-and-observability.md#evals)). |
| **CLI exit codes** | 0 completed or success, 1 failed, 2 cancelled, 3 aborted, 4 still waiting, 5 quarantined, 6 refused, 64 usage, 65 script refused at start, 70 internal error, 130 second Ctrl-C ([07](07-code-architecture.md#embedded-server-and-cli)). |
| **Out of 0.1** | `ptc replay --inspect`; `continueAsNew` (a run that reaches the history cap of 50k entries is aborted with `limit_exceeded`; closes question 6); a columnar read model; sharding over several Postgres clusters. |
| **Type-checking uses tsgo (TypeScript 7), driven as a CLI** | TypeScript 7.0 has no programmatic API. The tsgo binary is embedded in the `ptc` binary and extracted to a cache directory at first launch, so ai-ptc stays a single binary. The checker's own rules run on the swc syntax tree that start already parses: syntactic, and slightly less precise than rules based on types ([tools](04-tools.md#type-checking)). |

## Architecture (approved for 0.1)

| Decision | Why |
| --- | --- |
| Strict layering: domain, application, adapters, composition roots | The core must not know how events are stored. Boundaries are enforced in code. |
| **Ports live in `ptc-engine`**, not in `ptc-core` | Core stays a model; the engine says what it needs from outside. |
| **No serde at all in `ptc-core`**; `ptc-format` owns the stored format (v1) | A refactor of domain types can never break the journal of a paused run. Costs a conversion layer. |
| Core knows payload identity (value or digest), not location | Inline vs S3, keys, sizes and previews are storage concerns. |
| Ports shaped by invariants (`commit_activation`) | The commit invariant is guaranteed by every store, not re-implemented by callers. |
| The runtime only executes; replay lives in the engine | The V8 adapter stays small and swappable; replay logic is tested without V8. |
| Non-determinism only through ports | The same engine is production, mock mode and the test harness. |
| Dependency rules checked by a test | Boundaries hold without relying on discipline. |
| Review the architecture principles of each contract and design, with diagrams, before the code that depends on it | Process rule. |
| **The journal is the record; OTel, SSE and the dashboard are projections** of committed entries | One source of truth. Replay can never duplicate telemetry, and the dashboard isn't limited by OTel sampling or retention. |
| Service telemetry through the `tracing` facade; OpenTelemetry only in `ptc-otel`, installed by composition roots; nothing in `ptc-core` | The engine stays free of vendor SDKs; exporters are a deployment choice. |
| A read side separate from `JournalStore`: the `ReadModel` port, filled by a projector | `JournalStore` stays shaped by the write path's invariants; queries across runs get their own tables. |
| The dashboard is a client of the public HTTP API | The SDK, the agent tools and the dashboard see the same thing; no back door into the store. |
| Run metadata (labels, trace context) stored with the run, outside the journal's events | The script never sees it, so it can't affect determinism, and it never ends up in cassettes. |

Approved in the design review of November 2026. They shape the stored format, the
runtime's observable behavior or the port contracts, so they are frozen with format v1:

| Decision | Alternative rejected | Why |
| --- | --- | --- |
| **Determinism epoch** in `run_started`. Glue, generators, library and codegen are kept per epoch in the binary; V8 and ICU get a worker pool per epoch | Pinning the crate version and refusing the runs of another version | The script is immutable, the runtime is not: a change in V8, the glue or codegen can shift a `seq` or a digest of a suspended run. |
| **A replay mismatch quarantines the run** | `run_aborted { non_determinism }` at once | By construction it is a fault of the platform, never of the script. The run waits for a fix. |
| **A script's terminal command waits for its tool calls** | Dropping pending calls when `main` settles | A forgotten `await` becomes harmless, and a run that ends by itself has an outcome for every call, which a fork needs. Costs: a raced slow tool is still awaited. |
| **An unhandled rejection fails the run** | Ignoring it | Node's behavior. Ignoring it hid failed calls. |
| **Idle with nothing pending fails the run with `DeadlockError`** | Leaving the run active | Nothing can wake the script. It is the script's own outcome, reproduced by replay, so `fail_run` and not `run_aborted`. |
| **256-bit seed; ChaCha20 for `crypto`**, sfc32 kept for `Math.random` | sfc32 with a 64-bit seed for both | Scripts make tokens and passwords with `crypto`, and they were predictable. |
| **Canonical JSON is RFC 8785** | "Keys sorted, no whitespace" | Digests are compared for as long as runs live; that wording left numbers and key order to the serializer. |
| **Idempotency keys for starting a run and for signals** | Keys on tool calls only | A retried request must not start a second run, nor leave a second signal in the mailbox. |

## Technical (open to challenge)

| Decision | Alternative rejected | Why |
| --- | --- | --- |
| Durability by event-sourced replay | V8 heap snapshots | V8 can't snapshot a running isolate with pending promises, and snapshots break on every V8 upgrade. |
| Inbox + single writer per run (lease + fence) | Writing completions straight into the history | Makes history order deterministic and replay exact. |
| Commit invariant; `run_aborted` distinct from `fail_run` | Committing triggers and commands separately | Replay never sees a half-processed trigger; host failures are not replayed as script failures. |
| Ops are synchronous; promises resolved by the JS glue | Async ops on deno_core's event loop | No hidden scheduling; delivery order is fully controlled. |
| Logical time = `recorded_at` of the last delivered trigger, monotonic per run | Real time, or time recorded per call | Time moves only when the outside world speaks, identically on replay. |
| Seeded generators for `Math.random` and `crypto` | Journaling each random value | No journal entries for randomness; the seed is in `run_started`. |
| `TZ=UTC` and `en-US` locale enforced; sandbox refuses to start otherwise | Host-dependent `Intl` | Identical output on every machine. |
| Signals: one FIFO mailbox per name; timeouts are durable timers cancelled on arrival | Drop signals nobody waits for | Nothing is lost if the signal arrives before the script waits. |
| Content addressing everywhere (SHA-256; canonical JSON for payloads) | Random ids for scripts and payloads | Deduplication, cacheability, and digest comparison during replay. |
| Inline threshold 1 KiB | Everything in S3 | Keeps tiny payloads one read away. Configurable. The tiers above it are an owner decision ([payloads](06-storage-and-scale.md#payloads)). |
| Postgres + an embedded store behind one port with a shared conformance suite | One backend | Scale in production, zero setup locally. |
| V8 instance on a dedicated thread behind `ScriptExecutor` | Running V8 on the async runtime | V8 objects are `!Send`; the engine stays a normal async crate. |
| Ports as traits returning `impl Future + Send`; engine generic over adapters | `dyn` traits with `async_trait` | Static dispatch, no boxing; composition roots pick types. |
| Tool policies `retry` / `at_most_once`, idempotency key `{run_id}/{seq}` | Always retry | A crash mid-call must not silently duplicate a non-idempotent effect. |
| Single-file scripts, imports only from `ptc` and `ptc:tools` | npm imports | Determinism and supply chain. Curated libraries later. |
| Tool names kept as defined by the server | camelCase renaming | They must match the descriptions the model reads. |
| Callsites mapped to TypeScript lines through source maps | JS positions | Traces and errors point at the code the model wrote. |
| Projector fed by feed items written in `commit_activation`, one per consumer, claimed and acknowledged | An in-process hook after commit; a high-water-mark cursor on a sequence | Nothing lost on a crash; Postgres sequences are not visible in commit order; a slow OTel backend doesn't delay the dashboard. |
| Trace id = the harness's trace id if given, else the run id; span ids derived from `(run, seq)`, `(run, fence)`, `(run, idx)` | Random span ids | A span exported twice is the same span; live service spans can name journal spans as parents. |
| Run span exported when the run ends, with an id known from the start | A span open for the whole run; no run span at all | Durable runs outlive spans. Short runs still appear as a complete tree. |
| A run with a `traceparent` joins the harness's trace | A new trace per run, with a span link | The run shows up under the LLM call that wrote it. Costs very long traces for long runs. |
| Spans carry payload digests, sizes and previews, never values | Full payloads as attributes | Model-written arguments may hold sensitive data; OTel backends have weaker access control and attribute size limits. |
| Sampling per run, from the trace id; the read model is never sampled | Sampling per activation | Every worker agrees without coordination; the dashboard stays complete. |
| Logs exported as OTel log records | Span events | They need no open span and correlate with the activation span. |
| W3C trace context forwarded to MCP servers in `_meta` | No propagation | MCP servers' own spans join the run's trace. |
| The `ptc` binary is its own crate (entry point), depending on `ptc-cli` and `ptc-server` | The binary target in `ptc-cli`, linking the server | Nothing else depends on a composition root, and the CLI reaches the server only through the API. |
| The HTTP DTOs, the OpenAPI document and a thin Rust HTTP client live in `ptc-api` (adapter support, depends only on `ptc-core`), shared by `ptc-server` and `ptc-cli` | DTOs in `ptc-server` | The CLI is an API client and cannot depend on a composition root. |

### Taken while implementing the layering (November 2026)

| Decision | Alternative rejected | Why |
| --- | --- | --- |
| ECMAScript number formatting for RFC 8785 through `ryu-js`, added to the dependencies `ptc-core` may have | Our own formatter on top of Rust's shortest float printing | Rust's `{:e}` rounds exact ties up; ECMAScript and RFC 8785 round them to even (`1424953923781206.25` → `…206.2`). A runtime test compares the core's canonical numbers with V8's on thousands of random doubles. |
| `serde_json` with `float_roundtrip` in the whole workspace | Its default float parser | The default parser can be one ULP off (`333333333.33333329`): a value going through JS and Rust would change, and its digest with it. |
| The stored format writes every JSON value in its canonical form | The JSON library's key order | The stored bytes no longer depend on a feature some dependency enables (`preserve_order`), and they are exactly the bytes a payload's digest is computed on. |
| Dynamic `import()` is refused when the script compiles | Refusing it in the module loader | There is no event loop in the sandbox: the import would never settle, and the script would end with a misleading `DeadlockError`. |
| The checks at start run on the syntax tree (swc): imported modules and names, default export | Loading the module in a throwaway isolate | Cheap and deterministic, with no script code running before the run exists. What only shows when the script runs (a top-level exception) still aborts the run with `invalid_script` at its first activation. |
| The CPU budget applies to each call into the script | A budget per activation | An activation can replay the full history: a long run must not exhaust its budget replaying it. |
| Each generator gets its own key, `SHA-256(domain ‖ seed)`; `crypto` is the ChaCha20 keystream of RFC 8439 with a zero nonce | Keying both generators with the seed | What `Math.random` reveals tells nothing about `crypto`, and the keystream is a standard one. |
| Unhandled rejections are read from deno_core's queue of rejections without a handler, drained after each microtask checkpoint | Our own `PromiseRejectCallback` | deno_core already tracks rejections, and removes those handled later in the same checkpoint (Node's semantics). |
| Errors created by the glue have no glue frames in their stack, and `fail_run` drops glue frames | Raw V8 stacks | The glue's line numbers would otherwise leak into journals and change them on every edit of the glue. |
| Each instance runs on its own thread with a single-threaded tokio runtime (for module loading), next to a watchdog thread | One thread per isolate pool | Simplest correct model. Kept at the target scale with the bounded [sticky cache](03-execution.md#sticky-execution) (owner decision above). |
| The epoch corpus lives in a tooling crate, `ptc-e2e`: blue cases must succeed, red cases must fail exactly as specified; journals are compared byte for byte and replayed from their files | Corpus inside `ptc-cli` | The corpus does not need the CLI, and the CLI's end-to-end tests reuse the same cases. |
| Values reach the script with their keys in canonical order, live and on replay | Keeping the order they arrived in | Stored values are canonical: a run would see one order live and another on replay, diverge, and be quarantined while healthy. Found by review. |
| An activation that fails for any reason other than the script (a missing blob, a store error, a replay interrupted by a limit or an internal error) keeps its lease and is retried when it expires | Releasing the lease at once; quarantining | The run picked first is the one with the oldest inbox item: released at once, it would be picked again and again and block every other run. A transient failure must not stop a healthy run for good. Found by review. After 5 failures in a row the run is quarantined (owner decision above). |
| The real `Date` stays private to the glue; only `Date.parse` and `Date.UTC` are copied | Making the logical `Date` inherit from it | Through `Object.getPrototypeOf(Date)` a script read the real clock. Found by review. |
| After a cancellation, a script that nothing can wake fails with `CancelledError`, not `DeadlockError` | `DeadlockError` | Cancelling a wait built on `setTimeout` drops the timer: the run must still end `cancelled`. Found by review. |
| Lone surrogates become U+FFFD when a value leaves the script | Refusing the value | A string cut in the middle of an emoji is common in generated code; no host string can hold half of a pair. Found by review. |


### Taken while freezing the HTTP API and the sandbox lib (November 2026)

| Decision | Alternative rejected | Why |
| --- | --- | --- |
| The sandbox lib lives in `crates/ptc-runtime/lib/`: `ptc.d.ts`, `globals.d.ts`, `fetch.d.ts`, `tsconfig.json` and `globals.json` (the declared names, the `ptc` exports, the hints for absent globals). `ptc-runtime` embeds it (`sandbox_lib`); `ptc-check` embeds the same files by path with `include_str!`. It declares the latest epoch | A copy in `ptc-check`; a crate of its own | The runtime is what the lib describes, and two adapters cannot depend on each other. Scripts are only type-checked when a run starts, and a run starts under the latest epoch. |
| The checker's ECMAScript lib is ES2024 as tsgo's granular libs, without the `sharedmemory` libs (es2017, es2020, es2024) and `es2021.weakref`, plus `esnext.temporal` | `"lib": ["es2024"]` | The sandbox removes `SharedArrayBuffer`, `Atomics`, `WeakRef` and `FinalizationRegistry`, and has `Temporal`. Checked with TypeScript 7.0.2. |
| `ToolError` has a private constructor in `ptc.d.ts`; timer callbacks take `any` arguments, as on the web | The declaration of 01 word for word; typed timer arguments | Only the glue builds a `ToolError`: `new ToolError("x")` would throw. `setTimeout(resolve, 100)` inside `new Promise` must check. |
| `fetch` is declared only with the `http` tool, as a subset of the web's: a string or `URL`; `method`, `headers`, `body`; a `Response` with its status, headers and body as text, JSON, `ArrayBuffer` or bytes. `Request`, `Response` and `Headers` are types, not globals | The full WHATWG `fetch` | It is a journaled tool call: no streams, no `AbortSignal`, redirects followed by the dispatcher. |
| API JSON: camelCase fields, snake_case enumerated values, RFC 3339 times (`jiff`), durations as scripts write them or in milliseconds. Request bodies refuse unknown fields | snake_case as in the journal; milliseconds everywhere | MCP and TypeScript are camelCase, so the SDK's generated types need no mapping. A field a model misspells is refused, not ignored. |
| Problem types are `urn:ptc:problem:<slug>`, 16 kinds (`ptc_api::ProblemType`); `invalid-script` carries `diagnostics`; an error body that is not problem+json reads as `about:blank` | URLs on a domain | No domain to own; the URN is stable and never dereferenced. |
| Outbound webhooks follow Standard Webhooks: `webhook-id`, `webhook-timestamp`, `webhook-signature: v1,<base64 HMAC-SHA256>`, secrets `whsec_…`, 5 minutes of tolerance; written with `hmac` and `sha2` | Our own header; the `standardwebhooks` crate | Every language has a verifier for that specification. The crate is unmaintained since 2024. |
| Lifecycle events add `run.cancelled`, `run.aborted` and `run.quarantined` to those of 07 | Only `run.completed`, `run.failed`, `run.suspended` | The harness must learn of every end of a run, and of a quarantine (owner decision: reported through webhooks). |
| `POST /hooks/{token}`: a JSON body is the payload, another body a string, an empty one `null`; the delivery id is the first of `Idempotency-Key`, `webhook-id`, `X-GitHub-Delivery` | One header only | Senders name their delivery ids differently. |
| Endpoints added to 07's table: `POST /v1/tasks/{token}/defer`, `GET /v1/scripts/{digest}`, `GET` and `DELETE /v1/subscriptions`, `DELETE /v1/api-keys/{id}`, `GET /v1/server`, `POST /v1/server/stop`. `fork` takes `Idempotency-Key` | — | The `deferred` state, the dashboard's source view, revocable keys and subscriptions, and the CLI's version check each need a call. A fork creates a run. |
| API keys have one of four roles: `admin` (keys of every tenant), `harness`, `worker` (host-tool tasks only), `reader` (`GET` only) | A single role | The SDK worker and the dashboard should not hold a harness's rights. |
| A payload is shown as `{digest, bytes, preview, value?}`; the value is left out above 256 KiB, except in `GET /v1/runs/{id}/entries/{idx}` | Values always inline | Run states, streams and webhooks stay small; 256 KiB is the bound of `run_script`. |
| Lists in query strings repeat their key (`label=agent:triage&label=…`), read with `serde_html_form` (axum-extra's `Query`) | Comma-separated values | Label values may hold commas. |
| The OpenAPI operations are declared in `ptc-api` by bodiless functions; their paths are `ptc_api::routes`, which the server's router uses; tests check both name the same operations, and that every route fits one axum router | Annotating the server's handlers | The document is a contract written before the server, and the CLI depends on it, never on the server. |
| The client is `reqwest` (rustls, unix sockets natively), with `sse-stream` for the event stream | hyper with `hyperlocal`; `eventsource-stream` | One client for both transports; `sse-stream` is maintained, and `rmcp` uses it. |
| A deployed server refuses stdio MCP servers unless its configuration allows them; the embedded server accepts them | Accepting any command | A stdio server is a process the tenant chooses, started by ai-ptc. |

### Taken while freezing the contracts in code (November 2026)

| Decision | Alternative rejected | Why |
| --- | --- | --- |
| A toolset's digest is the SHA-256 of its pinned toolset document v1 in canonical form, `"v": 1` included; `ptc-core` builds the same JSON, and `ptc-format` writes exactly those bytes | A digest over a core-only shape | A store can check a stored toolset against its key, as it does a blob. The e2e journals were recorded again: only `run_started.toolset` changed. |
| A tool is named `{namespace}.{name}`: its MCP server, or its host-tool group (optional); built-ins are named `http` and `webhook`; namespaces are identifiers | Free-form names with a separate namespace field | `ptc:tools` exports one namespace per server or group, and a name says where a call goes. |
| A tool's `concurrency` caps the calls of that tool in flight in one run (16 by default); the server's caps the calls in flight to it per worker (8). In flight: claimed and not expired, or deferred | A single cap per run | The harness can serialize one tool (`ask_user`: 1) without slowing the others. The store enforces both at claim, from values copied into the task. |
| A claim lasts the policy's heartbeat timeout (60 s by default, every route); host tools have no execution timeout by default, the others 5 min | A lease duration chosen by the claimer | One rule for the dispatcher and the harness's workers; host tools may last days. |
| One token per claim; a claim taken again refuses the old token; a deferred task keeps its token until answered | One token per task | A worker that lost its claim learns it at its next heartbeat, and only the live attempt reports (the tool sees the same idempotency key either way). |
| `acquire_runnable` takes the random `draw` (tenants with work ordered by id, index `draw` modulo their count); the sticky grace counts from the oldest inbox item | The store drawing at random | Stores stay deterministic and the fairness of the draw is testable. A run left by a dead worker is taken over at once. |
| Quarantine records its kind (`non_determinism`, `failures`) and the last build that retried it; `acquire_quarantined` gives each run to one worker of a new build, once | The reason alone | Giving up must write `non_determinism` or `internal`, and several workers of one build must not all retry a run. |
| Retention and payload cleanup apply each tenant's settings inside the store (`runs_to_archive(now)`, `unreferenced_payloads(now)`); an archive is a journal document in the `BlobStore`, under the SHA-256 of its bytes; `BlobStore::put` refuses bytes that do not hash to their key | A threshold computed by the caller per tenant | The tenant without a record (the embedded `local`) gets the defaults with no special case; one content-addressed store holds payloads and archives. |
| A cassette writes args, results and payloads as plain JSON values, one call or signal per line; a signal records `elapsed_ms` since the start; calls without an outcome are left out. The cassette model lives in `ptc-core`, derived from a history | `{"value": …}` wrappers; the cassette in `ptc-format` only | Cassettes are written and edited by people; mock mode in `ptc-engine` needs the model without depending on the format. |
| Observed result types are a `Shape` in `ptc-core` whose merge is commutative, associative and idempotent; the read model folds held results into it | Shapes kept by the projector | The projection is at least once and in any order; codegen reads shapes through `ReadModel::observed_shapes`. |
| Three feed consumers: `read_model`, `otel` and `webhooks` | Lifecycle webhooks sent after commit | A webhook is a projection of the journal like the others: nothing is lost on a crash. `run.quarantined` joins the lifecycle events; `run.failed` covers failed, cancelled and aborted runs. |
| Workers and claims take a `TenantScope` (all, only, except); a claim's `TaskFilter` is `All` (mock mode), `Dispatched` (MCP and built-ins) or `Host` (a tenant's host tools, by toolset and names) | Filters added per backend | Per-tenant worker pools and the host-tool long-poll need them at the port. |
| Tenant ids are 1 to 64 ASCII letters, digits, `-` and `_`; API key roles are strings the store keeps and the server interprets | Free-form tenants; roles in the engine | Tenants appear in blob paths; roles belong to the HTTP API. |
| `Toolset::from_host_tools` builds host tools with the default policy and an input schema that accepts anything, for mock mode and tests; the e2e scenarios use it | Keeping a list of names as a toolset | Every toolset is pinned; in mock mode every call goes to the mocks whatever its route. |

### Taken while implementing the tool tasks (November 2026)

| Decision | Alternative rejected | Why |
| --- | --- | --- |
| Backoff of a transient failure: `1 s × 4^(n−1)` after attempt n, at most 5 min, the upper half of it jitter drawn from `SHA-256(idempotency key # attempt)` (0.5–1 s, 2–4 s, 8–16 s, 32–64 s) | Random jitter; a fixed delay | The engine stays deterministic given its ports, and calls that failed together (an outage) come back apart. |
| Attempts count every claim, 5 at most. A transient failure at the 5th is delivered; a claim that expired a 5th time fails with `outcome_unknown`. Only a claim that expired is an interruption: a task claimed again after a retry, an approval or renewed credentials is sent | Separate counters per cause | One counter in the task, as the store keeps it; parking and approvals happen before anything is sent. |
| The engine renews the claims of the calls it dispatches (`heartbeat_tool_claims`, called by the worker loop about every second; a claim is renewed once a third of its heartbeat timeout has passed), until the tool's execution timeout | A sleep port; a claim as long as the execution timeout | The engine has no runtime of its own. A dead dispatcher still frees its call within the heartbeat timeout, and a call past its execution timeout loses its claim even if the dispatcher hangs. |
| In virtual time the driver jumps to the next retry the engine scheduled, as to a timer; the engine keeps these times in memory | A `next_tool_task` method on the store | Retries are the engine's own work. A virtual-time engine restarted during a backoff waits for another wake-up. |
| `JournalStore::tool_task(token)`: the task holding a claim's token (port addition) | Token-only use cases | `/fail {retryable}` and `/defer` carry only the token; the policy, the attempt and the run's deadline are in the task. |
| `ToolTask` and `ToolCall` carry `schema_change_approved`, set by `approve_tool_task` on a task parked by `SchemaChanged` (port addition) | Approving without telling the dispatcher | The dispatcher would see the same mismatch and park the call again forever. |
| A parked task is reported (`tool.schema_changed`, `credentials.expired`, id `{run_id}/{seq}/{attempt}`) to the tenant's subscriptions after it is parked; a failed delivery is logged | Reporting before parking; through the feed | An approval sent in response must find the task parked. Parking is not in the journal, so the feed does not see it; the run's state still shows the parked call. |
| Credentials are resolved for MCP tools only. Given at start, they are stored before the run is created, and deleted when the start creates no run (an idempotency key already used, the quota) | Resolving for every route; storing after creation | Secrets go only where the transport needs them; a worker may dispatch the first call at once. |
| The engine answers the `webhook` built-in of a live run itself: args `{"name": string}`, result `{"url": "{public_url}/hooks/{token}"}`, `unavailable` without a configured public URL. In mock mode the mocks answer it | A dispatcher adapter | Webhook tokens live in the `JournalStore`, which no adapter holds. |
| Host tools: a retryable failure is retried under `retry` with attempts left, otherwise delivered as `unavailable`; a non-retryable one as `error`. A deferral ends at the run's deadline, or earlier at the requested timeout or the tool's execution timeout from now. A token of another tenant is gone | Keeping the worker's failure kind | The API's failure carries no kind. The claim's start is not stored, so an execution timeout cannot be counted from it. |
| A result above `max_payload_bytes` fails a dispatched call with `error`, and is refused when a host worker completes (413) | Storing it | The script could not hold it within its limits; the worker can still fail the task. |
| `RoutingDispatcher { mcp, builtins, host }` sends each call to the dispatcher of its route; `Unrouted` fails the calls of a route without one (`unavailable`) | One dispatcher for every route | Composition roots plug `ptc-mcp`, `ptc-http` and mocks per route; host tools only reach a dispatcher in mock mode. |
