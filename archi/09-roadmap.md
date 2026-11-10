# 9. Roadmap

Ordered by risk: the riskiest part (the deterministic runtime) came first.

| # | Milestone | Done when | Status |
| --- | --- | --- | --- |
| M0 | **Runtime spike**: `ptc-core` + `ptc-runtime` | A TS script with `Promise.all` over fake tools runs, its replay is identical, non-determinism is detected. | Done |
| 0.1 layering | **Strict boundaries**: pure core, `ptc-format`, `ptc-engine` with ports, runtime as an adapter | The M0 behavior passes on the new layering, with the dependency rules checked. | In progress |
| M1 | **Embedded engine**: SQLite store, file blobs, activations, timers, signals, logical and virtual time, CLI | `ptc run`, `ptc signal`, `ptc replay`, `ptc trace` work locally on a run that sleeps, is interrupted, and resumes in another process. | Next |
| M2 | **Tools**: codegen, MCP dispatch, policies, idempotency, `ptc check` | A script calls a real MCP server, with types checked beforehand. | |
| M3 | **Testing**: cassettes, TS mocks, scenarios | `ptc test` replays a recorded real run as a scenario. | |
| M4 | **Server and scale**: Postgres, S3 payloads, leases and fencing, N workers, HTTP API, host tools, TS SDK and agent tools, webhooks, run metadata, feed and projector, read model and query API, OTel export (`ptc-otel`) | A harness drives runs through the SDK, with several workers on one Postgres; each run appears as one trace under the harness's span, and runs can be searched by label and tool. | |
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

## Next steps

1. Finish `ptc-core` and its tests.
2. `ptc-format` v1 with golden fixtures.
3. `ptc-engine`: ports, use cases, driver, in-memory adapters, conformance suite.
4. `ptc-runtime` as an adapter: `V8Executor` on dedicated threads, its own JS encoding,
   the `signal_awaited` op; the 10 runtime tests pass again.
5. Dependency rules test crate, telemetry rules included (no telemetry in `ptc-core`,
   `opentelemetry*` only in `ptc-otel`).
6. M1: `ptc-blob` (files), `ptc-store-sqlite`, `ptc-cli` with end-to-end tests.
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
6. **`continueAsNew` shape** and the default history cap (50k entries).
7. **Tenancy.** S3 keys are per tenant, but the domain model has no tenant, and the
   dashboard needs access scoped by tenant. Where does the tenant live: in the run id, in
   the store, or as a port parameter? Proposal: in the run metadata, never in core.
8. **`webhook(name)` security.** Unguessable URL only, or a signed request?
9. **Fork semantics** (post-V1): which recorded results are reused, and how the new run
   references its parent.
10. **Run labels.** Limits (count, key and value size), whether they can change after the
    run starts, and whether all of them become span attributes (cardinality, personal
    data). Proposal: a bounded string map, set at start only, exported to OTel only for
    an allow-listed set of keys.
11. **Retention.** Finished histories are archived (scaling path, step 2). How long do
    histories and the read model stay queryable, and does the dashboard read archived
    histories from S3? Proposal: the read model is kept per tenant retention; a run's
    detail page loads its archived history on demand.
