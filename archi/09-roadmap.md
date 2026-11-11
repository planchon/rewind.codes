# 9. Roadmap

Ordered by risk: the riskiest part (the deterministic runtime) came first. Version 0.1 is
ai-ptc once M1 to M6 are done.

| # | Milestone | Done when | Status |
| --- | --- | --- | --- |
| M0 | **Runtime spike**: `ptc-core` + `ptc-runtime` | A TS script with `Promise.all` over fake tools runs, its replay is identical, non-determinism is detected. | Done |
| Layering | **Strict boundaries**: pure core, `ptc-format`, `ptc-engine` with ports, runtime as an adapter; format v1 with the decisions of the November review (epoch, 256-bit seed, RFC 8785, `expires_at`) | The M0 behavior passes on the new layering, with the dependency rules checked. | Done |
| M1 | **Embedded engine**: Turso store, file blobs, activations, timers, signals, logical and virtual time, run TTL, idempotency keys, how a script ends, quarantine, CLI | `ptc run`, `ptc signal`, `ptc replay`, `ptc trace` work locally on a run that sleeps, is interrupted, and resumes in another process. A forgotten `await`, an unhandled rejection and a deadlock each end as specified. | Next |
| M2 | **Tools**: codegen, MCP dispatch, policies, idempotency, `ptc check` with its own rules and the sandbox `lib`, web-standard globals | A script calls a real MCP server, with types checked beforehand. | |
| M2.5 | **Evals**: task suite, deterministic fake MCP servers, a harness driving the `ptc` CLI | A model runs the suite end to end (generate → check → run → fix), and the report gives first-try check pass rate, task success, iterations and tokens per model and prompt rendering. | |
| M3 | **Testing and fork**: cassettes, TS mocks, scenarios, `ptc fork`, observed result types | `ptc test` replays a recorded real run as a scenario. `ptc fork` continues a failed run with a corrected script without repeating its recorded calls. | |
| M4 | **Server and scale**: Postgres, S3 payloads, leases and fencing, N workers aware of epochs, HTTP API, host tools, TS SDK and agent tools (`retry_run` and the bounded `run_script` result included), webhooks, run metadata, feed and projector, read model and query API, OTel export (`ptc-otel`) | A harness drives runs through the SDK, with several workers on one Postgres; each run appears as one trace under the harness's span, and runs can be searched by label and tool. | |
| M5 | **Performance and hardening**: sticky execution, startup snapshots, limits, sandboxed workers | Load tests and limit tests pass. | |
| M6 | **Dashboard**: every run and all the code executed on the platform, over the public HTTP API | An operator finds any run by label, tool or error, and sees its code next to its trace. | |

## What M0 proved

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

At the end of M0: 35 tests passing, clippy clean.

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

Not there yet, and planned:

- M1: the Turso store (`ptc-store-turso`) and `ptc-blob` (files), both passing the
  conformance suite; `ptc-cli`; end-to-end tests across processes reusing the corpus.
- M1: an activation that fails 5 times in a row quarantines the run. Today it is retried every
  lease period, forever. It needs the store to count consecutive failures and to record why a
  run is quarantined.
- Instances are not kept between activations: every activation replays the full history
  (sticky execution is M5).
- M2: tool schemas, policies per tool (`at_most_once` as an opt-out), retries with backoff on
  transient errors, codegen, type-checking at start and `ptc check`, the toolset-change
  warning.
- M2: web-standard globals (`URL`, `TextEncoder`, `atob`…) from deno crates. None yet;
  `queueMicrotask` is present.
- `webhook(name)` is designed, not implemented (M4).
- The feed, the projector, OpenTelemetry and the read model (M4).
- Before M4: the CPU budget is measured as the wall-clock time of a call, so a heavily loaded
  host can make a script exceed it (live, the run is aborted; during replay, the activation is
  retried). It moves to the thread's CPU clock.
- Before M4: leases are not renewed during an activation. A replay longer than the lease
  (30 s) lets another worker take the run over, and with several workers on a very long
  history they can fence each other off forever. One worker (M1) is not affected; M4
  renews the lease with a heartbeat during an activation.

## Next steps

1. Present the principles of M1 for review (process rule): where the store's rows meet
   `ptc-format`, the file layout of `ptc-blob`, the CLI's commands, and how several `ptc`
   processes share one `.ptc` directory given Turso's experimental multi-process access.
2. Repeated activation failures quarantine the run (store contract, conformance suite).
3. `ptc-blob` (files, atomic writes) and the Turso store, both through the conformance
   suite.
4. `ptc-cli` with `run`, `signal`, `cancel`, `tick`, `worker`, `replay`, `trace`, `runs`, and
   its end-to-end tests: a run that sleeps, is interrupted and resumes in another process,
   driven by the corpus scenarios.

## Open questions

Questions 1, 3, 4, 5, 7, 8, 10, 12, 13, 14, 15, 16 and 17 were settled in November 2026: see
[08-decisions.md](08-decisions.md#taken-after-the-layering-november-2026). The numbers of the
others are kept.

- **2.** **Thread per live instance.** Each live V8 instance holds a thread. Instances are
  dropped when a run suspends until M5, where a bounded sticky cache comes. Is a
  thread-per-instance model acceptable at the target scale, or do we need an isolate
  pool per thread?
- **6.** **`continueAsNew` shape** and the default history cap (50k entries). Does the
  continuation get a new TTL? Not planned: to settle when a real run nears the cap.
- **9.** **Fork semantics** (M3): how the new run references its parent, and which recorded
  results are reused. In particular: what happens at the first command that differs
  from the parent's (continue live, or reject the fork); which idempotency key the
  reused and the re-executed calls carry; and what a fork does with a call left without
  an outcome by a cancelled or aborted parent. To settle in the M3 review.
- **11.** **Retention durations and deletion.** The shape is decided (finished journals
  archived to S3 and loaded on demand, read model per tenant retention). Still open: how
  long a finished journal stays in Postgres, the default tenant retention, and whether runs
  are ever deleted (tool arguments can hold personal data). To settle with M4.

## Inputs for the M4 architecture review

Raised by the review of November 2026 and accepted as directions. None is designed yet:
each goes through the M4 principles review before any code.

- **Caching.** The latency of the polling path is accepted (owner decision); gains come
  from caching. To settle:
  - what is kept between activations (instances, histories, payloads), and whether sticky
    execution moves from M5 to M4;
  - payloads of medium size kept in Postgres (the `payloads` table), with S3 above a
    measured threshold instead of 1 KiB.
- **Tool task lifecycle.**
  - Timeouts, heartbeats and asynchronous completion for host tools that last days, such
    as `ask_user`.
  - A concurrency limit per run and per server in the dispatcher. It is invisible to the
    journal. To settle with it: whether calls still queued when a script fails are
    dispatched.
  - Credentials resolved at dispatch through a provider: a token expires during a long
    sleep.
  - An `approval` policy that parks a task until the harness accepts it, so that scripts
    do not bypass the harness's permission gates.
- **Security.**
  - SSRF protection and an egress allow-list for the `http` tool.
  - API authentication.
  - Agent tools that take a `run_id` check the run's labels.
  - Tenancy settled first (in the run metadata): it touches every key, the quotas, and
    the fairness of `acquire_runnable`.
