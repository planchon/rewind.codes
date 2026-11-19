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
findings fixed. Accounts, organizations and access
([06](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)), designed on November 14,
2026, are built too: `rewind-workos`, `AccessStore` on both stores, a permission on every
endpoint, `/auth` and the organization endpoints in `rewind-server`, the dashboard's sign-in and
Organization views. Namespaces ([06](06-storage-and-scale.md#namespaces)), designed the same day,
are built too: end users' tokens (opaque and signed), `Rewind-Namespace`, the six quotas with
their alerts, credentials per namespace, the erasure of a namespace, `rewind namespaces`, the
SDK's `rewind.namespace(name)` and the dashboard's filter and Namespaces views.
On November 15, 2026, with accounts, namespaces and workflows together: `cargo fmt --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo doc --workspace --no-deps` with
warnings as errors and `cargo test --workspace` (998 tests, Postgres, MinIO and a Collector in
Docker; `rewind-workos` also against WorkOS Staging) pass, the SDK's and the dashboard's checks
pass, and `acceptance/run.sh` passes the eight criteria of [0.1 is done when](#01-is-done-when),
the eighth against WorkOS Staging. A run of a workflow acts for an end user: the request's
namespace, or its parent's when a script starts it ([08](08-decisions.md#taken-while-merging-the-namespaces-and-the-workflows-november-2026)).
Not yet decided: whether the workflows an end user's agent saves belong to its namespace; they
are the tenant's, scoped by labels. Not yet done: the evals with real models (they need API
keys), a run of the CI workflow on GitHub, and the gVisor profile on a Linux host.

Later on November 15, 2026, the dashboard was redesigned on Geist, Vercel's design system
([08](08-decisions.md#taken-for-the-dashboards-redesign-november-2026)): one sidebar that turns
into the page's own, Observability (every run, the active, the waiting and the failures, with
the filters in its sidebar) in place of Runs, Waiting and Failures, Run in sections, the
organization from the member's menu, Find, and no namespace filter. Its checks (vitest, Biome,
TypeScript, the build) pass; the acceptance suite's dashboard steps (criteria 7 and 8) were
moved to the new frame and have not been run against the binary since.

On November 14, 2026, [workflows across runs](03-execution.md#workflows-across-runs) were built
through every layer: the workflow version and its pinning in toolsets (`rewind-core`, format v1),
the store's workflows and watchers (both stores, schema version 4, the conformance suites), the
engine's registration, starts of workflow runs, built-ins and ends (live, forks, mock mode and
cassettes), the glue of `rewind:workflows` in epoch 1, codegen and the checker's
`check_workflow`, `/v1/workflows` and the `workflow:` and `started_by:` terms, the SDK's
`workflows` resource and `save_workflow`, and the dashboard's links between runs. Eight cases
joined the corpus. `cargo fmt --check`, clippy, `cargo doc` with warnings as errors and
`cargo test --workspace` (961 tests once merged with access, Postgres, MinIO and a Collector in
Docker) pass, and so do the SDK's and the dashboard's checks. The workflow endpoints take the
permissions `toolsets:read` and `toolsets:write`.

On November 17, 2026, [workflow test suites](05-testing-and-observability.md#testing-and-mocks),
designed with the owner on November 16, were built through every layer: tests, suites and paths
in `rewind-core`; the test document and the suite of the workflow version document in format v1
(a version without a suite keeps its digest); the test runner, the checks for stale and
incompatible tests, the gate of `register_workflow`, recording one test per path and the cleanup
and erasure of tests in `rewind-engine`, with the index of tests in both stores (Postgres
migration 6, Turso schema 6, the conformance suite); `TsMockLoader` in `rewind-runtime`;
`POST /v1/test` with tests and a workflow's suite, `POST /v1/workflows` through the gate,
`POST /v1/workflows/{name}/record`, `GET /v1/tests/{digest}` and the gate of a fork of a run of a
workflow, with the problems `tests-failed` and `invalid-test`; the edge sending what runs tests to
the workers; `rewind test` on a script and its tests file and `rewind workflows record`; the SDK's
`test_script`, `save_workflow`, `get_workflow` and `record_tests`; the dashboard's Workflows view;
the documentation; and the evals' repair tasks (`fix_workflow_regression`,
`upgrade_workflow_suite`), which count the tests models write and the fixes the gate refuses. The
tests of every Rust crate it touched, clippy and rustfmt pass, and so do the SDK's, the
dashboard's and the documentation's checks. Not yet done: the example `examples/testing`, which
waits for a release of the SDK with these changes (the examples pin the published SDK), the
acceptance suite and the release gate run against the binary, and a run of CI.

| Crate | State |
| --- | --- |
| `rewind-core` | Pure domain, no serde: ids (`Seed` on 256 bits, `Epoch`), RFC 8785 canonical JSON (golden vectors of the RFC), payloads, the four event families (`signal_awaited` and `invalid_script` included), `RunState`, history validation, `ReplayCursor`; the pinned toolset (MCP servers and their transports; tools with route, description, input and output schemas, policy) and its digest; tool outcomes; cassettes derived from a history; observed result shapes. |
| `rewind-format` | Format v1: one event per row, journal documents (the run's id, script and pinned toolset, then its entries), cassettes, pinned toolsets (canonical, hashing to the toolset's digest), values in canonical form. Golden fixtures pin every variant's spelling; unknown versions, types and fields are refused. |
| `rewind-engine` | The final ports of 0.1, each with an in-memory adapter; the six conformance suites, of `JournalStore`, `BlobStore`, `ReadModel`, `SecretStore`, `SpanExporter` and `WebhookSender`, passed by the in-memory adapters. Use cases: start and fork (script checks, the type-check through the `TypeChecker` port unless skipped, idempotency key, TTL, the tenant's quota, credentials), signal with idempotency key, cancel, activate, fire timers, expire runs, quarantine and give up, replay, export of journal documents, forks (with the parent's calls reused, keys of calls left without an outcome, and the compatibility report). Activation with the commit invariant, limits and the cancellation grace; sticky execution in an LRU instance cache bounded by count and memory; the lease renewed every 10 s; quarantine after 5 failed activations, those whose worker died included, retried once by a new build. Tool tasks: policies and backoff, claims heartbeated up to the execution timeout, credentials resolved at dispatch, parked tasks and their webhooks, approvals, the host-tool worker's claim, heartbeat, deferral and answers by token, the `webhook` built-in, a routing dispatcher. Payloads in three tiers. The projector (read model with each run's snapshot, its waits and the fingerprint of how it failed, run telemetry with deterministic span ids, run metrics, lifecycle webhooks); service telemetry through `tracing`: the activation span under the run span, the tool attempt span under the tool call span, and the measurements of `rewind_engine::metrics`; tests in mock mode (rules, mock handlers, cassettes, static responses), checked for stale and incompatible tests, several at once; suites composed from a parent's and the gate on save; tests recorded from runs, one per path; the cleanup of tests and their erasure with a namespace, retention and deletion, the LLM trace. Namespaces: the bounds of a start and a fork, the CPU time of the calls that execute counted per namespace and day and written every 10 s, quota alerts sent as `quota.alert`, credentials per namespace, the erasure of a namespace in the retention jobs. Driver in real and virtual time, which jumps to timers, tool call retries and deadlines; use-case tests on a fake executor. |
| `rewind-runtime` | `V8Executor`: the checks at start (syntax, imported modules and names, default export), an isolate per instance on its own thread, started from the epoch's V8 snapshot, the epoch-1 glue, logical time, sfc32 and ChaCha20 keyed by the seed, `run.id` and `run.startedAt`, the web-standard globals of `deno_web`, how a script ends (forgotten `await`, unhandled rejection, `DeadlockError`), a CPU watchdog on the isolate thread's CPU clock, which also reports each call's CPU time, and a heap limit; args validated against the pinned schema inside the op; `webhook()`; `fetch` with the `http` tool, in the format of `rewind-mcp`'s; `rewind:tools` generated from routes, MCP content with `text()` and `json()`; TypeScript mock handlers as a `ToolDispatcher` (`TsMocks`); a budget of array buffers (resizable ones included), built-ins measured against the heap before V8 runs them to their end, and the stack limit pinned before each call (the `memory` module, the only `unsafe` code); the memory each instance holds, from V8's heap statistics, for the sticky cache's bound; the JavaScript of `deno_web` and `deno_webidl` vendored and given to the snapshot build from memory, so the binary runs on a machine without the cargo registry ([08](08-decisions.md#taken-while-building-operations-november-2026)). |
| `rewind-store-turso` | `JournalStore`, `ReadModel` and `SecretStore` on Turso Database (`turso` 0.8.3): the tables of 06, events in format v1, schema version 1 with migrations embedded and a newer schema refused, settings and virtual time in `meta`, secrets encrypted with a data key per tenant under the master key; the read model's labels, waits, task states, quarantine and texts, searched with `instr`, and statistics counted in Rust. Namespaces (schema 3): their quotas, use per day, alerts and script counts, the quota checks of `create_run`, end users' tokens and token keys. Passes the four conformance suites, each check on a new database file; `examples/load.rs` measures commit latency and throughput ([08](08-decisions.md#taken-while-building-the-embedded-store-november-2026)). |
| `rewind-store-postgres` | `JournalStore`, `ReadModel` and `SecretStore` on Postgres with sqlx 0.8: `FOR UPDATE SKIP LOCKED`, fencing, tenant in every key, fair `acquire_runnable`, advisory locks for the claim limits, embedded migrations run under a lock; `history` and `inbox` keyed by `run_id` and tested hash-partitioned; the read model's query as SQL over indexes scoped by tenant, text through `pg_trgm`, statistics and failure groups as single statements. Namespaces (migration 3): their quotas, use per day, alerts and script counts, the quota checks of `create_run` under an advisory lock of the tenant, end users' tokens and token keys. Passes the conformance suites against Postgres 17 in Docker, and a concurrency test with 3 engines × 4 workers on one database; about 830 activations/s with 4 × 4 on a laptop (`cargo run --release -p rewind-store-postgres --example postgres_load`). |
| `rewind-blob` | `BlobStore` on local files (`FileBlobs`: atomic writes, staged, synced and renamed, under `{tenant}/sha256/`) and on S3 or any S3-compatible service (`S3Blobs`, through `object_store`, with conditional puts); `CachedBlobs`, the workers' cache in memory, bounded in bytes, in front of either. Digests checked on every put and get. Passes the conformance suite of `BlobStore` on files and on MinIO in Docker ([08](08-decisions.md#taken-while-building-the-blob-stores-november-2026)). |
| `rewind-workos` | `WorkOs`, the `IdentityProvider` port on WorkOS through its SDK `workos` 3.7: GitHub's authorization URL and code, Magic Auth, email verification, the choice of an organization, refreshes onto an organization, each with the browser's IP address and user agent and the invitation's token; WorkOS's steps and a closed sign-up as answers; access tokens checked locally against the JWKS (RS256, issuer, `client_id`, expiry with 30 s of leeway); organizations, users, active memberships and invitations without roles, the waitlist; WorkOS's messages passed on without the secrets sent. Tested against a fake WorkOS (wiremock, test RSA keys, a rotation of keys) and against WorkOS Staging when its variables are set ([08](08-decisions.md#taken-for-accounts-organizations-and-access-november-2026)). |
| `rewind-architecture-tests` | The dependency rules, telemetry and `anyhow` included, each with a test proving it catches its violation; `rewind-agent-tools` and `rewind-mcp-server` classified as adapter support, on `rewind-api` and never the engine. |
| `rewind-e2e` | The epoch-1 corpus: 30 blue and 31 red cases, forks, tool policies, MCP content and the `http` tool's format included, recorded and replayed from their journal documents; tests proving the harness cannot pass silently; the contract test of codegen, the checker and the runtime (`tests/declarations.rs`); `cargo run -p rewind-e2e -- replay <case> [--script file.ts]`. |
| `rewind-api` | The HTTP API `/v1` as a contract: the DTOs of every endpoint, problem+json errors, the query language of the run lists (parsed with `winnow`, errors with the span of the term at fault), `Idempotency-Key`, the event stream, webhook payloads signed following Standard Webhooks and their verification, the OpenAPI 3.1 document committed at `api/openapi.json` (a test fails when it is stale), and an async client over HTTP(S) and over the embedded server's unix socket. |
| `rewind-check` | `TsgoChecker`, the `TypeChecker` port: tsgo 7.0.2 embedded as its npm tarball, pinned by its integrity and extracted to a cache directory at first use; checks in strict mode against the sandbox lib and `rewind-tools.d.ts`, reading nothing outside its directory; diagnostics with a location, a hint and a code frame; the rules `floating-promise`, `ignored-async-callback` and `absent-global` on the swc syntax tree. About 50 ms per check. |
| `rewind-codegen` | Our JSON Schema → TypeScript converter (objects, `required`, enums, `anyOf`/`oneOf`/`allOf`, `$ref`/`$defs`, arrays, `additionalProperties`); `rewind-tools.d.ts` (one namespace per MCP server or host-tool group, `ToolContent` for MCP content, observed types marked `@observed`, the `fetch` reference with the `http` tool); the compact prompt rendering and each tool's own declaration (for `search_tools`). Golden tests on GitHub-like, FastMCP-like and mixed toolsets. |
| `rewind-mcp` | `McpDispatcher`: an MCP client (`rmcp` 3.9) over streamable HTTP and stdio, discovery as a toolset pins it, connections pooled by tenant, server and credentials, the idempotency key and the trace context in `_meta`, credentials in the transport, the schema check at dispatch (a changed tool parks the call), the execution timeout, errors mapped to transient failures, failures and parked calls; on a deployed server, MCP servers reached only at public addresses or at the internal hosts the operator allows, whose answers are never quoted (`McpEgress`, `InternalAllowList`). `HttpTool`: the `http` built-in and its egress rules (allow-list, one resolution, special-purpose addresses refused, the checked address connected, redirects checked again, 5 MiB and 30 s). Tested against fixture servers over HTTP and stdio. |
| `rewind-agent-tools` | The agent tools in Rust: the SDK's set, its definitions word for word, and the MCP set (no `save_workflow`; `list_orgs`, `list_namespaces`, `list_toolsets`; `org`, `namespace` and `toolset` as arguments; each tool's title, annotations and permission); the handlers over the ports `Api` and `McpApi`, one method per operation of `/v1`, as the SDK's `AgentTools` (keys of runs and signals, the labels' scope, `save_workflow`'s derived toolset, refusals as results and faults as errors); the bounded texts of the SDK's `render.ts` and `bound.ts`, and the MCP bounds (80 000 bytes, `wait` 30 s and 200 s at most); `rewind_api::Client` as an `McpApi`. Tested on its own fake API, and through the evals. The golden fixtures shared with the SDK's tests are not written yet. |
| `rewind-mcp-server` | Rewind's MCP server: an `rmcp` `ServerHandler` (`initialize` with the instructions, `tools/list` filtered by the caller's organizations, `tools/call` resolving `org`, checking the permission, then running the agent tool with an API bound to the organization and the namespace), for the callers its host's `Connector` gives; served over Streamable HTTP without session (`http_service`, JSON answers, `GET` and `DELETE` 405) and over stdio. Tested with a fake connector and API: the tools per permission, `org` resolution, titles and annotations, the bound, the namespace of checks, tests and runs, and whole exchanges through rmcp's client over a stream and over HTTP. `POST /mcp` is not mounted in `rewind-server` yet. |
| `rewind-otel` | OpenTelemetry 0.33, OTLP over gRPC or HTTP with protobuf, both with TLS, configured or from the `OTEL_*` variables. `OtelSpanExporter`, the `SpanExporter` port: run telemetry exported with the projector's ids, under the scope `rewind.run`. `install`: the process's `tracing` subscriber, logs on stderr, and `ServiceTelemetry`, its own layer that turns `tracing` spans into OpenTelemetry spans joined to a run's trace by their fields, events into log records, and measurements into metrics exported periodically; sampled per run like run telemetry. Passes the conformance suite of `SpanExporter` on the SDK's in-memory exporters and on an OpenTelemetry Collector in Docker over gRPC and HTTP; the engine is driven under the subscriber to test its spans and metrics ([08](08-decisions.md#taken-while-building-opentelemetry-november-2026), [08](08-decisions.md#taken-while-finishing-service-telemetry-november-2026)). |
| `rewind-cli` | The client commands of the `rewind` binary over `rewind-api`: `run`, `status`, `signal`, `cancel`, `fork`, `trace`, `runs`, `release`, `export`, `toolsets`, with `--json` and the exit codes of 07; `rewind mcp`, the agent tools of the MCP set over stdio against the embedded server, as its tenant `local`, for debugging (never a remote server: `--url` is a usage error), tested end to end with rmcp's client against a stand-in server on the socket; the remote server of `REWIND_URL`, or the embedded server of `.rewind/`, started on demand (lock check, detached start, socket wait, version handshake, replacement of another version). In the process: `check` (the checks at start, tsgo and the checker's rules), `test` (a script's tests file in mock mode, with cassettes, TypeScript mocks and `rewind.tools.json`, in virtual time; on the server for a workflow's suite), `workflows record`, `replay` of a run or a journal file, with the compatibility report of a corrected script. Ctrl-C finishes the call in flight; `rewind-otel`'s subscriber installed. Tested against a stub embedded server, a stand-in API, and the e2e corpus (every journal replayed, the cases a test expresses run through `rewind test`); end to end through the `rewind` binary (`crates/rewind/tests/cli.rs`): a run that sleeps, is interrupted and resumes in another process, a signal from another process, `kill -9` of the server mid-activation, the corpus live on the embedded server with the test as host-tool worker, `rewind test` on a recorded cassette and `rewind fork` of a failed run. |
| Sandbox lib | `crates/rewind-runtime/lib/`: `rewind.d.ts`, `globals.d.ts`, `fetch.d.ts`, tsgo's `tsconfig.json` and `globals.json`, checked with TypeScript 7.0.2. A test checks that the isolate's globals are exactly the declared ones, with and without the `http` tool. |
| `dashboard` | Overview (the home page), Observability (every run, the active, the waiting and the failures), Run, Scripts, Tools and Namespaces views over the API `/v1`, with types generated from `api/openapi.json`, on Geist's tokens and Base UI's menus and dialogs: one sidebar that turns into the page's own (Observability's views, saved searches and filters; a run's sections and details; the organization's parts), the organization picker and Find at its top, the account at its bottom; one query bar with suggestions, its filters terms of the query, kept in the URL and followed between views, saved searches in the browser; runs by outcome over time, the runs that wait and what for, failures grouped by fingerprint, as SVG charts; the source with each call, timer, signal wait and log next to its line, and a time cursor that steps through the run, with hovers and watches of the values that crossed the journal; a timeline that compresses idle gaps, with a playhead; payloads on demand, live through the event stream, light and dark themes. With WorkOS, the sign-in on its own pages (GitHub, a code by email, WorkOS's steps, the waitlist), the onboarding (create an organization, accept an invitation), the organization picker and the Organization views (members, invitations, roles, API keys, settings), opened from the member's menu; every view following the permissions `whoami` answers; without WorkOS, an API key. Tested with vitest against a mocked API; built to `dashboard/dist/`, which `rewind-server` embeds ([07](07-code-architecture.md#dashboard)). |
| `rewind-server` | The engine host. Every endpoint of `rewind-api` with axum: API keys hashed in the store and members' access tokens checked by the identity provider, grants, roles and label scopes, no one giving more than they hold, organizations, members and invitations with their first grants (an invitation's, else the default role), tenants and their quotas (429), `Idempotency-Key`, problem+json; runs searched with the query language, their statistics and failure groups, from the read model, where task states and quarantine are recorded by whoever changes them; the event stream of a run; host-tool tasks long-polled; inbound webhooks; outbound webhooks signed following Standard Webhooks (passes the `WebhookSender` suite). The worker role: activations, tool dispatch with its claims' heartbeats, the scheduler, the projector (`run.quarantined` included), retention, the retry of quarantined runs, virtual time kept in the store. The dashboard's sign-in `/auth` on the api role (GitHub, codes by email, WorkOS's steps, the waitlist, refreshes and switches), its cookies sealed with `rewind-workos`'s `CookieSeal`; an event stream of an access token closed at its expiry. The embedded server of a `.rewind/` directory: its lock, its socket, its idle stop. The dashboard's build embedded and served. Adapters wired by configuration: Turso or Postgres, files or S3, WorkOS (`rewind-workos`, the four `WORKOS_*`), `rewind-mcp`, `rewind-codegen`, `rewind-check`, `rewind-otel`; the deployment's TTLs, the workers' cache bounds, and the retention of the tenants it creates. Tested over HTTP on Turso and on Postgres (an api server and two workers), the sign-in with a cookie jar and the engine's fake of WorkOS, and through the `rewind` binary (lock, socket, idle stop, `kill -9` and restart, virtual time). |
| `rewind` | The `rewind` binary: it restarts itself with `TZ=UTC`, then hands `rewind server …` to `rewind-server` and everything else to `rewind-cli`. |
| `evals` | 22 tasks from easy to hard (fan-out, pagination, `ToolError`, signals with and without timeout, sleeping and polling, webhooks, idempotent retries under a rate limit, outputs above 256 KB, `retry_run`, `cancel_run`) over six deterministic fake MCP servers built with `rmcp`; the engine wired in process in virtual time, behind `rewind-agent-tools`' port `Api`, which the world implements as the server answers an API key limited to the episode's labels ([08](08-decisions.md#taken-while-building-the-agent-tools-and-the-mcp-server-november-2026)); Claude (Messages API) and OpenAI (Responses API) clients and a scripted model; reports per model and prompt rendering (`compact`, `dts`, `search`) in JSON and Markdown. Every task's reference solution passes through the scripted model in `cargo test` ([08](08-decisions.md#taken-while-building-the-evals-november-2026)). |
| `acceptance` | The acceptance suite of 0.1, one command (`acceptance/run.sh`): Playwright specs against the real `rewind` binary, Postgres, MinIO and an OpenTelemetry Collector in Docker, `@modelcontextprotocol/server-everything` through `npx`, the embedded dashboard in Chromium, and WorkOS Staging for the sign-in; it also runs `crates/rewind/tests/cli.rs` and the operations' load and limit tests, and writes the status of each criterion to `acceptance/results/summary.md` ([its README](../acceptance/README.md)). |
| Release gate | `acceptance/release.sh`, run before any release ([its README](../acceptance/release/README.md)): the tree built in release mode; criteria 1, 2, 3, 5 and 7; thirteen areas on a deployed setup behind an edge (Postgres, MinIO, a Collector, an api server, two workers) and a strict one with a deployed server's defaults, about 330 checks, each a success, a failure or an attack that must be refused with no effect; the lifecycle (workers killed, a full restart, Postgres paused, archive, the erasure of a run and of a namespace); sweeps of every recorded exchange and log (no unexpected 5xx, no panic, no leaked canary secret, every problem type provoked); the load test. A report says GO, NO-GO or PARTIAL (`acceptance/results/release/report.html`). Its first runs found seven deviations from the spec, all fixed: the label filter of `GET /v1/scripts`, the 80 % threshold of quota alerts, host tools' schemas compiled at registration, a bare `require(…)` refused at start, `/auth` answering 404 whatever its body without WorkOS, an oversized body refused from its `Content-Length` before `100 Continue`, and an expired claim of an `at_most_once` host tool failed at once, even when no worker polls again (04). |
| `rewind-load` | The load generator: drives a deployment's API with many concurrent runs that call its fake MCP server and its host tool, sleep and read signals; reports throughput, latencies and errors, and checks that every run ends as expected and no call is sent beyond its attempts ([numbers](#load-tests)). |
| Operations | The image (`Dockerfile`: distroless, non-root, one image for every role); the reference deployment (`deploy/`: Compose with Postgres, MinIO, a Collector, an edge, an api and two workers whose only way out is an egress gateway, gVisor and previous-epoch pools as profiles, `check-walls.sh`); CI (`.github/workflows/ci.yml`: Rust, SDK, dashboard, image, the deployment under load with runc and gVisor); the limits tested through the API (`rewind-server/tests/limits.rs`, the SDK's `test/live.test.ts`). |

## Contracts frozen first

| Contract | Specified in | Implemented by | Consumed by |
| --- | --- | --- | --- |
| Engine ports (traits and conformance suites) | [07](07-code-architecture.md#ports) | The adapters: runtime, stores, blobs, MCP, checker, OpenTelemetry, secrets, webhooks | `rewind-engine` |
| Stored format v1 (events, journal documents, cassettes, pinned toolsets, workflow versions, tests) | [02](02-journal.md#stored-format-v1) | `rewind-format` | The stores, `rewind test`, the e2e corpus, journal export and archive, suites |
| Pinned toolset (schemas, policies, declarations) | [04](04-tools.md#toolsets-and-code-generation) | `rewind-codegen` | The runtime (`rewind:tools`), the checker, the dispatcher, the SDK, the model's prompt |
| HTTP API `/v1` (OpenAPI) | [07](07-code-architecture.md#http-api-v1) | `rewind-api` (DTOs, OpenAPI document, Rust client), `rewind-server` | The CLI, the SDK and its agent tools, the dashboard, the evals |
| Sandbox lib (`rewind.d.ts` and the checker's `lib`) | [04](04-tools.md#type-checking) | `rewind-runtime` (the globals that exist) | `rewind-check`, the prompt |
| Access and identity: the catalog of permissions and the built-in roles, `AccessStore`, `IdentityProvider`, `/auth` | [06](06-storage-and-scale.md#tenancy-namespaces-identity-and-access), [07](07-code-architecture.md#sign-in-auth) | `rewind-api`, `rewind-engine` (ports, in-memory adapters, the conformance of `AccessStore`) | The stores, `rewind-workos`, `rewind-server`, the dashboard |
| Namespaces: their quotas, usage and alerts, end users' tokens and token keys, credentials per namespace, `Rewind-Namespace` | [06](06-storage-and-scale.md#namespaces), [07](07-code-architecture.md#ports) | `rewind-api`, `rewind-engine` (ports, in-memory adapters, conformance) | The stores, `rewind-runtime`, `rewind-server`, the CLI, the SDK, the dashboard |
| Connections and MCP: the connection's tables, grants and tokens, `Rewind-Organization`, `/mcp`, `/oauth`, the tools' definitions | [06](06-storage-and-scale.md#connections), [07](07-code-architecture.md#mcp-and-oauth) | `rewind-api`, `rewind-engine` (`AccessStore`), `rewind-agent-tools` | The stores, `rewind-server`, `rewind-mcp-server`, the CLI, the evals, the dashboard |

Each contract is reviewed by the owner, with diagrams, before the code that depends on it.
The seven were approved in November 2026, access and identity and namespaces on the 14th,
connections and MCP on the 19th.

## Workstreams

| Workstream | Crates | What remains | Depends on (contracts) |
| --- | --- | --- | --- |
| Domain and format | `rewind-core`, `rewind-format` | Nothing beyond what the other workstreams need | Stored format v1, pinned toolset |
| Engine | `rewind-engine` | Nothing beyond what `rewind-server` wires: the worker, scheduler, projector and retention loops | Engine ports, stored format v1, pinned toolset, namespaces |
| Runtime (V8) | `rewind-runtime` | Nothing | Engine ports, pinned toolset, sandbox lib, namespaces |
| Stores | `rewind-store-turso`, `rewind-store-postgres` | Nothing | Engine ports, stored format v1, access and identity, namespaces |
| Identity | `rewind-workos` | Measuring how WorkOS refuses an unknown GitHub account while its Waitlist is on: at the callback (`error=`), or when the code is exchanged. The code by email was measured on Production on 16 November 2026 ([08](08-decisions.md#taken-for-accounts-organizations-and-access-november-2026)) | Access and identity |
| Blobs | `rewind-blob` | Files and S3; the `payloads` table with size and preview; workers' local cache; cleanup | Engine ports |
| Tools | `rewind-codegen`, `rewind-mcp` | `rewind-codegen`: JSON Schema → TypeScript, prompt rendering, observed types. `rewind-mcp`: stdio and HTTP, idempotency key and trace context in `_meta`. Built-ins `http` and `webhook`; credentials | Engine ports, pinned toolset |
| Type-checking | `rewind-check` | tsgo; the checker's own rules; the sandbox lib | Engine ports, pinned toolset, sandbox lib |
| Observability | `rewind-otel` | Nothing beyond what `rewind-cli` wires: the subscriber, and the exporter given to the projector | Engine ports, stored format v1, HTTP API `/v1` |
| Server | `rewind-server`, `rewind-api` | Nothing | Engine ports, HTTP API `/v1`, access and identity, namespaces |
| CLI | `rewind-cli` | Client of the embedded or a remote server, through `rewind-api`; local commands: `check`, `test`, replay of an exported journal; exit codes | HTTP API `/v1`, stored format v1, sandbox lib, namespaces |
| TypeScript SDK | `sdk/typescript` | Client; host-tool worker; agent tools; adapters for the Anthropic and OpenAI tool formats | HTTP API `/v1`, pinned toolset, namespaces |
| Dashboard | `dashboard` | Nothing | HTTP API `/v1`, access and identity, namespaces |
| Evals | `evals` | Tasks; deterministic fake MCP servers; the loop generate → check → run → fix; reports per model | HTTP API `/v1`, pinned toolset, sandbox lib |
| Claude as a harness | `rewind-agent-tools`, `rewind-mcp-server`, `rewind-server`, the stores, `rewind-cli`, `dashboard` | Everything: designed on 19 November 2026 | Connections and MCP, HTTP API `/v1`, access and identity, namespaces |
| Operations | `Dockerfile`, `deploy/`, CI, `rewind-load` | Setting WorkOS's variables on Railway's api service, and deploying the dashboard with its `/auth` rewrite | Engine ports, HTTP API `/v1`, access and identity |

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
- **API authentication and tenancy** ([06](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)).
- **Accounts, organizations and access**: WorkOS for identity only, Rewind's permissions, roles and
  grants, sign-in on the dashboard's pages ([06](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)).
- **Namespaces**: one per end user of a customer, end users' tokens, quotas against abuse and
  their alerts ([06](06-storage-and-scale.md#namespaces)).
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
- **Workflows across runs**: named, typed workflows that scripts start and signal, the end of a
  run coming back as a signal, attached and detached runs, scopes
  ([01](01-product.md#workflows), [03](03-execution.md#workflows-across-runs)).
- **Workflow test suites**: end-to-end tests that give a run its world and check what leaves it,
  suites inherited along a workflow's versions, the gate on save, tests recorded from past runs
  ([05](05-testing-and-observability.md#testing-and-mocks)).
- **Claude as a harness**: Rewind's MCP server, its OAuth authorization server, and a person's
  connection opening the organizations they tick; the dashboard's Connect Claude and consent
  ([01](01-product.md#claude-as-the-harness), [06](06-storage-and-scale.md#connections),
  [07](07-code-architecture.md#mcp-and-oauth)).

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
- `rewind test` replays a recorded real run as a test. `rewind fork` continues a failed run
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
| Sign-in with a code by email, an organization, an invitation with a role, an API key; each endpoint refuses a caller without its permission | Passes against WorkOS Staging; not run without its credentials (`WORKOS_API_KEY`, `WORKOS_CLIENT_ID`) | `acceptance/tests/c8-accounts.spec.ts`, in Chromium against the dashboard `rewind server` serves with Staging's `WORKOS_*`: someone new signs in with a code, creates an organization in the onboarding, invites a second person as `reader` and creates a `harness` key; one request per permission of the catalog for the key and for the invitee, each refused (403, naming it) outside their role and answered within it; the invitee joins through the invitation's link and a code; removed by the owner, their next request is refused. Everything it created in WorkOS is removed |

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
