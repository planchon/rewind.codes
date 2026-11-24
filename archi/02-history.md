# 2. The history

Each run has an append-only, ordered history. It is a list of **steps**: a step is one thing
from outside (a trigger) and everything the workflow did in response, and it is one or more
events. Every event has an index, its position in the history (`0, 1, 2, …`), a `recorded_at`
timestamp, and belongs to one of four families.

| Family | Events | Written when |
| --- | --- | --- |
| Lifecycle | `run_started { code, action_set, input, seed, epoch, expires_at, mode }`, `run_aborted { reason }` | The run's first activation; the engine ending the run. |
| Commands (emitted by the workflow's code) | `schedule_call { seq, action, args, callsite }`, `start_timer { seq, duration_ms, fire_at, callsite }`, `cancel_timer { seq }`, `complete_run { output }`, `fail_run { error }` | The code did something that needs the outside world, or finished. |
| Completions (handed to the code) | `call_completed { seq, result }`, `call_failed { seq, error }`, `timer_fired { seq }`, `signal_received { name, payload }`, `cancel_requested { reason }` | Something from the outside world reached the run. |
| Annotations | `log { level, message, callsite }`, `signal_awaited { name, timeout, callsite }` | Observability only. Not checked during replay. |

**Triggers** are `run_started` and every completion. A trigger is what wakes the code up;
commands are what the code does in response. A trigger, with the commands and annotations
emitted in response to it, is a step.

Field notes:

- `code` and `action_set` in `run_started` are content digests (SHA-256): of the workflow's
  code and of the pinned action set. A run always replays the exact code and action set that
  produced it.
- `mode` is `live` or `test` (a [test run](05-testing-and-observability.md#test-runs)).
- `seed` is 256 bits. It keys the sandbox's random generators, so it is as sensitive as
  the payloads.
- `epoch` is the [determinism epoch](03-execution.md#determinism-epoch) the run started
  under.
- `expires_at` is the run's deadline: the time it was started plus its
  [TTL](03-execution.md#run-ttl), 30 days by default.
- `input`, `args`, `result`, `output` and `payload` are **payloads**: a JSON value,
  identified by the digest of its canonical JSON (RFC 8785). In the domain model an event
  holds either the value or only its digest (see
  [storage](06-storage-and-scale.md#payloads)).
- `call_failed.error` is `{ kind, message, data? }` with kind `error`,
  `unavailable`, `timeout` or `outcome_unknown`.
- `fail_run.error` is `{ name, message, stack? }`. Besides what `main` throws, the
  runtime fails a run with the rejection its code left unhandled, or with the name
  `DeadlockError` ([how a run ends](03-execution.md#how-a-run-ends)).
- `cancel_requested.reason` is `ttl` when the run's deadline has passed.
- `run_aborted.reason` is `{ kind, message }` with kind `limit_exceeded`,
  `non_determinism`, `cancelled`, `invalid_code` or `internal`. `non_determinism` is
  only written when an operator, or the deadline, ends a
  [quarantined](03-execution.md#non-determinism-and-quarantine) run.
- `signal_awaited.timeout` is the `seq` of the timer that implements the timeout, if any.
- Signal names starting with `rewind:` are reserved. A `signal_received` named
  `rewind:ended:<run id>` is the end of a run this run started or watches (its UUID: a run's
  handle is for people, never in the history)
  ([workflows](03-execution.md#workflows-across-runs)). Its payload is
  `{ "status": "completed", "output" }`, `{ "status": "failed" | "cancelled", "error" }` with the
  error of its `fail_run`, or `{ "status": "aborted", "reason" }` with the reason of its
  `run_aborted`.

## `seq`

`seq` is a per-run counter (`#1`, `#2`, …) assigned to commands **in the order the
code emits them**. The code is single-threaded and deterministic, so the same code
fed the same completions always assigns the same `seq` to the same call, even under
`Promise.all`.

## Callsites

`callsite` is the position in the **TypeScript** source of the line that issued the
command, obtained from the transpiler's source map (`new Error().stack` is source-mapped
inside the isolate). It is what links traces to code.

## The commit invariant

A trigger is committed in the same batch as every event the code emitted in response to
it: a step is committed whole. A batch is one atomic write of the run's partition, durable
once its commit returns ([partitions](06-storage-and-scale.md#partitions-owners-and-nodes)).

- Starting a run only enqueues `run_started` in the inbox. The first activation commits
  it together with the first commands.
- If an activation is aborted (CPU limit, heap limit, …), the trigger is committed
  with `run_aborted`, and the commands emitted before the abort are dropped along with
  their effects.
- A replay mismatch commits nothing: the run is
  [quarantined](03-execution.md#non-determinism-and-quarantine).

Replay depends on this invariant: at any point, the history contains complete steps and
nothing else.

`run_aborted` is distinct from `fail_run`. `fail_run` is the code's own outcome and is
reproduced by replay; `run_aborted` is the engine ending the run, and replay stops there.

## Run state

The run's state is a pure function of its history (`RunState::from_history` in
`rewind-core`):

- `status`: `queued` (only `run_started` enqueued: no step committed yet), `active`, `completed`,
  `failed`, `cancelled` (a `fail_run` whose error is `CancelledError`), `aborted`. A run is
  active until it has ended; the last four are its endings.
- pending calls (`seq → action`), pending timers (`seq → fire_at`), awaited signals.
- `started_at`, `updated_at`.

An ending clears everything pending. The history is authoritative for whether a run has ended,
and how.

The status a run shows splits an active run in two. The partition owner computes it at each
batch, from the waits it already stores in the run's head (the calls in flight, backing off or
waiting, with their states; the timers; the awaited signals; quarantine), and writes it in the
head and in the run's summary, which serve queries
([partitions](06-storage-and-scale.md#partitions-owners-and-nodes)):

| Status | When |
| --- | --- |
| `queued` | No step committed yet. |
| `running` | A step is in progress (a trigger in its inbox, or its activation running), or a call Rewind makes itself, to an MCP server, an API or `http`, is in flight or backing off. |
| `waiting` | Active, and nothing Rewind does can move it until something arrives: a signal, a timer, a harness action, an approval, a deferred answer, a parked call, a release from quarantine. |
| `completed`, `failed`, `cancelled`, `aborted` | The run has ended, as its history says. |

What a waiting run waits for is its `waitingFor`, read from the same waits: items of kind
`signal`, `timer`, `call` (with the call's state: `ready` or `claimed` for a harness action,
`approval`, `deferred` or `parked`) or `quarantined`. An active run is queued, running or
waiting; an ended run is completed, failed, cancelled or aborted.

When a run ends by itself (`complete_run` or `fail_run`, without a cancellation), every
call it scheduled has a recorded outcome: the ending command is only emitted once
no call is pending ([how a run ends](03-execution.md#how-a-run-ends)).

Quarantine is not in the history. Like the counts of failed activations, it is service state
kept in the run's head; a quarantined run is `waiting`, for `quarantined`
([quarantine](03-execution.md#non-determinism-and-quarantine)).

## Stored format v1

What is replayed or exported is written in the stored format, defined by `crates/rewind-format`:
JSON, versioned `"v": 1`, every value in its canonical form. It covers seven documents:

| Document | Fields | Where it lives |
| --- | --- | --- |
| Event | `type`, then the event's fields (`seq`, `action`, `args`, `callsite`…); a payload is `{"value": …}` or `{"digest": "9f2c…"}` | A run's history or inbox, in its partition ([where data lives](06-storage-and-scale.md#where-data-lives)) |
| Run export | `v`, `run` (the run id), `code` (the TypeScript source), `action_set` (the pinned action set document), `events[]` | The run's export (`GET /v1/runs/{id}/export`); the archive, where an attempt that ended is sealed with every payload up to 64 KiB inlined as a value ([retention](06-storage-and-scale.md#retention-and-deletion)); the e2e corpus |
| Cassette | `v`, `calls[{action, args, outcome}]`, `signals[{name, payload, elapsed_ms}]`, `matching`: `sequential` or `by_args` | A file or S3; replayed by `rewind test` ([testing](05-testing-and-observability.md#testing)) |
| Pinned action set | `v`, `servers[{name, transport, concurrency, auth?}]`, `apis[{name, base_url, concurrency, auth?}]` when it has APIs, `actions[{name, route, operation?, description?, schemas, policy}]` (`operation`, for an API action: `method`, `path`, its `parameters[{name, in}]`, `body` and `idempotency_key`), and `workflows[{name, id, version, digest, description?, schemas}]` when it pins workflows (`id` the workflow's, `version` the version's number, `digest` its document's); `auth` says how calls authenticate ([04](04-tools.md#authenticating-calls)), never with a value | The `action_sets` table of the control plane; its digest is the digest of its canonical JSON ([action sets](04-tools.md#action-sets-and-code-generation)) |
| Workflow version | `v`, `code` and `action_set` (digests), `schemas: { input, output, signals: { <name>: schema } }` when it has an interface (without one, it accepts any input, output and signal name); its suite: `base?` (the version it derives from, a digest), `tests?[{name, test}]` (the suite it passed), `retired?[{name, test, reason}]`, `updated?[{name, old, new, reason}]` (`test`, `old` and `new` are test digests), each left out when empty; `labels: { <key>: value }` when it was made under a grant limited to labels; and `secrets: { <server>: "org/<name>" \| "ns/<name>" }`, the [secrets](04-tools.md#secrets) its runs use, when it names any. Its workflow's name and description are not part of it: they are the workflow's, and change without a version | The `workflow_versions` table of the control plane, under its workflow and its number; its digest is the version's `digest` ([workflows](01-product.md#workflows), [suites](05-testing-and-observability.md#suites-and-the-gate)) |
| Test | `v`, `name`, `input?`, `start?` (milliseconds), `seed?`, `world?: { actions?: { <action>: rule \| [rule] }, handlers?, cassette? }` (a rule: `args?`, `returns` or `error`, `times?`; the cassette as above, without its `v`), `signals?[{name, payload?, when?}]`, `expect?: { status?, output?, error?, calls?[{action, args?, times?}], order? }`, `from?: { run, version, namespace }` (the run a recorded test comes from) | A blob in the `BlobStore`, under the digest of its canonical JSON ([tests](05-testing-and-observability.md#testing)) |
| Partition records | Sixteen records (`rewind_format::partition`), one per kind of thing a partition holds: the run head (its origin and the length of its handle's suffix, its status and what it waits for, its determinism epoch, its code and pinned action set, the attempt and the last one sealed, the counts of failed activations and of `unfinished` ones, quarantine, waits, deadline, history length, metadata, dates), a history event (the event, in its stored form above), an inbox item, a signal key, a task (the dispatcher's record of a call: its state, its `delivery` and `next_delivery_at`), a timer, a payload, a watcher, a signal URL token (`SignalUrlToken`), the run's summary in the run index, a start key, the counters of a namespace and hour, the shape of an action's results, the gauges of a namespace, an outbox item, the tenant state (`TenantState`) ([08](08-decisions.md#taken-while-building-the-partitions-on-slatedb-november-2026)) | A partition ([where data lives](06-storage-and-scale.md#where-data-lives)); the keys they are stored under belong to `rewind-store-slatedb` |

A reader refuses an unknown version, type or field. Every change of the format comes with
an explicit migration. The vocabulary of 0.1 (history, event, action, call, delivery, code…)
was given to the stored data by one such migration, run once and offline,
`rewind server rename-data` ([where data lives](06-storage-and-scale.md#where-data-lives)): it
rewrote every document and record in place under `"v": 1`, and recomputed the digest of each
content-addressed document (pinned action sets, workflow versions), rewriting every reference to
it. A workflow's name and a version's number name what they named before; an old digest no
longer resolves.

Service state and run metadata stay out of the events and the run export. They live in
the run's head and the other partition records: never replayed, never exported, never in a
cassette.

- labels, idempotency keys, the tenant, the trace context;
- the workflow and version a run belongs to, the run that started it, the runs that watch its
  end, and the allowed labels it inherited ([workflows](03-execution.md#workflows-across-runs));
- the secrets a run uses, and every secret's value;
- the status the partition owner computes, and what the run waits for;
- quarantine, the counts of failed activations and of activations that never finished;
- the outbox, and the counters;
- a fork's link to its source.

## Where it lives in the code

- The model (`Event`, `Command`, `Completion`, `HistoryEvent`, `RunState`, `ReplayCursor`)
  is in `crates/rewind-core`. It has no serde: the core does not know how events are stored.
- The stored form is defined by `crates/rewind-format` ([above](#stored-format-v1)), the
  partition records included. The keys they are stored under belong to
  `crates/rewind-store-slatedb`: the engine never sees a key.
- See [code architecture](07-code-architecture.md).
