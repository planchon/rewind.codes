<img src="assets/icon.svg" width="64" height="64" alt="">

# Rewind

A deterministic, durable TypeScript runtime for programmatic tool calling, for agent
harness developers. The model writes a script that calls MCP and harness tools; Rewind
runs it in a V8 sandbox where every interaction with the outside world is journaled.
The journal gives durable execution (`await sleep("10d")`, signals), replay, testing
against recorded runs, and observability.

- Site: [rewind.codes](https://rewind.codes); the hosted dashboard is [app.rewind.codes](https://app.rewind.codes).
- Documentation: [docs.rewind.codes](https://docs.rewind.codes), from [docs/](docs/README.md) (Mintlify); runnable
  [examples/](examples/README.md) of the SDK.
- Architecture: [archi/](archi/README.md), the specification the code follows.
- TypeScript SDK: [sdk/typescript/](sdk/typescript/README.md), the client, the host-tool worker and the agent tools.
- Dashboard: [dashboard/](dashboard/README.md), the operator's view of every run, served by `rewind-server`.
- Self-hosting: [deploy/](deploy/README.md), a deployment of nodes on Postgres (the control plane) and a bucket on R2
  or S3 (the runs).

## Crates

| Crate | Layer | Role |
| --- | --- | --- |
| [`rewind-core`](crates/rewind-core) | domain | The journal, payload identity (RFC 8785), run state, the determinism check. Pure: no I/O, no serde. |
| [`rewind-engine`](crates/rewind-engine) | application | Use cases, ports, the actors of a node (partition owners, executors, organizations, router, edge), replay, the deterministic driver, in-memory adapters, conformance suites. |
| [`rewind-format`](crates/rewind-format) | adapter support | The stored formats, v1: journals, partition records, cassettes, pinned toolsets. |
| [`rewind-runtime`](crates/rewind-runtime) | adapter | The V8 sandbox behind the `ScriptExecutor` port. |
| [`rewind-store-slatedb`](crates/rewind-store-slatedb) | adapter | The partitions on SlateDB, in a bucket or a directory. |
| [`rewind-store-postgres`](crates/rewind-store-postgres), [`rewind-store-turso`](crates/rewind-store-turso) | adapter | The control plane: the catalog, access, secrets, and the cluster's members. |
| [`rewind-blob`](crates/rewind-blob) | adapter | The `BlobStore` and the run archive on local files and on S3, and their cache. |
| [`rewind-server`](crates/rewind-server) | composition root | `rewind server`: one process is the HTTP edge and a node; the embedded server of a `.rewind/` directory. |
| [`rewind-architecture-tests`](crates/rewind-architecture-tests) | tooling | The dependency rules between crates, no timer outside the timer wheel, the backoff helpers and the clocks that read no store, and `unsafe` only where it is allowed. |
| [`rewind-e2e`](crates/rewind-e2e) | tooling | The end-to-end corpus, recorded and replayed. |
| [`evals`](evals) | tooling | The eval suite: models solve tasks end to end against fake MCP servers ([evals/README.md](evals/README.md)). |

Dependencies only point down; `rewind-architecture-tests` fails the build otherwise.

## Commands

```sh
scripts/gate.sh                                              # before a commit: fmt, clippy, every test, docs (needs Docker)
cargo nextest run -p rewind-engine                           # one crate's tests (TZ=UTC is set by .cargo/config.toml)
cargo nextest run --workspace                                # every test (cargo install cargo-nextest)
cargo test -p rewind-e2e                                     # the end-to-end corpus
cargo test -p rewind-e2e red::deadlock                       # one case
REWIND_E2E_RECORD=1 cargo test -p rewind-e2e --test corpus   # record the journals again, then review the diff
cargo run -p rewind-e2e -- replay epoch-1/blue/triage_workflow [--script other.ts]
cargo run -p evals -- list                                   # the eval tasks
cargo run -p evals -- run --model claude-opus-5-5            # run models on them (costs tokens)
```

## End-to-end corpus

`crates/rewind-e2e/corpus/epoch-1/{blue,red}/<case>/` holds a `script.ts`, a `scenario.json`
(input, tools, steps such as signals, cancellations and time moving forward, and
expectations) and the `journal.v1.json` recorded from it.

- **Blue** cases must succeed: the run completes, or waits for the outside world.
- **Red** cases must fail exactly as specified: a refused start; a failed, cancelled or
  aborted run (deadlock, unhandled rejection, limits, forbidden imports, cancellation grace,
  deadline); a replay that diverges from the journal; a tampered journal.

Each case runs live and must meet its expectations, must record the committed journal byte
for byte, and the committed journal must replay with the script on a fresh executor. A build
that stops replaying a journal of a determinism epoch it supports fails.

## License

[FSL-1.1-ALv2](LICENSE.md), the Functional Source License with Apache 2.0 as its future
license. Any use is allowed except offering Rewind, or something substantially similar, as a
competing commercial product or service. Each version becomes Apache 2.0 two years after it
is released.
