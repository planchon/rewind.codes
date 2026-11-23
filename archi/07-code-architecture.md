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
   journal, determinism check (`ReplayCursor`), run state, payload identity, durations,
   script and toolset model, run and partition ids. No I/O, no async, no V8, **no serde**. It
   knows nothing about storage, transport, or where time and ids come from.

2. **Ports live in `rewind-engine`, and adapters implement them.** The engine holds the use
   cases (start, activate, signal, cancel, replay, project, fork, seal, erase) and the actors
   that run them ([inside a node](#inside-a-node)), and declares what it needs from the outside
   world as traits. It never names Postgres, SlateDB, R2, V8, MCP or OpenTelemetry.

3. **Ports follow the domain's invariants, not the keys.** "A trigger is committed atomically
   with everything it caused" is one port method, `PartitionLog::commit`, taking what changed;
   the engine never composes keys. Fencing is part of the port contract. Every adapter of a port
   passes the same conformance suite.

4. **Each boundary owns its representation.** Domain types are converted at the edge and
   never leak through.

   | Boundary | Owner | Why it is separate |
   | --- | --- | --- |
   | Stored format: events, journal documents, partition records, cassettes, pinned toolsets | `rewind-format`, versioned (`"v": 1`, [02](02-journal.md#stored-format-v1)) | Runs stay paused for days across deploys. The stored form must stay stable while domain types change. Migrations are explicit. |
   | HTTP / SDK | DTOs in `rewind-api` | Public contract with its own versioning (`/v1`). Shared by the server and the CLI. |
   | Between nodes | the node link's DTOs in `rewind-server` | Internal to a deployment: every node of a deployment runs the same build, apart from the executors kept for a previous epoch, which speak the same link. |
   | JS ↔ Rust | `rewind-runtime` | Internal to the sandbox; shaped for the glue code. |

5. **Core knows a payload's identity, not where it lives.** A payload is a JSON value
   identified by the digest of its canonical form; an event holds the value or only the
   digest. In the event, in the run's records or in a blob, key, size and preview are the
   adapters' business.

6. **Non-determinism comes in only through ports.** Real time, ids, seeds, tool results
   and signals all come from ports. Core and runtime are deterministic, and the engine is
   deterministic given its ports. Its actors are state machines that the driver steps one
   message at a time, so the engine with in-memory adapters and a virtual clock is both mock
   mode and the test harness.

7. **The runtime only executes.** `rewind-runtime` takes a script and a start, receives
   deliveries, and returns the emitted events. It knows nothing about history, replay,
   storage or dispatch. `replay()` belongs to the engine, and so does the instance cache.
   V8 objects can't change threads, so instances run on dedicated threads behind the
   `ScriptExecutor` port; the engine never handles a V8 type.

8. **Errors don't cross boundaries raw.** Each crate has its own error type. Adapters
   convert turso, sqlx, SlateDB, object store and deno errors into port errors. No `anyhow` in
   libraries.

9. **Tooling enforces the boundaries, not discipline.** A test crate reads
   `cargo metadata` and asserts the dependency rules below.

10. **The journal is the record; telemetry and dashboards are projections.** What a
    script did (OTel spans, the run index behind the dashboard, SSE) is derived from
    committed entries, never emitted from the isolate, from the runtime, or during
    replay. The platform's own behavior goes through the `tracing` facade in the engine
    and the adapters. OpenTelemetry lives only in `rewind-otel`, installed by the
    composition roots. `rewind-core` has no telemetry. Details in
    [05](05-testing-and-observability.md#observability).

11. **Nothing polls.** A component waits on one channel and wakes when a message arrives. The
    only timers are the timer wheel, the backoff of a retry after an error, and a few clocks that
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
| `CatalogStore` | The control plane's catalog: scripts, toolsets and workflow versions by digest, the index of tests and the suite of each version; tenants; namespaces, with their partition, display name, exceptions and erasure; subscriptions | memory (engine), `rewind-store-turso`, `rewind-store-postgres` |
| `PartitionStore` | One partition of the run plane, with one writer: runs and their heads, history per attempt, inbox, signal keys, tool tasks, timers, payloads, watchers, webhook tokens; the run index and summaries; start keys; counters; the outbox; the states of the organizations homed there; fencing | memory (engine), `rewind-store-slatedb` |
| `RunArchive` | The sealed journal of each ended attempt, and the erasure of a run's objects | memory (engine), `rewind-blob` (files, S3) |
| `Cluster` | The nodes alive, the partition map and its epochs, a node's liveness, and the changes of the control plane | memory (engine), `rewind-store-turso` (one node), `rewind-store-postgres` |
| `NodeLink` | Requests, subscriptions and activations sent to another node | in process (engine), HTTP/2 (`rewind-server`) |
| `AccessStore` | Who may do what: API keys, the tenants' roles, grants, each tenant's default role; end users' tokens and the keys that sign them | memory (engine), `rewind-store-turso`, `rewind-store-postgres` |
| `IdentityProvider` | Who someone is: sign-in, sessions and access tokens, users, organizations, memberships, invitations, the waitlist. Never what they may do | memory (engine), `rewind-workos` (WorkOS) |
| `BlobStore` | Payload values above 64 KiB under their run, test documents under their tenant; deletion of a run's blobs | memory (engine), `rewind-blob` (files, S3) |
| `ScriptExecutor` / `ScriptInstance` | Start a script, deliver completions, report emitted events and the outcome | `rewind-runtime` (V8); a fake in engine tests |
| `ToolDispatcher` | Execute a tool call with its idempotency key and its authentication: a secret, or the tenant's signing key | static responses and cassettes (engine, mock mode); TypeScript mocks (`rewind-runtime`, in a separate isolate); `rewind-mcp` (MCP servers, built-ins `http` and `webhook`); `rewind-openapi` (APIs) |
| `TypeChecker` | Type-check a script against the sandbox lib and a toolset's declarations | static (engine), `rewind-check` (tsgo) |
| `SpanExporter` | Export spans derived from committed entries | memory (engine), `rewind-otel` (OpenTelemetry) |
| `SecretStore` | Named secrets of the organization and of each namespace, their metadata and reads, and each tenant's signing keys; values and private keys encrypted at rest | memory (engine), `rewind-store-turso`, `rewind-store-postgres` |
| `WebhookSender` | Lifecycle events to the harness's subscriptions | memory (engine), HTTP (`rewind-server`) |
| `Clock` | Current time | system clock, virtual clock |
| `Entropy` | Run ids and seeds | system (uuid v7 / random), deterministic (tests) |

Host tools have no dispatcher: the organization actor hands their tasks to the harness's SDK
worker over its task stream (`GET /v1/tasks/stream`, [routes](04-tools.md#routes)).

Every store method is scoped by the tenant: a run (`RunRef { tenant, id }`), a script, a toolset, a
payload or a task of one tenant is never visible to another. Run ids are unique across tenants.
Stores never read a clock: every method that depends on time takes `now`. Tokens (tool tasks,
webhooks) are the only values a store draws from a CSPRNG. Text a store keeps never holds a NUL
character (U+0000), which Postgres refuses: the engine refuses it at its entry points
(`NulCharacter`), and values are JSON, where it is escaped
([decided](08-decisions.md#taken-while-fixing-the-review-of-the-engine-and-the-stores-november-2026)).
A key looked up with a NUL character matches nothing, and never reaches a store: a token or an id
is not found, and a filter is refused with 400
([decided](08-decisions.md#taken-while-fixing-the-runs-that-kill-their-workers-and-the-lookups-with-a-nul-november-2026)).

### `CatalogStore`

Methods (declared in `crates/rewind-engine/src/ports/catalog.rs`):

```
put_script(tenant, script, compiled) / get_script(tenant, digest)
put_toolset(tenant, toolset, declarations) / get_toolset(tenant, digest) / list_toolsets(tenant)
put_workflow(tenant, workflow, labels, now) -> StoredWorkflow / get_workflow(tenant, version)
workflow_versions(tenant, name, labels) / workflows_with_script(tenant, script, labels)
latest_workflows(WorkflowFilter { tenant, labels, after, limit })
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

- A workflow version is immutable: a second `put_workflow` of a version changes nothing and returns
  the first. `workflow_versions` lists a name's versions carrying `labels`, newest first (created
  last first, then stored last first): the first is the latest; `workflows_with_script` lists the
  versions of any name whose script is `script`, in the same order; `latest_workflows` gives the
  latest version of each name, by name.
- Tests ([suites](05-testing-and-observability.md#suites-and-the-gate)): the documents are
  blobs; the store indexes them. `reserve_tests` inserts a test's row, or makes an existing one
  new again, before its blob is written, and returns the digests whose deletion has started.
  `put_workflow` writes the rows of `workflow_tests` from the version's suite, and refuses
  (`Conflict`) a version that references a test without a row, or whose deletion has started.
  `unreferenced_tests` marks the rows no version references, stored more than 24 hours ago, and
  returns them; their blobs are deleted, then `drop_tests` removes the rows still marked. With
  `erased`, it removes the given rows whatever references them: a namespace's erasure takes the
  tests `namespace_tests` lists.
- `ensure_namespace` creates a namespace at its first use with the tenant's defaults and its
  partition, `hash(tenant, name) mod partitions`, and returns it; a namespace being deleted is
  `Deleting`. The partition of a namespace never changes.
- `revoke_namespace_tokens` sets the namespace's `tokens_revoked_before`, which refuses the
  signed tokens issued earlier. `delete_namespace` marks the namespace; `namespaces_to_erase`
  lists the marked ones; `drop_namespace` removes a marked namespace, once its partition holds
  nothing of it.
- Only scripts that compile are stored: `start_run` refuses the others before creating a run
  ([execution](03-execution.md#starting-a-run)). `ScriptExecutor::compile` takes the toolset,
  to check what the script imports from `rewind:tools`.
- Every write sends its `ControlPlaneChange` in the same transaction (`Cluster` below).

### `PartitionStore`

Methods (declared in `crates/rewind-engine/src/ports/partition/`):

```
open(partition, node) -> PartitionLog            # the only writer from now on: fences the previous one
recover(horizon) -> Recovery { runnable, quarantined, timers, tasks, unsealed, outbox, counters, organizations }
run(run) -> RunHead?                  summary(run) -> RunSummary?
history(run, attempt, from) -> [HistoryEntry]    inbox(run) -> [InboxItem]
tasks(run) -> [ToolTask]              payload(run, digest) -> PayloadRow?
watchers(run) -> [Watcher]            timers(from, to) -> [DueTimer]
started(tenant, namespace, key) -> RunId?        signal_key(run, key) -> bool
webhook(token_hash) -> (RunId, name)?
search(Scope, RunQuery, RunOrder, Page { cursor, limit, budget }) -> Found { summaries, cursor }
counters(Scope, Window) -> Counters   gauges(Scope) -> Gauges     shapes(tenant, namespace?, tools)
organization(tenant) -> OrganizationState?
commit(Batch) -> ()                   # atomic and durable; Fenced once another node opened the partition
```

A `Batch` is a list of changes, of one or many runs, applied together:

```
CreateRun { run, head, start_key?, entries?, payloads, summary }   # entries: a fork's copied history
Enqueue { run, item, signal_key? }
Activation { run, attempt, entries, consumed, payloads, tasks, timers, head, summary, counters, outbox }
Task { run, seq, change }             Timer(Set | Cancel)
Failure { run, error }                Quarantine { run, kind, reason } | Release { run }
Seal { run, attempt }                 Erase { run }             EraseNamespace { tenant, namespace }
Interrupted { run }                   Watch { run, watcher }
Outbox(Add | Remove)                  Counters([CounterDelta])
WebhookToken { run, name, hash }      Organization { tenant, state }
```

Contracts, all covered by the conformance suite:

- `open` fences: once another `open` of the same partition succeeded, every `commit` of the earlier
  log fails with `Fenced`, and none of its changes is visible. A commit that returned is durable: a
  later `open`, on any node, reads it.
- A batch is atomic: after a crash, either all its changes are read or none.
- The owner is the only writer, so the store checks what a single writer may get wrong and refuses
  it (`Conflict`): entries appended anywhere but at the attempt's history length, a start key
  already used in the namespace, a consumed inbox item that does not exist, a change to a run that
  does not exist or has been erased.
- `Enqueue` with a signal key already seen for that run changes nothing. A terminal `Activation`
  clears the run's timers, tool tasks and inbox, and its webhook tokens stop resolving; nothing is
  enqueued for an ended attempt.
- `Activation` writes, with the entries, the payload records of the payloads that left the events
  (digest, size, preview, the value up to 64 KiB), the run's summary and its index entries, and the
  counters it moves.
- `recover(horizon)` returns the runs that are runnable (a non-empty inbox, not quarantined, not
  ended), the quarantined runs, the timers due before `horizon`, the tool tasks that are `ready`,
  `deferred` or waiting for approval, the attempts that ended and are not sealed, the outbox, the
  counters of every tenant, and the organization states homed here.
- `Seal` deletes the attempt's history, inbox, tool tasks, payload records and signal keys, and keeps
  the run's head, summary and index entries. `Erase` deletes everything of the run, and the counts it
  held. Neither is refused for a run already sealed or erased.
- `search` evaluates every term of the query on the summaries of the scope, in the order asked,
  reads at most `budget` summaries, and returns a cursor when it stopped before the end. Summaries
  are kept for ended runs until their erasure.
- `counters` sums the hourly counters of the scope over the window, rounded out to whole hours;
  `gauges` reads the exact gauges (active runs, waiting runs by kind).
- Timers are ordered by due time, then by run and kind; `timers(from, to)` includes `from` and
  excludes `to`.

### `RunArchive`

```
put(SealedAttempt { run, attempt, script, toolset, entries }) -> ()   # idempotent
get(run, attempt) -> SealedAttempt?
erase(run) -> ()                                               # every attempt and every blob of the run
```

The archive writes the [journal document](02-journal.md#stored-format-v1) of the attempt, every
payload up to 64 KiB inlined. A second `put` of the same attempt changes nothing. `get` of an
erased run finds nothing.

### `Cluster` and `NodeLink`

```
Cluster::join(NodeInfo { id, address, build, epochs, pool, executors_only }) -> Membership
Membership::watch() -> Departed { node }      # the lock this node waits on came free
Membership::assign(Moves) -> PartitionMap     # one transaction: owners and epochs + 1, then the change sent
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
- `assign` moves partitions in one transaction, raising each moved partition's epoch, and sends
  `PartitionMap` as it commits.
- `changes` yields every `ControlPlaneChange` committed by any process after the call: the partition
  map, a namespace, a tenant, a key, a token or token key, a grant or role, a connection, a secret, a
  workflow, a subscription. On Turso and in memory, the cluster has one member and the changes are
  a broadcast inside the process.
- `NodeLink` carries the requests an edge sends an owner (start, signal, cancel, fork, release, give
  up, erase, a task's answer, reads), those an owner sends an organization actor and back
  (admission, counts, slots, host tasks), the hand-out of a host task to the edge that holds the SDK
  stream, subscriptions, and activations for remote executors. An owner that no longer holds a
  partition answers `NotOwner`.

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
  `user:<id>`, `key:<hash>` or `invitation:<id>`; `keep` names a role (`owner`) that the write
  must not leave without a holder in the tenant, else it returns `LastHolder` and changes
  nothing. `claim_grant(tenant, invitation, user)` moves an invitation's grant to the user who
  accepted it, unless the user already has one. `set_default_role(tenant, role) /
  default_role(tenant)`. Built-in roles are not stored: the server knows them. Every write sends
  its `ControlPlaneChange`.
- **`IdentityProvider`** (`crates/rewind-engine/src/ports/identity.rs`): `authorization_url`
  (GitHub), `authenticate_code`, `send_magic_code` (`Sent`, or `SignUpClosed` without a code),
  `authenticate_magic_code`, `verify_email`,
  `select_organization`, `refresh`, each answering `Authenticated` (the user, the organization if
  one is selected, the tokens) or the next step the provider asks for
  (`OrganizationSelectionRequired`, `EmailVerificationRequired`, `SignUpClosed`);
  `verify_access_token -> AccessClaims { user, session, organization, expires_at }`;
  `revoke_session`, `join_waitlist`; organizations (`create`, `rename`, `delete`, `get`),
  `user`, memberships (`of_user`, `of_organization`, `add`, `remove`), invitations (`invite`,
  `of_organization`, `for_email`, `get`, `accept`, `resend`, `revoke`). Memberships and
  invitations carry no role. Tokens are opaque to Rewind but for the access token's claims.
- **`BlobStore`**: `put(owner, digest, bytes)`, `get(owner, digest)`, `copy(from, to, digest)`,
  `delete_run(run)`, `delete(owner, digest)`. The owner is a run (`BlobOwner::Run`), for its payload
  values above 64 KiB, or a tenant's tests (`BlobOwner::Tests`). Immutable bytes keyed by their
  SHA-256: a payload's canonical JSON, so the key is the payload's digest, or a test's document.
  `put` refuses bytes that do not hash to the digest; a second `put` changes nothing. `get` checks
  the digest of what it reads. `copy` serves a fork, `delete_run` the erasure of a run, `delete` the
  cleanup of tests.
- **`ScriptExecutor` / `ScriptInstance`**: `compile`, `start`, `deliver`, `memory_usage`
  (the memory an instance holds, which bounds the instance cache), and `cpu_time` (the CPU time
  of the instance's last call into the script, failed or not: loading the script and
  `main(input)` after `start`, one delivery after `deliver`). The executor knows nothing about
  the instance cache nor about replay, which live in the engine: the engine decides which calls
  count toward a namespace's `cpuPerMonth` ([quotas](06-storage-and-scale.md#quotas)).
- **`ToolDispatcher`**: `dispatch(call, auth) -> Dispatched`. The call carries the run, its
  namespace and labels, the `seq`, the pinned tool and its server or API, the args, the
  idempotency key, the attempt, whether the harness approved a schema change (then the call is
  sent without checking the schema again), the task's token and the trace context. `auth`
  (`CallAuth`) is what the server's `auth` asks ([authenticating
  calls](04-tools.md#authenticating-calls)): nothing, the attached secret's value with where it
  goes, or the tenant's signing key with the claims the engine knows (`iss`, `sub`, `labels`,
  `run`, `tool`, `idempotency_key`, the attempt), with which the dispatcher signs each request it
  builds, adding `aud`, `htm`, `htu`, `body`, `iat`, `exp` and `jti`, since the JWT covers the
  method, the URL and the body. Mock mode passes nothing. `Dispatched` is one of: `Result` and `Failed` (the
  outcome); `Transient`, after which the engine puts the task back to `ready` with backoff, or
  fails it under `at_most_once`; `Deferred`, the handler answering later through the task's
  token; `Park` with a reason (schema changed at dispatch, credentials refused). The dispatcher
  never writes to a store. The engine parks a call itself (`secret_missing`) when the secret
  the run attached no longer exists, before anything is sent.
- **`TypeChecker`**: `check(script, declarations) -> diagnostics`, and
  `check_workflow(script, declarations, workflow)` for a workflow's script, `workflow` being its
  own declarations (its signals, and `entry.ts`). Deterministic.
  Diagnostics are written for a model, with a location in `script.ts`, a severity, their source
  (a TypeScript code or one of the checker's own rules), a hint and a code frame
  ([type-checking](04-tools.md#type-checking)).
- **`SpanExporter`**: `export(spans)`, `export_logs(logs)`. Spans have deterministic ids, so
  exporting one twice gives the same span.
- **`SecretStore`**: secrets are keyed by tenant, namespace (`*` for the organization's) and
  name: a `SecretId`, what an attachment names once resolved in the run's namespace.
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
- **`WebhookSender`**: `send(subscription, event)`. Each delivery is signed with HMAC, and
  retried when it fails. Events are `run.completed`, `run.failed`, `run.cancelled`,
  `run.aborted`, `run.suspended`, `run.quarantined`, `tool.schema_changed`,
  `credentials.expired` (with its reason, `refused` or `missing`, and the attached secret's scope
  and name when there is one) and `quota.alert`, with a deterministic id for receivers to deduplicate;
  every event about a run names its namespace. Subscriptions are kept by the `CatalogStore`.
- **`Clock`** and **`Entropy`**: unchanged. `Entropy` draws a run id for a partition: a UUID of version 8, laid
  out as a version 7, whose 16 bits after the variant hold it.

Port style: traits whose methods return `impl Future<Output = …> + Send`; adapters may
implement them with `async fn`. The engine is generic over its adapters (static
dispatch); composition roots pick the concrete types. `ScriptExecutor` has an associated
`Instance: ScriptInstance` type, `PartitionStore` an associated `Log: PartitionLog`.

## Crates

| Crate | Layer | Contents |
| --- | --- | --- |
| `rewind-core` | domain | ids (`RunId`, `Seq`, `Seed` on 256 bits, `Epoch`), `Digest`, `Payload` + canonical JSON (RFC 8785), `Script`, the pinned `Toolset` (servers, tools, routes, schemas, policies), events, `RunState`, `ReplayCursor`, history validation, `Cassette`, observed `Shape`, `Timestamp`, durations |
| `rewind-engine` | application | ports, use cases, the actors of a node (`partition`: the owner, its committer, fan-out, timer wheel, registry, outbox and sealer; `executors`: the ready queue, the instance cache, activation and replay; `org`: the organization actor; `tasks`: calls and their policies; `node`: partitions taken and handed over, recovery; `router`: the edge's way to an owner), fork, workflows across runs, projector and span derivation, the driver (the actors stepped on one thread, real or virtual time), in-memory adapters (`MemoryCatalog`, `MemoryPartitions`, `MemoryArchive`, `MemoryCluster`, `LocalLink`, `MemoryAccess`, `MemoryIdentity`, `MemoryBlobs`, `MemorySecrets`, `MemorySpanExporter`, `MemoryWebhooks`, `StaticDispatcher`, `StaticTypeChecker`, system and virtual clocks, system and seeded entropy), the conformance suites of the ports with state and of `SpanExporter` (feature `conformance`) |
| `rewind-format` | adapter support | serde DTOs for the stored format v1 (events, journal documents, partition records, cassettes, pinned toolsets; JSON values in their canonical form); golden fixture tests |
| `rewind-api` | adapter support | the HTTP DTOs (serde, utoipa) of `/v1` and `/auth`, the catalog of permissions and the built-in roles, the OpenAPI document generated from them, and a thin Rust HTTP client; shared by `rewind-server` and `rewind-cli` |
| `rewind-runtime` | adapter | deno_core isolate on a dedicated thread, started from a V8 startup snapshot, deterministic globals, ops, JS glue (one version per supported epoch), how a script ends, module loader, CPU watchdog (which also reports each call's CPU time), heap limit, the budget of array buffers and the pinned stack limit (`memory`, the only `unsafe` code), `V8Executor`; the TypeScript mock dispatcher |
| `rewind-store-slatedb` | adapter | `PartitionStore` on SlateDB over `object_store`: R2 or any S3-compatible service, local files, memory; the key layout of a partition |
| `rewind-store-turso` | adapter | the embedded control plane: `CatalogStore`, `AccessStore`, `SecretStore`, and a `Cluster` of one node (Turso Database, the `turso` crate) |
| `rewind-store-postgres` | adapter | the production control plane: `CatalogStore`, `AccessStore`, `SecretStore`, and `Cluster` (session advisory locks, `LISTEN` and `NOTIFY`) |
| `rewind-workos` | adapter | `IdentityProvider` on WorkOS, through `workos`, its official Rust SDK: AuthKit's authentication, users, organizations, memberships, invitations, the waitlist, access tokens checked against the JWKS |
| `rewind-blob` | adapter | `BlobStore` and `RunArchive` on local files and S3 (R2) |
| `rewind-mcp` | adapter | `ToolDispatcher` outside mock mode: MCP client (discovery, dispatch, the attached secret or the signature in the transport) and the built-ins `http` and `webhook` |
| `rewind-openapi` | adapter | API tools ([04](04-tools.md#api-tools)): an OpenAPI 3.0 or 3.1 document read into pinned tools at registration, and `ApiDispatcher`, the `ToolDispatcher` of the `api` route: the request built from the args, its authentication, the answer mapped to an outcome |
| `rewind-outbound` | adapter support | what leaves Rewind for a tenant's servers: the egress checks (a name resolved once, private addresses refused, the connection to the checked address, redirects within the origin) and the HTTP client built on them, the JWT signing of [signed calls](04-tools.md#signed-calls); used by `rewind-mcp`, `rewind-openapi` and the server's webhooks |
| `rewind-agent-tools` | adapter support | the agent tools in Rust, as the SDK defines them: their definitions (the SDK's set, and MCP's: `save_workflow` out, `list_orgs`, `list_namespaces` and `list_toolsets` in, `org`, `namespace` and `toolset` as arguments; `list_secrets` and the `secrets` argument in both), their handlers over a port `Api` (one method per operation of `/v1` they call, with `rewind-api`'s DTOs), and their bounded plain-text results; golden fixtures shared with the SDK's |
| `rewind-mcp-server` | adapter support | Rewind's MCP server ([MCP and OAuth](#mcp-and-oauth)): an `rmcp` `ServerHandler` over `rewind-agent-tools`, with the tools' titles, annotations and `instructions`, served over Streamable HTTP without session (by `rewind-server`) or stdio (by `rewind-cli`) |
| `rewind-codegen` | adapter support | our JSON Schema → TypeScript converter; pinned toolsets → `rewind:tools`, `rewind-tools.d.ts` (with `rewind:workflows`), prompt rendering; a workflow's own declarations |
| `rewind-check` | adapter | type-checking with tsgo, driven as a CLI and embedded in the `rewind` binary; the checker's own rules on the swc syntax tree |
| `rewind-otel` | adapter | OpenTelemetry SDK and OTLP export (gRPC, HTTP): `SpanExporter`, and the `tracing` subscriber the composition roots install, which joins service spans to a run's trace through their fields `trace_id`, `span_id`, `parent_span_id` and `traceparent`, and records the measurements of the engine and the adapters as metrics |
| `rewind-cli` | composition root | the client commands of the `rewind` binary: a client of the API `/v1` (embedded or remote server); local commands `check`, `test`, replay of an exported journal; `rewind mcp` over stdio |
| `rewind-server` | composition root | `rewind server`: the edge (HTTP API with axum, SSE, the task stream, `/mcp`, the OAuth authorization server of its connections, the dashboard) and the node (partitions, organization actors, executors, projections, webhooks) in one process; the node link over HTTP/2 and its DTOs; `--executors-only` and `--pool`; embedded mode; `migrate-runs` |
| `rewind` | entry point | the `rewind` binary: hands `rewind server …` to `rewind-server` and every other command to `rewind-cli`, nothing else |
| `rewind-architecture-tests` | tooling | dependency rules |
| `rewind-e2e` | tooling | the epoch corpus: recorded scenarios (blue: must succeed, red: must fail as specified), replayed from their stored journals; `rewind-e2e replay <case>` |
| `evals` | tooling | eval suite: tasks, deterministic fake MCP servers, the engine wired in process behind `rewind-agent-tools`' port `Api`, clients of the model APIs, reports ([08](08-decisions.md#taken-while-building-the-evals-november-2026)) |
| `sdk/typescript` | client | client (types generated from the OpenAPI document), host-tool worker, agent tools, `verifyRewindCall` for a server that receives [signed calls](04-tools.md#signed-calls) (`jose`) |
| `dashboard` | client | React single-page application over the public HTTP API, built to `dashboard/dist/` ([below](#dashboard)) |

One `rewind` binary holds both composition roots. The `rewind` crate is that binary: it depends on
`rewind-cli` and `rewind-server` and only dispatches the command line, `rewind server …` to
`rewind_server::main(args)` and every other command to `rewind_cli::main(args)`, each given the
arguments after its command. The CLI's code never calls the server's: it reaches it only
through the API, and starts the embedded server by running `rewind server --embedded` as a
separate process.

`rewind-server` wires each adapter in its own module of `src/wiring/` (control plane,
partitions, archive and blobs, cluster, tools, checker, telemetry, webhooks, clock, identity),
picked by configuration: `--store turso:<path>` or `postgres://…` for the control plane,
`--bucket <directory>` or `s3://<bucket>` (with its endpoint, for R2) for the partitions, the
archive and the blobs, `--partitions` (64 by default, 1 embedded), `--cluster-database` (where the
nodes take their liveness locks, `postgres` by default), `--cluster-secret`, `--link-listen` and
`--link-address` (the node link's port and the address the other nodes dial), `--node-id`,
`--otlp-endpoint`, WorkOS's variables ([sign-in](#sign-in-auth)).

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
  `rewind-blob` also on `rewind-format`, the tool dispatchers also on `rewind-outbound`), and never
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
| Owner | `partition` | Partition held | Its mailbox: the edge's requests, executors' results, call results, `TimerDue` | Batches to its committer; after each durable batch: runs to the ready queue, timers to the wheel, calls to the call executor or to the organization actor, `Changed` to subscribed edges, ended attempts to the sealer, deliveries to the projections, counts to the organization actors |
| Committer | `partition` | Partition held | The batch being filled | One `PartitionLog::commit`, then `Durable` to the owner |
| Timer wheel | `partition` | Partition held | Its earliest timer, and timers set or cancelled | `TimerDue`; once an hour, a read of the next hour's timers |
| Sealer | `partition` | Partition held | `Ended`, once durable | `RunArchive::put`, then `Seal` to the owner |
| Executors | `executors` | Node | The node's ready queue, and activations from other owners | The activation's result to the owner of the run |
| Call executor | `tasks` | Node | Calls that are durable and hold a slot | `ToolDispatcher::dispatch`, then the outcome to the owner and `SlotFreed` |
| Organization actor | `org` | Tenant homed in a held partition | Its mailbox | `Admitted` or `Refused`, `SlotGranted`, `TaskHanded`, alerts in its own batches |
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
    cpu[(tenant, namespace, day)] += instance.cpu_time()   # calls that execute, never a replay's
    entries += [item] + emitted
move payloads > 1 KiB out of the entries: to the run's payload records, or to a blob above 64 KiB, put first
effects ← tool tasks, timers, cancelled timers, signals and starts of runs of the namespace, from the new commands
owner ← Activation { entries, consumed, payloads, effects, status, summary, counters }   # committed in the next batch
cache[run] ← instance
```

The activation is a `tracing` span (service telemetry), with the run's trace id, its own span id
`H(run, "activation", index of its first entry)` and parent, the run span `H(run, "run")`,
recorded as fields. Nothing about the script is emitted during replay.

- Script checks at start fail → `StartError::InvalidScript`, and no run is created. A text to
  store with a NUL character → `StartError::NulCharacter` (a signal's key:
  `EngineError::NulCharacter`), before anything is stored.
- Executor start errors (top-level exception) → `run_aborted { invalid_script }`.
- The run's started activations without an end exceed 5 → the script is not run; the owner
  quarantines it (kind `failures`), the reason saying how many never finished.
- Replay mismatch → nothing is committed; the owner quarantines the run (kind `non_determinism`).
- Any other failure → nothing is committed; the owner writes the failure, which counts it; the fifth
  in a row quarantines the run, and a timer tries the run again otherwise
  ([activations](03-execution.md#activations)).

Rules the engine keeps, tested in the driver:

- **Starts.** The edge finds or creates the namespace (`ensure_namespace`), then asks its owner.
  The owner answers a known start key with its run, asks the organization actor to admit the start
  (`AdmitStart`), and commits `CreateRun` once admitted. A start refused for a quota names the
  quota, the scope, the limit, the use and, for a monthly quota, the start of the next month. A
  retried request with its key returns its run, whatever the quotas
  ([quotas](06-storage-and-scale.md#quotas)).
- **Workflows across runs** ([execution](03-execution.md#workflows-across-runs)). A run started by
  a script records the starter as its first watcher, attached unless the run is detached. The
  batch of a terminal activation enqueues the run's end as the signal `rewind:ended:<run>` for
  every watcher that has not ended, with that name as idempotency key, and enqueues
  `cancel_requested` for every run this one watches attached and that has not ended. A fork's
  watch is never attached; when the run it watches has already ended, the engine enqueues the end
  itself, under the same key. All of them live in the run's partition: it is one batch.
- **Tool tasks** follow the [lifecycle](04-tools.md#tool-task-lifecycle). A task that becomes
  ready is sent once its batch is durable, within the concurrency limits: the policy's calls of
  the same tool in flight per run, held by the owner, and the server's or API's calls in flight in
  the organization, held by the organization actor. A host task goes to the organization actor,
  which hands it to an SDK stream with a free slot, with a new token. The outcome of a task that no
  longer exists is dropped. Approving a task parked by `schema_changed` marks it
  `schema_change_approved`. A secret put again resumes the calls parked by `credentials_refused`
  or `secret_missing` in the runs that attach it.
- **Timers.** Script timers, call timeouts, deferred deadlines, retry backoff, run deadlines,
  retention and webhook retries are timer records; the wheel fires them. A deadline enqueues
  `cancel_requested { reason: "ttl" }` once; for a quarantined run, the engine gives it up
  (`run_aborted { non_determinism }`, or `{ internal }` after repeated failures).
- **Ends.** A terminal activation clears the run's timers, tool tasks, inbox and webhook tokens,
  and its quarantine. The sealer seals the attempt, and the retention timer erases the run.

## Tests

| Where | What |
| --- | --- |
| `rewind-core` | Unit tests of the pure model: canonical JSON (RFC 8785 golden vectors), digests, matching, `ReplayCursor`, run state, durations |
| `rewind-format` | Round trips and golden fixtures (a stored v1 document must always decode) |
| `rewind-engine` | Use cases and actors in the driver, with in-memory adapters and a fake executor: idempotency keys, epochs and remote executors, quarantine, failure counts and attempts, run expiry, tool task lifecycle and concurrency, admission and quotas, fairness, workflows across runs, sealing and retention, recovery after a node dies, a partition moved, an owner fenced while cut off; the conformance suites of `CatalogStore`, `PartitionStore`, `RunArchive`, `Cluster`, `AccessStore`, `BlobStore`, `SecretStore` and `SpanExporter`, passed by the in-memory adapters; span derivation; projector idempotence (a delivery applied twice changes nothing) |
| `rewind-runtime` | Sandbox behavior, driven by the engine with in-memory adapters and virtual time (`Promise.all`, time, randomness, signals, imports, callsites, cancellation, limits, the exact set of globals, RFC 8785 numbers against V8, and how a script ends: forgotten `await`, unhandled rejection, deadlock) |
| Epoch corpus (`rewind-e2e`) | `corpus/epoch-N/{blue,red}/<case>/`: a script, a `scenario.json` (input, tools, steps, expectations) and its recorded `journal.v1.json`. Each case runs live and must meet its expectations, must record the committed journal byte for byte, and the committed journal must replay with the script on a fresh executor. Blue cases must succeed; red cases must fail exactly as specified (refused start, failed, cancelled or aborted run, diverging replay, tampered journal). `REWIND_E2E_RECORD=1` records the journals again |
| `rewind-store-turso`, `rewind-store-postgres` | The conformance suites of `CatalogStore`, `Cluster`, `AccessStore` and `SecretStore` (Postgres: two processes' locks and notifications) |
| `rewind-store-slatedb` | The conformance suite of `PartitionStore`, on memory, on local files, and on MinIO in Docker |
| `rewind-workos` | Against a fake WorkOS over HTTP (wiremock): every call, every error mapped, access tokens signed with a test key; against WorkOS Staging when `WORKOS_API_KEY` and `WORKOS_CLIENT_ID` are set |
| `rewind-blob` | The conformance suites of `BlobStore` and `RunArchive`, on files and on MinIO |
| `rewind-otel` | The conformance suite of `SpanExporter`, on the SDK's in-memory exporters and on an OpenTelemetry Collector in Docker over gRPC and HTTP; service spans and logs joined to a run's trace by their fields; service and run metrics, the engine driven under the subscriber |
| `rewind-cli` | End to end through the embedded server: V8 + engine + SlateDB on files + Turso, across processes (sleep, interrupt, resume) |
| `rewind-server` | Several nodes over Postgres and MinIO: a node killed during an activation, a node joining and leaving, the node link, the task stream, the change stream |
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
  designates the tenant, or for a connection the header `Rewind-Organization` does; the caller's grant, or a token's own permissions, say what it may do,
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
- A run is named by its UUID or by its handle, `<namespace>/<origin>/<suffix>`, percent-encoded
  in a path, wherever the API takes one ([handles](06-storage-and-scale.md#run-handles)). A run
  answers with both: `id`, its UUID, and `handle`; with `scriptTitle`, its script's title; the
  runs it names (`parent`, `startedBy`, a secret's `lastRead`) with their handles too. A workflow
  version has its `number` among the versions of its workflow (`triage v12`), and the API takes
  `<name>@v<number>` wherever it takes a version.
- The OpenAPI document is generated from the DTOs of `rewind-api` (utoipa) and committed at
  `api/openapi.json`; a test fails when it is stale. The SDK's and the dashboard's types are
  generated from it (`api/README.md`).

| Group | Endpoint | Permission | Does |
| --- | --- | --- | --- |
| Toolsets | `POST /v1/toolsets` | `toolsets:write` | Registers and pins a toolset: its MCP servers, its APIs from their OpenAPI documents ([API tools](04-tools.md#api-tools)), its host tools, and the workflows it names; `secrets` attaches organization's secrets to list the tools of an MCP server that needs one (`secrets:use`); `base` derives it from a pinned toolset; returns its declarations and the API operations it left out |
|  | `GET /v1/toolsets` | `toolsets:read` | The tenant's toolsets, with their servers and tool names |
|  | `GET /v1/toolsets/{id}/declarations` | `toolsets:read` | The declarations, `format=dts` or `prompt` |
|  | `GET /v1/toolsets/{id}/search` | `toolsets:read` | `search_tools`, for large toolsets |
| Workflows | `POST /v1/workflows` | `toolsets:write` | Checks a [workflow](01-product.md#workflows) against its toolset and its schemas, and its attached `secrets` against the caller's rights ([secrets](04-tools.md#secrets)), runs its suite through the [gate](05-testing-and-observability.md#suites-and-the-gate), and saves it as a new version; returns its declarations. 422 `tests-failed` lists the tests that did not pass |
|  | `GET /v1/workflows` | `toolsets:read` | The tenant's workflows, each with its latest version; with `script`, every version whose script it is, newest first |
|  | `GET /v1/workflows/{name}` | `toolsets:read` | A workflow's versions, newest first, each with its suite |
|  | `POST /v1/workflows/{name}/record` | `runs:read` | Records tests from the workflow's past runs, one per path ([recorded tests](05-testing-and-observability.md#tests-recorded-from-runs)); stores them, adds them to no suite |
|  | `GET /v1/tests/{digest}` | `toolsets:read` | A test document; a recorded one also needs `runs:read` in its namespace |
| Scripts | `POST /v1/check` | `runs:start` | Type-check only |
|  | `POST /v1/test` | `runs:start` | Runs tests in mock mode, in virtual time: the given ones, and a workflow's suite ([testing](05-testing-and-observability.md#running-tests)) |
| Runs | `POST /v1/runs` | `runs:start` | Starts a run: `Idempotency-Key`, `wait`, `labels`, `traceparent`, `ttl`; a script, a toolset and the `secrets` it attaches, or a `workflow` (`triage` or `triage@<version>`), which brings its version's |
|  | `GET /v1/runs` | `runs:read` | Lists runs: the [query language](05-testing-and-observability.md#query-language) `q`, filters by label, status, tool, error; `sort` by start (newest or oldest first), last activity, next wake, duration (longest first) or number of tool calls |
|  | `GET /v1/runs/stats` | `runs:read` | From the hourly counters of a namespace or the tenant ([statistics](05-testing-and-observability.md#statistics)): counts by status, a series over a time window, durations, runs waiting by kind, facets on status, workflow and namespace |
|  | `GET /v1/runs/failures` | `runs:read` | The failed, cancelled and aborted runs of a namespace or the tenant over a window, grouped by fingerprint ([statistics](05-testing-and-observability.md#statistics)) |
|  | `GET /v1/runs/{id}` | `runs:read` | The run's state, with the names of the secrets it attached |
|  | `GET /v1/runs/{id}/trace?format=llm` | `runs:read` | The LLM trace |
|  | `GET /v1/runs/{id}/entries/{idx}` | `runs:read` | One journal entry in full |
|  | `GET /v1/runs/{id}/events` | `runs:read` | SSE, `follow=1` |
| Changes | `GET /v1/changes` | `runs:read` | SSE of what changes in the organization, as the caller may see it (an end user's namespace, a grant's labels, the namespace `Rewind-Namespace` names): a `run` event holds the run's summary, the same object as an item of `GET /v1/runs`, pushed by the run's owner once its batch is durable; `script`, `workflow`, `secret`, `namespace` and `connection` events name what changed in the control plane; `resync` says more changed at once than one pass reads, and the client reads again what it shows. No resume cursor: after a drop, the client reads again what it shows. Closed as a run's event stream is: at a member's access token's expiry (one already expired is refused with 401), once an end user's token or a connection stops authenticating, once the caller's grant no longer gives `runs:read` with the same labels or its key is revoked |
|  | `POST /v1/runs/{id}/signals/{name}` | `runs:control` | `Idempotency-Key`; 409 if the run has ended; 422 for a name starting with `rewind:`, and, for a run of a workflow, a signal it does not declare or a payload that does not match |
|  | `POST /v1/runs/{id}/cancel` | `runs:control` | Cancels the run |
|  | `POST /v1/runs/{id}/fork` | `runs:start` | `retry_run`: a corrected script, an optional `at`, `input` and `secrets` ([forks](03-execution.md#forks)); for a run of a workflow, its version's suite runs against the script first (422 `tests-failed`) |
|  | `GET /v1/runs/{id}/journal` | `runs:read` | Exports the journal document; `?as=cassette` exports a cassette |
|  | `POST /v1/runs/{id}/release` | `runs:quarantine` | Takes the run out of quarantine |
|  | `POST /v1/runs/{id}/give-up` | `runs:quarantine` | Gives up a quarantined run |
|  | `DELETE /v1/runs/{id}` | `runs:delete` | Erases the run, payloads included |
| Host-tool tasks | `GET /v1/tasks/stream` | `tasks:work` | SSE: the tasks handed to this worker as they become ready, filtered by toolset and tools, and to the runs the caller sees (a token's namespace, a grant's labels); `slots` bounds the tasks in flight on it. The open stream is the liveness of the tasks handed on it ([lifecycle](04-tools.md#tool-task-lifecycle)) |
|  | `POST /v1/tasks/{token}/defer` | `tasks:work` | The handler answers later: the task is `deferred` until `complete` or `fail`, whether the stream stays open or not |
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
|  | `GET /v1/tools/stats` | `runs:read` | Per tool, from the hourly counters of a namespace or the tenant over a window: calls, latency, errors, retries |
|  | `GET /v1/payloads/{digest}` | `runs:read` | A payload, on demand, through the run that holds it (`run`, required: a payload belongs to its run) |
| Organization | `GET /v1/whoami` | any caller | Who calls: the tenant, the principal (a key's id, the user with their email, name and picture, or a token: its kind, `opaque` or `signed`, its id or key and `jti`, its expiry), the grant (role, allowed labels) or the token's namespace and labels, the permissions it holds; for a member, the organization and every organization of the user; for a connection, its id, person and client, and the organizations it opens with their access |
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
| Connections | `GET /v1/connections` | a member, even without organization | The caller's [connections](06-storage-and-scale.md#connections): the client, the organizations each opens with its access and labels, when it was made and last used |
| | `PATCH /v1/connections/{id}` | its person | Sets the organizations it opens, their access and labels: each an organization where the person holds a grant giving that access |
| | `DELETE /v1/connections/{id}` | its person | Revokes it: its tokens and its grants |
| | `GET /v1/organization/connections` | `members:read` | The connections that open the organization: the person, the client, the access, the labels, the last use |
| | `DELETE /v1/organization/connections/{id}` | `members:manage` | Removes the organization from the connection |
| | `GET /v1/oauth/consent?request=…` | a member, even without organization | What an authorization request asks ([MCP and OAuth](#mcp-and-oauth)): the client, where it returns, the person's organizations and the accesses they may give |
| | `POST /v1/oauth/consent` | a member, even without organization | Allows it (organizations, accesses, labels) or denies it; answers where to send the browser |
| Access | `GET /v1/permissions` | any caller | The catalog: every permission with its description |
| | `GET /v1/roles` | `roles:read` | The built-in roles and the tenant's own, with their permissions |
| | `PUT /v1/roles/{name}` | `roles:write` | Creates or changes a role of the tenant |
| | `DELETE /v1/roles/{name}` | `roles:write` | Deletes a role no grant points to (409 otherwise) |
| | `GET /v1/api-keys` | `api_keys:read` | The tenant's keys, without their secret, each with its grant |
| | `POST /v1/api-keys` | `api_keys:write` | Creates a key with a name, a role and allowed labels; an operator may name another tenant. The secret is only in this answer |
| | `DELETE /v1/api-keys/{id}` | `api_keys:write` | Revokes a key |
| Namespaces | `GET /v1/namespaces` | `namespaces:read` | The namespaces, paged: `prefix` (of the name, or of the display name case aside), `above` (a share of a limit, `0.8`), `order` (`cpu`, `runs`, `scripts`, `name`); each with its display name, its limits, its exceptions and its use this month; how many match across the pages (`total`) |
|  | `GET /v1/namespaces/{name}` | `namespaces:read` | One namespace: its display name, its limits and exceptions, its use per day over a `month`, its alerts, its tokens in use |
|  | `PUT /v1/namespaces/{name}` | `namespaces:write` | Creates the namespace, or sets its exceptions; `null` gives a quota back to the default |
|  | `PATCH /v1/namespaces/{name}` | `namespaces:write` | A JSON merge patch of an existing namespace (404 otherwise): `displayName` sets its display name, `null` or `""` removes it. Answers as `PUT` |
|  | `DELETE /v1/namespaces/{name}` | `namespaces:write` | Erases the end user in the background (202): tokens, runs, secrets, usage |
|  | `GET /v1/organization/namespace-defaults` | `namespaces:read` | The default limits of every namespace, and the tenant's ceiling |
|  | `PUT /v1/organization/namespace-defaults` | `namespaces:write` | Sets the default limits |
|  | `GET /v1/organization/namespace-names` | `namespaces:read` | Every namespace that has a display name, with it, by name; not paged: what the dashboard loads once to show display names |
|  | `GET /v1/organization/quota-alerts` | `namespaces:read` | The alerts of a `month`, of one `namespace` or of all |
| Tokens | `POST /v1/namespaces/{name}/tokens` | `tokens:write` | Mints an end user's opaque token: `ttl`, `permissions`, `labels`; creates the namespace if needed. The secret is only in this answer |
|  | `DELETE /v1/namespaces/{name}/tokens` | `tokens:write` | Revokes the namespace's opaque tokens and the signed ones issued before now |
|  | `DELETE /v1/tokens/{id}` | `tokens:write` | Revokes one opaque token |
|  | `GET /v1/organization/token-keys` | `tokens:write` | The keys the tenant signs its tokens with |
|  | `POST /v1/organization/token-keys` | `tokens:write` | Registers a public key (a JWK: Ed25519, P-256 or RSA) with a name and the permissions its tokens may carry; answers its `kid` |
|  | `DELETE /v1/organization/token-keys/{kid}` | `tokens:write` | Deletes a key: every token it signed is refused |
| Secrets | `GET /v1/secrets` | `secrets:read` | The organization's [secrets](04-tools.md#secrets) the caller sees (its labels): each one's name, labels, who wrote it and when, reads per day over 30 days and last read; never a value |
|  | `PUT /v1/secrets/{name}` | `secrets:write` | Creates or replaces an organization's secret: `value`, `labels`; makes ready the calls parked by a refusal or a missing secret in the runs that attach it. Refused to an end user's token (403) |
|  | `DELETE /v1/secrets/{name}` | `secrets:write` | Deletes it: the next call of a run that attaches it is parked |
|  | `GET /v1/secrets/{name}/usage` | `secrets:read` | What reads it: the active runs that attach it, those of them waiting on a call it serves, the workflows whose latest version attaches it, its reads per day, its last read |
|  | `GET /v1/namespaces/{name}/secrets` | `secrets:read` | The end user's secrets, as above |
|  | `PUT /v1/namespaces/{name}/secrets/{secret}` | `secrets:write` | Creates or replaces an end user's secret, as above; a token, in its own namespace. Refused to a grant or a token limited to labels (403) |
|  | `DELETE /v1/namespaces/{name}/secrets/{secret}` | `secrets:write` | Deletes it, as above |
|  | `GET /v1/namespaces/{name}/secrets/{secret}/usage` | `secrets:read` | What reads it, as above |
|  | `GET /v1/organization/signing-keys` | `secrets:read` | The tenant's [signing keys](04-tools.md#signed-calls): `kid`, made, retired; and the URL of their public keys |
|  | `POST /v1/organization/signing-keys` | `organization:write` | Replaces the signing key: the old one stops signing and stays published 24 hours |
|  | `GET /organizations/{tenant}/jwks.json` | none | The public keys, outside `/v1`: the current one and those retired less than 24 hours ago; `Cache-Control: max-age=300` |
| Operator | `PUT /v1/organizations/{tenant}/quotas` | the operator | Sets a tenant's ceiling |
| Server | `GET /v1/server` | any caller | The server's version and epochs, and how many partitions and nodes the deployment has |
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
  access token is 401. A key and a token fix their tenant, and a token its namespace: `org` and
  `namespace` may repeat them, not name others.
- **`initialize`** answers `instructions`: from an organization to a run, through `list_orgs`,
  `list_namespaces`, `list_toolsets`, `search_tools`, then checking, testing, and running with
  the secrets `list_secrets` names for the servers that need one.
- **`tools/list`** lists the tools that some organization of the caller allows, each with a
  `title`, its input schema and its annotations. **`tools/call`** runs the agent tool as the
  SDK's handler does, with the same plain-text results; a mistake the model can fix is a result
  with `isError`. `wait` is at most 200 s, 30 s by default (Claude gives a call 240 s), and a
  result at most 80 000 characters (Claude Code takes 25 000 tokens by default), the rest paged
  with `offset` as in the SDK.
- **How a tool runs.** The tool calls `/v1` on this server over the loopback address, with the
  caller's Bearer, `Rewind-Organization` and `Rewind-Namespace`: one authorization, the code and
  the limits every client goes through. What runs model-written code (`POST /v1/test`,
  `POST /v1/workflows`, a fork's suite) runs on the node that answers, under its sandbox.
- **No `Host` check.** Every request carries a Bearer and none reads a cookie, so a page that
  rebinds a name to the server gains nothing.

| Tool | Permission | Arguments beyond the SDK's | Annotations |
| --- | --- | --- | --- |
| `list_orgs` | none | none | read-only |
| `list_namespaces` | `namespaces:read` | `org`, `prefix` | read-only |
| `list_toolsets` | `toolsets:read` | `org` | read-only |
| `search_tools` | `toolsets:read` | `org`, `toolset` | read-only |
| `list_secrets` | `secrets:read` | `org`, `namespace` (optional) | read-only |
| `get_workflow` | `toolsets:read` | `org` | read-only |
| `check_script`, `test_script` | `runs:start` | `org`, `namespace`, `toolset` | read-only |
| `get_run`, `list_runs` | `runs:read` | `org`, `namespace` (optional) | read-only |
| `record_tests` | `runs:read` | `org` | |
| `run_script` | `runs:start` | `org`, `namespace`, `toolset` | open world |
| `retry_run` | `runs:start` | `org` | open world |
| `signal_run` | `runs:control` | `org` | |
| `cancel_run` | `runs:control` | `org` | destructive |

`org` is an organization's id or name, as `list_orgs` gives them; it may be left out when the
caller reaches one organization only, and leaving it out with several answers their list.
`toolset` is a toolset's id, as `list_toolsets` gives it. A new namespace's name creates it at
the first run, as on `/v1`.

## Embedded server and CLI

The CLI never opens what `.rewind/` stores. It is a client of the API `/v1`: of a remote server, or
of the embedded server of a `.rewind/` directory. Only one process opens the directory, so Turso
stays in its stable single-process mode, SlateDB has one writer, and the instance cache lives in
that server.

`rewind server --embedded` is the engine host for a `.rewind/` directory: one node, which owns the
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
  tenant `local`.
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
  the replay of an exported journal. `rewind test` runs on the server (`POST /v1/test`) when the
  tests need what the server holds: a workflow's suite (`--workflow`) or a toolset registered
  there (`--toolset <id>`).

The `.rewind/` directory:

```
project/
  script.ts
  rewind.tools.json         static responses per tool, for mock mode
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
| `rewind run script.ts [--input …] [--wait 30s] [--ttl 30d] [--secret <server>=<scope>/<name>]…` | Starts a run and waits for it, up to `--wait`; `--secret` attaches a [secret](04-tools.md#secrets) |
| `rewind check script.ts` | Type-checks a script against the toolset's declarations ([tools](04-tools.md#type-checking)) |
| `rewind test [script.ts] [--tests file] [--workflow name] [--toolset id\|file]` | Runs tests in virtual time ([testing](05-testing-and-observability.md#testing-and-mocks)): those of `--tests` (`script.tests.json` next to the script by default), in the process; with `--workflow`, the suite of that workflow too, on the server, against the script or the version's own |
| `rewind workflows record <name> [--query …] [--run <id>…] [--limit n]` | Records tests from a workflow's past runs; prints their digests, names and paths |
| `rewind signal <run> <name> [payload]` | Sends a signal |
| `rewind cancel <run>` | Cancels a run |
| `rewind replay <run> [--script fixed.ts]` | Replays locally and checks determinism; `<run>` is a run id or an exported journal |
| `rewind fork <run> --script fixed.ts [--at <index>] [--input …] [--secret …]` | Starts a [fork](03-execution.md#forks) |
| `rewind trace <run> [--entry n]` | LLM-readable trace |
| `rewind runs` | Lists runs: a query of the [query language](05-testing-and-observability.md#query-language) (`rewind runs '@agent:triage status:failed'`), filters, `--sort` |
| `rewind release <run> [--give-up]` | Takes a run out of quarantine, or gives it up |
| `rewind status <run> [--wait 30s]` | A run's state, waiting up to `--wait` for it to end |
| `rewind export <run> [--as journal\|cassette] [-o file]` | The run's journal document, or a cassette of its calls and signals |
| `rewind toolsets add\|declarations\|search` | Registers a toolset file; its declarations; searches its tools |
| `rewind secrets list\|put\|delete\|usage [name] [--namespace <name>]` | The organization's secrets, or a namespace's; `put` reads the value from standard input, never from an argument |
| `rewind namespaces list\|show\|rename\|quotas\|delete` | The namespaces and their use; one namespace; sets its display name, or removes it; sets its exceptions, or the defaults with `--defaults`; erases one |
| `rewind server [--embedded] [--executors-only] [--pool name] [--virtual-time] [--idle-timeout 10m]` | Runs the server in the foreground (`rewind server --help` lists its options) |
| `rewind server migrate-runs` | Once, at the deploy that moves runs to partitions: moves the runs of the old tables into the partitions and the archive ([08](08-decisions.md#taken-for-the-object-storage-rewrite-november-2026)) |
| `rewind mcp` | Serves the agent tools over MCP on stdin and stdout, against the embedded server: to debug them with an MCP client (the MCP Inspector, Claude Code on this machine). Never a remote server |

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
| Serving | At the root of its origin. The routes are paths (`/runs/…`): the server, and Vercel for the hosted dashboard, answer `index.html` for any path that is neither a file of the build nor under a root of the API (`/v1`, `/auth`, `/oauth`, `/.well-known`, `/mcp`, `/hooks`, `/assets`, `/organizations`); the server answers 404 for a missing file name at the root (`/favicon.ico`). An old link with a fragment (`/#/runs/…`) is rewritten by the app at load. `index.html` is not cached; `assets/` is immutable |
| Sign-in | With WorkOS: its own pages, GitHub or a code by email, through `/auth` on the same origin ([sign-in](#sign-in-auth)); onboarding (create an organization, accept an invitation), the organization picker, the waitlist, which someone new also opens from the first page. Without WorkOS: an API key typed by the operator, kept for the tab, or on the device when asked |
| API | `/v1` on the same origin, with the access token or the key as Bearer. The views follow the permissions `whoami` answers: reading needs `runs:read`; Members, Invitations, Roles and API keys (the organization, from the member's menu) are shown to whoever holds their permissions, Namespaces to whoever holds `namespaces:read`, Secrets to whoever holds `secrets:read`, and their writes to whoever holds `secrets:write`; a value is a field that is written and never read back |
| Claude | Connect Claude (`/connect`): the URL, the steps per client, the good practices, the connection seen arriving. The consent (`/connect/authorize`), reached after the sign-in when the person was not signed in. The person's connections (Account › Connections), and the organization's (Organization › Connections, `members:read`) |
| Namespace | None picked: the dashboard never sends `Rewind-Namespace` and reads every namespace of the organization; a run's namespace is a term of the query (`namespace:`) |
| Layers | `views/`, `app/` and `components/` render state and call actions; they import only `state/`. `state/` holds the TanStack DB collections (`@tanstack/db`, pinned to an exact version), the hooks that read them and the actions, and decides what to load, when it is stale and what a change touches. `sources/` holds the HTTP client, the change stream and the run event streams: a new way for the server to push changes changes this folder only. Biome's `noRestrictedImports` (`biome.jsonc`) keeps `sources/` and `@tanstack/*` (but `@tanstack/react-virtual`, a component) out of the views. What the caller may do is `state/access.tsx`; browser storage, `model/storage.ts`; views may read the contexts `app/` provides (the server's answer, by the gate) |
| Event streams | Read with `fetch`, not `EventSource`, which cannot send the `Authorization` header. A run's is resumed with `from` after a drop. The change stream (`GET /v1/changes`) is one per tab, reopened after a drop (1 s, doubling up to 30 s) with a read of what is on screen; while it is down, the views keep what they show, and the dot beside the organization picker says it is not live. No view reads on a timer, and the lint refuses `refetchInterval` |
| Resources | None from another origin: the fonts are in the build. No inline script. The one image from elsewhere is a member's picture, as WorkOS gives it |

`dashboard/README.md` has the development commands; `pnpm dev:mock` runs it against a mocked API.
