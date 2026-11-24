# 9. Plan for 0.1

Version 0.1 is the whole specification: every document in `archi/` describes 0.1. It is
built as one product: each part is built directly in its final shape, with no interim
version.

The contracts between the parts are frozen first. Then every workstream is built in
parallel against them. 0.1 is done when the [acceptance criteria](#01-is-done-when) pass.

## What the runtime spike proved

- **Concurrency**: `Promise.all` over 3 actions whose results arrive in reverse order gives
  the right result, and its replay is identical.
- **Determinism**: `Date`, `Math.random`, `crypto.randomUUID`, `performance.now` and
  `Intl` give identical results across runs with the same seed; a different seed gives
  different values.
- **Changed code detected**: a changed argument or an extra call fails replay at the
  exact diverging event.
- **Durable time**: after `await sleep("10d")` the code sees exactly 10 days later;
  `setTimeout` uses the same durable timers.
- **Signals**: buffered when early, cancel their timeout timer on arrival, time out to
  `undefined`.
- **Sandbox**: no `fetch`, `Deno`, `WeakRef` or `process`; only `rewind` and `rewind:actions` are
  importable, dynamic `import()` included.
- **Callsites** point to TypeScript lines.
- **Errors and cancellation**: `ActionError` (with `outcomeUnknown`) and `CancelledError`
  reach the code and can be caught.

## Current state of the code

Every part of 0.1 is built, on the run plane of the object-storage rewrite (designed with the
owner on 22 November 2026, [08](08-decisions.md#taken-for-the-object-storage-rewrite-november-2026)):
the runs live in partitions on SlateDB over object storage, each written by its one owner; Postgres
holds the control plane, Turso when embedded; every process is an HTTP edge and a node, and the
nodes talk over the node link. The schema's migration sets the run tables of the design before
it aside, and `rewind server migrate-runs` moves their runs into the partitions and the archive,
once.

On 23 November 2026 the release gate said **GO** on the run plane: 353 of 353 checks (the
criteria, thirteen areas on a deployed setup of two nodes on Postgres and MinIO and on a strict
one, the lifecycle, the sweeps and the load test), no unexpected 5xx, no leaked canary
([status](#acceptance-status-november-2026), [numbers](#load-tests)). Its first run on the run
plane was NO-GO, 186 of 352 checks; what it found is fixed and recorded in
[08](08-decisions.md#taken-while-bringing-the-release-gate-to-go-on-the-run-plane-november-2026):
partitions fenced at start by an open under way when they moved, a read left answering errors on
a fenced partition, the SDK streams of a reopened home partition and of a node that died, the
consumption of an erased namespace, starts racing on one idempotency key, a history read while
its attempt was sealed, and the checks that relied on the previous timing. `cargo fmt --check`,
`cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` with
Postgres, MinIO and a Collector in Docker pass, and so do the SDK's and the dashboard's checks.

The run plane was released the same day: `rewind server migrate-runs` moved production's runs
once, before its node started; Railway's node runs on AWS S3 in eu-west-3, the bucket
`rewind.codes-paris-prod-23112026-13b80` (`--bucket`, `--s3-region`, `--cluster-secret`), which it
reaches with the `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` of its variables; SDK 0.5.0 is on
npm, and Dax runs it, its worker on the call stream. Main's CI was then brought to green
([08](08-decisions.md#taken-while-bringing-mains-ci-to-green-after-the-release-november-2026)), and
the node redeployed on 24 November with a volume, so that a deploy stops the old node before it
starts the new one
([08](08-decisions.md#taken-while-redeploying-the-hosted-node-november-2026)).

What is left of 0.1:

- **The rename release** ([below](#the-rename-release-designed-24-november-2026)): every name of
  the code, the API, the SDK, the dashboard, the docs and the stored data moves to the words of
  this specification, with two changes of behavior, the statuses `running` and `waiting` and the
  workflow model.
- Older and still open: the evals with real models (they need API keys), the gVisor profile on a
  Linux host, the example `examples/testing` (it pins the published SDK), a connection made from
  claude.ai and Claude Code against a deployment, and criterion 8 of
  [0.1 is done when](#01-is-done-when) against WorkOS Staging on this build.

How it got here, each part built with its tests and reviewed, its choices in
[08](08-decisions.md):

- **14–15 November 2026**: accounts, organizations and access; namespaces; workflows across runs;
  the dashboard's redesign on Geist.
- **17 November**: workflow test suites.
- **19 November**: Claude as a harness: people's connections, the OAuth authorization server,
  `/mcp`.
- **20–22 November**: secrets and API actions, signed calls; the live dashboard.
- **22–23 November**: the run plane. Its contracts were frozen in code (WS0), then built in
  parallel: the partitions on SlateDB (WS1), the actors (WS2), the control plane on Postgres and
  Turso with the archive (WS3), the server's cutover with the node link, the call stream and
  `migrate-runs` (WS4), the clients (WS5: SDK 0.5.0 with the call stream, the dashboard on the
  change stream alone, the CLI's held waits), and their integration with the release gate (WS6).
  Released on 23 November.
- **21–24 November**: the vocabulary and the workflow model, decided with the owner, and the
  rename release designed.

The table describes the code in the words of this specification. The code still carries the
previous names (scripts, toolsets, tools, the journal, host tools, tasks…) until the rename
release lands, which renames each of them as its
[decisions](08-decisions.md#taken-for-the-vocabulary-november-2026) say.

| Crate | State |
| --- | --- |
| `rewind-core` | Pure domain, no serde: ids (`Seed` on 256 bits, `Epoch`, `PartitionId`, run ids of version 8 that carry their partition), RFC 8785 canonical JSON (golden vectors of the RFC), payloads, the four event families (`signal_awaited` and `invalid_code` included), `RunState`, history validation, `ReplayCursor`; the pinned action set (MCP servers and their transports, APIs; actions with route, description, input and output schemas, policy, `auth`) and its digest; call outcomes; cassettes derived from a history; observed result shapes; workflow versions, tests and suites. |
| `rewind-format` | Format v1: one event per row, run exports (the run's id, code and pinned action set, then its events), cassettes, pinned action sets (canonical, hashing to the action set's digest), workflow versions, tests, and the sixteen partition records (`rewind_format::partition`); values in canonical form. Golden fixtures pin every variant's spelling; unknown versions, types and fields are refused. |
| `rewind-engine` | The ports of 0.1, each with an in-memory adapter and a conformance suite: `CatalogStore`, `PartitionStore` and `PartitionLog`, `RunArchive`, `Cluster` and its membership, `NodeLink`, `BlobStore`, `AccessStore`, `SecretStore`, `IdentityProvider`, `CodeExecutor`, `ActionDispatcher`, `TypeChecker`, `SpanExporter`, `WebhookSender`, `Clock`, `Entropy`. The run plane's actors, state machines that take a message and return effects ([07](07-code-architecture.md#inside-a-node)): the owner of a partition (its runs in memory, the committer's group commit with the batch retried unit by unit on a conflict, the fan-out after durability, the timer wheel and its horizon, the outbox and its projections, the sealer, retention and erasure, recovery after a takeover), the tenant actor (admissions in order, quotas and their alerts, CPU, the slots of MCP servers and APIs, harness calls handed to SDK streams with their reconnect timeout), the executors (sticky instances in an LRU cache, local or on a node of the run's epoch), the call executor (policies, backoff, timeouts, parked calls, the secrets a run uses and signed calls), the node (rendezvous hashing of the partitions, the ring of session locks, joins, hand-overs and takeovers, subscriptions, SDK streams); `node::runtime`, which drives them with tokio; the edge's use cases through a `Gateway` (starts, forks, signals, cancels, calls, reads, searches across partitions, statistics from counters, transcripts, tests recorded from runs, workflow versions through their gate); the router; the driver, every node of a cluster stepped on one thread in virtual time, for the tests and the test runs; `migrate`, the runs of the set-aside tables moved once. Use-case and cluster tests run in the driver on a fake executor. |
| `rewind-runtime` | `V8Executor`: the checks at start (syntax, imported modules and names, default export), an isolate per instance on its own thread, started from the epoch's V8 snapshot, the epoch-1 glue, logical time, sfc32 and ChaCha20 keyed by the seed, `run.id` and `run.startedAt`, the web-standard globals of `deno_web`, how a run ends (forgotten `await`, unhandled rejection, `DeadlockError`), a CPU watchdog on the isolate thread's CPU clock, which also reports each call's CPU time, and a heap limit; args validated against the pinned schema inside the op; `signalUrl()`; `fetch` with the `http` action, in the format of `rewind-mcp`'s; `rewind:actions` generated from routes, MCP content with `text()` and `json()`; a test's TypeScript handlers as an `ActionDispatcher` (`TsHandlers`); a budget of array buffers (resizable ones included), built-ins measured against the heap before V8 runs them to their end, and the stack limit pinned before each call (the `memory` module, the only `unsafe` code); the memory each instance holds, from V8's heap statistics, for the sticky cache's bound; the JavaScript of `deno_web` and `deno_webidl` vendored and given to the snapshot build from memory, so the binary runs on a machine without the cargo registry ([08](08-decisions.md#taken-while-building-operations-november-2026)). |
| `rewind-store-slatedb` | `PartitionStore` on SlateDB 0.18 over object storage (S3 or any S3-compatible service, local files embedded, memory in tests): one database per partition at `partitions/{id}/`, the partition records under keys of their own (a read is one point get or one range scan), a batch committed as one transaction written as one write batch and durable when the commit returns, reads of what is durable only, the compactor in the writer's process, one block cache per process, SlateDB's timers listed and lengthened. An open claims the writer and compactor epochs after the ones it read, so an open under way when the partition moved away fails instead of fencing the new owner. Passes the conformance suite on memory, files and MinIO; `examples/bench.rs` measures batches and cold reads ([08](08-decisions.md#taken-while-building-the-partitions-on-slatedb-november-2026)). |
| `rewind-store-turso` | The embedded control plane on Turso Database (`turso` 0.8.3), `.rewind/control.db`: `CatalogStore` (code, action sets, workflows and their versions, namespaces and their partitions, tests, tenants), `Cluster` with its one member in the process and the partition map in the database, `AccessStore` (keys, roles, grants, end users' tokens and token keys, connections and their OAuth clients and codes), `SecretStore` (named secrets and signing keys sealed under a ring of master keys); every write's change sent once committed through a broadcast in the process; `TursoLegacy`, the set-aside run tables `migrate-runs` reads and drops. Schema versioned with its migrations embedded, a newer schema refused. Passes the conformance suites, each check on a new database file. |
| `rewind-store-postgres` | The control plane on Postgres with sqlx 0.8: `CatalogStore`, `Cluster` (the nodes and the ring of session advisory locks they watch, in a database of their own, the partition map, takeovers in one transaction), `AccessStore` and `SecretStore`, as the embedded store's, with advisory locks where writers compete; every write's change sent with `NOTIFY` in its transaction (`control_plane`, `partition_map`, framed above 7,900 bytes) and listened to by every process; `PostgresLegacy`, the run tables migration 15 set aside in the schema `legacy`, which `migrate-runs` reads and drops. Migrations embedded and run under a lock. Passes the conformance suites against Postgres 17 in Docker ([08](08-decisions.md#taken-while-building-the-control-plane-of-the-run-plane-november-2026)). |
| `rewind-blob` | The archive and the blobs: `FileArchive` and `S3Archive` keep the sealed attempts of ended runs at `runs/{tenant}/{run}/{attempt}.json`, run exports written once (`If-None-Match: *`), `CachedArchive` in front; `FileBlobs` (atomic writes) and `S3Blobs` (`object_store`, conditional puts) keep a run's payload values above 64 KiB under the run and a tenant's tests, `CachedBlobs` in front, bounded in bytes; erasing a run deletes its prefix; `LegacyFiles` and `LegacyS3` read the tenant-keyed blobs `migrate-runs` moves. Digests checked on every put and get. Passes the conformance suites of `BlobStore` and `RunArchive` on files and on MinIO in Docker. |
| `rewind-workos` | `WorkOs`, the `IdentityProvider` port on WorkOS through its SDK `workos` 3.7: GitHub's authorization URL and code, Magic Auth, email verification, the choice of an organization, refreshes onto an organization, each with the browser's IP address and user agent and the invitation's token; WorkOS's steps and a closed sign-up as answers; access tokens checked locally against the JWKS (RS256, issuer, `client_id`, expiry with 30 s of leeway); organizations, people, active memberships and invitations without roles, the waitlist; WorkOS's messages passed on without the secrets sent. Tested against a fake WorkOS (wiremock, test RSA keys, a rotation of keys) and against WorkOS Staging when its variables are set ([08](08-decisions.md#taken-for-accounts-organizations-and-access-november-2026)). |
| `rewind-architecture-tests` | The dependency rules, telemetry and `anyhow` included, each with a test proving it catches its violation; `rewind-agent-tools` and `rewind-mcp-server` classified as adapter support, on `rewind-api` and never the engine; the guard against timers (`tests/timers.rs`): no `interval` and no `sleep` outside the timer wheel, the backoffs after an error and the clocks, each exception keyed by file and function with its reason; the only `unsafe` code in `rewind-runtime`'s `memory`. |
| `rewind-e2e` | The epoch-1 corpus: 38 blue and 43 red cases, forks, action policies, MCP content, the `http` action's format, workflows and secrets included, run live on one node of the run plane in the engine's driver, recorded and replayed from their run exports; tests proving the harness cannot pass silently; the contract test of codegen, the checker and the runtime (`tests/declarations.rs`); `cargo run -p rewind-e2e -- replay <case> [--code file.ts]`. |
| `rewind-api` | The HTTP API `/v1` as a contract: the DTOs of every endpoint, problem+json errors, the query language of the run lists (parsed with `winnow`, errors with the span of the term at fault), `Idempotency-Key`, held waits, the event stream of a run, the change stream `GET /v1/changes`, the call stream `GET /v1/calls/stream`, webhook payloads signed following Standard Webhooks and their verification, the connections and the consent, the OAuth authorization server's wire formats (`oauth`), `Rewind-Organization`, the OpenAPI 3.1 document committed at `api/openapi.json` (a test fails when it is stale), and an async client over HTTP(S) and over the embedded server's unix socket. |
| `rewind-check` | `TsgoChecker`, the `TypeChecker` port: tsgo 7.0.2 embedded as its npm tarball, pinned by its integrity and extracted to a cache directory at first use; checks in strict mode against the sandbox lib and `rewind-actions.d.ts`, reading nothing outside its directory; diagnostics with a location, a hint and a code frame; the rules `floating-promise`, `ignored-async-callback` and `absent-global` on the swc syntax tree. About 50 ms per check. |
| `rewind-codegen` | Our JSON Schema → TypeScript converter (objects, `required`, enums, `anyOf`/`oneOf`/`allOf`, `$ref`/`$defs`, arrays, `additionalProperties`); `rewind-actions.d.ts` (one namespace per group: an MCP server, an API or a set of harness actions; `ActionContent` for MCP content, observed types marked `@observed`, the `fetch` reference with the `http` action, `rewind:workflows`); the compact prompt rendering and each action's own declaration (for `search_actions`). Golden tests on GitHub-like, FastMCP-like and mixed action sets. |
| `rewind-mcp` | `McpDispatcher`: an MCP client (`rmcp` 3.9) over streamable HTTP and stdio, discovery as an action set pins it (a listing with the organization's secret or signed with the reduced claims), connections pooled by tenant, server and identity (an HMAC under a key of the pool: the secret, or the signing key and the end user), the idempotency key and the trace context in `_meta`, the secret the run uses in its header or variable, every HTTP request of a signed session signed (the call's `tools/call` with its claims), the schema check at dispatch (a changed tool of the server parks the call), the execution timeout, errors mapped to transient failures, failures and parked calls; on a deployed server, MCP servers reached only where `rewind-outbound`'s `ActionEgress` lets them, whose internal answers are never quoted. `HttpAction`: the `http` built-in and its egress rules (allow-list, then `rewind-outbound`'s checks: one resolution, special-purpose addresses refused, the checked address connected; redirects checked again, 5 MiB and 30 s). Tested against fixture servers over HTTP (one checking the JWT against a JWKS, one a secret header) and stdio (a secret in a variable). |
| `rewind-outbound` | What leaves Rewind for a tenant's servers: `Egress` (one resolution, private, loopback, link-local and metadata addresses refused, the connection to the checked address), `InternalAllowList` (`--allow-internal-tools`), `ActionEgress` and its clients (redirects within the origin with 307 or 308, refused for a signed session, followed by the API dispatcher, checked for documents; whether an internal address was reached), `place` (a secret in its header with its format and encoding, or a variable), `Signer` (an EdDSA JWT per request from the tenant's Ed25519 seed through `aws-lc-rs` and `jsonwebtoken`), `jwk` and `verify` (a receiving server's checks). Used by `rewind-mcp`, `rewind-openapi`, the server's webhooks and OAuth documents. |
| `rewind-openapi` | API actions: `read`, an OpenAPI 3.0 or 3.1 document (as JSON values) into an `Api` and an action per operation (names, parameters, the JSON body, the first `2xx` JSON schema, `$ref` within the document as `$defs`, 3.0's `nullable` and bounds converted, the security requirement as `auth`, `skipped` operations, the refusals of 04); `fetch`, a document from its URL through the egress checks (2 MiB); `ApiDispatcher`, the `api` route (the request in OpenAPI's default styles, `traceparent`, `Idempotency-Key`, the secret or a JWT per request, redirects within the origin signed again, the answer table of 04 with `Retry-After`). Tested on 3.0 and 3.1 fixtures and against a fake API checking the JWT against a JWKS. |
| `rewind-agent-tools` | The agent tools in Rust: the SDK's set, its definitions word for word, and the MCP set (no `update_workflow`; `list_organizations`, `list_namespaces`, `list_action_sets`; `organization`, `namespace` and `action_set` as arguments; each tool's title, annotations and permission); the handlers over the ports `Api` and `McpApi`, one method per operation of `/v1`, as the SDK's `AgentTools` (keys of runs and signals, the allowed labels, `update_workflow`'s derived action set, refusals as results and faults as errors); the bounded texts of the SDK's `render.ts` and `bound.ts`, and the MCP bounds (80 000 bytes, `wait` 30 s and 200 s at most); `rewind_api::Client` as an `McpApi`. Tested on its own fake API, and through the evals. The golden fixtures shared with the SDK's tests are not written yet. |
| `rewind-mcp-server` | Rewind's MCP server: an `rmcp` `ServerHandler` (`initialize` with the instructions, `tools/list` filtered by the caller's organizations, `tools/call` resolving `organization`, checking the permission, then running the agent tool with an API bound to the organization and the namespace), for the callers the server's `Connector` gives; served over Streamable HTTP without session (`http_service`, JSON answers, `GET` and `DELETE` 405) and over stdio. Tested with a fake connector and API: the tools per permission, `organization` resolution, titles and annotations, the bound, the namespace of checks, tests and runs, and whole exchanges through rmcp's client over a stream and over HTTP. |
| `rewind-otel` | OpenTelemetry 0.33, OTLP over gRPC or HTTP with protobuf, both with TLS, configured or from the `OTEL_*` variables. `OtelSpanExporter`, the `SpanExporter` port: run telemetry exported with the projector's ids, under the scope `rewind.run`. `install`: the process's `tracing` subscriber, logs on stderr, and `ServiceTelemetry`, its own layer that turns `tracing` spans into OpenTelemetry spans joined to a run's trace by their fields, events into log records, and measurements into metrics exported periodically; sampled per run like run telemetry. Passes the conformance suite of `SpanExporter` on the SDK's in-memory exporters and on an OpenTelemetry Collector in Docker over gRPC and HTTP; the engine is driven under the subscriber to test its spans and metrics ([08](08-decisions.md#taken-while-building-opentelemetry-november-2026), [08](08-decisions.md#taken-while-finishing-service-telemetry-november-2026)). |
| `rewind-cli` | The client commands of the `rewind` binary over `rewind-api`: `run`, `status`, `signal`, `cancel`, `fork`, `transcript`, `runs`, `release`, `export`, `action-sets`, `workflows`, `secrets`, `namespaces`, with `--json` and the exit codes of 07, a wait being one request held until the run ends or is quarantined; `rewind mcp`, the agent tools of the MCP set over stdio against the embedded server, as its organization `local`, for debugging; the remote server of `REWIND_URL`, or the embedded server of `.rewind/`, started on demand (lock check, detached start, its readiness on a pipe, version handshake, replacement of another version). In the process: `check`, `test` (a workflow's tests file as test runs, with cassettes, TypeScript handlers and `rewind.actions.json`, in virtual time; on the server for a workflow's suite), `workflows record`, `replay` of a run or of a run export file, with the compatibility report of corrected code. Tested against a stub embedded server, a stand-in API and the e2e corpus, and end to end through the `rewind` binary (`crates/rewind/tests/cli.rs`): a run that sleeps, is interrupted and resumes in another process, a signal from another process, `kill -9` of the server mid-activation, the corpus live on the embedded server, `rewind test` on a recorded cassette and `rewind fork` of a failed run. |
| Sandbox lib | `crates/rewind-runtime/lib/`: `rewind.d.ts`, `globals.d.ts`, `fetch.d.ts`, tsgo's `tsconfig.json` and `globals.json`, checked with TypeScript 7.0.2. A test checks that the isolate's globals are exactly the declared ones, with and without the `http` action. |
| `sdk/typescript` | SDK 0.5.0: the client of `/v1`, its waits held by the server and sent again with the time left; the worker of harness actions on a call stream (`GET /v1/calls/stream`: slots, a worker id kept across reconnections, `gone`), whose open stream is the liveness of its calls; the agent tools and their adapters for the Anthropic and OpenAI tool formats; secrets, signing keys and `verifyRewindCall`. Tested with vitest against a mocked API, and live against a server (`test/live.test.ts`). |
| `dashboard` | Overview (the home page), Runs (every run, the running, the waiting and the failures), Run, Workflows, Actions, Namespaces and Secrets views over the API `/v1`, with types generated from `api/openapi.json`, on Geist's tokens and Base UI's menus and dialogs: one sidebar that turns into the page's own (Runs' views and filters; a run's sections and details; the organization's parts) and holds no control, the organization picker and Find at its top, the account at its bottom; workflows named by their names and runs by their handles; one query bar with suggestions, its filters terms of the query, kept in the URL; a table of runs ordered and filtered from its headers; statistics of a namespace or the organization over whole hours, from the counters; runs by outcome over time, the runs that wait and what for, failures by fingerprint, as SVG charts; the code with each call, timer, signal wait and log next to its line, and a time cursor that steps through the run, with hovers and watches of the values that crossed the history; a timeline that compresses idle gaps, with a playhead; payloads on demand, through their run; light and dark themes. Its state in TanStack DB collections behind `state/`, its sources in `sources/`; live through the change stream alone, with no view reading on a timer (a Biome rule refuses `refetchInterval` and `setInterval`), and nothing on screen that moves on its own; optimistic writes; signals and cancellations from a run's page. With WorkOS, the sign-in on its own pages, the onboarding, the organization picker and the Organization views; every view following the permissions `whoami` answers; without WorkOS, an API key. Tested with vitest against a mocked API; built to `dashboard/dist/`, which `rewind-server` embeds ([07](07-code-architecture.md#dashboard)). |
| `rewind-server` | `rewind server`, one process that is an HTTP edge and a node of the run plane: the node's actors on `rewind-engine`'s runtime, its partitions on SlateDB, its membership of the cluster on the control plane (Postgres, or Turso for one node), `--executors-only`, `--pool` and `--organization-pool`; the node link over HTTP/2 (`/link/call`, `/link/activate`, `/link/subscribe`), authenticated by the cluster's secret, its DTOs JSON with the partition records' spelling. The edge: every endpoint of `rewind-api` with axum on the engine's `Edge`, API keys and members' access tokens, grants, roles and allowed labels, organizations, members and invitations, organizations' ceilings (429), `Idempotency-Key`, problem+json; searches merged across partitions, statistics from their counters; held waits; runs found by their handles at the owner of their namespace's partition, workflows' names from the catalog; the event stream of a run, the change stream and the call stream, pushed by the owners; signal URLs, whose posts become signals; webhooks, signed following Standard Webhooks, sent once per try from the owners' outboxes; `/auth` through WorkOS; people's connections, the OAuth authorization server and `POST /mcp`; the control plane's daily jobs on every node. The embedded server of a `.rewind/` directory: one node, one partition on local files, Turso, its lock, its socket, its readiness pipe, its idle stop, virtual time. `rewind server migrate-runs`. The dashboard's build embedded and served. Tested over HTTP on Turso and on Postgres with MinIO, with several nodes in one process over real HTTP/2 (`tests/cluster`), and through the `rewind` binary. |
| `rewind` | The `rewind` binary: it restarts itself with `TZ=UTC`, then hands `rewind server …` to `rewind-server` and everything else to `rewind-cli`. |
| `evals` | 22 tasks from easy to hard (fan-out, pagination, `ActionError`, signals with and without timeout, sleeping and polling, signal URLs, redeliveries under a rate limit, outputs above 256 KB, `retry_run`, `cancel_run`) over six deterministic fake MCP servers built with `rmcp`; the run plane in the engine's driver, in virtual time, behind `rewind-agent-tools`' port `Api`, which the world implements as the server answers an API key limited to the episode's labels ([08](08-decisions.md#taken-while-building-the-agent-tools-and-the-mcp-server-november-2026)); Claude (Messages API) and OpenAI (Responses API) clients and a scripted model; reports per model and prompt rendering (`compact`, `dts`, `search`) in JSON and Markdown. Every task's reference solution passes through the scripted model in `cargo test` ([08](08-decisions.md#taken-while-building-the-evals-november-2026)). |
| `acceptance` | The acceptance suite of 0.1, one command (`acceptance/run.sh`): Playwright specs against the real `rewind` binary, two nodes on Postgres and MinIO with a Collector in Docker, `@modelcontextprotocol/server-everything` through `npx`, the embedded dashboard in Chromium, and WorkOS Staging for the sign-in; it also runs `crates/rewind/tests/cli.rs` and the operations' load and limit tests, and writes the status of each criterion to `acceptance/results/summary.md` ([its README](../acceptance/README.md)). |
| Release gate | `acceptance/release.sh`, run before any release ([its README](../acceptance/release/README.md)): the tree built in release mode; criteria 1, 2, 3, 5 and 7; thirteen areas on a deployed setup behind an edge (Postgres, MinIO for the partitions, the archive and the blobs, a Collector, two nodes on the node link) and a strict one with a deployed server's defaults, 353 checks, each a success, a failure or an attack that must be refused with no effect; the lifecycle (the owner of a partition killed during an activation and while a harness action holds its call, a node stopping and joining, a full restart, Postgres paused, the archive, retention, the erasure of a run and of a namespace); sweeps of every recorded exchange and log (no unexpected 5xx, no panic, no leaked canary secret, every problem type provoked); the load test. A report says GO, NO-GO or PARTIAL (`acceptance/results/release/report.html`). |
| `rewind-load` | The load generator: drives a deployment's API with many concurrent runs that call its fake MCP server and its harness action on a call stream, sleep and read signals; reports throughput, latencies and errors, and checks that every run ends as expected and no call is delivered beyond the failures it met ([numbers](#load-tests)). |
| Operations | The image (`Dockerfile`: distroless, non-root, one image for every node); the reference deployment (`deploy/`: Compose with Postgres, MinIO, a Collector, an edge, two nodes and a node of the previous epoch whose only way out is an egress gateway, gVisor as a profile, `check-walls.sh`, `migrate-runs` before the nodes start); Railway runs one node; CI (`.github/workflows/ci.yml`: Rust, SDK, dashboard, image, the deployment under load with runc and gVisor); the limits tested through the API (`rewind-server/tests/limits.rs`, the SDK's `test/live.test.ts`). |

## Contracts frozen first

| Contract | Specified in | Implemented by | Consumed by |
| --- | --- | --- | --- |
| Engine ports (traits and conformance suites) | [07](07-code-architecture.md#ports) | The adapters: runtime, stores, blobs, MCP, checker, OpenTelemetry, secrets, webhooks | `rewind-engine` |
| Stored format v1 (events, run exports, cassettes, pinned action sets, workflow versions, tests) | [02](02-history.md#stored-format-v1) | `rewind-format` | The stores, `rewind test`, the e2e corpus, run export and archive, suites |
| Pinned action set (schemas, policies, declarations) | [04](04-tools.md#action-sets-and-code-generation) | `rewind-codegen` | The runtime (`rewind:actions`), the checker, the dispatcher, the SDK, the model's prompt |
| HTTP API `/v1` (OpenAPI) | [07](07-code-architecture.md#http-api-v1) | `rewind-api` (DTOs, OpenAPI document, Rust client), `rewind-server` | The CLI, the SDK and its agent tools, the dashboard, the evals |
| Sandbox lib (`rewind.d.ts` and the checker's `lib`) | [04](04-tools.md#type-checking) | `rewind-runtime` (the globals that exist) | `rewind-check`, the prompt |
| Access and identity: the catalog of permissions and the built-in roles, `AccessStore`, `IdentityProvider`, `/auth` | [06](06-storage-and-scale.md#tenancy-namespaces-identity-and-access), [07](07-code-architecture.md#sign-in-auth) | `rewind-api`, `rewind-engine` (ports, in-memory adapters, the conformance of `AccessStore`) | The stores, `rewind-workos`, `rewind-server`, the dashboard |
| Namespaces: their quotas, usage and alerts, end users' tokens and token keys, secrets per namespace, `Rewind-Namespace` | [06](06-storage-and-scale.md#namespaces), [07](07-code-architecture.md#ports) | `rewind-api`, `rewind-engine` (ports, in-memory adapters, conformance) | The stores, `rewind-runtime`, `rewind-server`, the CLI, the SDK, the dashboard |
| Connections and MCP: the connection's tables, grants and tokens, `Rewind-Organization`, `/mcp`, `/oauth`, the agent tools' definitions | [06](06-storage-and-scale.md#connections), [07](07-code-architecture.md#mcp-and-oauth) | `rewind-api`, `rewind-engine` (`AccessStore`), `rewind-agent-tools` | The stores, `rewind-server`, `rewind-mcp-server`, the CLI, the evals, the dashboard |
| The run plane: `CatalogStore`, `PartitionStore`, `RunArchive`, `Cluster`, `NodeLink`, the partition records, the actors' messages, the call stream, statistics from counters | [06](06-storage-and-scale.md#partitions-owners-and-nodes), [07](07-code-architecture.md#ports), [07](07-code-architecture.md#inside-a-node), [02](02-history.md#stored-format-v1) | `rewind-core` (`PartitionId`, run ids), `rewind-engine` (ports, actors, in-memory adapters, conformance), `rewind-format` | `rewind-store-slatedb`, the stores, `rewind-blob`, `rewind-server`, the CLI, the SDK, the dashboard, the e2e corpus |

Each contract is reviewed by the owner, with diagrams, before the code that depends on it.
The seven were approved in November 2026, access and identity and namespaces on the 14th,
connections and MCP on the 19th, the run plane on the 22nd. The vocabulary every contract is
written in, and the workflow model, were approved on 24 November
([08](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)).

## Workstreams

The rename release ([below](#the-rename-release-designed-24-november-2026)) touches every
workstream. Beyond it, what remains of each is:

| Workstream | Crates | What remains | Depends on (contracts) |
| --- | --- | --- | --- |
| Domain and format | `rewind-core`, `rewind-format` | Nothing | Stored format v1, pinned action set, the partition records |
| Engine | `rewind-engine` | Nothing | Engine ports, stored format v1, pinned action set, namespaces, the run plane |
| Runtime (V8) | `rewind-runtime` | Nothing | Engine ports, pinned action set, sandbox lib, namespaces |
| Stores | `rewind-store-turso`, `rewind-store-postgres`, `rewind-store-slatedb` | Nothing | Engine ports, stored format v1, access and identity, namespaces, the run plane |
| Identity | `rewind-workos` | Measuring how WorkOS refuses an unknown GitHub account while its Waitlist is on: at the callback (`error=`), or when the code is exchanged. The code by email was measured on Production on 16 November 2026 ([08](08-decisions.md#taken-for-accounts-organizations-and-access-november-2026)) | Access and identity |
| Blobs | `rewind-blob` | Nothing | Engine ports, the run plane |
| Actions | `rewind-codegen`, `rewind-mcp`, `rewind-outbound`, `rewind-openapi` | Nothing | Engine ports, pinned action set, secrets |
| Type-checking | `rewind-check` | Nothing | Engine ports, pinned action set, sandbox lib |
| Observability | `rewind-otel` | Nothing | Engine ports, stored format v1, HTTP API `/v1` |
| Server | `rewind-server`, `rewind-api` | Nothing | Engine ports, HTTP API `/v1`, access and identity, namespaces, the run plane |
| CLI | `rewind-cli` | Nothing | HTTP API `/v1`, stored format v1, sandbox lib, namespaces |
| TypeScript SDK | `sdk/typescript` | Nothing | HTTP API `/v1`, pinned action set, namespaces, secrets, the call stream |
| Dashboard | `dashboard` | Nothing | HTTP API `/v1`, access and identity, namespaces, connections and MCP, secrets, the change stream |
| Evals | `evals` | Their run with Claude and OpenAI models, which needs API keys | HTTP API `/v1`, pinned action set, sandbox lib |
| Claude as a harness | `rewind-agent-tools`, `rewind-mcp-server`, `rewind-server`, the stores, `rewind-cli`, `dashboard` | A connection made from claude.ai and Claude Code against a deployment | Connections and MCP, HTTP API `/v1`, access and identity, namespaces |
| Operations | `Dockerfile`, `deploy/`, CI, `rewind-load` | The dashboard's deployment with its `/auth` rewrite; the gVisor profile on a Linux host | Engine ports, HTTP API `/v1`, access and identity, the run plane |

### The run plane on object storage (designed 22 November 2026)

These workstreams built the run plane in parallel, each ending with its suites green; WS4 cut the
server over and deleted the previous design, and WS6 brought the release gate to GO on 23 November
2026. The run plane was released the same day, which ended them.

| Workstream | Crates | What it builds | Depends on |
| --- | --- | --- | --- |
| WS0 contracts | `rewind-core`, `rewind-engine` (ports), `rewind-format` | `PartitionId` and run ids; the ports `CatalogStore`, `PartitionStore`, `RunArchive`, `Cluster`, `NodeLink` with their in-memory adapters and conformance suites; the partition records | The run plane contract |
| WS1 partitions | `rewind-store-slatedb` | `PartitionStore` on SlateDB: the key layout, the conformance suite on memory, files and MinIO; `examples/bench.rs` | WS0 |
| WS2 actors | `rewind-engine` | The owner, committer, fan-out, timer wheel, registry, outbox, sealer; executors, local and remote; the call executor; the tenant actor; the node; the router; the driver; the use cases moved onto them | WS0 |
| WS3 control plane | `rewind-store-postgres`, `rewind-store-turso`, `rewind-blob` | `CatalogStore` and `Cluster` on both stores, the run tables moved to `legacy`, `NOTIFY` on every write; `RunArchive` and blobs under runs | WS0 |
| WS4 server | `rewind-server`, `rewind-api` | One process: edge and node; the node link; the call stream; statistics from counters; the change stream from owners and `LISTEN`; `--executors-only`, `--pool`; the embedded node; `migrate-runs`; the previous design's background loops deleted | WS2, WS3 |
| WS5 clients | `sdk/typescript`, `dashboard`, `rewind-cli`, `rewind-architecture-tests` | SDK 0.5.0 with the call stream; the dashboard without timers; the CLI's waits and readiness pipe; the guard against timers | WS0, then WS4's routes |
| WS6 integration | `rewind-e2e`, `rewind-load`, `deploy/` | The corpus, the cluster tests, the release gate, the load test, the migration of production's runs, the deploy on Railway with S3 | Everything |

### The rename release (designed 24 November 2026)

Every name of Rewind moves to the vocabulary the owner decided on 21, 22 and 24 November 2026
([08](08-decisions.md#taken-for-the-vocabulary-november-2026),
[08](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)), in which
this specification is written: a script becomes a workflow and its text its code, a toolset an
action set, a tool an action, the journal the history, an entry an event, a host tool a harness
action, a task a call wherever a customer sees it, a call's attempt a delivery, mock mode a test
run with handlers, the LLM trace a transcript, a failure group a fingerprint, and the rest of
the map, in the code, the API, the SDK, the agent tools, the CLI, the dashboard, the docs, the
stored data and the telemetry. The release renames and changes no behavior, with two exceptions
the owner chose:

- **The statuses**: `queued`, `running`, `waiting`, then `completed`, `failed`, `cancelled` or
  `aborted`. The partition owner derives running or waiting at each batch from the waits it
  already stores; the API gives what a run waits for in `waitingFor`, the queries take
  `status:waiting` and `waiting:<kind>`, and `run.waiting` is sent at the moments
  `run.suspended` was ([02](02-history.md#run-state)). When and how a run executes does not
  change.
- **The workflow model**: every code the API receives is a workflow, with an id the API assigns;
  a check or a test creates nothing; new code is a workflow's next version only when the call
  names that workflow; a name is optional and unique in its scope; a workflow belongs to the
  namespace whose run created it, or to the organization once a member promotes it; an
  organization's workflow is generic, executed as it is by any namespace's runs or forked into a
  namespace, and a fork pulls its source's new versions through its own suite
  ([01](01-product.md#workflows)). Workflow forking ships in this release. One Workflows view
  replaces Scripts and Testing; titles go, and those a person gave become names where they are
  valid and free.

What it keeps working:

- **Code already written** (R1): the sandbox still resolves `rewind:tools`, `webhook()`,
  `ToolError` and `WorkflowError` for code that uses them, without declaring or documenting them,
  so that Dax's saved workflows and their active runs, which wait on signals for up to a week,
  replay. The [determinism epoch](03-execution.md#determinism-epoch) goes up by one, from 1 to 2:
  an error caught as `ToolError` is now an `ActionError`.
- **No new version of the format or the API**: the stored format stays `"v": 1` and the API
  `/v1`. SDK 0.6.0 breaks every renamed method, type and path, and the API answers no old path.
- **Values in others' hands**: the prefixes `rewind_wh_` (signal URLs) and `rewind_tk_` (call
  tokens) and the handles already stored (their origin `script` included) are frozen. A signed
  call's JWT carries `action` beside `tool`, and verifiers read `action`.

The data is rewritten once, offline (R2, R6): the node stops, `rewind server rename-data`
rewrites the partitions and the archive on S3, the catalog, the access store (`user:` principals
become `member:`) and the subscriptions, and the new build starts. It recomputes the digest of
every content-addressed document (pinned action sets, workflow versions) and rewrites every
reference to it: names and version numbers keep working, an old digest stops resolving. It gives
each distinct code that ran a workflow in the namespace of its first run, moves each saved
workflow by its use (one whose runs all come from one namespace becomes that namespace's, one
that several namespaces run becomes the organization's), and turns the titles people gave into
names. A few minutes without the API; it is rehearsed first on a copy of production's data.

Rewind and Dax move the same day (R3): Dax's move to SDK 0.6.0 is prepared on a branch of Dax and
deployed right after the node. The release is built on the branch
`feature/planchon/glossaire-rewind`, reviewed by the owner, then fast-forwarded onto main.

| Step | What it builds | Depends on |
| --- | --- | --- |
| Specification | `archi/` 01 to 09 in the new words, so every later step reads one reference | The decisions |
| Domain and format | `rewind-core` and `rewind-format`: the records, the events, the names; the golden fixtures written again | The specification |
| Engine and stores | `rewind-engine`, `rewind-runtime`, `rewind-codegen`, `rewind-check`, the stores and `rewind-blob`: the statuses, the workflow model in the catalog, the sandbox's old names and epoch 2, `rename-data` | Domain and format |
| API, server, CLI and tools | `rewind-api` with `api/openapi.json` generated again, `rewind-server`, `rewind-cli`, `rewind-mcp`, `rewind-openapi`, `rewind-outbound`, `rewind-otel`, `rewind-agent-tools`, `rewind-mcp-server`: paths, types, permissions, webhooks, problems, flags, spans and metrics, the agent tools | Engine and stores |
| TypeScript | SDK 0.6.0; the dashboard's Runs, Workflows and Actions, with before and after screenshots as its rules ask; the examples | `api/openapi.json` |
| Docs | Every page in the new words, the renamed pages with redirects from the old ones, the glossary in two levels (`docs/glossary.mdx`) | The specification |
| Checks | Every test suite; the two exceptions, each with its own tests; the release gate on the run plane; `rename-data` rehearsed on a copy of production's data | Everything |

Then the owner's review, main, and the release: the node with `rename-data`, the dashboard, SDK
0.6.0 on npm, the docs; Dax right after.

## Designs settled (November 2026)

Each design below was reviewed by the owner, with diagrams. The list points to where it is
specified.

- **Embedded mode**: the CLI is an API client of `rewind server --embedded`
  ([07](07-code-architecture.md#embedded-server-and-cli)).
- **Sticky execution and a thread per instance** (was question 2)
  ([03](03-execution.md#sticky-execution)).
- **Payloads in three tiers** ([06](06-storage-and-scale.md#payloads)).
- **Fork semantics** (was question 9) ([03](03-execution.md#forks)).
- **Call lifecycle**: harness actions that last days, the `approval` policy, the concurrency of
  actions, an action set's change during a run ([04](04-tools.md#call-lifecycle)).
- **Secrets and API actions**: actions from OpenAPI documents, calls signed with a key per organization,
  named secrets that a run names at start, uses and reads alone, the dashboard's Secrets
  ([04](04-tools.md#api-actions), [04](04-tools.md#authenticating-calls),
  [08](08-decisions.md#taken-for-secrets-and-api-toolsets-november-2026)).
- **API authentication and tenancy** ([06](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)).
- **Accounts, organizations and access**: WorkOS for identity only, Rewind's permissions, roles and
  grants, sign-in on the dashboard's pages ([06](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)).
- **Namespaces**: one per end user of a customer, end users' tokens, quotas against abuse and
  their alerts ([06](06-storage-and-scale.md#namespaces)).
- **Egress of the `http` action** ([04](04-tools.md#the-http-action-and-egress)), and of MCP
  servers and APIs ([04](04-tools.md#where-mcp-servers-and-apis-may-be)).
- **Retention and deletion** (was question 11)
  ([06](06-storage-and-scale.md#retention-and-deletion)).
- **Sandboxed nodes** ([06](06-storage-and-scale.md#sandboxed-nodes)).
- **tsgo in `rewind-check`** and the checker's own rules ([04](04-tools.md#type-checking)).
- **The JSON Schema → TypeScript converter**: ours, in Rust
  ([04](04-tools.md#action-sets-and-code-generation)).
- **The dashboard**: a React SPA served by `rewind-server`
  ([05](05-testing-and-observability.md#dashboard)).
- **The dashboard's debugger and search**: the time cursor, the waiting runs, fingerprints,
  the query language ([05](05-testing-and-observability.md#query-language)).
- **Evals**: the models, run on demand ([05](05-testing-and-observability.md#evals)).
- **Workflows across runs**: typed workflows that code starts and signals, the end of a run
  coming back as a signal, attached and detached child runs, allowed labels
  ([01](01-product.md#workflows), [03](03-execution.md#workflows-across-runs)).
- **Workflow test suites**: end-to-end tests that give a run its world and check what leaves it,
  a workflow's suite carried along its versions, the gate on each new version, tests recorded
  from past runs ([05](05-testing-and-observability.md#testing)).
- **Claude as a harness**: Rewind's MCP server, its OAuth authorization server, and a person's
  connection opening the organizations they tick; the dashboard's Connect Claude and consent
  ([01](01-product.md#claude-as-the-harness), [06](06-storage-and-scale.md#connections),
  [07](07-code-architecture.md#mcp-and-oauth)).
- **The live dashboard**: the change stream `GET /v1/changes`, the dashboard's state layer
  (TanStack DB behind `state/`), a screen that never moves on its own, and signals, cancellations
  and retries from a run's page ([05](05-testing-and-observability.md#dashboard),
  [07](07-code-architecture.md#dashboard),
  [08](08-decisions.md#taken-for-the-live-dashboard-november-2026)). The owners of the partitions
  push the changes of their runs, and the control plane notifies its own
  ([08](08-decisions.md#taken-for-the-object-storage-rewrite-november-2026)).
- **The vocabulary and the workflow model** (24 November 2026): one word for each thing, a
  glossary in two levels, the statuses `running` and `waiting`, and the workflow as the template
  of its runs, scoped to a namespace or the organization, forkable when generic
  ([01](01-product.md#workflows), [above](#the-rename-release-designed-24-november-2026),
  [08](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)).

## Out of 0.1

- `rewind replay <run> --inspect`, which attaches the V8 inspector to step through a past run.
- `continueAsNew`. A run that reaches the history cap (50k events) is aborted with
  `limit_exceeded`.
- An analytics engine for statistics on any query, reading the archive. Statistics come from the
  partitions' hourly counters ([05](05-testing-and-observability.md#statistics)).
- Moving a namespace to another partition, and splitting a partition.

Decided, and left to releases after the rename release, because they change behavior
([08](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)). None of
them is built:

- **Retries as attempts** (V11): a retry fixes a failed run in place and adds an attempt of the
  same run, live from `at`, as a fork. The retries branch is rebuilt on the partitions first, and
  takes the new names when it merges. Until then, `retry_run` starts a fork
  ([03](03-execution.md#forks)).
- **The cassette's removal** (V4): recording a run writes an ordinary test, each call a rule of
  `world.actions` with `exact: true` (one rule per outcome, `times: 1` each, for a call made
  several times with the same args), each signal an entry of `signals` at the time it arrived;
  `world.cassette`, `?as=cassette` and `rewind export --as cassette` go, and
  `rewind export --as test` writes a run as a test.
- **A test given a run**: `{ from: <run id> }` takes that run's answers.
- **One Step span per step** (V25), from its trigger to the commands it caused, in place of the
  Activation span; the commit of an activation that holds several steps stays service telemetry.

## 0.1 is done when

- A harness drives runs through the SDK, on several nodes sharing one Postgres and one bucket.
  Each run appears as one trace under the harness's span, and runs can be searched by label and
  action.
- Locally, `rewind run`, `rewind signal`, `rewind replay` and `rewind transcript` work on a run that
  sleeps, is interrupted, and resumes in another process. A forgotten `await`, an unhandled
  rejection and a deadlock each end as specified.
- A workflow calls a real MCP server, with types checked beforehand.
- The evals report, per model and prompt rendering, the first-try check pass rate, task
  success, iterations and tokens.
- `rewind test` replays a recorded real run as a test. `rewind fork` continues a failed run
  with corrected code without repeating its recorded calls.
- Load tests and limit tests pass.
- An operator finds any run by label, action or error in the dashboard, and sees its code next
  to its history. They step through a run, see the runs that wait and what for, and the
  failures grouped by fingerprint.
- Someone signs in to the dashboard with a code by email, creates an organization, invites a
  member with a role, and creates an API key; each endpoint refuses a caller without its
  permission.

### Acceptance status (November 2026)

`acceptance/release.sh` is the release gate, and `acceptance/run.sh` proves each criterion end to
end on the real `rewind` binary; what each check does is in
[acceptance/README.md](../acceptance/README.md). On 23 November 2026, on the run plane, the gate
said **GO**, 353 of 353 checks, and `acceptance/run.sh` was run for criteria 1, 2, 3, 4, 5 and 7,
which pass; criterion 8 is not run without WorkOS Staging's credentials. The rename release runs
them again in the new words.

| Criterion | Status | Proven by |
| --- | --- | --- |
| SDK, several nodes on one Postgres and one bucket, one trace per run, search by label and action | Passes | `acceptance/tests/c1-sdk-workers-postgres.spec.ts`: two nodes on Postgres and MinIO, a Collector writing to a file; harness actions answered by the SDK's worker on a call stream, a sleep, a signal, an approval |
| Locally across processes; forgotten `await`, unhandled rejection, deadlock | Passes | `crates/rewind/tests/cli.rs` (sleep, `kill -9`, resume in another process; `run`, `signal`, `status`, `transcript`, `replay`) and `acceptance/tests/c2-local-endings.spec.ts` |
| A real MCP server, types checked beforehand | Passes | `acceptance/tests/c3-c5-mcp-test-fork.spec.ts` with `@modelcontextprotocol/server-everything` 2.2.1 over stdio |
| The evals' report per model and prompt rendering | Passes with scripted models; the run with Claude and OpenAI models is pending API keys | `acceptance/tests/c4-evals.spec.ts` |
| `rewind test` of a recorded real run; `rewind fork` without repeating calls | Passes | `acceptance/tests/c3-c5-mcp-test-fork.spec.ts`, checked against the MCP server's own log of the calls it received |
| Load tests and limit tests | Passes | `deploy/acceptance.sh`, the gate's load stage: the limits through the API, then the reference deployment loaded by `rewind-load` with the thresholds of [Load tests](#load-tests), and the agent tools' bound against it |
| The dashboard: find by label, action or error; code next to the history | Passes | `acceptance/tests/c7-dashboard.spec.ts`, in Chromium against the dashboard `rewind server` serves |
| Sign-in with a code by email, an organization, an invitation with a role, an API key; each endpoint refuses a caller without its permission | Not run on the run plane: it needs WorkOS Staging's credentials (`WORKOS_API_KEY`, `WORKOS_CLIENT_ID`). It passed against Staging on 15 November 2026, and the gate's Access area covers the permission matrix with the operator's keys | `acceptance/tests/c8-accounts.spec.ts`, in Chromium against the dashboard `rewind server` serves with Staging's `WORKOS_*` |

## Load tests

`deploy/acceptance.sh` runs the reference deployment (`deploy/`: two nodes of 2 CPUs and 1 GiB
each, sharing Postgres 17 and a MinIO bucket for their partitions, the archive and the blobs, a
node of the previous epoch that runs activations only, a Collector, the egress gateway) and loads
it with `rewind-load`. Each run makes an MCP call, a call of a harness action answered on a call
stream, an MCP call that fails transiently once and is redelivered after its backoff (0.5–1 s),
sleeps 1 s, reads a signal sent twice with one idempotency key, and waits 100 ms for a second one:
about seven activations. A load passes when every run completed with its expected output, each
call reached its server or its worker as often as the failures it met allow (the echo and the
harness action once, the flaky call twice), and these thresholds hold:

| Load | Runs/s at least | p99 end to end at most | p99 activation at most |
| --- | --- | --- | --- |
| 1,000 runs, 100 at once | 10 | 15 s | 2 s |
| 200 runs, 50 at once, 100 KiB payloads (S3) | 4 | 20 s | 3 s |

Measured on the run plane on 23 November 2026, by the release gate's load stage, on a MacBook
(Apple silicon, Docker with 8 CPUs), latencies in milliseconds:

| Load | Runs/s | Calls/s | Start p50 / p99 | First call p50 / p99 | Activation p50 / p99 | End to end p50 / p99 |
| --- | --- | --- | --- | --- | --- | --- |
| 1,000 runs, 100 at once | 41.3 | 165 | 59 / 1,260 | 24 / 349 | 21 / 279 | 2,183 / 3,865 |
| 200 runs, 50 at once, 100 KiB payloads | 18.9 | 76 | 96 / 732 | 60 / 294 | 29 / 234 | 2,342 / 3,470 |

No error and no call beyond its failures in either. The floor of a run's end-to-end latency is
about 2 s: the sleep, the redelivery's backoff and the signal's timeout. Starts dominate the rest:
each start type-checks the code with tsgo, so a burst of starts queues there. An activation (an
action's result accepted to the run's next call) is a batch of its partition, one write to the
bucket, and the owner hands the next call on as soon as it is durable.

## Open questions

Questions 1, 2, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 and 17 were settled in November
2026: see [08-decisions.md](08-decisions.md#taken-after-the-layering-november-2026). Questions
2, 9 and 11 were settled with the [designs](#designs-settled-november-2026). Question 6
(`continueAsNew`) is [out of 0.1](#out-of-01).

Question 18 is open: a secret an agent passes in a run's input, instead of naming it among the
[secrets the run uses](04-tools.md#secrets), is kept in the history as any input is, and nothing
prevents it yet.
