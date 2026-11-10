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
| **Built to scale** | Postgres in production. SQLite only for the embedded engine (CLI, local dev, tests). |
| **Payloads live in S3**, except very small ones | Threshold 1 KiB by default. |
| **Mocks are recorded from real runs** | Cassettes from journals; TypeScript mock handlers as a complement. |
| **Events can come from any source** | Harness, humans, other agents, other runs, external systems through webhooks. |
| **No `fetch`, no file system** | Network only through the opt-in, journaled `http` tool. No file system at all. |
| **The agent gets several tools**, not only `run_script` | `check_script`, `test_script`, `get_run`, `list_runs`, `signal_run`, `cancel_run`, `search_tools`. |
| **Everything goes through the journal** | Replay re-executes against the journal, never restores a snapshot. |
| **Type-checking is in V1** | Generate → check → fix is the agent's main loop. |
| **`deno_core` + `deno_ast`** | Instead of raw `rusty_v8`. |
| **CLI surface for V1** | `run`, `check`, `test`, `replay`, `trace`, plus `signal`, `cancel`, `worker`. |
| **Concurrency, side effects, randomness**: handled as proposed | `seq` in emission order; tool policies and idempotency keys; seeded PRNG. |
| **OpenTelemetry for everything in the service** | Service telemetry and run telemetry, exported over OTLP. |
| **A dashboard of every run and all the code executed on the platform** (future, M6) | Built on the journal, not on an OTel backend. |
| **Stay with full replay for 0.1** | Instances are dropped when a run suspends; sticky execution stays in M5. |

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

## Technical (open to challenge)

| Decision | Alternative rejected | Why |
| --- | --- | --- |
| Durability by event-sourced replay | V8 heap snapshots | V8 can't snapshot a running isolate with pending promises, and snapshots break on every V8 upgrade. |
| Inbox + single writer per run (lease + fence) | Writing completions straight into the history | Makes history order deterministic and replay exact. |
| Commit invariant; `run_aborted` distinct from `fail_run` | Committing triggers and commands separately | Replay never sees a half-processed trigger; host failures are not replayed as script failures. |
| Ops are synchronous; promises resolved by the JS glue | Async ops on deno_core's event loop | No hidden scheduling; delivery order is fully controlled. |
| Logical time = `recorded_at` of the last delivered trigger, monotonic per run | Real time, or time recorded per call | Time moves only when the outside world speaks, identically on replay. |
| Seeded sfc32 PRNG for `Math.random` and `crypto` | Journaling each random value | No journal entries for randomness; the seed is in `run_started`. |
| `TZ=UTC` and `en-US` locale enforced; sandbox refuses to start otherwise | Host-dependent `Intl` | Identical output on every machine. |
| Signals: one FIFO mailbox per name; timeouts are durable timers cancelled on arrival | Drop signals nobody waits for | Nothing is lost if the signal arrives before the script waits. |
| Content addressing everywhere (SHA-256; canonical JSON for payloads) | Random ids for scripts and payloads | Deduplication, cacheability, and digest comparison during replay. |
| Inline threshold 1 KiB | Everything in S3 | Keeps tiny payloads one read away. Configurable. |
| Postgres + SQLite behind one port with a shared conformance suite | One backend | Scale in production, zero setup locally. |
| rusqlite (bundled) for the SQLite store | sqlx for both | Simple, synchronous, no runtime coupling for the embedded store. |
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
