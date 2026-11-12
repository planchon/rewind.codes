# 7. Code architecture

Approved for 0.1. Hexagonal layering: a pure domain, an application layer that declares
ports, adapters that implement them, and composition roots that wire everything.

## Layers

```
 entry points   ptc-cli (API client, local commands)      ptc-server (HTTP API, workers, embedded mode)
                    │  composition roots: the only crates that know every adapter
                    └───────────── ptc-api (DTOs, OpenAPI, HTTP client) ─────────────┘
 ───────────────────┼─────────────────────────────────────────────────────────────────────
 adapters       ptc-runtime   ptc-store-turso   ptc-store-postgres   ptc-blob   ptc-mcp   ptc-check   ptc-otel
                (V8)          └──────── ptc-format (stored format, v1) ────────┘
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
   contract. Every adapter of a port passes the same conformance suite.

4. **Each boundary owns its representation.** Domain types are converted at the edge and
   never leak through.

   | Boundary | Owner | Why it is separate |
   | --- | --- | --- |
   | Stored format: events, journal documents, cassettes, pinned toolsets | `ptc-format`, versioned (`"v": 1`, [02](02-journal.md#stored-format-v1)) | Runs stay paused for days across deploys. The stored form must stay stable while domain types change. Migrations are explicit. |
   | HTTP / SDK | DTOs in `ptc-api` | Public contract with its own versioning (`/v1`). Shared by the server and the CLI. |
   | JS ↔ Rust | `ptc-runtime` | Internal to the sandbox; shaped for the glue code. |

5. **Core knows a payload's identity, not where it lives.** A payload is a JSON value
   identified by the digest of its canonical form; an event holds the value or only the
   digest. Inline, in the `payloads` table or in S3, key, size and preview are the
   adapters' business.

6. **Non-determinism comes in only through ports.** Real time, ids, seeds, tool results
   and signals all come from ports. Core and runtime are deterministic, and the engine is
   deterministic given its ports. So the engine with in-memory adapters and a virtual
   clock is both mock mode and the test harness.

7. **The runtime only executes.** `ptc-runtime` takes a script and a start, receives
   deliveries, and returns the emitted events. It knows nothing about history, replay,
   storage or dispatch. `replay()` belongs to the engine, and so does the instance cache.
   V8 objects can't change threads, so instances run on dedicated threads behind the
   `ScriptExecutor` port; the engine never handles a V8 type.

8. **Errors don't cross boundaries raw.** Each crate has its own error type. Adapters
   convert turso, sqlx, S3 and deno errors into port errors. No `anyhow` in libraries.

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

All ports are declared in `ptc-engine` (`crates/ptc-engine/src/ports/`), each with an in-memory
adapter in `ptc-engine` that is the reference implementation of its contract. The ports with
state, `JournalStore`, `BlobStore`, `ReadModel` and `SecretStore`, and `SpanExporter` have a
conformance suite in `ptc-engine` (feature `conformance`, modules `conformance::journal`,
`::blobs`, `::read_model`, `::secrets`, `::spans`), and every adapter of the port passes it.
`SpanExporter` has no read side: its suite reads the backend through a collector the adapter's
test provides. The contracts of the other ports are not observable through the port alone (an
HMAC signature, the egress rules): their adapters test them.

| Port | Responsibility | Adapters |
| --- | --- | --- |
| `JournalStore` | Scripts and toolsets by digest; runs, history, inbox, timers, tool tasks; leases, heartbeats and fencing; `commit_activation`; failure counts and quarantine; the feed; webhook tokens; tenants and API keys; sticky affinity; payload metadata and references; retention | memory (engine), `ptc-store-turso`, `ptc-store-postgres` |
| `BlobStore` | Large payload values by digest, per tenant; deletion for cleanup | memory (engine), `ptc-blob` (files, S3) |
| `ScriptExecutor` / `ScriptInstance` | Start a script, deliver completions, report emitted events and the outcome | `ptc-runtime` (V8); a fake in engine tests |
| `ToolDispatcher` | Execute a tool call with its idempotency key and credentials | static responses and cassettes (engine, mock mode); TypeScript mocks (`ptc-runtime`, in a separate isolate); `ptc-mcp` (MCP servers, built-ins `http` and `webhook`) |
| `TypeChecker` | Type-check a script against the sandbox lib and a toolset's declarations | static (engine), `ptc-check` (tsgo) |
| `ReadModel` | Projections for questions across runs | memory (engine), `ptc-store-turso`, `ptc-store-postgres` |
| `SpanExporter` | Export spans derived from committed entries | memory (engine), `ptc-otel` (OpenTelemetry) |
| `SecretStore` | Credentials per toolset and per run, encrypted at rest | memory (engine), `ptc-store-turso`, `ptc-store-postgres` |
| `WebhookSender` | Lifecycle events to the harness's subscriptions | memory (engine), HTTP (`ptc-server`) |
| `Clock` | Current time | system clock, virtual clock |
| `Entropy` | Run ids and seeds | system (uuid v7 / random), deterministic (tests) |

Host tools have no dispatcher: their tasks stay in the `JournalStore`, and the harness's
SDK worker claims them through `GET /v1/tasks` ([routes](04-tools.md#routes)).

### `JournalStore`

Methods (declared in `crates/ptc-engine/src/ports/journal/mod.rs`):

```
put_script(tenant, script, compiled) / get_script(tenant, digest)
put_toolset(tenant, toolset, declarations) / get_toolset(tenant, digest)
create_run(NewRun { run: (tenant, id), started, metadata, created_at }, key?) -> Created { run, created } | QuotaExceeded
enqueue(run, completion, key?, now) -> Accepted | Duplicate | RunEnded
get_run(run)                       list_runs(RunFilter { tenant, status, labels, quarantined, after, limit })
acquire_runnable(AcquireRunnable { owner, epochs, tenants, now, lease_duration, sticky_grace, draw }) -> Lease { run, fence }
acquire_run(run, owner, now, lease_duration) -> Lease   # any run that has not ended, quarantined or not
acquire_quarantined(owner, build, epochs, now, lease_duration) -> Lease   # each quarantined run once per build
renew(lease, now, lease_duration)
load_history(run, from)            take_inbox(lease)
commit_activation(lease, Activation { entries, consumed, payloads, tool_calls, timers, cancelled_timers, status }, now)
release(lease)                     forget_sticky(run, owner)
record_failure(lease, error, now) -> Failures { count, quarantined }
quarantine(lease, mismatch, now)   release_quarantine(run)
fire_due_timers(now, limit)        next_timer()
expire_due_runs(now, limit) -> ExpiredRuns { cancelled, quarantined }    next_deadline()
claim_tool_tasks(owner, TaskFilter::{All | Dispatched { tenants } | Host { tenant, toolset, tools }}, now, limit)
    -> [ClaimedTask { task, token, previous_claim_expired }]
heartbeat_tool_task(token, now)    retry_tool_task(token, next_attempt_at)    next_tool_attempt(after)
defer_tool_task(token, until)      park_tool_task(token, reason)
complete_tool_task(token, outcome, now)    expire_deferred_tool_tasks(now, limit)
approve_tool_task(task, now) / reject_tool_task(task, failure, now)    resume_parked(Run(run) | Toolset { tenant, toolset }, now)
tool_tasks(run)                    tool_task(token)
create_webhook_token(run, name) -> token    resolve_webhook_token(token) -> { run, name }
put_subscription(subscription)     subscriptions(tenant)    delete_subscription(tenant, id)
get_payload(tenant, digest) -> { digest, size, preview, value? }
unreferenced_payloads(now, limit) -> [(tenant, digest)]    forget_payloads([(tenant, digest)], now)
runs_to_archive(now, limit)        archive_run(run, document, now)
runs_to_delete(now, limit)         delete_run(run, now)
put_tenant(tenant) / get_tenant(id) / list_tenants()
put_api_key(key) / find_api_key(hash) / revoke_api_key(hash, now)
claim_feed(consumer, owner, now, lease_duration, limit)    ack_feed(ids)
```

Stores never read a clock: every method that depends on time takes `now`, and the random draw
of `acquire_runnable` is passed in too. Tokens (tool tasks, webhooks) are the only values a store
draws from a CSPRNG. Every method is scoped by the tenant: a run (`RunRef { tenant, id }`), a
script, a toolset, a payload or a task of one tenant is never visible to another. Run ids are
unique across tenants.

`metadata` is the run metadata (labels, trace context, the parent of a fork and its `at`), stored with
the run and never part of its events. `commit_activation` also writes one feed item per
consumer (`read_model`, `otel`, `webhooks`) in the same transaction: the store guarantees that
every committed activation is eventually projected.

Contracts, all covered by the conformance suite:

- `key` is an idempotency key. `create_run` with a key already used in the tenant returns the
  existing run; `enqueue` with a key already seen for that run does nothing
  ([storage](06-storage-and-scale.md#idempotency-of-the-api)).
- `create_run` returns `QuotaExceeded` when the tenant already has its maximum of active runs
  ([tenancy](06-storage-and-scale.md#tenancy-and-authentication)); a retried request with its key
  still returns its run.
- `acquire_runnable` only returns runs whose epoch is in `epochs` and whose tenant is in
  `tenants`, and never a quarantined run. It draws a tenant among those with runnable work
  (ordered by id, the one at `draw` modulo their number), then takes that tenant's oldest
  runnable run (the oldest inbox item first).
- Sticky affinity: `commit_activation` records the committing worker as the run's sticky
  worker. `acquire_runnable` gives a runnable run to its sticky worker first, and to the other
  workers only once its oldest inbox item has waited `sticky_grace` (5 s). `forget_sticky` drops
  the priority when the worker evicts the instance
  ([sticky execution](03-execution.md#sticky-execution)).
- `renew` extends a lease whose fence still matches. A worker calls it about every 10 s while
  it holds the run ([leases](06-storage-and-scale.md#leases-fencing-workers)).
- `record_failure` counts the run's activations that failed in a row and keeps the lease. The
  fifth quarantines the run with that error as its reason (kind `failures`) and releases the
  lease. A successful `commit_activation` and `release_quarantine` reset the count
  ([activations](03-execution.md#activations)).
- `quarantine` releases the lease without committing anything and records the mismatch (kind
  `non_determinism`); `release_quarantine` makes the run runnable again; `acquire_quarantined`
  hands each quarantined run of the given epochs to one worker of a new build, once
  ([execution](03-execution.md#non-determinism-and-quarantine)).
- `expire_due_runs` enqueues `cancel_requested { reason: "ttl" }`, once, for the runs past
  their deadline, as `fire_due_timers` does for timers. It enqueues nothing for a quarantined
  run: it reports it, and the engine gives it up (`run_aborted { non_determinism }`, or
  `{ internal }` after repeated failures) through `acquire_run` and an ordinary
  `commit_activation`.
- A terminal commit clears the run's timers, tool tasks, inbox, webhook tokens and quarantine;
  nothing is enqueued for a terminal run any more (`Enqueued::RunEnded`).
- Tool tasks follow the [lifecycle](04-tools.md#tool-task-lifecycle). A new task starts in
  `approval` when its policy asks for it, else `ready`. `claim_tool_tasks` only hands out
  `ready` tasks whose `next_attempt_at` has passed, and `claimed` tasks whose claim expired
  (attempt + 1, `previous_claim_expired`), within the concurrency limits: the policy's calls of
  the same tool in flight per run, and the server's calls in flight per worker for an MCP tool;
  a claimed task not expired and a deferred task are in flight. A claim lasts the policy's
  heartbeat timeout and has its own token: once the task is claimed again, the old token is
  refused; `tool_task` finds the task a live token holds. `complete_tool_task` writes the outcome
  to the run's inbox, and drops the outcome of a task that no longer exists. Approving a task
  parked by `schema_changed` marks it `schema_change_approved`. `resume_parked` makes ready again
  the tasks parked by `credentials_expired`, once the run's or the toolset's credentials are
  renewed.
  `expire_deferred_tool_tasks` ends the deferred tasks whose deadline passed, with a `timeout`
  failure. `next_tool_attempt` returns the earliest `next_attempt_at` later than `after` of the
  `ready` tasks, across tenants: the next retry after a backoff, which virtual time jumps to.
- A webhook token is unguessable, the same for a run and a signal name, and maps to one run and
  one signal name. A token whose run has ended no longer resolves.
- `commit_activation` writes, in the same transaction, the `payloads` rows of the payloads
  that left the events (digest, size, preview, and the value up to 64 KiB) and the run's
  `payload_refs` ([payloads](06-storage-and-scale.md#payloads)). `delete_run` starts the grace
  of the payloads it leaves unreferenced; `unreferenced_payloads` and `forget_payloads` apply
  each tenant's grace.
- `runs_to_archive` lists the finished runs past their tenant's journal retention;
  `archive_run` removes their history once their journal document is in the `BlobStore`, and
  keeps the record and the payload references; `runs_to_delete` lists the archives past their
  retention; `delete_run` erases a run, its feed items and its payload references
  ([retention](06-storage-and-scale.md#retention-and-deletion)).
- Only scripts that compile are stored: `start_run` refuses the others before creating a run
  ([execution](03-execution.md#starting-a-run)). `ScriptExecutor::compile` takes the toolset,
  to check what the script imports from `ptc:tools`.
- `next_timer` and `next_deadline` let the driver jump in virtual time.

### The other ports

- **`BlobStore`**: `put(tenant, digest, bytes)`, `get(tenant, digest)`, `delete(tenant,
  digest)`. Immutable bytes keyed by their SHA-256: payload values above the table tier (their
  canonical JSON, so the key is the payload's digest) and the journal documents of archived
  runs. `put` refuses bytes that do not hash to the digest; a second `put` changes nothing.
  `get` checks the digest of what it reads. `delete` serves cleanup.
- **`ScriptExecutor` / `ScriptInstance`**: `compile`, `start`, `deliver`, and `memory_usage`
  (the memory an instance holds, which bounds the instance cache). The executor knows nothing
  about the instance cache, which lives in the engine.
- **`ToolDispatcher`**: `dispatch(call, credentials) -> Dispatched`. The call carries the run,
  the `seq`, the pinned tool and its server, the args, the idempotency key, the attempt, whether
  the harness approved a schema change (then the call is sent without checking the schema
  again), the claim's token and the trace context. `Dispatched` is one of: `Result` and `Failed` (the
  outcome); `Transient`, after which the engine puts the task back to `ready` with backoff, or
  fails it under `at_most_once`; `Deferred`, the handler answering later through the claim's
  token; `Park` with a reason (schema changed at dispatch, credentials refused). The dispatcher
  never writes to the journal.
- **`TypeChecker`**: `check(script, declarations) -> diagnostics`. Deterministic.
  Diagnostics are written for a model, with a location in `script.ts`, a severity, their source
  (a TypeScript code or one of the checker's own rules), a hint and a code frame
  ([type-checking](04-tools.md#type-checking)).
- **`ReadModel`**: `apply(run, entries)`, idempotent and order-independent, keyed by `(run,
  index)`. `search_runs(search)`, `scripts(filter)`, `tool_stats(filter)`,
  `observed_shapes(tenant, tools)`. `purge(ended_before, tenants)` and `delete_run(run)` serve
  [retention](06-storage-and-scale.md#retention-and-deletion).
- **`SpanExporter`**: `export(spans)`, `export_logs(logs)`. Spans have deterministic ids, so
  exporting one twice gives the same span.
- **`SecretStore`**: `put(tenant, owner, credentials)`, where the owner is a toolset or a
  run; `resolve(tenant, run, toolset)`, which returns the run's credentials if it has some,
  else the toolset's, never merged; `delete(tenant, owner)`. Credentials are headers and
  environment variables per MCP server. Envelope encryption: each tenant has a data key, itself
  encrypted by a master key from the deployment's configuration. Secrets leave the store only
  toward the dispatcher ([credentials](04-tools.md#credentials)).
- **`WebhookSender`**: `send(subscription, event)`. Each delivery is signed with HMAC, and
  retried when it fails. Events are `run.completed`, `run.failed` (failed, cancelled or
  aborted), `run.suspended`, `run.quarantined`, `tool.schema_changed` and
  `credentials.expired`, with a deterministic id for receivers to deduplicate. Subscriptions
  are kept by the `JournalStore`.
- **`Clock`** and **`Entropy`**: unchanged.

Port style: traits whose methods return `impl Future<Output = …> + Send`; adapters may
implement them with `async fn`. The engine is generic over its adapters (static
dispatch); composition roots pick the concrete types. `ScriptExecutor` has an associated
`Instance: ScriptInstance` type.

## Crates

| Crate | Layer | Contents |
| --- | --- | --- |
| `ptc-core` | domain | ids (`RunId`, `Seq`, `Seed` on 256 bits, `Epoch`), `Digest`, `Payload` + canonical JSON (RFC 8785), `Script`, the pinned `Toolset` (servers, tools, routes, schemas, policies), events, `RunState`, `ReplayCursor`, history validation, `Cassette`, observed `Shape`, `Timestamp`, durations |
| `ptc-engine` | application | ports, use cases, activation, instance cache, replay, fork, driver (`run_until_idle`, real or virtual time), projector and span derivation, in-memory adapters (`MemoryJournal`, `MemoryBlobs`, `MemoryReadModel`, `MemorySecrets`, `MemorySpanExporter`, `MemoryWebhooks`, `StaticDispatcher`, `StaticTypeChecker`, system and virtual clocks, system and seeded entropy), the conformance suites of the ports with state and of `SpanExporter` (feature `conformance`) |
| `ptc-format` | adapter support | serde DTOs for the stored format v1 (events, journal documents, cassettes, pinned toolsets; JSON values in their canonical form); golden fixture tests |
| `ptc-api` | adapter support | the HTTP DTOs (serde, utoipa), the OpenAPI document generated from them, and a thin Rust HTTP client; shared by `ptc-server` and `ptc-cli` |
| `ptc-runtime` | adapter | deno_core isolate on a dedicated thread, started from a V8 startup snapshot, deterministic globals, ops, JS glue (one version per supported epoch), how a script ends, module loader, CPU watchdog and heap limit, `V8Executor`; the TypeScript mock dispatcher |
| `ptc-store-turso` | adapter | embedded `JournalStore`, `ReadModel` and `SecretStore` (Turso Database, the `turso` crate) |
| `ptc-store-postgres` | adapter | production `JournalStore` (`FOR UPDATE SKIP LOCKED`), `ReadModel` and `SecretStore` |
| `ptc-blob` | adapter | `BlobStore` on local files and S3 |
| `ptc-mcp` | adapter | `ToolDispatcher` outside mock mode: MCP client (discovery, dispatch, credentials in the transport) and the built-ins `http` (with its egress checks) and `webhook` |
| `ptc-codegen` | adapter support | our JSON Schema → TypeScript converter; pinned toolsets → `ptc:tools`, `ptc-tools.d.ts`, prompt rendering |
| `ptc-check` | adapter | type-checking with tsgo, driven as a CLI and embedded in the `ptc` binary; the checker's own rules on the swc syntax tree |
| `ptc-otel` | adapter | OpenTelemetry SDK and OTLP export (gRPC, HTTP): `SpanExporter`, and the `tracing` subscriber the composition roots install, which joins service spans to a run's trace through their fields `trace_id`, `span_id`, `parent_span_id` and `traceparent`, and records the measurements of the engine and the adapters as metrics |
| `ptc-cli` | composition root | the client commands of the `ptc` binary: a client of the API `/v1` (embedded or remote server); local commands `check`, `test`, replay of an exported journal |
| `ptc-server` | composition root | `ptc server`: HTTP API (axum), SSE, webhooks, host-tool tasks, workers, scheduler, projector; roles `api` and `worker`; embedded mode; serves the dashboard |
| `ptc` | entry point | the `ptc` binary: hands `ptc server …` to `ptc-server` and every other command to `ptc-cli`, nothing else |
| `ptc-architecture-tests` | tooling | dependency rules |
| `ptc-e2e` | tooling | the epoch corpus: recorded scenarios (blue: must succeed, red: must fail as specified), replayed from their stored journals; `ptc-e2e replay <case>` |
| `evals` | tooling | eval suite driving the `ptc` CLI: tasks, fake MCP servers, reports |
| `sdk/typescript` | client | client (types generated from the OpenAPI document), host-tool worker, agent tools |
| `dashboard` | client | React single-page application over the public HTTP API, built to `dashboard/dist/` ([below](#dashboard)) |

One `ptc` binary holds both composition roots. The `ptc` crate is that binary: it depends on
`ptc-cli` and `ptc-server` and only dispatches the command line. The CLI's code never calls
the server's: it reaches it only through the API, and starts the embedded server by running
`ptc server --embedded` as a separate process.

## Dependency rules

Checked by `crates/ptc-architecture-tests` from `cargo metadata`:

- Every crate of the workspace is classified in a layer; an unclassified crate fails the check.
- `ptc-core` depends on no internal crate. Its external dependencies are limited to
  `ryu-js` (ECMAScript number formatting, for RFC 8785), `serde_json`, `sha2`, `thiserror` and
  `uuid` (without features).
- `ptc-engine` depends only on `ptc-core`.
- `ptc-format` depends only on `ptc-core`.
- `ptc-api` depends only on `ptc-core`. Only the composition roots and the tooling crates
  depend on it: the CLI is an API client, and cannot depend on a composition root to reach
  the DTOs.
- Adapters depend on `ptc-core` and `ptc-engine` (stores also on `ptc-format`), and never
  on each other.
- Only `ptc-cli` and `ptc-server` may depend on adapters, and the tooling crates
  (`ptc-e2e`, `evals`), which wire adapters like a composition root for tests. `ptc-cli` and
  `ptc-server` never depend on each other. Only the `ptc` binary crate depends on the
  composition roots. Nothing depends on a tooling crate.
- The `ptc` binary crate depends only on `ptc-cli` and `ptc-server`, and nothing depends on it.
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
    emitted ← instance.deliver(item, recorded_at)     # payloads loaded from where they live
    entries += [item] + emitted
move payloads > 1 KiB out of the entries: to the payloads rows, or to the BlobStore above 64 KiB
effects ← tool tasks, timers, cancelled timers derived from the new commands
journal.commit_activation(lease, Activation { entries, consumed, payloads, effects, status })   # + feed items
cache[run] ← instance                                  # sticky execution
```

The lease is renewed about every 10 s while this runs. The activation itself is a `tracing` span
(service telemetry), with the run's trace id, span id `H(run, fence)` and parent, the run span
`H(run, "run")`, recorded as fields.
Nothing about the script is emitted during replay.

- Script checks at start fail → `StartError::InvalidScript`, and no run is created.
- Executor start errors (top-level exception) → `run_aborted { invalid_script }`.
- Replay mismatch → nothing is committed; `journal.quarantine(lease, mismatch)`.
- Any other failure → nothing is committed; `journal.record_failure(lease, error, now)`.

## Tests

| Where | What |
| --- | --- |
| `ptc-core` | Unit tests of the pure model: canonical JSON (RFC 8785 golden vectors), digests, matching, `ReplayCursor`, run state, durations |
| `ptc-format` | Round trips and golden fixtures (a stored v1 document must always decode) |
| `ptc-engine` | Use cases with in-memory adapters and a fake executor; the conformance suites of `JournalStore` (idempotency keys, epochs, leases and renewal, quarantine and failure counts, run expiry, the feed, tool task lifecycle and concurrency, sticky affinity, tenants and fairness, payload references, retention), `BlobStore`, `ReadModel`, `SecretStore` and `SpanExporter`, passed by the in-memory adapters; span derivation; projector idempotence (applying a feed item twice changes nothing) |
| `ptc-runtime` | Sandbox behavior, driven by the engine with in-memory adapters and virtual time (`Promise.all`, time, randomness, signals, imports, callsites, cancellation, limits, the exact set of globals, RFC 8785 numbers against V8, and how a script ends: forgotten `await`, unhandled rejection, deadlock) |
| Epoch corpus (`ptc-e2e`) | `corpus/epoch-N/{blue,red}/<case>/`: a script, a `scenario.json` (input, tools, steps, expectations) and its recorded `journal.v1.json`. Each case runs live and must meet its expectations, must record the committed journal byte for byte, and the committed journal must replay with the script on a fresh executor. Blue cases must succeed; red cases must fail exactly as specified (refused start, failed, cancelled or aborted run, diverging replay, tampered journal). `PTC_E2E_RECORD=1` records the journals again |
| `ptc-store-turso`, `ptc-store-postgres` | The conformance suites of `JournalStore`, `ReadModel` and `SecretStore` |
| `ptc-blob` | The conformance suite of `BlobStore` |
| `ptc-otel` | The conformance suite of `SpanExporter`, on the SDK's in-memory exporters and on an OpenTelemetry Collector in Docker over gRPC and HTTP; service spans and logs joined to a run's trace by their fields; service and run metrics, the engine driven under the subscriber |
| `ptc-cli` | End to end through the embedded server: V8 + engine + Turso + files, across processes (sleep, interrupt, resume) |
| `ptc-architecture-tests` | Dependency rules, telemetry rules included |

## HTTP API `/v1`

`ptc-server` serves the API. The CLI, the SDK and its agent tools, the dashboard and the
evals are its clients.

Conventions:

- Authentication by an API key as a Bearer token. The key designates the tenant
  ([tenancy](06-storage-and-scale.md#tenancy-and-authentication)).
- `Idempotency-Key` on run creation (start and fork) and on signals
  ([idempotency](06-storage-and-scale.md#idempotency-of-the-api)).
- Errors are `application/problem+json`, with a message written for a model. Their `type` is
  `urn:ptc:problem:<slug>`; the slugs are listed in `api/README.md`.
- JSON fields are camelCase, enumerated values snake_case, times RFC 3339, durations `"30s"`
  or a number of milliseconds.
- The OpenAPI document is generated from the DTOs of `ptc-api` (utoipa) and committed at
  `api/openapi.json`; a test fails when it is stale. The SDK's and the dashboard's types are
  generated from it (`api/README.md`).

| Group | Endpoint | Does |
| --- | --- | --- |
| Toolsets | `POST /v1/toolsets` | Registers and pins a toolset; returns its declarations |
| | `GET /v1/toolsets/{id}/declarations` | The declarations, `format=dts` or `prompt` |
| | `GET /v1/toolsets/{id}/search` | `search_tools`, for large toolsets |
| | `PUT /v1/toolsets/{id}/credentials` | Sets or renews the toolset's [credentials](04-tools.md#credentials) |
| Scripts | `POST /v1/check` | Type-check only |
| | `POST /v1/test` | Runs a scenario in mock mode, in virtual time |
| Runs | `POST /v1/runs` | Starts a run: `Idempotency-Key`, `wait`, `labels`, `traceparent`, `ttl` |
| | `GET /v1/runs` | Lists runs, filtered by labels, status, tool |
| | `GET /v1/runs/{id}` | The run's state |
| | `GET /v1/runs/{id}/trace?format=llm` | The LLM trace |
| | `GET /v1/runs/{id}/entries/{idx}` | One journal entry in full |
| | `GET /v1/runs/{id}/events` | SSE, `follow=1` |
| | `POST /v1/runs/{id}/signals/{name}` | `Idempotency-Key`; 409 if the run has ended |
| | `POST /v1/runs/{id}/cancel` | Cancels the run |
| | `POST /v1/runs/{id}/fork` | `retry_run`: a corrected script, an optional `at` and `input` ([forks](03-execution.md#forks)) |
| | `GET /v1/runs/{id}/journal` | Exports the journal document; `?as=cassette` exports a cassette |
| | `POST /v1/runs/{id}/release` | Takes the run out of quarantine |
| | `POST /v1/runs/{id}/give-up` | Gives up a quarantined run |
| | `PUT /v1/runs/{id}/credentials` | Sets or renews the run's credentials |
| | `DELETE /v1/runs/{id}` | Erases the run, payloads included |
| Host-tool tasks | `GET /v1/tasks` | Long-poll, filtered by toolset and tools |
| | `POST /v1/tasks/{token}/heartbeat` | Extends the task's claim |
| | `POST /v1/tasks/{token}/defer` | The handler answers later: the task is `deferred`, without heartbeats, until `complete` or `fail` |
| | `POST /v1/tasks/{token}/complete` | The result; also later, asynchronously |
| | `POST /v1/tasks/{token}/fail` | The failure |
| | `POST /v1/tasks/{id}/approve` | Approval policy, parked tasks: the task goes on |
| | `POST /v1/tasks/{id}/reject` | The call fails with a `ToolError` |
| Webhooks | `POST /hooks/{token}` | Inbound, without authentication, at an unguessable URL: becomes a signal |
| | `POST /v1/subscriptions` | Outbound: signed lifecycle events |
| | `GET /v1/subscriptions` | Lists the subscriptions, without their secrets |
| | `DELETE /v1/subscriptions/{id}` | Deletes a subscription |
| Reading (dashboard) | `GET /v1/scripts` | Every distinct script, its runs, its tools |
| | `GET /v1/scripts/{digest}` | One script's source |
| | `GET /v1/tools/stats` | Latency, errors, retries |
| | `GET /v1/payloads/{digest}` | A payload, on demand |
| Administration | `POST /v1/api-keys` | Creates an API key for a tenant, with its scope; keys are revocable |
| | `DELETE /v1/api-keys/{id}` | Revokes a key |
| | `GET /v1/server` | The server's version, epochs and roles |
| | `POST /v1/server/stop` | Stops the embedded server, when the CLI replaces it with one of its own version |

## Embedded server and CLI

The CLI never opens `journal.db`. It is a client of the API `/v1`: of a remote server, or of
the embedded server of a `.ptc/` directory. Only one process opens the database, so Turso
stays in its stable single-process mode, and the instance cache lives in that server.

`ptc server --embedded` is the engine host for a `.ptc/` directory. It takes both roles, api
and worker.

| When | What happens |
| --- | --- |
| A command needs the server | If `.ptc/server.lock` is free, no server runs: the command starts one, detached. |
| The server starts | It takes an exclusive lock on `.ptc/server.lock`, opens `journal.db`, and listens on the unix socket `.ptc/server.sock`. Due timers fire at startup. |
| A second server starts | It fails to take the lock and exits. |
| The server is killed (`kill -9`) | The lock is released. The next command starts a new server, and expired leases give the runs back. |
| The CLI's version differs from the server's | The CLI asks the server to stop and starts a new one. |
| 10 minutes pass without requests and without work in progress | The server stops. A sleeping run resumes at the next `ptc` command. |

- The socket is mode 0600 and takes no API key. The embedded server has one implicit tenant,
  `local`.
- `ptc server` run in the foreground never stops on idle.
- Virtual time is an option of the server, kept in the `meta` table.
- Local-only commands run in the CLI process, without a server: `ptc check`, `ptc test`, and
  the replay of an exported journal.

The `.ptc/` directory:

```
project/
  script.ts
  ptc.tools.json            static responses per tool, for mock mode
  .ptc/                     created when the first server starts; --dir picks another
    server.lock             held by the embedded server
    server.sock             the API /v1, mode 0600
    journal.db              Turso: every table of the store; meta holds the schema version and virtual time
    journal.db-wal          Turso's write-ahead log
    blobs/
      local/sha256/ab/cdef… payloads above 64 KiB of the tenant local, named by their digest
      .staging/             where a blob is written and synced before it is renamed
```

Commands:

| Command | Does |
| --- | --- |
| `ptc run script.ts [--input …] [--wait 30s] [--ttl 30d]` | Starts a run and waits for it, up to `--wait` |
| `ptc check script.ts` | Type-checks a script against the toolset's declarations ([tools](04-tools.md#type-checking)) |
| `ptc test scenario.json` | Runs a scenario against mocks and cassettes, in virtual time ([testing](05-testing-and-observability.md#testing-and-mocks)) |
| `ptc signal <run> <name> [payload]` | Sends a signal |
| `ptc cancel <run>` | Cancels a run |
| `ptc replay <run> [--script fixed.ts]` | Replays locally and checks determinism; `<run>` is a run id or an exported journal |
| `ptc fork <run> --script fixed.ts [--at <index>] [--input …]` | Starts a [fork](03-execution.md#forks) |
| `ptc trace <run> [--entry n]` | LLM-readable trace |
| `ptc runs` | Lists runs |
| `ptc release <run> [--give-up]` | Takes a run out of quarantine, or gives it up |
| `ptc status <run> [--wait 30s]` | A run's state, waiting up to `--wait` for it to end |
| `ptc export <run> [--as journal\|cassette] [-o file]` | The run's journal document, or a cassette of its calls and signals |
| `ptc toolsets add\|declarations\|search` | Registers a toolset file; its declarations; searches its tools |
| `ptc server [--embedded] [--role api\|worker] [--virtual-time]` | Runs the server in the foreground |

Every command takes `--json`. A first Ctrl-C lets the command finish the call in flight; a
second one exits at once.

Exit codes:

| Code | Meaning |
| --- | --- |
| 0 | The run completed, or the command succeeded |
| 1 | The run failed |
| 2 | The run was cancelled |
| 3 | The run was aborted by the host |
| 4 | The run is still waiting: `--wait` elapsed, or it waits for the outside world |
| 5 | The run is quarantined |
| 6 | Refused: unknown run, or a signal to a run that has ended |
| 64 | Usage error |
| 65 | Script refused at start |
| 70 | Internal error |
| 130 | Interrupted by a second Ctrl-C |

## Dashboard

`dashboard/` is the React single-page application of [05](05-testing-and-observability.md#dashboard):
Vite, strict TypeScript, pnpm. Its types are generated from `api/openapi.json`, and it calls only the
public API.

Its build is what `ptc-server` serves:

| | |
| --- | --- |
| Build | `pnpm --dir dashboard install --frozen-lockfile && pnpm --dir dashboard build` |
| Output | `dashboard/dist/`: `index.html`, `favicon.svg`, `assets/` (scripts, styles, fonts, named by content hash). Not committed: `ptc-server` embeds the directory when it is compiled |
| Serving | Static files under any path prefix. Every path in the build is relative and the routes live in the URL fragment (`#/runs/…`), so no fallback route is needed. `index.html` is not cached; `assets/` is immutable |
| API | `/v1` on the same origin. Only `GET` requests, so a `reader` key is enough. The operator types the key; the browser keeps it for the tab, or on the device when asked |
| Event stream | Read with `fetch`, not `EventSource`, which cannot send the `Authorization` header; resumed with `from` after a drop |
| Resources | None from another origin: the fonts are in the build. No inline script |

`dashboard/README.md` has the development commands; `pnpm dev:mock` runs it against a mocked API.
