# 6. Storage, scale and security

## Backends

Storage sits behind the `JournalStore`, `BlobStore`, `ReadModel` and `SecretStore` ports of
`ptc-engine`.

| Use | Journal, read model, secrets | Large payloads |
| --- | --- | --- |
| Production (`ptc-server`) | Postgres (`ptc-store-postgres`) | S3 or any S3-compatible service (`ptc-blob`) |
| Embedded (`ptc server --embedded`, local dev, tests) | Turso Database (`ptc-store-turso`, the `turso` crate) | Local files (`ptc-blob`) |
| Engine unit tests, mock mode | In memory | In memory |

Every adapter of `JournalStore`, `BlobStore`, `ReadModel` and `SecretStore` passes that port's
conformance suite, shipped by `ptc-engine` behind the `conformance` feature.

## Tables

Every key starts with the tenant ([tenancy](#tenancy-and-authentication)).

```
tenants    (id PK, data_key, active_run_quota, retention)   -- data_key encrypted by the master key
api_keys   (hash PK, tenant, role, allowed_labels, revoked_at)
scripts    (tenant, digest, source, transpiled, source_map)   PK (tenant, digest)  -- only scripts that compile
toolsets   (tenant, digest, spec, declarations)               PK (tenant, digest)
runs       (tenant, id, script, toolset, status, epoch, history_len, blocked_on, runnable_at,
            expires_at, quarantine, failures, lease_owner, lease_until, fence, sticky_worker,
            parent, labels, trace_context, idempotency_key, created_at, updated_at, ended_at,
            archive)
            PK (tenant, id), UNIQUE (tenant, idempotency_key)
history    (run_id, idx, recorded_at, event)              PK (run_id, idx)
inbox      (id, run_id, event, received_at)
signal_keys (run_id, key)                                  PK (run_id, key)
timers     (run_id, seq, fire_at)                          index on fire_at
tool_tasks (run_id, seq, tool, route, args, policy, server_concurrency, state, attempt, token,
            lease_owner, lease_until, next_attempt_at, parked_reason, created_at)
webhook_tokens (token PK, run_id, name)
payloads   (tenant, digest, bytes, preview, value, unreferenced_since)
                                                           PK (tenant, digest)  -- value up to 64 KiB
payload_refs (tenant, digest, run_id)                      PK (tenant, digest, run_id)
secrets    (tenant, owner, ciphertext)                     PK (tenant, owner)   -- owner: a toolset or a run
subscriptions (tenant, id, url, secret, events)
feed       (id, consumer, run_id, fence, from_idx, to_idx, lease_owner, lease_until)
meta       (key PK, value)                                 -- schema version; virtual time (embedded)

-- read model, written only by the projector
run_summaries (run_id PK, script, toolset, status, labels, started_at, ended_at,
               last_activity_at, tool_calls, errors, history_len)
tool_calls    (run_id, seq, tool, callsite_line, scheduled_at, completed_at, outcome,
               error_kind, args_digest, result_digest)      PK (run_id, seq)
```

`event` columns hold the `ptc-format` v1 JSON of the event. `labels`, `trace_context`,
`idempotency_key` and `parent` are run metadata: stored with the run, never in its events
([05](05-testing-and-observability.md#run-metadata)).

`epoch` and `expires_at` are copies of the values in `run_started`, kept for queries:
workers pick runs by epoch, and the scheduler picks the runs past their deadline.
`quarantine` holds why a run is
[quarantined](03-execution.md#non-determinism-and-quarantine) (non-determinism or repeated
failures, the reason, and the last build that retried it), and `failures` counts its
activations that failed in a row. A tool task keeps the route and policy of its tool, copied from
the pinned toolset: the claim enforces the concurrency limits from them. `archive` points to the
journal document of an archived run, in the `BlobStore`.

## Leases, fencing, workers

- **Workers** hold nothing a run needs: their instance cache only saves a replay. They pick
  runnable runs with `FOR UPDATE SKIP LOCKED` (Postgres), among the epochs they support and
  outside quarantine, with the [fairness](#tenancy-and-authentication) between tenants and
  the priority of the [sticky worker](03-execution.md#sticky-execution). Acquiring a lease
  increments `fence`. The lease lasts 30 s, and the worker renews it about every 10 s while
  it works on the run, during the replay as during the execution: a long replay keeps its
  lease, and a dead worker frees the run at most 30 s after its last renewal.
- A commit only succeeds if the fence still matches, and `PK (run_id, idx)` is a second
  guard. A worker that lost its lease (GC pause, network partition) can never corrupt a
  history.
- The **scheduler** polls due timers the same way and pushes `timer_fired` into inboxes.
  It also polls the runs past their deadline and pushes
  `cancel_requested { reason: "ttl" }`.
- The **projector** claims feed items the same way, applies them to the read model and
  the OTel export, then deletes them.
- A terminal commit clears the run's pending timers and tool tasks.

Scaling path:

1. More workers.
2. Partition `history` and `inbox` by `run_id`; archive the histories of finished runs
   ([retention](#retention-and-deletion)).

Sharding over several Postgres clusters and a columnar read model are out of 0.1
([plan](09-roadmap.md#out-of-01)).

## Idempotency of the API

Tool calls carry a key on the way out ([tools](04-tools.md#routes)). The way in needs the
same protection: a request can succeed while its response is lost, and the caller then
retries without knowing.

| Request | Key | A second request with the same key | Without a key |
| --- | --- | --- | --- |
| Start a run | Unique per tenant | Returns the existing run | A second run starts and repeats every effect |
| Send a signal | Unique per run | Is acknowledged and ignored | A second `approval` stays in the mailbox and approves the script's next `signal("approval")` |

- A signal's key is kept in `signal_keys` for the life of the run, not in the inbox. It
  must outlive the delivery of the signal, or a late retry would pass.
- Keys are run metadata. They never appear in events, so they are neither replayed nor
  exported in cassettes.
- Defaults in the SDK: the agent tools use the model's tool call id for `run_script`,
  and a webhook uses the sender's delivery id when there is one.

## Payloads

Payloads (run input, tool args and results, run output, signal payloads) are stored in one
of three tiers, by size:

| Size | Where the value lives |
| --- | --- |
| Up to 1 KiB | In the event itself |
| Up to 64 KiB | In the `payloads` table (Postgres, Turso) |
| Above | In S3, or in files for the embedded server; its size and preview are in `payloads` |

The thresholds are configurable, and load tests confirm the defaults. Medium payloads stay
in the database so that a miss of the [instance cache](03-execution.md#sticky-execution) does
not cost an S3 round trip.

The responsibilities are split by layer:

| Layer | Knows |
| --- | --- |
| `ptc-core` | A payload's **identity**: the SHA-256 of its canonical JSON, as defined by RFC 8785 (JCS). An event holds either the value (`Payload::Value`) or only the digest (`Payload::Digest`). The determinism check compares digests. |
| `ptc-engine` | **When** to move values: at commit, a payload above the inline threshold leaves the event, which keeps only the digest. Its value goes into the `payloads` table with the commit, or to the `BlobStore` before it when it is above the second threshold. On delivery to the script, the value is loaded from where it lives. |
| `ptc-format` | **How** a payload is written in the journal: `{"value": …}` or `{"digest": "9f2c…"}`. A value is written in its canonical form, so its stored bytes are exactly the bytes its digest is computed on, whatever the JSON library's settings. |
| `ptc-blob` | **Where** large values live: `{tenant}/sha256/{digest}` on S3, `.ptc/blobs/{tenant}/sha256/ab/<digest>` on disk (atomic writes); workers keep a local cache in front of S3. |

Consequences:

- The canonical form is part of the stored format: a digest written today is compared
  during replays for as long as the run lives. RFC 8785 fixes what "sorted keys, no
  whitespace" leaves to the serializer: keys are ordered by UTF-16 code units, numbers
  are IEEE 754 doubles written as ECMAScript writes them, and string escaping is
  minimal. Golden vectors in `ptc-core`'s tests pin it.
- Payloads are immutable, deduplicated within a tenant, and cacheable forever.
- During replay, recorded command args are compared by digest, so they are never
  downloaded. Only completion results delivered to the script are fetched, and workers
  keep a local cache.
- Traces, OTel spans and the dashboard must render without downloading payloads. Size
  and preview are store metadata kept next to the digest in the `payloads` table, not part
  of the domain model ([decided](08-decisions.md#taken-after-the-layering-november-2026)).

## Tenancy and authentication

Every request carries an API key as a Bearer token: `ptc_sk_…`. Keys are stored hashed. Each
key has a tenant and a scope:

- a role, which says which endpoints the key may call;
- optionally, allowed labels. The key then only sees the runs carrying these labels: the
  agent tools of one conversation only see that conversation's runs.

The tenant is part of every storage key:

| Data | Key |
| --- | --- |
| Runs and their metadata | `(tenant, id)` |
| Idempotency keys of runs | `(tenant, key)` |
| Scripts, toolsets | `(tenant, digest)` |
| Payloads | `(tenant, digest)`; blobs at `{tenant}/sha256/…` |
| Read model, secrets, quotas | Per tenant |

- **Quota**: each tenant has a maximum number of active runs. Above it, starting a run
  returns 429.
- **Fairness**: workers draw a tenant at random among those with runnable work, then take
  that tenant's oldest runnable run. A large tenant cannot starve the others.

The embedded server is the exception: one implicit tenant and no key
([embedded server](07-code-architecture.md#embedded-server-and-cli)).

## Retention and deletion

Active runs are never touched. Defaults, configurable per tenant:

| Data | Kept |
| --- | --- |
| A finished run's journal | 30 days in Postgres (or Turso). Then archived to S3 as a [journal document](02-journal.md#stored-format-v1) for 1 year, and loaded on demand |
| The read model | 90 days |
| Payloads | As long as a run's journal, live or archived, references them; deleted 24 h after the last reference is gone |

- `DELETE /v1/runs/{id}` erases a run at any time: its journal, its archive, its read model
  rows, its credentials and its payload references. Tool arguments can hold personal data.
- A `payload_refs` table, filled at commit, records which runs reference a payload. Cleanup
  deletes the payloads that no run has referenced for 24 h, per tenant.

## Security

Scripts are untrusted (model-generated). They have no ambient authority: every
capability comes from the toolset, and secrets never enter the isolate
([credentials](04-tools.md#credentials)).

A run's seed keys its random generators, `crypto` included. It is protected like the
payloads.

### Sandboxed workers

A V8 isolate is not a hard boundary against V8 vulnerabilities. Only workers run code
written by a model, so only workers are sandboxed.

The same binary takes a role at start:

| Role | Runs | V8 |
| --- | --- | --- |
| `ptc server --role api` | HTTP API, authentication, SSE | No |
| `ptc server --role worker` | V8, dispatcher, scheduler, projector | Yes |

- Workers run under gVisor (`runsc`).
- They run in pools per tenant when needed. Trusted tenants share a pool.
- They reach the outside only through the egress proxy, and have no route to the cloud
  metadata endpoint ([egress](04-tools.md#the-http-tool-and-egress)).
- After an upgrade of V8 or ICU, a worker pool of the previous
  [epoch](03-execution.md#determinism-epoch) stays up until its runs end, at their TTL.

Three walls stand between two tenants: V8, gVisor, and separate pools.
