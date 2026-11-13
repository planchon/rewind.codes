# Acceptance of 0.1

Each criterion of [09, "0.1 is done when"](../archi/09-roadmap.md#01-is-done-when) proven end to
end on the real `ptc` binary: real processes, real Postgres, MinIO and OpenTelemetry Collector in
Docker, the real reference MCP server, the dashboard in a browser.

```sh
acceptance/run.sh          # every criterion
acceptance/run.sh 1 7      # some of them
```

It needs Rust, Node 22.12 or later with pnpm 10, and Docker. It builds `ptc`, the evals, the SDK
and the suite, runs every check, removes its containers and stops its servers, and writes
`acceptance/results/`: `summary.md` (the status of each criterion), `report.json` and `html/`
(Playwright's reports), and each server's and step's log.

## What proves each criterion

| Criterion | Proven by | How |
| --- | --- | --- |
| 1. A harness drives runs through the SDK, with several workers on one Postgres; each run is one trace under the harness's span; runs are searched by label and tool | [`tests/c1-sdk-workers-postgres.spec.ts`](tests/c1-sdk-workers-postgres.spec.ts) | Postgres, MinIO and a Collector writing to a file; `ptc server --role api` and two `--role worker`. The SDK registers host tools, serves them with its worker, starts 6 runs (each with its own `traceparent` and labels, retried with the same idempotency key), signals them, approves a call under the `approval` policy, and waits for them. The runs are found with `runs.list` by label and by tool; a 100 KiB result lands on MinIO; in the Collector's output each run is one trace under its harness span (run, tool call, timer and signal wait spans, and the workers' activation spans, under the run span), both workers ran activations, and the host tool got the tool call span as its parent |
| 2. Locally, `ptc run`, `ptc signal`, `ptc replay` and `ptc trace` on a run that sleeps, is interrupted and resumes in another process; a forgotten `await`, an unhandled rejection and a deadlock end as specified | [`crates/ptc/tests/cli.rs`](../crates/ptc/tests/cli.rs) (run by `run.sh`) and [`tests/c2-local-endings.spec.ts`](tests/c2-local-endings.spec.ts) | `cli.rs`: a run sleeps, its embedded server is killed, a new process signals it, a new server fires its timer, it completes; `ptc status`, `trace`, `replay` and `export`; a signal from another process; `kill -9` mid-activation. The spec: `ptc check` refuses the forgotten `await`; run anyway, the run completes only once the forgotten call has its outcome; a forgotten call that fails, and a rejection handled too late, fail the run with the `ToolError` and name the call; a deadlock fails with `DeadlockError`; `ptc runs --error` finds them |
| 3. A script calls a real MCP server, with types checked beforehand | [`tests/c3-c5-mcp-test-fork.spec.ts`](tests/c3-c5-mcp-test-fork.spec.ts), criterion 3 | `@modelcontextprotocol/server-everything` over stdio through `npx`, registered with `ptc toolsets add`. `ptc check` refuses `get-sum({ a: "2" })` (TS2322) against the registered toolset and the file, `ptc run` refuses it, and no run exists; a correct script calling three of its tools (one with an output schema) completes. The server's own log (every JSON-RPC message it received) shows the three calls with the idempotency key and the trace context |
| 4. The evals report, per model and prompt rendering, the first-try check pass rate, task success, iterations and tokens | [`tests/c4-evals.spec.ts`](tests/c4-evals.spec.ts) | `evals run` with two scripted models (one right first time, one refused by the check then fixed) on the three renderings: `report.json` has one summary per model and rendering with each measure, and `summary.md` the table. Real models need API keys this suite does not have: their run is pending keys |
| 5. `ptc test` replays a recorded real run as a scenario; `ptc fork` continues a failed run with a corrected script without repeating its recorded calls | [`tests/c3-c5-mcp-test-fork.spec.ts`](tests/c3-c5-mcp-test-fork.spec.ts), criterion 5 | The run of criterion 3 exported as a cassette and replayed by `ptc test` (the MCP server receives nothing; a wrong expectation fails). A run that fails after two MCP calls is forked with a corrected script: the fork completes and the MCP server's log shows no new call |
| 6. Load tests and limit tests pass | `deploy/acceptance.sh` (run by `run.sh`) | Built by the operations workstream: see 09 for its state |
| 7. An operator finds any run by label, tool or error in the dashboard, and sees its code next to its trace | [`tests/c7-dashboard.spec.ts`](tests/c7-dashboard.spec.ts) | The dashboard served by `ptc server`, in Chromium. The operator signs in with a `reader` key and lands on the Overview (one run waiting now). In Runs, the query bar finds runs by label (`@team:support`), by tool, by a tool call's error kind and by error name; a facet's value narrows the list; a misspelt field is underlined with the server's message. The operator opens a run and sees its source with each call, timer and log next to its line, and its timeline; with the time cursor, steps to its first step and the next (the marks of each line, the timeline's playhead), hovers a variable bound to an awaited call and sees what it returned, and watches it as the cursor moves. Waiting lists the run waiting for a signal, with the line it is paused on; Failures groups the deadlock, and the group opens its run; a failed run shows its failed call on its line and its error |

## Layout

| Path | Holds |
| --- | --- |
| `run.sh` | The one command |
| `tests/` | One Playwright spec per criterion (3 and 5 share one: 5 replays and forks the runs of 3) |
| `support/` | Servers and processes, Docker containers, the project directory of the CLI, the Collector's output, the summary |
| `results/` | Not committed: the last run's reports and logs |

Specs can also run one by one, once `ptc` and the SDK are built: `pnpm --dir acceptance exec
playwright test tests/c7-dashboard.spec.ts`. `PTC_BIN` names another `ptc` binary.
