# Changelog

Every version of `@rewind.codes/sdk`. The package follows [Semantic Versioning](https://semver.org).

## 0.2.0 (2026-11-15)

For the HTTP API `/v1` with namespaces (the end users of a harness, their tokens and their quotas) and workflows
(named scripts with typed inputs, outputs and signals, that scripts start and signal).

- `rewind.namespace(name)`: the same client acting for one end user, whose requests name its namespace
  (`Rewind-Namespace`). It starts runs, checks and tests scripts in that namespace, and narrows reads to it.
- `new Rewind({ token })`: a client holding an end user's token, opaque (`rewind_nt_…`) or a JWT the harness signed
  with a key it registered. It reaches that end user's namespace only.
- `rewind.namespaces`: `list` and `listAll` (by prefix, above a share of a limit, ordered by CPU time, runs, scripts or
  name; a page says how many match, `total`), `get` (limits, use per day, alerts, tokens in use), `put` (exceptions to the default limits), `delete`,
  `setCredentials` (an end user's credentials for a toolset) and `alerts`; `rewind.namespaces.defaults` (`get`, `put`);
  `rewind.namespaces.tokens` (`create`, `revoke`, `revokeAll`); `rewind.tokenKeys` (`create`, `list`, `delete`).
- The host-tool worker gives each handler the end user it acts for: `ctx.namespace`.
- The agent tools work with a client bound to a namespace or holding a token. A quota reached is told to the model.
- `RewindApiError.quota` says which quota a `quota-exceeded` problem reached; `isNamespaceName` checks a name; the
  problem slugs `namespace-deleting`, `role-in-use`, `last-owner` and `identity-unavailable`; the types of namespaces,
  quotas, alerts, tokens and token keys.
- `verifyWebhook` accepts `quota.alert`, the alert of a limit crossed.
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

Breaking changes:

- A key's client must name the end user to start a run, check or test a script: `rewind.namespace(user.id).runs.start(…)`.
  Without a namespace the server answers 400, thrown as `NamespaceRequiredError` (a `RewindApiError`); the agent tools
  throw it to the harness instead of answering the model.
- `Run`, `RunSummary`, `Task` and the events about a run name their `namespace`.
- `apiKeys.create` takes a `name` and a `role` by name (a built-in role or one of the organization's); only the
  operator names a `tenant`. `Role` describes a role (its name, description and permissions), no longer the name of
  a key's role.
- `AgentToolsClient` needs `workflows`.

## 0.1.0 (2026-11-14)

The first version, for the HTTP API `/v1` of Rewind 0.1.

- The client of the API: toolsets, scripts, runs (signals, cancellations, forks, traces, journals and their event
  stream), tasks, approvals, payloads, webhook subscriptions, tools, API keys and the server's version.
- The host-tool worker, which executes the harness's own tools when scripts call them.
- The agent tools (`run_script`, `get_run`…), with adapters for the Anthropic and OpenAI tool formats.
- `verifyWebhook`, the Standard Webhooks signature check of deliveries.
