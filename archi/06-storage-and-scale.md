# 6. Storage, scale and security

## Backends

Storage sits behind the `JournalStore` and `BlobStore` ports of `ptc-engine`.

| Use | Journal | Payloads |
| --- | --- | --- |
| Production (`ptc-server`) | Postgres (`ptc-store-postgres`) | S3 or any S3-compatible service (`ptc-blob`) |
| Embedded (`ptc-cli`, local dev, tests) | Turso Database (`ptc-store-turso`, the `turso` crate) | Local files (`ptc-blob`) |
| Engine unit tests, mock mode | In memory | In memory |

Every journal adapter must pass the same conformance suite (shipped by `ptc-engine`
behind the `conformance` feature).

## Tables

```
scripts    (digest PK, source, transpiled, source_map)     -- only scripts that compile
toolsets   (digest PK, spec, declarations)
runs       (id PK, script, toolset, status, epoch, history_len, blocked_on, runnable_at,
            expires_at, quarantine, lease_owner, lease_until, fence, sticky_worker,
            labels, trace_context, idempotency_key UNIQUE, created_at, updated_at)
history    (run_id, idx, recorded_at, event)              PK (run_id, idx)
inbox      (id, run_id, event, received_at)
signal_keys (run_id, key)                                  PK (run_id, key)
timers     (run_id, seq, fire_at)                          index on fire_at
tool_tasks (run_id, seq, tool, args, policy, state, attempt, lease_owner, lease_until, next_attempt_at)
feed       (id, consumer, run_id, fence, from_idx, to_idx, lease_owner, lease_until)   M4

-- read model (M4), written only by the projector
run_summaries (run_id PK, script, toolset, status, labels, started_at, ended_at,
               last_activity_at, tool_calls, errors, history_len)
tool_calls    (run_id, seq, tool, callsite_line, scheduled_at, completed_at, outcome,
               error_kind, args_digest, result_digest)      PK (run_id, seq)
```

`event` columns hold the `ptc-format` v1 JSON of the event. `labels`, `trace_context`
and `idempotency_key` are run metadata: stored with the run, never in its events
([05](05-testing-and-observability.md#run-metadata)).

`epoch` and `expires_at` are copies of the values in `run_started`, kept for queries:
workers pick runs by epoch, and the scheduler picks the runs past their deadline.
`quarantine` holds the mismatch message of a
[quarantined](03-execution.md#non-determinism-and-quarantine) run.

## Leases, fencing, workers

- **Workers** are stateless and pick runnable runs with `FOR UPDATE SKIP LOCKED`
  (Postgres), among the epochs they support and outside quarantine. Acquiring a lease
  increments `fence`. The lease lasts 30 s and, from M4, the worker renews it about every
  10 s while it works on the run, during the replay as during the execution: a long replay
  keeps its lease, and a dead worker frees the run at most 30 s after its last renewal.
- A commit only succeeds if the fence still matches, and `PK (run_id, idx)` is a second
  guard. A worker that lost its lease (GC pause, network partition) can never corrupt a
  history.
- The **scheduler** polls due timers the same way and pushes `timer_fired` into inboxes.
  It also polls the runs past their deadline and pushes
  `cancel_requested { reason: "ttl" }`.
- The **projector** claims feed items the same way, applies them to the read model and
  the OTel export, then deletes them (M4).
- A terminal commit clears the run's pending timers and tool tasks.

Scaling path:

1. More workers.
2. Partition `history` and `inbox` by `run_id`; archive the histories of finished runs.
3. Several Postgres clusters sharded by `run_id` if needed.
4. Move the read model to a columnar store, fed by the same feed, if dashboard queries
   outgrow Postgres.

## Idempotency of the API

Tool calls carry a key on the way out ([tools](04-tools.md)). The way in needs the same
protection: a request can succeed while its response is lost, and the caller then
retries without knowing.

| Request | Key | A second request with the same key | Without a key |
| --- | --- | --- | --- |
| Start a run | Unique | Returns the existing run | A second run starts and repeats every effect |
| Send a signal | Unique per run | Is acknowledged and ignored | A second `approval` stays in the mailbox and approves the script's next `signal("approval")` |

- A signal's key is kept in `signal_keys` for the life of the run, not in the inbox. It
  must outlive the delivery of the signal, or a late retry would pass.
- Keys are run metadata. They never appear in events, so they are neither replayed nor
  exported in cassettes.
- Defaults in the SDK: the agent tools use the model's tool call id for `run_script`,
  and a webhook uses the sender's delivery id when there is one.

## Payloads

Payloads (run input, tool args and results, run output, signal payloads) live in **S3**,
except very small ones. Postgres only holds the journal's structure.

The responsibilities are split by layer:

| Layer | Knows |
| --- | --- |
| `ptc-core` | A payload's **identity**: the SHA-256 of its canonical JSON, as defined by RFC 8785 (JCS). An event holds either the value (`Payload::Value`) or only the digest (`Payload::Digest`). The determinism check compares digests. |
| `ptc-engine` | **When** to move values: at commit, payloads above the threshold (default 1 KiB) are written to the `BlobStore` and the event keeps only the digest. On delivery to the script, the value is loaded through the `BlobStore`. |
| `ptc-format` | **How** a payload is written in the journal: `{"value": …}` or `{"digest": "9f2c…"}`. A value is written in its canonical form, so its stored bytes are exactly the bytes its digest is computed on, whatever the JSON library's settings. |
| `ptc-blob` | **Where** values live: `{tenant}/sha256/{digest}` on S3, `.ptc/blobs/sha256/ab/<digest>` on disk (atomic writes). |

Consequences:

- The canonical form is part of the stored format: a digest written today is compared
  during replays for as long as the run lives. RFC 8785 fixes what "sorted keys, no
  whitespace" leaves to the serializer: keys are ordered by UTF-16 code units, numbers
  are IEEE 754 doubles written as ECMAScript writes them, and string escaping is
  minimal. Golden vectors in `ptc-core`'s tests pin it.
- Blobs are immutable, deduplicated within a tenant, and cacheable forever.
- During replay, recorded command args are compared by digest, so they are never
  downloaded. Only completion results delivered to the script are fetched, and workers
  keep a local cache. Sticky execution avoids most fetches.
- Traces, OTel spans and the dashboard must render without downloading payloads. Size
  and preview are store metadata kept next to the digest, not part of the domain model
  (exact place to settle, see [roadmap](09-roadmap.md#open-questions)).

## Security

Scripts are untrusted (model-generated). They have no ambient authority: every
capability comes from the toolset, and secrets never enter the isolate. A V8 isolate is
not a hard boundary against V8 vulnerabilities, so multi-tenant deployments run workers
inside an additional sandbox (gVisor, Firecracker, or a container per tenant).

A run's seed keys its random generators, `crypto` included. It is protected like the
payloads.

The points still to design for M4 (egress of the `http` tool, API authentication, scope
of the agent tools, tenancy) are listed in the
[roadmap](09-roadmap.md#inputs-for-the-m4-architecture-review).
