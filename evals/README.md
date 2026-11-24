# evals

The eval suite of Rewind ([archi/05](../archi/05-testing-and-observability.md#evals)). Models
solve realistic tasks end to end, and the choices that are cheap to change (names, error
formats, the `rewind` library, the prompt rendering, what `run_workflow` returns) are decided on
the numbers ([08](../archi/08-decisions.md#taken-while-building-the-evals-november-2026)).

## What an episode is

```
model ──► system prompt (introduction, the rewind module, the action set in one rendering)
  │        + the agent tools: run_workflow, check_workflow, test_workflow, get_run, list_runs,
  │          signal_run, cancel_run, retry_run, search_actions, update_workflow, get_workflow,
  │          record_tests
  ▼
agent tools (rewind-agent-tools) ──► Api: the world, as /v1 answers ──► run plane in process:
                                       │   tsgo check, V8 sandbox, history, workflows and their
                                       │   suites (the gate), tests, McpDispatcher (the real MCP
                                       ▼   client)
                         fake MCP servers (rmcp, streamable HTTP on 127.0.0.1)
                         tracker · chat · crm · shop · ops · office · shipping
```

1. The task's world starts: its fake servers, the action set pinned from them (discovery through
   `McpDispatcher`), the declarations from `rewind-codegen`, and the run plane at Monday
   2026-03-02T09:00:00Z in virtual time. The task's setup, if it has one, is played first, with
   the agent's labels: runs of code, workflows created through the gate with their suite (on
   the action set as it was then, for a server that changed since), runs of those workflows.
2. The model gets the task and loops: generate a code, check it, run it, fix it. Each tool
   call is handled by `rewind-agent-tools`, the SDK's agent tools in Rust, with the same text
   back: 256 KiB pages, diagnostics with code frames, the trace up to a failure, the tests that
   did not pass. They reach the world through their port `Api`, which the world implements as
   the server answers (`src/api.rs`). A `wait` elapses in virtual time. The agent is a caller
   limited to its labels, in the namespace `local`: the runs and workflows it creates carry its
   labels, a run without them is not found; a run of new code creates a workflow, and
   `update_workflow` names it or makes its next version through its suite.
3. When the model answers (or after 30 turns), the world goes on in virtual time, up to 60
   days: sleeping runs wake up, the world answers signal waits as the task says, servers call
   their webhooks. Then production runs the workflows the task names: the latest version the
   agent sees, as tomorrow's run would.
4. The task's checker looks at the fake servers' state, the runs, the workflows (every
   version, its suite, its lineage) and the model's last answer.

## Tasks

`cargo run -p evals -- list` lists them: 24 tasks from easy to hard, each exercising one or
more of fan-out with `Promise.all`, pagination, `ActionError` and its `data`, results typed by
an output schema or read with `.json()`, `Date.now()`, bulk updates, outputs above 256 KB,
sleeping and polling, signals with and without a timeout, webhooks, a rate limit with
idempotent retries, several servers joined, and the runs of an earlier turn found, cancelled
or corrected with `retry_run`.

Two are repair tasks ([05](../archi/05-testing-and-observability.md#evals)): the model gets a
workflow with its suite, and succeeds when its fix passes the gate and does what the workflow
is for.

| Task | The model must |
| --- | --- |
| `fix_workflow_regression` | Find the failed run of `ship_orders` (an order collected in store has a label without tracking), record it as a test and give it the expectation it should have met (`record_tests`, `{ from, expect }`), save a fix that passes the inherited suite and that test, and finish the run with `retry_run`. The checker wants a new version derived from the old one, keeping its tests and adding one recorded from the failed run, a completed fork, and one label per order: a label bought twice is paid twice. |
| `upgrade_workflow_suite` | Adapt `bug_digest` to the tracker's new `list_issues` (`labels` renamed `label`, a reporter on every issue, a cursor instead of a truncated list): its tests are stale, so it updates those that still hold, each with a reason, and retires the one about truncated lists or updates it too. The checker wants a new version keeping the tests of what the workflow is for, a reason on every update and retirement, and production's run of the new version posting every open bug to #bugs. |

Each task is in `src/tasks/`; its checker computes what it expects from the fixtures of
`src/fake/`. Each has a reference solution in `tests/solutions/`, which `cargo test` runs
through the scripted model: a checker that cannot be passed measures nothing.

## Running

Models run only on demand: they cost tokens.

```sh
cargo run -p evals -- list
cargo run -p evals -- run                                     # Claude Opus, Sonnet, Haiku and gpt-6-astra, compact rendering
cargo run -p evals -- run --model claude-sonnet-5-5 --task fix_failed_run --repeat 3
cargo run -p evals -- run --model claude-haiku-5-5 --rendering all
cargo run -p evals -- run --model scripted:turns.json        # replays turns from a file, no key
```

| Option | Default | |
| --- | --- | --- |
| `--model` | the four of 08 | `claude-…` (`ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`), `gpt-…` (`OPENAI_API_KEY`, `OPENAI_BASE_URL`), `scripted:<file.json>` |
| `--rendering` | `compact` | `compact`, `dts`, `search`, or `all` |
| `--task` | every task | |
| `--repeat` | 1 | episodes per task, model and rendering |
| `--jobs` | 4 | episodes at the same time |
| `--max-turns` | 30 | |
| `--effort` | `high` | the models' reasoning effort; `default` leaves the API's |
| `--out` | `target/evals/<time>` | |

A scripted file is an array of turns, `{ "text"?, "calls"?: [{ "name", "input" }] }`; in an
input, `{{run_id}}` stands for the last run id the tool results gave.

## Reports

Each run writes to its output directory:

- `summary.md`: per model and rendering, the first-try check pass rate, task success, model
  turns, codes submitted, the share of episodes where the model wrote tests (`test_workflow`,
  `update_workflow`), the fixes the gate refused per episode, tokens and errors; then each
  task's successes per column, and why each failed episode failed.
- `report.json`: the same summaries, and every episode's record.
- `transcripts/`: each episode's system prompt and turns, tool results included.

An episode whose model API or the harness failed counts as an error, and is left out of the
rates.
