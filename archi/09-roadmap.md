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
- **Sandbox**: no `fetch`, `Deno`, `WeakRef` or `process`; only `rewind` and `rewind:tools` are
  importable, dynamic `import()` included.
- **Callsites** point to TypeScript lines.
- **Errors and cancellation**: `ToolError` (with `outcomeUnknown`) and `CancelledError`
  reach the script and can be caught.

## Current state of the code

Every workstream is built and integrated on `main`, and every part was reviewed and its
findings fixed. On November 12, 2026: `cargo fmt --check`, `cargo clippy --workspace
--all-targets -- -D warnings`, `cargo doc --workspace --no-deps` with warnings as errors and
`cargo test --workspace` (795 tests, Postgres, MinIO and a Collector in Docker) pass, the
SDK's and the dashboard's checks pass, and `acceptance/run.sh` passes the seven criteria of
[0.1 is done when](#01-is-done-when). Not yet done: the evals with real models (they need
API keys), a run of the CI workflow on GitHub, the gVisor profile on a Linux host, and accounts,
organizations and access ([06](06-storage-and-scale.md#tenancy-identity-and-access)), designed on
November 14, 2026 and being built (see [Workstreams](#workstreams)).

| Crate | State |
| --- | --- |
| `rewind-core` | Pure domain, no serde: ids (`Seed` on 256 bits, `Epoch`), RFC 8785 canonical JSON (golden vectors of the RFC), payloads, the four event families (`signal_awaited` and `invalid_script` included), `RunState`, history validation, `ReplayCursor`; the pinned toolset (MCP servers and their transports; tools with route, description, input and output schemas, policy) and its digest; tool outcomes; cassettes derived from a history; observed result shapes. |
| `rewind-format` | Format v1: one event per row, journal documents (the run's id, script and pinned toolset, then its entries), cassettes, pinned toolsets (canonical, hashing to the toolset's digest), values in canonical form. Golden fixtures pin every variant's spelling; unknown versions, types and fields are refused. |
| `rewind-engine` | The final ports of 0.1, each with an in-memory adapter; the six conformance suites, of `JournalStore`, `BlobStore`, `ReadModel`, `SecretStore`, `SpanExporter` and `WebhookSender`, passed by the in-memory adapters. Use cases: start and fork (script checks, the type-check through the `TypeChecker` port unless skipped, idempotency key, TTL, the tenant's quota, credentials), signal with idempotency key, cancel, activate, fire timers, expire runs, quarantine and give up, replay, export of journal documents, forks (with the parent's calls reused, keys of calls left without an outcome, and the compatibility report). Activation with the commit invariant, limits and the cancellation grace; sticky execution in an LRU instance cache bounded by count and memory; the lease renewed every 10 s; quarantine after 5 failed activations, those whose worker died included, retried once by a new build. Tool tasks: policies and backoff, claims heartbeated up to the execution timeout, credentials resolved at dispatch, parked tasks and their webhooks, approvals, the host-tool worker's claim, heartbeat, deferral and answers by token, the `webhook` built-in, a routing dispatcher. Payloads in three tiers. The projector (read model with each run's snapshot, its waits and the fingerprint of how it failed, run telemetry with deterministic span ids, run metrics, lifecycle webhooks); service telemetry through `tracing`: the activation span under the run span, the tool attempt span under the tool call span, and the measurements of `rewind_engine::metrics`; mock mode (cassettes, mock handlers, static responses, scenarios), retention and deletion, the LLM trace. Driver in real and virtual time, which jumps to timers, tool call retries and deadlines; use-case tests on a fake executor. |
| `rewind-runtime` | `V8Executor`: the checks at start (syntax, imported modules and names, default export), an isolate per instance on its own thread, started from the epoch's V8 snapshot, the epoch-1 glue, logical time, sfc32 and ChaCha20 keyed by the seed, `run.id` and `run.startedAt`, the web-standard globals of `deno_web`, how a script ends (forgotten `await`, unhandled rejection, `DeadlockError`), a CPU watchdog on the isolate thread's CPU clock and a heap limit; args validated against the pinned schema inside the op; `webhook()`; `fetch` with the `http` tool, in the format of `rewind-mcp`'s; `rewind:tools` generated from routes, MCP content with `text()` and `json()`; TypeScript mock handlers as a `ToolDispatcher` (`TsMocks`); a budget of array buffers (resizable ones included), built-ins measured against the heap before V8 runs them to their end, and the stack limit pinned before each call (the `memory` module, the only `unsafe` code); the memory each instance holds, from V8's heap statistics, for the sticky cache's bound; the JavaScript of `deno_web` and `deno_webidl` vendored and given to the snapshot build from memory, so the binary runs on a machine without the cargo registry ([08](08-decisions.md#taken-while-building-operations-november-2026)). |
| `rewind-store-turso` | `JournalStore`, `ReadModel` and `SecretStore` on Turso Database (`turso` 0.8.3): the tables of 06, events in format v1, schema version 1 with migrations embedded and a newer schema refused, settings and virtual time in `meta`, secrets encrypted with a data key per tenant under the master key; the read model's labels, waits, task states, quarantine and texts, searched with `instr`, and statistics counted in Rust. Passes the three conformance suites, each check on a new database file; `examples/load.rs` measures commit latency and throughput ([08](08-decisions.md#taken-while-building-the-embedded-store-november-2026)). |
| `rewind-store-postgres` | `JournalStore`, `ReadModel` and `SecretStore` on Postgres with sqlx 0.8: `FOR UPDATE SKIP LOCKED`, fencing, tenant in every key, fair `acquire_runnable`, advisory locks for the claim limits, embedded migrations run under a lock; `history` and `inbox` keyed by `run_id` and tested hash-partitioned; the read model's query as SQL over indexes scoped by tenant, text through `pg_trgm`, statistics and failure groups as single statements. Passes the conformance suites against Postgres 17 in Docker, and a concurrency test with 3 engines × 4 workers on one database; about 830 activations/s with 4 × 4 on a laptop (`cargo run --release -p rewind-store-postgres --example postgres_load`). |
| `rewind-blob` | `BlobStore` on local files (`FileBlobs`: atomic writes, staged, synced and renamed, under `{tenant}/sha256/`) and on S3 or any S3-compatible service (`S3Blobs`, through `object_store`, with conditional puts); `CachedBlobs`, the workers' cache in memory, bounded in bytes, in front of either. Digests checked on every put and get. Passes the conformance suite of `BlobStore` on files and on MinIO in Docker ([08](08-decisions.md#taken-while-building-the-blob-stores-november-2026)). |
| `rewind-architecture-tests` | The dependency rules, telemetry and `anyhow` included, each with a test proving it catches its violation. |
| `rewind-e2e` | The epoch-1 corpus: 30 blue and 31 red cases, forks, tool policies, MCP content and the `http` tool's format included, recorded and replayed from their journal documents; tests proving the harness cannot pass silently; the contract test of codegen, the checker and the runtime (`tests/declarations.rs`); `cargo run -p rewind-e2e -- replay <case> [--script file.ts]`. |
| `rewind-api` | The HTTP API `/v1` as a contract: the DTOs of every endpoint, problem+json errors, the query language of the run lists (parsed with `winnow`, errors with the span of the term at fault), `Idempotency-Key`, the event stream, webhook payloads signed following Standard Webhooks and their verification, the OpenAPI 3.1 document committed at `api/openapi.json` (a test fails when it is stale), and an async client over HTTP(S) and over the embedded server's unix socket. |
| `rewind-check` | `TsgoChecker`, the `TypeChecker` port: tsgo 7.0.2 embedded as its npm tarball, pinned by its integrity and extracted to a cache directory at first use; checks in strict mode against the sandbox lib and `rewind-tools.d.ts`, reading nothing outside its directory; diagnostics with a location, a hint and a code frame; the rules `floating-promise`, `ignored-async-callback` and `absent-global` on the swc syntax tree. About 50 ms per check. |
| `rewind-codegen` | Our JSON Schema → TypeScript converter (objects, `required`, enums, `anyOf`/`oneOf`/`allOf`, `$ref`/`$defs`, arrays, `additionalProperties`); `rewind-tools.d.ts` (one namespace per MCP server or host-tool group, `ToolContent` for MCP content, observed types marked `@observed`, the `fetch` reference with the `http` tool); the compact prompt rendering and each tool's own declaration (for `search_tools`). Golden tests on GitHub-like, FastMCP-like and mixed toolsets. |
| `rewind-mcp` | `McpDispatcher`: an MCP client (`rmcp` 3.9) over streamable HTTP and stdio, discovery as a toolset pins it, connections pooled by tenant, server and credentials, the idempotency key and the trace context in `_meta`, credentials in the transport, the schema check at dispatch (a changed tool parks the call), the execution timeout, errors mapped to transient failures, failures and parked calls; on a deployed server, MCP servers reached only at public addresses or at the internal hosts the operator allows, whose answers are never quoted (`McpEgress`, `InternalAllowList`). `HttpTool`: the `http` built-in and its egress rules (allow-list, one resolution, special-purpose addresses refused, the checked address connected, redirects checked again, 5 MiB and 30 s). Tested against fixture servers over HTTP and stdio. |
| `rewind-otel` | OpenTelemetry 0.33, OTLP over gRPC or HTTP with protobuf, both with TLS, configured or from the `OTEL_*` variables. `OtelSpanExporter`, the `SpanExporter` port: run telemetry exported with the projector's ids, under the scope `rewind.run`. `install`: the process's `tracing` subscriber, logs on stderr, and `ServiceTelemetry`, its own layer that turns `tracing` spans into OpenTelemetry spans joined to a run's trace by their fields, events into log records, and measurements into metrics exported periodically; sampled per run like run telemetry. Passes the conformance suite of `SpanExporter` on the SDK's in-memory exporters and on an OpenTelemetry Collector in Docker over gRPC and HTTP; the engine is driven under the subscriber to test its spans and metrics ([08](08-decisions.md#taken-while-building-opentelemetry-november-2026), [08](08-decisions.md#taken-while-finishing-service-telemetry-november-2026)). |
| `rewind-cli` | The client commands of the `rewind` binary over `rewind-api`: `run`, `status`, `signal`, `cancel`, `fork`, `trace`, `runs`, `release`, `export`, `toolsets`, with `--json` and the exit codes of 07; the remote server of `REWIND_URL`, or the embedded server of `.rewind/`, started on demand (lock check, detached start, socket wait, version handshake, replacement of another version). In the process: `check` (the checks at start, tsgo and the checker's rules), `test` (scenarios in mock mode with a cassette, TypeScript mocks and `rewind.tools.json`, in virtual time), `replay` of a run or a journal file, with the compatibility report of a corrected script. Ctrl-C finishes the call in flight; `rewind-otel`'s subscriber installed. Tested against a stub embedded server, a stand-in API, and the e2e corpus (every journal replayed, the cases a scenario expresses run through `rewind test`); end to end through the `rewind` binary (`crates/rewind/tests/cli.rs`): a run that sleeps, is interrupted and resumes in another process, a signal from another process, `kill -9` of the server mid-activation, the corpus live on the embedded server with the test as host-tool worker, `rewind test` on a recorded cassette and `rewind fork` of a failed run. |
| Sandbox lib | `crates/rewind-runtime/lib/`: `rewind.d.ts`, `globals.d.ts`, `fetch.d.ts`, tsgo's `tsconfig.json` and `globals.json`, checked with TypeScript 7.0.2. A test checks that the isolate's globals are exactly the declared ones, with and without the `http` tool. |
| `dashboard` | Overview (the home page), Waiting, Failures, Runs, Run, Scripts and Tools views over the API `/v1`, with types generated from `api/openapi.json`: one query bar with suggestions and facets, kept in the URL and followed between views, saved searches in the browser; runs by outcome over time, the runs that wait and what for, failures grouped by fingerprint, as SVG charts; the source with each call, timer, signal wait and log next to its line, and a time cursor that steps through the run, with hovers and watches of the values that crossed the journal; a timeline that compresses idle gaps, with a playhead; payloads on demand, live through the event stream, light and dark themes. Tested with vitest against a mocked API; built to `dashboard/dist/`, which `rewind-server` embeds ([07](07-code-architecture.md#dashboard)). |
| `rewind-server` | The engine host. Every endpoint of `rewind-api` with axum: API keys hashed in the store, roles and label scopes, tenants and their quotas (429), `Idempotency-Key`, problem+json; runs searched with the query language, their statistics and failure groups, from the read model, where task states and quarantine are recorded by whoever changes them; the event stream of a run; host-tool tasks long-polled; inbound webhooks; outbound webhooks signed following Standard Webhooks (passes the `WebhookSender` suite). The worker role: activations, tool dispatch with its claims' heartbeats, the scheduler, the projector (`run.quarantined` included), retention, the retry of quarantined runs, virtual time kept in the store. The embedded server of a `.rewind/` directory: its lock, its socket, its idle stop. The dashboard's build embedded and served. Adapters wired by configuration: Turso or Postgres, files or S3, `rewind-mcp`, `rewind-codegen`, `rewind-check`, `rewind-otel`; the deployment's TTLs, the workers' cache bounds, and the retention of the tenants it creates. Tested over HTTP on Turso and on Postgres (an api server and two workers), and through the `rewind` binary (lock, socket, idle stop, `kill -9` and restart, virtual time). |
| `rewind` | The `rewind` binary: it restarts itself with `TZ=UTC`, then hands `rewind server …` to `rewind-server` and everything else to `rewind-cli`. |
| `evals` | 22 tasks from easy to hard (fan-out, pagination, `ToolError`, signals with and without timeout, sleeping and polling, webhooks, idempotent retries under a rate limit, outputs above 256 KB, `retry_run`, `cancel_run`) over six deterministic fake MCP servers built with `rmcp`; the engine wired in process in virtual time; the SDK's agent tools ported on `rewind-api`'s DTOs; Claude (Messages API) and OpenAI (Responses API) clients and a scripted model; reports per model and prompt rendering (`compact`, `dts`, `search`) in JSON and Markdown. Every task's reference solution passes through the scripted model in `cargo test` ([08](08-decisions.md#taken-while-building-the-evals-november-2026)). |
| `acceptance` | The acceptance suite of 0.1, one command (`acceptance/run.sh`): Playwright specs against the real `rewind` binary, Postgres, MinIO and an OpenTelemetry Collector in Docker, `@modelcontextprotocol/server-everything` through `npx`, and the embedded dashboard in Chromium; it also runs `crates/rewind/tests/cli.rs` and the operations' load and limit tests, and writes the status of each criterion to `acceptance/results/summary.md` ([its README](../acceptance/README.md)). |
| `rewind-load` | The load generator: drives a deployment's API with many concurrent runs that call its fake MCP server and its host tool, sleep and read signals; reports throughput, latencies and errors, and checks that every run ends as expected and no call is sent beyond its attempts ([numbers](#load-tests)). |
| Operations | The image (`Dockerfile`: distroless, non-root, one image for every role); the reference deployment (`deploy/`: Compose with Postgres, MinIO, a Collector, an edge, an api and two workers whose only way out is an egress gateway, gVisor and previous-epoch pools as profiles, `check-walls.sh`); CI (`.github/workflows/ci.yml`: Rust, SDK, dashboard, image, the deployment under load with runc and gVisor); the limits tested through the API (`rewind-server/tests/limits.rs`, the SDK's `test/live.test.ts`). |

## Contracts frozen first

| Contract | Specified in | Implemented by | Consumed by |
| --- | --- | --- | --- |
| Engine ports (traits and conformance suites) | [07](07-code-architecture.md#ports) | The adapters: runtime, stores, blobs, MCP, checker, OpenTelemetry, secrets, webhooks | `rewind-engine` |
| Stored format v1 (events, journal documents, cassettes, pinned toolsets) | [02](02-journal.md#stored-format-v1) | `rewind-format` | The stores, `rewind test`, the e2e corpus, journal export and archive |
| Pinned toolset (schemas, policies, declarations) | [04](04-tools.md#toolsets-and-code-generation) | `rewind-codegen` | The runtime (`rewind:tools`), the checker, the dispatcher, the SDK, the model's prompt |
| HTTP API `/v1` (OpenAPI) | [07](07-code-architecture.md#http-api-v1) | `rewind-api` (DTOs, OpenAPI document, Rust client), `rewind-server` | The CLI, the SDK and its agent tools, the dashboard, the evals |
| Sandbox lib (`rewind.d.ts` and the checker's `lib`) | [04](04-tools.md#type-checking) | `rewind-runtime` (the globals that exist) | `rewind-check`, the prompt |
| Access and identity: the catalog of permissions and the built-in roles, `AccessStore`, `IdentityProvider`, `/auth` | [06](06-storage-and-scale.md#tenancy-identity-and-access), [07](07-code-architecture.md#sign-in-auth) | `rewind-api`, `rewind-engine` (ports, in-memory adapters, the conformance of `AccessStore`) | The stores, `rewind-workos`, `rewind-server`, the dashboard |

Each contract is reviewed by the owner, with diagrams, before the code that depends on it.
The six were approved in November 2026, access and identity on the 14th.

## Workstreams

| Workstream | Crates | What remains | Depends on (contracts) |
| --- | --- | --- | --- |
| Domain and format | `rewind-core`, `rewind-format` | Nothing beyond what the other workstreams need | Stored format v1, pinned toolset |
| Engine | `rewind-engine` | Nothing beyond what `rewind-server` wires: the worker, scheduler, projector and retention loops | Engine ports, stored format v1, pinned toolset |
| Runtime (V8) | `rewind-runtime` | Nothing | Engine ports, pinned toolset, sandbox lib |
| Stores | `rewind-store-turso`, `rewind-store-postgres` | `AccessStore` (keys moved out of `JournalStore`, roles, grants, default roles), migration 2 on Postgres | Engine ports, stored format v1, access and identity |
| Identity | `rewind-workos` | Everything: `IdentityProvider` on WorkOS | Access and identity |
| Blobs | `rewind-blob` | Files and S3; the `payloads` table with size and preview; workers' local cache; cleanup | Engine ports |
| Tools | `rewind-codegen`, `rewind-mcp` | `rewind-codegen`: JSON Schema → TypeScript, prompt rendering, observed types. `rewind-mcp`: stdio and HTTP, idempotency key and trace context in `_meta`. Built-ins `http` and `webhook`; credentials | Engine ports, pinned toolset |
| Type-checking | `rewind-check` | tsgo; the checker's own rules; the sandbox lib | Engine ports, pinned toolset, sandbox lib |
| Observability | `rewind-otel` | Nothing beyond what `rewind-cli` wires: the subscriber, and the exporter given to the projector | Engine ports, stored format v1, HTTP API `/v1` |
| Server | `rewind-server`, `rewind-api` | `/auth`, access tokens, grants and permissions on every endpoint, the organization, member, invitation, role and key endpoints, the wiring of WorkOS | Engine ports, HTTP API `/v1`, access and identity |
| CLI | `rewind-cli` | Client of the embedded or a remote server, through `rewind-api`; local commands: `check`, `test`, replay of an exported journal; exit codes | HTTP API `/v1`, stored format v1, sandbox lib |
| TypeScript SDK | `sdk/typescript` | Client; host-tool worker; agent tools; adapters for the Anthropic and OpenAI tool formats | HTTP API `/v1`, pinned toolset |
| Dashboard | `dashboard` | Sign-in, onboarding, the organization picker, the waitlist, the Organization views; the views following the caller's permissions | HTTP API `/v1`, access and identity |
| Evals | `evals` | Tasks; deterministic fake MCP servers; the loop generate → check → run → fix; reports per model | HTTP API `/v1`, pinned toolset, sandbox lib |
| Operations | `Dockerfile`, `deploy/`, CI, `rewind-load` | WorkOS's variables on Railway, `/auth` rewritten by Vercel and the edge | Engine ports, HTTP API `/v1`, access and identity |

## Designs settled (November 2026)

Each design below was reviewed by the owner, with diagrams. The list points to where it is
specified.

- **Embedded mode**: the CLI is an API client of `rewind server --embedded`
  ([07](07-code-architecture.md#embedded-server-and-cli)).
- **Sticky execution and a thread per instance** (was question 2)
  ([03](03-execution.md#sticky-execution)).
- **Payloads in three tiers** ([06](06-storage-and-scale.md#payloads)).
- **Fork semantics** (was question 9) ([03](03-execution.md#forks)).
- **Tool task lifecycle**: host tools that last days, the `approval` policy, tool
  concurrency, toolset change during a run ([04](04-tools.md#tool-task-lifecycle)).
- **Credentials** resolved at dispatch ([04](04-tools.md#credentials)).
- **API authentication and tenancy** ([06](06-storage-and-scale.md#tenancy-identity-and-access)).
- **Accounts, organizations and access**: WorkOS for identity only, Rewind's permissions, roles and
  grants, sign-in on the dashboard's pages ([06](06-storage-and-scale.md#tenancy-identity-and-access)).
- **Egress of the `http` tool** ([04](04-tools.md#the-http-tool-and-egress)), and of MCP
  servers ([04](04-tools.md#where-mcp-servers-may-be)).
- **Retention and deletion** (was question 11)
  ([06](06-storage-and-scale.md#retention-and-deletion)).
- **Sandboxed workers** ([06](06-storage-and-scale.md#sandboxed-workers)).
- **tsgo in `rewind-check`** and the checker's own rules ([04](04-tools.md#type-checking)).
- **The JSON Schema → TypeScript converter**: ours, in Rust
  ([04](04-tools.md#toolsets-and-code-generation)).
- **The dashboard**: a React SPA served by `rewind-server`
  ([05](05-testing-and-observability.md#dashboard)).
- **The dashboard's debugger and search**: the time cursor, the waiting runs, failure groups,
  the query language ([05](05-testing-and-observability.md#query-language)).
- **Evals**: the models, run on demand ([05](05-testing-and-observability.md#evals)).

## Out of 0.1

- `rewind replay <run> --inspect`, which attaches the V8 inspector to step through a past run.
- `continueAsNew`. A run that reaches the history cap (50k entries) is aborted with
  `limit_exceeded`.
- A columnar read model. The read model lives in Postgres.
- Sharding over several Postgres clusters.

## 0.1 is done when

- A harness drives runs through the SDK, with several workers on one Postgres. Each run
  appears as one trace under the harness's span, and runs can be searched by label and tool.
- Locally, `rewind run`, `rewind signal`, `rewind replay` and `rewind trace` work on a run that sleeps,
  is interrupted, and resumes in another process. A forgotten `await`, an unhandled
  rejection and a deadlock each end as specified.
- A script calls a real MCP server, with types checked beforehand.
- The evals report, per model and prompt rendering, the first-try check pass rate, task
  success, iterations and tokens.
- `rewind test` replays a recorded real run as a scenario. `rewind fork` continues a failed run
  with a corrected script without repeating its recorded calls.
- Load tests and limit tests pass.
- An operator finds any run by label, tool or error in the dashboard, and sees its code next
  to its trace. They step through a run, see the runs that wait and what for, and the
  failures grouped by cause.
- Someone signs in to the dashboard with a code by email, creates an organization, invites a
  member with a role, and creates an API key; each endpoint refuses a caller without its
  permission.

### Acceptance status (November 2026)

`acceptance/run.sh` proves each criterion end to end on the real `rewind` binary; what each check
does is in [acceptance/README.md](../acceptance/README.md).

| Criterion | Status | Proven by |
| --- | --- | --- |
| SDK, several workers on one Postgres, one trace per run, search by label and tool | Passes | `acceptance/tests/c1-sdk-workers-postgres.spec.ts`: an api server and two workers on Postgres, blobs on MinIO, a Collector writing to a file; host tools served by the SDK's worker, a sleep, a signal, an approval |
| Locally across processes; forgotten `await`, unhandled rejection, deadlock | Passes | `crates/rewind/tests/cli.rs` (sleep, `kill -9`, resume in another process; `run`, `signal`, `status`, `trace`, `replay`) and `acceptance/tests/c2-local-endings.spec.ts` |
| A real MCP server, types checked beforehand | Passes | `acceptance/tests/c3-c5-mcp-test-fork.spec.ts` with `@modelcontextprotocol/server-everything` 2.2.1 over stdio |
| The evals' report per model and prompt rendering | Passes with scripted models; the run with Claude and OpenAI models is pending API keys | `acceptance/tests/c4-evals.spec.ts` |
| `rewind test` of a recorded real run; `rewind fork` without repeating calls | Passes | `acceptance/tests/c3-c5-mcp-test-fork.spec.ts`, checked against the MCP server's own log of the calls it received |
| Load tests and limit tests | Passes | `deploy/acceptance.sh`: the limits through the API, then the reference deployment loaded by `rewind-load` with the thresholds of [Load tests](#load-tests), and the agent tools' bound against it |
| The dashboard: find by label, tool or error; code next to the trace | Passes | `acceptance/tests/c7-dashboard.spec.ts`, in Chromium against the dashboard `rewind server` serves |

## Load tests

`deploy/acceptance.sh` runs the reference deployment (`deploy/`: an api server and two workers
of 2 CPUs and 1 GiB each, Postgres 17, MinIO, a Collector, the egress gateway) and loads it with
`rewind-load`. Each run makes an MCP call, a host-tool call, an MCP call that fails transiently once
and is retried after its backoff (0.5–1 s), sleeps 1 s, reads a signal sent twice with one
idempotency key, and waits 100 ms for a second one: about seven activations. A load passes when
every run completed with its expected output, no tool call reached its tool more times than the
run's attempts, every run was in the read model within 3 minutes of the last run's end, and these
thresholds hold:

| Load | Runs/s at least | p99 end to end at most | p99 activation at most |
| --- | --- | --- | --- |
| 1,000 runs, 100 at once | 10 | 15 s | 2 s |
| 200 runs, 50 at once, 100 KiB payloads (S3) | 4 | 20 s | 3 s |

Measured in November 2026 on a MacBook (Apple silicon, Docker Desktop with 8 CPUs and 3.9 GiB,
other builds running), latencies in milliseconds:

| Load | Runs/s | Tool calls/s | Start p50 / p99 | First call p50 / p99 | Activation p50 / p99 | End to end p50 / p99 | Read model complete after |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1,000 runs, 100 at once | 17.3 | 69 | 488 / 3,418 | 996 / 4,408 | 176 / 610 | 5,325 / 8,733 | 19 s |
| 200 runs, 50 at once, 100 KiB payloads | 13.0 | 52 | 370 / 1,551 | 244 / 643 | 122 / 317 | 3,705 / 4,486 | 0.1 s |

No error and no duplicated call in either. In two runs of `acceptance/run.sh` on the merged
tree, with more builds running, the same loads gave 12.8–20.1 and 8.8–10.5 runs/s, end-to-end
p99 of 10.5–12.3 and 5.5–7.0 s, and activation p99 of 690–930 and 460–960 ms: the thresholds
leave room for a busy machine, not for a regression of several times. The floor of a run's end-to-end latency is about 3 s:
the sleep, the retry's backoff and the signal's timeout. Starts dominate the rest: each start
type-checks the script with tsgo in the api server, so a burst of starts queues there. An
activation (a host tool's result accepted to the run's next MCP call) takes 120–180 ms at the
median, most of it the up to 250 ms an api server's write waits before another process's worker
polls it ([08](08-decisions.md#taken-while-building-the-server-november-2026)). Under the
heavier load the projector lags the journal by up to 20 s, which `GET /v1/runs` and the
dashboard show.

## Open questions

Questions 1, 2, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 and 17 were settled in November
2026: see [08-decisions.md](08-decisions.md#taken-after-the-layering-november-2026). Questions
2, 9 and 11 were settled with the [designs](#designs-settled-november-2026). Question 6
(`continueAsNew`) is [out of 0.1](#out-of-01). No other question is open.
