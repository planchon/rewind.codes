# 2. The journal

Each run has an append-only, ordered history. Every entry has an index (`0, 1, 2, …`),
a `recorded_at` timestamp, and one event from four families.

| Family | Events | Written when |
| --- | --- | --- |
| Lifecycle | `run_started { script, toolset, input, seed, epoch, expires_at, mode }`, `run_aborted { reason }` | The run's first activation; the host ending the run. |
| Commands (emitted by the script) | `schedule_tool_call { seq, tool, args, callsite }`, `start_timer { seq, duration_ms, fire_at, callsite }`, `cancel_timer { seq }`, `complete_run { output }`, `fail_run { error }` | The script did something that needs the outside world, or finished. |
| Completions (delivered to the script) | `tool_call_completed { seq, result }`, `tool_call_failed { seq, error }`, `timer_fired { seq }`, `signal_received { name, payload }`, `cancel_requested { reason }` | Something from the outside world reached the run. |
| Annotations | `log { level, message, callsite }`, `signal_awaited { name, timeout, callsite }` | Observability only. Not checked during replay. |

**Triggers** are `run_started` and every completion. A trigger is what wakes the script
up; commands are what the script does in response.

Field notes:

- `script` and `toolset` in `run_started` are content digests (SHA-256). A run always
  replays the exact code and toolset that produced it.
- `mode` is `live` or `mock`.
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
- `tool_call_failed.error` is `{ kind, message, data? }` with kind `error`,
  `unavailable`, `timeout` or `outcome_unknown`.
- `fail_run.error` is `{ name, message, stack? }`. Besides what `main` throws, the
  runtime fails a script with the rejection it left unhandled, or with the name
  `DeadlockError` ([how a script ends](03-execution.md#how-a-script-ends)).
- `cancel_requested.reason` is `ttl` when the run's deadline has passed.
- `run_aborted.reason` is `{ kind, message }` with kind `limit_exceeded`,
  `non_determinism`, `cancelled`, `invalid_script` or `internal`. `non_determinism` is
  only written when an operator, or the deadline, ends a
  [quarantined](03-execution.md#non-determinism-and-quarantine) run.
- `signal_awaited.timeout` is the `seq` of the timer that implements the timeout, if any.
- Signal names starting with `rewind:` are reserved. A `signal_received` named
  `rewind:ended:<run id>` is the end of a run this run started or watches (its UUID: a run's
  handle is for people, never in the journal)
  ([workflows](03-execution.md#workflows-across-runs)). Its payload is
  `{ "status": "completed", "output" }`, `{ "status": "failed" | "cancelled", "error" }` with the
  error of its `fail_run`, or `{ "status": "aborted", "reason" }` with the reason of its
  `run_aborted`.

## `seq`

`seq` is a per-run counter (`#1`, `#2`, …) assigned to commands **in the order the
script emits them**. The script is single-threaded and deterministic, so the same script
fed the same completions always assigns the same `seq` to the same call, even under
`Promise.all`.

## Callsites

`callsite` is the position in the **TypeScript** source of the line that issued the
command, obtained from the transpiler's source map (`new Error().stack` is source-mapped
inside the isolate). It is what links traces to code.

## The commit invariant

A trigger is committed in the same batch as every entry the script emitted in response to
it. A batch is one atomic write of the run's partition, durable once its commit returns
([partitions](06-storage-and-scale.md#partitions-owners-and-nodes)).

- Starting a run only enqueues `run_started` in the inbox. The first activation commits
  it together with the first commands.
- If an activation is aborted (CPU limit, heap limit, …), the trigger is committed
  with `run_aborted`, and the commands emitted before the abort are dropped along with
  their effects.
- A replay mismatch commits nothing: the run is
  [quarantined](03-execution.md#non-determinism-and-quarantine).

Replay depends on this invariant: at any point, the history contains complete
"trigger → response" groups and nothing else.

`run_aborted` is distinct from `fail_run`. `fail_run` is the script's own outcome and is
reproduced by replay; `run_aborted` is the host ending the run, and replay stops there.

## Run state

The run's state is a pure function of its history (`RunState::from_history` in
`rewind-core`):

- `status`: `pending` (only `run_started` enqueued), `active`, `completed`, `failed`,
  `cancelled` (a `fail_run` whose error is `CancelledError`), `aborted`.
- pending tool calls (`seq → tool`), pending timers (`seq → fire_at`), awaited signals.
- `started_at`, `updated_at`.

A terminal status clears everything pending. The run's head keeps a copy of the status for
queries, but the history is authoritative.

When a run ends by itself (`complete_run` or `fail_run`, without a cancellation), every
tool call it scheduled has a recorded outcome: the terminal command is only emitted once
no call is pending ([how a script ends](03-execution.md#how-a-script-ends)).

Quarantine is not a status. Like the counts of failed activations, it is service state kept
in the run's head, outside the history.

## Stored format v1

What is replayed or exported is written in the stored format, defined by `crates/rewind-format`:
JSON, versioned `"v": 1`, every value in its canonical form. It covers seven documents:

| Document | Fields | Where it lives |
| --- | --- | --- |
| Event | `type`, then the event's fields (`seq`, `tool`, `args`, `callsite`…); a payload is `{"value": …}` or `{"digest": "9f2c…"}` | A run's history or inbox, in its partition ([where data lives](06-storage-and-scale.md#where-data-lives)) |
| Journal document | `v`, `run` (the run id), `script` (the TypeScript source), `toolset` (the pinned toolset document), `entries[]` | Journal export; the archive, where an attempt that ended is sealed with every payload up to 64 KiB inlined as a value ([retention](06-storage-and-scale.md#retention-and-deletion)); the e2e corpus |
| Cassette | `v`, `calls[{tool, args, outcome}]`, `signals[{name, payload, elapsed_ms}]`, `matching`: `sequential` or `by_args` | A file or S3; replayed by `rewind test` ([mocks](05-testing-and-observability.md#testing-and-mocks)) |
| Pinned toolset | `v`, `servers[{name, transport, concurrency, auth?}]`, `apis[{name, base_url, concurrency, auth?}]` when it has APIs, `tools[{name, route, operation?, description?, schemas, policy}]` (`operation`, for an API tool: `method`, `path`, its `parameters[{name, in}]`, `body` and `idempotency_key`), and `workflows[{name, version, description?, schemas}]` when it pins workflows; `auth` says how calls authenticate ([04](04-tools.md#authenticating-calls)), never with a value | The `toolsets` table of the control plane; its digest is the digest of its canonical JSON ([toolsets](04-tools.md#toolsets-and-code-generation)) |
| Workflow version | `v`, `name`, `description?`, `script` and `toolset` (digests), `schemas: { input, output, signals: { <name>: schema } }`; its suite: `parent?` (a version), `tests?[{name, test}]`, `retired?[{name, test, reason}]`, `updated?[{name, from, to, reason}]` (`test`, `from` and `to` are test digests), each left out when empty; `labels: { <key>: value }` when it was saved under a grant limited to labels; and `secrets: { <server>: "org/<name>" \| "ns/<name>" }`, its [attachments](04-tools.md#secrets), when it attaches any | The `workflows` table of the control plane; its digest is the version ([workflows](01-product.md#workflows), [suites](05-testing-and-observability.md#suites-and-the-gate)). Without a suite, labels or attachments, the document a version had before versions carried them |
| Test | `v`, `name`, `input?`, `start?` (milliseconds), `seed?`, `world?: { tools?: { <tool>: rule \| [rule] }, mocks?, cassette? }` (a rule: `args?`, `returns` or `error`, `times?`; the cassette as above, without its `v`), `signals?[{name, payload?, when?}]`, `expect?: { status?, output?, error?, calls?[{tool, args?, times?}], order? }`, `origin?: { run, version, namespace }` | A blob in the `BlobStore`, under the digest of its canonical JSON ([tests](05-testing-and-observability.md#testing-and-mocks)) |
| Partition records | Sixteen records (`rewind_format::partition`), one per kind of thing a partition holds: the run head (status, epoch, script and pinned toolset, the attempt and the last one sealed, the counts of failed and started activations, quarantine, waits, deadline, history length, metadata, dates), a history entry (the event, in its stored form above), an inbox item, a signal key, a tool task, a timer, a payload, a watcher, a webhook token, the run's summary in the run index, a start key, the counters of a namespace and hour, the shape of a tool's results, the gauges of a namespace, an outbox entry, the organization state ([08](08-decisions.md#taken-while-building-the-partitions-on-slatedb-november-2026)) | A partition ([where data lives](06-storage-and-scale.md#where-data-lives)); the keys they are stored under belong to `rewind-store-slatedb` |

A reader refuses an unknown version, type or field. Every change of the format comes with
an explicit migration.

Service state and run metadata stay out of the events and the journal document. They live in
the run's head and the other partition records: never replayed, never exported, never in a
cassette.

- labels, idempotency keys, the tenant, the trace context;
- the workflow and version a run belongs to, the run that started it, the runs that watch its
  end, and the label scope it inherited ([workflows](03-execution.md#workflows-across-runs));
- the secrets a run attached, and every secret's value;
- quarantine, the counts of failed activations and of activations that never finished;
- the outbox, and the counters;
- a fork's link to its parent.

## Where it lives in the code

- The model (`Event`, `Command`, `Completion`, `HistoryEntry`, `RunState`, `ReplayCursor`)
  is in `crates/rewind-core`. It has no serde: the core does not know how events are stored.
- The stored form is defined by `crates/rewind-format` ([above](#stored-format-v1)), the
  partition records included. The keys they are stored under belong to
  `crates/rewind-store-slatedb`: the engine never sees a key.
- See [code architecture](07-code-architecture.md).
