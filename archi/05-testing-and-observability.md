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

`ptc test scenario.json` runs it in the CLI process, without a server, and reports a trace
plus the failed expectations. `POST /v1/test` does the same on a server.

In mock mode, the CLI also takes static responses per tool from `ptc.tools.json`
(`{"$error": "message"}` for a failure).

## Replay, debugging and forks

- `ptc replay <run>`: fetches the journal and the script, re-executes them locally, and
  checks determinism. No tool is called. `<run>` can also be an exported journal document:
  the replay then runs in the CLI process, without a server.
- `ptc replay <run> --script fixed.ts`: tells whether a corrected script is compatible
  with an existing history, and if not, where it diverges.
- `ptc fork <run> --script fixed.ts [--at <index>] [--input …]`: a new run that reuses the
  recorded results of its parent, then continues live. An agent can fix a bug at step 7
  without redoing the side effects of steps 1–6. The agent tool is
  `retry_run({ run_id, code })`, and the API `POST /v1/runs/{id}/fork`.

A fork rests on two things the engine already guarantees:

- What `ptc replay --script` checks: the new script is replayed against the parent's
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

The choices that are cheap to change today (names, error formats, the `ptc` library, the
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
| Emitted by | The `tracing` facade in `ptc-engine` and the adapters, live | The projector, from committed entries |
| Exported by | The `tracing` subscriber from `ptc-otel`, installed by `ptc-server` and `ptc-cli` | `ptc-otel`, through the `SpanExporter` port |

Both land in the same OTel trace (see the span model below): the dispatcher's attempt
spans appear under the tool call span projected from the journal. The engine only
records the run's trace id and the parent span id as `tracing` fields; `ptc-otel` turns
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
- **Attributes**: `ptc.run_id`, `ptc.script` (digest), `ptc.toolset`, `ptc.seq`,
  `ptc.tool`, `ptc.callsite` (TypeScript file and line), outcome and error kind, attempts,
  run labels. Payloads appear as digest, size and preview, **never as values**: arguments
  are written by a model and may hold sensitive data, and OTel backends have weaker access
  control than the store.
- **Sampling** is decided per run from the trace id, so every worker and every
  activation agrees without coordination. The read model is never sampled.
- **Context propagation**: tool calls forward the W3C trace context (the tool call span as
  parent) to MCP servers in `_meta`, next to the idempotency key, and to host tools
  through the SDK.
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

### Read model

`JournalStore` is shaped by the write path's invariants. Questions across runs go to a
separate **read model** behind the `ReadModel` port, filled by the projector:

- `run_summaries`: status, script, toolset, labels, start and end, last activity, number
  of tool calls and errors, history length.
- `tool_calls`: run, `seq`, tool, callsite line, scheduled and completed at, outcome,
  error kind, payload digests.

It answers "which runs called `github.close_issue`", "failures by error kind", "latency
and error rate per tool", "every script agent X ran this week". Its projections are tables
in Postgres, or in Turso for the embedded server; a columnar read model is out of 0.1.

### Dashboard

The dashboard shows every run and all the code executed on the platform. It is a React
single-page application, served by `ptc-server`.

| View | Shows |
| --- | --- |
| Scripts | Every distinct script (content-addressed, so deduplicated), with run counts, outcomes and tools used |
| Runs | Filters by status, label, script, tool, error kind and time |
| Run | The source, with each tool call, timer and signal next to the line that caused it; the timeline; payloads loaded on demand; live through SSE |
| Tools | Latency, error rate and retries, per tool and per toolset |

- It is a client of the **public HTTP API**, with no direct access to the store. The SDK,
  the agent tools and the dashboard see the same thing.
- Access is scoped by tenant, since it exposes code and payloads.
- It reads the journal through the read model, not an OTel backend. OTel backends sample,
  keep data for weeks, truncate attributes and don't store source code; a run paused for
  a month would fall out of them.

### LLM trace and metrics

- **Event stream**: `GET /v1/runs/{id}/events?follow=1` (SSE), for harness UIs and the
  dashboard.
- **LLM trace**: `GET /v1/runs/{id}/trace?format=llm` and `ptc trace <run>`. Payloads are
  truncated, and each entry can be expanded individually (`ptc trace <run> --entry 7`).

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
