# 5. Testing and observability

## Testing and mocks

A **test** describes the world a script meets and what the outside must see at the end. It
never names the script: the script under test is a parameter, so a test written for one version
of a workflow runs unchanged against the next. Tests are always end to end. The engine runs the
script in mock mode, and a test checks only how the run ended and the tool calls it made, never
a variable, a helper or a log line.

### Mock mode

A run has a mode: `live` or `mock`. In `mock` mode the dispatcher is replaced and time
is virtual (timers fire immediately, and logical time jumps to `fire_at`).

Because the engine is deterministic given its ports, mock mode is not a separate
system: it is the same engine wired with in-memory adapters and a virtual clock. The
test harness of the engine itself is built the same way. The **driver** runs the engine's actors
there (the partition owner, the executors, the organization actor) one message at a time on one
thread. When none of them has anything left to do, it moves the virtual clock to the earliest
timer of the [timer wheel](06-storage-and-scale.md#partitions-owners-and-nodes). The same inputs
always give the same steps in the same order.

A test starts at a fixed time with a fixed seed, in virtual time: its result is a function of
the script, the toolset, the test and the epoch. The same four give the same report, so a test
cannot be flaky.

Mock mode reads no secret and signs nothing: a test needs no attachment, and `/v1/test` checks
none ([secrets](04-tools.md#secrets)).

### The test document

```json
{
  "name": "closes the stale issues once approved",
  "input": { "repo": "acme/api" },
  "world": {
    "tools": {
      "github.list_issues": [
        { "args": { "state": "open" },
          "returns": [{ "number": 4, "updated_at": "2025-10-01T00:00:00Z" }] },
        { "args": { "labels": "reopened" }, "returns": [] }
      ],
      "github.close_issue": { "returns": {} },
      "slack.post_message": { "returns": { "ok": true } }
    }
  },
  "signals": [{ "name": "approval", "payload": { "close": true } }],
  "expect": {
    "status": "completed",
    "output": { "closed": 1, "reopened": 0 },
    "calls": [
      { "tool": "slack.post_message", "args": { "channel": "#triage" }, "times": 1 },
      { "tool": "github.close_issue", "times": 1 }
    ],
    "order": "listed"
  }
}
```

| Field | Meaning |
| --- | --- |
| `name` | Unique within a suite. |
| `input` | The run's input; `{}` when absent. |
| `start` | When the run starts, in virtual time; 2026-01-01T00:00:00Z when absent. |
| `seed` | The run's seed, 64 hex digits; the same fixed seed for every test when absent. |
| `world.tools` | For each tool, one answer for every call, or a list of rules tried in order. A rule has `args` (the call's args contain them), `returns` (the result) or `error` (`{ message, kind?, data? }`, `kind` as a tool failure's), and `times` (how many calls it answers; any number when absent). |
| `world.mocks` | TypeScript mock handlers, executed in a separate isolate so they cannot break the script's determinism. |
| `world.cassette` | Calls and signals recorded from a run: a [cassette](02-journal.md#stored-format-v1), matched `sequential` (strict `seq` order) or `by_args` (tool and canonical args). Its signals are sent at the time they arrived in the recorded run. |
| `signals` | Sent to the run in order. `when` is `"blocked"` (the default: once the run can go no further without the outside world) or a duration of virtual time after the start, such as `"2h"`. |
| `expect` | What must hold at the end; everything not given is not checked. |
| `origin` | For a [recorded test](#tests-recorded-from-runs): the run, the workflow version it ran and its namespace. |

A call is answered by the first source that has an answer: the rules of `world.tools`, then the
mock handlers, then the cassette. `rewind test` adds the static responses of `rewind.tools.json`
last. A start of a [workflow](01-product.md#workflows) is answered like any call: `returns` is
the child's output, `error` its failure, and no child runs
([execution](03-execution.md#workflows-across-runs)).

**The world is the list of what the script may touch.** A call no source answers fails with a
`ToolError` ("no recorded response for `github.delete_repo({…})`"), and fails the test, even if
the script catches the error.

| Expectation | Holds when |
| --- | --- |
| `status` | The run is in this state: `completed`, `failed`, `cancelled`, `aborted`, or `active` for a run that can go no further in virtual time without the outside world. |
| `output` | The output contains it: objects key by key, recursively; other values equal as JavaScript sees them (`2` and `2.0` are the same number). |
| `error` | The run failed with an error of this `name`, whose message contains `message`. |
| `calls` | For each entry, the calls to `tool` whose args contain `args`: exactly `times` of them, or at least one when `times` is absent. `times: 0` says a call must not happen. Calls the world answers that `calls` does not list are allowed: a fix that adds a harmless read breaks no test. |
| `order` | `any` (the default), or `listed`: the first call matching each entry comes in the order of the entries (those with `times: 0` aside). |

### Checked before running

Before a test runs, it is checked against what it runs with, without starting an isolate. A test
that fails a check is reported with its problem and is not run:

- **`stale`**: a tool of `world.tools` or of `expect.calls` is not in the toolset (the
  `workflows.<name>.start` and `.signal` of the workflows it pins count); a rule's `returns`
  does not match the tool's output schema, when the tool declares one; a rule's `args` has a
  key that the tool's input schema does not declare, when it declares `properties`. An MCP
  server that changed its tools makes the fixtures written for the old ones stale.
- **`incompatible`**, for a test of a workflow: the workflow's input schema refuses the input;
  a signal is not one the workflow declares, or its schema refuses the payload. A version that
  changed its interface makes the tests written for the old one incompatible.

A test that runs and does not meet its expectations is **`failed`**.

### Running tests

`rewind test`, `POST /v1/test` and the agent tool `test_script` run tests against a script: in
the CLI process, or on a server node. Each test runs in its own engine with
in-memory adapters; four run at once, and a run of tests takes 5 minutes of wall-clock time at
most. Mock handlers that cannot load fail their test. A test's report says whether it passed, its problem,
its failures (one sentence each, written for a model), the run's status, output or error, how
many times each tool was called, and the LLM trace of the run.

```
POST /v1/test   { "script": "…", "toolset": "…", "tests": [ … ], "workflow": "triage" }
```

With `workflow`, the suite of that version runs too, and checks for incompatibility against
its interface; the script and the toolset default to the version's. For `rewind test`, a tests
file is a JSON array of tests, whose `world.mocks` and `world.cassette` may name files relative
to it; without a toolset, the toolset is the tools the tests answer, as host tools. The tests run
in the CLI process, or on the server when they need what it holds: a workflow's suite, or a
toolset registered there. This is how a candidate
is checked before it is saved: a corrected script (`script`), or the version on a new toolset
whose MCP server changed (`toolset`).

### Suites and the gate

A workflow version carries a **suite**: the tests it passed, which every version derived from it
must pass too. The [workflow version document](02-journal.md#stored-format-v1) holds, besides its
script, toolset and schemas:

| Field | Holds |
| --- | --- |
| `parent` | The version it derives from. |
| `tests` | The name and digest of each test of its suite. |
| `retired` | The parent's tests it does not keep: name, digest and reason. |
| `updated` | The parent's tests it replaced: name, the old and the new digest, and the reason. |

Saving a version (`POST /v1/workflows`, the agent tool `save_workflow`) goes through a gate:

1. Its parent is the version named by `parent`, or the latest version of the same name saved
   under the caller's labels (its lineage: a version saved under other labels is another's), or
   none.
2. Its suite is the parent's tests, minus those `retire` names, with those `update` names
   replaced, plus `tests`. `retire` and `update` name inherited tests and give a reason. An
   update gives a whole new test, or only a new `expect` for the inherited one. Names are unique
   in a suite. A test recorded from a namespace erased since is left out, and listed as retired
   with the reason `recorded from an erased namespace`.
3. Every test is checked, then runs against the version's script and toolset.
4. The version is stored only if every test passes. Otherwise nothing is stored, and the answer
   (422 `tests-failed`) lists each test that did not pass, with its problem and its failures.

There is no way around the gate. To save a behavior an inherited test refuses, the version
retires the test because it is deprecated, or updates it because its state or its result is
out of date, and says why. The reasons stay in the lineage: a person reading a version learns
what changed on purpose.

A test is given to a save inline, by the digest of a stored test (one [recorded from a
run](#tests-recorded-from-runs)), or as `{ "from": "<digest>", "expect": { … } }`: a stored test
with a new expectation, so a model corrects a recorded test without reading its cassette.

`retry_run` of a run of a workflow (`POST /v1/runs/{id}/fork`) runs the suite of the run's
version against the corrected script first; the fork is refused (422 `tests-failed`) if a test
does not pass. A fix that breaks another behavior never reaches production calls.

The tests of a script that is not a workflow are not stored: they run through `test_script`,
and become a suite when the script is saved as a workflow.

### Tests recorded from runs

Past runs of a workflow have already met the real world: real inputs, real tool answers, real
signal timings. Recording turns them into tests, so a new version must keep doing what the old
one did on those inputs.

```
POST /v1/workflows/triage/record   { "query": "status:completed", "limit": 20 }
POST /v1/workflows/triage/record   { "runs": ["0193a5c4-…"] }
```

It takes the completed and failed runs of the workflow, of any version, that the caller sees
(its namespace, then its labels): those `runs` names, or those `query` selects, in the [query
language](#query-language), the 1,000 newest at most. It groups them by **path**: the tools a run called in
`seq` order, the signals it waited for, and how it ended, with consecutive repeats collapsed
(`github.close_issue×2` and `×9` are one path). It keeps the newest run of each path, up to
`limit` tests (20 by default, 100 at most). A few hundred runs usually follow a handful of paths.

| Field of the test | Taken from the run |
| --- | --- |
| `name` | `recorded <run id>` |
| `input` | Its input. |
| `start`, `seed` | Its start time and its seed, so `Date.now()` and random values are what they were. |
| `world.cassette` | Its calls, matched `by_args`, the outcomes of the children it started included, and its signal timeline. |
| `expect` | Its status, its output or its error, and how many times it called each tool. |
| `origin` | The run, its version and its namespace. |

A recorded test says: on this real input, the new version makes the same calls with the same
args and returns the same output. Any difference fails it. For a failed run, the model gives the
expectation it should have met (`{ from, expect }`), which turns the run into a regression test.
After an intended change, the model retires the recorded tests it affects, with one reason, and
records again from the new version's runs.

Recording stores the tests and returns, for each, its digest, name, path and run. It adds them
to no suite: a test joins a suite only when a save references it, through the gate. A run whose
test would be larger than 4 MiB is skipped, and named in the answer.

A recorded test holds the run's data and its seed, with which the run's random values (tokens a
script generated included) can be computed again. It is read like the run: only a caller who may
read the runs of its namespace reads it. The gate runs it whoever saves.

### Where tests are stored

A test is a document of the [stored format v1](02-journal.md#stored-format-v1), stored as a blob
in the `BlobStore` (AWS S3 in production) under its tenant and the digest of its canonical JSON
(`tests/{tenant}/sha256/{digest}`). A workflow version holds digests only. The `tests` table of
the control plane indexes the blobs, with the namespace a recorded test came from, and
`workflow_tests` the tests each version references
([where data lives](06-storage-and-scale.md#where-data-lives)). A test lives as long as a version
references it; a daily job of the control plane deletes one no version references, 24 hours after
it was stored. A recorded test is erased with its namespace
([retention](06-storage-and-scale.md#retention-and-deletion)).

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

Repair tasks give the model a workflow with its suite and a failed run of it: it succeeds when
its fix passes the gate. They also measure whether models write tests with their scripts
unprompted, and how often the gate refuses a fix they believed correct.

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
| Covers | The platform itself: activations, replay duration, batches and their latency, partitions owned and taken over, the ready queue, store reads, V8 heap, dispatcher attempts and retries | What each script did: tool calls, timers, signal waits, logs, outcome |
| Source of truth | The process | The journal |
| Emitted by | The `tracing` facade in `rewind-engine` and the adapters, live | The projector, from durable batches |
| Exported by | The `tracing` subscriber from `rewind-otel`, installed by `rewind-server` and `rewind-cli` | `rewind-otel`, through the `SpanExporter` port |

Both land in the same OTel trace (see the span model below): the dispatcher's attempt
spans appear under the tool call span projected from the journal. The engine only
records the run's trace id and the parent span id as `tracing` fields; `rewind-otel` turns
them into OTel context, so the engine never depends on OpenTelemetry.

### The projector

The projector turns durable batches into run telemetry: OTel spans and logs, and the lifecycle
webhooks. It consumes the partition's
[outbox](06-storage-and-scale.md#the-outbox). The batch that commits an
activation also writes one outbox entry per projection (`otel`, `webhooks`), naming the entries
it committed: `(run, from_idx, to_idx)`. Once the batch is durable, the owner hands those entries
to the projector in memory. A later batch deletes the outbox entry, once the entries are exported
or delivered. An owner that recovers the partition feeds the projector again with the entries
left in the outbox. `quota.alert` takes the same path, from the outbox of the organization
actor's home partition.

- Delivery is at least once. Every projection is idempotent, keyed by deterministic span ids,
  or by the deterministic id of a webhook event.
- One outbox entry per projection, so a slow OTel backend never delays the webhooks.
- A failed webhook delivery is retried with backoff, each retry a timer of the
  [timer wheel](06-storage-and-scale.md#partitions-owners-and-nodes). Its outbox entry stays until
  the delivery succeeds or is given up.
- The run index and the counters are written by the owner in the same batch as the facts they
  describe ([read model](#read-model)). A list or a count is never behind the journal.
- A projection can be rebuilt by re-reading histories and sealed journals: backfill, a new
  projection, a bug fix.

The event stream of a single run ([below](#llm-trace-and-metrics)) takes another path: the edge
subscribes to the run at its owner, which pushes the entries of each batch once it is durable.

### Span model

One OTel trace per run.

| Span | Span id | Start → end | Exported by |
| --- | --- | --- | --- |
| Run | `H(run, "run")` | `run_started` → terminal entry | Projector, when the run ends |
| Activation | `H(run, "activation", idx)`, `idx` being the index of the first entry it commits | taken from the ready queue → its batch durable | Engine (service telemetry), once its batch is durable |
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
- **Sampling** is decided per run from the trace id, so every node and every
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

- `namespace`: the end user the run belongs to
  ([namespaces](06-storage-and-scale.md#namespaces)). A fork has its parent's.
- `labels`: a string map (agent, conversation, environment…), used to filter in the dashboard
  and as span attributes.
- `trace_context`: the W3C `traceparent` and `tracestate`.
- `idempotency_key`: see [storage](06-storage-and-scale.md#idempotency-of-the-api).
- `secrets`: the secrets attached at start, a scope and a name per server, never a value
  ([secrets](04-tools.md#secrets)). A run of a workflow has its version's.
- The tenant ([decided](08-decisions.md#taken-after-the-layering-november-2026): it lives here, never in the core).
- For a fork, the run it was forked from ([forks](03-execution.md#forks)).
- For a run of a [workflow](01-product.md#workflows): the workflow and its version; when a
  script started it, that run, the call's `seq` and whether it is detached; the label scope it
  inherited ([execution](03-execution.md#workflows-across-runs)).

### Read model

Questions across runs are answered by each partition, from records its owner writes in the same
batch as the facts they describe ([where data lives](06-storage-and-scale.md#where-data-lives)).
There is no separate store to fill: a list or a count is as recent as the partition's last
durable batch, and no query reads a journal.

- **The run index.** One summary per run, live or ended, kept until the run is erased: namespace,
  status, script and its first line, toolset, labels, mode, the parent of a fork, the workflow of a
  run of a workflow (its name: the version is on the run) and the run that started it, start and
  end, last activity, number of tool calls and errors, history length, the tools it called; for a
  run that waits, what it waits for: a signal (its name, the timeout as wake time), a timer (its
  due time), a tool call dispatched by Rewind, a host tool's call, an approval, a parked or a
  deferred call, each with the entry that opened it and its callsite; for a run that ended badly,
  the error's name, message, line and fingerprint; whether it is quarantined. Summaries are kept
  in the order of their start under their namespace, and under each label's value, so a
  namespace's runs, or those carrying a label, are read in order without reading the others.
- **Counters.** Per tenant, namespace and hour: the runs started and ended, by status and by
  workflow, with a sketch of their durations; the failures by fingerprint, with the scripts and
  lines they came from and their latest runs; per tool, the calls, the errors by kind, a sketch of
  the latency and the retries; per script, the runs by status and the tools it called. Per tenant
  and namespace, the active runs that wait now, by what they wait for. Per tenant, namespace and
  tool, the shape observed of its results, which merges in any order. They are what the
  [statistics](#statistics) read.
- **Fingerprints.** A run that failed, was cancelled or was aborted gets a fingerprint: its
  error's name and its message with numbers, UUIDs, long hexadecimal ids, times and
  double-quoted strings replaced by placeholders (for an abort, the abort's kind and message).
  Runs with the same fingerprint are one failure group, across the many scripts models write.
  The engine computes it in the batch that ends the run, so every adapter stores the same value.
- **Service state.** Quarantine and the state of a tool task are not in the journal. They live in
  the run's head, which the owner writes in its batches, and the summary shows them as they are:
  the run's quarantine, and for each call its attempt and whether it waits for an approval, is
  parked or deferred.

It answers "which runs called `github.close_issue`", "every script agent X ran this week",
"which runs wait for an approval" from the index, and "failures by error kind", "latency and
error rate per tool" from the counters. An analytics engine over every history, for questions
nobody planned, is out of 0.1; it would read the sealed journals of the
[archive](06-storage-and-scale.md#where-data-lives).

### Query language

Runs are searched with one query language, the same in the API
(`q`), the dashboard's query bar, the SDK and `rewind runs`. Terms are separated by spaces
and all must hold; a comma inside a term means "or"; a leading `-` excludes.

| Term | Matches runs |
| --- | --- |
| `namespace:alice`, `namespace:alice,bob` | in one of these namespaces |
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
hold what a tool returned, which the index does not hold. The server limits every query
to the caller's tenant; to a token's namespace, or the namespace of the `Rewind-Namespace`
header; and for a grant or a token limited to labels, to its labels.

A query runs where its runs are. A query limited to one namespace (by a token, the
`Rewind-Namespace` header, or a `namespace:` term with one value) goes to that namespace's
partition; any other goes to every partition, and the edge merges their pages in the query's
order. A partition reads summaries in the order asked, through the index of the namespace or of a
label when a term allows it, and evaluates every term on each summary. It reads at most 10,000
summaries for one page: its **scan budget**. A page cut by the budget comes back with fewer runs
than asked and a cursor that resumes where it stopped. Only a page without a cursor is the last.

### Statistics

`GET /v1/runs/stats`, `GET /v1/runs/failures`, `GET /v1/scripts` and `GET /v1/tools/stats`
answer from the [counters](#read-model): they read the partitions that hold their scope, and the
edge adds the counters up. Their answers keep their shapes. They take a scope and a window:

- **The scope** is the caller's tenant, or one namespace: a token's, the `Rewind-Namespace`
  header's, or the one a query `q` names with a single `namespace:` term. Any other term of `q`
  is refused with its position (`invalid-query`).
- **The window** is made of whole hours, in UTC: its bounds are rounded out to the hour.
- **Facets** count the runs of the scope and the window by status, by workflow and by namespace.
  The runs waiting now, by what they wait for, come from the gauge of the active runs.
- **Failure groups** list the fingerprints of the window with their counts per hour, the scripts
  and the lines they came from, and their latest runs. A group's labels are those of its latest
  runs, read from the index.
- **Tool statistics** are per tool, filtered by the start of a tool's name (`github.`);
  **scripts** list each script with its runs by status and the tools it called.
- A grant or a token limited to labels reads none of them (403): counters do not know labels.

The Overview, the Tools and Scripts views and a namespace's page are drawn from them; the runs
behind each of their marks come from a search.

### Dashboard

The dashboard shows every run and all the code executed on the platform. It is a React
single-page application, served by `rewind-server`.

| View | Shows |
| --- | --- |
| Overview | The home page: runs by outcome over time, success rate, durations, runs waiting now, quarantined runs, what fails most, failing tools; every mark opens the runs behind it in Observability. In a namespace's scope, its runs, with its alerts of the month first when it has any |
| Observability | The runs, looked at and filtered. Four views: every run (the query bar, with suggestions drawn from the facets, the runs over time of the namespace and the window shown, and the results), the active ones, those waiting on the outside world (grouped by what they wait for: signals, approvals, timers, tools; with the line each is paused on and when it wakes) and the failures (failed, cancelled and aborted runs grouped by fingerprint: count over time, scripts, labels, the line most runs failed at, its latest run by its handle). Its sidebar holds the views and the filters: the statuses, workflows and namespaces of the window with their counts (facets), which add or remove their term in the query. The table's headers order the runs by their column (newest or oldest first, latest activity, longest, most tool calls, soonest to wake) and filter what each shows, as terms of the query. In a namespace's scope, its runs: no Namespace facet or column, and handles without their namespace |
| Run | One run, analysed in depth. At its top, how it ended; a cancellation is told as a cancellation, not a failure: its cause (its deadline, a request, its script) and what the run awaited then ([decisions](08-decisions.md#taken-for-the-dashboards-type-scale-november-2026)). Its header holds its handle ([handles](06-storage-and-scale.md#run-handles)), its mock or quarantine tags and its actions: its forks, beside them a menu of the controls its permissions allow (send a signal, with the signals a workflow declares and their payloads' schemas; cancel; retry a failed run), its journal and its cassette. Its sidebar holds no action: its sections and its details (namespace, workflow and the run that started it, script, toolset, labels, attached secrets). Its sections: the debugger (the source, with each tool call, timer and signal next to the line that caused it; the timeline; payloads loaded on demand; live through SSE; a time cursor), the journal, the logs, and the runs it started ([decisions](08-decisions.md#taken-for-the-dashboards-sidebar-and-run-page-november-2026)) |
| Scripts | Every distinct script (content-addressed, so deduplicated), by its title ([titles](04-tools.md#script-titles)), with its run counts, outcomes and tools used since its first run, without a window of time, for the organization or one namespace (its scope, or `namespace:` over every namespace); a script's page shows the suites of the workflow versions it is the script of |
| Testing | Its own sidebar: the workflows, the suite of the version shown (each test a click away, its origin as a glyph) and the version's details. A workflow opens at one version, switched in the breadcrumb (the latest by default): its interface and its [suite](#suites-and-the-gate) as a table, each test with its origin (inherited, added, recorded, updated, retired, with the reasons), its input, what it expects, the calls it checks and its world. Each test has its own page, switched in the breadcrumb or with previous/next: its input and expected output, its world, signals and expectations; every JSON value opens whole, colored, in a dialog, its objects and arrays folding. No test result is shown: every test of a suite passed when its version was saved |
| Tools | Latency, error rate and retries per tool, since the first call, without a window of time, for the organization or one namespace (its scope, or `namespace:` over every namespace), filtered by the start of a tool's name |
| Namespaces | Over every namespace: the alerts of the month first, then the tenant's ceiling and the default limits, then the namespaces sorted by CPU, runs or scripts of the month, each with its use against its limits (orange from 80 %, red at 100 %); a search by the start of a name or of a display name with the runs' query bar, and a filter of those above 80 %; a namespace opens its scope. In a namespace's scope, its Usage (`/ns/<name>/usage`), over a month: its alerts first, when it has any; its cumulative CPU against its quota, its use of every limit, its tokens in use, its exceptions; and the controls its permissions allow (rename it, change its limits, revoke its tokens, delete it). How its runs went, its runs, its scripts and its secrets are its scope's other views |
| Secrets | Over every namespace, the organization's secrets; in a namespace's scope, that end user's own, then the organization's its runs can attach, listed only (they are written over every namespace): each with its labels, who wrote it and when, its reads per day and its last read, and the runs and workflows that attach it. A value is written, never shown. Deleting one first shows what it would stop: the active runs that attach it, those waiting on a call that reads it, the workflows whose versions attach it ([secrets](04-tools.md#secrets)) |
| Organization | Opened from the member's menu: the members and their grants, the invitations, the roles and their permissions, the API keys, the signing keys and the address of their public keys, the settings; each part shown to whoever holds its permission |

- **One sidebar.** The organization picker and a search of runs and views sit at its top, and
  between them, in a namespace's scope, the namespace. Below,
  the views; a view that needs more than a toolbar (Observability, Run, Organization) turns the
  sidebar into its own, with a "back" at its head: from Observability or Organization it shows
  the views again without leaving the page, from a run it returns to the list it was opened
  from. The member's menu, at the bottom, holds the organization, the theme, the time zone and
  the sign-out. The sidebar holds no action: a page's actions are in its header. A click on its
  right edge folds it to its icons, and a click on the rail's edge unfolds it; a drag of the edge
  resizes it.
- **Explanations.** What a page or a section is opens from an (i) beside its title, in a card on
  hover, focus or click, never as a paragraph under it.
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
  searches are kept in the browser.
- **Live.** Each tab follows the organization through one change stream
  ([`GET /v1/changes`](07-code-architecture.md#http-api-v1)): a run's new status, duration or
  count shows in every row and panel at once, without a request. A script, a workflow, a secret,
  a namespace or a connection that changes refreshes the views that show it; a script's title a
  person gives or removes too. The searches on screen are read again a second after the last
  change of a run (at most 5 s after the first of a burst), and their statistics at most every
  10 s. Nothing is read again on a timer. The stream starts where things change: the edge that
  serves it subscribes to the tenant on every node, whose owners push the runs of each durable
  batch, each with its handle and its script's title, and it listens to the changes of the
  control plane ([inside a node](07-code-architecture.md#inside-a-node)). While the stream is
  down, the tab reconnects (after 1 s, doubling up to 30 s), keeps what it shows meanwhile, and
  reads again what is on screen once it is back.
- **Nothing moves on its own.** After a view's first answer, nothing it shows gives way to a
  loading state: a refresh, a reconnection or a resync swaps the data in place, and a search
  the operator changed keeps the previous rows, dimmed, until its answer. Equal data renders
  nothing; rows are keyed by run, so an open menu, the selection, focus and the scroll survive
  every update; cells keep their size. In the tables of runs, of scripts and of namespaces, an
  answer that adds, drops or reorders rows applies at once while the table is scrolled to its
  top; otherwise the rows keep their place, each with its new values, and the answer waits
  behind "N new · Show" in the table's toolbar, until a click, a new search, a return to the
  page, or an answer that arrives with the table at its top. A read after "Load more" asks for
  as many rows as are shown, and a page more.
- **Names, not ids.** A run shows by its handle, `namespace/origin/suffix` (its namespace left
  out where a column of the page says it), its UUID one click away in its details; a script by
  its title, its digest on hover; a workflow version by its number (`triage v12`). A run's page
  is at its handle's path. A script's page gives it a title and a description for whoever holds
  `scripts:write`.
- **Namespaces.** The dashboard looks at every namespace of the organization, or at one: the
  organization › namespace › objects ([decisions](08-decisions.md#taken-for-the-dashboards-hierarchy-november-2026)).
  A picker at the left of every page's header but the organization's sets the scope: its search
  first, then every namespace, then the namespaces, the most active this month first. One
  namespace's views live under `/ns/<name>/` and are the same views (Overview, Observability,
  Scripts, Testing, Tools, Secrets): their reads of runs, statistics and scripts send
  `Rewind-Namespace`. The sidebar's last entry is Namespaces over every namespace, Usage in one.
  In a namespace, a row under the organization picker, above the search, shows it (its mark, its
  display name) and leads back to the same view over every namespace. Picking a scope keeps the
  view, with its query (less its `namespace:` terms), its window and its order; from the page of
  one object (a run, a script, a workflow), it opens the list of its kind. Such a page keeps its
  address; a run's page shows its namespace in the picker and the sidebar. What belongs to no
  namespace is the organization's: there is no default namespace, and a run always belongs to
  one. The dashboard opens over every namespace, where a run's namespace is a column, a filter and
  the term `namespace:`. A namespace's mark is a 5 × 5 mirrored pattern drawn from its name, the
  organization's a round gradient. Wherever a namespace is shown, its
  [display name](06-storage-and-scale.md#namespaces) stands for it when it has one, and its id
  shows on hover; links and the query keep the id.
- **Signing in.** Members sign in with GitHub or a code by email, on the dashboard's pages, and
  pick an organization; someone without one creates it or accepts an invitation. A server
  without WorkOS asks for an API key ([access](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)).
- It is a client of the **public HTTP API**, with no direct access to the store. The SDK,
  the agent tools and the dashboard see the same thing.
- Access is scoped by tenant, since it exposes code and payloads, and by the caller's
  permissions.
- It reads runs from the run index and from their journals, live in their partition or sealed
  in the archive. OTel backends sample, keep data for weeks, truncate attributes and don't
  store source code; a run paused for a month would fall out of them.

### LLM trace and metrics

- **Event stream**: `GET /v1/runs/{id}/events?follow=1` (SSE), for harness UIs and the
  dashboard. The edge subscribes to the run at its owner, which pushes the entries of each
  durable batch; the events of an ended attempt come from its sealed journal.
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

- **Metrics**: runs by status, activation latency, replay duration, batch latency and size,
  ready-queue depth, partitions owned and taken over, tool latency and error rate, inbox depth.
