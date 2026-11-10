# 3. Execution model

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
offload payloads above the threshold to the BlobStore
derive effects from the new commands: tool tasks, timers, cancelled timers
commit_activation(lease, { entries, consumed inbox items, effects, status })   # one transaction, fenced
release lease (or keep the instance warm)
```

Commands and the effects they create (tool tasks, timers) are committed in the same
transaction (outbox pattern), so an effect can never be lost or duplicated by a crash
between the two. A lease holder that lost its lease cannot commit: the fence no longer
matches. The same transaction writes the feed items that projectors read (telemetry and
read model, see [05](05-testing-and-observability.md#the-feed-and-the-projector)).

Failures of the activation itself:

- The script fails to load (syntax error, forbidden import) → `run_aborted { invalid_script }`.
- Replay detects non-determinism → nothing is committed, and the run is
  [quarantined](#non-determinism-and-quarantine).
- A limit is exceeded → `run_aborted { limit_exceeded }`.

## Inside the isolate

Nothing is asynchronous from V8's point of view:

- Ops are synchronous: they record a command, assign a `seq`, and return it.
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
`fire_at`, so a 10-day sleep takes no wall-clock time.

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
- A toolset whose schemas changed while the run was suspended: dispatch validates args
  against the current schema and fails clearly.

Full replay on every activation is O(history²) over a run's life. Mitigations:

- **Sticky execution**: the worker keeps the instance in memory and gets affinity for
  the run's next activation. Replay is the fallback, not the hot path.
- **`continueAsNew(state)`** for runs that loop forever.
- Histories are capped (default 50k entries).

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
  is then activated again.
- If it is still quarantined at its deadline, or if an operator gives up on it, the host
  ends it with `run_aborted { non_determinism }`.

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
| `main` has not settled, and no tool call, timer or signal wait is pending | Failure, `DeadlockError`: nothing can wake the script any more. |

**Unhandled rejections** follow Node, its known trap included:
`const p = tool(); await other(); await p` fails if `p` is rejected before it is
awaited. V8 reports every promise rejected without a handler; the runtime collects them
and checks after each microtask checkpoint.

**A run does not end while a tool call is in flight.** A forgotten `await` is the most
common mistake in generated code, and `tsc` does not catch it.

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
  what tells a [fork](05-testing-and-observability.md#replay-debugging-and-forks) which
  effects happened.
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

- Per activation: CPU time (a watchdog thread calls `terminate_execution`), heap size
  (V8 heap limit + near-heap-limit callback), number of commands emitted.
- Per run: history length, payload sizes.

Limit violations end the run with `run_aborted { limit_exceeded }`.
