# 6. Storage, scale and security

## Backends

Storage sits behind the ports of `rewind-engine` ([07](07-code-architecture.md#ports)), in two
planes.

- The **control plane** holds what customers and operators configure, written at the pace of
  people: tenants, namespaces, keys, tokens, roles, grants, connections, workflows and their code,
  action sets, the index of tests, secrets, subscriptions, and the cluster's nodes and partitions.
  It sits behind `CatalogStore`, `AccessStore`, `SecretStore` and `Cluster`.
- The **run plane** holds runs: everything a run records, its index and its counters. It is cut
  into [partitions](#partitions-owners-and-nodes) behind `PartitionStore`. The histories of ended
  attempts are sealed into the **archive** behind `RunArchive`. Payload values above 64 KiB and
  test documents are blobs behind `BlobStore`.

| Use | Control plane | Partitions | Archive and blobs |
| --- | --- | --- | --- |
| Production (`rewind-server`) | Postgres (`rewind-store-postgres`) | SlateDB on S3 or any S3-compatible service (R2, MinIO…) (`rewind-store-slatedb`) | S3 or any S3-compatible service (`rewind-blob`) |
| Embedded (`rewind server --embedded`, local dev, tests) | Turso Database (`rewind-store-turso`, the `turso` crate) | SlateDB on local files under `.rewind/partitions/` | Local files under `.rewind/` (`rewind-blob`) |
| Engine unit tests, test runs | In memory | In memory | In memory |

Postgres never holds a run, live or ended. Every adapter passes its port's conformance suite,
shipped by `rewind-engine` behind the `conformance` feature.

**A store is written only when a state changes**: a fact a run records, an owner that moves, a
setting someone changes. Nothing is written while nothing happens: no lease is renewed, no
heartbeat stored, no queue position kept, no table polled. A call costs its run two
appends: the call, before it is sent, and its outcome, before the code sees it
([decided](08-decisions.md#taken-for-the-object-storage-rewrite-november-2026)).

## Where data lives

Every key starts with the tenant, and what belongs to an end user names its namespace
([tenancy](#tenancy-namespaces-identity-and-access)).

### The control plane

```
tenants    (id PK, quotas, namespace_defaults, retention)
data_keys  (tenant PK, wrapped_key, master_key)      -- wrapped_key: the tenant's data key, encrypted
                                                     -- by the master key whose id is master_key
namespaces (tenant, name, partition, display_name, quotas, tokens_revoked_before, created_at,
            last_active_at, deleting_since)                PK (tenant, name)
api_keys   (hash PK, tenant, name, created_by, created_at, revoked_at)
tokens     (hash PK, tenant, namespace, permissions, labels, expires_at, created_by, created_at,
            revoked_at)                                    index (tenant, namespace)
token_keys (kid PK, tenant, name, jwk, permissions, created_by, created_at)
roles      (tenant, name, description, permissions, created_at, updated_at)   PK (tenant, name)
grants     (tenant, principal, role, allowed_labels, granted_by, granted_at)   PK (tenant, principal)
                                                -- principal: member:…, key:…, invitation:…, connection:…
oauth_clients (id PK, kind, name, uri, logo_uri, redirect_uris, auth_method, secret_hash,
               fetched_at, expires_at, created_at, last_used_at)
                                                -- id: a client's metadata URL, or rc_… (registered)
oauth_codes (hash PK, client_id, user_id, redirect_uri, code_challenge, resource, organizations,
             labels, expires_at, used_at, connection_id)
connections (id PK, user_id, client_id, client_name, created_at, last_used_at, revoked_at,
             expires_at)                                   index (user_id)
connection_tokens (hash PK, connection_id, kind, expires_at, created_at, used_at)
                                                           index (connection_id)
access_settings (tenant PK, default_role)
code       (tenant, digest, source, transpiled, source_map)   PK (tenant, digest)  -- only code that compiles
action_sets (tenant, digest, spec, declarations)              PK (tenant, digest)
workflows  (tenant, id, namespace, name, description, labels, source_workflow, source_version,
            created_by, created_at, promoted_at)           PK (tenant, id)
                                                           unique (tenant, namespace, name)
                                                           -- namespace *: the organization's workflows
workflow_versions (tenant, workflow, version, digest, document, labels, created_at)
                                                           PK (tenant, workflow, version)
                                                           index (tenant, digest)
workflow_codes (tenant, namespace, labels, code, workflow) PK (tenant, namespace, labels, code)
workflow_tests (tenant, workflow, name, digest, added_at, retired_at)
                                                           PK (tenant, workflow, name, digest)
tests      (tenant, digest, namespace, size, reserved_at, deletion_started_at)
                                                           PK (tenant, digest), index (tenant, namespace)
                                                           -- the document is a blob
secrets    (tenant, namespace, name, ciphertext, labels, created_by, created_at, updated_by,
            updated_at, last_read_at, last_read_run, last_read_namespace)
                                                            PK (tenant, namespace, name)
                                                 -- namespace *: the organization's secrets
secret_reads (tenant, namespace, name, day, reads)         PK (tenant, namespace, name, day)
signing_keys (tenant, kid, public_key, private_ciphertext, created_at, retired_at)
                                                           PK (tenant, kid)
subscriptions (tenant, id, url, secret, types)
nodes      (id PK, address, build, epochs, pool, executors_only, joined_at)
partitions (id PK, pool, node, generation)                 -- generation: bumped at each move
legacy_runs (id PK, tenant, partition)                     -- runs moved by migrate-runs, whose ids carry no partition
meta       (key PK, value)                                 -- schema version; virtual time (embedded)
```

A `workflows` row is a [workflow](01-product.md#workflows): its id, `wf_` and 12 characters the API
draws when it creates the workflow; its scope, the namespace whose run created it, or `*` once a
member promotes it to the organization; its name, optional and unique in its scope; its
description, shown and never part of its identity; the labels of the grant that created it; and,
for a fork, the organization's workflow it copies and the version of it it last took
(`source_workflow`, `source_version`). A `workflow_versions` row holds a
[workflow version](02-history.md#stored-format-v1) document and its digest, with its `version`,
its number among the versions of its workflow, which a new version takes under a lock of its
workflow (`triage@v12`). `workflow_codes` maps each code a scope has seen, under a set of labels, to
the workflow that first had it there: a run given code without a workflow finds its workflow
there, in its namespace and then in the organization's, and a new code creates a workflow in its
namespace ([workflows](01-product.md#workflows)). `workflow_tests` holds each workflow's suite:
every test it holds or held, by name and digest, with when it joined and when it was retired,
written with the version or the addition that changed it; a fork starts with a copy of its
source's rows. A `tests` row indexes a [test](05-testing-and-observability.md#testing) whose
document is a blob of the `BlobStore`; `namespace` is the one a recorded test came from, none for a
test written by hand. `secrets`, `secret_reads` and `signing_keys` belong to the `SecretStore`: a
value and a private key are encrypted with the tenant's data key, the names, labels and dates are
not. `tokens` and `token_keys` belong to the `AccessStore`. `oauth_clients`, `oauth_codes`,
`connections` and `connection_tokens` belong to the `AccessStore` too, and to no tenant: a
[connection](#connections) is a person's, and only its grants, one per organization it opens, are a
tenant's; `user_id` there is the person's WorkOS id. `nodes` and `partitions` belong to the
`Cluster`; `namespaces.partition` is chosen when the namespace is created and never changes
([partitions](#partitions-owners-and-nodes)).

Every write of a control-plane store sends what it changed in the same transaction: `NOTIFY
control_plane` on Postgres, a broadcast inside the process on Turso and in memory. Every process
listens, and drops what it had cached of it.

### A partition

A partition is one SlateDB database, `partitions/{id}/` in the bucket. Its sixteen kinds of
records are written in the stored format ([partition records](02-history.md#stored-format-v1)),
under keys that belong to `rewind-store-slatedb`; the engine never sees a key.

| Records | One per | Holds |
| --- | --- | --- |
| Run head | Run | Its origin and the length of its handle's suffix ([handles](#run-handles)), status, determinism epoch, its code and pinned action set, the attempt it is at and the last one sealed, the counts of failed and unfinished activations, quarantine, waits, deadline, the attempt's history length, dates, and the run metadata: namespace, labels, trace context, idempotency key, source, workflow and version, the run that started it and its `seq`, `detached`, allowed labels, the secrets it uses ([05](05-testing-and-observability.md#run-metadata)) |
| History | Event of an attempt | The event, its position (`idx`), `recorded_at` |
| Inbox | Completion the run has not taken yet | The event, when it was received |
| Signal key | Signal key used | Kept for the life of the run ([idempotency](#idempotency-of-the-api)) |
| Task | Call | The call's action, route and policy copied from the pinned action set, args, state, delivery, the token handed out, `next_delivery_at`, parked reason |
| Timer | Due time | What fires: a timer of the code, a call's timeout, a deferred call's deadline, a redelivery's backoff, the run's deadline, the end of its retention, a webhook's next try |
| Payload | Payload of a run above 1 KiB | Digest, size, preview, and the value up to 64 KiB ([payloads](#payloads)) |
| Watcher | Run that waits for this run's end | The watcher, its `seq`, whether it is attached ([workflows](03-execution.md#workflows-across-runs)) |
| Signal URL token | Token | The hash of the token, the run and the signal name |
| Run summary and index | Run, live or ended | What lists, searches and the change stream show, its status and what it waits for included ([05](05-testing-and-observability.md#read-model)) |
| Start key | Idempotency key of a start | The run it started, unique per namespace |
| Counters | Namespace and hour | Exact counts of the hour, written in the batch of what they count: runs created, started and ended by status and by workflow, a sketch of their durations, CPU time, failures by fingerprint, calls per action, runs and actions per workflow ([quotas](#quotas), [statistics](05-testing-and-observability.md#statistics)) |
| Gauges | Namespace | What holds now among the namespace's runs in the partition: the active runs by status (queued, running, waiting), the waiting ones by what they wait for, the quarantined among them, the runs of each workflow; moved in the batch of every change to a run's head or summary |
| Shape | Namespace and action | The shape observed in the action's results, merged in any order ([observed types](04-tools.md#observed-result-types)) |
| Outbox item | What waits to leave the partition | A span batch to export, a webhook to send, a `quota.alert` ([outbox](#the-outbox)) |
| Tenant state | Tenant homed here | What its tenant actor keeps: alerts sent, the admissions it holds |

The determinism epoch and the deadline in a run head are copies of the values in `run_started`,
kept so the owner can choose executors and schedule the deadline. Quarantine holds why a run is
[quarantined](03-execution.md#non-determinism-and-quarantine) (non-determinism or repeated
failures, the reason, and the last build that retried it); `failures` counts the run's
activations that failed in a row, and `unfinished` the activations an owner started without seeing
their end.

The status in a run's head and summary is the owner's: it sets it at each batch, from what the
head already holds ([run state](02-history.md#run-state)). A run is `queued` while no step of it
is committed; `running` while a step is in progress, or a call Rewind makes itself (to an MCP
server, an API or `http`) is in flight or backing off; `waiting` otherwise, until it ends: on a
signal, a timer, a call of a harness action (`ready` or `claimed`), a call waiting for approval,
deferred or parked, or a quarantine; and then the ending its history records. The kinds it waits
for are the summary's `waitingFor`, which the gauges count.

### The archive and the blobs

| Object | Key | Written |
| --- | --- | --- |
| Sealed attempt | `runs/{tenant}/{run}/{attempt}.json` | When an attempt ends: its [run export](02-history.md#stored-format-v1), every payload up to 64 KiB inlined as a value |
| Payload above 64 KiB | `blobs/{tenant}/{run}/sha256/{digest}` | Before the batch that references it |
| Test | `tests/{tenant}/sha256/{digest}` | When a test is reserved ([suites](05-testing-and-observability.md#suites-and-the-gate)) |

Embedded, the same keys are paths under `.rewind/`. Every object is immutable: it is written once and
read forever after, so every process caches it without asking again.

### Rewriting the stored names

The stored data takes the vocabulary of November 2026 once, in place, without a new format: every
document stays `"v": 1`, and the API stays `/v1`
([decided](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)).
`rewind server rename-data` does it, offline: the nodes stop, the command runs once against the
deployment's stores, and the new build starts after it. It is rehearsed first on a copy of
production's data.

| Store | What it rewrites |
| --- | --- |
| The partitions, on S3 | Every record in the new names: the events (`schedule_call { action }`, `call_completed`, `call_failed`, the `code` and `action_set` of `run_started`), the statuses (`queued`; a run that was `active` is `running` or `waiting`, from the waits its head holds), the wait kinds (`harness`, `quarantined`), the tasks' `delivery`, `next_delivery_at`, `idempotent` and route `harness`, a fork's `source`, the summaries' mode `test`, the heads' `unfinished`, the signal URL tokens, the outbox items' webhook `type`, the tenant state |
| The archive, on S3 | Every sealed attempt, as a run export in the new names |
| The catalog | `code` and `action_sets`; every pinned action set and workflow version in the new names; the workflows, their versions, their codes and their suites (below); the partitions' `generation` |
| The access store | Grants' principals `member:<id>`; the permissions of roles, tokens and token keys: `actions:read`, `actions:write` with `workflows:write` where `toolsets:write` was, `workflows:write` where `scripts:write` was, `calls:work`, `calls:approve` |
| The subscriptions | Their `types`, with the new types (`run.waiting`, `action.schema_changed`, `secret.expired`) |

- **Digests.** A content-addressed document whose fields are renamed, a pinned action set or a
  workflow version, gets the digest of its rewritten document, and every reference to it is
  rewritten: runs' heads and summaries, `run_started`, versions' `base`, the workflows an action
  set pins. Names and version numbers keep naming what they named (`triage`, `triage@v12`); an
  old digest stops resolving. A code's digest never changes: the code is the text that was sent,
  kept as it is, and the sandbox still resolves the old names it may use
  ([determinism epoch](03-execution.md#determinism-epoch)).
- **Workflows.** Each workflow saved before the rewrite gets an id, and keeps its name and its
  versions, numbered as they were. It belongs to the namespace all its runs came from, or to the
  organization when several namespaces ran it or none did. A title a person gave a code becomes
  its workflow's name where it is a valid name, free in its scope; the command lists the others.
  Every other code that ran gets a workflow in the namespace of its first run, with that code as
  version 1, as a run would have created it; `workflow_codes` maps each code to its workflow.
- **Frozen.** What others hold keeps its value: the prefixes `rewind_wh_` of signal URLs and
  `rewind_tk_` of call tokens, and every handle already stored, with `script` as its origin
  included ([handles](#run-handles)). A signed call's JWT carries `action` beside `tool`
  ([signed calls](04-tools.md#signed-calls)).

## Partitions, owners and nodes

```
 edge (every process) ──router──► owner of the partition ──batch──► SlateDB on S3
                                   │  one writer, fenced by SlateDB's writer epoch
                                   ├─ ready queue ──► executors (V8) of its node, or of a node of the run's epoch
                                   ├─ timer wheel, registry, outbox, sealer
                                   └─ tenant actors homed in the partition
```

- **Partitions.** A deployment has a fixed number of partitions: 64 in production, 1 embedded. A
  namespace's partition is chosen when the namespace is created, from its tenant and name, and
  kept in its row: every run of a namespace lives in that partition, so its idempotency keys,
  lists and counters stay in one place. A run id is a UUID of version 8, laid out as a version 7
  (the time first), whose 16 bits after the variant hold its partition: any process finds a run's
  partition from its id alone, and so from a call's token or a signal URL's token, which carry
  their run's id. A run moved from the tables of the previous design keeps its version 7 id, and
  `legacy_runs` in the control plane says where it lives.
- **One owner per partition.** The owner is the only writer of its partition. Opening a partition
  fences the previous writer: SlateDB raises the writer epoch, and a commit of the old one fails.
  An owner whose commit is refused that way drops the partition at once. Correctness never depends
  on noticing that a node died; only the time it takes another to take over does.
- **Batches.** Everything an owner writes goes through its committer, which writes as one batch
  everything that reached it while the previous batch was being written: atomic, and durable when
  the commit returns. A batch holds the changes of many runs, and costs one write to the bucket. The
  [commit invariant](02-history.md#the-commit-invariant) holds per batch, and the status of each
  run a batch changes is set in it ([above](#a-partition)).
- **Messages after facts.** Once a batch is durable, its fan-out sends what follows from it: a run
  made runnable goes to the ready queue, a timer to the wheel, a call to be sent, a change to the
  edges subscribed to the run, its namespace or its tenant, an outbox item to its projection. A
  message is never the only trace of anything: a node that dies loses its messages, and the next
  owner rebuilds them from the records.
- **Recovery.** An owner that opens a partition reads, once: the runnable runs, which it queues
  with one more unfinished activation each, since an activation may have been running when the
  previous owner died; the timers due before the horizon; the calls in flight, delivered again or
  failed as their policy says; the outbox; the ended attempts not yet sealed; the counters, which
  it reports to their tenants' actors. Nothing is read again on a timer.
- **The timer wheel.** It holds the partition's timers due before the horizon, one hour ahead, and
  sleeps until the earliest. Once per horizon it reads the next hour's timers from the partition.
  That read is the only loop of the run plane.
- **Executors.** A node runs activations on its executor threads, fed by its ready queue: fair
  across tenants (the tenants with runnable runs take turns), and within a tenant the runs with the
  fewest unfinished activations first, then the oldest trigger. An owner sends an activation to its
  own node when the node's build supports the run's [epoch](03-execution.md#determinism-epoch), and
  otherwise, through the node link, to a node whose build does. A tenant can have its own pool of
  nodes ([sandboxing](#sandboxed-nodes)); its activations go there.
- **Nodes.** Every process is both the HTTP edge and a node. Each node holds a Postgres session
  lock on its own id for as long as it lives, and the nodes form a ring in which each waits on the
  lock of the next. When a node dies, its session ends and its lock comes free: the node waiting
  on it moves the dead node's partitions to the others in one transaction (each partition's
  `generation` + 1, `NOTIFY partition_map`), and waits on the next node of the ring. A process that
  dies frees its lock at once; a machine that vanishes, once TCP keepalive gives up, about 10 s. The
  locks live in a database of their own on the same Postgres server (`postgres` by default),
  where nothing else is stored: a watcher blocked on a lock holds back the cleanup of that database
  only. A node whose lock connection fails gives its partitions up at once. A node
  that joins takes some partitions from the others, and one that stops hands its partitions over
  before it exits. Partitions go to nodes by rendezvous hashing, so a change moves only what it
  must.
- **The edge.** The router finds the owner of a run from the partition map, cached and dropped on
  `NOTIFY partition_map`. It reaches an owner in its own process through a channel, and another
  process through the node link: HTTP/2 on the private network, authenticated by the cluster's
  secret. An owner that no longer holds the partition answers `NotOwner`, and the router reads the
  map again.
- **The tenant actor.** Each tenant has one, on the owner of its home partition, chosen from
  the tenant's id. It orders the tenant's admissions, counts its CPU, holds its MCP and API slots
  and hands the calls of its harness actions to its workers' call streams ([quotas](#quotas),
  [04](04-tools.md#call-lifecycle)).

Scaling: more nodes take more partitions, and more executors run more activations. A partition's
writes are bounded by what one batch per write to the bucket carries, and its reads are served by SlateDB's
cache in front of the bucket. More partitions only serve the namespaces created after them.

### The outbox

A run reaches only runs of its own namespace ([namespaces](#namespaces)): the child runs it
starts, the runs it signals, the runs that watch its end, the runs it started attached and
cancels with it. They all live in its partition, so each of these effects is written in the same
batch as the fact that caused it, and needs no message between nodes.

What leaves the partition goes through its **outbox**: the spans exported to OpenTelemetry, the
webhooks, each with its `type` (`run.completed`, `run.waiting`…), and `quota.alert`. An outbox item
is written in the batch of the fact that causes it. Once the batch is durable, the projection
consumes it, and the item is deleted, in a later batch, once exported or sent. Recovery hands the
projections what is left. A failed send's next try is a timer.

## Run handles

A run's id is a UUID, and stays its identity everywhere: the history, the partitions, the archive
and their keys, the signals runs send each other (`rewind:ended:<id>`), the trace. People read,
copy and type its **handle**, `<namespace>/<origin>/<suffix>`
([decisions](08-decisions.md#taken-for-script-titles-and-run-handles-november-2026),
[revised](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)):

- **namespace**: the run's namespace, by its id (a display name changes; an id never does).
- **origin**: what ran, frozen when the run is created: its workflow's name; else its workflow's
  short id, `wf-` and the first 6 characters of its id after `wf_` (`wf-7k2m9q`)
  ([names](04-tools.md#workflow-names)). A fork's is its source's. A description and the first
  line of a code's top comment are shown, and never make an origin.
- **suffix**: the start of the id in Crockford's base32 (lower case), its version and variant
  left out: the 48 bits of its millisecond and the 12 bits after them, 12 characters. The process
  that draws the id writes in those 12 bits its number (6 bits, drawn at random when it draws
  its first) and a counter of its ids within the millisecond (6 bits; a 65th id moves to the
  next millisecond): one process never repeats a suffix. `/` occurs in no part, so a handle
  splits without doubt.

The dashboard shows the namespace part by the namespace's display name where it has one
(`paul/prod_gym/06h0az8t4x7j`), its id in the handle's title; what it links to, copies and
accepts is the handle with the id.

A handle never names two runs of a tenant. It carries its namespace, and every run of a
namespace is created by the owner of the namespace's partition, which serializes its creations
([decisions](08-decisions.md#taken-while-bringing-script-titles-and-run-handles-to-the-run-plane-november-2026)).
With the start's other reads, the owner reads the heads of the namespace's runs whose ids share
the new one's 12 characters, one range of ids for each version (7 for a run moved from the
previous design, 8 for a run of a partition), adds the runs it created that are not durable yet,
and gives the run the shortest suffix, of at least 12 characters, that none of them has. The run
keeps it in its head and its summary, with its origin. Ids of different processes rarely share
those bits; a run of a partition that does gets at least 16 characters, the three after the 12
being its partition's. A run moved from the previous design keeps the origin and the suffix its
tables gave it: `script` or its workflow, and 25 characters (the whole id) when it shared its
first 64 bits with another run of its tenant.

A handle never changes: people copy it into tickets and logs. A handle stored before the
[rewrite of the stored names](#rewriting-the-stored-names) keeps its origin, the title's slug or
`script` included, and a workflow named later does not rename the handles of its runs.

The runs a suffix names are a range of ids for each version: a handle needs no index of its own.
The API takes a handle wherever it takes a run (`GET /v1/runs/{handle}`, percent-encoded in a
path): the edge finds the namespace's partition in the catalog and asks its owner, which reads
the heads of the namespace's runs in those ranges and keeps the one whose suffix has the
handle's length, of the handle's origin. A run's view names its source and the run that started
it (its parent), both of its namespace, by their handles, which its owner reads; by their ids
once they are erased.

## Idempotency of the API

Calls carry a key on the way out ([actions](04-tools.md#routes)). The way in needs the
same protection: a request can succeed while its response is lost, and the caller then
retries without knowing.

| Request | Key | A second request with the same key | Without a key |
| --- | --- | --- | --- |
| Start a run | Unique per namespace | Returns the existing run | A second run starts and repeats every effect |
| Send a signal | Unique per run | Is acknowledged and ignored | A second `approval` stays in the mailbox and approves the code's next `signal("approval")` |
| A run starts a child run | `rewind:{run_id}/{seq}`, the call's key | Returns the run already started | |
| A run signals a run | `rewind:{run_id}/{seq}`, the call's key | Is acknowledged and ignored | |
| A run ends | `rewind:ended:{run id}`, for each run that watches it | Is ignored | |

- A start's key is kept in its namespace's partition, as long as the run is kept. A signal's key
  is kept with the run, for the life of the run, apart from the inbox. It must outlive the
  arrival of the signal, or a late retry would pass.
- The effects of one run on another (a start, a signal, an end) carry these keys, so an effect
  applied twice, by a recovery for instance, changes nothing the second time.
- Keys are run metadata. They never appear in events, so they are neither replayed nor
  exported, in a run export or in a cassette.
- Defaults in the SDK: the agent tools derive `run_workflow`'s key from the model's tool call
  id and the agent's labels, and a signal URL uses the sender's delivery id when there is one.

## Payloads

Payloads (run input, call args and results, run output, signal payloads) are stored in one
of three tiers, by size:

| Size | Where the value lives |
| --- | --- |
| Up to 1 KiB | In the event itself |
| Up to 64 KiB | In the run's payload records, in its partition; inlined in its sealed attempt |
| Above | In a blob under the run, `blobs/{tenant}/{run}/sha256/{digest}`; its size and preview are in the run's payload records |

The thresholds are configurable, and load tests confirm the defaults. Medium payloads stay in the
partition so that a miss of the [instance cache](03-execution.md#sticky-execution) reads them with
the run's history, without a round trip per value.

A payload belongs to its run. The same value carried by two runs is stored twice; within a run it
is stored once. A fork copies the blobs it references from its source under its own run. Erasing a
run deletes its payloads with it, so no cleanup job looks for unreferenced payloads
([decided](08-decisions.md#taken-for-the-object-storage-rewrite-november-2026)).

The responsibilities are split by layer:

| Layer | Knows |
| --- | --- |
| `rewind-core` | A payload's **identity**: the SHA-256 of its canonical JSON, as defined by RFC 8785 (JCS). An event holds either the value (`Payload::Value`) or only the digest (`Payload::Digest`). The determinism check compares digests. |
| `rewind-engine` | **When** to move values: at commit, a payload above the inline threshold leaves the event, which keeps only the digest. Its value goes into the run's payload records with the batch, or to the `BlobStore` before it when it is above the second threshold. When the code receives it, the value is loaded from where it lives. |
| `rewind-format` | **How** a payload is written in the history: `{"value": …}` or `{"digest": "9f2c…"}`. A value is written in its canonical form, so its stored bytes are exactly the bytes its digest is computed on, whatever the JSON library's settings. |
| `rewind-blob` | **Where** large values live: under their run on S3 or any S3-compatible service, `.rewind/blobs/{tenant}/{run}/sha256/ab/<digest>` on disk (atomic writes); every node keeps a local cache in front of the bucket. |

Consequences:

- The canonical form is part of the stored format: a digest written today is compared
  during replays for as long as the run lives. RFC 8785 fixes what "sorted keys, no
  whitespace" leaves to the serializer: keys are ordered by UTF-16 code units, numbers
  are IEEE 754 doubles written as ECMAScript writes them, and string escaping is
  minimal. Golden vectors in `rewind-core`'s tests pin it.
- Payloads are immutable and cacheable forever.
- During replay, recorded command args are compared by digest, so they are never
  downloaded. Only completion results given to the code are fetched, and nodes
  keep a local cache.
- Traces, OTel spans and the dashboard must render without downloading payloads. Size
  and preview are store metadata kept next to the digest in the run's payload records, not
  part of the domain model ([decided](08-decisions.md#taken-after-the-layering-november-2026)).

## Tenancy, namespaces, identity and access

Four questions are kept apart:

| Question | Answered by |
| --- | --- |
| Who calls? | An API key, a member's session with WorkOS, an end user's token, or a person's connection |
| Which tenant? | The key's tenant, the member's organization, the token's, or the one a connection's request names (`Rewind-Organization`) |
| Which namespace? | The token's; for the customer's keys, members and connections, the `Rewind-Namespace` header |
| What may the caller do? | Rewind alone: the caller's grant, its role and the role's permissions; for a token, the permissions it was given |

WorkOS holds identity and nothing else: users, organizations, memberships, invitations and
the waitlist. It never says what someone may do. Rewind configures no role or permission there,
and never reads the role of a membership nor the `role`, `roles` and `permissions` claims of an
access token ([decided](08-decisions.md#taken-for-accounts-organizations-and-access-november-2026)).

### Tenants and organizations

A tenant is a WorkOS organization: the tenant's id is the organization's id (`org_01J…`, which
fits the tenant ids' alphabet). Rewind copies no WorkOS user, organization or membership: WorkOS is
their only source. A tenant's record (quotas, retention) is created with the organization, or
at the first request of a member of an organization that has none.

Tenant is the engine's word: the code, the storage and this spec where it describes them say
tenant, and the actor that serves one is its tenant actor (`TenantState`). The API, the SDK, the
CLI, the dashboard and the docs say organization: a field holding its id is `organizationId`
(`Identity`, `ApiKey`), a path names it `{organization}`, and the operator's flags say
`--organization-pool` and `--admin-organization`.

The tenant is part of every storage key:

| Data | Key |
| --- | --- |
| Runs, their metadata, records and summaries | `(tenant, id)`, in the partition of their namespace |
| Idempotency keys of runs | `(tenant, namespace, key)`, in the namespace's partition |
| Namespaces | `(tenant, namespace)`; their counters in their partition |
| Code, action sets | `(tenant, digest)` |
| Workflows | `(tenant, id)`, their names unique per `(tenant, namespace)`, `*` for the organization's |
| Workflow versions | `(tenant, workflow, version)`, found by `(tenant, digest)` too |
| Payloads | Under their run; blobs at `blobs/{tenant}/{run}/sha256/…` |
| Sealed attempts | `runs/{tenant}/{run}/…` |
| Secrets, quotas, keys, tokens, roles, grants | Per tenant |

- **Quotas**: per namespace and per tenant ([quotas](#quotas)).
- **Fairness**: a node's ready queue takes turns among the tenants with runnable runs, then
  takes the chosen tenant's run with the fewest unfinished activations, the oldest trigger first.
  A large tenant cannot starve the others, and an end user's share of its tenant's queue is bounded
  by its namespace's `activeRuns`.

### Namespaces

A tenant is a customer of Rewind, and serves its own users, its **end users**. A namespace
isolates one end user: the runs started for them and everything that follows from those runs.
An agent acting for Alice holds a token of Alice's namespace and cannot see or reach anything of
Bob's, whatever workflow its model writes.

- **Name.** The customer's own id for the end user: 1 to 128 letters, digits, `-`, `_`, `.`, `:`
  and `@`, case-sensitive, not starting with `rewind:`, which Rewind keeps for its own names.
  `*` is not a name: it stands for the tenant in `usage`, `quota_alerts`, `secrets` and
  `workflows`. A namespace's name appears in webhooks, the dashboard and the service's logs,
  so an opaque id is better than an email.
- **Display name.** The customer's label for the end user (`Acme Corp`), optional, set and
  removed with `PATCH /v1/namespaces/{name}`. Trimmed; at most 128 characters, none of them a
  control character; not unique. The dashboard, the CLI and the agent tools show it beside the
  name wherever it is set, and `GET /v1/namespaces?prefix=` finds a namespace by its start, case
  aside. The name stays the key everywhere: paths, `namespace:` in queries, `Rewind-Namespace`,
  tokens and webhooks.
- **Created at first use**: when a token is minted for it, when a signed token naming it is first
  used, or when a run is created in it, with the tenant's default limits. `PUT
  /v1/namespaces/{name}` creates it ahead, or sets its exceptions.
- **What belongs to a namespace**: its runs, with their histories, signals, idempotency keys,
  tasks of calls, signal URL tokens, payloads, summaries and counters; its
  [workflows](01-product.md#workflows), the ones its runs created and the ones it forked from the
  organization's, with their versions and suites, until a member promotes one; the end user's
  [secrets](04-tools.md#secrets); its usage and alerts; its tokens. A fork of a run stays in its
  source's namespace, and nothing a run does reaches another namespace.
- **What stays the tenant's**: action sets; the organization's workflows, generic, which the runs
  of every namespace execute as they are and which a namespace forks to change them; the
  organization's secrets; its signing key; subscriptions, since webhooks go to the customer, each
  naming its namespace; members, roles, keys and token keys; the storage of code, deduplicated per
  tenant, which a token reads only through the workflows it sees (its namespace's and the
  organization's).
- **The customer names the namespace** with the header `Rewind-Namespace`. Creating in a
  namespace requires it: starting a run, `/v1/check` and `/v1/test`, which apply the namespace's
  limits (400 without it), and forking a workflow into it. Creating a workflow names its namespace
  with it too; without it, the workflow is the organization's. There is no default namespace. A
  fork of a run takes its source's namespace. Reading without the header covers every namespace of
  the tenant; with it, that namespace only, and the organization's workflows, which its runs may
  execute: a run or a workflow of another namespace is 404, for a fork, a signal or a cancellation
  too.
- **An end user's token fixes the namespace.** A header naming another is 403; anything of
  another namespace is 404; lists hold its namespace only, and the organization's workflows;
  `calls:work` receives only the calls of its namespace's runs.
- **Deleting a namespace** erases an end user ([retention](#retention-and-deletion)).

### Quotas

Quotas protect the platform and the customer against an end user who abuses the customer's
product. Six limits, each optional:

| Quota | Counts | Above it | Alert |
| --- | --- | --- | --- |
| `cpuPerMonth` | The CPU time of the isolates running the namespace's runs in the calendar month (UTC), replays excluded | Nothing is refused, nothing is paused | At 80 % and 100 % |
| `activeRuns` | The runs active now: queued, running or waiting | Starting or forking a run: 429 | At the first refusal of the month |
| `runsPerMonth` | The runs created in the month, forks included | 429 until the 1st of the next month | At 80 % and 100 % |
| `workflows` | The distinct workflows with at least one run kept in the namespace (active, ended or archived), its own and the organization's | 429 for a run of a workflow that has none in the namespace, a new one included, until a run is erased | At 80 % and 100 % |
| `maxTtl` | The TTL a run may ask for; the default TTL is lowered to it | 400 `invalid-request` | |
| `maxCodeLines` | The lines of the code sent | 422 `invalid-code` with a diagnostic, also from `/v1/check` and `/v1/test` | |

Three levels:

- **The tenant's defaults** (`namespace_defaults`), set by the customer (`namespaces:write`), apply
  to every namespace.
- **A namespace's exceptions**, set by the customer: a paying end user, or a blocked one
  (`activeRuns: 0`). Per quota, a namespace's limit is its exception if it has one, else the
  default.
- **The tenant's ceiling** (`quotas`), set by the operator: the customer's offer. For
  `cpuPerMonth`, `activeRuns`, `runsPerMonth` and `workflows` it bounds the tenant's totals, the
  sum over its namespaces; `maxTtl` and `maxCodeLines` bound every namespace.

A start passes its namespace's limit and the tenant's ceiling. The tenant actor admits the
tenant's starts one at a time from its queue, so two concurrent starts cannot take the same last
place; no lock is taken. A refusal (`quota-exceeded`) says which quota, at which scope (`namespace`
or `organization`), the limit, the use and when it frees (`resetsAt`, with `Retry-After` for a
monthly quota), in words written for a model that never name another namespace. A retried
request with its idempotency key still returns its run. A run admitted is `queued` until its first
step commits: `queued` never means waiting for admission.

- **Counting.** Each partition counts, per tenant and namespace, its active runs, the runs
  created in the month, the runs it keeps per workflow (`workflows`: a run adds one to its
  workflow's count, erasing it removes one) and the CPU time per UTC day, in the batch of what it
  counts: the counts commit with their facts, and cannot drift from them. After a batch that moved
  them, the owner sends the counts to the tenant actor, as absolute values: a message lost
  is corrected by the next, and a new tenant actor asks every owner once.
- **Admission.** The tenant actor answers each start (`AdmitStart`) from the sum of the
  counts the owners reported, plus the starts it admitted that no report includes yet. It knows
  the namespaces' limits and the tenant's ceiling from the control plane, cached and dropped on
  change. A start above a limit is refused at once (429); it is never held in a queue.
- **CPU time.** The runtime reports the CPU time of each turn of the code in its isolate, which its
  watchdog measures anyway. The engine counts the turns that execute and leaves out those of a
  replay, which are the cost of the instance cache, not the end user's. The owner adds them to the
  namespace's day in the batch of the activation, so a node that dies loses none of them.
- **CPU time is not in the history.** It measures what the service spent, and differs from one
  replay to the next: it is metering, like service telemetry, and never changes what a run does.
- **Alerts.** A threshold crossed is recorded once per tenant, namespace (or `*`), quota, month
  and threshold, by the tenant actor, in its home partition, with an outbox item that sends
  it to the tenant's subscriptions as a webhook of type `quota.alert` (`namespace`, `quota`,
  `threshold`, `used`, `limit`, `period`; `namespace` is null for the tenant's ceiling), and the
  dashboard lists it. The customer decides what follows: telling the end user, raising their
  limits, or blocking them.

### Who calls

Every request to `/v1` carries a Bearer token, and nothing else authenticates it (no cookie):

- **An API key**, `rewind_sk_…`, for harnesses, the SDK, its worker and the CLI. Keys are stored
  hashed; each belongs to a tenant, has a name and records who created it.
- **A WorkOS access token**, for the dashboard: a JWT signed RS256, checked locally against
  WorkOS's public keys (JWKS, cached, fetched again for an unknown `kid`): the issuer
  `https://api.workos.com`, the deployment's `client_id`, the expiry with 30 s of leeway. `sub`
  is the person, `sid` the session, `org_id` the tenant. A token without `org_id` reaches only
  the onboarding endpoints (who am I, create an organization, the invitations sent to me).
- **An end user's token**, for an agent acting for one end user, in one of two forms the
  customer chooses:
  - **Opaque**, `rewind_nt_…`, minted by the customer's backend with
    `POST /v1/namespaces/{name}/tokens` (`tokens:write`): one namespace, its permissions, optional
    labels, an expiry (1 hour by default, 30 days at most). Stored hashed, as keys are; its
    SHA-256 is its id.
  - **Signed by the customer**: a JWT signed with a key whose public part the customer registered
    (`POST /v1/organization/token-keys`, `tokens:write`), EdDSA (Ed25519), ES256 or RS256. Rewind
    names the key `rk_…`, and the token's header gives that name as `kid`. Claims: `sub` the
    namespace, `aud` `rewind`, `iat` and `exp` (at most 30 days apart), and optionally
    `permissions`, `labels` and `jti`. The customer mints them without calling Rewind; a key
    records the permissions its tokens may carry.

- **A person's connection**, for an MCP client acting for a person, Claude above all
  ([connections](#connections)): an access token `rewind_ct_…`, valid one hour, that the client
  renews with its refresh token. It names no organization: each request names one with
  `Rewind-Organization`.

  A Bearer starting with `rewind_nt_` is an opaque token, one starting with `rewind_ct_` a
  connection's; a JWT whose `kid` starts with `rk_` is a customer's token; any other JWT is a
  WorkOS access token. `DELETE
  /v1/namespaces/{name}/tokens` revokes the namespace's opaque tokens and refuses its signed
  tokens issued before that moment (`tokens_revoked_before`); `DELETE /v1/tokens/{id}` revokes
  one; deleting a token key invalidates every token it signed. Every request reads the token's
  state again: a revocation applies from the next request, and closes an event stream opened
  with the token. A daily job deletes an opaque token a day after it expired or was revoked.

The dashboard gets its access token from `/auth`, the only routes that read a cookie
([07](07-code-architecture.md#sign-in-auth)). People sign in with GitHub or with a 6-digit code
sent by email (WorkOS Magic Auth), on the dashboard's own pages. Sign-ups are closed: an
unknown person joins the WorkOS waitlist, and the operator's approval sends an invitation. A
member of an organization invites others by email.

The embedded server is the exception: its socket takes no token, and its one caller is the
operator of the tenant `local` ([embedded server](07-code-architecture.md#embedded-server-and-cli)).

### Connections

A person lets an MCP client act for them: Claude above all, on claude.ai, Desktop, mobile and
in Claude Code ([01](01-product.md#claude-as-the-harness)). Rewind is the OAuth authorization
server of its own MCP endpoint ([07](07-code-architecture.md#mcp-and-oauth)), and a
**connection** is what the person's consent creates: the person's key, outside every
organization, opening the organizations they tick.

- **One per person and client.** claude.ai is one client (Desktop and mobile share its
  connectors), Claude Code another. Each connection has its own tokens and is revoked alone.
- **The organizations it opens.** At consent, the person ticks organizations where they hold a
  grant, the one open in the dashboard ticked in advance, and gives each an access: `read`
  (`runs:read`, `actions:read`, `namespaces:read`, `secrets:read`) or `run` (`runs:start`,
  `runs:control` and `secrets:use` too). The connection gets a grant in each one, for the principal `connection:cn_…`: the
  built-in role `reader` for `read`, `harness` for `run`, with the labels chosen (`via: claude`
  by default). An organization the person joins later is not added by itself.
- **Never more than the person.** In the organization a request names, the connection may use
  the permissions of its grant's role that are among the seven above and that the person's own
  grant there gives now; it sees only the runs carrying the labels of both grants. Without a
  grant of the connection or of the person there, it has no access (403). Nothing else is
  pinned: the namespace is named per request, as a key names it, and the action set per call.
- **Never administers.** Whatever the person's role, a connection holds no other permission than
  those seven: no members, roles, keys, tokens, action sets nor workflows (neither `actions:write`
  nor `workflows:write`), and it writes no secret. It reads secrets' names and names them for its
  runs to use, never a value.
- **Tokens.** An access token `rewind_ct_…` lives one hour. A refresh token `rewind_rt_…` is
  replaced at each use, and lapses after 30 days unused. Both are stored hashed. A refresh token
  used twice revokes the connection, and so does an authorization code exchanged twice.
- **Changed and revoked at once.** The person changes the organizations, accesses and labels of
  a connection without reconnecting the client, or revokes it (`/v1/connections`). Whoever holds
  `members:manage` in an organization removes it from a connection
  (`/v1/organization/connections`); removing a member removes their connections' grants there.
  At each refresh, the server checks the person's memberships in WorkOS and removes the
  organizations they left. A daily job deletes a connection 30 days after its last refresh token
  lapsed. Every request reads the connection, its grants and the person's again.
- **The organization of a request.** A connection's request names its organization with the
  header `Rewind-Organization` (400 without it on the organization's data; 403 for an organization
  the connection does not open). `/mcp` takes it as the agent tools' `organization` argument.
  Other callers may repeat their own organization there; naming another is 403.

### What the caller may do

```
principal (a member, a key, an invitation, a connection) ──► grant (role, labels) ──► role ──► permissions
                                                                               │
endpoint: require(permission), then the grant's labels on what it touches ◄────┘
```

- **Permissions** are Rewind's catalog, one per kind of action, defined in `rewind-api`. Every
  endpoint requires one. A new endpoint either uses one of them or adds one to the catalog.
- **Roles** are sets of permissions. Four are built in and never change; an organization
  creates its own from the catalog. A role's name is 1 to 64 lowercase letters, digits, `-`
  and `_`, and never a built-in name or `operator`.
- **A grant** gives one role, and optionally allowed labels, to one principal in one tenant: a
  member (`member:user_…`, by their WorkOS id), an API key, an invitation not yet accepted, or a
  person's connection (`cn_…`). A principal has at most one grant per tenant. A grant whose role
  no longer exists gives no permission.

| Permission | Lets the caller | owner | harness | worker | reader |
| --- | --- | --- | --- | --- | --- |
| `runs:read` | Read runs, their statistics and fingerprints, a run's state, transcript, events, event stream and export, the change stream, the workflows with their versions, suites and tests, the action statistics, payloads | yes | yes | | yes |
| `runs:start` | Start and fork runs; `/v1/check`, `/v1/test` | yes | yes | | |
| `runs:control` | Send signals, cancel | yes | yes | | |
| `runs:quarantine` | Release a quarantined run or give it up | yes | yes | | |
| `runs:delete` | Erase a run | yes | yes | | |
| `actions:read` | List action sets, read their declarations, search their actions | yes | yes | | yes |
| `actions:write` | Register an action set | yes | yes | | |
| `workflows:write` | Create a workflow; name it, describe it, give it a version or tests; promote a namespace's workflow to the organization; fork an organization's workflow into a namespace, and pull a fork's source ([workflows](01-product.md#workflows)) | yes | yes | | |
| `secrets:read` | List the secrets of the organization and of the namespaces, their metadata and usage, never a value; read the signing keys' ids | yes | yes | | yes |
| `secrets:write` | Put and delete secrets | yes | yes | | |
| `secrets:use` | Name an organization's secret for a run or a workflow version to use ([secrets](04-tools.md#secrets)) | yes | yes | | |
| `calls:work` | Receive the calls of harness actions on a call stream, defer, complete, fail | yes | yes | yes | |
| `calls:approve` | Approve or reject a call waiting for approval or parked | yes | yes | | |
| `subscriptions:read` | List the webhook subscriptions | yes | yes | | yes |
| `subscriptions:write` | Create and delete them | yes | yes | | |
| `members:read` | List the members, their grants and the invitations | yes | yes | | yes |
| `members:invite` | Invite, resend and revoke invitations | yes | | | |
| `members:manage` | Change a member's grant, remove a member | yes | | | |
| `roles:read` | List the roles | yes | yes | | yes |
| `roles:write` | Create, change and delete the organization's roles | yes | | | |
| `api_keys:read` | List the tenant's keys | yes | | | |
| `api_keys:write` | Create and revoke keys | yes | | | |
| `organization:write` | Rename the organization, set its default role | yes | | | |
| `namespaces:read` | List the namespaces, their limits, usage, alerts and tokens in use | yes | yes | | yes |
| `namespaces:write` | Set the default limits and a namespace's exceptions, delete a namespace | yes | yes | | |
| `tokens:write` | Mint and revoke end users' tokens; register and delete token keys | yes | yes | | |

The roles `harness`, `worker` and `reader` give exactly what the key roles of the same names
gave before roles became data. `workflows:write` is in the built-in roles that registered action
sets or wrote workflows before it existed, `owner` and `harness`, and the rewrite of the stored
names gives it to every role of an organization that held either
([rewriting](#rewriting-the-stored-names)). A grant applies in every namespace of its tenant: the
customer holds all rights on its namespaces, and there is no grant per namespace.

Rules, enforced by the server:

- **Labels.** A grant limited to labels sees only the runs carrying all of them: other runs are
  missing (404), lists filter them, and what the principal creates (runs, workflows,
  subscriptions) gets them; a contradicting label is refused (403). With `calls:work`, it receives
  only the calls of the runs carrying them, and the token of another run's call is answered as a
  call that no longer exists (410): one tenant's environments (`env=dev`, `env=prod`) each run
  their own worker, though they register the same content-addressed action set. It sees, writes
  and uses only the organization's secrets carrying all its labels, and the secrets it writes get
  them. It reads and uses a namespace's secrets as any grant does, and never writes one (403): a
  namespace's secret serves the end user's runs whatever their labels. The same holds for a token
  limited to labels. Such a grant cannot read statistics ([05](05-testing-and-observability.md#statistics)), nor use the permissions
  `members:invite`, `members:manage`, `roles:write`, `api_keys:*` and `organization:write` (403).
- **Workflows.** A workflow belongs to a namespace, seen only in it, or to the organization, seen
  from every namespace ([workflows](01-product.md#workflows)). A namespace's runs execute the
  organization's workflows as they are; a member or a key with `workflows:write` forks one into a
  namespace to change it there, and promotes a namespace's workflow to the organization. A
  workflow created under a grant limited to labels carries those labels, and so does each of its
  versions: the same code sent under other labels, or organization-wide, is another workflow, and
  sent again under the same labels finds it. Such a principal only lists, reads, changes, forks and
  pins the workflows carrying its labels, and a run it starts takes them as its **allowed
  labels**: the code of that run and of the runs it starts only reach runs carrying them
  ([workflows](03-execution.md#workflows-across-runs)).
- **No one gives more than they hold.** Creating a key, inviting someone, setting a grant or
  writing a role requires holding every permission of the role given. Opening an organization to
  a connection requires holding the permissions of the access given. Changing or removing a
  principal's grant also requires holding every permission of its current role.
- **An organization keeps an owner.** A write that would leave a tenant without a grant of the
  role `owner` is refused, atomically in the store.
- **A role in use stays.** A role of the tenant cannot be deleted while a grant points to it.
  Changing a role changes at once what every principal holding it may do.
- **No grant, no access.** A member without a grant gets 403 on the tenant's data. Members get
  their grant at their first session in the organization: the pending grant of the invitation
  they accepted, else the organization's default role (`reader` unless changed). The creator of
  an organization gets `owner`.
- **Every request reads the grant.** A grant removed or narrowed applies from the next request,
  without waiting for the access token to expire. An event stream opened with an access token
  is closed at the token's expiry, and the dashboard resumes it with `from`.
- **Connections.** A connection's grant gives `reader` or `harness`, bounded by the seven
  permissions of its accesses and by the person's own grant at each request
  ([connections](#connections)). Only its person changes it; an administrator removes it.
- **The operator.** The key given with `--admin-key` has the role `operator`: every permission
  in its own tenant, the keys of every organization (`POST /v1/api-keys` with an
  `organizationId`), the ceiling of every organization
  (`PUT /v1/organizations/{organization}/quotas`), and the stop of the embedded server.
  `operator` is not a role of any organization; only an operator creates an operator key.
- **Tokens.** A token holds the permissions it was given, among `runs:read`, `runs:start`,
  `runs:control`, `runs:quarantine`, `runs:delete`, `actions:read`, `secrets:read`,
  `secrets:write`, `secrets:use`, `calls:work` and `calls:approve`, by default `runs:read`,
  `runs:start`, `runs:control` and `actions:read`, and only within its namespace. It never holds
  another permission: never `workflows:write`, so an end user's token creates workflows only
  through the runs it starts, and never names, versions, promotes, forks or pulls one. Whoever
  mints a token or registers a token key must hold every permission it gives; a signed token that
  asks for a permission its key may not give is refused (401). A token's labels work as a grant's.
  `secrets:read` and `secrets:write` in a token cover its namespace's secrets only; `secrets:use`
  lets it name the organization's secrets its labels allow. A token uses its own namespace's
  secrets without it.

## Retention and deletion

Active runs are never touched. Defaults, configurable per tenant:

| Data | Kept |
| --- | --- |
| An ended run: its summary, its sealed attempts and its payloads | 1 year after it ended. The summary stays in its partition's index; each attempt's [run export](02-history.md#stored-format-v1) is in the archive, loaded on demand |
| An end user's opaque token | Until 24 h after it expired or was revoked; a namespace's erasure revokes its tokens |
| A test | As long as a workflow's suite holds it or held it, which covers every test a version passed; one no workflow references is deleted 24 h after it was stored. A recorded test outlives its run, and is erased with its namespace |

- **Sealing.** When an attempt ends, the sealer writes its run export to the archive, every payload
  up to 64 KiB inlined, then deletes the attempt's history, inbox, tasks, payload records and
  signal keys from the partition, in a later batch. The run's head and summary stay in the index.
  Until the seal is durable, the run is read from its partition; after it, from the archive. A
  sealed attempt never changes, and every process caches it.
- **Retention is a timer.** The batch that seals a run writes the timer of its retention's end.
  When it fires, the owner erases the run.
- `DELETE /v1/runs/{id}` erases a run at any time: its sealed attempts and its blobs (the run's
  prefix in the archive), then its records, summary and counts in its partition, in one batch.
  Call arguments can hold personal data.
- `DELETE /v1/namespaces/{name}` erases an end user and answers 202. The namespace stops taking
  runs and tokens (409 `namespace-deleting`), and its tokens are revoked. The change reaches the
  owner of its partition, which cancels its active runs through the normal path and erases its
  ended runs as `DELETE /v1/runs/{id}` erases one; once the partition holds nothing of the
  namespace, the control plane erases the tests recorded from it, its workflows with their
  versions and suites (its forks included; the organization's workflows its runs executed stay),
  its secrets and their reads, and the namespace. A version keeps the digest of an erased test in
  its document; the next version's gate leaves the test out
  ([suites](05-testing-and-observability.md#suites-and-the-gate)). The tenant's own counts keep
  what the namespace consumed: deleting an end user gives nothing back to the tenant's ceiling. The
  batch that erases the namespace's counters adds, hour by hour, what it consumed (runs created,
  started and ended, their durations, CPU time, the runs of each organization workflow) to the
  tenant's `rewind:erased:<partition>`, counted in the tenant's totals and its ceiling and shown
  as no namespace; its failures, its actions and its own workflows go with it.
- Payloads go with their run, so nothing looks for payloads no run references.
- Tests are blobs of the `BlobStore` under the tenant. A test's row is reserved before its blob
  is written; the daily cleanup marks the rows no workflow references, deletes their blobs, then
  removes the rows still marked
  ([decided](08-decisions.md#taken-while-fixing-the-review-of-the-engine-and-the-stores-november-2026)).
- Daily jobs of the control plane, scheduled once a day: the tokens and connections to delete, the
  tests no workflow references.

## Security

A workflow's code is untrusted (model-generated). It has no ambient authority: every
capability comes from the action set, and secrets never enter the isolate
([secrets](04-tools.md#secrets)). A run reads only the secrets its start named, checked
against the rights of whoever named them.

A run's seed keys its random generators, `crypto` included. It is protected like the
payloads.

A deployed server also sends requests where a tenant says: to its MCP servers and APIs (from the
edge at registration, from the node of the run's partition at each call) and to its webhook
subscriptions. None goes to the internal network: all go through the egress checks of the `http`
action, a name resolved once, private, loopback, link-local and metadata addresses refused, the
connection to the checked address ([MCP servers and APIs](04-tools.md#where-mcp-servers-and-apis-may-be)).
Only the operator opens the way: to internal MCP servers and APIs by host or network
(`--allow-internal-addresses`), whose answers are then never quoted to the tenant, and to webhook
receivers on the private network (`--allow-private-webhooks`). The embedded server, for one
local developer, reaches any address.

What Rewind sends to a tenant's servers proves who calls in one of two ways
([authenticating calls](04-tools.md#authenticating-calls)): a secret the run uses, or a JWT
signed with the tenant's Ed25519 key, whose public part anyone reads at
`{public-url}/organizations/{organization}/jwks.json`. One tenant's key never signs for another,
and a JWT names its audience: a server that receives one cannot replay it against another.

### Sandboxed nodes

A V8 isolate is not a hard boundary against V8 vulnerabilities. Every node may run code written by
a model, so every node is sandboxed.

The same binary runs every process. Options at start change what a node takes on:

| Start | Runs | V8 |
| --- | --- | --- |
| `rewind server` | The HTTP edge, the owners of its partitions, the tenant actors homed there, executors | Yes |
| `rewind server --executors-only` | Executors only: activations sent by owners, for the determinism epochs of its build | Yes |
| `rewind server --pool <name>` | A tenant's own pool: the owners send that tenant's activations there | Yes |

- Nodes run under gVisor (`runsc`). A sandbox runs its own network stack: it takes over the
  network namespace it starts in, and reaches no DNS server on the host's loopback, such as
  Docker's: a node's DNS server stands on its network.
- A tenant that needs it gets its own pool of executors, which the operator gives it with
  `--organization-pool <organization>=<pool>` (or `REWIND_ORGANIZATION_POOLS`) on every node.
  Trusted tenants share the default pool.
- Nodes reach the outside only through the egress proxy, and have no route to the cloud
  metadata endpoint ([egress](04-tools.md#the-http-action-and-egress)).
- Their container has a memory limit. The sandbox bounds each instance's heap and array
  buffers, but V8 runs some built-ins to their end once the limit is reached, and aborts the
  process on a few results too large for it
  ([limits](03-execution.md#resource-limits)): the container bounds what is left. A node that
  aborts loses nothing: another owner takes its partitions over, and the run whose activation
  killed it is quarantined once it has done so five times.
- After an upgrade of V8 or ICU, a few nodes of the previous build stay up with
  `--executors-only` for the runs of the previous [epoch](03-execution.md#determinism-epoch),
  until those runs end, at their TTL.

Three walls stand between two tenants: V8, gVisor, and separate pools.
