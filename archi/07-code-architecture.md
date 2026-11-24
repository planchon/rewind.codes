# 7. Code architecture

Approved for 0.1. Hexagonal layering: a pure domain, an application layer that declares
ports, adapters that implement them, and composition roots that wire everything.

## Layers

```
 entry points   rewind-cli (API client, local commands)      rewind-server (edge, node, embedded mode)
                    │  composition roots: the only crates that know every adapter
                    └───────────── rewind-api (DTOs, OpenAPI, HTTP client) ─────────────┘
 ───────────────────┼─────────────────────────────────────────────────────────────────────────────
 adapters       rewind-runtime  rewind-store-slatedb  rewind-store-turso  rewind-store-postgres  rewind-blob
                (V8)            └──────────── rewind-format (stored format, v1) ────────────┘
                rewind-mcp  rewind-openapi  rewind-check  rewind-otel  rewind-workos
                └ rewind-outbound ┘
                    │  implement the ports
 ───────────────────┼─────────────────────────────────────────────────────────────────────────────
 application    rewind-engine     use cases, actors, ports (traits), in-memory adapters
                    │  uses the model
 domain         rewind-core       pure model + pure logic
```

Dependencies only point down. Adapters never depend on each other.

## Principles

1. **`rewind-core` holds the domain and nothing else.** Types and pure functions: events,
   history, determinism check (`ReplayCursor`), run state, payload identity, durations,
   code and action set model, run, workflow and partition ids. No I/O, no async, no V8,
   **no serde**. It knows nothing about storage, transport, or where time and ids come from.

2. **Ports live in `rewind-engine`, and adapters implement them.** The engine holds the use
   cases (start, activate, signal, cancel, replay, project, fork, seal, erase) and the actors
   that run them ([inside a node](#inside-a-node)), and declares what it needs from the outside
   world as traits. It never names Postgres, SlateDB, S3, V8, MCP or OpenTelemetry.

3. **Ports follow the domain's invariants, not the keys.** "A trigger is committed atomically
   with everything it caused" is one port method, `PartitionLog::commit`, taking what changed;
   the engine never composes keys. Fencing is part of the port contract. Every adapter of a port
   passes the same conformance suite.

4. **Each boundary owns its representation.** Domain types are converted at the edge and
   never leak through.

   | Boundary | Owner | Why it is separate |
   | --- | --- | --- |
   | Stored format: events, run exports, partition records, cassettes, pinned action sets, workflow versions | `rewind-format`, versioned (`"v": 1`, [02](02-history.md#stored-format-v1)) | Runs stay paused for days across deploys. The stored form must stay stable while domain types change. Migrations are explicit. |
   | HTTP / SDK | DTOs in `rewind-api` | Public contract with its own versioning (`/v1`). Shared by the server and the CLI. |
   | Between nodes | the node link's DTOs in `rewind-server` | Internal to a deployment: every node of a deployment runs the same build, apart from the executors kept for a previous epoch, which speak the same link. |
   | JS ↔ Rust | `rewind-runtime` | Internal to the sandbox; shaped for the glue code. |

5. **Core knows a payload's identity, not where it lives.** A payload is a JSON value
   identified by the digest of its canonical form; an event holds the value or only the
   digest. In the event, in the run's records or in a blob, key, size and preview are the
   adapters' business.

6. **Non-determinism comes in only through ports.** Real time, ids, seeds, action results
   and signals all come from ports. Core and runtime are deterministic, and the engine is
   deterministic given its ports. Its actors are state machines that the driver steps one
   message at a time, so the engine with in-memory adapters and a virtual clock is both what
   executes a test run and what the engine's own tests drive.

7. **The runtime only executes.** `rewind-runtime` takes a code and a start, is given
   completions and signals, and returns the emitted events. It knows nothing about history,
   replay, storage or dispatch. `replay()` belongs to the engine, and so does the instance cache.
   V8 objects can't change threads, so instances run on dedicated threads behind the
   `CodeExecutor` port; the engine never handles a V8 type.

8. **Errors don't cross boundaries raw.** Each crate has its own error type. Adapters
   convert turso, sqlx, SlateDB, object store and deno errors into port errors. No `anyhow` in
   libraries.

9. **Tooling enforces the boundaries, not discipline.** A test crate reads
   `cargo metadata` and asserts the dependency rules below.

10. **The history is the record; telemetry and dashboards are projections.** What a
    run did (OTel spans, the run index behind the dashboard, SSE) is derived from
    committed events, never emitted from the isolate, from the runtime, or during
    replay. The platform's own behavior goes through the `tracing` facade in the engine
    and the adapters. OpenTelemetry lives only in `rewind-otel`, installed by the
    composition roots. `rewind-core` has no telemetry. Details in
    [05](05-testing-and-observability.md#observability).

11. **Nothing polls.** A component waits on one channel and wakes when a message arrives. The
    only timers are the timer wheel, the backoffs after an error, and a few clocks that
    read no store (the embedded server's idle stop, the metrics export, the dashboard's session
    refresh and its display clock, the SSE keep-alive comment, the daily jobs). `rewind-architecture-tests` refuses `tokio::time::interval` and
    `sleep` anywhere else, and the dashboard's lint refuses `refetchInterval`
    ([decided](08-decisions.md#taken-for-the-object-storage-rewrite-november-2026)).

## Ports

All ports are declared in `rewind-engine` (`crates/rewind-engine/src/ports/`), each with an in-memory
adapter in `rewind-engine` that is the reference implementation of its contract. The ports with
state, `CatalogStore`, `PartitionStore`, `RunArchive`, `Cluster`, `AccessStore`, `BlobStore` and
`SecretStore`, and `SpanExporter` and `WebhookSender` have a conformance suite in `rewind-engine`
(feature `conformance`, modules `conformance::catalog`, `::partition`, `::archive`, `::cluster`,
`::access`, `::blobs`, `::secrets`, `::spans`, `::webhooks`), and every adapter of the port passes
it. `SpanExporter` and `WebhookSender` have no read side: their suites read the backend through a
collector or a receiver the adapter's test provides (the receiver of the HTTP sender checks each
signature). The contracts of the other ports are not observable through the port alone (the egress
rules, an identity provider, the node link): their adapters test them.

| Port | Responsibility | Adapters |
| --- | --- | --- |
| `CatalogStore` | The control plane's catalog: code and action sets by digest; workflows by id, by name in their scope and by code, with their versions and their suites; the index of tests; tenants; namespaces, with their partition, display name, exceptions and erasure; subscriptions | memory (engine), `rewind-store-turso`, `rewind-store-postgres` |
| `PartitionStore` | One partition of the run plane, with one writer: runs and their heads, history per attempt, inbox, signal keys, the tasks of calls, timers, payloads, watchers, signal URL tokens; the run index and summaries; start keys; counters; the outbox; the states of the tenants homed there; fencing | memory (engine), `rewind-store-slatedb` |
| `RunArchive` | The sealed attempt of each ended attempt, and the erasure of a run's objects | memory (engine), `rewind-blob` (files, S3) |
| `Cluster` | The nodes alive, the partition map and its generations, a node's liveness, and the changes of the control plane | memory (engine), `rewind-store-turso` (one node), `rewind-store-postgres` |
| `NodeLink` | Requests, subscriptions and activations sent to another node | in process (engine), HTTP/2 (`rewind-server`) |
| `AccessStore` | Who may do what: API keys, the tenants' roles, grants, each tenant's default role; end users' tokens and the keys that sign them | memory (engine), `rewind-store-turso`, `rewind-store-postgres` |
| `IdentityProvider` | Who someone is: sign-in, sessions and access tokens, people, organizations, memberships, invitations, the waitlist. Never what they may do | memory (engine), `rewind-workos` (WorkOS) |
| `BlobStore` | Payload values above 64 KiB under their run, test documents under their tenant; deletion of a run's blobs | memory (engine), `rewind-blob` (files, S3) |
| `CodeExecutor` / `CodeInstance` | Start a code, give it completions and signals, report emitted events and the outcome | `rewind-runtime` (V8); a fake in engine tests |
| `ActionDispatcher` | Execute a call with its idempotency key and its authentication: a secret, or the tenant's signing key | rules and cassettes (engine, test runs); TypeScript handlers (`rewind-runtime`, in a separate isolate); `rewind-mcp` (MCP servers, built-ins `http` and `signalUrl`); `rewind-openapi` (APIs) |
| `TypeChecker` | Type-check a code against the sandbox lib and an action set's declarations | static (engine), `rewind-check` (tsgo) |
| `SpanExporter` | Export spans derived from committed events | memory (engine), `rewind-otel` (OpenTelemetry) |
| `SecretStore` | Named secrets of the organization and of each namespace, their metadata and reads, and each tenant's signing keys; values and private keys encrypted at rest | memory (engine), `rewind-store-turso`, `rewind-store-postgres` |
| `WebhookSender` | Webhooks to the organization's subscriptions | memory (engine), HTTP (`rewind-server`) |
| `Clock` | Current time | system clock, virtual clock |
| `Entropy` | Run ids, workflow ids and seeds | system (uuid v7 / random), deterministic (tests) |

Harness actions have no dispatcher: the tenant actor hands their calls to the harness's worker
over its call stream (`GET /v1/calls/stream`, [routes](04-tools.md#routes)).

Every store method is scoped by the tenant: a run (`RunRef { tenant, id }`), a workflow, a code, an
action set, a payload or a task of one tenant is never visible to another. Run ids are unique across
tenants. Stores never read a clock: every method that depends on time takes `now`. Tokens (of the
calls handed to a worker, of signal URLs) are the only values a store draws from a CSPRNG. Text a
store keeps never holds a NUL character (U+0000), which Postgres refuses: the engine refuses it at
its entry points (`NulCharacter`), and values are JSON, where it is escaped
([decided](08-decisions.md#taken-while-fixing-the-review-of-the-engine-and-the-stores-november-2026)).
A key looked up with a NUL character matches nothing, and never reaches a store: a token or an id
is not found, and a filter is refused with 400
([decided](08-decisions.md#taken-while-fixing-the-runs-that-kill-their-workers-and-the-lookups-with-a-nul-november-2026)).

### `CatalogStore`

Methods (declared in `crates/rewind-engine/src/ports/catalog.rs`):

```
put_code(tenant, code, compiled) / get_code(tenant, digest)
put_action_set(tenant, action_set, declarations) / get_action_set(tenant, digest) / list_action_sets(tenant)
create_workflow(tenant, NewWorkflow { id, scope, name?, description?, source?, version, suite }, now)
    -> Created(StoredWorkflow) | Existing(StoredWorkflow) | NameTaken
workflow(tenant, id) -> StoredWorkflow?
workflow_named(tenant, namespace?, name, labels) -> StoredWorkflow?   # the namespace's, else the organization's
workflow_with_code(tenant, namespace, code, labels) -> (StoredWorkflow, StoredVersion)?
list_workflows(WorkflowFilter { tenant, scope, code?, labels, after, limit })
update_workflow(tenant, id, WorkflowPatch { name?, description? }, now) -> Written(StoredWorkflow) | NameTaken | NotFound
put_version(tenant, id, NewVersion { document, labels, pulled? }, now) -> StoredVersion
versions(tenant, id, labels) -> [StoredVersion]   version(tenant, id, Number(n) | Digest(d)) -> StoredVersion?
change_suite(tenant, id, SuiteChange { added, replaced, retired }, now) -> [SuiteTest]   suite(tenant, id)
promote_workflow(tenant, id, now) -> Promoted(StoredWorkflow) | NameTaken | NotFound
reserve_tests(tenant, [TestRecord { digest, namespace?, size }], now) -> [digest being deleted]
get_test(tenant, digest) -> TestRecord    namespace_tests(tenant, namespace, limit) -> [digest]
unreferenced_tests(now, limit) -> [(tenant, digest)]    drop_tests([(tenant, digest)], erased) -> count
put_subscription(subscription)     subscriptions(tenant)    delete_subscription(tenant, id)
put_tenant(tenant) / get_tenant(id) / list_tenants()
ensure_namespace(tenant, name, partitions, now) -> Namespace | Deleting   # created at first use, its partition chosen
put_namespace(tenant, name, exceptions, now) / get_namespace(tenant, name)
set_namespace_display_name(tenant, name, display_name?) -> Written(Namespace) | Deleting | none
namespace_display_names(tenant) -> [(name, display_name)]
list_namespaces(NamespaceFilter { tenant, name, prefix, after, limit }) -> { [Namespace], total }
revoke_namespace_tokens(tenant, name, now)
delete_namespace(tenant, name, now) -> Started | NotFound    namespaces_to_erase(limit)
drop_namespace(tenant, name) -> Dropped
```

- A workflow's id (`wf_` and 12 characters) is drawn by the engine from `Entropy`; its scope is a
  namespace or the organization ([workflows](01-product.md#workflows)). `create_workflow` writes the
  workflow, its version 1 and its suite in one transaction. Without a name, for a code its scope
  already has, it answers `Existing` with that workflow and changes nothing; a name its scope already
  has is `NameTaken`. A fork is created the same way, in its namespace, with its `source` (the
  organization's workflow and version), version 1 a copy of that version, and the source's suite.
- `workflow_named` resolves a name in the namespace given, then among the organization's workflows: a
  namespace's workflow shadows the organization's of the same name. `workflow_with_code` reads
  `workflow_codes`: in the namespace then the organization, under `labels`, the workflow that first
  had that code there, and its version with that code (the latest, if several). Both see only the
  workflows whose versions carry `labels`, as `versions` and `list_workflows` do. Every write that
  gives a workflow a version with a code its scope has not seen under those labels adds the code to
  `workflow_codes` in the same transaction.
- A version is immutable. `put_version` takes the next number of its workflow, from 1, under a lock
  of the workflow; with `pulled` (a fork taking its source's version), it moves the fork's `source`
  to that version in the same transaction. `versions` lists a workflow's versions carrying `labels`,
  newest first; `version` finds one by its number or its digest, whatever its labels.
- `update_workflow` sets a workflow's name and description, without a version: its id never
  changes. `promote_workflow` moves a namespace's workflow to the organization, with its id, its
  versions and its suite, unless the organization has its name (`NameTaken`).
- Tests ([suites](05-testing-and-observability.md#suites-and-the-gate)): the documents are
  blobs; the store indexes them. `reserve_tests` inserts a test's row, or makes an existing one
  new again, before its blob is written, and returns the digests whose deletion has started.
  A workflow's suite is its rows of `workflow_tests`: `create_workflow` and `change_suite` write
  them, and `put_version` the tests the version passed; each refuses (`Conflict`) a test without a
  row, or whose deletion has started. `unreferenced_tests` marks the rows no suite and no version
  references, stored more than 24 hours ago, and returns them; their blobs are deleted, then
  `drop_tests` removes the rows still marked. With `erased`, it removes the given rows whatever
  references them: a namespace's erasure takes the tests `namespace_tests` lists.
- `ensure_namespace` creates a namespace at its first use with the tenant's defaults and its
  partition, `hash(tenant, name) mod partitions`, and returns it; a namespace being deleted is
  `Deleting`. The partition of a namespace never changes.
- `revoke_namespace_tokens` sets the namespace's `tokens_revoked_before`, which refuses the
  signed tokens issued earlier. `delete_namespace` marks the namespace; `namespaces_to_erase`
  lists the marked ones; `drop_namespace` removes a marked namespace, its workflows with their
  versions and suites, once its partition holds nothing of it.
- Only code that compiles is stored: `start_run` refuses the rest before creating a run
  ([execution](03-execution.md#starting-a-run)). `CodeExecutor::compile` takes the action set,
  to check what the code imports from `rewind:actions`.
- Every write sends its `ControlPlaneChange` in the same transaction (`Cluster` below); every
  write of a workflow, its versions or its suite sends the workflow's.

### `PartitionStore`

Methods (declared in `crates/rewind-engine/src/ports/partition/`):

```
open(partition, node) -> PartitionLog            # the only writer from now on: fences the previous one
recover(horizon) -> Recovery { runnable, quarantined, timers, tasks, unsealed, outbox, counters, tenants }
run(run) -> RunHead?                  summary(run) -> RunSummary?
runs_in_range(tenant, low, high) -> [RunHead]     # the runs a handle's suffix names, by id
history(run, attempt, from) -> [HistoryEvent]    inbox(run) -> [InboxItem]
tasks(run) -> [CallTask]              payload(run, digest) -> PayloadRow?
watchers(run) -> [Watcher]            timers(from, to) -> [DueTimer]
started(tenant, namespace, key) -> RunId?        signal_key(run, key) -> bool
signal_url(token_hash) -> (RunId, name)?
search(Scope, RunQuery, RunOrder, Page { cursor, limit, budget }) -> Found { summaries, cursor }
counters(Scope, Window) -> Counters   gauges(Scope) -> Gauges     shapes(tenant, namespace?, actions)
tenant(tenant) -> TenantState?
commit(Batch) -> ()                   # atomic and durable; Fenced once another node opened the partition
```

A `Batch` is a list of changes, of one or many runs, applied together:

```
CreateRun { run, head, start_key?, events?, payloads, summary }   # events: a fork's copied history
Enqueue { run, item, signal_key? }
Activation { run, attempt, events, consumed, payloads, tasks, timers, head, summary, counters, outbox }
Task { run, seq, change }             Timer(Set | Cancel)
Failure { run, error }                Quarantine { run, kind, reason } | Release { run }
Seal { run, attempt }                 Erase { run }             EraseNamespace { tenant, namespace }
Interrupted { run }                   Watch { run, watcher }
Outbox(Add | Remove)                  Counters([CounterDelta])
SignalUrlToken { run, name, hash }    Tenant { tenant, state }
```

Contracts, all covered by the conformance suite:

- `open` fences: once another `open` of the same partition succeeded, every `commit` of the earlier
  log fails with `Fenced`, and none of its changes is visible. A commit that returned is durable: a
  later `open`, on any node, reads it.
- A batch is atomic: after a crash, either all its changes are read or none.
- The owner is the only writer, so the store checks what a single writer may get wrong and refuses
  it (`Conflict`): events appended anywhere but at the attempt's history length, a start key
  already used in the namespace, a consumed inbox item that does not exist, a change to a run that
  does not exist or has been erased.
- `Enqueue` with a signal key already seen for that run changes nothing. An `Activation` that ends
  the run clears its timers, tasks and inbox, and its signal URL tokens stop resolving; nothing is
  enqueued for an ended attempt.
- `Activation` writes, with the events, the payload records of the payloads that left the events
  (digest, size, preview, the value up to 64 KiB), the run's summary and its index entries, and the
  counters it moves.
- A run's head and summary hold the status its owner computed for the batch (`queued`, `running`,
  `waiting` or an ending) and what it waits for; the store keeps what it is given, and indexes the
  summary by it.
- `recover(horizon)` returns the runs that are runnable (a non-empty inbox, not quarantined, not
  ended), the quarantined runs, the timers due before `horizon`, the tasks of the calls that are
  `ready`, `deferred` or in `approval`, the attempts that ended and are not sealed, the outbox, the
  counters of every tenant, and the tenant states homed here.
- `Seal` deletes the attempt's history, inbox, tasks and payload records, and keeps the run's
  head, summary, index entries and signal keys, which live as long as the run
  ([idempotency](06-storage-and-scale.md#idempotency-of-the-api)). `Erase` deletes everything of the run, and the counts it
  held. Neither is refused for a run already sealed or erased.
- `search` evaluates every term of the query on the summaries of the scope, in the order asked,
  reads at most `budget` summaries, and returns a cursor when it stopped before the end. Summaries
  are kept for ended runs until their erasure.
- `counters` sums the hourly counters of the scope over the window, rounded out to whole hours;
  `gauges` reads the exact gauges (active runs, running and waiting runs, waiting runs by kind,
  the quarantined among them).
- Timers are ordered by due time, then by run and kind; `timers(from, to)` includes `from` and
  excludes `to`.

### `RunArchive`

```
put(SealedAttempt { run, attempt, code, action_set, events }) -> ()   # idempotent
get(run, attempt) -> SealedAttempt?
erase(run) -> ()                                               # every attempt and every blob of the run
```

The archive writes the [run export](02-history.md#stored-format-v1) of the attempt, every
payload up to 64 KiB inlined. A second `put` of the same attempt changes nothing. `get` of an
erased run finds nothing.

### `Cluster` and `NodeLink`

```
Cluster::join(NodeInfo { id, address, build, epochs, pool, executors_only }) -> Membership
Membership::watch() -> Departed { node }      # the lock this node waits on came free
Membership::assign(Moves) -> PartitionMap     # one transaction: owners and generations + 1, then the change sent
Membership::partition_map() -> PartitionMap   Membership::nodes() -> [NodeInfo]
Membership::changes() -> stream of ControlPlaneChange
Membership::leave()                           # after the node handed its partitions over

NodeLink::call(node, NodeRequest) -> NodeReply
NodeLink::subscribe(node, SubscriptionKey, from) -> stream of Changed
NodeLink::activate(node, ActivationRequest) -> ActivationResult      # a remote executor
```

- A node's liveness is its membership: on Postgres, a session advisory lock on the node's id held
  on a connection of its own, released when the process or its connection dies. `watch` blocks on
  the next node's lock and returns once it is granted.
- `assign` moves partitions in one transaction, raising each moved partition's generation, and
  sends `PartitionMap` as it commits.
- `changes` yields every `ControlPlaneChange` committed by any process after the call: the partition
  map, a namespace, a tenant, a key, a token or token key, a grant or role, a connection, a secret, a
  workflow, a subscription. On Turso and in memory, the cluster has one member and the changes are
  a broadcast inside the process.
- `NodeLink` carries the requests an edge sends an owner (start, signal, cancel, fork, release, give
  up, erase, a call's answer, reads), those an owner sends a tenant actor and back
  (admission, counts, slots, harness calls), the hand-out of a harness call to the edge that holds
  the worker's call stream, subscriptions, and activations for remote executors. An owner that no
  longer holds a partition answers `NotOwner`.

### The other ports

- **`AccessStore`** (`crates/rewind-engine/src/ports/access.rs`): the server's authorization
  data, apart from the run plane, which the engine never reads for it.
  `put_key(key) / find_key(hash) / list_keys(tenant) / revoke_key(hash, now)`: keys are kept by
  the SHA-256 of their secret, with their tenant, name, creator and dates.
  `put_role(role) / get_role(tenant, name) / list_roles(tenant) / delete_role(tenant, name) ->
  Deleted | NotFound | InUse`: the tenant's own roles, their permissions as strings that the
  server interprets; `InUse` while a grant points to the role, decided in the same transaction.
  `put_token(token) / find_token(hash) / revoke_token(tenant, hash, now) / revoke_tokens(tenant,
  namespace, now) -> count / count_tokens(tenant, namespace, now) / purge_tokens(before, limit)
  -> count`: end users' opaque tokens, by the SHA-256 of their secret, with their tenant,
  namespace, permissions, labels, expiry, creator and dates; `find_token` returns a token even
  when it is revoked or expired, and the server refuses it; `purge_tokens` deletes, in every
  tenant, those expired or revoked before a time. `put_token_key(key) / find_token_key(kid) / list_token_keys(tenant) /
  delete_token_key(tenant, kid)`: the public keys a tenant signs its tokens with (a JWK), named
  `rk_…`, with the permissions their tokens may carry.
  `put_grant(grant, keep) / grant(tenant, principal) / list_grants(tenant) /
  delete_grant(tenant, principal, keep)`: one grant per (tenant, principal), a principal being
  `member:<id>`, `key:<hash>` or `invitation:<id>`; `keep` names a role (`owner`) that the write
  must not leave without a holder in the tenant, else it returns `LastHolder` and changes
  nothing. `claim_grant(tenant, invitation, member)` moves an invitation's grant to the member who
  accepted it, unless the member already has one. `set_default_role(tenant, role) /
  default_role(tenant)`. Built-in roles are not stored: the server knows them. Every write sends
  its `ControlPlaneChange`.
- **`IdentityProvider`** (`crates/rewind-engine/src/ports/identity.rs`): `authorization_url`
  (GitHub), `authenticate_code`, `send_magic_code` (`Sent`, or `SignUpClosed` without a code),
  `authenticate_magic_code`, `verify_email`,
  `select_organization`, `refresh`, each answering `Authenticated` (the person, the organization if
  one is selected, the tokens) or the next step the provider asks for
  (`OrganizationSelectionRequired`, `EmailVerificationRequired`, `SignUpClosed`);
  `verify_access_token -> AccessClaims { person, session, organization, expires_at }`;
  `revoke_session`, `join_waitlist`; organizations (`create`, `rename`, `delete`, `get`),
  `person` (a WorkOS user), memberships (`of_person`, `of_organization`, `add`, `remove`),
  invitations (`invite`, `of_organization`, `for_email`, `get`, `accept`, `resend`, `revoke`).
  Memberships and invitations carry no role. Tokens are opaque to Rewind but for the access token's
  claims.
- **`BlobStore`**: `put(owner, digest, bytes)`, `get(owner, digest)`, `copy(from, to, digest)`,
  `delete_run(run)`, `delete(owner, digest)`. The owner is a run (`BlobOwner::Run`), for its payload
  values above 64 KiB, or a tenant's tests (`BlobOwner::Tests`). Immutable bytes keyed by their
  SHA-256: a payload's canonical JSON, so the key is the payload's digest, or a test's document.
  `put` refuses bytes that do not hash to the digest; a second `put` changes nothing. `get` checks
  the digest of what it reads. `copy` serves a fork, `delete_run` the erasure of a run, `delete` the
  cleanup of tests.
- **`CodeExecutor` / `CodeInstance`**: `compile`, `start`, `deliver`, `memory_usage`
  (the memory an instance holds, which bounds the instance cache), and `cpu_time` (the CPU time
  of the instance's last call into the code, failed or not: loading the code and
  `main(input)` after `start`, one inbox item after `deliver`). The executor knows nothing about
  the instance cache nor about replay, which live in the engine: the engine decides which calls
  into the code count toward a namespace's `cpuPerMonth` ([quotas](06-storage-and-scale.md#quotas)).
- **`ActionDispatcher`**: `dispatch(call, auth) -> Dispatched`. The call carries the run, its
  namespace and labels, the `seq`, the pinned action and its server or API, the args, the
  idempotency key, the delivery, whether the harness approved a schema change (then the call is
  sent without checking the schema again), the task's token and the trace context. `auth`
  (`CallAuth`) is what the server's `auth` asks ([authenticating
  calls](04-tools.md#authenticating-calls)): nothing, the value of the secret the run uses for that
  server with where it goes, or the tenant's signing key with the claims the engine knows (`iss`,
  `sub`, `labels`, `run`, `action` and `tool` beside it, `idempotency_key`, the delivery), with
  which the dispatcher signs each request it builds, adding `aud`, `htm`, `htu`, `body`, `iat`, `exp`
  and `jti`, since the JWT covers the method, the URL and the body. A test run passes nothing.
  `Dispatched` is one of: `Result` and `Failed` (the outcome); `Transient`, after which the engine
  puts the task back to `ready` with backoff for a redelivery, or fails the call when its action is
  not `idempotent`; `Deferred`, the handler answering later through the task's token; `Park` with a
  reason (schema changed at dispatch, credentials refused). The dispatcher never writes to a store.
  The engine parks a call itself (`secret_missing`) when a secret the run uses no longer exists,
  before anything is sent.
- **`TypeChecker`**: `check(code, declarations) -> diagnostics`, and
  `check_workflow(code, declarations, interface)` for the code of a version that has an interface,
  `interface` being its own declarations (its signals, and `entry.ts`). Deterministic.
  Diagnostics are written for a model, with a location in `workflow.ts`, a severity, their source
  (a TypeScript code or one of the checker's own rules), a hint and a code frame
  ([type-checking](04-tools.md#type-checking)).
- **`SpanExporter`**: `export(spans)`, `export_logs(logs)`. Spans have deterministic ids, so
  exporting one twice gives the same span.
- **`SecretStore`**: secrets are keyed by tenant, namespace (`*` for the organization's) and
  name: a `SecretId`, what a run's `secrets` name once resolved in the run's namespace.
  `put(tenant, NewSecret { id, value, labels, by }, now) -> Created | Replaced` creates or
  replaces one; `list(tenant, namespace, labels, since) -> [SecretMeta]` and `meta(tenant, id,
  since)` answer names, labels, who wrote them and when, reads per day from `since` and the last
  read (by a run, with its namespace, or by a registration; date), never a value; `read(tenant,
  id, SecretReader::Run { run, namespace } | SecretReader::Registration, now) -> Option<Secret>`
  answers the value and counts the read, for the dispatcher's path and a registration's listing
  only;
  `delete(tenant, id)`, and `delete_namespace(tenant, namespace)` for an erased end user. Signing keys: `signing_key(tenant, now) -> SigningKey`, made at the first call;
  `rotate_signing_key(tenant, now)`; `public_keys(tenant, now) -> [Jwk]`, the current key and
  those retired less than 24 hours ago. Envelope encryption: each tenant has a data key, itself
  encrypted by a master key from the deployment's configuration; a value is bound to its tenant,
  namespace and name, a private key to its tenant and `kid`. The adapter holds the current
  master key and the previous ones; `master_keys_in_use() -> [id]` answers the ids its data keys
  are held under, and `rewrap_data_keys() -> count` encrypts those held under a previous key with
  the current one ([master key](04-tools.md#secrets)). Values and private keys leave the
  store only toward the dispatcher ([secrets](04-tools.md#secrets)). `put` and `delete` send their
  `ControlPlaneChange`: the owners resume the calls parked for the secret.
- **`WebhookSender`**: `send(subscription, webhook)`. Each webhook is signed with HMAC, and sent
  again when sending it fails. Its types are `run.completed`, `run.failed`, `run.cancelled`,
  `run.aborted`, `run.waiting`, `run.quarantined`, `action.schema_changed`,
  `secret.expired` (with its reason, `refused` or `missing`, and the scope and name of the secret
  the run uses when there is one) and `quota.alert`, with a deterministic id for receivers to
  deduplicate; every webhook about a run names its namespace. Subscriptions are kept by the
  `CatalogStore`.
- **`Clock`** and **`Entropy`**: unchanged. `Entropy` draws a run id for a partition: a UUID of version 8, laid
  out as a version 7, whose 16 bits after the variant hold it; and a workflow's id, `wf_` and 12
  characters of Crockford's base32 (lower case).

Port style: traits whose methods return `impl Future<Output = …> + Send`; adapters may
implement them with `async fn`. The engine is generic over its adapters (static
dispatch); composition roots pick the concrete types. `CodeExecutor` has an associated
`Instance: CodeInstance` type, `PartitionStore` an associated `Log: PartitionLog`.

## Crates

| Crate | Layer | Contents |
| --- | --- | --- |
| `rewind-core` | domain | ids (`RunId`, `WorkflowId`, `Seq`, `Seed` on 256 bits, `Epoch`), `Digest`, `Payload` + canonical JSON (RFC 8785), `Code`, the pinned `ActionSet` (servers, actions, routes, schemas, policies), events, `RunState`, `ReplayCursor`, history validation, `Cassette`, observed `Shape`, `Timestamp`, durations |
| `rewind-engine` | application | ports, use cases, the actors of a node (`partition`: the owner, its committer, fan-out, timer wheel, registry, outbox and sealer; `executors`: the ready queue, the instance cache, activation and replay; `tenant`: the tenant actor; `tasks`: calls and their policies; `node`: partitions taken and handed over, recovery; `router`: the edge's way to an owner), fork, workflows across runs, workflows (found by code or name, versions through the gate, promotion, forks and pulls), projector and span derivation, the driver (the actors stepped on one thread, real or virtual time), in-memory adapters (`MemoryCatalog`, `MemoryPartitions`, `MemoryArchive`, `MemoryCluster`, `LocalLink`, `MemoryAccess`, `MemoryIdentity`, `MemoryBlobs`, `MemorySecrets`, `MemorySpanExporter`, `MemoryWebhooks`, `StaticDispatcher`, `StaticTypeChecker`, system and virtual clocks, system and seeded entropy), the conformance suites of the ports with state and of `SpanExporter` (feature `conformance`) |
| `rewind-format` | adapter support | serde DTOs for the stored format v1 (events, run exports, partition records, cassettes, pinned action sets, workflow versions, tests; JSON values in their canonical form); the names stored before the rename release, read only by `rename-data` (below); golden fixture tests |
| `rewind-api` | adapter support | the HTTP DTOs (serde, utoipa) of `/v1` and `/auth`, the catalog of permissions and the built-in roles, the OpenAPI document generated from them, and a thin Rust HTTP client; shared by `rewind-server` and `rewind-cli` |
| `rewind-runtime` | adapter | deno_core isolate on a dedicated thread, started from a V8 startup snapshot, deterministic globals, ops, JS glue (one version per supported epoch), how a run ends, module loader (with the names code already written uses, `rewind:tools`, `webhook`, `ToolError` and `WorkflowError`, resolved without being declared), CPU watchdog (which also reports each call into the code's CPU time), heap limit, the budget of array buffers and the pinned stack limit (`memory`, the only `unsafe` code), `V8Executor`; the dispatcher of a test's TypeScript handlers |
| `rewind-store-slatedb` | adapter | `PartitionStore` on SlateDB over `object_store`: S3 or any S3-compatible service (R2, MinIO…), local files, memory; the key layout of a partition |
| `rewind-store-turso` | adapter | the embedded control plane: `CatalogStore`, `AccessStore`, `SecretStore`, and a `Cluster` of one node (Turso Database, the `turso` crate) |
| `rewind-store-postgres` | adapter | the production control plane: `CatalogStore`, `AccessStore`, `SecretStore`, and `Cluster` (session advisory locks, `LISTEN` and `NOTIFY`) |
| `rewind-workos` | adapter | `IdentityProvider` on WorkOS, through `workos`, its official Rust SDK: AuthKit's authentication, users, organizations, memberships, invitations, the waitlist, access tokens checked against the JWKS |
| `rewind-blob` | adapter | `BlobStore` and `RunArchive` on local files and S3 or any S3-compatible service |
| `rewind-mcp` | adapter | `ActionDispatcher` outside test runs: MCP client (discovery, dispatch, the secret the run uses or the signature in the transport) and the built-ins `http` and `signalUrl` |
| `rewind-openapi` | adapter | API actions ([04](04-tools.md#api-actions)): an OpenAPI 3.0 or 3.1 document read into pinned actions at registration, and `ApiDispatcher`, the `ActionDispatcher` of the `api` route: the request built from the args, its authentication, the answer mapped to an outcome |
| `rewind-outbound` | adapter support | what leaves Rewind for a tenant's servers: the egress checks (a name resolved once, private addresses refused, the connection to the checked address, redirects within the origin) and the HTTP client built on them, the JWT signing of [signed calls](04-tools.md#signed-calls); used by `rewind-mcp`, `rewind-openapi` and the server's webhooks |
| `rewind-agent-tools` | adapter support | the agent tools in Rust, as the SDK defines them: their definitions (the SDK's set, and MCP's: `update_workflow` out, `list_organizations`, `list_namespaces` and `list_action_sets` in, `organization`, `namespace` and `action_set` as arguments; `list_secrets` and the `secrets` argument in both), their handlers over a port `Api` (one method per operation of `/v1` they call, with `rewind-api`'s DTOs), and their bounded plain-text results; golden fixtures shared with the SDK's |
| `rewind-mcp-server` | adapter support | Rewind's MCP server ([MCP and OAuth](#mcp-and-oauth)): an `rmcp` `ServerHandler` over `rewind-agent-tools`, with the tools' titles, annotations and `instructions`, served over Streamable HTTP without session (by `rewind-server`) or stdio (by `rewind-cli`) |
| `rewind-codegen` | adapter support | our JSON Schema → TypeScript converter; pinned action sets → `rewind:actions`, `rewind-actions.d.ts` (with `rewind:workflows`), prompt rendering; a version's own declarations, from its interface |
| `rewind-check` | adapter | type-checking with tsgo, driven as a CLI and embedded in the `rewind` binary; the checker's own rules on the swc syntax tree |
| `rewind-otel` | adapter | OpenTelemetry SDK and OTLP export (gRPC, HTTP): `SpanExporter`, and the `tracing` subscriber the composition roots install, which joins service spans to a run's trace through their fields `trace_id`, `span_id`, `parent_span_id` and `traceparent`, and records the measurements of the engine and the adapters as metrics |
| `rewind-cli` | composition root | the client commands of the `rewind` binary: a client of the API `/v1` (embedded or remote server); local commands `check`, `test`, replay of an exported run; `rewind mcp` over stdio |
| `rewind-server` | composition root | `rewind server`: the edge (HTTP API with axum, SSE, the call stream, `/mcp`, the OAuth authorization server of its connections, the dashboard) and the node (partitions, tenant actors, executors, projections, webhooks) in one process; the node link over HTTP/2 and its DTOs; `--executors-only` and `--pool`; embedded mode; `migrate-runs` and `rename-data` |
| `rewind` | entry point | the `rewind` binary: hands `rewind server …` to `rewind-server` and every other command to `rewind-cli`, nothing else |
| `rewind-architecture-tests` | tooling | dependency rules |
| `rewind-e2e` | tooling | the epoch corpus: recorded scenarios (blue: must succeed, red: must fail as specified), replayed from their stored histories; `rewind-e2e replay <case>` |
| `evals` | tooling | eval suite: tasks, deterministic fake MCP servers, the engine wired in process behind `rewind-agent-tools`' port `Api`, clients of the model APIs, reports ([08](08-decisions.md#taken-while-building-the-evals-november-2026)) |
| `sdk/typescript` | client | client (types generated from the OpenAPI document), the worker of harness actions, agent tools, `verifyRewindCall` for a server that receives [signed calls](04-tools.md#signed-calls) (`jose`) |
| `dashboard` | client | React single-page application over the public HTTP API, built to `dashboard/dist/` ([below](#dashboard)) |

One `rewind` binary holds both composition roots. The `rewind` crate is that binary: it depends on
`rewind-cli` and `rewind-server` and only dispatches the command line, `rewind server …` to
`rewind_server::main(args)` and every other command to `rewind_cli::main(args)`, each given the
arguments after its command. The CLI's code never calls the server's: it reaches it only
through the API, and starts the embedded server by running `rewind server --embedded` as a
separate process.

`rewind-server` wires each adapter in its own module of `src/wiring/` (control plane,
partitions, archive and blobs, cluster, actions, checker, telemetry, webhooks, clock, identity),
picked by configuration: `--store turso:<path>` or `postgres://…` for the control plane,
`--bucket <directory>` or `s3://<bucket>` (with its region, and its endpoint for a service other than AWS S3) for the partitions, the
archive and the blobs, `--partitions` (64 by default, 1 embedded), `--cluster-database` (where the
nodes take their liveness locks, `postgres` by default), `--cluster-secret`, `--link-listen` and
`--link-address` (the node link's port and the address the other nodes dial), `--node-id`,
`--organization-pool <organization>=<pool>` (the organizations whose activations run on a pool of
their own, given to every node), `--otlp-endpoint`, WorkOS's variables ([sign-in](#sign-in-auth)).

`rewind server rename-data` rewrites, once, what a deployment stored under the names it had before
the rename release ([decided](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)).
It runs offline: the node is stopped, the command runs with the same configuration, and the new
build starts after it; it is rehearsed first on a copy of production's data. It reads each document
and record under the names it was written with, which `rewind-format` decodes for this command
only, and writes it under the new ones, through the stores' adapters: the partitions and the
archive (events, heads, summaries, tasks, timers, the outbox, the tenant states, the sealed
attempts, each run's status computed from its waits), the catalog (action sets, workflows and
their versions, the code that ran given its workflows, the titles people gave becoming names where
they are valid and free), the access store (`user:` principals become `member:`) and the
subscriptions (`types`). The format stays `"v": 1`. Every content-addressed document, a pinned action
set or a workflow version, gets the digest of its new form, and every reference to it is rewritten:
names and version numbers keep working, an old digest stops resolving. A code's digest does not
change: its text is not rewritten. The tokens of signal URLs (`rewind_wh_`) and of calls
(`rewind_tk_`), and the handles already given, are kept as they are.

## Dependency rules

Checked by `crates/rewind-architecture-tests`, from `cargo metadata` and the sources:

- Every crate of the workspace is classified in a layer; an unclassified crate fails the check.
- `rewind-core` depends on no internal crate. Its external dependencies are limited to
  `ryu-js` (ECMAScript number formatting, for RFC 8785), `serde_json`, `sha2`, `thiserror` and
  `uuid` (without features).
- `rewind-engine` depends only on `rewind-core`.
- `rewind-format` depends only on `rewind-core`.
- `rewind-api` depends only on `rewind-core`. Only the composition roots, the tooling crates,
  `rewind-agent-tools` and `rewind-mcp-server` depend on it: the CLI is an API client, and
  cannot depend on a composition root to reach the DTOs.
- `rewind-agent-tools` depends only on `rewind-core` and `rewind-api`; `rewind-mcp-server` only
  on those and `rewind-agent-tools`. Only the composition roots and the tooling crates depend on
  them. Neither knows the engine: the agent tools reach Rewind through `/v1`'s operations.
- Adapters depend on `rewind-core` and `rewind-engine` (the stores, `rewind-store-slatedb` and
  `rewind-blob` also on `rewind-format`, the action dispatchers also on `rewind-outbound`), and never
  on each other. `rewind-outbound` depends only
  on `rewind-core`.
- Only `rewind-cli` and `rewind-server` may depend on adapters, and the tooling crates
  (`rewind-e2e`, `evals`), which wire adapters like a composition root for tests. `rewind-cli` and
  `rewind-server` never depend on each other. Only the `rewind` binary crate depends on the
  composition roots. Nothing depends on a tooling crate.
- The `rewind` binary crate depends only on `rewind-cli` and `rewind-server`, and nothing depends on it.
- Libraries do not use `anyhow`: each has its own error types.
- Telemetry: `rewind-core` has no telemetry dependency (`tracing` included). The `tracing`
  facade is allowed in `rewind-engine` and the adapters. `opentelemetry*` crates are allowed
  only in `rewind-otel`.
- No polling: `tokio::time::interval`, `tokio::time::sleep` and `sleep_until` appear only in the
  engine's timer wheel, the backoff helpers and the clocks that read no store
  ([principles](#principles)); the dashboard's lint refuses `refetchInterval`.
- No `unsafe` code, with one exception: the `memory` module of `rewind-runtime`, for what V8
  offers only through raw pointers (the allocator of array buffers, a hook on collections, the
  stack limit). The workspace denies `unsafe_code` and every crate takes the workspace's lints;
  every crate root forbids it but `rewind-runtime`'s, which denies it so that `memory` alone may
  allow it. Every `unsafe` block there carries a `// SAFETY:` comment
  (`clippy::undocumented_unsafe_blocks` is denied). `rewind-architecture-tests` reads the sources
  and refuses `unsafe`, or a relaxed lint, anywhere else.

## Inside a node

Every actor is a state machine: it takes a message, changes its state, and returns effects (a
batch to commit, messages to send, timers to set). It does no I/O itself. In a process, a task
drives each actor over its channel; in tests, the driver steps every actor of every node one
message at a time on one thread, and jumps a virtual clock to the earliest timer of the wheels
when nothing else is left to do.

| Actor | Module | One per | Waits on | Sends |
| --- | --- | --- | --- | --- |
| Owner | `partition` | Partition held | Its mailbox: the edge's requests, executors' results, call results, `TimerDue` | Batches to its committer, with the status it computed for each run they change; after each durable batch: runs to the ready queue, timers to the wheel, calls to the call executor or to the tenant actor, `Changed` to subscribed edges, ended attempts to the sealer, committed events to the projections, counts to the tenant actors |
| Committer | `partition` | Partition held | The batch being filled | One `PartitionLog::commit`, then `Durable` to the owner |
| Timer wheel | `partition` | Partition held | Its earliest timer, and timers set or cancelled | `TimerDue`; once an hour, a read of the next hour's timers |
| Sealer | `partition` | Partition held | `Ended`, once durable | `RunArchive::put`, then `Seal` to the owner |
| Executors | `executors` | Node | The node's ready queue, and activations from other owners | The activation's result to the owner of the run |
| Call executor | `tasks` | Node | Calls that are durable and hold a slot | `ActionDispatcher::dispatch`, then the outcome to the owner and `SlotFreed` |
| Tenant actor | `tenant` | Tenant homed in a held partition | Its mailbox | `Admitted` or `Refused`, `SlotGranted`, `CallHanded`, alerts in its own batches |
| Node | `node` | Process | `Membership::watch`, `Membership::changes`, a stop signal | `open` and recovery of the partitions it takes, hand-over of those it gives |
| Router | `router` | Process | The edge's requests | A channel to a local owner, or `NodeLink::call` |

An activation, from the ready queue to its batch:

```
run ← ready queue (fair across tenants; fewest started activations, then oldest trigger)
history ← log.history(run, attempt) unless the instance cache holds the run at its history length
instance ← cache[run], or executor.start + ReplayCursor over history
for item in inbox(run):
    recorded_at ← max(clock.now(), last recorded_at)
    if the attempt ended: drop the item
    emitted ← instance.deliver(item, recorded_at)     # payloads loaded from where they live
    cpu[(tenant, namespace, day)] += instance.cpu_time()   # items that execute, never a replay's
    events += [item] + emitted
move payloads > 1 KiB out of the events: to the run's payload records, or to a blob above 64 KiB, put first
effects ← calls' tasks, timers, cancelled timers, signals and starts of runs of the namespace, from the new commands
status ← queued, running, waiting or the ending, from the run's state and its waits
owner ← Activation { events, consumed, payloads, effects, status, summary, counters }   # committed in the next batch
cache[run] ← instance
```

The activation is a `tracing` span (service telemetry), with the run's trace id, its own span id
`H(run, "activation", position of its first event)` and parent, the run span `H(run, "run")`,
recorded as fields. Nothing about the code is emitted during replay.

- Code checks at start fail → `StartError::InvalidCode`, and no run is created. A text to
  store with a NUL character → `StartError::NulCharacter` (a signal's key:
  `EngineError::NulCharacter`), before anything is stored.
- Executor start errors (top-level exception) → `run_aborted { invalid_code }`.
- The run's started activations without an end exceed 5 → the code is not run; the owner
  quarantines it (kind `failures`), the reason saying how many never finished.
- Replay mismatch → nothing is committed; the owner quarantines the run (kind `non_determinism`).
- Any other failure → nothing is committed; the owner writes the failure, which counts it; the fifth
  in a row quarantines the run, and a timer tries the run again otherwise
  ([activations](03-execution.md#activations)).

Rules the engine keeps, tested in the driver:

- **Starts.** The edge checks the code, finds or creates the namespace (`ensure_namespace`), and
  finds the run's workflow: the one the request names (by its id, or by its name in the namespace
  then the organization), else the one its code is in the namespace or the organization
  (`workflow_with_code`), else a new one in the namespace with the code as its version 1
  (`create_workflow`), within the namespace's `workflows` quota. Then it asks the namespace's owner.
  The owner answers a known start key with its run, asks the tenant actor to admit the start
  (`AdmitStart`), and commits `CreateRun` once admitted. A start refused for a quota names the
  quota, the scope, the limit, the use and, for a monthly quota, the start of the next month. A
  retried request with its key returns its run, whatever the quotas
  ([quotas](06-storage-and-scale.md#quotas)). A workflow created for a start refused after it
  stays, and the next run of the same code finds it.
- **Statuses.** The owner computes the status of each run a batch changes, from its history and
  the waits its head holds: `queued` until a step is committed; `running` while a step is in
  progress (its inbox holds a trigger, or an executor runs it) or a call Rewind makes itself, to an
  MCP server, an API or `http`, is in flight or backing off; else `waiting`, on a signal, a timer, a
  harness action (`ready` or `claimed`), an approval, a deferred answer, a parked call or a
  quarantine, with what it waits for; and its ending once it ended. It writes the status in the
  head and the summary, and moves the gauges, in the same batch. A batch that leaves the run
  waiting only on the outside world puts `run.waiting` in the outbox; a timer alone sends nothing.
- **Workflows across runs** ([execution](03-execution.md#workflows-across-runs)). A run started by
  a run's code records the starter as its first watcher, attached unless the run is detached. The
  batch of an activation that ends a run enqueues the run's end as the signal `rewind:ended:<run>`
  for every watcher that has not ended, with that name as idempotency key, and enqueues
  `cancel_requested` for every run this one watches attached and that has not ended. A fork's
  watch is never attached; when the run it watches has already ended, the engine enqueues the end
  itself, under the same key. All of them live in the run's partition: it is one batch.
- **Calls** follow the [lifecycle](04-tools.md#call-lifecycle). A call that becomes
  ready is sent once its batch is durable, within the concurrency limits: the policy's calls of
  the same action in flight per run, held by the owner, and the server's or API's calls in flight in
  the organization, held by the tenant actor. A harness action's call goes to the tenant actor,
  which hands it to a worker's call stream with a free slot, with a new token. The actor holds the
  streams in memory: when the owner of its home partition opens it, after a takeover or again on
  the same node, it tells every node, and each registers its streams of the tenants homed there
  again, and asks again for the slots its owners' calls of those tenants wait for. The outcome of a
  call whose task no longer exists is dropped. Approving a call parked by `schema_changed` marks its
  task `schema_change_approved`. A secret put again resumes the calls parked by
  `credentials_refused` or `secret_missing` in the runs that use it.
- **Timers.** Code timers, call timeouts, deferred deadlines, redelivery backoff, run deadlines,
  retention and webhooks sent again are timer records; the wheel fires them. A deadline enqueues
  `cancel_requested { reason: "ttl" }` once; for a quarantined run, the engine gives it up
  (`run_aborted { non_determinism }`, or `{ internal }` after repeated failures).
- **Ends.** An activation that ends the run clears its timers, tasks, inbox and signal URL tokens,
  and its quarantine. The sealer seals the attempt, and the retention timer erases the run.

## Tests

| Where | What |
| --- | --- |
| `rewind-core` | Unit tests of the pure model: canonical JSON (RFC 8785 golden vectors), digests, matching, `ReplayCursor`, run state, durations |
| `rewind-format` | Round trips and golden fixtures (a stored v1 document must always decode); the old names `rename-data` reads, each decoded into its new form |
| `rewind-engine` | Use cases and actors in the driver, with in-memory adapters and a fake executor: idempotency keys, epochs and remote executors, quarantine, failure counts and unfinished activations, run expiry, the call lifecycle and concurrency, statuses and what a run waits for, admission and quotas, fairness, workflows across runs, workflows found by code and by name, sealing and retention, recovery after a node dies, a partition moved, an owner fenced while cut off; the conformance suites of `CatalogStore`, `PartitionStore`, `RunArchive`, `Cluster`, `AccessStore`, `BlobStore`, `SecretStore` and `SpanExporter`, passed by the in-memory adapters; span derivation; projector idempotence (an event applied twice changes nothing) |
| `rewind-runtime` | Sandbox behavior, driven by the engine with in-memory adapters and virtual time (`Promise.all`, time, randomness, signals, imports, the old names of code already written, callsites, cancellation, limits, the exact set of globals, RFC 8785 numbers against V8, and how a run ends: forgotten `await`, unhandled rejection, deadlock) |
| Epoch corpus (`rewind-e2e`) | `corpus/epoch-N/{blue,red}/<case>/`: a workflow's code, a `scenario.json` (input, actions, steps, expectations) and its recorded `history.v1.json`. Each case runs live and must meet its expectations, must record the committed history byte for byte, and the committed history must replay with the code on a fresh executor. Blue cases must succeed; red cases must fail exactly as specified (refused start, failed, cancelled or aborted run, diverging replay, tampered history). `REWIND_E2E_RECORD=1` records the histories again |
| `rewind-store-turso`, `rewind-store-postgres` | The conformance suites of `CatalogStore`, `Cluster`, `AccessStore` and `SecretStore` (Postgres: two processes' locks and notifications) |
| `rewind-store-slatedb` | The conformance suite of `PartitionStore`, on memory, on local files, and on MinIO in Docker |
| `rewind-workos` | Against a fake WorkOS over HTTP (wiremock): every call, every error mapped, access tokens signed with a test key; against WorkOS Staging when `WORKOS_API_KEY` and `WORKOS_CLIENT_ID` are set |
| `rewind-blob` | The conformance suites of `BlobStore` and `RunArchive`, on files and on MinIO |
| `rewind-otel` | The conformance suite of `SpanExporter`, on the SDK's in-memory exporters and on an OpenTelemetry Collector in Docker over gRPC and HTTP; service spans and logs joined to a run's trace by their fields; service and run metrics, the engine driven under the subscriber |
| `rewind-cli` | End to end through the embedded server: V8 + engine + SlateDB on files + Turso, across processes (sleep, interrupt, resume) |
| `rewind-server` | Several nodes over Postgres and MinIO: a node killed during an activation, a node joining and leaving, the node link, the call stream, the change stream; `rename-data` on data stored under the old names |
| `rewind-architecture-tests` | Dependency rules, telemetry rules included; no timer outside the wheel, the backoff helpers and the clocks that read no store; `unsafe` code only in `rewind-runtime`'s `memory` module |
| `evals` | Every task solved by its reference solution, through the agent tools and the scripted model; the fake servers, the agent tools' texts, the metrics; the model clients against local fakes of the Anthropic and OpenAI APIs |

## HTTP API `/v1`

`rewind-server` serves the API. The CLI, the SDK and its agent tools and the dashboard are its
clients. The evals use its DTOs, with the engine in process
([08](08-decisions.md#taken-while-building-the-evals-november-2026)).

Conventions:

- Authentication by a Bearer token: an API key, a WorkOS access token for the dashboard, an
  end user's token (opaque `rewind_nt_…`, or a JWT the customer signed with a registered key), or
  a person's connection (`rewind_ct_…`, [connections](06-storage-and-scale.md#connections)). It
  designates the organization, or for a connection the header `Rewind-Organization` does; the caller's grant, or a token's own permissions, say what it may do,
  and every endpoint requires one permission of the catalog
  ([access](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)). A missing permission
  is 403, with a message that names it.
- `Rewind-Namespace` names the namespace of a key's or a member's request: required to start a
  run, `/v1/check`, `/v1/test` and to fork a workflow; on `POST /v1/workflows`, the namespace the
  workflow is created in, the organization's without it; on reads, it narrows to that namespace,
  and names resolve in it first. A token's namespace is its own: the header may repeat it, and
  naming another is 403 ([namespaces](06-storage-and-scale.md#namespaces)).
- `Idempotency-Key` on run creation (start and fork) and on signals
  ([idempotency](06-storage-and-scale.md#idempotency-of-the-api)).
- Errors are `application/problem+json`, with a message written for a model. Their `type` is
  `urn:rewind:problem:<slug>`; the slugs are listed in `api/README.md`.
- JSON fields are camelCase, enumerated values snake_case, times RFC 3339, durations `"30s"`
  or a number of milliseconds.
- A run is named by its UUID or by its handle, `<namespace>/<origin>/<suffix>`, percent-encoded
  in a path, wherever the API takes one ([handles](06-storage-and-scale.md#run-handles)). Its
  origin is its workflow's name, else the workflow's short id (`wf-7k2m9q`); a handle stored
  before the rename release keeps the origin `script`. A run answers with both: `id`, its UUID,
  and `handle`; with `workflow`, its workflow's id and name, `version`, its version's number, and
  `codeDigest`, its code's digest; the runs it names (`parent` for a child run, `source` for a
  fork, `startedBy`, a secret's `lastRead`) with their handles too. A run's error, and the error
  of `run_failed`, is an `ErrorInfo`: its name, its message and its stack.
- A workflow is named by its id (`wf_` and 12 characters) or by its name, wherever the API takes
  one: a name resolves in the caller's namespace first, then among the organization's workflows
  ([workflows](01-product.md#workflows)). A version has `version`, its number among the versions
  of its workflow (`triage v12`), and `digest`, the digest of its document; the API takes
  `<workflow>@v<version>` and `<workflow>@<digest>` wherever it takes a version, and the latest
  version where a workflow is named alone.
- The OpenAPI document is generated from the DTOs of `rewind-api` (utoipa) and committed at
  `api/openapi.json`; a test fails when it is stale. The SDK's and the dashboard's types are
  generated from it (`api/README.md`).

| Group | Endpoint | Permission | Does |
| --- | --- | --- | --- |
| Action sets | `POST /v1/action-sets` | `actions:write` | Registers and pins an action set: its MCP servers, its APIs from their OpenAPI documents ([API actions](04-tools.md#api-actions)), its harness actions, and the organization's workflows it names; `secrets` names the organization's secrets with which Rewind lists the tools of an MCP server that needs one (`secrets:use`); `base` derives it from a pinned action set; returns its declarations and the API operations it left out. 422 `invalid-action-set` |
|  | `GET /v1/action-sets` | `actions:read` | The organization's action sets, with their servers and action names |
|  | `GET /v1/action-sets/{id}/declarations` | `actions:read` | The declarations, `format=dts` or `prompt` |
|  | `GET /v1/action-sets/{id}/search` | `actions:read` | `search_actions`, for large action sets |
| Workflows | `POST /v1/workflows` | `workflows:write` | Creates a [workflow](01-product.md#workflows) in the namespace `Rewind-Namespace` names, else the organization's: `code`, `actionSet`, and optionally `name`, `description`, `input`, `output`, `signals`, `secrets` and `tests`. Checks its code against its action set and its interface, and the secrets its runs use against the caller's rights ([secrets](04-tools.md#secrets)), runs its tests through the [gate](05-testing-and-observability.md#suites-and-the-gate), and stores it with its version 1; answers 201 with its id and its declarations. Without a name, a code its scope already has answers that code's workflow, unchanged (200). 409 `name-taken` for a name its scope already has; 422 `invalid-workflow` for an invalid name, signal name or schema; 422 `tests-failed` lists the tests that did not pass |
|  | `GET /v1/workflows` | `runs:read` | The workflows the caller sees, paged: its namespace's and the organization's (`scope=namespace` or `organization`), those one of whose versions has a code (`code=<digest>`), a query `q`; each with its latest version, the counts of its runs and the actions they call |
|  | `GET /v1/workflows/{id}` | `runs:read` | One workflow, by its id or its name: its name and description, its scope (its namespace, or the organization), its `source` for a fork (the organization's workflow and version), its versions newest first (each with its `version`, `digest`, code digest, interface, action set, `base` and the suite it passed), its suite with the reasons of its replacements and retirements, and the code of its latest version, or of the version `@v12` or `@<digest>` names |
|  | `PATCH /v1/workflows/{id}` | `workflows:write` | A JSON merge patch: `name`, `description`; `null` removes one. The same id, no new version; sends the workflow's change. 409 `name-taken`; 422 `invalid-workflow` for an invalid name |
|  | `POST /v1/workflows/{id}/versions` | `workflows:write` | Makes its next version: `code` (by default its latest version's), `actionSet`, `input`, `output`, `signals`, `secrets`, and `tests`, `replace` and `retire` with their reasons; through the gate. The same code with a new interface or action set is a new version too. 422 `tests-failed` |
|  | `POST /v1/workflows/{id}/tests` | `workflows:write` | Adds tests to its suite, or replaces or retires some, with their reasons: each runs against its latest version first. No new version. 422 `tests-failed` |
|  | `POST /v1/workflows/{id}/promote` | `workflows:write` | Moves a namespace's workflow to the organization, with the same id, versions and suite. 409 `name-taken` when the organization has a workflow of its name |
|  | `POST /v1/workflows/{id}/fork` | `workflows:write` | Forks an organization's workflow into the namespace `Rewind-Namespace` names: a workflow of that namespace with the same name, its `source` the source's latest version or the one named (`@v12`), its version 1 that version (code, interface, action set, secrets), its suite a copy of the source's. 409 `not-forkable` for a namespace's workflow; 409 `name-taken` when the namespace has a workflow of that name |
|  | `POST /v1/workflows/{id}/pull` | `workflows:write` | A fork takes its source's latest version (code, interface, action set, secrets) as its next version once its own suite passes, and its `source` moves to it. Nothing to pull answers the fork unchanged. 422 `tests-failed`; 409 `not-a-fork` for a workflow without a source |
|  | `POST /v1/workflows/{id}/record` | `runs:read` | Records tests from the workflow's past runs, one per path ([recorded tests](05-testing-and-observability.md#tests-recorded-from-runs)); stores them, adds them to no suite |
|  | `GET /v1/tests/{digest}` | `runs:read` | A test document; a recorded one only to a caller who reads the runs of the namespace it was recorded in |
| Checks and tests | `POST /v1/check` | `runs:start` | Type-checks `code` against an action set (`actionSet`): the diagnostics, and, when its scope already has the code, that workflow's id and version. Creates nothing |
|  | `POST /v1/test` | `runs:start` | Runs tests as test runs, in virtual time: the given ones against `code`, and a workflow's suite against its version or against `code` ([testing](05-testing-and-observability.md#running-tests)); answers a `TestReport`. Creates nothing and changes no workflow |
| Runs | `POST /v1/runs` | `runs:start` | Starts a run: `Idempotency-Key`, `wait`, `labels`, `traceparent`, `ttl`; either `code`, with an `actionSet` and the `secrets` it uses, which runs as the version of the workflow that already has that code in the namespace or the organization, else as version 1 of a new workflow of the namespace ([workflows](01-product.md#workflows)); or `workflow` (an id or a name, with `@v12` or `@<digest>`, else its latest version), which brings its version's code, action set and secrets. Both is 422. 422 `invalid-code` for code refused at start |
|  | `GET /v1/runs` | `runs:read` | Lists runs: the [query language](05-testing-and-observability.md#query-language) `q`, filters by label, status (`queued`, `running`, `waiting`, an ending, or `active` for the first three), what a run waits for (`signal`, `timer`, `harness`, `approval`, `deferred`, `parked`, `quarantined`), workflow, action, error; `sort` by start (newest or oldest first), last activity, next wake, duration (longest first) or number of calls |
|  | `GET /v1/runs/stats` | `runs:read` | From the hourly counters of a namespace or the organization ([statistics](05-testing-and-observability.md#statistics)): counts by status, a series over a time window by `interval`, durations, runs waiting by kind, facets on status, workflow and namespace |
|  | `GET /v1/runs/failures` | `runs:read` | The failed, cancelled and aborted runs of a namespace or the organization over a window, by `interval`, grouped by fingerprint ([statistics](05-testing-and-observability.md#statistics)) |
|  | `GET /v1/runs/{id}` | `runs:read` | The run's state: its status, and `waitingFor` while it waits (items of kind `signal`, `timer`, `call` with its `state` and `delivery`, or `quarantined`), its workflow and version, its `deliveries`, its error, and the names of the secrets it uses |
|  | `GET /v1/runs/{id}/transcript` | `runs:read` | The run's [transcript](05-testing-and-observability.md#transcript-and-metrics) |
|  | `GET /v1/runs/{id}/events/{idx}` | `runs:read` | One event of its history in full, by its position |
|  | `GET /v1/runs/{id}/events` | `runs:read` | SSE of its history's events, `follow=1` |
| Changes | `GET /v1/changes` | `runs:read` | SSE of the changes in the organization, as the caller may see them (an end user's namespace, a grant's labels, the namespace `Rewind-Namespace` names): a `run` change holds the run's summary, the same object as an item of `GET /v1/runs`, pushed by the run's owner once its batch is durable; `workflow` (a workflow created, named, described, given a version or a test, promoted, forked or pulled), `secret`, `namespace` and `connection` changes name what changed in the control plane; `resync` says more changed at once than one pass reads, and the client reads again what it shows. No resume cursor: after a drop, the client reads again what it shows. Closed as a run's event stream is: at a member's access token's expiry (one already expired is refused with 401), once an end user's token or a connection stops authenticating, once the caller's grant no longer gives `runs:read` with the same labels or its key is revoked |
|  | `POST /v1/runs/{id}/signals/{name}` | `runs:control` | `Idempotency-Key`; 409 if the run has ended; 422 for a name starting with `rewind:`, and, for a run of a version that declares its signals, a signal it does not declare or a payload that does not match |
|  | `POST /v1/runs/{id}/cancel` | `runs:control` | Cancels the run |
|  | `POST /v1/runs/{id}/fork` | `runs:start` | `retry_run`: corrected `code`, an optional `at`, `input` and `secrets` ([forks](03-execution.md#forks)); for a run of a workflow with a suite, the suite of its version runs against the code first (422 `tests-failed`). The fork's `source` is the run; it is a run of the workflow that has its code in its namespace, or of a new one, and its handle keeps its source's origin |
|  | `GET /v1/runs/{id}/export` | `runs:read` | Exports the run: `?as=run` (the default) the run export, `?as=cassette` a cassette |
|  | `POST /v1/runs/{id}/release` | `runs:quarantine` | Takes the run out of quarantine |
|  | `POST /v1/runs/{id}/give-up` | `runs:quarantine` | Gives up a quarantined run |
|  | `DELETE /v1/runs/{id}` | `runs:delete` | Erases the run, payloads included |
| Calls | `GET /v1/calls/stream` | `calls:work` | SSE for a [worker](01-product.md#who-uses-it): `call` messages hand it the calls of harness actions as they become ready, filtered by action set and actions, and to the runs the caller sees (a token's namespace, a grant's labels); a `gone` message says a call handed on it no longer waits for its answer (its run ended, or the call timed out), and frees its slot; `slots` bounds the calls in flight on it. The open stream is the liveness of the calls handed on it: a call handed on a stream that closed waits `reconnectTimeout` for its worker to come back ([lifecycle](04-tools.md#call-lifecycle)) |
|  | `POST /v1/calls/{token}/defer` | `calls:work` | The handler answers later: the call is `deferred` until `complete` or `fail`, whether the stream stays open or not |
|  | `POST /v1/calls/{token}/complete` | `calls:work` | The result; also later, asynchronously. 410 `call-gone` when the call no longer waits for it |
|  | `POST /v1/calls/{token}/fail` | `calls:work` | The failure, `retryable` or not |
|  | `POST /v1/calls/{id}/approve` | `calls:approve` | A call waiting for approval, or parked: the call goes on. 409 `call-not-waiting` otherwise |
|  | `POST /v1/calls/{id}/reject` | `calls:approve` | The call fails with an `ActionError` |
| Signal URLs and webhooks | `POST /hooks/{token}` | none | A [signal URL](01-product.md#signals), without authentication, at an unguessable URL: what is posted to it becomes its signal |
|  | `POST /v1/subscriptions` | `subscriptions:write` | Subscribes a URL to signed webhooks, of the `types` it lists |
|  | `GET /v1/subscriptions` | `subscriptions:read` | Lists the subscriptions, without their secrets |
|  | `DELETE /v1/subscriptions/{id}` | `subscriptions:write` | Deletes a subscription |
| Reading (dashboard) | `GET /v1/actions/stats` | `runs:read` | Per action, by its `group` and its `action`, from the hourly counters of a namespace or the organization over a window: calls, latency, errors, redeliveries |
|  | `GET /v1/payloads/{digest}` | `runs:read` | A payload, on demand, through the run that holds it (`run`, required: a payload belongs to its run) |
| Organization | `GET /v1/whoami` | any caller | Who calls: the organization (`organizationId`), the principal (a key's id, the member with their email, name and picture, or a token: its kind, `opaque` or `signed`, its id or key and `jti`, its expiry), the grant (role, allowed labels) or the token's namespace and labels, the permissions it holds; for a member, the organization and every organization of the person; for a connection, its id, person and client, and the organizations it opens with their access |
| | `POST /v1/organizations` | a member, even without organization | Creates an organization: in WorkOS with the caller's membership, then its tenant and the caller's `owner` grant; if the second step fails, the organization is deleted in WorkOS |
| | `PATCH /v1/organization` | `organization:write` | The organization's name and default role |
| | `GET /v1/invitations` | a member, even without organization | The pending invitations sent to the caller's email |
| | `POST /v1/invitations/{id}/accept` | a member, even without organization | Joins the invitation's organization; the invitation's pending grant becomes the caller's |
| | `GET /v1/organization/members` | `members:read` | The organization's memberships in WorkOS, each with its grant |
| | `PUT /v1/organization/members/{member}/grant` | `members:manage` | Sets the member's role and allowed labels |
| | `DELETE /v1/organization/members/{member}` | `members:manage` | Deletes the member's grant, then their membership in WorkOS |
| | `GET /v1/organization/invitations` | `members:read` | The organization's invitations, each with its pending grant |
| | `POST /v1/organization/invitations` | `members:invite` | Invites an email with a role and allowed labels: the invitation in WorkOS, the pending grant in Rewind |
| | `POST /v1/organization/invitations/{id}/resend` | `members:invite` | Sends the invitation again |
| | `DELETE /v1/organization/invitations/{id}` | `members:invite` | Revokes the invitation and deletes its pending grant |
| Connections | `GET /v1/connections` | a member, even without organization | The caller's [connections](06-storage-and-scale.md#connections): the client, the organizations each opens with its access and labels, when it was made and last used |
| | `PATCH /v1/connections/{id}` | its person | Sets the organizations it opens, their access and labels: each an organization where the person holds a grant giving that access |
| | `DELETE /v1/connections/{id}` | its person | Revokes it: its tokens and its grants |
| | `GET /v1/organization/connections` | `members:read` | The connections that open the organization: the person, the client, the access, the labels, the last use |
| | `DELETE /v1/organization/connections/{id}` | `members:manage` | Removes the organization from the connection |
| | `GET /v1/oauth/consent?request=…` | a member, even without organization | What an authorization request asks ([MCP and OAuth](#mcp-and-oauth)): the client, where it returns, the person's organizations and the accesses they may give |
| | `POST /v1/oauth/consent` | a member, even without organization | Allows it (organizations, accesses, labels) or denies it; answers where to send the browser |
| Access | `GET /v1/permissions` | any caller | The catalog: every permission with its description |
| | `GET /v1/roles` | `roles:read` | The built-in roles and the organization's own, with their permissions |
| | `PUT /v1/roles/{name}` | `roles:write` | Creates or changes a role of the organization |
| | `DELETE /v1/roles/{name}` | `roles:write` | Deletes a role no grant points to (409 otherwise) |
| | `GET /v1/api-keys` | `api_keys:read` | The organization's keys, without their secret, each with its grant |
| | `POST /v1/api-keys` | `api_keys:write` | Creates a key with a name, a role and allowed labels; an operator may name another organization (`organizationId`). The secret is only in this answer |
| | `DELETE /v1/api-keys/{id}` | `api_keys:write` | Revokes a key |
| Namespaces | `GET /v1/namespaces` | `namespaces:read` | The namespaces, paged: `prefix` (of the name, or of the display name case aside), `above` (a share of a limit, `0.8`), `order` (`cpu`, `runs`, `workflows`, `name`); each with its display name, its limits, its exceptions and its use this month; how many match across the pages (`total`) |
|  | `GET /v1/namespaces/{name}` | `namespaces:read` | One namespace: its display name, its limits and exceptions, its use per day over a `month`, its alerts, its tokens in use |
|  | `PUT /v1/namespaces/{name}` | `namespaces:write` | Creates the namespace, or sets its exceptions; `null` gives a quota back to the default |
|  | `PATCH /v1/namespaces/{name}` | `namespaces:write` | A JSON merge patch of an existing namespace (404 otherwise): `displayName` sets its display name, `null` or `""` removes it. Answers as `PUT` |
|  | `DELETE /v1/namespaces/{name}` | `namespaces:write` | Erases the end user in the background (202): tokens, runs, workflows, secrets, usage |
|  | `GET /v1/organization/namespace-defaults` | `namespaces:read` | The default limits of every namespace, and the organization's ceiling |
|  | `PUT /v1/organization/namespace-defaults` | `namespaces:write` | Sets the default limits |
|  | `GET /v1/organization/namespace-names` | `namespaces:read` | Every namespace that has a display name, with it, by name; not paged: what the dashboard loads once to show display names |
|  | `GET /v1/organization/quota-alerts` | `namespaces:read` | The alerts of a `month`, of one `namespace` or of all |
| Tokens | `POST /v1/namespaces/{name}/tokens` | `tokens:write` | Mints an end user's opaque token: `ttl`, `permissions`, `labels`; creates the namespace if needed. The secret is only in this answer |
|  | `DELETE /v1/namespaces/{name}/tokens` | `tokens:write` | Revokes the namespace's opaque tokens and the signed ones issued before now |
|  | `DELETE /v1/tokens/{id}` | `tokens:write` | Revokes one opaque token |
|  | `GET /v1/organization/token-keys` | `tokens:write` | The keys the organization signs its tokens with |
|  | `POST /v1/organization/token-keys` | `tokens:write` | Registers a public key (a JWK: Ed25519, P-256 or RSA) with a name and the permissions its tokens may carry; answers its `kid` |
|  | `DELETE /v1/organization/token-keys/{kid}` | `tokens:write` | Deletes a key: every token it signed is refused |
| Secrets | `GET /v1/secrets` | `secrets:read` | The organization's [secrets](04-tools.md#secrets) the caller sees (its labels): each one's name, labels, who wrote it and when, reads per day over 30 days and last read; never a value |
|  | `PUT /v1/secrets/{name}` | `secrets:write` | Creates or replaces an organization's secret: `value`, `labels`; makes ready the calls parked by a refusal or a missing secret in the runs that use it. Refused to an end user's token (403) |
|  | `DELETE /v1/secrets/{name}` | `secrets:write` | Deletes it: the next call of a run that uses it is parked |
|  | `GET /v1/secrets/{name}/usage` | `secrets:read` | What reads it: the active runs that use it, those of them waiting on a call it serves, the workflows whose latest version uses it, its reads per day, its last read |
|  | `GET /v1/namespaces/{name}/secrets` | `secrets:read` | The end user's secrets, as above |
|  | `PUT /v1/namespaces/{name}/secrets/{secret}` | `secrets:write` | Creates or replaces an end user's secret, as above; a token, in its own namespace. Refused to a grant or a token limited to labels (403) |
|  | `DELETE /v1/namespaces/{name}/secrets/{secret}` | `secrets:write` | Deletes it, as above |
|  | `GET /v1/namespaces/{name}/secrets/{secret}/usage` | `secrets:read` | What reads it, as above |
|  | `GET /v1/organization/signing-keys` | `secrets:read` | The organization's [signing keys](04-tools.md#signed-calls): `kid`, made, retired; and the URL of their public keys |
|  | `POST /v1/organization/signing-keys` | `organization:write` | Replaces the signing key: the old one stops signing and stays published 24 hours |
|  | `GET /organizations/{organization}/jwks.json` | none | The public keys, outside `/v1`: the current one and those retired less than 24 hours ago; `Cache-Control: max-age=300` |
| Operator | `PUT /v1/organizations/{organization}/quotas` | the operator | Sets an organization's ceiling |
| Server | `GET /v1/server` | any caller | The server's version and the determinism epochs its build supports, and how many partitions and nodes the deployment has |
| | `POST /v1/server/stop` | the operator | Stops the embedded server, when the CLI replaces it with one of its own version |

### Sign-in (`/auth`)

The dashboard's sign-in, served by every node's edge when WorkOS is configured. These routes are for a
browser only: they read and write the cookie `rewind_session`, and `/v1` never reads it. They are
described in the OpenAPI document like `/v1`.

| Endpoint | Does |
| --- | --- |
| `GET /auth/config` | The sign-in methods (`github`, `magic_auth`) and whether sign-ups go through the waitlist; 404 without WorkOS, and the dashboard asks for an API key |
| `GET /auth/github` | Redirects to WorkOS for GitHub, with a random `state` and the `invitation_token` of the page, both kept in the cookie `rewind_oauth` (5 minutes) |
| `GET /auth/callback` | Checks `state`, exchanges the code, keeps the outcome in `rewind_session`, redirects to the dashboard (`/`); when the sign-in did not go through, to `/?auth_error=<code>` (`state_mismatch`, `provider_error`, `sign_in_failed`), never with a token or WorkOS's message |
| `POST /auth/magic-auth` | Sends the 6-digit code to an email. While sign-ups are closed, an email without an account gets none: the answer is `sign_up_closed`, and the person may join the waitlist |
| `POST /auth/magic-auth/verify` | Checks the code |
| `POST /auth/email-verification` | The code WorkOS sends when it requires the email to be verified |
| `POST /auth/organization` | The organization chosen when WorkOS asks for one |
| `POST /auth/session` | The current state: a valid access token, refreshed when less than a minute is left; with `organizationId`, switches organization. Writes a member's first grant in an organization |
| `POST /auth/waitlist` | Puts an email on the waitlist; the same answer whether the email has an account or not. The landing's form posts here too, through its own origin ([08](08-decisions.md#taken-while-deploying-rewind-on-railway-and-vercel-november-2026)) |
| `POST /auth/logout` | Revokes the session in WorkOS and clears the cookie |

- `POST /auth/magic-auth` answers 202 when it sent a code, `POST /auth/waitlist` 202 and
  `POST /auth/logout` 204, with no body. Every other answer but the redirects is an `AuthState`: `authenticated` (the access token, its
  expiry, the person, the organization or none), `organization_selection_required` (the
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
| `WORKOS_API_KEY` | Its secret key, `sk_…`. Given to every node, never to a node started with `--executors-only` |
| `WORKOS_COOKIE_PASSWORD` | 32 random bytes in hexadecimal (64 characters). Changing it signs everyone out |
| `WORKOS_REDIRECT_URI` | `https://app.rewind.codes/auth/callback`: registered in WorkOS, on the dashboard's origin |

The four are given together or not at all (exit code 64 otherwise). Without them the server has
no `/auth` and accepts only API keys; keys still get their rights from grants and roles. A node
started with `--executors-only` serves no HTTP route but the node link.

## MCP and OAuth

Every node's edge serves Rewind's MCP server, for Claude and any MCP client
([01](01-product.md#claude-as-the-harness)), and the OAuth authorization server its people
connect through ([connections](06-storage-and-scale.md#connections)). OAuth needs the public URL
(`--public-url`, the issuer and the resource's base) and WorkOS (the consent needs a person
signed in). Without them, `/mcp` takes API keys and end users' tokens only, and serves no
discovery.

| Endpoint | Caller | Does |
| --- | --- | --- |
| `POST /mcp` | A connection's access token, an API key or an end user's token | MCP over Streamable HTTP, without session |
| `GET /.well-known/oauth-protected-resource/mcp` | none | The resource's metadata (RFC 9728); the same at `/.well-known/oauth-protected-resource` |
| `GET /.well-known/oauth-authorization-server` | none | The authorization server's metadata (RFC 8414) |
| `GET /oauth/authorize` | a browser | Checks the request, then sends the browser to the consent page |
| `POST /oauth/register` | none | Registers a client (RFC 7591) |
| `POST /oauth/token` | a client | Exchanges a code (with PKCE) or a refresh token for tokens |
| `POST /oauth/revoke` | a client | Revokes the connection of a token (RFC 7009) |

Discovery:

- **The resource**: `resource` is `{public-url}/mcp`, `authorization_servers` `[{public-url}]`
  alone (Claude uses the first), `bearer_methods_supported` `["header"]`, `scopes_supported`
  `["offline_access"]`, `resource_name` `Rewind`.
- **The authorization server**: `issuer` `{public-url}` and the endpoints above,
  `response_types_supported` `["code"]`, `grant_types_supported`
  `["authorization_code", "refresh_token"]`, `code_challenge_methods_supported` `["S256"]`,
  `token_endpoint_auth_methods_supported` `["none", "client_secret_post", "client_secret_basic"]`,
  `client_id_metadata_document_supported` `true` (with `none`, Claude then prefers its metadata
  document to registering), `scopes_supported` `["offline_access"]`. Rights come from the
  consent, not from scopes: an unknown scope is ignored.

The flow:

1. A request to `/mcp` without a valid Bearer is 401, with
   `WWW-Authenticate: Bearer resource_metadata="{public-url}/.well-known/oauth-protected-resource/mcp"`,
   and `error="invalid_token"` when a token was sent.
2. **Clients.** A `client_id` that is an `https` URL with a path is a client's metadata document
   (CIMD): the edge fetches it with the egress rules of MCP servers
   ([04](04-tools.md#where-mcp-servers-and-apis-may-be)), no redirect, at most 5 KiB within 5 s, and
   keeps it as its HTTP caching says, 24 hours at most. It names itself as `client_id`, lists
   its `redirect_uris`, and authenticates with `none`. Other clients register
   (`POST /oauth/register`) and get a `client_id` `rc_…`, and a secret only for
   `client_secret_post` or `client_secret_basic`, stored hashed and shown once. A redirect URI is
   `https`, `http` on a loopback address, or a private-use scheme holding a dot
   (`cursor://…`); never `javascript:`, `data:` nor `file:`. A registered client that no
   connection uses is deleted after 24 hours; past 10 000 of them, registering answers 503.
3. **`GET /oauth/authorize`** takes `response_type=code`, `client_id`, `redirect_uri`,
   `code_challenge` with `code_challenge_method=S256`, `state`, and optionally `scope` and
   `resource`. An unknown client, or a redirect URI it does not list, is answered there (400) and
   never redirected; a redirect URI matches exactly, but a loopback one, compared without its
   port (Claude Code's). Any other fault goes back to the client with `error` and `state`
   (`invalid_request`, `unsupported_response_type`, `invalid_target` for a `resource` other
   than `{public-url}/mcp`). A valid request is sealed, as `/auth`'s cookies are, for 10
   minutes, and the browser sent to `{dashboard}/connect/authorize?request=…`, the dashboard
   being the origin of `WORKOS_REDIRECT_URI`.
4. **The consent.** The page reads the request (`GET /v1/oauth/consent`): the client's name and
   site, the host it returns to, with a warning for a loopback one, and the person's
   organizations, each with their role and the accesses they may give. `POST /v1/oauth/consent`
   allows it (organizations, accesses, labels) or denies it, and answers the URL to send the
   browser to: the redirect URI with `code` and `state`, or with `error=access_denied`.
5. **`POST /oauth/token`** (`application/x-www-form-urlencoded`). `grant_type=authorization_code`
   takes the code (5 minutes, stored hashed with the request and the choices), the same
   `redirect_uri`, the `client_id` (with its secret, for a confidential client) and the
   `code_verifier`; it creates the connection, its grants and its tokens in one transaction.
   `grant_type=refresh_token` replaces both tokens. The answer: `access_token`, `token_type`
   `Bearer`, `expires_in` 3600, `refresh_token`, `scope`. A code or refresh token that is unknown,
   expired, used or another client's is 400 `invalid_grant`; one used twice also revokes its
   connection.
6. Claude gives discovery, registration and tokens 10 s each, and a refresh 30 s.

`/mcp`:

- **Without session.** Each `POST /mcp` carries a whole exchange and is answered in JSON
  (`application/json`), without `Mcp-Session-Id`; `GET` and `DELETE /mcp` are 405.
- **Its Bearers.** A connection's access token, an API key, or an end user's token; a WorkOS
  access token is 401. A key and a token fix their organization, and a token its namespace:
  `organization` and `namespace` may repeat them, not name others.
- **`initialize`** answers `instructions`: from an organization to a run, through
  `list_organizations`, `list_namespaces`, `list_action_sets`, `search_actions`, then checking,
  testing, and running a workflow with the secrets `list_secrets` names for the servers that need
  one.
- **`tools/list`** lists the agent tools that some organization of the caller allows, each with a
  `title`, its input schema and its annotations. **`tools/call`** runs the agent tool as the
  SDK's handler does, with the same plain-text results; a mistake the model can fix is a result
  with `isError`. `wait` is at most 200 s, 30 s by default (Claude gives a call 240 s), and a
  result at most 80 000 characters (Claude Code takes 25 000 tokens by default), the rest paged
  with `offset` as in the SDK.
- **How an agent tool runs.** The tool calls `/v1` on this server over the loopback address, with
  the caller's Bearer, `Rewind-Organization` and `Rewind-Namespace`: one authorization, the code
  and the limits every client goes through. What runs model-written code (`POST /v1/test`, a
  workflow's gate, a fork's suite) runs on the node that answers, under its sandbox.
- **No `Host` check.** Every request carries a Bearer and none reads a cookie, so a page that
  rebinds a name to the server gains nothing.

| Tool | Permission | Arguments beyond the SDK's | Annotations |
| --- | --- | --- | --- |
| `list_organizations` | none | none | read-only |
| `list_namespaces` | `namespaces:read` | `organization`, `prefix` | read-only |
| `list_action_sets` | `actions:read` | `organization` | read-only |
| `search_actions` | `actions:read` | `organization`, `action_set` | read-only |
| `list_secrets` | `secrets:read` | `organization`, `namespace` (optional) | read-only |
| `get_workflow` | `runs:read` | `organization`, `namespace` (optional) | read-only |
| `check_workflow`, `test_workflow` | `runs:start` | `organization`, `namespace`, `action_set` | read-only |
| `get_run`, `list_runs` | `runs:read` | `organization`, `namespace` (optional) | read-only |
| `record_tests` | `runs:read` | `organization`, `namespace` (optional) | |
| `run_workflow` | `runs:start` | `organization`, `namespace`, `action_set` | open world |
| `retry_run` | `runs:start` | `organization` | open world |
| `signal_run` | `runs:control` | `organization` | |
| `cancel_run` | `runs:control` | `organization` | destructive |

`organization` is an organization's id or name, as `list_organizations` gives them; it may be
left out when the caller reaches one organization only, and leaving it out with several answers
their list. `action_set` is an action set's id, as `list_action_sets` gives it; `run_workflow`
needs it with `code`, not with `workflow`. A `namespace` given to `get_workflow` and
`record_tests` is where a workflow's name resolves first, before the organization's workflows. A
new namespace's name creates it at the first run, as on `/v1`. `update_workflow` is not served
over MCP ([agent tools](01-product.md#agent-tools)).

## Embedded server and CLI

The CLI never opens what `.rewind/` stores. It is a client of the API `/v1`: of a remote server, or
of the embedded server of a `.rewind/` directory. Only one process opens the directory, so Turso
stays in its stable single-process mode, SlateDB has one writer, and the instance cache lives in
that server.

`rewind server --embedded` is the engine for a `.rewind/` directory: one node, which owns the
only partition, with its edge on a unix socket. It runs the same code as a production node, over
SlateDB on local files and Turso, with a cluster of one member and the node link inside the process.

| When | What happens |
| --- | --- |
| A command needs the server | If `.rewind/server.lock` is free, no server runs: the command starts one, detached, with `rewind server --embedded --dir <dir> --idle-timeout 10m`, and waits for it to write `ready` on the pipe it was given. |
| The server starts | It takes an exclusive lock on `.rewind/server.lock`, opens `control.db` and its partition, recovers it (the timers that came due while it was down fire at once), listens on the unix socket `.rewind/server.sock`, then writes `ready`. |
| A second server starts | It fails to take the lock and exits with code 75. |
| The server is killed (`kill -9`) | The lock is released. The next command starts a new server, which recovers the partition: the activations that were running run again. |
| The CLI's version differs from the server's | The CLI asks the server to stop and starts a new one. |
| 10 minutes pass without requests and without work in progress | The server stops. A sleeping run resumes at the next `rewind` command. |

- The socket is mode 0600 and takes no token. Its one caller is the operator of the implicit
  organization `local`.
- The CLI names a namespace with `--namespace` or `REWIND_NAMESPACE`. Against its own embedded
  server it uses `local` when none is given; against a remote server, a command that starts a
  run without one is a usage error (64).
- `rewind server` run in the foreground never stops on idle: only `--idle-timeout` makes it. The
  idle stop is one timer, pushed back by each request and each activation.
- A command that waits for a run (`--wait`) sends one request, answered when the run ends or the
  wait elapses.
- Virtual time is an option of the server (`--virtual-time`, `--real-time`), kept in the
  `meta` table.
- `rewind server` exits with 0 once stopped, 64 for a usage error, 70 when it cannot start, and 75
  when another server holds the directory's lock.
- Local-only commands run in the CLI process, without a server: `rewind check`, `rewind test`, and
  the replay of an exported run. `rewind test` runs on the server (`POST /v1/test`) when the
  tests need what the server holds: a workflow's suite (`--workflow`) or an action set registered
  there (`--action-set <id>`).

The `.rewind/` directory:

```
project/
  workflow.ts
  rewind.actions.json       the project's default rules, per action, for test runs
  .rewind/                  created when the first server starts; --dir picks another
    server.lock             held by the embedded server
    server.sock             the API /v1, mode 0600
    control.db              Turso: the control plane; meta holds the schema version and virtual time
    control.db-wal          Turso's write-ahead log
    master.key              the master key of the secret store, mode 0600, made at the first start
    partitions/0/           SlateDB: the partition's write-ahead log, tables and manifest
    runs/local/<run>/1.json the sealed attempts of the tenant local
    blobs/local/<run>/sha256/ab/cdef…   payloads above 64 KiB, named by their digest
    tests/local/sha256/ab/cdef…         test documents
    .staging/               where an object is written and synced before it is renamed
```

Commands:

| Command | Does |
| --- | --- |
| `rewind run workflow.ts [--input …] [--wait 30s] [--ttl 30d] [--secret <server>=<scope>/<name>]…` | Starts a run of the code and waits for it, up to `--wait`; `--secret` names a [secret](04-tools.md#secrets) the run uses |
| `rewind check workflow.ts` | Type-checks code against the action set's declarations ([type-checking](04-tools.md#type-checking)); creates nothing |
| `rewind test [workflow.ts] [--tests file] [--workflow <workflow>] [--action-set id\|file]` | Runs tests as test runs, in virtual time ([testing](05-testing-and-observability.md#testing)): those of `--tests` (`workflow.tests.json` next to the code by default), in the process; with `--workflow`, that workflow's suite too, on the server, against the code or its version's own. Creates nothing |
| `rewind workflows list\|show\|create\|update\|promote\|fork\|pull` | The workflows of the namespace and of the organization; one workflow, its versions and its suite; creates one from a file (`--name`, `--description`, `--action-set`, `--interface`, `--tests`); names or describes one, adds tests to its suite, or makes its next version (`--code`, `--interface`, `--tests`); promotes one to the organization; forks an organization's workflow into the namespace; pulls its source's latest version into a fork |
| `rewind workflows record <workflow> [--query …] [--run <id>…] [--limit n]` | Records tests from a workflow's past runs; prints their digests, names and paths |
| `rewind signal <run> <name> [payload]` | Sends a signal |
| `rewind cancel <run>` | Cancels a run |
| `rewind replay <run> [--code fixed.ts]` | Replays locally and checks determinism; `<run>` is a run id or an exported run |
| `rewind fork <run> --code fixed.ts [--at <position>] [--input …] [--secret …]` | Starts a [fork](03-execution.md#forks) |
| `rewind transcript <run> [--event n]` | The run's transcript, for a model; with `--event`, one event of its history in full |
| `rewind runs` | Lists runs: a query of the [query language](05-testing-and-observability.md#query-language) (`rewind runs '@agent:triage status:failed'`), filters (`--status`, `--workflow`, `--action`, `--source`, `--waiting quarantined`…), `--sort` |
| `rewind release <run> [--give-up]` | Takes a run out of quarantine, or gives it up |
| `rewind status <run> [--wait 30s]` | A run's status and what it waits for, waiting up to `--wait` for it to end |
| `rewind export <run> [--as run\|cassette] [-o file]` | The run export, or a cassette of its calls and signals |
| `rewind action-sets add\|declarations\|search` | Registers an action set file; its declarations; searches its actions |
| `rewind secrets list\|put\|delete\|usage [name] [--namespace <name>]` | The organization's secrets, or a namespace's; `put` reads the value from standard input, never from an argument |
| `rewind namespaces list\|show\|rename\|quotas\|delete` | The namespaces and their use; one namespace; sets its display name, or removes it; sets its exceptions (`--workflows`, `--max-code-lines`…), or the defaults with `--defaults`; erases one |
| `rewind server [--embedded] [--executors-only] [--pool name] [--virtual-time] [--idle-timeout 10m]` | Runs the server in the foreground (`rewind server --help` lists its options, among them `--admin-organization`, `--organization-pool` or `REWIND_ORGANIZATION_POOLS`, `--quota-workflows`, `--quota-max-code-lines`, `--allow-private-webhooks`, and `--public-url`, the base of the URLs `signalUrl(name)` returns) |
| `rewind server migrate-runs` | Once, at the deploy that moves runs to partitions: moves the runs of the old tables into the partitions and the archive ([08](08-decisions.md#taken-for-the-object-storage-rewrite-november-2026)) |
| `rewind server rename-data` | Once, at the rename release, with the node stopped and before the new build starts: rewrites the stored data in the new names (the partitions and the archive on S3, the catalog, the access store, the subscriptions), recomputes the digest of every content-addressed document (pinned action sets, workflow versions) and rewrites every reference to it, turns the titles people gave into names where they are valid and free, and gives every workflow its scope; the format stays `"v": 1`, and the prefixes `rewind_wh_` and `rewind_tk_` and the handles stored are kept. Rehearsed first on a copy of production's data ([08](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)) |
| `rewind mcp` | Serves the agent tools over MCP on stdin and stdout, against the embedded server: to debug them with an MCP client (the MCP Inspector, Claude Code on this machine). Never a remote server |

Every command takes `--json`. A first Ctrl-C lets the command finish the call in flight; a
second one exits at once.

Exit codes:

| Code | Meaning |
| --- | --- |
| 0 | The run completed, or the command succeeded |
| 1 | The run failed |
| 2 | The run was cancelled |
| 3 | The run was aborted by the engine |
| 4 | The run has not ended: `--wait` elapsed while it was queued, running or waiting, or it waits for the outside world |
| 5 | The run is waiting, quarantined |
| 6 | Refused: unknown run, or a signal to a run that has ended |
| 64 | Usage error |
| 65 | Code refused at start |
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
| Serving | At the root of its origin. The routes are paths (`/runs/…`): the server, and Vercel for the hosted dashboard, answer `index.html` for any path that is neither a file of the build nor under a root of the API (`/v1`, `/auth`, `/oauth`, `/.well-known`, `/mcp`, `/hooks`, `/assets`, `/organizations`); the server answers 404 for a missing file name at the root (`/favicon.ico`). An old link with a fragment (`/#/runs/…`) is rewritten by the app at load. `index.html` is not cached; `assets/` is immutable |
| Sign-in | With WorkOS: its own pages, GitHub or a code by email, through `/auth` on the same origin ([sign-in](#sign-in-auth)); onboarding (create an organization, accept an invitation), the organization picker, the waitlist, which someone new also opens from the first page. Without WorkOS: an API key typed by the operator, kept for the tab, or on the device when asked |
| API | `/v1` on the same origin, with the access token or the key as Bearer. The views follow the permissions `whoami` answers: reading needs `runs:read`; Members, Invitations, Roles and API keys (the organization, from the member's menu) are shown to whoever holds their permissions, Namespaces to whoever holds `namespaces:read`, Secrets to whoever holds `secrets:read`, and their writes to whoever holds `secrets:write`; a workflow's name, description, promotion, fork and pull to whoever holds `workflows:write`; a value is a field that is written and never read back |
| Claude | Connect Claude (`/connect`): the URL, the steps per client, the good practices, the connection seen arriving. The consent (`/connect/authorize`), reached after the sign-in when the person was not signed in. The person's connections (Account › Connections), and the organization's (Organization › Connections, `members:read`) |
| Namespace | None picked: the dashboard reads every namespace of the organization, and sends `Rewind-Namespace` only to fork a workflow into the namespace the member picks; a run's namespace is a term of the query (`namespace:`) |
| Views | Runs (`/runs`: All, Running, Waiting, Failures), a run's page (`/runs/<handle>`, its History at `/runs/<handle>/history`, its Controls, Export run and Export cassette, the Test run badge on a test run), Workflows (`/workflows`, `/workflows/:id`, a test at `/workflows/:id/tests/:digest`), Actions (`/actions`). Its labels say harness action, Organization id, Fingerprint and Redeliveries ([05](05-testing-and-observability.md#dashboard)) |
| Layers | `views/`, `app/` and `components/` render state and call its writes; they import only `state/`. `state/` holds the TanStack DB collections (`@tanstack/db`, pinned to an exact version), the hooks that read them and the writes, the functions that change data through the API, and decides what to load, when it is stale and what a change touches. `sources/` holds the HTTP client, the change stream and the run event streams: a new way for the server to push changes changes this folder only. Biome's `noRestrictedImports` (`biome.jsonc`) keeps `sources/` and `@tanstack/*` (but `@tanstack/react-virtual`, a component) out of the views. What the caller may do is `state/access.tsx`; browser storage, `model/storage.ts`; views may read the contexts `app/` provides (the server's answer, by the gate) |
| Streams | Read with `fetch`, not `EventSource`, which cannot send the `Authorization` header. A run's event stream is resumed with `from` after a drop. The change stream (`GET /v1/changes`) is one per tab, reopened after a drop (1 s, doubling up to 30 s) with a read of what is on screen; while it is down, the views keep what they show. No view reads on a timer, and the lint refuses `refetchInterval` |
| Resources | None from another origin: the fonts are in the build. No inline script. The one image from elsewhere is a member's picture, as WorkOS gives it |

`dashboard/README.md` has the development commands; `pnpm dev:mock` runs it against a fake API.
