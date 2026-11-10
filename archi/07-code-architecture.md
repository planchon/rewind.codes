# 7. Code architecture

Approved for 0.1. Hexagonal layering: a pure domain, an application layer that declares
ports, adapters that implement them, and composition roots that wire everything.

## Layers

```
 entry points   ptc-cli (embedded)                    ptc-server (HTTP API, workers)
                    │  composition roots: the only crates that know every adapter
 ───────────────────┼─────────────────────────────────────────────────────────────────────
 adapters       ptc-runtime   ptc-store-sqlite   ptc-store-postgres   ptc-blob   ptc-mcp   ptc-check   ptc-otel
                (V8)          └──────── ptc-format (stored journal, v1) ───────┘
                    │  implement the ports
 ───────────────────┼─────────────────────────────────────────────────────────────────────
 application    ptc-engine     use cases + ports (traits) + in-memory adapters
                    │  uses the model
 domain         ptc-core       pure model + pure logic
```

Dependencies only point down. Adapters never depend on each other.

## Principles

1. **`ptc-core` holds the domain and nothing else.** Types and pure functions: events,
   journal, determinism check (`ReplayCursor`), run state, payload identity, durations,
   script and toolset model. No I/O, no async, no V8, **no serde**. It knows nothing about
   storage, transport, or where time and ids come from.

2. **Ports live in `ptc-engine`, and adapters implement them.** The engine holds the use
   cases (start, activate, signal, cancel, replay, project, fork) and declares what it needs from
   the outside world as traits. It never names Postgres, S3, V8, MCP or OpenTelemetry.

3. **Ports follow the domain's invariants, not the tables.** "A trigger is committed
   atomically with everything it caused" is one port method, `commit_activation`; the
   engine doesn't coordinate four inserts. Leases and fencing are part of the port
   contract. Every store adapter passes the same conformance suite.

4. **Each boundary owns its representation.** Domain types are converted at the edge and
   never leak through.

   | Boundary | Owner | Why it is separate |
   | --- | --- | --- |
   | Stored journal (and cassettes) | `ptc-format`, versioned (`"v": 1`) | Runs stay paused for days across deploys. The stored form must stay stable while domain types change. Migrations are explicit. |
   | HTTP / SDK | DTOs in `ptc-server` | Public contract with its own versioning. |
   | JS ↔ Rust | `ptc-runtime` | Internal to the sandbox; shaped for the glue code. |

5. **Core knows a payload's identity, not where it lives.** A payload is a JSON value
   identified by the digest of its canonical form; an event holds the value or only the
   digest. Inline or S3, key, size and preview are the adapters' business.

6. **Non-determinism comes in only through ports.** Real time, ids, seeds, tool results
   and signals all come from ports. Core and runtime are deterministic, and the engine is
   deterministic given its ports. So the engine with in-memory adapters and a virtual
   clock is both mock mode and the test harness.

7. **The runtime only executes.** `ptc-runtime` takes a script and a start, receives
   deliveries, and returns the emitted events. It knows nothing about history, replay,
   storage or dispatch. `replay()` belongs to the engine. V8 objects can't change
   threads, so instances run on dedicated threads behind the `ScriptExecutor` port; the
   engine never handles a V8 type.

8. **Errors don't cross boundaries raw.** Each crate has its own error type. Adapters
   convert rusqlite, sqlx, S3 and deno errors into port errors. No `anyhow` in libraries.

9. **Tooling enforces the boundaries, not discipline.** A test crate reads
   `cargo metadata` and asserts the dependency rules below.

10. **The journal is the record; telemetry and dashboards are projections.** What a
    script did (OTel spans, the read model behind the dashboard, SSE) is projected from
    committed entries, never emitted from the isolate, from the runtime, or during
    replay. The platform's own behavior goes through the `tracing` facade in the engine
    and the adapters. OpenTelemetry lives only in `ptc-otel`, installed by the
    composition roots. `ptc-core` has no telemetry. Details in
    [05](05-testing-and-observability.md#observability).

## Ports

All ports are declared in `ptc-engine`.

| Port | Responsibility | Adapters |
| --- | --- | --- |
| `JournalStore` | Scripts and toolsets by digest; runs, history, inbox, timers, tool tasks; leases and fencing; `commit_activation` | memory (engine), `ptc-store-sqlite`, `ptc-store-postgres` |
| `BlobStore` | Payload values by digest | memory (engine), `ptc-blob` (files, S3) |
| `ScriptExecutor` / `ScriptInstance` | Start a script, deliver completions, report emitted events and the outcome | `ptc-runtime` (V8); a fake in engine tests |
| `ToolDispatcher` | Execute a tool call with its idempotency key | static responses (engine, CLI), `ptc-mcp`, host-tool long-poll (server) |
| `TypeChecker` (M2) | Type-check a script against a toolset's declarations | `ptc-check` |
| `ReadModel` (M4) | Projections for questions across runs: apply committed entries idempotently; search runs, scripts and tool calls; tool stats | memory (engine), `ptc-store-postgres` |
| `SpanExporter` (M4) | Export spans derived from committed entries | memory (engine tests), `ptc-otel` |
| `Clock` | Current time | system clock, virtual clock |
| `Entropy` | Run ids and seeds | system (uuid v7 / random), deterministic (tests) |

`JournalStore` methods (0.1):

```
put_script / get_script            put_toolset / get_toolset
create_run(run, metadata)          enqueue(run, completion)
acquire_runnable(owner, now, lease_duration) -> Lease { run_id, fence }
load_history(run, from)            take_inbox(lease)
commit_activation(lease, Activation { entries, consumed, tool_calls, timers, cancelled_timers, status })
release(lease)
fire_due_timers(now)               next_timer()
claim_tool_tasks(owner, now)       complete_tool_task(task, outcome)
list_runs(filter)                  get_run(run)
claim_feed(consumer, owner, now)   ack_feed(items)                       (M4)
```

`metadata` is the run metadata (labels, trace context), stored with the run and never
part of its events. From M4, `commit_activation` also writes one feed item per consumer
in the same transaction: the store guarantees that every committed activation is
eventually projected.

Port style: traits whose methods return `impl Future<Output = …> + Send`; adapters may
implement them with `async fn`. The engine is generic over its adapters (static
dispatch); composition roots pick the concrete types. `ScriptExecutor` has an associated
`Instance: ScriptInstance` type.

## Crates

| Crate | Layer | Contents |
| --- | --- | --- |
| `ptc-core` | domain | ids (`RunId`, `Seq`, `Seed`), `Digest`, `Payload` + canonical JSON, `Script`, `Toolset`, events, `RunState`, `ReplayCursor`, `Timestamp`, durations |
| `ptc-engine` | application | ports, use cases, driver (`run_until_idle`, real or virtual time), projector and span derivation (M4), in-memory adapters, store conformance suite (feature `conformance`) |
| `ptc-format` | adapter support | serde DTOs for the stored journal, v1; golden fixture tests |
| `ptc-runtime` | adapter | deno_core isolate, deterministic globals, ops, JS glue, module loader, `V8Executor` |
| `ptc-store-sqlite` | adapter | embedded `JournalStore` (rusqlite, bundled) |
| `ptc-store-postgres` | adapter (M4) | production `JournalStore` (`FOR UPDATE SKIP LOCKED`) |
| `ptc-blob` | adapter | `BlobStore` on local files, then S3 (M4) |
| `ptc-mcp` | adapter (M2) | MCP client: discovery and dispatch |
| `ptc-codegen` | adapter support (M2) | JSON Schema / MCP tool lists → `ptc:tools`, `.d.ts`, prompt rendering |
| `ptc-check` | adapter (M2) | `tsc` in an isolate |
| `ptc-otel` | adapter (M4) | OpenTelemetry SDK and OTLP export: `SpanExporter`, and the `tracing` subscriber the composition roots install |
| `ptc-cli` | composition root | the `ptc` binary: embedded engine (SQLite, files, V8) or remote client |
| `ptc-server` | composition root (M4) | HTTP API (axum), SSE, webhooks, workers |
| `ptc-architecture-tests` | tooling | dependency rules |
| `sdk/typescript` | client (M4) | client, host-tool worker, agent tools |
| `dashboard` | client (M6) | web UI over the public HTTP API |

Crates are added when their milestone starts.

## Dependency rules

Checked by `crates/ptc-architecture-tests` from `cargo metadata`:

- `ptc-core` depends on no internal crate. Its external dependencies are limited to
  `serde_json`, `sha2`, `thiserror` and `uuid` (without features).
- `ptc-engine` depends only on `ptc-core`.
- `ptc-format` depends only on `ptc-core`.
- Adapters depend on `ptc-core` and `ptc-engine` (stores also on `ptc-format`), and never
  on each other.
- Only `ptc-cli` and `ptc-server` may depend on adapters.
- Telemetry: `ptc-core` has no telemetry dependency (`tracing` included). The `tracing`
  facade is allowed in `ptc-engine` and the adapters. `opentelemetry*` crates are allowed
  only in `ptc-otel`.

## Activation, in the engine

```
lease ← journal.acquire_runnable(owner, clock.now(), lease_duration)
history ← journal.load_history(run)
instance ← cache[run] if its history length matches, else executor.start + ReplayCursor over history
for item in journal.take_inbox(lease):
    recorded_at ← max(clock.now(), last recorded_at)
    if run is terminal: drop item (completion for an ended run)
    emitted ← instance.deliver(item, recorded_at)     # payloads loaded from BlobStore when needed
    entries += [item] + emitted
offload payloads > 1 KiB to BlobStore; keep the digest in the entry
effects ← tool tasks, timers, cancelled timers derived from the new commands
journal.commit_activation(lease, Activation { entries, consumed, effects, status })   # + feed items (M4)
```

The activation itself is a `tracing` span (service telemetry), with the run's trace id
and span id `H(run, fence)` recorded as fields. Nothing about the script is emitted during
replay.

- Executor start errors → `run_aborted { invalid_script }`.
- Replay mismatch → `run_aborted { non_determinism }`.

## Tests

| Where | What |
| --- | --- |
| `ptc-core` | Unit tests of the pure model: canonical JSON, digests, matching, `ReplayCursor`, run state, durations |
| `ptc-format` | Round trips and golden fixtures (a stored v1 journal must always decode) |
| `ptc-engine` | Use cases with in-memory adapters and a fake executor; conformance suite for stores (feed included from M4); span derivation; projector idempotence (applying a feed item twice changes nothing) |
| `ptc-runtime` | Sandbox behavior with a test-support driver (`Promise.all`, time, randomness, signals, imports, callsites, cancellation) |
| `ptc-store-sqlite` | The engine's conformance suite |
| `ptc-cli` | End to end: V8 + engine + SQLite + files, across processes (sleep, interrupt, resume) |
| `ptc-architecture-tests` | Dependency rules, telemetry rules included |

## Embedded CLI (M1)

`ptc` with `--dir .ptc` (SQLite database and blob files):

| Command | Does |
| --- | --- |
| `ptc run script.ts [--input …] [--wait 30s] [--virtual-time]` | Starts a run and drives it until it completes or suspends |
| `ptc signal <run> <name> [payload]` | Enqueues a signal |
| `ptc cancel <run>` | Enqueues a cancellation |
| `ptc tick` | Fires due timers and runs ready activations once |
| `ptc worker` | Loops: activations, timers, tool tasks |
| `ptc replay <run> [--script fixed.ts]` | Replays locally and checks determinism |
| `ptc trace <run> [--entry n]` | LLM-readable trace |
| `ptc runs` | Lists runs |

Tools come from `ptc.tools.json` (static responses, `{"$error": "…"}` for failures) until
M2.
