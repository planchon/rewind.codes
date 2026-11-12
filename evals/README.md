# evals

The eval suite of ai-ptc ([archi/05](../archi/05-testing-and-observability.md#evals)). Models
solve realistic tasks end to end, and the choices that are cheap to change (names, error
formats, the `ptc` library, the prompt rendering, what `run_script` returns) are decided on
the numbers ([08](../archi/08-decisions.md#taken-while-building-the-evals-november-2026)).

## What an episode is

```
model ──► system prompt (introduction, the ptc module, the toolset in one rendering)
  │        + the agent tools: run_script, check_script, test_script, get_run, list_runs,
  │          signal_run, cancel_run, retry_run, search_tools
  ▼
agent tools (the SDK's, ported) ──► engine in process: tsgo check, V8 sandbox, journal
                                       │   McpDispatcher (the real MCP client)
                                       ▼
                         fake MCP servers (rmcp, streamable HTTP on 127.0.0.1)
                         tracker · chat · crm · shop · ops · office
```

1. The task's world starts: its fake servers, the toolset pinned from them (discovery through
   `McpDispatcher`), the declarations from `ptc-codegen`, and the engine at Monday
   2026-03-02T09:00:00Z in virtual time. Setup runs, if the task has any, run first.
2. The model gets the task and loops: generate a script, check it, run it, fix it. Each tool
   call is handled as the SDK handles it, with the same text back: 256 KiB pages, diagnostics
   with code frames, the trace up to a failure. A `wait` elapses in virtual time.
3. When the model answers (or after 30 turns), the world goes on in virtual time, up to 60
   days: sleeping runs wake up, the world answers signal waits as the task says, servers call
   their webhooks.
4. The task's checker looks at the fake servers' state, the runs and the model's last answer.

## Tasks

`cargo run -p evals -- list` lists them: 22 tasks from easy to hard, each exercising one or
more of fan-out with `Promise.all`, pagination, `ToolError` and its `data`, results typed by
an output schema or read with `.json()`, `Date.now()`, bulk updates, outputs above 256 KB,
sleeping and polling, signals with and without a timeout, webhooks, a rate limit with
idempotent retries, several servers joined, and the runs of an earlier turn found, cancelled
or corrected with `retry_run`.

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
  turns, scripts, tokens and errors; then each task's successes per column, and why each
  failed episode failed.
- `report.json`: the same summaries, and every episode's record.
- `transcripts/`: each episode's system prompt and turns, tool results included.

An episode whose model API or the harness failed counts as an error, and is left out of the
rates.
