# 9. Roadmap

Ordered by risk: the riskiest part (the deterministic runtime) came first.

| # | Milestone | Done when | Status |
| --- | --- | --- | --- |
| M0 | **Runtime spike**: `ptc-core` + `ptc-runtime` | A TS script with `Promise.all` over fake tools runs, its replay is identical, non-determinism is detected. | Done |
| 0.1 layering | **Strict boundaries**: pure core, `ptc-format`, `ptc-engine` with ports, runtime as an adapter; format v1 with the decisions of the November review (epoch, 256-bit seed, RFC 8785, `expires_at`) | The M0 behavior passes on the new layering, with the dependency rules checked. | In progress |
| M1 | **Embedded engine**: SQLite store, file blobs, activations, timers, signals, logical and virtual time, run TTL, idempotency keys, how a script ends, quarantine, CLI | `ptc run`, `ptc signal`, `ptc replay`, `ptc trace` work locally on a run that sleeps, is interrupted, and resumes in another process. A forgotten `await`, an unhandled rejection and a deadlock each end as specified. | Next |
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

The 0.1 refactor has started, and **the workspace does not compile at the moment**.
Nothing is committed yet.

- `ptc-core`: `ids.rs` (no serde, no id generation), `digest.rs`, `payload.rs`
  (`Value | Digest`, canonical JSON) and `artifacts.rs` (`Script`, `Toolset`) are
  rewritten. Still to do: `event.rs` without serde (adding `signal_awaited` and
  `invalid_script`), `state.rs`, removing serde from `time.rs` and `Cargo.toml`, updating
  the `replay.rs` tests and `lib.rs`.
- `ptc-runtime`: still targets the M0 core API (serde events, `ContentHash`).

The decisions of the November 2026 review ([08](08-decisions.md)) are documented, not
implemented. The M0 code still:

- has a 64-bit seed, and sfc32 behind `crypto` as well as `Math.random`;
- records the crate version as `runtime_version`, with no epoch;
- emits the terminal command as soon as `main` settles, with tool calls still pending;
- does not check for unhandled rejections, and leaves a deadlocked script without an
  outcome;
- canonicalizes JSON with sorted keys only.

## Next steps

1. Finish `ptc-core` and its tests: `Epoch`, the 256-bit `Seed`, `expires_at` in
   `run_started`, and RFC 8785 with golden vectors. RFC 8785 writes numbers as
   ECMAScript does, which `serde_json` does not: either our own code in `ptc-core`, or
   `ryu-js` added to the dependencies the core is allowed.
2. `ptc-format` v1 with golden fixtures.
3. `ptc-engine`: ports (idempotency keys, epochs, quarantine and run expiry included),
   use cases, driver, in-memory adapters, conformance suite.
4. `ptc-runtime` as an adapter: `V8Executor` on dedicated threads, its own JS encoding,
   the `signal_awaited` op, ChaCha20 for `crypto`, the rules of
   [how a script ends](03-execution.md#how-a-script-ends), glue selected by epoch; the 10
   runtime tests pass again, with new ones for those rules.
5. Dependency rules test crate, telemetry rules included (no telemetry in `ptc-core`,
   `opentelemetry*` only in `ptc-otel`).
6. M1: `ptc-blob` (files), `ptc-store-sqlite`, `ptc-cli` with end-to-end tests; a first
   epoch corpus recorded from them.
7. fmt, clippy (`-D warnings`), full test suite.

## Open questions

1. **Size and preview of offloaded payloads.** Traces, OTel spans and the dashboard must
   render without downloading payloads. Proposal: the store keeps
   `(digest, bytes, preview)` as blob metadata (a `payloads` table), outside the domain
   model and outside `ptc-format`'s event encoding.
2. **Thread per live instance.** Each live V8 instance holds a thread. Instances are
   dropped when a run suspends in 0.1; a bounded sticky cache comes in M5. Is a
   thread-per-instance model acceptable at the target scale, or do we need an isolate
   pool per thread?
3. **Toolset drift during long pauses.** Today: dispatch validates against the current
   schema and fails clearly. Alternative: pin MCP server versions per run.
4. **Signals to a finished run.** The engine drops completions for terminal runs. The API
   should probably reject them (409) so the sender knows.
5. **Cancellation grace.** One grace activation after `cancel_requested`. Enough, or a
   time-bounded grace?
6. **`continueAsNew` shape** and the default history cap (50k entries). Does the
   continuation get a new TTL?
7. **Tenancy.** S3 keys are per tenant, but the domain model has no tenant, and the
   dashboard needs access scoped by tenant. Where does the tenant live: in the run id, in
   the store, or as a port parameter? Proposal: in the run metadata, never in core.
8. **`webhook(name)` security.** Unguessable URL only, or a signed request?
9. **Fork semantics** (M3): how the new run references its parent, and which recorded
   results are reused. In particular: what happens at the first command that differs
   from the parent's (continue live, or reject the fork); which idempotency key the
   reused and the re-executed calls carry; and what a fork does with a call left without
   an outcome by a cancelled or aborted parent.
10. **Run labels.** Limits (count, key and value size), whether they can change after the
    run starts, and whether all of them become span attributes (cardinality, personal
    data). Proposal: a bounded string map, set at start only, exported to OTel only for
    an allow-listed set of keys.
11. **Retention.** Finished histories are archived (scaling path, step 2). How long do
    histories and the read model stay queryable, and does the dashboard read archived
    histories from S3? Proposal: the read model is kept per tenant retention; a run's
    detail page loads its archived history on demand.
12. **`run_script` result budget.** The default size above which the output is
    truncated, and how the model pages through the rest. To settle with the evals.
13. **Web-standard globals.** The exact baseline, and how it is provided: selected parts
    of `deno_web` and `deno_url`, or our own implementations. Bare `deno_core` has none
    of `URL`, `TextEncoder` or `atob`.
14. **Quarantine release.** By hand only, or retried automatically when a new build is
    deployed?
15. **Run TTL.** The maximum a deployment allows, and whether a run that expires gets a
    status of its own instead of `cancelled`.

## Inputs for the M4 architecture review

Raised by the review of November 2026 and accepted as directions. None is designed yet:
each goes through the M4 principles review before any code.

- **The short path.** Most runs last a few seconds inside an agent loop, and the design
  has no latency budget for them. To settle:
  - whether sticky execution moves from M5 to M4;
  - `commit_activation` handing the new tool tasks, already claimed, to the worker that
    commits, instead of a second polling hop;
  - wake-ups by `LISTEN/NOTIFY` rather than polling;
  - payloads of medium size kept in Postgres (the `payloads` table of question 1), with
    S3 above a measured threshold instead of 1 KiB.
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
  - Tenancy (question 7) settled first: it touches every key, the quotas, and the
    fairness of `acquire_runnable`.
- **Operations.** Retention and deletion with deduplicated blobs; usage metering as a
  projection of the journal.
