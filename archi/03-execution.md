# 3. Execution model

## Starting a run

Starting a run checks the script first and refuses it at once when it cannot run: the agent
that wrote it learns in the same call, not from an aborted run later. No run is created and
nothing is stored. Refused at start, with a diagnostic written for the model (a line and a
column in `script.ts`):

- a syntax error;
- an import of anything other than `ptc` and `ptc:tools`, a dynamic `import()`, a `require`;
- an import of a name `ptc` does not export, or of a tool the run's toolset does not have;
- no default export, or a default export that is certainly not a function (a literal, an
  object, a class).

Starting a run also type-checks the script against the toolset's declarations, as
`ptc check` does, so that most mistakes are refused before a run exists.

## Activations

Completions never go straight into the history. They land in a per-run **inbox**. The
engine, holding the run's lease, moves them into the history one at a time, interleaved
with the commands they cause. The history therefore has exactly one writer at any time,
which is what makes its order meaningful.

```
acquire lease on run R (increments R.fence)
instance ← cached instance whose history length matches, or new instance + replay(R.history)
for trigger in take_inbox(R) (arrival order):
  recorded_at ← max(now, last recorded_at)          # monotonic per run
  if R is terminal: drop the completion
  append trigger; deliver it to the instance; run microtasks until idle
  append the commands and annotations emitted
move payloads above 1 KiB out of the events (payloads table or BlobStore)
derive effects from the new commands: tool tasks, timers, cancelled timers
commit_activation(lease, { entries, consumed inbox items, effects, status })   # one transaction, fenced
release lease; the instance stays in the worker's cache (sticky execution)
```

Commands and the effects they create (tool tasks, timers) are committed in the same
transaction (outbox pattern), so an effect can never be lost or duplicated by a crash
between the two. A lease holder that lost its lease cannot commit: the fence no longer
matches. The same transaction writes the feed items that projectors read (telemetry and
read model, see [05](05-testing-and-observability.md#the-feed-and-the-projector)).

Failures of the activation itself:

- The script fails to load although it passed the checks at start (an exception thrown by
  its top-level code, a default export that turns out not to be a function) →
  `run_aborted { invalid_script }`, with the diagnostic.
- Replay detects non-determinism → nothing is committed, and the run is
  [quarantined](#non-determinism-and-quarantine).
- A limit is exceeded → `run_aborted { limit_exceeded }`.
- Anything else (a missing blob, a store error, a replay interrupted by a limit or an
  internal error of the executor) → nothing is committed, and the lease is kept until it
  expires: the run is activated again then, and the other runs are activated meanwhile.
  After **5 such failures in a row**, the run is [quarantined](#non-determinism-and-quarantine)
  with the last error: a run that cannot make progress is set aside and reported, not
  retried forever.

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

In virtual time (tests, mock mode), the clock jumps straight to the next timer's
`fire_at`, or to the next retry of a tool call after its backoff, so a 10-day sleep takes no
wall-clock time.

## Replay

Replay creates a fresh instance, runs `main(input)`, and walks the history with
`ReplayCursor` (pure, in `ptc-core`):

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

- A change in the ai-ptc runtime itself: see the [determinism epoch](#determinism-epoch).
- A toolset whose schemas changed while the run was suspended: dispatch checks the call
  against the current schema and parks the task
  ([toolset change](04-tools.md#when-a-toolset-changes-during-a-run)).

Replaying the full history on every activation would be O(history²) over a run's life.
Mitigations:

- [Sticky execution](#sticky-execution): the worker keeps the instance in memory and gets
  the run's next activation first. Replay is the fallback, not the hot path.
- Histories are capped (default 50k entries). A run that reaches the cap is aborted with
  `limit_exceeded`; `continueAsNew` is out of 0.1.

## Sticky execution

After a commit, the worker keeps the script instance in a cache, and gets the run's next
activation first.

- The cache is an LRU, bounded per worker: 500 instances and 4 GiB by default. An instance's
  memory is its isolate's, as V8's heap statistics give it after each call into the script.
- The store records the run's sticky worker at commit. When the run becomes runnable again,
  that worker gets it first. Other workers only take it after 5 s.
- **Hit**: the instance is cached, and its history length is the run's. Only the new trigger
  is delivered: no replay.
- **Miss**: a new instance starts from a V8 startup snapshot and replays the history.
- An evicted instance releases the worker's priority on its run.

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

With the parent's seed, the corrected script emits the same commands as long as it takes the
same path. The `at` index forces the fork live from an entry even if the commands still
match.

The link to the parent is [run metadata](05-testing-and-observability.md#run-metadata). The
fork's history is self-contained: it replays without the parent.

## Determinism epoch

A suspended run has no saved memory. It is rebuilt by re-executing the script, and the
journal only holds what came from outside (tool results, timer fires, signals) and the
seed. Everything computed inside the sandbox is computed again. If a new build of ai-ptc
computes it differently, the script emits a command that no longer matches the history.

What can change a `seq`, a digest, or a value the script observes:

| Component | Example |
| --- | --- |
| V8 and ICU | `toLocaleString`, `localeCompare`, the text of built-in error messages |
| JS glue | A different order in which `signal(name, { timeout })` allocates its timer shifts every later `seq` |
| Random generators | A different algorithm gives a different `crypto.randomUUID()` |
| `ptc` library and sandbox globals | A global added or changed |
| `ptc:tools` codegen | A different normalization of args changes their digest |

The transpiled script is stored with the script, so a transpiler upgrade doesn't count.

These components are versioned together by one integer, the **determinism epoch**. It is
recorded in `run_started` and bumped by any change of the kind above. It is not the crate
version: most releases leave it unchanged.

The glue is observable in subtle ways too: the line numbers of its own frames can appear in
`error.stack`. Errors the glue creates (`ToolError`, `CancelledError`, `DeadlockError`) carry
no glue frame, and the stack recorded in `fail_run` drops them. The epoch corpus records the
rest, so such a change shows up in CI.

- **Glue, generators, library and codegen** are JavaScript embedded in the binary. A
  build keeps the versions of every epoch that may still have live runs, and uses the one
  matching the run.
- **V8 and ICU** cannot coexist in one process. After such an upgrade, a pool of workers
  on the previous build stays up for the runs of the previous epoch. The
  [run TTL](#run-ttl) bounds how long: 30 days by default.
- Workers declare the epochs they support, and are only offered runs of those epochs.
- A corpus of recorded journals, kept per epoch, is replayed in CI. A change that breaks
  an epoch the build claims to support fails the build.

## Non-determinism and quarantine

Scripts cannot be non-deterministic, by construction. A replay mismatch during an
activation is therefore a fault of the platform (an epoch that was not bumped, a bug in
the glue), never of the script. Ending the run would make its owner pay for it.

- The activation commits nothing and releases its lease.
- The store marks the run **quarantined**, with the mismatch message, and stops offering
  it to workers. Quarantine is service state, like a lease: it is not in the history.
- A quarantined run is released through the API or the CLI, typically after a fix, and
  is then activated again. A worker of a new build also retries the quarantined runs of
  the epochs it supports once: a replay that fails again commits nothing and leaves the run
  quarantined.
- If it is still quarantined at its deadline, or if an operator gives up on it, the host
  ends it with `run_aborted { non_determinism }`.

A run whose activations failed 5 times in a row is quarantined the same way, with the last
error as its reason. Released, it is activated again; given up on, it ends with
`run_aborted { internal }`. The failure is logged and reported through OpenTelemetry and
webhooks.

`ptc replay <run> --script fixed.ts` still reports a mismatch as its result: there, a
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
| `main` has not settled, and no tool call, timer or signal wait is pending | Failure, `DeadlockError`: nothing can wake the script any more. After a cancellation, the failure is `CancelledError` instead: the run ends `cancelled` (a wait built on `setTimeout` loses its timer when cancelled). |

**Unhandled rejections** follow Node, its known trap included:
`const p = tool(); await other(); await p` fails if `p` is rejected before it is
awaited. V8 reports every promise rejected without a handler; the runtime collects them
and checks after each microtask checkpoint.

**A run does not end while a tool call is in flight.** A forgotten `await` is the most
common mistake in generated code, and the TypeScript compiler does not catch it.

- After `main` returned, the glue holds the output and the script stays alive: pending
  tool calls complete, their continuations run, and the table above still applies. So
  the effect the script asked for happens, and if it fails, the unhandled rejection
  fails the run. `complete_run` is emitted when the last tool call has settled.
- After a failure, no script code runs any more. The outcomes of the tool calls in
  flight are recorded as they arrive, without being delivered to the script, and
  `fail_run` is emitted after the last one.
- Timers and signal waits still pending at that point are dropped: they have no effect
  outside the run.

Consequences:

- When a run ends by itself, every tool call it scheduled has a recorded outcome. This is
  what tells a [fork](#forks) which effects happened.
- Cancellation and host aborts (`run_aborted`) are the exceptions. They end the run at
  once, and a call left without a completion has an unknown outcome.
- The cost: a script that races a slow tool against a timer still waits for that tool
  before it ends.

**Waiting for a signal is not a deadlock**: something outside can still wake the script.
The [run TTL](#run-ttl) bounds it.

`ptc check` catches the forgotten `await` before anything runs
([type-checking](04-tools.md#type-checking)).

## Signals and timers in the glue

- `sleep(d)` and `setTimeout` emit `start_timer`; `timer_fired` resolves them.
  `clearTimeout` emits `cancel_timer`.
- `signal(name)` reads the name's FIFO mailbox. If it is empty, it registers a waiter and
  emits a `signal_awaited` annotation.
- `signal(name, { timeout })` also starts a timer. If the signal arrives first, the
  timer is cancelled (`cancel_timer`); if the timer fires first, the waiter resolves to
  `undefined`.

## Cancellation

`cancel_requested` rejects every pending await in the script with `CancelledError`. The
script may catch it to clean up (it gets one grace activation). Otherwise the run ends
`cancelled`, and its pending tool tasks and timers are cancelled. Cancellation does not
wait for the tool calls in flight: their results are dropped when they arrive.

The grace is the delivery of `cancel_requested` itself: once the script is idle again,
it must have ended by itself (`complete_run`, or `fail_run`, `CancelledError` included). If it
has not, for instance because it awaits a cleanup tool call, the host commits
`cancel_requested` with `run_aborted { cancelled }` and drops what the script emitted in
response, like any host abort.

## Run TTL

Every run has a deadline, `expires_at`, recorded in `run_started`: the time the run was
started plus a TTL of **30 days by default**. The harness can set another TTL when it
starts the run, up to a maximum set by the deployment.

When the deadline passes, the scheduler enqueues `cancel_requested { reason: "ttl" }`.
The run then follows the cancellation path, grace activation included.

The TTL bounds everything that would otherwise wait forever: a `signal()` without a
timeout, a tool call that never answers, a quarantined run, and the time a worker pool
of a previous epoch must stay up.

## Resource limits

- Per call into the script: CPU time (a watchdog thread calls `terminate_execution`).
- Per instance: heap size (V8 heap limit + near-heap-limit callback).
- Per activation: number of commands emitted.
- Per run: history length, payload sizes.

Limit violations end the run with `run_aborted { limit_exceeded }`: the trigger is committed
with `run_aborted`, and what the script emitted in response to it is dropped.

The CPU budget applies to each call into the script (`main(input)` or one delivery, 5 s by
default), not to the whole activation: an activation can replay the full history, and a long
run must not exhaust its budget replaying it. The budget is CPU time of the isolate's thread.
The heap limit is 128 MiB by default.
