# 5. Testing and observability

## Testing and mocks

A run has a mode: `live` or `mock`. In `mock` mode the dispatcher is replaced and time
is virtual (timers fire immediately, and logical time jumps to `fire_at`).

Because the engine is deterministic given its ports, mock mode is not a separate
system: it is the same engine wired with in-memory adapters and a virtual clock. The
test harness of the engine itself is built the same way.

Mock sources, tried in order:

1. **Cassettes recorded from real runs.** Any run's journal can be exported as a
   cassette (`GET /v1/runs/{id}/journal?as=cassette`): tool calls with args and outcomes,
   and the timeline of signals. Matching is `sequential` (strict `seq` order) or `by_args`
   (tool + canonical args). A cassette is a document of the
   [stored format v1](02-journal.md#stored-format-v1).
2. **TypeScript mock handlers**, executed in a separate isolate so they cannot break the
   script's determinism.
3. Otherwise the call fails with "no recorded response for `github.close_issue({…})`".

A scenario ties it together:

```json
{
  "script": "triage.ts",
  "input": { "repo": "acme/api" },
  "cassette": "cassettes/triage-2026-11-09.json",
  "mocks": "triage.mocks.ts",
  "signals": [{ "name": "approval", "when": "blocked", "payload": { "close": true } }],
  "expect": { "status": "completed", "output": { "closed": 3 }, "calls": { "github.close_issue": 3 } }
}
```

`rewind test scenario.json` runs it in the CLI process, without a server, and reports a trace
plus the failed expectations. `POST /v1/test` does the same on a server.

In mock mode, the CLI also takes static responses per tool from `rewind.tools.json`
(`{"$error": "message"}` for a failure).

A script that starts [workflows](01-product.md#workflows) is tested the same way: the mock
sources answer `workflows.triage.start` with the child's output (or its failure), and no
child runs ([execution](03-execution.md#workflows-across-runs)).

## Replay, debugging and forks

- `rewind replay <run>`: fetches the journal and the script, re-executes them locally, and
  checks determinism. No tool is called. `<run>` can also be an exported journal document:
  the replay then runs in the CLI process, without a server.
- `rewind replay <run> --script fixed.ts`: tells whether a corrected script is compatible
  with an existing history, and if not, where it diverges.
- `rewind fork <run> --script fixed.ts [--at <index>] [--input …]`: a new run that reuses the
  recorded results of its parent, then continues live. An agent can fix a bug at step 7
  without redoing the side effects of steps 1–6. The agent tool is
  `retry_run({ run_id, code })`, and the API `POST /v1/runs/{id}/fork`.

A fork rests on two things the engine already guarantees:

- What `rewind replay --script` checks: the new script is replayed against the parent's
  history, and as long as its commands match, the recorded outcomes are reused.
- A run that ends by itself has a recorded outcome for every tool call it scheduled
  ([how a script ends](03-execution.md#how-a-script-ends)), so the fork knows which
  effects happened.

What a fork copies, when it goes live, and which idempotency keys its calls carry:
[forks](03-execution.md#forks).

## Evals

Whether the system works for models is measured, not assumed. An eval suite runs models
end to end on the embedded engine: a task, a toolset of deterministic fake MCP servers,
and the loop generate → check → run → fix, in virtual time.

Reported per model and per prompt rendering:

- check pass rate on the first try;
- task success;
- number of iterations;
- tokens.

Mock mode makes this cheap: runs are hermetic and take no wall-clock time. The suite also
replays cassettes recorded from real runs.

The models evaluated are Claude Opus, Sonnet and Haiku, and one OpenAI model. The suite runs
on demand, not in CI: each run costs model tokens.

The choices that are cheap to change today (names, error formats, the `rewind` library, the
prompt rendering, what `run_script` returns) are decided on these numbers.

## Observability

**The journal is the record; telemetry and dashboards are projections of it** (principle
10 in [07](07-code-architecture.md#principles)). Nothing about what a script did is
emitted from the isolate, from the runtime, or during replay. An activation can replay
the whole history, so telemetry emitted while executing would be emitted again on every
replay.

### Two kinds of telemetry

| | Service telemetry | Run telemetry |
| --- | --- | --- |
| Covers | The platform itself: activations, lease contention, replay duration, commits, store queries, V8 heap, dispatcher attempts and retries | What each script did: tool calls, timers, signal waits, logs, outcome |
| Source of truth | The process | The journal |
| Emitted by | The `tracing` facade in `rewind-engine` and the adapters, live | The projector, from committed entries |
| Exported by | The `tracing` subscriber from `rewind-otel`, installed by `rewind-server` and `rewind-cli` | `rewind-otel`, through the `SpanExporter` port |

Both land in the same OTel trace (see the span model below): the dispatcher's attempt
spans appear under the tool call span projected from the journal. The engine only
records the run's trace id and the parent span id as `tracing` fields; `rewind-otel` turns
them into OTel context, so the engine never depends on OpenTelemetry.

### The feed and the projector

`commit_activation` also writes, in the same transaction, one **feed item** per
consumer (`read_model`, `otel`, `webhooks`): `(run_id, fence, from_idx, to_idx)`, the entries
the activation just committed. The projector claims feed items the way the dispatcher claims
tool tasks (lease, `SKIP LOCKED`), applies them, and acknowledges them.

- Delivery is at least once. Every projection is idempotent, keyed by `(run_id, idx)` or
  by deterministic span ids.
- One feed item per consumer, so a slow OTel backend never delays the dashboard.
- A projection can be rebuilt by re-reading histories: backfill, a new projection, a bug
  fix.
- Feed items are claimed, not read with a high-water mark: Postgres sequence values are
  not visible in commit order, so a cursor could skip an activation that committed late.

The SSE stream of a single run doesn't need the feed: it tails that run's history by
index and is woken by commits.

### Span model

One OTel trace per run.

| Span | Span id | Start → end | Exported by |
| --- | --- | --- | --- |
| Run | `H(run, "run")` | `run_started` → terminal entry | Projector, when the run ends |
| Activation | `H(run, fence)` | lease acquired → commit | Engine (service telemetry), at commit |
| Tool call | `H(run, seq)` | `schedule_tool_call` → completion | Projector, when the completion is committed |
| Tool attempt | random | dispatch → response | Dispatcher (service telemetry), child of the tool call span |
| Timer | `H(run, seq)` | `start_timer` → `timer_fired` or `cancel_timer` | Projector |
| Signal wait | `H(run, idx of signal_awaited)` | `signal_awaited` → `signal_received` or timeout | Projector |

- **Trace id**: the harness's trace id if a `traceparent` was given when the run started
  (the run span is then a child of the harness's span, typically the LLM call that wrote
  the script); otherwise the run id (a UUID v7 and an OTel trace id are both 128 bits).
- **Deterministic span ids** (`H` is a truncated SHA-256). A span exported again after a
  crash is the same span. Live service spans (activations, dispatcher attempts) can
  name journal spans as parents before those are exported.
- **The run span is exported when the run ends**, but its id is known from the start, so
  every child links to it right away. A short run appears as a complete tree. A run
  paused for days shows its children under a parent that appears when the run ends.
- **Logs** are exported as OTel log records, correlated with the trace and the activation
  span.
- **Attributes**: `rewind.run_id`, `rewind.script` (digest), `rewind.toolset`, `rewind.seq`,
  `rewind.tool`, `rewind.callsite` (TypeScript file and line), outcome and error kind, attempts,
  run labels. Payloads appear as digest, size and preview, **never as values**: arguments
  are written by a model and may hold sensitive data, and OTel backends have weaker access
  control than the store.
- **Sampling** is decided per run from the trace id, so every worker and every
  activation agrees without coordination. The read model is never sampled.
- **Context propagation**: tool calls forward the W3C trace context (the tool call span as
  parent) to MCP servers in `_meta`, next to the idempotency key, and to host tools
  through the SDK. A run of a workflow started by a script takes the trace context of its
  start call: its run span is a child of that call's span, so a tree of runs is one trace.
- In mock mode and virtual time, nothing is exported by default: timestamps are virtual.

### Run metadata

Given by the harness when it starts a run and stored with the run. The script never sees
it, and it is not part of the journal's events, so it is never replayed and never ends
up in a cassette.

- `labels`: a string map (agent, conversation, end user, environment…), used to filter in
  the dashboard and as span attributes.
- `trace_context`: the W3C `traceparent` and `tracestate`.
- `idempotency_key`: see [storage](06-storage-and-scale.md#idempotency-of-the-api).
- The tenant ([decided](08-decisions.md#taken-after-the-layering-november-2026): it lives here, never in the core).
- For a fork, the run it was forked from ([forks](03-execution.md#forks)).
- For a run of a [workflow](01-product.md#workflows): the workflow and its version; when a
  script started it, that run, the call's `seq` and whether it is detached; the label scope it
  inherited ([execution](03-execution.md#workflows-across-runs)).

### Read model

`JournalStore` is shaped by the write path's invariants. Questions across runs go to a
separate **read model** behind the `ReadModel` port, filled by the projector:

- `run_summaries`: status, script and its first line, toolset, labels, mode, the parent of a
  fork, the workflow of a run of a workflow (its name: the version is on the run) and the run
  that started it, start and end, last activity, number of tool calls and errors, history length; for a
  run that waits, what it waits for and when it wakes; for a run that ended badly, the error's
  name, message, line and fingerprint; whether it is quarantined.
- `run_labels`: one row per label of a run, for equality, prefix and presence.
- `run_waits`: what an active run waits for: a signal (its name, the timeout as wake time), a
  timer (its due time), a tool call dispatched by Rewind, or a host tool's call; each with
  the entry that opened it and its callsite.
- `tool_calls`: run, `seq`, tool, callsite line, scheduled and completed at, outcome,
  error kind, payload digests, and the furthest state its task reached: its attempt, and
  whether it waits for an approval, is parked or deferred.

It answers "which runs called `github.close_issue`", "failures by error kind", "latency
and error rate per tool", "every script agent X ran this week", "which runs wait for an
approval". Its projections are tables in Postgres, or in Turso for the embedded server; a
columnar read model is out of 0.1.

- **Snapshots.** The projector already loads a run's history to project a feed item. It folds
  it into a snapshot of the run (status, open waits, how it ended) and gives it with the
  entries; the read model keeps the snapshot with the highest history length, so items
  applied out of order or twice give the same projection. No query reads a journal.
- **Fingerprints.** A run that failed, was cancelled or was aborted gets a fingerprint: its
  error's name and its message with numbers, UUIDs, long hexadecimal ids, times and
  double-quoted strings replaced by placeholders (for an abort, the abort's kind and message).
  Runs with the same fingerprint are one failure group, across the many scripts models write.
  The projector computes it, so every adapter stores the same value.
- **Service state.** Quarantine and the state of a tool task are not in the journal. Whoever
  changes them records them in the read model, as attempts are recorded: the worker that
  quarantines a run, the endpoints that release or give it up; whoever claims, defers,
  approves, parks or resumes a task. A task's records only move forward (its attempt, then
  its state within the attempt), so they too can arrive in any order.

### Query language

The runs of the read model are searched with one query language, the same in the API
(`q`), the dashboard's query bar, the SDK and `rewind runs`. Terms are separated by spaces
and all must hold; a comma inside a term means "or"; a leading `-` excludes.

| Term | Matches runs |
| --- | --- |
| `@agent:triage` | carrying the label `agent` with value `triage`. `@customer:acme*`: a value starting with `acme`; `@conversation:*`: the label is set |
| `status:failed,aborted` | with one of these statuses |
| `tool:github.close_issue`, `tool:github.*` | that scheduled a call to this tool, or to a tool starting with `github.` |
| `error:timeout`, `error:TypeError` | with a tool call that failed with this kind, or that ended with this error name (or abort kind) |
| `waiting:*`, `waiting:signal,approval` | active, and waiting for: `signal`, `timer`, `call` (a tool Rewind calls), `host` (a host tool), `approval`, `parked`, `deferred` |
| `signal:approval` | waiting for this signal |
| `started:>2026-11-01`, `started:>now-24h`, `ended:<…`, `activity:>…` | by time: a date, a time, or `now` minus a duration; `>`, `>=`, `<`, `<=`, or a range `a..b` |
| `duration:>5m`, `calls:>=20`, `errors:>0` | by duration (ended runs), number of tool calls, number of failed calls |
| `script:9f2c4e1a`, `toolset:…`, `id:0193a5c4` | by digest or id prefix: 8 hexadecimal digits at least |
| `parent:<run id>` | forks of this run |
| `workflow:triage`, `started_by:<run id>` | runs of this workflow; runs this run started |
| `fingerprint:<fingerprint>` | in this failure group |
| `quarantined:yes`, `mode:mock` | quarantined (`no` for the others), by mode |
| `rate`, `"rate limit"` | containing this text, case-insensitive, in the error's message, the script's first line, a label's value or the run's id |

Field names are case-sensitive and a term that cannot be read is refused with its position
(`invalid-query`), so a client can underline it. Logs and payloads are never searched: they may
hold what a tool returned, which the read model does not index. The server limits every query
to the caller's tenant, and for a key limited to labels, to its labels.

### Dashboard

The dashboard shows every run and all the code executed on the platform. It is a React
single-page application, served by `rewind-server`.

| View | Shows |
| --- | --- |
| Overview | The home page: runs by outcome over time, success rate, durations, runs waiting now, quarantined runs, what fails most, failing tools; every mark opens the runs behind it |
| Waiting | Every run paused on the outside world, grouped by what it waits for (signals, approvals, timers, tools), with the line it is paused on and when it wakes |
| Failures | Failed, cancelled and aborted runs grouped by fingerprint: count over time, scripts, labels, the line most runs failed at, a sample run |
| Runs | The query bar, with suggestions drawn from the values that exist, and the counts of each value (facets) beside the results |
| Scripts | Every distinct script (content-addressed, so deduplicated), with run counts, outcomes and tools used |
| Run | The source, with each tool call, timer and signal next to the line that caused it; the timeline; payloads loaded on demand; live through SSE; a time cursor; for a run of a workflow, its workflow, the run that started it and the runs it started |
| Tools | Latency, error rate and retries, per tool and per toolset |

- **The time cursor.** The commit invariant makes a history a sequence of steps: a trigger
  (the start, a tool result, a timer, a signal, a cancellation) and the commands committed
  with it. The Run page steps through them like a debugger: at each step the source marks the
  awaits that just resumed, the calls issued, those still open and those done; hovering a call
  shows its arguments and result as known at that step, and a variable bound to an awaited
  call (`const x = await call()`, one level of destructuring) or `input` shows its value; a
  click pins it as a watch. The cursor is in the URL (`?step=`), the timeline shows it, and on
  a live run it follows the head. Only values that crossed the journal are known: what the
  script computes in between is not recorded, and the dashboard does not execute code.
- **Searches.** The query follows the operator from one view to another in the URL. Saved
  searches are kept in the browser, like the key.
- It is a client of the **public HTTP API**, with no direct access to the store. The SDK,
  the agent tools and the dashboard see the same thing.
- Access is scoped by tenant, since it exposes code and payloads.
- It reads the journal through the read model, not an OTel backend. OTel backends sample,
  keep data for weeks, truncate attributes and don't store source code; a run paused for
  a month would fall out of them.

### LLM trace and metrics

- **Event stream**: `GET /v1/runs/{id}/events?follow=1` (SSE), for harness UIs and the
  dashboard.
- **LLM trace**: `GET /v1/runs/{id}/trace?format=llm` and `rewind trace <run>`. Payloads are
  truncated, and each entry can be expanded individually (`rewind trace <run> --entry 7`).

```
run 01JC…7Q  suspended · waiting for signal "approval" (timeout in 2d 23h)
script triage.ts · toolset 9f2c4e1a07b3 · started 2026-11-09T10:02:11Z · cpu 180ms

  #  at      event                                                       line
  1  +0.0s   github.list_issues({repo:"acme/api",state:"open"})            L5   → 37 items (12.4 KB) 310ms
  2  +0.3s   slack.post_message({channel:"#triage",text:"3 stale…"})       L8   → ok 120ms
  3  +0.4s   timer #3 started 3d (signal "approval" timeout)               L10
```

- **Metrics**: runs by status, activation latency, replay duration, tool latency and
  error rate, inbox depth.
