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
| **Payloads live in S3**, except very small ones | Threshold 1 KiB by default. |
| **Mocks are recorded from real runs** | Cassettes from journals; TypeScript mock handlers as a complement. |
| **Events can come from any source** | Harness, humans, other agents, other runs, external systems through webhooks. |
| **No `fetch`, no file system** | Network only through the opt-in, journaled `http` tool. No file system at all. |
| **The agent gets several tools**, not only `run_script` | `check_script`, `test_script`, `get_run`, `list_runs`, `signal_run`, `cancel_run`, `retry_run`, `search_tools`. |
| **Everything goes through the journal** | Replay re-executes against the journal, never restores a snapshot. |
| **Type-checking is in V1** | Generate → check → fix is the agent's main loop. |
| **`deno_core` + `deno_ast`** | Instead of raw `rusty_v8`. |
| **CLI surface for V1** | `run`, `check`, `test`, `replay`, `fork`, `trace`, plus `signal`, `cancel`, `worker`. |
| **Concurrency, side effects, randomness**: handled as proposed | `seq` in emission order; tool policies and idempotency keys; seeded PRNG. |
| **OpenTelemetry for everything in the service** | Service telemetry and run telemetry, exported over OTLP. |
| **A dashboard of every run and all the code executed on the platform** (future, M6) | Built on the journal, not on an OTel backend. |
| **Full replay until sticky execution (M5)** | Until then, instances are dropped when a run suspends and every activation replays the history. |
| **Every run has a TTL, 30 days by default** | `expires_at` in `run_started`; at the deadline the run is cancelled through the normal path. It bounds signal waits, tool calls that never answer, quarantine, and the life of a previous epoch. |
| **The system is measured by evals** (M2.5) | Models run end to end on the embedded engine against fake MCP servers. Choices that are cheap to change are decided on the numbers. |
| **Fork comes before the dashboard** | `ptc fork` in M3 and `retry_run({ run_id, code })` as an agent tool. Fixing step 7 without redoing steps 1–6 is what the journal is for. |
| **Result types are inferred from the journal** when a server declares no `outputSchema` | Observed shapes, for type-checking and the prompt only. |
| **`run_script` returns a bounded result** | It lands in the model's context: output truncated above 256 KB, last log lines, errors with a code frame. |
| **What type-checks is what exists** | A baseline of deterministic web-standard globals; the checker's `lib` declares exactly the sandbox's globals; hints for absent ones such as `fetch`. |

### Taken after the layering (November 2026)

Answers to the questions raised once the layering was done. "Version 0.1" is ai-ptc once M1
to M6 are done.

| Decision | Notes |
| --- | --- |
| **An invalid script fails fast** | Starting a run compiles and checks the script (imports, names, default export; type-checking from M2) and refuses it before any run exists. The agent learns in the same call; it does not wait for an execution. |
| **An interrupted tool call is retried with its idempotency key** | `retry` is the default policy of every tool; `at_most_once` is an opt-out per tool in the toolset. After 5 attempts the call fails with `outcomeUnknown`. |
| **A toolset that changed during a run is reported, not failed silently** | The harness is warned and the agent or the developer decides what happens to the run (closes question 3; mechanism designed with M2). |
| **The cancellation grace is one delivery** | The script ends within the delivery of `cancel_requested`, or it is aborted (closes question 5). |
| **The CPU budget applies to each call into the script**, measured as CPU time | The watchdog moves from wall-clock time to the thread's CPU clock before M4. |
| **An activation that fails 5 times in a row quarantines the run** | With the last error as reason. Logged in M1; reported through OpenTelemetry and webhooks in M4 (closes question 16). |
| **Web-standard globals come from existing crates** | `deno_webidl`, `deno_url`, `deno_web` and the like, with their non-deterministic parts replaced (closes question 13). In general, take a maintained crate rather than rewrite one. |
| **A signal or a cancellation sent to a finished run is refused** | 409 in the API, an error in the CLI, instead of being dropped (closes question 4). |
| **The tenant lives in the run metadata**, never in the core | Closes question 7. |
| **Payload size and preview are blob metadata** | A `payloads (digest, bytes, preview)` table kept by the store (closes question 1). |
| **Webhooks: an unguessable URL and an HMAC signature** | Closes question 8. |
| **Run labels: a bounded map set at start**, exported to OpenTelemetry for allow-listed keys only | Closes question 10. |
| **TTL: a maximum per deployment** (365 days by default) | A run past its deadline ends `cancelled` with the reason `ttl`; no status of its own (closes question 15). |
| **No `LISTEN/NOTIFY`** | Wake-ups in M4 do not rely on Postgres notifications. |
| **The corpus lives in `ptc-e2e`** | A tooling crate allowed to depend on adapters. |
| **The embedded store is Turso Database** | The `turso` crate, the Rust rewrite of SQLite, rather than SQLite through rusqlite or libSQL (closes question 17). It is in beta and its multi-process access is experimental: how `ptc` commands share one `.ptc` directory is settled in the M1 principles review. The store stays behind `JournalStore` and its conformance suite, so the choice can change. |
| **`run_script` returns at most 256 KB of output** | Above it the output is truncated and the model pages through the rest (closes question 12). |
| **A quarantined run is released by hand, or retried when a new build is deployed** | A replay that fails again writes nothing, so the automatic retry is safe; a run nobody fixes ends at its TTL (closes question 14). |
| **Retention: active runs are never touched** | A finished run's journal stays in Postgres for a configurable period, then is archived to S3 and loaded on demand; the read model is kept per tenant retention. Durations and deletion stay open (question 11). |
| **Leases are renewed by a heartbeat during an activation** (M4) | The lease stays short (30 s) and the worker renews it about every 10 s while it lives, during the replay as during the execution. A long replay keeps its lease; a dead worker frees the run at most 30 s after its last renewal. The fence still refuses the commit of a worker that lost its lease. |
| **The latency of the polling path is accepted** | A script of 5 sequential calls makes about ten store round trips. Gains come later from caching (instances, histories, payloads), not from a separate short path. |
| **The embedded CLI** | Schema versioned with migrations in the binary, refusing a newer schema; `--json` on every command and one exit code per outcome; a first Ctrl-C finishes the call in flight and commits it, a second one exits at once. |

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
| Review architecture principles before each milestone | Process rule. |
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
| Inline threshold 1 KiB | Everything in S3 | Keeps tiny payloads one read away. Configurable. |
| Postgres + an embedded store behind one port with a shared conformance suite | One backend | Scale in production, zero setup locally. |
| V8 instance on a dedicated thread behind `ScriptExecutor` | Running V8 on the async runtime | V8 objects are `!Send`; the engine stays a normal async crate. |
| Ports as traits returning `impl Future + Send`; engine generic over adapters | `dyn` traits with `async_trait` | Static dispatch, no boxing; composition roots pick types. |
| Tool policies `retry` / `at_most_once`, idempotency key `{run_id}/{seq}` | Always retry | A crash mid-call must not silently duplicate a non-idempotent effect. |
| `tsc` embedded in an isolate | Requiring tsgo or Node next to ai-ptc | Single binary for harness developers. |
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

### Taken while implementing the layering (November 2026)

| Decision | Alternative rejected | Why |
| --- | --- | --- |
| ECMAScript number formatting for RFC 8785 through `ryu-js`, added to the dependencies `ptc-core` may have | Our own formatter on top of Rust's shortest float printing | Rust's `{:e}` rounds exact ties up; ECMAScript and RFC 8785 round them to even (`1424953923781206.25` → `…206.2`). A runtime test compares the core's canonical numbers with V8's on thousands of random doubles. |
| `serde_json` with `float_roundtrip` in the whole workspace | Its default float parser | The default parser can be one ULP off (`333333333.33333329`): a value going through JS and Rust would change, and its digest with it. |
| The stored format writes every JSON value in its canonical form | The JSON library's key order | The stored bytes no longer depend on a feature some dependency enables (`preserve_order`), and they are exactly the bytes a payload's digest is computed on. |
| Dynamic `import()` is refused when the script compiles | Refusing it in the module loader | There is no event loop in the sandbox: the import would never settle, and the script would end with a misleading `DeadlockError`. |
| The checks at start run on the syntax tree (swc): imported modules and names, default export | Loading the module in a throwaway isolate | Cheap and deterministic, with no script code running before the run exists. What only shows when the script runs (a top-level exception) still aborts the run with `invalid_script` at its first activation. |
| The CPU budget applies to each call into the script | A budget per activation | Every activation replays the full history: a long run must not exhaust its budget replaying it. |
| Each generator gets its own key, `SHA-256(domain ‖ seed)`; `crypto` is the ChaCha20 keystream of RFC 8439 with a zero nonce | Keying both generators with the seed | What `Math.random` reveals tells nothing about `crypto`, and the keystream is a standard one. |
| Unhandled rejections are read from deno_core's queue of rejections without a handler, drained after each microtask checkpoint | Our own `PromiseRejectCallback` | deno_core already tracks rejections, and removes those handled later in the same checkpoint (Node's semantics). |
| Errors created by the glue have no glue frames in their stack, and `fail_run` drops glue frames | Raw V8 stacks | The glue's line numbers would otherwise leak into journals and change them on every edit of the glue. |
| Each instance runs on its own thread with a single-threaded tokio runtime (for module loading), next to a watchdog thread | One thread per isolate pool | Simplest correct model to start with; open question 2 stays open. |
| The epoch corpus lives in a tooling crate, `ptc-e2e`: blue cases must succeed, red cases must fail exactly as specified; journals are compared byte for byte and replayed from their files | Corpus inside `ptc-cli` (M1) | The corpus can exist before the CLI; the CLI's end-to-end tests will reuse the same cases. |
| Values reach the script with their keys in canonical order, live and on replay | Keeping the order they arrived in | Stored values are canonical: a run would see one order live and another on replay, diverge, and be quarantined while healthy. Found by review. |
| An activation that fails for any reason other than the script (a missing blob, a store error, a replay interrupted by a limit or an internal error) keeps its lease and is retried when it expires | Releasing the lease at once; quarantining | The run picked first is the one with the oldest inbox item: released at once, it would be picked again and again and block every other run. A transient failure must not stop a healthy run for good. Found by review. After 5 failures in a row the run is quarantined (owner decision above). |
| The real `Date` stays private to the glue; only `Date.parse` and `Date.UTC` are copied | Making the logical `Date` inherit from it | Through `Object.getPrototypeOf(Date)` a script read the real clock. Found by review. |
| After a cancellation, a script that nothing can wake fails with `CancelledError`, not `DeadlockError` | `DeadlockError` | Cancelling a wait built on `setTimeout` drops the timer: the run must still end `cancelled`. Found by review. |
| Lone surrogates become U+FFFD when a value leaves the script | Refusing the value | A string cut in the middle of an emoji is common in generated code; no host string can hold half of a pair. Found by review. |

