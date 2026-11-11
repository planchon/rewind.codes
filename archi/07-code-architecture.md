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

`JournalStore` methods (as implemented in `crates/ptc-engine/src/ports/journal.rs`):

```
put_script(script, compiled) / get_script         put_toolset / get_toolset
create_run(run, key?) -> Created { run_id, created }     # run carries its metadata
enqueue(run, completion, key?, now) -> Accepted | Duplicate | RunEnded
acquire_runnable(owner, epochs, now, lease_duration) -> Lease { run_id, fence }
acquire_run(run, owner, now, lease_duration) -> Lease   # any run that has not ended, quarantined or not
load_history(run, from)            take_inbox(lease)
commit_activation(lease, Activation { entries, consumed, tool_calls, timers, cancelled_timers, status })
release(lease)                     quarantine(lease, mismatch) / release_quarantine(run)
fire_due_timers(now)               next_timer()
expire_due_runs(now) -> ExpiredRuns { cancelled, quarantined }    next_deadline()
claim_tool_tasks(owner, now, lease_duration, limit)    complete_tool_task(task, outcome, now)
list_runs(filter)                  get_run(run)
claim_feed(consumer, owner, now)   ack_feed(items)                       (M4)
```

Stores never read a clock: every method that depends on time takes `now`.

`metadata` is the run metadata (labels, trace context), stored with the run and never
part of its events. From M4, `commit_activation` also writes one feed item per consumer
in the same transaction: the store guarantees that every committed activation is
eventually projected.

Contracts added by the November 2026 review, all covered by the conformance suite:

- `key` is an idempotency key. `create_run` with a key already used returns the existing
  run; `enqueue` with a key already seen for that run does nothing
  ([storage](06-storage-and-scale.md#idempotency-of-the-api)).
- `acquire_runnable` only returns runs whose epoch is in `epochs`, and never a
  quarantined run.
- `quarantine` releases the lease without committing anything and records the mismatch;
  `release_quarantine` makes the run runnable again
  ([execution](03-execution.md#non-determinism-and-quarantine)).
- `expire_due_runs` enqueues `cancel_requested { reason: "ttl" }`, once, for the runs past
  their deadline, as `fire_due_timers` does for timers. It enqueues nothing for a quarantined
  run: it reports it, and the engine gives it up (`run_aborted { non_determinism }`) through
  `acquire_run` and an ordinary `commit_activation`.
- A terminal commit clears the run's timers, tool tasks and inbox; nothing is enqueued for a
  terminal run any more (`Enqueued::RunEnded`).
- `claim_tool_tasks` hands out a task again once its claim expired, with its attempt
  incremented: the engine calls the tool again with the same idempotency key, up to 5
  attempts ([tools](04-tools.md#side-effects-and-dispatch)). `complete_tool_task` drops the
  outcome of a task that no longer exists.
- Only scripts that compile are stored: `start_run` refuses the others before creating a run
  ([execution](03-execution.md#starting-a-run)). `ScriptExecutor::compile` takes the toolset,
  to check what the script imports from `ptc:tools`.
- `next_timer` and `next_deadline` let the driver jump in virtual time.

Port style: traits whose methods return `impl Future<Output = …> + Send`; adapters may
implement them with `async fn`. The engine is generic over its adapters (static
dispatch); composition roots pick the concrete types. `ScriptExecutor` has an associated
`Instance: ScriptInstance` type.

## Crates

| Crate | Layer | Contents |
| --- | --- | --- |
| `ptc-core` | domain | ids (`RunId`, `Seq`, `Seed` on 256 bits, `Epoch`), `Digest`, `Payload` + canonical JSON (RFC 8785), `Script`, `Toolset`, events, `RunState`, `ReplayCursor`, history validation, `Timestamp`, durations |
| `ptc-engine` | application | ports, use cases, activation, replay, driver (`run_until_idle`, real or virtual time), projector and span derivation (M4), in-memory adapters (`MemoryJournal`, `MemoryBlobs`, `StaticDispatcher`, system and virtual clocks, system and seeded entropy), store conformance suite (feature `conformance`) |
| `ptc-format` | adapter support | serde DTOs for the stored journal, v1 (JSON values in their canonical form); golden fixture tests |
| `ptc-runtime` | adapter | deno_core isolate on a dedicated thread, deterministic globals, ops, JS glue (one version per supported epoch), how a script ends, module loader, CPU watchdog and heap limit, `V8Executor` |
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
| `ptc-e2e` | tooling | the epoch corpus: recorded scenarios (blue: must succeed, red: must fail as specified), replayed from their stored journals; `ptc-e2e replay <case>` |
| `evals` | tooling (M2.5) | eval suite driving the `ptc` CLI: tasks, fake MCP servers, reports |
| `sdk/typescript` | client (M4) | client, host-tool worker, agent tools |
| `dashboard` | client (M6) | web UI over the public HTTP API |

Crates are added when their milestone starts.

## Dependency rules

Checked by `crates/ptc-architecture-tests` from `cargo metadata`:

- Every crate of the workspace is classified in a layer; an unclassified crate fails the check.
- `ptc-core` depends on no internal crate. Its external dependencies are limited to
  `ryu-js` (ECMAScript number formatting, for RFC 8785), `serde_json`, `sha2`, `thiserror` and
  `uuid` (without features).
- `ptc-engine` depends only on `ptc-core`.
- `ptc-format` depends only on `ptc-core`.
- Adapters depend on `ptc-core` and `ptc-engine` (stores also on `ptc-format`), and never
  on each other.
- Only `ptc-cli` and `ptc-server` may depend on adapters, and the tooling crates
  (`ptc-e2e`, `evals`), which wire adapters like a composition root for tests. Nothing depends
  on a composition root or a tooling crate.
- Libraries do not use `anyhow`: each has its own error types.
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

- Script checks at start fail → `StartError::InvalidScript`, and no run is created.
- Executor start errors (top-level exception) → `run_aborted { invalid_script }`.
- Replay mismatch → nothing is committed; `journal.quarantine(lease, mismatch)`.

## Tests

| Where | What |
| --- | --- |
| `ptc-core` | Unit tests of the pure model: canonical JSON (RFC 8785 golden vectors), digests, matching, `ReplayCursor`, run state, durations |
| `ptc-format` | Round trips and golden fixtures (a stored v1 journal must always decode) |
| `ptc-engine` | Use cases with in-memory adapters and a fake executor; conformance suite for stores (idempotency keys, epochs, quarantine and run expiry; feed included from M4); span derivation; projector idempotence (applying a feed item twice changes nothing) |
| `ptc-runtime` | Sandbox behavior, driven by the engine with in-memory adapters and virtual time (`Promise.all`, time, randomness, signals, imports, callsites, cancellation, limits, the exact set of globals, RFC 8785 numbers against V8, and how a script ends: forgotten `await`, unhandled rejection, deadlock) |
| Epoch corpus (`ptc-e2e`) | `corpus/epoch-N/{blue,red}/<case>/`: a script, a `scenario.json` (input, tools, steps, expectations) and its recorded `journal.v1.json`. Each case runs live and must meet its expectations, must record the committed journal byte for byte, and the committed journal must replay with the script on a fresh executor. Blue cases must succeed; red cases must fail exactly as specified (refused start, failed, cancelled or aborted run, diverging replay, tampered journal). `PTC_E2E_RECORD=1` records the journals again |
| `ptc-store-sqlite` | The engine's conformance suite |
| `ptc-cli` | End to end: V8 + engine + SQLite + files, across processes (sleep, interrupt, resume) |
| `ptc-architecture-tests` | Dependency rules, telemetry rules included |

## Embedded CLI (M1)

`ptc` with `--dir .ptc` (SQLite database and blob files):

| Command | Does |
| --- | --- |
| `ptc run script.ts [--input …] [--wait 30s] [--ttl 30d] [--virtual-time]` | Starts a run and drives it until it completes or suspends |
| `ptc signal <run> <name> [payload]` | Enqueues a signal |
| `ptc cancel <run>` | Enqueues a cancellation |
| `ptc tick` | Fires due timers and runs ready activations once |
| `ptc worker` | Loops: activations, timers, tool tasks |
| `ptc replay <run> [--script fixed.ts]` | Replays locally and checks determinism |
| `ptc trace <run> [--entry n]` | LLM-readable trace |
| `ptc runs` | Lists runs |

Tools come from `ptc.tools.json` (static responses, `{"$error": "…"}` for failures) until
M2.
