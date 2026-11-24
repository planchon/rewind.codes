# 3. Execution model

## Starting a run

Starting a run checks the script first and refuses it at once when it cannot run: the agent
that wrote it learns in the same call, not from an aborted run later. No run is created and
nothing is stored. Refused at start, with a diagnostic written for the model (a line and a
column in `script.ts`):

- a syntax error;
- an import of anything other than `rewind`, `rewind:tools` and `rewind:workflows`, a dynamic
  `import()`, a `require`;
- an import of a name `rewind` does not export, of a tool the run's toolset does not have, or of
  a workflow it does not pin;
- no default export, or a default export that is certainly not a function (a literal, an
  object, a class);
- more lines than its namespace's `maxScriptLines`
  ([quotas](06-storage-and-scale.md#quotas));
- an import of a server or an API that needs a secret, without one attached for it; an
  attachment of a secret that does not exist, or that the caller may not attach
  ([secrets](04-tools.md#secrets)).

A run is started in a namespace, the end user it acts for, and the start must pass that
namespace's quotas and its tenant's, which the tenant's organization actor checks before the
run is created ([quotas](06-storage-and-scale.md#quotas)).

Starting a run also type-checks the script against the toolset's declarations, as
`rewind check` does, so that most mistakes are refused before a run exists.

## Activations

Completions never go straight into the history. They land in a per-run **inbox**. A run lives
in its namespace's partition, and each partition has one **owner**, the only node that writes
it ([partitions](06-storage-and-scale.md#partitions-owners-and-nodes)). The owner moves the
triggers into the history one activation at a time, interleaved with the commands they cause.
The history therefore has exactly one writer at any time, which is what makes its order
meaningful.

```
a durable batch puts a trigger in R's inbox: the owner queues R on its node's ready queue
an executor takes R; R is busy until its activation is committed
instance ← cached instance whose history length matches, or new instance + replay(R.history)
for trigger in R.inbox (arrival order):
  recorded_at ← max(now, last recorded_at)          # monotonic per run
  if R is terminal: drop the completion
  append trigger; deliver it to the instance; run microtasks until idle
  append the commands and annotations emitted
move payloads above 1 KiB out of the events (payload rows, or the BlobStore above 64 KiB)
derive effects from the new commands: tool tasks, timers, cancelled timers
the owner commits { entries, consumed inbox items, effects, status } in its next batch
once that batch is durable: calls are sent, timers set; the instance stays in the cache
if triggers reached R's inbox meanwhile, R is queued again
```

Commands and the effects they create (tool tasks, timers) are committed in the same batch, so
an effect can never be lost or duplicated by a crash between the two. Nothing an activation
causes leaves the owner before its batch is durable: a call is sent, a timer set, a subscriber
told only after. A node that lost its partition cannot commit: the node that opened it since
fenced it. The same batch writes the outbox entries that the projections read (telemetry and
webhooks, see [05](05-testing-and-observability.md#observability)).

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

- The script fails to load although it passed the checks at start (an exception thrown by
  its top-level code, a default export that turns out not to be a function) →
  `run_aborted { invalid_script }`, with the diagnostic.
- Replay detects non-determinism → nothing is committed, and the run is
  [quarantined](#non-determinism-and-quarantine).
- A limit is exceeded → `run_aborted { limit_exceeded }`.
- Anything else (a missing blob, a store error, a replay interrupted by a limit or an
  internal error of the executor) → nothing of the activation is committed. The owner
  records the failure in the run's head, with a timer 30 s later, and activates the run again
  when it fires; the other runs are activated meanwhile. After **5 such failures in a row**,
  the run is [quarantined](#non-determinism-and-quarantine) with the last error: a run that
  cannot make progress is set aside and reported, not retried forever.
- The node dies during the activation (the script crashed the process, the container was
  killed for its memory, a native dependency failed) → nothing is committed and nothing is
  recorded. This is a failure too, counted when the partition is recovered: the node that
  takes it over adds one to `attempts`, the activations that never finished, of every run that
  was runnable, since one of them may have been running. A commit resets the count. An
  executor that takes a run whose `attempts` is above 5 quarantines the run instead of
  activating it. A script that kills its nodes does not go from node to node forever, and the
  runs that were only waiting commit before it runs again, since the ready queue takes the runs
  with fewer unfinished activations first.

## Inside the isolate

Nothing is asynchronous from V8's point of view:

- Ops are synchronous: they record a command, assign a `seq`, and return it.
- Values cross the boundary as JSON. A value reaches the script with its object keys in
  canonical order (RFC 8785), so that it looks the same live and on replay, wherever it was
  loaded from. A string holding a lone surrogate (half of an emoji, after a `slice`) leaves
  the script with U+FFFD in its place.
- The script's output and the description of its failure are captured when the outcome is
  known: serializing them runs the script's code (`toJSON`, getters), which cannot happen once
  the terminal command is decided.
- The JS glue turns the `seq` into a promise whose resolver is stored by `seq`.
- Delivering a completion calls the glue's `deliver(completion)` and runs a microtask
  checkpoint.
- There is no event loop and no real timer inside the isolate.

V8 objects cannot change threads, so each script instance lives on a dedicated thread
behind the `ScriptExecutor` port. The engine talks to it through channels and never
handles a V8 type.

## Logical time

`Date.now()` returns the `recorded_at` of the **last trigger delivered** (`run_started`
for the first activation). Time only moves when the outside world speaks, and it moves
identically on replay. `recorded_at` is monotonic per run (`max(now, last)`).

After `await sleep("10d")`, `Date.now()` is at least start + 10 days, because
`timer_fired` is recorded after `fire_at`. `performance.now()` is the logical time
elapsed since the run started.

In virtual time (tests, mock mode), the driver jumps the clock straight to the earliest
timer of the timer wheel: a timer's `fire_at`, a tool call's next attempt after its backoff, a
deadline. A 10-day sleep takes no wall-clock time.

## Replay

Replay creates a fresh instance, runs `main(input)`, and walks the history with
`ReplayCursor` (pure, in `rewind-core`):

- When the next entry is a **completion**, deliver it and drain microtasks.
- Each **command** the script emits must match the next command in the history: same
  kind, same `seq`, same tool, same args (compared by payload digest). Callsites are
  ignored, so moving code around without changing behavior is allowed. A `fail_run`
  matches on error name and message.
- If the history expects a command and the script goes idle without emitting it, that is
  non-determinism too.
- Annotations are skipped. Thanks to the commit invariant, every log emitted during
  replay is already recorded, so logs are suppressed. No telemetry about the script is
  emitted during replay either: run telemetry is projected from committed entries
  ([05](05-testing-and-observability.md#observability)).
- If the history ends with `run_aborted`, replay stops there.
- At the end of the history, the script is in exactly the state it was in after the last
  commit, and the run continues live.

Errors are written to be read by a model:

```
history entry 1 is tool call #1 github.close_issue({"number":4}), but the script emitted tool call #1 github.close_issue({"number":5})
the script emitted tool call #3 slack.post_message({…}), which the history does not contain (at entry 6)
history entry 4 is timer #2 (3000ms), but the script went idle without emitting it
```

The history itself is validated first: not empty, starts with exactly one
`run_started`, contiguous indexes, nothing after `run_aborted`.

Scripts are immutable and content-addressed, so there is no workflow versioning problem
for the script. Two risks remain:

- A change in the Rewind runtime itself: see the [determinism epoch](#determinism-epoch).
- A toolset whose schemas changed while the run was suspended: dispatch checks the call
  against the current schema and parks the task
  ([toolset change](04-tools.md#when-a-toolset-changes-during-a-run)).

Replaying the full history on every activation would be O(history²) over a run's life.
Mitigations:

- [Sticky execution](#sticky-execution): the executors keep the instance in memory, and the
  run's next activation runs on them. Replay is the fallback, not the hot path.
- Histories are capped (default 50k entries). A run that reaches the cap is aborted with
  `limit_exceeded`; `continueAsNew` is out of 0.1.

## Sticky execution

After a commit, the executor leaves the script instance in its node's **instance cache**. Only
the owner of the run's partition activates the run, so its next activation runs on the same
node and finds the instance there while it is cached.

- The cache is an LRU, bounded per node: 500 instances and 4 GiB by default. An instance's
  memory is its isolate's, as V8's heap statistics give it after each call into the script.
- An owner that sends a run's activations to another node, for an epoch its own build does
  not support, sends them to the same node while that node lives, so the instance stays warm
  there.
- **Hit**: the instance is cached, and its history length is the run's. Only the new trigger
  is delivered: no replay.
- **Miss**: a new instance starts from a V8 startup snapshot and replays the history. A run
  whose partition moved to another node, or whose instance was evicted, misses once.

Each live instance keeps its own thread and isolate. The load tests check that this holds
within these bounds. An isolate pool per thread is built only if they fail.

The cache also saves most payload reads ([payloads](06-storage-and-scale.md#payloads)).

## Forks

A fork is a new run started from an existing one with a corrected script, so that an agent
can fix step 7 without redoing the side effects of steps 1 to 6
([usage](05-testing-and-observability.md#replay-debugging-and-forks)).
`POST /v1/runs/{id}/fork` takes the corrected script and an optional `at` index.

| The fork has | Value |
| --- | --- |
| Run id | Its own |
| Script | The corrected one |
| Seed | The parent's |
| Input | The parent's, unless the request overrides it |
| TTL | A new one |
| Toolset | The parent's |
| Namespace | The parent's: a fork never leaves it, and so lives in its parent's partition |
| Attached secrets | The parent's, unless the request gives its own; checked against the caller, since the script is new code ([secrets](04-tools.md#secrets)) |

The engine replays the new script against the parent's history:

- While its commands match the parent's, the parent's entries are copied into the fork's own
  history: completions keep their recorded results and their `recorded_at`. No tool is
  called again.
- At the first command that differs, at the entry `at`, or at the end of the parent's
  history, the fork continues live. A parent's cancellation or abort is where its history
  ends for the fork.
- A call made live carries the key `{fork_run_id}/{seq}`.
- A call the parent scheduled but left without an outcome (a cancelled or aborted parent) is
  dispatched again with the parent's key `{parent_run_id}/{seq}` if its policy is `retry`, so
  that the tool recognizes a duplicate. It fails with `outcomeUnknown` if its policy is
  `at_most_once`.
- A script that reads `run.id` diverges there: the fork has its own id.
- A workflow run started by a call copied from the parent stays the parent's child: the fork
  receives its end too (unless the copied history already holds it), and never cancels it. A
  start made live starts a child of the fork ([workflows](#workflows-across-runs)).

With the parent's seed, the corrected script emits the same commands as long as it takes the
same path. The `at` index forces the fork live from an entry even if the commands still
match.

The link to the parent is [run metadata](05-testing-and-observability.md#run-metadata). The
fork's history is self-contained: it replays without the parent. The payloads it references
that are blobs are copied under the fork's own run ([payloads](06-storage-and-scale.md#payloads)).

## Determinism epoch

A suspended run has no saved memory. It is rebuilt by re-executing the script, and the
journal only holds what came from outside (tool results, timer fires, signals) and the
seed. Everything computed inside the sandbox is computed again. If a new build of Rewind
computes it differently, the script emits a command that no longer matches the history.

What can change a `seq`, a digest, or a value the script observes:

| Component | Example |
| --- | --- |
| V8 and ICU | `toLocaleString`, `localeCompare`, the text of built-in error messages |
| JS glue | A different order in which `signal(name, { timeout })` allocates its timer shifts every later `seq` |
| Random generators | A different algorithm gives a different `crypto.randomUUID()` |
| `rewind` library and sandbox globals | A global added or changed |
| `rewind:tools` codegen | A different normalization of args changes their digest |

The transpiled script is stored with the script, so a transpiler upgrade doesn't count.

These components are versioned together by one integer, the **determinism epoch**. It is
recorded in `run_started` and bumped by any change of the kind above. It is not the crate
version: most releases leave it unchanged.

The glue is observable in subtle ways too: the line numbers of its own frames can appear in
`error.stack`. Errors the glue creates (`ToolError`, `CancelledError`, `DeadlockError`) carry
no glue frame, and the stack recorded in `fail_run` drops them. The epoch corpus records the
rest, so such a change shows up in its tests.

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
- A corpus of recorded journals, kept per epoch, is replayed by the tests run before every
  commit. A change that breaks an epoch the build claims to support fails them.

## Non-determinism and quarantine

Scripts cannot be non-deterministic, by construction. A replay mismatch during an
activation is therefore a fault of the platform (an epoch that was not bumped, a bug in
the glue), never of the script. Ending the run would make its owner pay for it.

- The activation commits nothing of its own.
- The owner marks the run **quarantined** in its head, with the mismatch message, and stops
  queuing it. Quarantine is service state, kept in the run's head: it is not in the history.
- A quarantined run is released through the API or the CLI, typically after a fix, and
  is then activated again. A node of a new build that opens a partition also retries, once,
  each of its quarantined runs of the epochs the build supports: a replay that fails again
  commits nothing and leaves the run quarantined.
- If it is still quarantined at its deadline, or if an operator gives up on it, the host
  ends it with `run_aborted { non_determinism }`.

A run whose activations failed 5 times in a row is quarantined the same way, with the last
error as its reason. An activation whose node died during it counts as a failed one, counted
when the partition is recovered: the reason then says how many never finished. Released, it is
activated again; given up on, it ends with `run_aborted { internal }`. The failure is logged
and reported through OpenTelemetry and webhooks.

`rewind replay <run> --script fixed.ts` still reports a mismatch as its result: there, a
different script is the point.

## How a script ends

The script's terminal command (`complete_run` or `fail_run`) is emitted when its outcome
is known **and no tool call is pending**. The outcome is evaluated each time the script
goes idle:

| Situation | Outcome |
| --- | --- |
| `main` returned | Success, with its return value as output. |
| `main` threw | Failure, with that error. |
| A promise was rejected and still has no handler once microtasks are drained | Failure, with the rejection as the error. |
| `main` has not settled, and no tool call, timer or signal wait is pending (a `result()` that waits for a workflow run is a signal wait) | Failure, `DeadlockError`: nothing can wake the script any more. After a cancellation, the failure is `CancelledError` instead: the run ends `cancelled` (a wait built on `setTimeout` loses its timer when cancelled). |

**Unhandled rejections** follow Node, its known trap included:
`const p = tool(); await other(); await p` fails if `p` is rejected before it is
awaited. V8 reports every promise rejected without a handler; the runtime collects them
and checks after each microtask checkpoint.

**A run does not end while a tool call is in flight.** A forgotten `await` is the most
common mistake in generated code, and the TypeScript compiler does not catch it.

- After `main` returned, the glue holds the output and the script stays alive: pending
  tool calls complete, their continuations run, and the table above still applies. So
  the effect the script asked for happens, and if it fails, the unhandled rejection
  fails the run. `complete_run` is emitted when the last tool call has settled and the last
  [attached](#workflows-across-runs) workflow run the script started has ended.
- After a failure, no script code runs any more. The outcomes of the tool calls in
  flight are recorded as they arrive, without being delivered to the script, and
  `fail_run` is emitted after the last one.
- Timers and signal waits still pending at that point are dropped: they have no effect
  outside the run. Attached workflow runs still running are not waited for: the run's end
  cancels them.

Consequences:

- When a run ends by itself, every tool call it scheduled has a recorded outcome. This is
  what tells a [fork](#forks) which effects happened.
- Cancellation and host aborts (`run_aborted`) are the exceptions. They end the run at
  once, and a call left without a completion has an unknown outcome.
- The cost: a script that races a slow tool against a timer still waits for that tool
  before it ends.

**Waiting for a signal is not a deadlock**: something outside can still wake the script.
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
a timer is due, the owner appends `timer_fired` in its next batch. The run's deadline, a tool
call's timeout and its next attempt after a backoff are timers of the same kind
([partitions](06-storage-and-scale.md#partitions-owners-and-nodes)).

## Workflows across runs

A script starts and signals runs of [workflows](01-product.md#workflows) through two built-ins
per workflow its toolset pins, `workflows.<name>.start` and `workflows.<name>.signal`, which
the engine serves itself, like `webhook` ([routes](04-tools.md#routes)). They are tool calls:
journaled as such, retried with the same idempotency key, reused by forks, answered by mocks.
The end of a run comes back to the run that started it as a signal. The journal has no event
of its own for any of it.

| In the script | In the caller's journal | What the engine does | In the target's journal |
| --- | --- | --- | --- |
| `triage.start(input, opts)` | `schedule_tool_call` of `workflows.triage.start` with `{ input, detached?, labels?, ttl? }`, then `tool_call_completed { runId }` | Creates the run of the pinned version: its script, toolset and attached secrets, the caller's labels plus `labels`, the trace context of the call, the caller's scope. Its run key is `rewind:{idempotency key}`, so a start made again after an interruption finds the same run | `run_started` |
| `run.signal(name, payload)`, `triage.get(id).signal(name, payload)` | `schedule_tool_call` of `workflows.triage.signal` with `{ run, name, payload }`, then `tool_call_completed {}` | Puts the signal in the target's inbox with the key `rewind:{idempotency key}`, so it is delivered once | `signal_received` |
| `run.result()` | `signal_awaited`, then `signal_received rewind:ended:<run>` | The target's terminal commit puts its end in the inbox of the run that started it, and of every run that watches it, in the same batch | `complete_run`, `fail_run` or `run_aborted` |

A workflow run starts in the namespace of the run that calls it
([routes](04-tools.md#routes)), and nothing a run does reaches another namespace, so a run and
every run it starts, signals or watches live in the same partition. Each effect in the table is
written in the same batch as the fact that caused it, under the keys above, which make a
second delivery harmless.

Checks:

- Inside the op, before the command is recorded, as tool args are: the input against the
  workflow's input schema; a signal's name against the names it declares, and its payload
  against that signal's schema; `labels` and `ttl` as when the API starts a run. A failure
  throws a `TypeError` at the call.
- At dispatch, `signal` fails with a `ToolError` when the target does not exist or is out of
  the caller's scope, is not a run of that workflow, has ended, or refuses the payload (its own
  version declares the signal differently). `start` fails with a `ToolError` (`unavailable`)
  when the tenant has reached its quota of active runs, and when a secret the version attaches
  does not exist (an `ns/` secret in the caller's namespace). The version's attachments are not
  checked against anyone's rights: they were when it was saved ([secrets](04-tools.md#secrets)).
- `result()` checks the output against the pinned output schema. A mismatch rejects with
  `WorkflowError` (`invalid_output`).

Ends:

- The glue takes the `rewind:ended:` signals itself: they never reach a mailbox the script
  reads. `result()` gives the same answer each time it is called, and an end that arrives
  before the start's result is kept for it.
- A run started without `detached: true` is **attached**: `complete_run` waits for it
  ([how a script ends](#how-a-script-ends)). A failure or a cancellation does not.
- When a run ends, the same terminal batch enqueues `cancel_requested` for each of its
  attached children still running, with the message `the run <id> that started it ended`. A
  cancellation does not wait for a start in flight: a child created after its caller ended is
  cancelled as soon as it exists.
- The end of a run that no running run watches (a detached child whose parent has ended) goes
  nowhere.
- A fork watches the children its copied history started: it receives their ends, and never
  cancels them ([forks](#forks)).

A script reaches only the runs of its tenant within its **scope**: the allowed labels of the
grant that started the first run of its tree (an API key's or a member's), none for a grant
without labels. Children inherit the
scope, and their labels always include it. Versions are content-addressed, the labels of a
grant limited to labels that saved one included ([access](06-storage-and-scale.md#tenancy-namespaces-identity-and-access)),
and a toolset pins a version, so a workflow cannot start itself, or a workflow that starts it.
A script starts a pinned version only if it was saved under its scope's labels, as its grant
could have started it from the API: a version saved under other labels may attach the
organization's secrets of those labels ([secrets](04-tools.md#secrets)).

In mock mode, `workflows.<name>.start` is answered by the mock sources like any tool call, and
their answer is the child's outcome: a result is its output, a failure its error. No run is
created: the start resolves with a run id drawn from the engine's entropy, and the end follows
at once as a signal. `workflows.<name>.signal` is accepted unless a mock source answers it. The
cassette of a run records each start with the outcome of its child, read from the end signal
in the journal (a child that had not ended is recorded as failed with that reason), and
leaves the `rewind:ended:` signals out of its signal timeline.

## Cancellation

`cancel_requested` rejects every pending await in the script with `CancelledError`. The
script may catch it to clean up (it gets one grace activation). Otherwise the run ends
`cancelled`, and its pending tool tasks and timers are cancelled. Cancellation does not
wait for the tool calls in flight: their results are dropped when they arrive. Nor does it
wait for the attached workflow runs the script started: the run's end cancels them.

The grace is the delivery of `cancel_requested` itself: once the script is idle again,
it must have ended by itself (`complete_run`, or `fail_run`, `CancelledError` included). If it
has not, for instance because it awaits a cleanup tool call, the host commits
`cancel_requested` with `run_aborted { cancelled }` and drops what the script emitted in
response, like any host abort.

## Run TTL

Every run has a deadline, `expires_at`, recorded in `run_started`: the time the run was
started plus a TTL of **30 days by default**. The harness can set another TTL when it
starts the run, up to a maximum set by the deployment, and by the `maxTtl` of the run's
namespace and of its tenant ([quotas](06-storage-and-scale.md#quotas)); the default is lowered
to that maximum.

The deadline is a timer of the run's partition, written in the batch that commits
`run_started` ([timers](#signals-and-timers-in-the-glue)). When it fires, the owner enqueues
`cancel_requested { reason: "ttl" }`. The run then follows the cancellation path, grace
activation included.

The TTL bounds everything that would otherwise wait forever: a `signal()` without a
timeout, a tool call that never answers, a quarantined run, and the time the nodes of a
previous epoch must stay up.

## Resource limits

- Per call into the script: CPU time (a watchdog thread calls `terminate_execution`).
- Per instance: heap size (V8 heap limit + near-heap-limit callback).
- Per instance: the memory of array buffers, which V8 keeps outside its heap (their
  allocator's budget).
- Per activation: number of commands emitted.
- Per run: history length, payload sizes.

Limit violations end the run with `run_aborted { limit_exceeded }`: the trigger is committed
with `run_aborted`, and what the script emitted in response to it is dropped.

Array buffers are the exception. An allocation past their budget fails as an allocation fails
in JavaScript: V8 throws `RangeError: Array buffer allocation failed`, which the script can
catch. It is deterministic: V8 collects garbage before it gives up, so the allocation fails
only when the buffers the script still reaches leave no room, the same live and on replay. A
resizable buffer takes its maximum length from the budget when it is created.

V8 terminates an execution at its next interrupt check, and some built-ins run to their end
without one: filling or copying an array over a huge length. Such a call that must create more
elements than the heap holds ends at once, as the heap limit ends it. What V8 still runs to its
end is bounded by the node's container
([sandboxed workers](06-storage-and-scale.md#sandboxed-nodes)).

The CPU time of each call is also reported to the engine, which counts the calls that execute,
not those of a replay, toward the namespace's `cpuPerMonth`
([quotas](06-storage-and-scale.md#quotas)). That count is metering: it never ends, pauses or
changes a run, and it is not in the journal.

The CPU budget applies to each call into the script (`main(input)` or one delivery, 5 s by
default), not to the whole activation: an activation can replay the full history, and a long
run must not exhaust its budget replaying it. The budget is CPU time of the isolate's thread.
The heap limit is 128 MiB by default, and so is the budget of array buffers.
