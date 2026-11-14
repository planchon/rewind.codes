# 7. Code architecture

Approved for 0.1. Hexagonal layering: a pure domain, an application layer that declares
ports, adapters that implement them, and composition roots that wire everything.

## Layers

```
 entry points   rewind-cli (API client, local commands)      rewind-server (HTTP API, workers, embedded mode)
                    │  composition roots: the only crates that know every adapter
                    └───────────── rewind-api (DTOs, OpenAPI, HTTP client) ─────────────┘
 ───────────────────┼─────────────────────────────────────────────────────────────────────
 adapters       rewind-runtime   rewind-store-turso   rewind-store-postgres   rewind-blob   rewind-mcp   rewind-check   rewind-otel
                (V8)             └──────── rewind-format (stored format, v1) ────────┘
                    │  implement the ports
 ───────────────────┼─────────────────────────────────────────────────────────────────────
 application    rewind-engine     use cases + ports (traits) + in-memory adapters
                    │  uses the model
 domain         rewind-core       pure model + pure logic
```

Dependencies only point down. Adapters never depend on each other.

## Principles

1. **`rewind-core` holds the domain and nothing else.** Types and pure functions: events,
   journal, determinism check (`ReplayCursor`), run state, payload identity, durations,
   script and toolset model. No I/O, no async, no V8, **no serde**. It knows nothing about
   storage, transport, or where time and ids come from.

2. **Ports live in `rewind-engine`, and adapters implement them.** The engine holds the use
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
   | Stored format: events, journal documents, cassettes, pinned toolsets | `rewind-format`, versioned (`"v": 1`, [02](02-journal.md#stored-format-v1)) | Runs stay paused for days across deploys. The stored form must stay stable while domain types change. Migrations are explicit. |
   | HTTP / SDK | DTOs in `rewind-api` | Public contract with its own versioning (`/v1`). Shared by the server and the CLI. |
   | JS ↔ Rust | `rewind-runtime` | Internal to the sandbox; shaped for the glue code. |

5. **Core knows a payload's identity, not where it lives.** A payload is a JSON value
   identified by the digest of its canonical form; an event holds the value or only the
   digest. Inline, in the `payloads` table or in S3, key, size and preview are the
   adapters' business.

6. **Non-determinism comes in only through ports.** Real time, ids, seeds, tool results
   and signals all come from ports. Core and runtime are deterministic, and the engine is
   deterministic given its ports. So the engine with in-memory adapters and a virtual
   clock is both mock mode and the test harness.

7. **The runtime only executes.** `rewind-runtime` takes a script and a start, receives
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
    and the adapters. OpenTelemetry lives only in `rewind-otel`, installed by the
    composition roots. `rewind-core` has no telemetry. Details in
    [05](05-testing-and-observability.md#observability).

## Ports

All ports are declared in `rewind-engine` (`crates/rewind-engine/src/ports/`), each with an in-memory
adapter in `rewind-engine` that is the reference implementation of its contract. The ports with
state, `JournalStore`, `AccessStore`, `BlobStore`, `ReadModel` and `SecretStore`, and `SpanExporter` and
`WebhookSender` have a conformance suite in `rewind-engine` (feature `conformance`, modules
`conformance::journal`, `::access`, `::blobs`, `::read_model`, `::secrets`, `::spans`, `::webhooks`), and
every adapter of the port passes it. `SpanExporter` and `WebhookSender` have no read side: their
suites read the backend through a collector or a receiver the adapter's test provides (the
receiver of the HTTP sender checks each signature). The contracts of the other ports are not
observable through the port alone (the egress rules, an identity provider): their adapters test them.

| Port | Responsibility | Adapters |
| --- | --- | --- |
| `JournalStore` | Scripts and toolsets by digest; runs, history, inbox, timers, tool tasks; leases, heartbeats and fencing; `commit_activation`; failure counts and quarantine; the feed; webhook tokens; tenants, namespaces, their quotas, usage and alerts; sticky affinity; payload metadata and references; retention | memory (engine), `rewind-store-turso`, `rewind-store-postgres` |
| `AccessStore` | Who may do what: API keys, the tenants' roles, grants, each tenant's default role; end users' tokens and the keys that sign them | memory (engine), `rewind-store-turso`, `rewind-store-postgres` |
| `IdentityProvider` | Who someone is: sign-in, sessions and access tokens, users, organizations, memberships, invitations, the waitlist. Never what they may do | memory (engine), `rewind-workos` (WorkOS) |
| `BlobStore` | Large payload values by digest, per tenant; deletion for cleanup | memory (engine), `rewind-blob` (files, S3) |
| `ScriptExecutor` / `ScriptInstance` | Start a script, deliver completions, report emitted events and the outcome | `rewind-runtime` (V8); a fake in engine tests |
| `ToolDispatcher` | Execute a tool call with its idempotency key and credentials | static responses and cassettes (engine, mock mode); TypeScript mocks (`rewind-runtime`, in a separate isolate); `rewind-mcp` (MCP servers, built-ins `http` and `webhook`) |
| `TypeChecker` | Type-check a script against the sandbox lib and a toolset's declarations | static (engine), `rewind-check` (tsgo) |
| `ReadModel` | Projections for questions across runs | memory (engine), `rewind-store-turso`, `rewind-store-postgres` |
| `SpanExporter` | Export spans derived from committed entries | memory (engine), `rewind-otel` (OpenTelemetry) |
| `SecretStore` | Credentials per toolset, per namespace and toolset, and per run, encrypted at rest | memory (engine), `rewind-store-turso`, `rewind-store-postgres` |
| `WebhookSender` | Lifecycle events to the harness's subscriptions | memory (engine), HTTP (`rewind-server`) |
| `Clock` | Current time | system clock, virtual clock |
| `Entropy` | Run ids and seeds | system (uuid v7 / random), deterministic (tests) |

Host tools have no dispatcher: their tasks stay in the `JournalStore`, and the harness's
SDK worker claims them through `GET /v1/tasks` ([routes](04-tools.md#routes)).

### `JournalStore`

Methods (declared in `crates/rewind-engine/src/ports/journal/mod.rs`):

```
put_script(tenant, script, compiled) / get_script(tenant, digest)
put_toolset(tenant, toolset, declarations) / get_toolset(tenant, digest) / list_toolsets(tenant)
create_run(NewRun { run: (tenant, id), namespace, started, metadata, created_at }, key?)
    -> Created { run, created } | QuotaExceeded { quota, scope, limit, used, resets_at } | NamespaceDeleting
enqueue(run, completion, key?, now) -> Accepted | Duplicate | RunEnded
get_run(run)                       list_runs(RunFilter { tenant, namespace, status, labels, quarantined, after, limit })
acquire_runnable(AcquireRunnable { owner, epochs, tenants, now, lease_duration, sticky_grace, draw }) -> Lease { run, fence }
acquire_run(run, owner, now, lease_duration) -> Lease   # any run that has not ended, quarantined or not
acquire_quarantined(owner, build, epochs, tenants, now, lease_duration) -> Lease   # each quarantined run once per build
renew(lease, now, lease_duration)
load_history(run, from)            take_inbox(lease)
commit_activation(lease, Activation { entries, consumed, payloads, tool_calls, timers, cancelled_timers, status }, now)
release(lease)                     forget_sticky(run, owner)
record_failure(lease, error, now) -> Failures { count, quarantined }
quarantine(lease, kind, reason, now)   release_quarantine(run)
fire_due_timers(now, limit)        next_timer()
expire_due_runs(now, limit) -> ExpiredRuns { cancelled, quarantined }    next_deadline()
claim_tool_tasks(owner, TaskFilter::{All | Dispatched { tenants } | Host { tenant, namespace?, toolset, tools }}, now, limit)
    -> [ClaimedTask { task, token, previous_claim_expired }]
heartbeat_tool_task(token, now)    retry_tool_task(token, next_attempt_at)    next_tool_attempt(after)
defer_tool_task(token, until)      park_tool_task(token, reason)
complete_tool_task(token, outcome, now)    expire_deferred_tool_tasks(now, limit)
approve_tool_task(task, now) / reject_tool_task(task, failure, now)
resume_parked(Run(run) | Toolset { tenant, toolset } | Namespace { tenant, namespace, toolset }, now)
tool_tasks(run)                    tool_task(token)
create_webhook_token(run, name) -> token    resolve_webhook_token(token) -> { run, name }
put_subscription(subscription)     subscriptions(tenant)    delete_subscription(tenant, id)
get_payload(tenant, digest) -> { digest, size, preview, value? }
reserve_payloads(tenant, [PayloadRecord], now) -> [digest being deleted]
unreferenced_payloads(now, limit) -> [(tenant, digest)]    forget_payloads([(tenant, digest)], now)
drop_payloads([(tenant, digest)], started) -> count
runs_to_archive(now, limit)        archive_run(run, document, now)
runs_to_delete(now, limit)         delete_run(run, now)
put_tenant(tenant) / get_tenant(id) / list_tenants()
put_namespace(tenant, name, exceptions, now) / get_namespace(tenant, name)
list_namespaces(NamespaceFilter { tenant, prefix, above, order, month, after, limit }) -> [NamespaceUsage]
namespace_usage(tenant, Namespace(name) | Tenant, month) -> Usage { cpu, runs_started, active_runs, scripts, days }
revoke_namespace_tokens(tenant, name, now)    namespace_has_script(tenant, name, digest)
delete_namespace(tenant, name, now) -> Started | NotFound    namespaces_to_erase(limit)
drop_namespace(tenant, name) -> Dropped | RunsLeft
record_usage([UsageDelta { tenant, namespace, day, cpu }], now) -> [QuotaAlert]
quota_alerts(tenant, namespace?, month)
claim_quota_alerts(owner, now, lease_duration, limit) / ack_quota_alerts(ids, now)
claim_feed(consumer, owner, now, lease_duration, limit)    ack_feed(ids)
```

Stores never read a clock: every method that depends on time takes `now`, and the random draw
of `acquire_runnable` is passed in too. Tokens (tool tasks, webhooks) are the only values a store
draws from a CSPRNG. Every method is scoped by the tenant: a run (`RunRef { tenant, id }`), a
script, a toolset, a payload or a task of one tenant is never visible to another. Run ids are
unique across tenants. Text a store keeps never holds a NUL character (U+0000), which Postgres
refuses: the engine refuses it at its entry points (`NulCharacter`), and values are JSON, where
it is escaped ([decided](08-decisions.md#taken-while-fixing-the-review-of-the-engine-and-the-stores-november-2026)).
A key looked up with a NUL character matches nothing, and never reaches a store: a token or an id
is not found, and a filter is refused with 400
([decided](08-decisions.md#taken-while-fixing-the-runs-that-kill-their-workers-and-the-lookups-with-a-nul-november-2026)).

`metadata` is the run metadata (labels, trace context, the parent of a fork and its `at`), stored with
the run and never part of its events. `commit_activation` also writes one feed item per
consumer (`read_model`, `otel`, `webhooks`) in the same transaction: the store guarantees that
every committed activation is eventually projected.

Contracts, all covered by the conformance suite:

- `key` is an idempotency key. `create_run` with a key already used in the tenant returns the
  existing run; `enqueue` with a key already seen for that run does nothing
  ([storage](06-storage-and-scale.md#idempotency-of-the-api)).
- `create_run` creates the run's namespace if it does not exist, with the tenant's defaults.
  Under a lock of the namespace and of the tenant, it refuses the run (`QuotaExceeded`) when the
  namespace's limit or the tenant's ceiling of `activeRuns`, `runsPerMonth` or `scripts` is
  reached, a script already counted in the namespace passing `scripts`; it names the quota, the
  scope, the limit, the use and, for a monthly quota, the start of the next month. A first
  refusal of `activeRuns` in a month records its alert. A created run counts in the month's
  `runs_started` of its namespace and of the tenant, and in its script's `namespace_scripts`
  row; crossing 80 % or 100 % of `runsPerMonth` or `scripts` records the alert. A namespace being
  deleted refuses every run (`NamespaceDeleting`). A retried request with its key still returns
  its run, whatever the quotas ([quotas](06-storage-and-scale.md#quotas)). Idempotency keys are
  unique per namespace.
- `record_usage` adds CPU time to the namespace's day and to the tenant's (`*`), and records
  the alerts of `cpuPerMonth` whose 80 % or 100 % the addition crosses, once per tenant,
  namespace or `*`, month and threshold. `claim_quota_alerts` leases the alerts not yet sent,
  as `claim_feed` leases feed items; `ack_quota_alerts` marks them sent.
- `delete_run` removes one from the run's `namespace_scripts` row, and the row at zero.
  `delete_namespace` marks the namespace; `namespaces_to_erase` lists the marked ones;
  `drop_namespace` removes a marked namespace with its usage and alerts, but not the tenant's
  `*` rows, once it has no run left (`RunsLeft` otherwise).
- `revoke_namespace_tokens` sets the namespace's `tokens_revoked_before`, which refuses the
  signed tokens issued earlier.
- `acquire_runnable` only returns runs whose epoch is in `epochs` and whose tenant is in
  `tenants`, and never a quarantined run. It draws a tenant among those with runnable work
  (ordered by id, the one at `draw` modulo their number), then takes that tenant's oldest
  runnable run (the oldest inbox item first).
- Sticky affinity: `commit_activation` records the committing worker as the run's sticky
  worker. `acquire_runnable` gives a runnable run to its sticky worker first, and to the other
  workers only once its oldest inbox item has waited `sticky_grace` (5 s). `forget_sticky` drops
  the priority when the worker evicts the instance
  ([sticky execution](03-execution.md#sticky-execution)). Only a commit makes a worker sticky: a
  worker that dies during an activation holds the run until its lease expires, and its priority
  adds nothing after that, since the run's oldest inbox item has waited longer than the grace.
- `renew` extends a lease whose fence still matches. A worker calls it about every 10 s while
  it holds the run ([leases](06-storage-and-scale.md#leases-fencing-workers)).
- `record_failure` counts the run's activations that failed in a row and keeps the lease. The
  fifth quarantines the run with that error as its reason (kind `failures`) and releases the
  lease. A successful `commit_activation` and `release_quarantine` reset the count
  ([activations](03-execution.md#activations)).
- `acquire_runnable` counts the activation it leases the run for, in the run's `attempts`; a
  successful `commit_activation` and `release_quarantine` reset the count. `acquire_run` and
  `acquire_quarantined` do not count. An activation that neither committed nor recorded a
  failure never finished: its worker died.
- `quarantine` releases the lease without committing anything and records its kind and reason:
  the mismatch (kind `non_determinism`), or the activations that never finished (kind
  `failures`); `release_quarantine` makes the run runnable again; `acquire_quarantined`
  hands each quarantined run of the given epochs and tenants to one worker of a new build, once;
  a run of a tenant out of scope is left to a worker of its own pool
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
- No commit references a deleted blob. The engine reserves the rows of large payloads
  (`reserve_payloads`) before writing their blobs: a reserved row starts its grace again. The
  cleanup marks the rows it deletes (`forget_payloads`), deletes their blobs, then removes the
  rows still marked (`drop_payloads`). A commit refuses a payload held by digest whose row is not
  reserved or is marked (`Conflict`); a payload whose deletion has started is committed with its
  value in its row, which stops the deletion. A deletion not finished within
  `PAYLOAD_DELETION_TIMEOUT` (1 h) is started again ([decided](08-decisions.md#taken-while-fixing-the-review-of-the-engine-and-the-stores-november-2026)).
- `runs_to_archive` lists the finished runs past their tenant's journal retention whose feed
  items are all applied; `archive_run` removes their history once their journal document is in
  the `BlobStore` (never while feed items are left), and keeps the record and the payload
  references; `runs_to_delete` lists the archives past their retention; `delete_run` erases a
  run, its feed items and its payload references
  ([retention](06-storage-and-scale.md#retention-and-deletion)).
- Only scripts that compile are stored: `start_run` refuses the others before creating a run
  ([execution](03-execution.md#starting-a-run)). `ScriptExecutor::compile` takes the toolset,
  to check what the script imports from `rewind:tools`.
- `next_timer` and `next_deadline` let the driver jump in virtual time.

### The other ports

- **`AccessStore`** (`crates/rewind-engine/src/ports/access.rs`): the server's authorization
  data, apart from the journal, which the engine never reads.
  `put_key(key) / find_key(hash) / list_keys(tenant) / revoke_key(hash, now)`: keys are kept by
  the SHA-256 of their secret, with their tenant, name, creator and dates.
  `put_role(role) / get_role(tenant, name) / list_roles(tenant) / delete_role(tenant, name) ->
  Deleted | NotFound | InUse`: the tenant's own roles, their permissions as strings that the
  server interprets; `InUse` while a grant points to the role, decided in the same transaction.
  `put_token(token) / find_token(hash) / revoke_token(tenant, hash, now) / revoke_tokens(tenant,
  namespace, now) -> count / count_tokens(tenant, namespace, now)`: end users' opaque tokens, by
  the SHA-256 of their secret, with their tenant, namespace, permissions, labels, expiry, creator
  and dates; `find_token` returns a token even when it is revoked or expired, and the server
  refuses it. `put_token_key(key) / find_token_key(kid) / list_token_keys(tenant) /
  delete_token_key(tenant, kid)`: the public keys a tenant signs its tokens with (a JWK), named
  `rk_…`, with the permissions their tokens may carry.
  `put_grant(grant, keep) / grant(tenant, principal) / list_grants(tenant) /
  delete_grant(tenant, principal, keep)`: one grant per (tenant, principal), a principal being
  `user:<id>`, `key:<hash>` or `invitation:<id>`; `keep` names a role (`owner`) that the write
  must not leave without a holder in the tenant, else it returns `LastHolder` and changes
  nothing. `claim_grant(tenant, invitation, user)` moves an invitation's grant to the user who
  accepted it, unless the user already has one. `set_default_role(tenant, role) /
  default_role(tenant)`. Built-in roles are not stored: the server knows them.
- **`IdentityProvider`** (`crates/rewind-engine/src/ports/identity.rs`): `authorization_url`
  (GitHub), `authenticate_code`, `send_magic_code`, `authenticate_magic_code`, `verify_email`,
  `select_organization`, `refresh`, each answering `Authenticated` (the user, the organization if
  one is selected, the tokens) or the next step the provider asks for
  (`OrganizationSelectionRequired`, `EmailVerificationRequired`, `SignUpClosed`);
  `verify_access_token -> AccessClaims { user, session, organization, expires_at }`;
  `revoke_session`, `join_waitlist`; organizations (`create`, `rename`, `delete`, `get`),
  `user`, memberships (`of_user`, `of_organization`, `add`, `remove`), invitations (`invite`,
  `of_organization`, `for_email`, `get`, `accept`, `resend`, `revoke`). Memberships and
  invitations carry no role. Tokens are opaque to Rewind but for the access token's claims.
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
- **`ReadModel`**: `apply(run, entries, snapshot)`, idempotent and order-independent, keyed by
  `(run, index)`; of the snapshots of a run, the one with the highest history length wins
  ([read model](05-testing-and-observability.md#read-model)). `search_runs(query, order,
  page)` takes the typed terms of the [query language](05-testing-and-observability.md#query-language)
  (each negatable, all joined by "and"; `namespace` among them), and a scope: the tenant, a
  namespace, labels. `run_stats(query, window)` the counts, series, durations and facets of the
  runs a query matches, `failure_groups(query, window)` its runs grouped by fingerprint;
  `scripts(filter)`, `tool_stats(filter)`, both with an optional namespace,
  `observed_shapes(tenant, tools)`. Service state comes from whoever changes it: `record_tasks(run, tasks)` keeps the
  furthest state of each call's task (its attempt, and whether it waits for an approval, is
  parked or deferred), and `attempts(run)` reads the attempts back for `tool_stats`' retries;
  `record_quarantine(run, quarantine, at)` keeps the latest. `purge(ended_before, tenants)`
  and `delete_run(run)` serve [retention](06-storage-and-scale.md#retention-and-deletion).
- **`SpanExporter`**: `export(spans)`, `export_logs(logs)`. Spans have deterministic ids, so
  exporting one twice gives the same span.
- **`SecretStore`**: `put(tenant, owner, credentials)`, where the owner is a toolset, a run, or
  a namespace's toolset; `resolve(tenant, run, namespace, toolset)`, which returns the run's
  credentials if it has some, else the namespace's for the toolset, else the toolset's, never
  merged; `delete(tenant, owner)`, and `delete_namespace(tenant, namespace)` for an erased end
  user. Credentials are headers and
  environment variables per MCP server. Envelope encryption: each tenant has a data key, itself
  encrypted by a master key from the deployment's configuration. Secrets leave the store only
  toward the dispatcher ([credentials](04-tools.md#credentials)).
- **`WebhookSender`**: `send(subscription, event)`. Each delivery is signed with HMAC, and
  retried when it fails. Events are `run.completed`, `run.failed`, `run.cancelled`,
  `run.aborted`, `run.suspended`, `run.quarantined`, `tool.schema_changed`,
  `credentials.expired` and `quota.alert`, with a deterministic id for receivers to deduplicate;
  every event about a run names its namespace. Subscriptions
  are kept by the `JournalStore`.
- **`Clock`** and **`Entropy`**: unchanged.

Port style: traits whose methods return `impl Future<Output = …> + Send`; adapters may
implement them with `async fn`. The engine is generic over its adapters (static
dispatch); composition roots pick the concrete types. `ScriptExecutor` has an associated
`Instance: ScriptInstance` type.

## Crates

| Crate | Layer | Contents |
| --- | --- | --- |
| `rewind-core` | domain | ids (`RunId`, `Seq`, `Seed` on 256 bits, `Epoch`), `Digest`, `Payload` + canonical JSON (RFC 8785), `Script`, the pinned `Toolset` (servers, tools, routes, schemas, policies), events, `RunState`, `ReplayCursor`, history validation, `Cassette`, observed `Shape`, `Timestamp`, durations |
| `rewind-engine` | application | ports, use cases, activation, instance cache, replay, fork, driver (`run_until_idle`, real or virtual time), projector and span derivation, in-memory adapters (`MemoryJournal`, `MemoryAccess`, `MemoryIdentity`, `MemoryBlobs`, `MemoryReadModel`, `MemorySecrets`, `MemorySpanExporter`, `MemoryWebhooks`, `StaticDispatcher`, `StaticTypeChecker`, system and virtual clocks, system and seeded entropy), the conformance suites of the ports with state and of `SpanExporter` (feature `conformance`) |
| `rewind-format` | adapter support | serde DTOs for the stored format v1 (events, journal documents, cassettes, pinned toolsets; JSON values in their canonical form); golden fixture tests |
| `rewind-api` | adapter support | the HTTP DTOs (serde, utoipa) of `/v1` and `/auth`, the catalog of permissions and the built-in roles, the OpenAPI document generated from them, and a thin Rust HTTP client; shared by `rewind-server` and `rewind-cli` |
| `rewind-runtime` | adapter | deno_core isolate on a dedicated thread, started from a V8 startup snapshot, deterministic globals, ops, JS glue (one version per supported epoch), how a script ends, module loader, CPU watchdog (which also reports each call's CPU time), heap limit, the budget of array buffers and the pinned stack limit (`memory`, the only `unsafe` code), `V8Executor`; the TypeScript mock dispatcher |
| `rewind-store-turso` | adapter | embedded `JournalStore`, `AccessStore`, `ReadModel` and `SecretStore` (Turso Database, the `turso` crate) |
| `rewind-store-postgres` | adapter | production `JournalStore` (`FOR UPDATE SKIP LOCKED`), `AccessStore`, `ReadModel` and `SecretStore` |
| `rewind-workos` | adapter | `IdentityProvider` on WorkOS, through `workos`, its official Rust SDK: AuthKit's authentication, users, organizations, memberships, invitations, the waitlist, access tokens checked against the JWKS |
| `rewind-blob` | adapter | `BlobStore` on local files and S3 |
| `rewind-mcp` | adapter | `ToolDispatcher` outside mock mode: MCP client (discovery, dispatch, credentials in the transport) and the built-ins `http` (with its egress checks) and `webhook` |
| `rewind-codegen` | adapter support | our JSON Schema → TypeScript converter; pinned toolsets → `rewind:tools`, `rewind-tools.d.ts`, prompt rendering |
| `rewind-check` | adapter | type-checking with tsgo, driven as a CLI and embedded in the `rewind` binary; the checker's own rules on the swc syntax tree |
| `rewind-otel` | adapter | OpenTelemetry SDK and OTLP export (gRPC, HTTP): `SpanExporter`, and the `tracing` subscriber the composition roots install, which joins service spans to a run's trace through their fields `trace_id`, `span_id`, `parent_span_id` and `traceparent`, and records the measurements of the engine and the adapters as metrics |
| `rewind-cli` | composition root | the client commands of the `rewind` binary: a client of the API `/v1` (embedded or remote server); local commands `check`, `test`, replay of an exported journal |
| `rewind-server` | composition root | `rewind server`: HTTP API (axum), SSE, webhooks, host-tool tasks, workers, scheduler, projector; roles `api` and `worker`; embedded mode; serves the dashboard |
| `rewind` | entry point | the `rewind` binary: hands `rewind server …` to `rewind-server` and every other command to `rewind-cli`, nothing else |
| `rewind-architecture-tests` | tooling | dependency rules |
| `rewind-e2e` | tooling | the epoch corpus: recorded scenarios (blue: must succeed, red: must fail as specified), replayed from their stored journals; `rewind-e2e replay <case>` |
| `evals` | tooling | eval suite: tasks, deterministic fake MCP servers, the engine wired in process with the agent tools of the SDK, clients of the model APIs, reports ([08](08-decisions.md#taken-while-building-the-evals-november-2026)) |
| `sdk/typescript` | client | client (types generated from the OpenAPI document), host-tool worker, agent tools |
| `dashboard` | client | React single-page application over the public HTTP API, built to `dashboard/dist/` ([below](#dashboard)) |

One `rewind` binary holds both composition roots. The `rewind` crate is that binary: it depends on
`rewind-cli` and `rewind-server` and only dispatches the command line, `rewind server …` to
`rewind_server::main(args)` and every other command to `rewind_cli::main(args)`, each given the
arguments after its command. The CLI's code never calls the server's: it reaches it only
through the API, and starts the embedded server by running `rewind server --embedded` as a
separate process.

`rewind-server` wires each adapter in its own module of `src/wiring/` (store, blobs, tools,
checker, telemetry, webhooks, clock, identity), picked by configuration: `--store turso:<path>` or
`postgres://…`, `--blobs <directory>` or `s3://<bucket>`, `--otlp-endpoint`, WorkOS's
variables ([sign-in](#sign-in-auth)).

## Dependency rules

Checked by `crates/rewind-architecture-tests`, from `cargo metadata` and the sources:

- Every crate of the workspace is classified in a layer; an unclassified crate fails the check.
- `rewind-core` depends on no internal crate. Its external dependencies are limited to
  `ryu-js` (ECMAScript number formatting, for RFC 8785), `serde_json`, `sha2`, `thiserror` and
  `uuid` (without features).
- `rewind-engine` depends only on `rewind-core`.
- `rewind-format` depends only on `rewind-core`.
- `rewind-api` depends only on `rewind-core`. Only the composition roots and the tooling crates
  depend on it: the CLI is an API client, and cannot depend on a composition root to reach
  the DTOs.
- Adapters depend on `rewind-core` and `rewind-engine` (stores also on `rewind-format`), and never
  on each other.
- Only `rewind-cli` and `rewind-server` may depend on adapters, and the tooling crates
  (`rewind-e2e`, `evals`), which wire adapters like a composition root for tests. `rewind-cli` and
  `rewind-server` never depend on each other. Only the `rewind` binary crate depends on the
  composition roots. Nothing depends on a tooling crate.
- The `rewind` binary crate depends only on `rewind-cli` and `rewind-server`, and nothing depends on it.
- Libraries do not use `anyhow`: each has its own error types.
- Telemetry: `rewind-core` has no telemetry dependency (`tracing` included). The `tracing`
  facade is allowed in `rewind-engine` and the adapters. `opentelemetry*` crates are allowed
  only in `rewind-otel`.
- No `unsafe` code, with one exception: the `memory` module of `rewind-runtime`, for what V8
  offers only through raw pointers (the allocator of array buffers, a hook on collections, the
  stack limit). The workspace denies `unsafe_code` and every crate takes the workspace's lints;
  every crate root forbids it but `rewind-runtime`'s, which denies it so that `memory` alone may
  allow it. Every `unsafe` block there carries a `// SAFETY:` comment
  (`clippy::undocumented_unsafe_blocks` is denied). `rewind-architecture-tests` reads the sources
  and refuses `unsafe`, or a relaxed lint, anywhere else.

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

- Script checks at start fail → `StartError::InvalidScript`, and no run is created. A text to
  store with a NUL character → `StartError::NulCharacter` (a signal's key:
  `EngineError::NulCharacter`), before anything is stored.
- Executor start errors (top-level exception) → `run_aborted { invalid_script }`.
- The run's last 5 activations committed nothing (`attempts` above 5) → the script is not run;
  `journal.quarantine(lease, failures, reason)`, the reason saying how many never finished.
- Replay mismatch → nothing is committed; `journal.quarantine(lease, non_determinism, mismatch)`.
- Any other failure → nothing is committed; `journal.record_failure(lease, error, now)`.

## Tests

| Where | What |
| --- | --- |
| `rewind-core` | Unit tests of the pure model: canonical JSON (RFC 8785 golden vectors), digests, matching, `ReplayCursor`, run state, durations |
| `rewind-format` | Round trips and golden fixtures (a stored v1 document must always decode) |
| `rewind-engine` | Use cases with in-memory adapters and a fake executor; the conformance suites of `JournalStore` (idempotency keys, epochs, leases and renewal, quarantine, failure counts and attempts, run expiry, the feed, tool task lifecycle and concurrency, sticky affinity, tenants and fairness, payload references, retention), `BlobStore`, `ReadModel`, `SecretStore` and `SpanExporter`, passed by the in-memory adapters; span derivation; projector idempotence (applying a feed item twice changes nothing) |
| `rewind-runtime` | Sandbox behavior, driven by the engine with in-memory adapters and virtual time (`Promise.all`, time, randomness, signals, imports, callsites, cancellation, limits, the exact set of globals, RFC 8785 numbers against V8, and how a script ends: forgotten `await`, unhandled rejection, deadlock) |
| Epoch corpus (`rewind-e2e`) | `corpus/epoch-N/{blue,red}/<case>/`: a script, a `scenario.json` (input, tools, steps, expectations) and its recorded `journal.v1.json`. Each case runs live and must meet its expectations, must record the committed journal byte for byte, and the committed journal must replay with the script on a fresh executor. Blue cases must succeed; red cases must fail exactly as specified (refused start, failed, cancelled or aborted run, diverging replay, tampered journal). `REWIND_E2E_RECORD=1` records the journals again |
| `rewind-store-turso`, `rewind-store-postgres` | The conformance suites of `JournalStore`, `AccessStore`, `ReadModel` and `SecretStore` |
| `rewind-workos` | Against a fake WorkOS over HTTP (wiremock): every call, every error mapped, access tokens signed with a test key; against WorkOS Staging when `WORKOS_API_KEY` and `WORKOS_CLIENT_ID` are set |
| `rewind-blob` | The conformance suite of `BlobStore` |
| `rewind-otel` | The conformance suite of `SpanExporter`, on the SDK's in-memory exporters and on an OpenTelemetry Collector in Docker over gRPC and HTTP; service spans and logs joined to a run's trace by their fields; service and run metrics, the engine driven under the subscriber |
| `rewind-cli` | End to end through the embedded server: V8 + engine + Turso + files, across processes (sleep, interrupt, resume) |
| `rewind-architecture-tests` | Dependency rules, telemetry rules included; `unsafe` code only in `rewind-runtime`'s `memory` module |
| `evals` | Every task solved by its reference solution, through the agent tools and the scripted model; the fake servers, the agent tools' texts, the metrics; the model clients against local fakes of the Anthropic and OpenAI APIs |

## HTTP API `/v1`

`rewind-server` serves the API. The CLI, the SDK and its agent tools and the dashboard are its
clients. The evals use its DTOs, with the engine in process
([08](08-decisions.md#taken-while-building-the-evals-november-2026)).

Conventions:

- Authentication by a Bearer token: an API key, a WorkOS access token for the dashboard, or an
  end user's token (opaque `rewind_nt_…`, or a JWT the customer signed with a registered key). It
  designates the tenant; the caller's grant, or a token's own permissions, say what it may do,
  and every endpoint requires one permission of the catalog
  ([access](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)). A missing permission
  is 403, with a message that names it.
- `Rewind-Namespace` names the namespace of a key's or a member's request: required to start a
  run, `/v1/check` and `/v1/test`; on reads, it narrows to that namespace. A token's namespace is
  its own: the header may repeat it, and naming another is 403
  ([namespaces](06-storage-and-scale.md#namespaces)).
- `Idempotency-Key` on run creation (start and fork) and on signals
  ([idempotency](06-storage-and-scale.md#idempotency-of-the-api)).
- Errors are `application/problem+json`, with a message written for a model. Their `type` is
  `urn:rewind:problem:<slug>`; the slugs are listed in `api/README.md`.
- JSON fields are camelCase, enumerated values snake_case, times RFC 3339, durations `"30s"`
  or a number of milliseconds.
- The OpenAPI document is generated from the DTOs of `rewind-api` (utoipa) and committed at
  `api/openapi.json`; a test fails when it is stale. The SDK's and the dashboard's types are
  generated from it (`api/README.md`).

| Group | Endpoint | Permission | Does |
| --- | --- | --- | --- |
| Toolsets | `POST /v1/toolsets` | `toolsets:write` | Registers and pins a toolset; returns its declarations |
|  | `GET /v1/toolsets` | `toolsets:read` | The tenant's toolsets, with their servers and tool names |
|  | `GET /v1/toolsets/{id}/declarations` | `toolsets:read` | The declarations, `format=dts` or `prompt` |
|  | `GET /v1/toolsets/{id}/search` | `toolsets:read` | `search_tools`, for large toolsets |
|  | `PUT /v1/toolsets/{id}/credentials` | `credentials:write` | Sets or renews the toolset's [credentials](04-tools.md#credentials) |
|  | `PUT /v1/namespaces/{name}/toolsets/{id}/credentials` | `credentials:write` | Sets or renews an end user's credentials for the toolset |
| Scripts | `POST /v1/check` | `runs:start` | Type-check only |
|  | `POST /v1/test` | `runs:start` | Runs a scenario in mock mode, in virtual time |
| Runs | `POST /v1/runs` | `runs:start` | Starts a run: `Idempotency-Key`, `wait`, `labels`, `traceparent`, `ttl` |
|  | `GET /v1/runs` | `runs:read` | Lists runs: the [query language](05-testing-and-observability.md#query-language) `q`, filters by label, status, tool, error; `sort` by start, last activity or next wake |
|  | `GET /v1/runs/stats` | `runs:read` | The runs a query matches: counts by status, a series over a time window, durations, runs waiting by kind, facets |
|  | `GET /v1/runs/failures` | `runs:read` | The failed, cancelled and aborted runs a query matches, grouped by fingerprint |
|  | `GET /v1/runs/{id}` | `runs:read` | The run's state |
|  | `GET /v1/runs/{id}/trace?format=llm` | `runs:read` | The LLM trace |
|  | `GET /v1/runs/{id}/entries/{idx}` | `runs:read` | One journal entry in full |
|  | `GET /v1/runs/{id}/events` | `runs:read` | SSE, `follow=1` |
|  | `POST /v1/runs/{id}/signals/{name}` | `runs:control` | `Idempotency-Key`; 409 if the run has ended |
|  | `POST /v1/runs/{id}/cancel` | `runs:control` | Cancels the run |
|  | `POST /v1/runs/{id}/fork` | `runs:start` | `retry_run`: a corrected script, an optional `at` and `input` ([forks](03-execution.md#forks)) |
|  | `GET /v1/runs/{id}/journal` | `runs:read` | Exports the journal document; `?as=cassette` exports a cassette |
|  | `POST /v1/runs/{id}/release` | `runs:quarantine` | Takes the run out of quarantine |
|  | `POST /v1/runs/{id}/give-up` | `runs:quarantine` | Gives up a quarantined run |
|  | `PUT /v1/runs/{id}/credentials` | `credentials:write` | Sets or renews the run's credentials |
|  | `DELETE /v1/runs/{id}` | `runs:delete` | Erases the run, payloads included |
| Host-tool tasks | `GET /v1/tasks` | `tasks:work` | Long-poll, filtered by toolset and tools |
|  | `POST /v1/tasks/{token}/heartbeat` | `tasks:work` | Extends the task's claim |
|  | `POST /v1/tasks/{token}/defer` | `tasks:work` | The handler answers later: the task is `deferred`, without heartbeats, until `complete` or `fail` |
|  | `POST /v1/tasks/{token}/complete` | `tasks:work` | The result; also later, asynchronously |
|  | `POST /v1/tasks/{token}/fail` | `tasks:work` | The failure |
|  | `POST /v1/tasks/{id}/approve` | `tasks:approve` | Approval policy, parked tasks: the task goes on |
|  | `POST /v1/tasks/{id}/reject` | `tasks:approve` | The call fails with a `ToolError` |
| Webhooks | `POST /hooks/{token}` | none | Inbound, without authentication, at an unguessable URL: becomes a signal |
|  | `POST /v1/subscriptions` | `subscriptions:write` | Outbound: signed lifecycle events |
|  | `GET /v1/subscriptions` | `subscriptions:read` | Lists the subscriptions, without their secrets |
|  | `DELETE /v1/subscriptions/{id}` | `subscriptions:write` | Deletes a subscription |
| Reading (dashboard) | `GET /v1/scripts` | `runs:read` | Every distinct script, its runs, its tools |
|  | `GET /v1/scripts/{digest}` | `runs:read` | One script's source |
|  | `GET /v1/tools/stats` | `runs:read` | Latency, errors, retries |
|  | `GET /v1/payloads/{digest}` | `runs:read` | A payload, on demand, through a run that holds it (`run`, required with a token, a header, or a grant limited to labels) |
| Organization | `GET /v1/whoami` | any caller | Who calls: the tenant, the principal (a key's id, the user with their email, name and picture, or a token: its kind, `opaque` or `signed`, its id or key and `jti`, its expiry), the grant (role, allowed labels) or the token's namespace and labels, the permissions it holds; for a member, the organization and every organization of the user |
| | `POST /v1/organizations` | a member, even without organization | Creates an organization: in WorkOS with the caller's membership, then the tenant and the caller's `owner` grant; if the second step fails, the organization is deleted in WorkOS |
| | `PATCH /v1/organization` | `organization:write` | The organization's name and default role |
| | `GET /v1/invitations` | a member, even without organization | The pending invitations sent to the caller's email |
| | `POST /v1/invitations/{id}/accept` | a member, even without organization | Joins the invitation's organization; the invitation's pending grant becomes the caller's |
| | `GET /v1/organization/members` | `members:read` | The organization's memberships in WorkOS, each with its grant |
| | `PUT /v1/organization/members/{user}/grant` | `members:manage` | Sets the member's role and allowed labels |
| | `DELETE /v1/organization/members/{user}` | `members:manage` | Deletes the member's grant, then their membership in WorkOS |
| | `GET /v1/organization/invitations` | `members:read` | The organization's invitations, each with its pending grant |
| | `POST /v1/organization/invitations` | `members:invite` | Invites an email with a role and allowed labels: the invitation in WorkOS, the pending grant in Rewind |
| | `POST /v1/organization/invitations/{id}/resend` | `members:invite` | Sends the invitation again |
| | `DELETE /v1/organization/invitations/{id}` | `members:invite` | Revokes the invitation and deletes its pending grant |
| Access | `GET /v1/permissions` | any caller | The catalog: every permission with its description |
| | `GET /v1/roles` | `roles:read` | The built-in roles and the tenant's own, with their permissions |
| | `PUT /v1/roles/{name}` | `roles:write` | Creates or changes a role of the tenant |
| | `DELETE /v1/roles/{name}` | `roles:write` | Deletes a role no grant points to (409 otherwise) |
| | `GET /v1/api-keys` | `api_keys:read` | The tenant's keys, without their secret, each with its grant |
| | `POST /v1/api-keys` | `api_keys:write` | Creates a key with a name, a role and allowed labels; an operator may name another tenant. The secret is only in this answer |
| | `DELETE /v1/api-keys/{id}` | `api_keys:write` | Revokes a key |
| Namespaces | `GET /v1/namespaces` | `namespaces:read` | The namespaces, paged: `prefix`, `above` (a share of a limit, `0.8`), `order` (`cpu`, `runs`, `scripts`, `name`); each with its limits, its exceptions and its use this month |
|  | `GET /v1/namespaces/{name}` | `namespaces:read` | One namespace: its limits and exceptions, its use per day over a `month`, its alerts, its tokens in use |
|  | `PUT /v1/namespaces/{name}` | `namespaces:write` | Creates the namespace, or sets its exceptions; `null` gives a quota back to the default |
|  | `DELETE /v1/namespaces/{name}` | `namespaces:write` | Erases the end user in the background (202): tokens, runs, credentials, usage |
|  | `GET /v1/organization/namespace-defaults` | `namespaces:read` | The default limits of every namespace, and the tenant's ceiling |
|  | `PUT /v1/organization/namespace-defaults` | `namespaces:write` | Sets the default limits |
|  | `GET /v1/organization/quota-alerts` | `namespaces:read` | The alerts of a `month`, of one `namespace` or of all |
| Tokens | `POST /v1/namespaces/{name}/tokens` | `tokens:write` | Mints an end user's opaque token: `ttl`, `permissions`, `labels`; creates the namespace if needed. The secret is only in this answer |
|  | `DELETE /v1/namespaces/{name}/tokens` | `tokens:write` | Revokes the namespace's opaque tokens and the signed ones issued before now |
|  | `DELETE /v1/tokens/{id}` | `tokens:write` | Revokes one opaque token |
|  | `GET /v1/organization/token-keys` | `tokens:write` | The keys the tenant signs its tokens with |
|  | `POST /v1/organization/token-keys` | `tokens:write` | Registers a public key (a JWK: Ed25519, P-256 or RSA) with a name and the permissions its tokens may carry; answers its `kid` |
|  | `DELETE /v1/organization/token-keys/{kid}` | `tokens:write` | Deletes a key: every token it signed is refused |
| Operator | `PUT /v1/organizations/{tenant}/quotas` | the operator | Sets a tenant's ceiling |
| Server | `GET /v1/server` | any caller | The server's version, epochs and roles |
| | `POST /v1/server/stop` | the operator | Stops the embedded server, when the CLI replaces it with one of its own version |

### Sign-in (`/auth`)

The dashboard's sign-in, served by the api role when WorkOS is configured. These routes are for a
browser only: they read and write the cookie `rewind_session`, and `/v1` never reads it. They are
described in the OpenAPI document like `/v1`.

| Endpoint | Does |
| --- | --- |
| `GET /auth/config` | The sign-in methods (`github`, `magic_auth`) and whether sign-ups go through the waitlist; 404 without WorkOS, and the dashboard asks for an API key |
| `GET /auth/github` | Redirects to WorkOS for GitHub, with a random `state` and the `invitation_token` of the page, both kept in the cookie `rewind_oauth` (5 minutes) |
| `GET /auth/callback` | Checks `state`, exchanges the code, keeps the outcome in `rewind_session`, redirects to the dashboard (`/#/`); when the sign-in did not go through, to `/?auth_error=<code>#/` (`state_mismatch`, `provider_error`, `sign_in_failed`), never with a token or WorkOS's message |
| `POST /auth/magic-auth` | Sends the 6-digit code to an email; the same answer whether the email has an account or not |
| `POST /auth/magic-auth/verify` | Checks the code |
| `POST /auth/email-verification` | The code WorkOS sends when it requires the email to be verified |
| `POST /auth/organization` | The organization chosen when WorkOS asks for one |
| `POST /auth/session` | The current state: a valid access token, refreshed when less than a minute is left; with `organizationId`, switches organization. Writes a member's first grant in an organization |
| `POST /auth/waitlist` | Puts an email on the waitlist |
| `POST /auth/logout` | Revokes the session in WorkOS and clears the cookie |

- `POST /auth/magic-auth` and `POST /auth/waitlist` answer 202 and `POST /auth/logout` 204, with no
  body. Every other answer but the redirects is an `AuthState`: `authenticated` (the access token, its
  expiry, the user, the organization or none), `organization_selection_required` (the
  organizations), `email_verification_required` (the email), `sign_up_closed` (the email, for the
  waitlist), or 401 without a session.
- `rewind_session` holds the access token, the refresh token and the pending token of a step,
  sealed with AES-256-GCM (`workos`'s `seal`, keyed by `WORKOS_COOKIE_PASSWORD`). Both cookies
  are `HttpOnly`, `Secure` and `Path=/auth`. `rewind_session` is `SameSite=Strict`; `rewind_oauth`
  is `SameSite=Lax`, since the browser comes back to `/auth/callback` from GitHub and WorkOS, a
  navigation from another site, which a `Strict` cookie would miss. The callback only reads it. A `POST` takes a JSON body only (415
  otherwise), which a form of another site cannot send without a preflight.
- The server always keeps the refresh token WorkOS returns last. The access token lives 5
  minutes (WorkOS's setting); the browser keeps it in memory only.

| Variable | Value |
| --- | --- |
| `WORKOS_CLIENT_ID` | The WorkOS environment's client, `client_…` |
| `WORKOS_API_KEY` | Its secret key, `sk_…`. On the api role only, never on a worker |
| `WORKOS_COOKIE_PASSWORD` | 32 random bytes in hexadecimal (64 characters). Changing it signs everyone out |
| `WORKOS_REDIRECT_URI` | `https://app.rewind.codes/auth/callback`: registered in WorkOS, on the dashboard's origin |

The four are given together or not at all (exit code 64 otherwise). Without them the server has
no `/auth` and accepts only API keys; keys still get their rights from grants and roles. A worker
accepts only API keys: the dashboard never calls the routes the edge sends to workers.

## Embedded server and CLI

The CLI never opens `journal.db`. It is a client of the API `/v1`: of a remote server, or of
the embedded server of a `.rewind/` directory. Only one process opens the database, so Turso
stays in its stable single-process mode, and the instance cache lives in that server.

`rewind server --embedded` is the engine host for a `.rewind/` directory. It takes both roles, api
and worker.

| When | What happens |
| --- | --- |
| A command needs the server | If `.rewind/server.lock` is free, no server runs: the command starts one, detached, with `rewind server --embedded --dir <dir> --idle-timeout 10m`. |
| The server starts | It takes an exclusive lock on `.rewind/server.lock`, opens `journal.db`, and listens on the unix socket `.rewind/server.sock`. Due timers fire at startup. |
| A second server starts | It fails to take the lock and exits with code 75. |
| The server is killed (`kill -9`) | The lock is released. The next command starts a new server, and expired leases give the runs back. |
| The CLI's version differs from the server's | The CLI asks the server to stop and starts a new one. |
| 10 minutes pass without requests and without work in progress | The server stops. A sleeping run resumes at the next `rewind` command. |

- The socket is mode 0600 and takes no token. Its one caller is the operator of the implicit
  tenant `local`.
- The CLI names a namespace with `--namespace` or `REWIND_NAMESPACE`. Against its own embedded
  server it uses `local` when none is given; against a remote server, a command that starts a
  run without one is a usage error (64).
- `rewind server` run in the foreground never stops on idle: only `--idle-timeout` makes it.
- Virtual time is an option of the server (`--virtual-time`, `--real-time`), kept in the
  `meta` table.
- `rewind server` exits with 0 once stopped, 64 for a usage error, 70 when it cannot start, and 75
  when another server holds the directory's lock.
- Local-only commands run in the CLI process, without a server: `rewind check`, `rewind test`, and
  the replay of an exported journal.

The `.rewind/` directory:

```
project/
  script.ts
  rewind.tools.json         static responses per tool, for mock mode
  .rewind/                  created when the first server starts; --dir picks another
    server.lock             held by the embedded server
    server.sock             the API /v1, mode 0600
    journal.db              Turso: every table of the store; meta holds the schema version and virtual time
    journal.db-wal          Turso's write-ahead log
    master.key              the master key of the secret store, mode 0600, made at the first start
    blobs/
      local/sha256/ab/cdef… payloads above 64 KiB of the tenant local, named by their digest
      .staging/             where a blob is written and synced before it is renamed
```

Commands:

| Command | Does |
| --- | --- |
| `rewind run script.ts [--input …] [--wait 30s] [--ttl 30d]` | Starts a run and waits for it, up to `--wait` |
| `rewind check script.ts` | Type-checks a script against the toolset's declarations ([tools](04-tools.md#type-checking)) |
| `rewind test scenario.json` | Runs a scenario against mocks and cassettes, in virtual time ([testing](05-testing-and-observability.md#testing-and-mocks)) |
| `rewind signal <run> <name> [payload]` | Sends a signal |
| `rewind cancel <run>` | Cancels a run |
| `rewind replay <run> [--script fixed.ts]` | Replays locally and checks determinism; `<run>` is a run id or an exported journal |
| `rewind fork <run> --script fixed.ts [--at <index>] [--input …]` | Starts a [fork](03-execution.md#forks) |
| `rewind trace <run> [--entry n]` | LLM-readable trace |
| `rewind runs` | Lists runs: a query of the [query language](05-testing-and-observability.md#query-language) (`rewind runs '@agent:triage status:failed'`), filters, `--sort` |
| `rewind release <run> [--give-up]` | Takes a run out of quarantine, or gives it up |
| `rewind status <run> [--wait 30s]` | A run's state, waiting up to `--wait` for it to end |
| `rewind export <run> [--as journal\|cassette] [-o file]` | The run's journal document, or a cassette of its calls and signals |
| `rewind toolsets add\|declarations\|search` | Registers a toolset file; its declarations; searches its tools |
| `rewind namespaces list\|show\|quotas\|delete` | The namespaces and their use; one namespace; sets its exceptions, or the defaults with `--defaults`; erases one |
| `rewind server [--embedded] [--role api\|worker] [--virtual-time] [--idle-timeout 10m]` | Runs the server in the foreground (`rewind server --help` lists its options) |

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

Its build is what `rewind-server` serves:

| | |
| --- | --- |
| Build | `pnpm --dir dashboard install --frozen-lockfile && pnpm --dir dashboard build` |
| Output | `dashboard/dist/`: `index.html`, `favicon.svg`, `assets/` (scripts, styles, fonts, named by content hash). Not committed: `rewind-server` embeds the directory when it is compiled |
| Serving | Static files under any path prefix. Every path in the build is relative and the routes live in the URL fragment (`#/runs/…`), so no fallback route is needed. `index.html` is not cached; `assets/` is immutable |
| Sign-in | With WorkOS: its own pages, GitHub or a code by email, through `/auth` on the same origin ([sign-in](#sign-in-auth)); onboarding (create an organization, accept an invitation), the organization picker, the waitlist. Without WorkOS: an API key typed by the operator, kept for the tab, or on the device when asked |
| API | `/v1` on the same origin, with the access token or the key as Bearer. The views follow the permissions `whoami` answers: reading needs `runs:read`; Members, Invitations, Roles and API keys are shown to whoever holds their permissions, Namespaces to whoever holds `namespaces:read` |
| Namespace | The filter of a namespace, kept in the URL (`?ns=`), is sent as `Rewind-Namespace` on every request |
| Event stream | Read with `fetch`, not `EventSource`, which cannot send the `Authorization` header; resumed with `from` after a drop |
| Resources | None from another origin: the fonts are in the build. No inline script. The one image from elsewhere is a member's picture, as WorkOS gives it |

`dashboard/README.md` has the development commands; `pnpm dev:mock` runs it against a mocked API.
