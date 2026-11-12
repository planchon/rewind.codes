<img src="assets/icon.svg" width="64" height="64" alt="">

# ai-ptc

A deterministic, durable TypeScript runtime for programmatic tool calling, for agent
harness developers. The model writes a script that calls MCP and harness tools; ai-ptc
runs it in a V8 sandbox where every interaction with the outside world is journaled.
The journal gives durable execution (`await sleep("10d")`, signals), replay, testing
against recorded runs, and observability.

- Architecture: [archi/](archi/README.md), the specification the code follows.
- TypeScript SDK: [sdk/typescript/](sdk/typescript/README.md), the client, the host-tool worker and the agent tools.
- Dashboard: [dashboard/](dashboard/README.md), the operator's view of every run, served by `ptc-server`.

## Crates

| Crate | Layer | Role |
| --- | --- | --- |
| [`ptc-core`](crates/ptc-core) | domain | The journal, payload identity (RFC 8785), run state, the determinism check. Pure: no I/O, no serde. |
| [`ptc-engine`](crates/ptc-engine) | application | Use cases, ports, activation, replay, driver, in-memory adapters, store conformance suite. |
| [`ptc-format`](crates/ptc-format) | adapter support | The stored journal, format v1. |
| [`ptc-runtime`](crates/ptc-runtime) | adapter | The V8 sandbox behind the `ScriptExecutor` port. |
| [`ptc-blob`](crates/ptc-blob) | adapter | The `BlobStore` on local files and on S3, and the workers' blob cache. |
| [`ptc-architecture-tests`](crates/ptc-architecture-tests) | tooling | The dependency rules between crates. |
| [`ptc-e2e`](crates/ptc-e2e) | tooling | The end-to-end corpus, recorded and replayed. |
| [`evals`](evals) | tooling | The eval suite: models solve tasks end to end against fake MCP servers ([evals/README.md](evals/README.md)). |

Dependencies only point down; `ptc-architecture-tests` fails the build otherwise.

## Commands

```sh
cargo test --workspace                                   # everything (TZ=UTC is set by .cargo/config.toml)
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p ptc-e2e                                    # the end-to-end corpus
cargo test -p ptc-e2e red::deadlock                      # one case
PTC_E2E_RECORD=1 cargo test -p ptc-e2e --test corpus     # record the journals again, then review the diff
cargo run -p ptc-e2e -- replay epoch-1/blue/triage_workflow [--script other.ts]
cargo run -p evals -- list                               # the eval tasks
cargo run -p evals -- run --model claude-opus-5-5        # run models on them (costs tokens)
```

## End-to-end corpus

`crates/ptc-e2e/corpus/epoch-1/{blue,red}/<case>/` holds a `script.ts`, a `scenario.json`
(input, tools, steps such as signals, cancellations and time moving forward, and
expectations) and the `journal.v1.json` recorded from it.

- **Blue** cases must succeed: the run completes, or waits for the outside world.
- **Red** cases must fail exactly as specified: a refused start; a failed, cancelled or
  aborted run (deadlock, unhandled rejection, limits, forbidden imports, cancellation grace,
  deadline); a replay that diverges from the journal; a tampered journal.

Each case runs live and must meet its expectations, must record the committed journal byte
for byte, and the committed journal must replay with the script on a fresh executor. A build
that stops replaying a journal of a determinism epoch it supports fails.
