# Changelog

Every version of `@rewind.codes/sdk`. The package follows [Semantic Versioning](https://semver.org).

## Unreleased

Workflows: named scripts with typed inputs, outputs and signals, that scripts start and signal.

- `rewind.workflows`: `create` (a new version; `invalid-script` with diagnostics, or `invalid-workflow`), `list` and
  `listAll` (the latest version of each), `versions`.
- `runs.start({ workflow: "triage" | "triage@<version>", input })` starts a run of a workflow instead of a script
  (`invalid-input` when the input does not match). `Run` and `RunSummary` have `workflow` and `startedBy`, and
  `runs.list` filters by them (`workflow`, `startedBy`; `workflow:triage` and `started_by:<run id>` in `q`).
- `toolsets.create` takes `workflows` (pinned for its scripts' `rewind:workflows`) and `base` (derives from a
  registered toolset); `Toolset` has `workflows`.
- The problem slugs `invalid-workflow`, `invalid-input` and `invalid-signal`.
- The agent tool `save_workflow`: it saves a workflow with the agent's toolset, derives a toolset that pins it, and
  the later `run_script`, `check_script`, `test_script` and `search_tools` use it (`agentTools.toolset`).
  `signal_run` says when the run refuses the signal (`invalid-signal`), and `get_run` which workflow a run executes
  and which run started it.
- `AgentToolsClient` needs `workflows`.

## 0.1.0 (2026-11-14)

The first version, for the HTTP API `/v1` of Rewind 0.1.

- The client of the API: toolsets, scripts, runs (signals, cancellations, forks, traces, journals and their event
  stream), tasks, approvals, payloads, webhook subscriptions, tools, API keys and the server's version.
- The host-tool worker, which executes the harness's own tools when scripts call them.
- The agent tools (`run_script`, `get_run`…), with adapters for the Anthropic and OpenAI tool formats.
- `verifyWebhook`, the Standard Webhooks signature check of deliveries.
