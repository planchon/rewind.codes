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
- Replay detects non-determinism → `run_aborted { non_determinism }`.
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

Scripts are immutable and content-addressed, so there is no workflow versioning problem.
The remaining risks:

- A change in the ai-ptc runtime itself: `runtime_version` is pinned in `run_started`,
  and workers refuse a run whose version they can't honor.
- A toolset whose schemas changed while the run was suspended: dispatch validates args
  against the current schema and fails clearly.

Full replay on every activation is O(history²) over a run's life. Mitigations:

- **Sticky execution**: the worker keeps the instance in memory and gets affinity for
  the run's next activation. Replay is the fallback, not the hot path.
- **`continueAsNew(state)`** for runs that loop forever.
- Histories are capped (default 50k entries).

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
`cancelled`, and its pending tool tasks and timers are cancelled.

## Resource limits

- Per activation: CPU time (a watchdog thread calls `terminate_execution`), heap size
  (V8 heap limit + near-heap-limit callback), number of commands emitted.
- Per run: history length, payload sizes.

Limit violations end the run with `run_aborted { limit_exceeded }`.
