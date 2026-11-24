# 5. Testing and observability

## Testing

A **test** describes the world a workflow's code meets and what the outside must see at the end.
It never names the code: the code under test is a parameter, so a test written for one version
of a workflow runs unchanged against the next. Tests are always end to end. The engine runs the
code in a test run, and a test checks only how the run ended and the calls it made, never a
variable, a helper or a log line.

### Test runs

A run has a mode: `live` or `test`. A **test run** is a run whose calls a test's world answers,
in virtual time: the dispatcher is replaced, and time is virtual (timers fire immediately, and
logical time jumps to `fire_at`).

Because the engine is deterministic given its ports, a test run is not a separate system: it is
the same engine wired with in-memory adapters and a virtual clock. The engine's own tests are
built the same way. The **driver** runs the engine's actors there (the partition owner, the
executors, the tenant actor) one message at a time on one thread. When none of them has
anything left to do, it moves the virtual clock to the earliest timer of the
[timer wheel](06-storage-and-scale.md#partitions-owners-and-nodes). The same inputs always give
the same steps in the same order.

A test starts at a fixed time with a fixed seed, in virtual time: its result is a function of
the code, the action set, the test and the epoch. The same four give the same report, so a test
cannot be flaky.

A test run reads no secret and signs nothing: a test uses no secret, and `/v1/test` checks
none ([secrets](04-tools.md#secrets)).

### The test document

```json
{
  "name": "closes the stale issues once approved",
  "input": { "repo": "acme/api" },
  "world": {
    "actions": {
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
      { "action": "slack.post_message", "args": { "channel": "#triage" }, "times": 1 },
      { "action": "github.close_issue", "times": 1 }
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
| `world.actions` | For each action, one answer for every call, or a list of rules tried in order. A rule has `args` (the call's args contain them), `returns` (the result) or `error` (`{ message, kind?, data? }`, `kind` as a failed call's), and `times` (how many calls it answers; any number when absent). |
| `world.handlers` | TypeScript handlers that answer the test's calls, executed in a separate isolate so they cannot break the code's determinism. |
| `world.cassette` | Calls and signals recorded from a run: a [cassette](02-history.md#stored-format-v1), matched `sequential` (strict `seq` order) or `by_args` (action and canonical args). Its signals are sent at the time they arrived in the recorded run. |
| `signals` | Sent to the run in order. `when` is `"waiting"` (the default: once the run waits for something other than a timer, which virtual time moves by itself) or a duration of virtual time after the start, such as `"2h"`. |
| `expect` | What must hold at the end; everything not given is not checked. |
| `from` | For a [test from a run](#tests-recorded-from-runs): the run it comes from, the workflow version that run ran and its namespace. |

A call is answered by the first source that has an answer: the rules of `world.actions`, then the
handlers, then the cassette. `rewind test` adds the static responses of `rewind.actions.json`
last. A start of a [workflow](01-product.md#workflows) is answered like any call: `returns` is
the child's output, `error` its failure, and no child runs
([execution](03-execution.md#workflows-across-runs)).

**The world is the list of what the code may touch.** A call no source answers fails with an
`ActionError` ("no recorded response for `github.delete_repo({…})`"), and fails the test, even if
the code catches the error.

| Expectation | Holds when |
| --- | --- |
| `status` | The run is in this state: `completed`, `failed`, `cancelled`, `aborted`, or `waiting` for a run that can go no further in virtual time without the outside world. |
| `output` | The output contains it: objects key by key, recursively; other values equal as JavaScript sees them (`2` and `2.0` are the same number). |
| `error` | The run failed with an error of this `name`, whose message contains `message`. |
| `calls` | For each item, the calls to `action` whose args contain `args`: exactly `times` of them, or at least one when `times` is absent. `times: 0` says a call must not happen. Calls the world answers that `calls` does not list are allowed: a fix that adds a harmless read breaks no test. |
| `order` | `any` (the default), or `listed`: the first call matching each item comes in the order of the items (those with `times: 0` aside). |

### Checked before running

Before a test runs, it is checked against what it runs with, without starting an isolate. A test
that fails a check is reported with its problem and is not run:

- **`stale`**: an action of `world.actions` or of `expect.calls` is not in the action set (the
  `workflows.<name>.start` and `.signal` of the workflows it pins count); a rule's `returns`
  does not match the action's output schema, when the action declares one; a rule's `args` has a
  key that the action's input schema does not declare, when it declares `properties`. An MCP
  server that changed its tools makes the fixtures written for the old ones stale.
- **`incompatible`**, for a test of a workflow: the version's input schema refuses the input;
  a signal is not one the version declares, or its schema refuses the payload. A version that
  changed its interface makes the tests written for the old one incompatible.

A test that runs and does not meet its expectations is **`failed`**.

### Running tests

`rewind test`, `POST /v1/test` and the agent tool `test_workflow` run tests against a workflow's
code: in the CLI process, or on a server node. Each test runs in its own engine with
in-memory adapters; four run at once, and a run of tests takes 5 minutes of wall-clock time at
most. Handlers that cannot load fail their test. The answer is a `TestReport`: for each test,
whether it passed, its problem, its failures (one sentence each, written for a model), the run's
status, output or error, how many times each action was called, and the
[transcript](#transcript-and-metrics) of the run.

```
POST /v1/test   { "code": "…", "actionSet": "…", "tests": [ … ], "workflow": "triage" }
```

With `workflow` (its id or its name, `triage@v12` for one version, else its latest), the
workflow's suite runs too, and checks for incompatibility against that version's interface; the
code and the action set default to the version's. A test reads a workflow and creates or changes
nothing: no workflow, version or test is stored, whatever code it is given, and the tests given
alone are not kept ([workflows](01-product.md#workflows)). For `rewind test`, a tests file is a
JSON array of tests, whose `world.handlers` and `world.cassette` may name files relative to it;
without an action set, the action set is the actions the tests answer, as harness actions. The
tests run in the CLI process, or on the server when they need what it holds: a workflow's suite,
or an action set registered there. This is how a candidate is checked before it becomes a
version: corrected code (`code`), or the version on a new action set whose MCP server changed
(`actionSet`).

### Suites and the gate

A workflow has a **suite**: its tests, which every new version of it must pass. Each version
records the suite it passed, so a version stays immutable: the
[workflow version document](02-history.md#stored-format-v1) holds, besides its code, action set
and schemas:

| Field | Holds |
| --- | --- |
| `base` | The version it derives from. |
| `tests` | The name and digest of each test of the suite it passed. |
| `retired` | The tests of the suite it does not keep: name, digest and reason. |
| `updated` | The tests of the suite it replaced: name, the old and the new digest, and the reason. |

Making a version, the first one (`POST /v1/workflows`) or the next one
(`POST /v1/workflows/{id}/versions`, the agent tool `update_workflow` given code or an interface),
goes through a gate:

1. Its base is the workflow's latest version made under the caller's labels (its lineage: a
   version made under other labels is another's), or none for a first version.
2. Its suite is the workflow's suite, minus those `retire` names, with those `update` names
   replaced, plus `tests`. `retire` and `update` name tests of the suite and give a reason. An
   update gives a whole new test, or only a new `expect` for the one in the suite. Names are
   unique in a suite. A test recorded from a namespace erased since is left out, and listed as
   retired with the reason `recorded from an erased namespace`.
3. Every test is checked, then runs against the version's code and action set.
4. The version is stored only if every test passes, with the next number of its workflow.
   Otherwise nothing is stored, and the answer (422 `tests-failed`) lists each test that did not
   pass, with its problem and its failures.

Tests added to a suite on their own (`POST /v1/workflows/{id}/tests`, `update_workflow` given
only `tests`, `update` or `retire`) go through the same gate against the workflow's latest
version, and make no version: once they pass, they join the workflow's suite, and its next
version lists them in its `tests`, and the retirements and updates made since, with their
reasons, in `retired` and `updated`.

There is no way around the gate. To keep a behavior a test of the suite refuses, the workflow
retires the test because it is deprecated, or updates it because its state or its result is
out of date, and says why. The reasons stay in the lineage: a person reading a version learns
what changed on purpose.

A test is given to the gate inline, by the digest of a stored test (one [recorded from a
run](#tests-recorded-from-runs)), or as `{ "test": "<digest>", "expect": { … } }`: a stored test
with a new expectation, so a model corrects a recorded test without reading its cassette.

A workflow forked from an organization's ([workflows](01-product.md#workflows)) starts with a
copy of its source's suite, which then evolves on its own. Pulling its source's latest version
(`POST /v1/workflows/{id}/pull`) makes it the fork's next version only once the fork's own suite
passes against it, else 422 `tests-failed`: a fork's tests say what its end user relies on.

`retry_run` of a run of a workflow (`POST /v1/runs/{id}/fork`) runs the suite the run's version
passed against the corrected code first; the fork is refused (422 `tests-failed`) if a test
does not pass. A fix that breaks another behavior never reaches production calls. The fork
names a run, not a workflow, so its code does not become a version of that workflow
([forks](03-execution.md#forks)): `update_workflow` makes it the next one.

Tests given to `test_workflow` alone are not stored: a test joins a workflow's suite only
through the gate.

### Tests recorded from runs

Past runs of a workflow have already met the real world: real inputs, real answers to their
calls, real signal timings. Recording turns them into tests, so a new version must keep doing
what the old one did on those inputs.

```
POST /v1/workflows/triage/record   { "query": "status:completed", "limit": 20 }
POST /v1/workflows/triage/record   { "runs": ["0193a5c4-…"] }
```

It takes the completed and failed runs of the workflow, of any version, that the caller sees
(its namespace, then its labels): those `runs` names, or those `query` selects, in the [query
language](#query-language), the 1,000 newest at most. It groups them by **path**: the actions a
run called in `seq` order, the signals it waited for, and how it ended, with consecutive repeats
collapsed (`github.close_issue×2` and `×9` are one path). It keeps the newest run of each path,
up to `limit` tests (20 by default, 100 at most). A few hundred runs usually follow a handful of
paths.

| Field of the test | Taken from the run |
| --- | --- |
| `name` | `recorded <run id>` |
| `input` | Its input. |
| `start`, `seed` | Its start time and its seed, so `Date.now()` and random values are what they were. |
| `world.cassette` | Its calls, matched `by_args`, the outcomes of the children it started included, and its signal timeline. |
| `expect` | Its status, its output or its error, and how many times it called each action. |
| `from` | The run, its version and its namespace. |

A recorded test says: on this real input, the new version makes the same calls with the same
args and returns the same output. Any difference fails it. For a failed run, the model gives the
expectation it should have met (`{ test, expect }`), which turns the run into a regression test.
Both are tests from a run: `from` names the run, and the expectation is its outcome, or the one
written. After an intended change, the model retires the recorded tests it affects, with one
reason, and records again from the new version's runs.

Recording stores the tests and returns, for each, its digest, name, path and run. It adds them
to no suite: a test joins a suite only when the gate is given it. A run whose test would be
larger than 4 MiB is skipped, and named in the answer.

A recorded test holds the run's data and its seed, with which the run's random values (tokens
its code generated included) can be computed again. It is read like the run: only a caller who
may read the runs of its namespace reads it. The gate runs it whoever makes the version.

### Where tests are stored

A test is a document of the [stored format v1](02-history.md#stored-format-v1), stored as a blob
in the `BlobStore` (AWS S3 in production) under its tenant and the digest of its canonical JSON
(`tests/{tenant}/sha256/{digest}`). A workflow version holds digests only. The `tests` table of
the control plane indexes the blobs, with the namespace a recorded test came from, and
`workflow_tests` the tests of each workflow's suite
([where data lives](06-storage-and-scale.md#where-data-lives)). A test lives as long as a suite
or a version references it; a daily job of the control plane deletes one nothing references,
24 hours after it was stored. A recorded test is erased with its namespace
([retention](06-storage-and-scale.md#retention-and-deletion)).

## Replay, debugging and forks

- `rewind replay <run>`: fetches the history and the code, re-executes them locally, and
  checks determinism. No action is called. `<run>` can also be a run export: the replay then
  runs in the CLI process, without a server.
- `rewind replay <run> --code fixed.ts`: tells whether corrected code is compatible with an
  existing history, and if not, where it diverges.
- `rewind fork <run> --code fixed.ts [--at <index>] [--input …]`: a new run that reuses the
  recorded results of its source, then continues live. An agent can fix a bug at step 7
  without redoing the side effects of steps 1–6. The agent tool is
  `retry_run({ run_id, code })`, and the API `POST /v1/runs/{id}/fork`.

A fork rests on two things the engine already guarantees:

- What `rewind replay --code` checks: the new code is replayed against the source's
  history, and as long as its commands match, the recorded outcomes are reused.
- A run that ends by itself has a recorded outcome for every call it scheduled
  ([how a run ends](03-execution.md#how-a-run-ends)), so the fork knows which
  effects happened.

What a fork copies, when it goes live, and which idempotency keys its calls carry:
[forks](03-execution.md#forks).

## Evals

Whether the system works for models is measured, not assumed. The evals run models end to end
on the embedded engine: a task, an action set of deterministic fake MCP servers, and the loop
generate → check → run → fix, in virtual time.

Reported per model and per prompt rendering:

- check pass rate on the first try;
- task success;
- number of iterations;
- tokens.

Test runs make this cheap: runs are hermetic and take no wall-clock time. The evals also
replay cassettes recorded from real runs.

Repair tasks give the model a workflow with its suite and a failed run of it: it succeeds when
its fix passes the gate. They also measure whether models write tests with their code
unprompted, and how often the gate refuses a fix they believed correct.

The models evaluated are Claude Opus, Sonnet and Haiku, and one OpenAI model. The evals run
on demand, not in CI: each run costs model tokens.

The choices that are cheap to change today (names, error formats, the `rewind` library, the
prompt rendering, what `run_workflow` returns) are decided on these numbers.

## Observability

**The history is the record; telemetry and dashboards are projections of it** (principle
10 in [07](07-code-architecture.md#principles)). Nothing about what a run's code did is
emitted from the isolate, from the runtime, or during replay. An activation can replay
the whole history, so telemetry emitted while executing would be emitted again on every
replay.

### Two kinds of telemetry

| | Service telemetry | Run telemetry |
| --- | --- | --- |
| Covers | The platform itself: activations, replay duration, batches and their latency, partitions owned and taken over, the ready queue, store reads, V8 heap, the dispatcher's deliveries and redeliveries | What each run's code did: calls, timers, signal waits, logs, outcome |
| Source of truth | The process | The history |
| Emitted by | The `tracing` facade in `rewind-engine` and the adapters, live | The projector, from durable batches |
| Exported by | The `tracing` subscriber from `rewind-otel`, installed by `rewind-server` and `rewind-cli` | `rewind-otel`, through the `SpanExporter` port |

Both land in the same OTel trace (see the span model below): the dispatcher's Delivery spans
appear under the Call span projected from the history. The engine only records the run's trace
id and the parent span id as `tracing` fields; `rewind-otel` turns them into OTel context, so the
engine never depends on OpenTelemetry.

### The projector

The projector turns durable batches into run telemetry: OTel spans and logs, and the
webhooks. It consumes the partition's
[outbox](06-storage-and-scale.md#the-outbox). The batch that commits an
activation also writes one outbox item per projection (`otel`, `webhooks`), naming the events
it committed: `(run, from_idx, to_idx)`. Once the batch is durable, the owner hands those events
to the projector in memory. A later batch deletes the outbox item, once the events are exported
or their webhooks posted. An owner that recovers the partition feeds the projector again with the
items left in the outbox. `quota.alert` takes the same path, from the outbox of the tenant
actor's home partition.

- Each projection happens at least once. Every projection is idempotent, keyed by deterministic
  span ids, or by the deterministic id of a webhook.
- One outbox item per projection, so a slow OTel backend never delays the webhooks.
- A webhook its subscription's endpoint does not accept is posted again with backoff, each time
  on a timer of the [timer wheel](06-storage-and-scale.md#partitions-owners-and-nodes). Its outbox
  item stays until the webhook is accepted or given up.
- The run index and the counters are written by the owner in the same batch as the facts they
  describe ([read model](#read-model)). A list or a count is never behind the history.
- A projection can be rebuilt by re-reading histories and sealed attempts: backfill, a new
  projection, a bug fix.

The event stream of a single run ([below](#transcript-and-metrics)) takes another path: the edge
subscribes to the run at its owner, which pushes the events of each batch once it is durable.

### Span model

One OTel trace per run.

| Span | Span id | Start → end | Exported by |
| --- | --- | --- | --- |
| Run | `H(run, "run")` | `run_started` → the event that ends the run | Projector, when the run ends |
| Activation | `H(run, "activation", idx)`, `idx` being the position of the first event it commits | taken from the ready queue → its batch durable | Engine (service telemetry), once its batch is durable |
| Call | `H(run, seq)` | `schedule_call` → completion | Projector, when the completion is committed |
| Delivery | random | dispatch → response | Dispatcher (service telemetry), child of the Call span |
| Timer | `H(run, seq)` | `start_timer` → `timer_fired` or `cancel_timer` | Projector |
| Signal wait | `H(run, idx of signal_awaited)` | `signal_awaited` → `signal_received` or timeout | Projector |

- **Trace id**: the harness's trace id if a `traceparent` was given when the run started
  (the run span is then a child of the harness's span, typically the LLM call that wrote
  the code); otherwise the run id (a UUID v7 and an OTel trace id are both 128 bits).
- **Deterministic span ids** (`H` is a truncated SHA-256). A span exported again after a
  crash is the same span. Live service spans (activations, the dispatcher's deliveries) can
  name the spans projected from the history as parents before those are exported.
- **The run span is exported when the run ends**, but its id is known from the start, so
  every child links to it right away. A short run appears as a complete tree. A run
  paused for days shows its children under a parent that appears when the run ends.
- **Logs** are exported as OTel log records, correlated with the trace and the activation
  span.
- **Attributes**: `rewind.run_id`, `rewind.code` (its code's digest), `rewind.action_set`,
  `rewind.seq`, `rewind.action`, `rewind.callsite` (TypeScript file and line), outcome and error
  kind, deliveries, run labels. Payloads appear as digest, size and preview, **never as values**:
  arguments are written by a model and may hold sensitive data, and OTel backends have weaker
  access control than the store.
- **Sampling** is decided per run from the trace id, so every node and every
  activation agrees without coordination. The read model is never sampled.
- **Context propagation**: calls forward the W3C trace context (the Call span as parent) to MCP
  servers in `_meta`, next to the idempotency key, and to harness actions through the SDK. A
  child run of a workflow takes the trace context of the call that started it: its run span is a
  child of that call's span, so a tree of runs is one trace.
- In test runs and virtual time, nothing is exported by default: timestamps are virtual.

### Run metadata

Given by the harness when it starts a run and stored with the run. The code never sees
it, and it is not part of the history's events, so it is never replayed and never ends
up in a cassette.

- `namespace`: the end user the run belongs to
  ([namespaces](06-storage-and-scale.md#namespaces)). A fork has its source's.
- `labels`: a string map (agent, conversation, environment…), used to filter in the dashboard
  and as span attributes.
- `trace_context`: the W3C `traceparent` and `tracestate`.
- `idempotency_key`: see [storage](06-storage-and-scale.md#idempotency-of-the-api).
- `secrets`: the secrets the run uses, a scope and a name per server, never a value
  ([secrets](04-tools.md#secrets)). A run started by naming a workflow uses its version's.
- The tenant ([decided](08-decisions.md#taken-after-the-layering-november-2026): it lives here, never in the core).
- For a fork, its source: the run it was forked from ([forks](03-execution.md#forks)).
- Its [workflow](01-product.md#workflows) and version: every run is a run of a workflow. For a
  child run, the run that started it (its parent), the call's `seq` and whether it is detached;
  the allowed labels it inherited ([execution](03-execution.md#workflows-across-runs)).

### Read model

Questions across runs are answered by each partition, from records its owner writes in the same
batch as the facts they describe ([where data lives](06-storage-and-scale.md#where-data-lives)).
There is no separate store to fill: a list or a count is as recent as the partition's last
durable batch, and no query reads a history.

- **The status.** The partition owner computes a run's status at each batch, from the history
  and the waits it already stores in the run's head: `queued` until a step is committed;
  `running` while a step is in progress, or while a call Rewind makes itself (to an MCP server,
  an API or `http`) is in flight or backing off; else `waiting`, until the run ends
  (`completed`, `failed`, `cancelled` or `aborted`). It writes the status in the run's head and
  its summary. A run is active until it ends: queued, running or waiting.
- **The run index.** One summary per run, active or ended, kept until the run is erased:
  namespace, status, its workflow (its id and name) and version, its code's digest and the first
  line of its top comment, action set, labels, mode, the source of a fork and the run that
  started it, start and end, last activity, number of calls and errors, history length, the
  actions it called; for a run that waits, what it waits for (`waitingFor`): a signal (its name,
  the timeout as wake time), a timer (its due time), a call (its action and its state: `ready` or
  `claimed` for a harness action, `approval`, `deferred` or `parked`), each with the event that
  opened it and its callsite, or its quarantine; for a run that ended badly, the error's name,
  message, line and fingerprint. Summaries are kept in the order of their start under their
  namespace, and under each label's value, so a namespace's runs, or those carrying a label, are
  read in order without reading the others.
- **Counters.** Per tenant, namespace and hour: the runs started and ended, by status and by
  workflow, with a sketch of their durations; the failures by fingerprint, with the workflows and
  lines they came from and their latest runs; per action, the calls, the errors by kind, a sketch
  of the latency and the redeliveries; per workflow, the runs by status and the actions it called.
  Per tenant and namespace, the gauges of the active runs now: by status, queued, running and
  waiting, and the waiting ones by what they wait for, the quarantined ones among them. Per
  tenant, namespace and action, the shape observed of its results, which merges in any order.
  They are what the [statistics](#statistics) read.
- **Fingerprints.** A run that failed, was cancelled or was aborted gets a fingerprint: its
  error's name and its message with numbers, UUIDs, long hexadecimal ids, times and
  double-quoted strings replaced by placeholders (for an abort, the abort's kind and message).
  Runs with the same fingerprint are counted together, across the many workflows models write.
  The engine computes it in the batch that ends the run, so every adapter stores the same value.
- **Service state.** Quarantine and the state of a call's task are not in the history. They
  live in the run's head, which the owner writes in its batches, and the summary shows them as
  they are: the run's quarantine, and for each call its delivery and whether it waits for an
  approval, is parked or deferred.

It answers "which runs called `github.close_issue`", "every workflow agent X ran this week",
"which runs wait for an approval" from the index, and "failures by error kind", "latency and
error rate per action" from the counters. An analytics engine over every history, for questions
nobody planned, is out of 0.1; it would read the sealed attempts of the
[archive](06-storage-and-scale.md#where-data-lives).

### Query language

Runs are searched with one query language, the same in the API
(`q`), the dashboard's query bar, the SDK and `rewind runs`. Terms are separated by spaces
and all must hold; a comma inside a term means "or"; a leading `-` excludes.

| Term | Matches runs |
| --- | --- |
| `namespace:alice`, `namespace:alice,bob` | in one of these namespaces |
| `@agent:triage` | carrying the label `agent` with value `triage`. `@customer:acme*`: a value starting with `acme`; `@conversation:*`: the label is set |
| `status:failed,aborted`, `status:active` | with one of these statuses: `queued`, `running`, `waiting`, `completed`, `failed`, `cancelled`, `aborted`; `active` for the three that have not ended |
| `action:github.close_issue`, `action:github.*` | that scheduled a call to this action, or to an action of the group `github` |
| `error:timeout`, `error:TypeError` | with a call that failed with this kind, or that ended with this error name (or abort kind) |
| `waiting:*`, `waiting:signal,approval` | waiting, for: `signal`, `timer`, `harness` (a harness action's call, ready or claimed), `approval`, `deferred`, `parked`, `quarantined` |
| `signal:approval` | waiting for this signal |
| `started:>2026-11-01`, `started:>now-24h`, `ended:<…`, `activity:>…` | by time: a date, a time, or `now` minus a duration; `>`, `>=`, `<`, `<=`, or a range `a..b` |
| `duration:>5m`, `calls:>=20`, `errors:>0` | by duration (ended runs), number of calls, number of failed calls |
| `action_set:…`, `id:0193a5c4` | by digest or id prefix: 8 hexadecimal digits at least |
| `source:<run id>` | forks of this run |
| `workflow:triage`, `workflow:wf_7k2m9q4x8d1c`, `started_by:<run id>` | runs of this workflow, by its name or its id; runs this run started |
| `fingerprint:<fingerprint>` | with this fingerprint |
| `mode:test` | by mode (`live`, `test`) |
| `rate`, `"rate limit"` | containing this text, case-insensitive, in the error's message, the first line of its code's top comment, a label's value or the run's id |

Field names are case-sensitive and a term that cannot be read is refused with its position
(`invalid-query`), so a client can underline it. Logs and payloads are never searched: they may
hold what an action returned, which the index does not hold. The server limits every query
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

`GET /v1/runs/stats`, `GET /v1/runs/failures`, `GET /v1/actions/stats` and the counts of
`GET /v1/workflows` answer from the [counters](#read-model): they read the partitions that hold
their scope, and the edge adds the counters up. Their answers keep their shapes. They take a
scope and a window:

- **The scope** is the caller's tenant, or one namespace: a token's, the `Rewind-Namespace`
  header's, or the one a query `q` names with a single `namespace:` term. Any other term of `q`
  is refused with its position (`invalid-query`).
- **The window** is made of whole hours, in UTC: its bounds are rounded out to the hour. A
  series over it steps by its `interval`.
- **Facets** count the runs of the scope and the window by status, by workflow and by namespace.
  The runs waiting now, by what they wait for, come from the gauges of the active runs.
- **Fingerprints** list the fingerprints of the window with their counts per hour, the workflows
  and the lines they came from, and their latest runs. A fingerprint's labels are those of its
  latest runs, read from the index.
- **Action statistics** are per action (`action`), filtered by its group (`group`, the start of
  its name: `github`); **workflows** list each workflow with its runs by status and the actions it
  called.
- A grant or a token limited to labels reads none of them (403): counters do not know labels.

The Overview, the Actions and Workflows views and a namespace's page are drawn from them; the
runs behind each of their marks come from a search.

### Dashboard

The dashboard shows every run and every workflow executed on the platform. It is a React
single-page application, served by `rewind-server`.

| View | Shows |
| --- | --- |
| Overview | The home page: runs by outcome over time, success rate, durations, runs waiting now, quarantined runs, what fails most, failing actions; every mark opens the runs behind it in Runs |
| Runs | The runs, looked at and filtered. Four views: All (the query bar, with suggestions drawn from the facets, the runs over time of the namespace and the window shown, and the results), Running, Waiting (the runs waiting on the outside world, grouped by what they wait for: signals, timers, harness actions, approvals, deferred and parked calls, quarantines; with the line each is paused on and when it wakes) and Failures (failed, cancelled and aborted runs by fingerprint: count over time, workflows, labels, the line most runs failed at, its latest run by its handle). Its sidebar holds the views and the filters: the statuses, workflows and namespaces of the window with their counts (facets), which add or remove their term in the query. The table's headers order the runs by their column (newest or oldest first, latest activity, longest, most calls, soonest to wake) and filter what each shows, as terms of the query |
| Run | One run, analysed in depth. At its top, how it ended; a cancellation is told as a cancellation, not a failure: its cause (its deadline, a request, its own code) and what the run awaited then ([decisions](08-decisions.md#taken-for-the-dashboards-type-scale-november-2026)). Its header holds its handle ([handles](06-storage-and-scale.md#run-handles)), its Test run or quarantine tags and its Controls: its forks, beside them a menu of the controls its permissions allow (send a signal, with the signals its version declares and their payloads' schemas; cancel; retry a failed run), Export run and Export cassette. Its sidebar holds no control: its sections and its details (namespace, workflow and version, the run that started it, code, action set, labels, the secrets it uses). Its sections: the debugger (the code, with each call, timer and signal next to the line that caused it; the timeline; payloads loaded on demand; live through SSE; a time cursor), the History, the logs, and the runs it started ([decisions](08-decisions.md#taken-for-the-dashboards-sidebar-and-run-page-november-2026)) |
| Workflows | Every workflow of the organization and of its namespaces, or of one namespace (`namespace:`), by how it is shown ([names](04-tools.md#workflow-names)), with its scope (its namespace or the organization), its run counts, outcomes and actions used in the window (the last hour, day, 7 or 30 days). A workflow's page (`/workflows/:id`) has its own sidebar: its versions, the suite (each test a click away, its origin as a glyph) and the version's details. It opens at one version, switched in the breadcrumb (the latest by default): its code, its interface and its [suite](#suites-and-the-gate) as a table, each test with its origin (inherited, added, recorded, updated, retired, with the reasons), its input, what it expects, the calls it checks and its world; a fork shows its source and the version it last pulled. Each test has its own page (`/workflows/:id/tests/:digest`), switched in the breadcrumb or with previous/next: its input and expected output, its world, signals and expectations; every JSON value opens whole, colored, in a dialog, its objects and arrays folding. No test result is shown: every test of a suite passed when it joined it. For whoever holds `workflows:write`, its header names and describes the workflow, promotes a namespace's workflow to the organization, forks an organization's into a namespace, and pulls a fork's source |
| Actions | Latency, error rate and redeliveries per action, in the window, for the organization or one namespace, filtered by group (the start of an action's name) |
| Namespaces | The alerts of the month first, then the tenant's ceiling and the default limits, then the namespaces sorted by CPU, runs or workflows of the month, each with its use against its limits (orange from 80 %, red at 100 %); a search by the start of a name or of a display name with the runs' query bar, and a filter of those above 80 %. A namespace's page, over a month: its alerts first, when it has any; how its runs went (started, completed, failed, waiting now, durations, the runs of each day by outcome, what fails most, the actions they call), its cumulative CPU against its quota, its tokens in use, its exceptions, then its runs of the month, in the Runs table forced to the namespace and the month (its headers order and filter them), and its workflows; and the controls its permissions allow (rename it, change its limits, revoke its tokens, delete it) |
| Secrets | The organization's secrets, then a namespace's on its own page: each with its labels, who wrote it and when, its reads per day and its last read, and the runs and workflows that use it. A value is written, never shown. Deleting one first shows what it would stop: the active runs that use it, those waiting on a call that reads it, the workflows whose versions use it ([secrets](04-tools.md#secrets)) |
| Organization | Opened from the member's menu: the members and their grants, the invitations, the roles and their permissions, the API keys, the signing keys and the address of their public keys, the settings with the organization's id; each part shown to whoever holds its permission |

- **One sidebar.** The organization picker and a search of runs and views sit at its top. Below,
  the views; a view that needs more than a toolbar (Runs, Run, a workflow, Organization) turns the
  sidebar into its own, with a "back" at its head: from Runs or Organization it shows the views
  again without leaving the page, from a run it returns to the list it was opened from. The
  member's menu, at the bottom, holds the organization, the theme, the time zone and the
  sign-out. The sidebar holds no control: a page's controls are in its header. A click on its
  right edge folds it to its icons, and a click on the rail's edge unfolds it; a drag of the edge
  resizes it.
- **Explanations.** What a page or a section is opens from an (i) beside its title, in a card on
  hover, focus or click, never as a paragraph under it.
- **The time cursor.** The commit invariant makes a history a sequence of steps: a trigger
  (the start, a call's result, a timer, a signal, a cancellation) and the commands committed
  with it. The Run page steps through them like a debugger: at each step the code marks the
  awaits that just resumed, the calls issued, those still open and those done; hovering a call
  shows its arguments and result as known at that step, and a variable bound to an awaited
  call (`const x = await call()`, one level of destructuring) or `input` shows its value; a
  click pins it as a watch. The cursor is in the URL (`?step=`), the timeline shows it, and on
  a live run it follows the head. Only values that crossed the history are known: what the
  code computes in between is not recorded, and the dashboard does not execute code.
- **Searches.** The query follows the operator from one view to another in the URL. Saved
  searches are kept in the browser.
- **Live.** Each tab follows the organization through one change stream
  ([`GET /v1/changes`](07-code-architecture.md#http-api-v1)): a run's new status, duration or
  count shows in every row and panel at once, without a request. A workflow, a secret, a
  namespace or a connection that changes refreshes the views that show it, a workflow named,
  described, given a version, promoted, forked or pulled included. The searches on screen are
  read again a second after the last change of a run (at most 5 s after the first of a burst),
  and their statistics at most every 10 s. Nothing is read again on a timer. The stream starts
  where things change: the edge that serves it subscribes to the tenant on every node, whose
  owners push the runs of each durable batch, each with its handle and its workflow, and it
  listens to the changes of the control plane
  ([inside a node](07-code-architecture.md#inside-a-node)). While the stream is down, the tab
  reconnects (after 1 s, doubling up to 30 s), keeps what it shows meanwhile, and reads again
  what is on screen once it is back.
- **Nothing moves on its own.** After a view's first answer, nothing it shows gives way to a
  loading state: a refresh, a reconnection or a resync swaps the data in place, and a search
  the operator changed keeps the previous rows, dimmed, until its answer. Equal data renders
  nothing; rows are keyed by run, so an open menu, the selection, focus and the scroll survive
  every update; cells keep their size. In the tables of runs, of workflows and of namespaces, an
  answer that adds, drops or reorders rows applies at once while the table is scrolled to its
  top; otherwise the rows keep their place, each with its new values, and the answer waits
  behind "N new · Show" in the table's toolbar, until a click, a new search, a return to the
  page, or an answer that arrives with the table at its top. A read after "Load more" asks for
  as many rows as are shown, and a page more.
- **Names, not ids.** A run shows by its handle, `namespace/origin/suffix` (its namespace left
  out where a column of the page says it), its UUID one click away in its details; a workflow by
  its name, else the first line of its code's top comment, else its short id (`wf-7k2m9q`), its
  id and its code's digest on hover; a workflow version by its number (`triage v12`). A run's
  page is at its handle's path, a workflow's at its id.
- **Namespaces.** No namespace is picked in the dashboard: it reads every namespace of the
  organization. A run's namespace is a column, a filter and the term `namespace:` of the query.
  Wherever a namespace is shown, its [display name](06-storage-and-scale.md#namespaces) stands for
  it when it has one, and its id shows on hover; links and the query keep the id.
- **Signing in.** Members sign in with GitHub or a code by email, on the dashboard's pages, and
  pick an organization; someone without one creates it or accepts an invitation. A server
  without WorkOS asks for an API key ([access](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)).
- It is a client of the **public HTTP API**, with no direct access to the store. The SDK,
  the agent tools and the dashboard see the same thing.
- Access is scoped by organization, since it exposes code and payloads, and by the caller's
  permissions.
- It reads runs from the run index and from their histories, live in their partition or sealed
  in the archive. OTel backends sample, keep data for weeks, truncate attributes and don't
  store source code; a run paused for a month would fall out of them.

### Transcript and metrics

- **Event stream**: `GET /v1/runs/{id}/events?follow=1` (SSE), for harness UIs and the
  dashboard. The edge subscribes to the run at its owner, which pushes the events of each
  durable batch; the events of an ended attempt come from its sealed attempt in the archive.
- **Transcript**: the run as text for a model, a header then one line per event:
  `GET /v1/runs/{id}/transcript`, `rewind transcript <run>`, and what the agent tool `get_run`
  returns. Payloads are truncated, and each event can be expanded individually
  (`rewind transcript <run> --event 7`, `GET /v1/runs/{id}/events/{idx}`).

```
run 01JC…7Q  waiting · for signal "approval" (timeout in 2d 23h)
workflow triage v3 · action set 9f2c4e1a07b3 · started 2026-11-09T10:02:11Z · cpu 180ms

  #  at      event                                                       line
  1  +0.0s   github.list_issues({repo:"acme/api",state:"open"})            L5   → 37 items (12.4 KB) 310ms
  2  +0.3s   slack.post_message({channel:"#triage",text:"3 stale…"})       L8   → ok 120ms
  3  +0.4s   timer #3 started 3d (signal "approval" timeout)               L10
```

- **Metrics**: runs by status, activation latency, replay duration, batch latency and size,
  ready-queue depth, partitions owned and taken over, call latency and error rate
  (`rewind.call.*`), deliveries and redeliveries (`rewind.delivery.*`), inbox depth.
