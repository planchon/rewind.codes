# 3. Execution model

## Starting a run

Every run is a run of a [workflow](01-product.md#workflows). A start either names one, by its id
or its name, with `@v12` or `@<digest>` for a version, else its latest: the run executes that
version's code, with its pinned action set and the secrets it uses. Or it gives code, with an
action set and the secrets its servers need, and names no workflow (giving both is refused,
422):

- The run is a run of the workflow that already has that code: looked up in the namespace,
  then among the organization's workflows, as names are; of that workflow's version with that
  code, the latest one if several have it.
- Otherwise the start creates a workflow in the run's namespace, with an id the API assigns and
  a version 1 of that code: without an interface (any input, output and signal name), with the
  run's action set and secrets. The same code sent again in that namespace finds it.
- Either way, the run executes against the action set and the secrets its request gives, even
  when its code is already a version of a workflow, as a run given code always does.

Starting a run checks its code first and refuses it at once when it cannot run: the agent
that wrote it learns in the same call, not from an aborted run later. No run is created, no
workflow either, and nothing is stored. Refused at start, with a diagnostic written for the
model (a line and a column in `workflow.ts`):

- a syntax error;
- an import of anything other than `rewind`, `rewind:actions` and `rewind:workflows`, a dynamic
  `import()`, a `require`;
- an import of a name `rewind` does not export, of an action the run's action set does not have,
  or of a workflow it does not pin;
- no default export, or a default export that is certainly not a function (a literal, an
  object, a class);
- more lines than its namespace's `maxCodeLines`
  ([quotas](06-storage-and-scale.md#quotas));
- an import of a server or an API that needs a secret, without a secret named for it; a secret
  that does not exist, or that the caller may not use
  ([secrets](04-tools.md#secrets)).

A run is started in a namespace, the end user it acts for, and the start must pass that
namespace's quotas and its tenant's, which the tenant actor checks before the run is created
([quotas](06-storage-and-scale.md#quotas)). A start that creates a workflow also counts toward
the namespace's `workflows` quota.

Starting a run also type-checks the code against the action set's declarations, as
`rewind check` does, so that most mistakes are refused before a run exists. A run of a workflow
named by its id or its name is not checked again: its version was checked when it was made.

## Activations

Completions never go straight into the history. They land in a per-run **inbox**. A run lives
in its namespace's partition, and each partition has one **owner**, the only node that writes
it ([partitions](06-storage-and-scale.md#partitions-owners-and-nodes)). The owner moves the
triggers into the history one activation at a time, interleaved with the commands they cause:
each trigger, with what the code emitted in response, is one **step**, and an activation
commits one or more steps. The history therefore has exactly one writer at any time, which is
what makes its order meaningful.

```
a durable batch puts a trigger in R's inbox: the owner queues R on its node's ready queue
an executor takes R; R is busy until its activation is committed
instance ← cached instance whose history length matches, or new instance + replay(R.history)
for trigger in R.inbox (arrival order):
  recorded_at ← max(now, last recorded_at)          # monotonic per run
  if R has ended: drop the completion
  append trigger; hand it to the instance; run microtasks until idle
  append the commands and annotations emitted
move payloads above 1 KiB out of the events (payload rows, or the BlobStore above 64 KiB)
derive effects from the new commands: the calls' tasks, timers, cancelled timers
the owner commits { events, consumed inbox items, effects, status } in its next batch
once that batch is durable: calls are sent, timers set; the instance stays in the cache
if triggers reached R's inbox meanwhile, R is queued again
```

Commands and the effects they create (the calls' tasks, timers) are committed in the same batch,
so an effect can never be lost or duplicated by a crash between the two. Nothing an activation
causes leaves the owner before its batch is durable: a call is sent, a timer set, a subscriber
told only after. A node that lost its partition cannot commit: the node that opened it since
fenced it. The same batch writes the outbox items that the projections read (telemetry and
webhooks, see [05](05-testing-and-observability.md#observability)).

The status in that batch is the one the owner computes at each batch, from the waits it stores
in the run's head: `queued` until the first step is committed, `running` while a step is in
progress or a call Rewind makes itself (to an MCP server, an API or `http`) is in flight or
backing off, `waiting` otherwise, until the run ends ([run state](02-history.md#run-state)).
The owner writes it in the run's head and summary whenever it changes, in the same batch as the
change that caused it.

While R is busy, the triggers that reach it are committed to its inbox by the owner, in its own
batches, and wait for the next activation. The owner groups what reaches it into batches: it
writes, as one batch, everything that arrived while the previous batch was being written, so
the activations of many runs share one write.

The executors are threads of a node that run V8. The owner runs an activation on its own
node's executors when its build supports the run's [epoch](#determinism-epoch). Otherwise it
sends the activation through the node link to a node whose build does: the request carries
what the executor needs, and the result comes back to the owner, which commits it as always
([inside a node](07-code-architecture.md#inside-a-node)).

The ready queue is fair across tenants: it takes the tenants with runnable runs in turn, and
within a tenant the runs with the fewest unfinished activations first, then the run whose
oldest trigger has waited longest.

Failures of the activation itself:

- The code fails to load although it passed the checks at start (an exception thrown by
  its top-level code, a default export that turns out not to be a function) →
  `run_aborted { invalid_code }`, with the diagnostic.
- Replay detects non-determinism → nothing is committed, and the run is
  [quarantined](#non-determinism-and-quarantine).
- A limit is exceeded → `run_aborted { limit_exceeded }`.
- Anything else (a missing blob, a store error, a replay interrupted by a limit or an
  internal error of the executor) → nothing of the activation is committed. The owner
  records the failure in the run's head, with a timer 30 s later, and activates the run again
  when it fires; the other runs are activated meanwhile. After **5 such failures in a row**,
  the run is [quarantined](#non-determinism-and-quarantine) with the last error: a run that
  cannot make progress is set aside and reported, not retried forever.
- The node dies during the activation (the code crashed the process, the container was
  killed for its memory, a native dependency failed) → nothing is committed and nothing is
  recorded. This is a failure too, counted when the partition is recovered: the node that
  takes it over adds one to `unfinished`, the activations that never finished, of every run that
  was runnable, since one of them may have been running. A commit resets the count. An
  executor that takes a run whose `unfinished` is above 5 quarantines the run instead of
  activating it. Code that kills its nodes does not go from node to node forever, and the
  runs that were only waiting commit before it runs again, since the ready queue takes the runs
  with fewer unfinished activations first.

## Inside the isolate

Nothing is asynchronous from V8's point of view:

- Ops are synchronous: they record a command, assign a `seq`, and return it.
- Values cross the boundary as JSON. A value reaches the code with its object keys in
  canonical order (RFC 8785), so that it looks the same live and on replay, wherever it was
  loaded from. A string holding a lone surrogate (half of an emoji, after a `slice`) leaves
  the code with U+FFFD in its place.
- The run's output and the description of its failure are captured when the outcome is
  known: serializing them runs the workflow's code (`toJSON`, getters), which cannot happen
  once the ending command is decided.
- The JS glue turns the `seq` into a promise whose resolver is stored by `seq`.
- Handing a completion to the code calls the glue's `deliver(completion)` and runs a microtask
  checkpoint.
- There is no event loop and no real timer inside the isolate.

V8 objects cannot change threads, so each instance of a workflow's code lives on a dedicated
thread behind the `CodeExecutor` port. The engine talks to it through channels and never
handles a V8 type.

## Logical time

`Date.now()` returns the `recorded_at` of the **last trigger handed to the code**
(`run_started` for the first activation). Time only moves when the outside world speaks, and
it moves identically on replay. `recorded_at` is monotonic per run (`max(now, last)`).

After `await sleep("10d")`, `Date.now()` is at least start + 10 days, because
`timer_fired` is recorded after `fire_at`. `performance.now()` is the logical time
elapsed since the run started.

In virtual time (tests, test runs), the driver jumps the clock straight to the earliest
timer of the timer wheel: a timer's `fire_at`, a call's next delivery after its backoff, a
deadline. A 10-day sleep takes no wall-clock time.

## Replay

Replay creates a fresh instance, runs `main(input)`, and walks the history with
`ReplayCursor` (pure, in `rewind-core`):

- When the next event is a **completion**, hand it to the code and drain microtasks.
- Each **command** the code emits must match the next command in the history: same
  kind, same `seq`, same action, same args (compared by payload digest). Callsites are
  ignored, so moving code around without changing behavior is allowed. A `fail_run`
  matches on error name and message.
- If the history expects a command and the code goes idle without emitting it, that is
  non-determinism too.
- Annotations are skipped. Thanks to the commit invariant, every log emitted during
  replay is already recorded, so logs are suppressed. No telemetry about the run is
  emitted during replay either: run telemetry is projected from committed events
  ([05](05-testing-and-observability.md#observability)).
- If the history ends with `run_aborted`, replay stops there.
- At the end of the history, the code is in exactly the state it was in after the last
  commit, and the run continues live.

Errors are written to be read by a model:

```
history event 1 is call #1 github.close_issue({"number":4}), but the code emitted call #1 github.close_issue({"number":5})
the code emitted call #3 slack.post_message({…}), which the history does not contain (at event 6)
history event 4 is timer #2 (3000ms), but the code went idle without emitting it
```

The history itself is validated first: not empty, starts with exactly one
`run_started`, contiguous positions, nothing after `run_aborted`.

Code is immutable and content-addressed: a run replays the exact code it started with, and a
new version of its workflow never changes it, so there is no versioning problem for a run.
Two risks remain:

- A change in the Rewind runtime itself: see the [determinism epoch](#determinism-epoch).
- An action set whose schemas changed while the run was waiting: dispatch checks the call
  against the current schema and parks it
  ([action set change](04-tools.md#when-an-action-set-changes-during-a-run)).

Replaying the full history on every activation would be O(history²) over a run's life.
Mitigations:

- [Sticky execution](#sticky-execution): the executors keep the instance in memory, and the
  run's next activation runs on them. Replay is the fallback, not the hot path.
- Histories are capped (default 50k events). A run that reaches the cap is aborted with
  `limit_exceeded`; `continueAsNew` is out of 0.1.

## Sticky execution

After a commit, the executor leaves the instance of the run's code in its node's **instance
cache**. Only the owner of the run's partition activates the run, so its next activation runs on
the same node and finds the instance there while it is cached.

- The cache is an LRU, bounded per node: 500 instances and 4 GiB by default. An instance's
  memory is its isolate's, as V8's heap statistics give it after each call into the code.
- An owner that sends a run's activations to another node, for an epoch its own build does
  not support, sends them to the same node while that node lives, so the instance stays warm
  there.
- **Hit**: the instance is cached, and its history length is the run's. Only the new trigger
  is handed to it: no replay.
- **Miss**: a new instance starts from a V8 startup snapshot and replays the history. A run
  whose partition moved to another node, or whose instance was evicted, misses once.

Each live instance keeps its own thread and isolate. The load tests check that this holds
within these bounds. An isolate pool per thread is built only if they fail.

The cache also saves most payload reads ([payloads](06-storage-and-scale.md#payloads)).

## Forks

A fork is a new run started from an existing one, its **source**, with corrected code, so that
an agent can fix step 7 without redoing the side effects of steps 1 to 6
([usage](05-testing-and-observability.md#replay-debugging-and-forks)).
`POST /v1/runs/{id}/fork` takes the corrected code and an optional `at`, the position of an
event.

| The fork has | Value |
| --- | --- |
| Run id | Its own |
| Code | The corrected one |
| Workflow | The one its code is in the namespace, else a new one (below) |
| Seed | The source's |
| Input | The source's, unless the request overrides it |
| TTL | A new one |
| Action set | The source's |
| Namespace | The source's: a fork never leaves it, and so lives in its source's partition |
| Secrets | The ones its source uses, unless the request names its own; checked against the caller, since the code is new ([secrets](04-tools.md#secrets)) |

When the source is a run of a workflow version with a suite, the suite runs against the
corrected code first, and the fork is refused (422 `tests-failed`) if a test does not pass
([suites](05-testing-and-observability.md#suites-and-the-gate)). The fork is then a run of the
workflow its code already is in the namespace, else of a new workflow it creates there, as a
start given code is ([starting a run](#starting-a-run)). The call names a run, not a workflow,
so the correction does not become a version of the source's workflow: the caller makes it that
workflow's next version when it should stay ([workflows](01-product.md#workflows)). The fork's
handle keeps its source's origin ([handles](06-storage-and-scale.md#run-handles)). A fork given
no new code is a run of its source's version.

The engine replays the new code against the source's history:

- While its commands match the source's, the source's events are copied into the fork's own
  history: completions keep their recorded results and their `recorded_at`. No action is
  called again.
- At the first command that differs, at the event `at`, or at the end of the source's
  history, the fork continues live. A source's cancellation or abort is where its history
  ends for the fork.
- A call made live carries the key `{fork_run_id}/{seq}`.
- A call the source scheduled but left without an outcome (a cancelled or aborted source) is
  delivered again with the source's key `{source_run_id}/{seq}` if its policy is
  `idempotent: true`, so that the action recognizes a duplicate. It fails with `outcomeUnknown`
  if its policy is `idempotent: false`.
- Code that reads `run.id` diverges there: the fork has its own id.
- A workflow run started by a call copied from the source stays the source's child: the fork
  receives its end too (unless the copied history already holds it), and never cancels it. A
  start made live starts a child of the fork ([workflows](#workflows-across-runs)).

With the source's seed, the corrected code emits the same commands as long as it takes the
same path. The `at` position forces the fork live from an event even if the commands still
match.

The link to the source is [run metadata](05-testing-and-observability.md#run-metadata). The
fork's history is self-contained: it replays without the source. The payloads it references
that are blobs are copied under the fork's own run ([payloads](06-storage-and-scale.md#payloads)).

## Determinism epoch

A waiting run has no saved memory. It is rebuilt by re-executing its code, and the
history only holds what came from outside (call results, timer fires, signals) and the
seed. Everything computed inside the sandbox is computed again. If a new build of Rewind
computes it differently, the code emits a command that no longer matches the history.

What can change a `seq`, a digest, or a value the code observes:

| Component | Example |
| --- | --- |
| V8 and ICU | `toLocaleString`, `localeCompare`, the text of built-in error messages |
| JS glue | A different order in which `signal(name, { timeout })` allocates its timer shifts every later `seq` |
| Random generators | A different algorithm gives a different `crypto.randomUUID()` |
| `rewind` library and sandbox globals | A global added or changed, an error renamed |
| `rewind:actions` codegen | A different normalization of args changes their digest |

The transpiled code is stored with the code, so a transpiler upgrade doesn't count.

These components are versioned together by one integer, the **determinism epoch**. It is
recorded in `run_started` and bumped by any change of the kind above. It is not the crate
version: most releases leave it unchanged.

The glue is observable in subtle ways too: the line numbers of its own frames can appear in
`error.stack`. Errors the glue creates (`ActionError`, `RunError`, `CancelledError`,
`DeadlockError`) carry no glue frame, and the stack recorded in `fail_run` drops them. The epoch
corpus records the rest, so such a change shows up in its tests.

- **Glue, generators, library and codegen** are JavaScript embedded in the binary. A
  build keeps the versions of every epoch that may still have live runs, and uses the one
  matching the run.
- **V8 and ICU** cannot coexist in one process. After such an upgrade, a few nodes of the
  previous build stay up for the runs of the previous epoch, without owning partitions
  (`--executors-only`): owners send them those runs' activations. The
  [run TTL](#run-ttl) bounds how long: 30 days by default.
- Nodes declare the epochs their build supports in the control plane's `nodes` table. An owner
  runs an activation on its own node when its build supports the run's epoch, and otherwise
  sends it to a node whose build does ([activations](#activations)).
- The same mechanism gives a tenant its own sandboxed pool of executors when it needs one
  (`--pool <name>`): the owners send that tenant's activations to the pool's nodes.
- A corpus of recorded histories, kept per epoch, is replayed by the tests run before every
  commit. A change that breaks an epoch the build claims to support fails them.

The vocabulary of 0.1 renamed names that code observes, and code written before it keeps
working ([decisions](08-decisions.md#taken-while-reviewing-the-vocabulary-against-main-november-2026)).
The sandbox still resolves `rewind:tools` (as `rewind:actions`), `webhook(name)` (as
`signalUrl(name)`), `ToolError` (as `ActionError`) and `WorkflowError` (as `RunError`) for code
that uses them. Nothing declares or documents them: the declarations, the prompts, the checks
at start and the type-check know only the new names, so new code is written with those, and the
versions and runs that hold older code are not checked again. An error the glue creates now
carries its new name, which code that reads `e.name` observes: the rename bumped the
determinism epoch by one, and the runs started under the previous epoch replay with its glue
and library, as after any bump.

## Non-determinism and quarantine

Workflows cannot be non-deterministic, by construction. A replay mismatch during an
activation is therefore a fault of the platform (an epoch that was not bumped, a bug in
the glue), never of the workflow's code. Ending the run would make the customer pay for it.

- The activation commits nothing of its own.
- The owner marks the run **quarantined** in its head, with the mismatch message, and stops
  queuing it. Quarantine is service state, kept in the run's head: it is not in the history.
  The run is `waiting`, for `quarantined`: someone releases it, a new build retries it, or it
  is given up ([run state](02-history.md#run-state)).
- A quarantined run is released through the API or the CLI, typically after a fix, and
  is then activated again. A node of a new build that opens a partition also retries, once,
  each of its quarantined runs of the epochs the build supports: a replay that fails again
  commits nothing and leaves the run quarantined.
- If it is still quarantined at its deadline, or if an operator gives up on it, the engine
  ends it with `run_aborted { non_determinism }`.

A run whose activations failed 5 times in a row is quarantined the same way, with the last
error as its reason. An activation whose node died during it counts as a failed one, counted
when the partition is recovered: the reason then says how many never finished. Released, it is
activated again; given up on, it ends with `run_aborted { internal }`. The failure is logged
and reported through OpenTelemetry and webhooks (`run.quarantined`).

`rewind replay <run> --code fixed.ts` still reports a mismatch as its result: there,
different code is the point.

## How a run ends

The run's ending command (`complete_run` or `fail_run`) is emitted when its outcome
is known **and no call is pending**. The outcome is evaluated each time the code
goes idle:

| Situation | Outcome |
| --- | --- |
| `main` returned | Success, with its return value as output. |
| `main` threw | Failure, with that error. |
| A promise was rejected and still has no handler once microtasks are drained | Failure, with the rejection as the error. |
| `main` has not settled, and no call, timer or signal wait is pending (a `result()` that waits for a workflow run is a signal wait) | Failure, `DeadlockError`: nothing can wake the code any more. After a cancellation, the failure is `CancelledError` instead: the run ends `cancelled` (a wait built on `setTimeout` loses its timer when cancelled). |

**Unhandled rejections** follow Node, its known trap included:
`const p = action(); await other(); await p` fails if `p` is rejected before it is
awaited. V8 reports every promise rejected without a handler; the runtime collects them
and checks after each microtask checkpoint.

**A run does not end while a call is in flight.** A forgotten `await` is the most
common mistake in generated code, and the TypeScript compiler does not catch it.

- After `main` returned, the glue holds the output and the code stays alive: pending
  calls complete, their continuations run, and the table above still applies. So
  the effect the code asked for happens, and if it fails, the unhandled rejection
  fails the run. `complete_run` is emitted when the last call has settled and the last
  [attached](#workflows-across-runs) workflow run the code started has ended.
- After a failure, none of the workflow's code runs any more. The outcomes of the calls in
  flight are recorded as they arrive, without being handed to the code, and
  `fail_run` is emitted after the last one.
- Timers and signal waits still pending at that point are dropped: they have no effect
  outside the run. Attached workflow runs still active are not waited for: the run's end
  cancels them.

Consequences:

- When a run ends by itself, every call it scheduled has a recorded outcome. This is
  what tells a [fork](#forks) which effects happened.
- Cancellation and the engine's aborts (`run_aborted`) are the exceptions. They end the run at
  once, and a call left without a completion has an unknown outcome.
- The cost: code that races a slow action against a timer still waits for that action
  before it ends.

**Waiting for a signal is not a deadlock**: something outside can still wake the run.
The [run TTL](#run-ttl) bounds it.

`rewind check` catches the forgotten `await` before anything runs
([type-checking](04-tools.md#type-checking)).

## Signals and timers in the glue

- `sleep(d)` and `setTimeout` emit `start_timer`; `timer_fired` resolves them.
  `clearTimeout` emits `cancel_timer`.
- `signal(name)` reads the name's FIFO mailbox. If it is empty, it registers a waiter and
  emits a `signal_awaited` annotation.
- `signal(name, { timeout })` also starts a timer. If the signal arrives first, the
  timer is cancelled (`cancel_timer`); if the timer fires first, the waiter resolves to
  `undefined`.

Outside the isolate, a timer is a key of the run's partition, sorted by its due time and
written in the batch that commits `start_timer`; the batch that commits `cancel_timer` deletes
it. The owner's **timer wheel** holds the timers due before its **horizon**, an hour ahead,
and once an hour reads the keys of the next hour: the one scheduling loop of a partition. When
a timer is due, the owner appends `timer_fired` in its next batch. The run's deadline, a call's
timeout and its next delivery after a backoff are timers of the same kind
([partitions](06-storage-and-scale.md#partitions-owners-and-nodes)).

## Workflows across runs

A workflow's code starts and signals runs of [workflows](01-product.md#workflows) through two
built-in actions per workflow its action set pins, `workflows.<name>.start` and
`workflows.<name>.signal`, which the engine serves itself, like `signalUrl`
([routes](04-tools.md#routes)). They are calls: recorded as such, delivered again with the same
idempotency key, reused by forks, answered by a test's world. The end of a run comes back to
the run that started it as a signal. The history has no event of its own for any of it.

An action set pins the organization's workflows it names
([action sets](04-tools.md#action-sets-and-code-generation)), and a start made through
`rewind:workflows` starts the pinned version, even in a namespace that has forked that workflow
since: a namespace's fork is what its runs and its harness get when they name the workflow, and
a pinned import keeps its pinned version ([workflows](01-product.md#workflows)).

| In the code | In the caller's history | What the engine does | In the target's history |
| --- | --- | --- | --- |
| `triage.start(input, opts)` | `schedule_call` of `workflows.triage.start` with `{ input, detached?, labels?, ttl? }`, then `call_completed { runId }` | Creates the run of the pinned version: its code, action set and the secrets it uses, the caller's labels plus `labels`, the trace context of the call, the caller's allowed labels. Its run key is `rewind:{idempotency key}`, so a start made again after an interruption finds the same run | `run_started` |
| `run.signal(name, payload)`, `triage.get(id).signal(name, payload)` | `schedule_call` of `workflows.triage.signal` with `{ run, name, payload }`, then `call_completed {}` | Puts the signal in the target's inbox with the key `rewind:{idempotency key}`, so it arrives once | `signal_received` |
| `run.result()` | `signal_awaited`, then `signal_received rewind:ended:<run>` | The commit that ends the target puts its end in the inbox of the run that started it, and of every run that watches it, in the same batch | `complete_run`, `fail_run` or `run_aborted` |

A workflow run starts in the namespace of the run that calls it
([routes](04-tools.md#routes)), and nothing a run does reaches another namespace, so a run and
every run it starts, signals or watches live in the same partition. Each effect in the table is
written in the same batch as the fact that caused it, under the keys above, which make a
second delivery harmless.

Checks:

- Inside the op, before the command is recorded, as an action's args are: the input against the
  workflow's input schema; a signal's name against the names it declares, and its payload
  against that signal's schema; `labels` and `ttl` as when the API starts a run. A failure
  throws a `TypeError` at the call.
- At dispatch, `signal` fails with an `ActionError` when the target does not exist or is outside
  the caller's allowed labels, is not a run of that workflow, has ended, or refuses the payload
  (its own version declares the signal differently). `start` fails with an `ActionError`
  (`unavailable`) when the tenant has reached its quota of active runs, and when a secret the
  version uses does not exist (an `ns/` secret in the caller's namespace). The version's secrets
  are not checked against anyone's rights: they were when the version was made
  ([secrets](04-tools.md#secrets)).
- `result()` checks the output against the pinned output schema. A mismatch rejects with
  `RunError` (`invalid_output`).

Ends:

- The glue takes the `rewind:ended:` signals itself: they never reach a mailbox the code
  reads. `result()` gives the same answer each time it is called, and an end that arrives
  before the start's result is kept for it.
- A run started without `detached: true` is **attached**: `complete_run` waits for it
  ([how a run ends](#how-a-run-ends)). A failure or a cancellation does not.
- When a run ends, the batch that ends it enqueues `cancel_requested` for each of its
  attached children still active, with the message `the run <id> that started it ended`. A
  cancellation does not wait for a start in flight: a child created after its parent ended is
  cancelled as soon as it exists.
- The end of a run that no active run watches (a detached child whose parent has ended) goes
  nowhere.
- A fork watches the children its copied history started: it receives their ends, and never
  cancels them ([forks](#forks)).

A workflow's code reaches only the runs of its tenant within its **allowed labels**: those of
the grant that started the first run of its tree (an API key's or a member's), none for a grant
without labels. Children inherit them, and their labels always include them. Versions are
content-addressed, the labels of a grant limited to labels that made one included
([access](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)), and an action set
pins a version, so a workflow cannot start itself, or a workflow that starts it. Code starts a
pinned version only if it was made under its allowed labels, as its grant could have started it
from the API: a version made under other labels may use the organization's secrets of those
labels ([secrets](04-tools.md#secrets)).

In a test run, `workflows.<name>.start` is answered by the test's world like any call (its
rules, its handlers, its cassette), and the answer is the child's outcome: a result is its
output, a failure its error. No run is created: the start resolves with a run id drawn from the
engine's entropy, and the end follows at once as a signal. `workflows.<name>.signal` is accepted
unless the test's world answers it. The cassette of a run records each start with the outcome
of its child, read from the end signal in the history (a child that had not ended is recorded as
failed with that reason), and leaves the `rewind:ended:` signals out of its signal timeline.

## Cancellation

`cancel_requested` rejects every pending await in the code with `CancelledError`. The
code may catch it to clean up (it gets one grace step). Otherwise the run ends
`cancelled`, and the tasks of its pending calls and its timers are cancelled. Cancellation does
not wait for the calls in flight: their results are dropped when they arrive. Nor does it
wait for the attached workflow runs the code started: the run's end cancels them.

The grace step is the handing of `cancel_requested` to the code itself: once the code is idle
again, the run must have ended by itself (`complete_run`, or `fail_run`, `CancelledError`
included). If it has not, for instance because it awaits a cleanup call, the engine commits
`cancel_requested` with `run_aborted { cancelled }` and drops what the code emitted in
response, like any abort of the engine's.

## Run TTL

Every run has a deadline, `expires_at`, recorded in `run_started`: the time the run was
started plus a TTL of **30 days by default**. The harness can set another TTL when it
starts the run, up to a maximum set by the deployment, and by the `maxTtl` of the run's
namespace and of its tenant ([quotas](06-storage-and-scale.md#quotas)); the default is
lowered to that maximum.

The deadline is a timer of the run's partition, written in the batch that commits
`run_started` ([timers](#signals-and-timers-in-the-glue)). When it fires, the owner enqueues
`cancel_requested { reason: "ttl" }`. The run then follows the cancellation path, grace
step included.

The TTL bounds everything that would otherwise wait forever: a `signal()` without a
timeout, a call that never answers, a quarantined run, and the time the nodes of a
previous epoch must stay up.

## Resource limits

- Per call into the code: CPU time (a watchdog thread calls `terminate_execution`).
- Per instance: heap size (V8 heap limit + near-heap-limit callback).
- Per instance: the memory of array buffers, which V8 keeps outside its heap (their
  allocator's budget).
- Per activation: number of commands emitted.
- Per run: history length, payload sizes.

Limit violations end the run with `run_aborted { limit_exceeded }`: the trigger is committed
with `run_aborted`, and what the code emitted in response to it is dropped.

Array buffers are the exception. An allocation past their budget fails as an allocation fails
in JavaScript: V8 throws `RangeError: Array buffer allocation failed`, which the code can
catch. It is deterministic: V8 collects garbage before it gives up, so the allocation fails
only when the buffers the code still reaches leave no room, the same live and on replay. A
resizable buffer takes its maximum length from the budget when it is created.

V8 terminates an execution at its next interrupt check, and some built-ins run to their end
without one: filling or copying an array over a huge length. Such a call that must create more
elements than the heap holds ends at once, as the heap limit ends it. What V8 still runs to its
end is bounded by the node's container
([sandboxed nodes](06-storage-and-scale.md#sandboxed-nodes)).

The CPU time of each call is also reported to the engine, which counts the calls that execute,
not those of a replay, toward the namespace's `cpuPerMonth`
([quotas](06-storage-and-scale.md#quotas)). That count is metering: it never ends, pauses or
changes a run, and it is not in the history.

The CPU budget applies to each call into the code (`main(input)`, or one trigger handed to it,
5 s by default), not to the whole activation: an activation can replay the full history, and a
long run must not exhaust its budget replaying it. The budget is CPU time of the isolate's
thread. The heap limit is 128 MiB by default, and so is the budget of array buffers.
