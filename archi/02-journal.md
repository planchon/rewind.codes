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

A trigger is committed in the same transaction as every entry the script emitted in
response to it.

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
`ptc-core`):

- `status`: `pending` (only `run_started` enqueued), `active`, `completed`, `failed`,
  `cancelled` (a `fail_run` whose error is `CancelledError`), `aborted`.
- pending tool calls (`seq → tool`), pending timers (`seq → fire_at`), awaited signals.
- `started_at`, `updated_at`.

A terminal status clears everything pending. The store keeps a copy of the status for
queries, but the history is authoritative.

When a run ends by itself (`complete_run` or `fail_run`, without a cancellation), every
tool call it scheduled has a recorded outcome: the terminal command is only emitted once
no call is pending ([how a script ends](03-execution.md#how-a-script-ends)).

Quarantine is not a status. Like a lease, it is service state kept by the store, outside
the history.

## Stored format v1

What is replayed or exported is written in the stored format, defined by `crates/ptc-format`:
JSON, versioned `"v": 1`, every value in its canonical form. It covers four documents:

| Document | Fields | Where it lives |
| --- | --- | --- |
| Event | `type`, then the event's fields (`seq`, `tool`, `args`, `callsite`…); a payload is `{"value": …}` or `{"digest": "9f2c…"}` | A row of `history` or `inbox` |
| Journal document | `v`, `run`, `script`, `toolset`, `entries[]` | Journal export, the S3 archive of a finished run, the e2e corpus |
| Cassette | `v`, `calls[{tool, args, outcome}]`, `signals[{name, payload, elapsed_ms}]`, `matching`: `sequential` or `by_args` | A file or S3; replayed by `ptc test` ([mocks](05-testing-and-observability.md#testing-and-mocks)) |
| Pinned toolset | `v`, `servers[{name, transport, concurrency}]`, `tools[{name, route, description?, schemas, policy}]` | The `toolsets` table; its digest is the digest of its canonical JSON ([toolsets](04-tools.md#toolsets-and-code-generation)) |

A reader refuses an unknown version, type or field. Every change of the format comes with
an explicit migration.

Service state and run metadata stay out of the format. They are table columns: never
replayed, never exported, never in a cassette.

- labels, idempotency keys, the tenant, the trace context;
- credentials;
- quarantine; lease, fence and sticky worker;
- the feed;
- a fork's link to its parent.

## Where it lives in the code

- The model (`Event`, `Command`, `Completion`, `HistoryEntry`, `RunState`, `ReplayCursor`)
  is in `crates/ptc-core`. It has no serde: the core does not know how events are stored.
- The stored form is defined by `crates/ptc-format` ([above](#stored-format-v1)).
- See [code architecture](07-code-architecture.md).
