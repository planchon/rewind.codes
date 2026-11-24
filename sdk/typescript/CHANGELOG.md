# Changelog

Every version of `@rewind.codes/sdk`. The package follows [Semantic Versioning](https://semver.org).

## 0.6.0 (unreleased)

The release that renames Rewind in the words of its glossary, and makes a workflow one entity. Everything Rewind runs
is a workflow, and its TypeScript text is its code; what a workflow calls is an action, in an action set; a call of a
harness action is a call; a run's record is its history, made of events. Every renamed method, type, option and value
breaks: the API keeps no old path, so 0.6.0 speaks only to a server of the same release.

### The workflow model

A workflow has an id (`wf_…`), a name unique in its scope if it has one, a description, versions numbered from 1 (each
a code, its interface, its action set and its secrets, with `version` its number and `digest` its hash) and a suite. It
belongs to the namespace whose run created it, or to the organization. Code a namespace runs for the first time becomes
a workflow of that namespace; the same code run again is a run of the same workflow.

- `rewind.workflows` holds the calls of a workflow: `check({ code, actionSet })` and `test(request)` (were
  `scripts.check` and `scripts.test`; a check creates nothing, and answers the workflow that already has the code, with
  its version, as `workflow`), `create`, `list` and `listAll` (`scope`, `code`, `q`, `since`, `until`; each workflow
  with its latest version and the counts of its runs), `get(workflow)` (by id or name, `@v12` or `@<digest>` for one
  version), `update(workflow, { name, description })`, `createVersion(workflow, version)` and
  `addTests(workflow, { tests, replace, retire })` through the gate, `promote`, `fork`, `pull`, and `record`. Every
  method that names a workflow takes its id or its name.
- New: `update`, `createVersion`, `addTests`, `promote`, `fork` (from a client bound to the namespace it forks into:
  `NamespaceRequiredError` otherwise) and `pull`; the problem slugs `name-taken`, `not-forkable` and `not-a-fork`.
- `workflows.create` makes a workflow and its version 1, the client's namespace's or, without one, the organization's;
  it no longer makes versions (`createVersion` does): `parent` goes. `workflows.versions(name)` goes: `get` answers the
  versions, newest first.
- `rewind.scripts` goes: `list` and `get` are `workflows.list` and `workflows.get`; `setTitle`, `ScriptTitleParams`,
  `ListScriptsParams`, titles, `headline`, `scriptTitle`, `titleSource`, the permission `scripts:write` and the slug
  `invalid-title` go: a workflow's only label is its name, else the first line of the comment at the top of its code
  (`WorkflowVersion.comment`).
- `Workflow` is the workflow (`id`, `shortId`, `name`, `description`, `namespace`, `source`, `promotedAt`, `suite`,
  `versions`, `code`) and the version shown (`version`, a `WorkflowVersion`: `version`, `digest`, `codeDigest`,
  `actionSet`, `input`, `output`, `signals`, `secrets`, `base`, `tests`, `retired`, `replaced`, `comment`,
  `declarations`). `WorkflowSummary` (an item of `list`), `WorkflowSource`, `SuiteEntry`, `CreateVersion`,
  `ChangeTests`, `PatchWorkflow` and `CodeComment` are new; `Script`, `ScriptSummary`, `ScriptPage`, `ScriptChanged`
  and `WorkflowVersions` go.
- `Run.workflow` is `{ id, name, version, digest }` (`version` the number, was `number`; `digest` the hash, was
  `version`), and so is `PinnedWorkflow` with its `id`; a run's handle takes its workflow's name, else its short id.

### Renamed

- **Resources.** `rewind.toolsets` → `rewind.actionSets` (`Toolsets` → `ActionSets`); `rewind.tools` →
  `rewind.actions` (`Tools` → `Actions`, `ToolStatsParams` → `ActionStatsParams`: `prefix` → `group`, a group's name,
  `tool` → `action`, and `actionSet`); `rewind.tasks` → `rewind.calls` (`Tasks` → `Calls`, `TaskFailure` →
  `CallFailure`, `TaskStreamParams` → `CallStreamParams`: `toolset` → `actionSet`, `tools` → `actions`).
- **Runs.** `runs.start({ script, toolset })` → `runs.start({ code, actionSet })` (`ScriptTarget` → `CodeTarget`);
  `runs.fork(id, { script })` → `{ code }`; `runs.trace(id)` → `runs.transcript(id)` (`Trace` → `Transcript`, no
  format); `runs.entry(id, idx)` → `runs.event(id, idx)` (`Entry` → `HistoryEvent`); `runs.journal(id, { as:
"journal" })` → `runs.export(id, { as: "run" })`; `runs.events` yields `StreamMessage` (was `StreamEvent`), whose
  `event` messages were `entry`. `runs.list`: `tool` → `action`, `parent` → `source`, `script` goes (`workflow` takes
  a workflow's id or name); `runs.stats` and `runs.failures`: `bucket` → `interval`; `runs.failures` answers a
  `FingerprintPage` (`fingerprints`, `totalFingerprints`; were `FailureGroupPage`, `groups`, `totalGroups`).
- **Statuses.** `queued`, `running` and `waiting` (were `pending` and `active`), then `completed`, `failed`,
  `cancelled`, `aborted`; `waitingFor` lists `signal`, `timer`, `call` (with its `state`) and `quarantined`.
  `TERMINAL_STATUSES` → `ENDED_STATUSES`, `isTerminal` → `isEnded`. A test expects `status: "waiting"` (was
  `"active"`) and sends its signals `when: "waiting"` by default (was `"blocked"`).
- **The worker.** `HostToolWorker` → `Worker`, `HostToolError` → `HarnessActionError`, `HostToolContext` →
  `HarnessActionContext`, `HostToolHandler` → `HarnessActionHandler`, `DeferredTask` → `DeferredCall` (`taskId` →
  `callId`). `rewind.worker({ toolset, tools })` → `rewind.worker({ actionSet, actions })`. The context's `task` →
  `call`, `attempt` → `delivery`, and `callId` is new; `WorkerErrorInfo.task` → `call`. The worker holds the call
  stream (`GET /v1/calls/stream`, messages `call` and `gone`; `calls.stream` yields `{ type: "call", call }`, was
  `{ type: "task", task }`), and a call that no longer waits aborts `ctx.signal` with `call-gone` (was `task-gone`).
- **Approvals.** `PendingApproval.taskId` → `callId`, `tool` → `action`, `attempt` → `delivery`; a run that waits for
  one is `waiting`, and the webhook is `run.waiting`.
- **Webhooks.** `verifyWebhook` and `rewind.webhooks.verify` answer a `Webhook` (was `WebhookEvent`) of a `WebhookType`
  (was `WebhookEventType`): `run.suspended` → `run.waiting`, `tool.schema_changed` → `action.schema_changed`,
  `credentials.expired` → `secret.expired`; `RunEvent` → `RunWebhook`. A subscription lists its `types` (was `events`).
- **Signed calls.** `verifyRewindCall` reads the claim `action` and answers `action` (was `tool`); a JWT that names
  its action only as `tool` is refused (`call`). `rewindIssuer(url, organizationId)` and
  `signingKeys.jwks(organizationId)` (were `tenant`).
- **Errors.** The slugs `invalid-script` → `invalid-code`, `invalid-toolset` → `invalid-action-set`,
  `toolset-unavailable` → `action-set-unavailable`, `task-gone` → `call-gone`, `task-not-waiting` →
  `call-not-waiting`. `RewindApiError.attachments` → `secrets` (`SecretProblem`, whose `attachable` → `usable`): a run
  uses secrets, it does not attach them.
- **Agent tools.** `run_script`, `check_script`, `test_script` → `run_workflow`, `check_workflow`, `test_workflow`;
  `search_tools` → `search_actions`; `save_workflow` → `update_workflow`, which names, describes and versions a
  workflow by its id or its name, or adds tests to its suite (`replace` was `update`). `AgentToolsOptions.toolset` →
  `actionSet`; the agent tools no longer derive an action set: `agentTools.toolset` and `AgentToolResult.toolset` go.
  `run_workflow` takes `code` or `workflow` and names the workflow by its id and version in its result;
  `check_workflow` answers the workflow that already has the code; `get_workflow` and `record_tests` take a workflow's
  id or name; `get_run` takes `event` (was `entry`) and returns the run's transcript. `AgentToolResult.code`
  (`CodeVerdict`: `passed` or `refused`) is new: whether the checks passed the code a call handed in.
- **Types**, as the API's: `Toolset`, `ToolsetSummary`, `CreateToolset` → `ActionSet…`, `CreateActionSet`; `Tool…`
  → `Action…` (`ActionFailure`, `ActionMatch`, `ActionPolicy`, `ActionRule(s)`, `ActionSchemaChanged`,
  `ActionSearchResults`, `ActionStats(Page)`, whose `retries` → `redeliveries`); `HostTool` → `HarnessAction`,
  `CreateActionSet.harness` (was `host`), `Route` `harness` (was `host`); `HttpTool` → `HttpAction`; `ToolPolicy`'s
  `onInterruption: "retry" | "at_most_once"` → `ActionPolicy.idempotent: true | false`, `heartbeatTimeout` →
  `reconnectTimeout`, `OnInterruption` goes; `HandedTask`, `TaskGone`, `TaskState` → `HandedCall`, `CallGone`,
  `CallState`; `Run.attempts` → `deliveries` (`Deliveries`), `HandedCall.delivery` and a waiting call's `delivery` (were
  `attempt`); `Run.script` → `codeDigest`, `Run.toolset` → `actionSet`, `Run.parent` → `source` (`ForkParent` →
  `ForkSource`); a waiting call's `kind` is `call` (was `tool_call`), with `action` and `callId` (were `tool`,
  `taskId`); `RunMode` `test` (was `mock`); `World.tools` → `actions`, `World.mocks` → `handlers`; `ExpectedCall.tool`
  → `action`; `TestFrom { from, expect }` → `{ test, expect }`; `Test.origin` → `from` (`TestOrigin` → `TestFromRun`);
  `TestRun` → `TestReport`; `TestResult.trace` → `transcript`; `TestUpdate` → `TestReplacement` (with `new`),
  `UpdatedTest` → `ReplacedTest` (`old`, `new`), a version's `updated` → `replaced`; `RunError` and `ScriptError` →
  `ErrorInfo`; `AbortKind` `invalid_code` (was `invalid_script`); `ChangeEvent` → `Change`; `FailureGroup` →
  `Fingerprint`; `SeriesBucket` → `SeriesInterval`; `AttachmentProblem(Kind)` → `SecretProblem(Kind)`;
  `CredentialsExpired(Reason)` → `SecretExpired(Reason)`; `WorkflowAttaching` → `WorkflowSecrets`; `Quotas.scripts` →
  `workflows`, `maxScriptLines` → `maxCodeLines`, and the namespaces' order `scripts` → `workflows`; `Identity.tenant`
  and `ApiKey.tenant` → `organizationId`; `RunSummary.toolCalls` → `calls`. The permissions `toolsets:read` and
  `toolsets:write` → `actions:read` and `actions:write`, `tasks:work` and `tasks:approve` → `calls:work` and
  `calls:approve`, and `workflows:write` names, versions, promotes, forks and pulls workflows.

## 0.5.0 (2026-11-23)

For the run plane on object storage, where nothing polls: host tools are handed on a task stream, a wait is one
request. Runs are named by their handles, scripts have titles, and workflow versions have numbers.

- The host-tool worker holds a task stream open (`GET /v1/tasks/stream`, server-sent events `task` and `gone`) and is
  handed each task as it becomes ready; the open stream is the liveness of every task handed on it. The long-poll and
  the heartbeats are gone: `ctx.heartbeat()`, the option `pollWait` and the claim's `heartbeatTimeoutMs` are removed.
  `concurrency` is the stream's slots (100 at most). After a drop the worker opens the stream again with the same id
  (`worker.id`), after a backoff that starts over once a stream lived, and its handlers go on: the server keeps their
  tasks for their grace. A stream silent for 45 s is taken as broken. `stop()` keeps the stream open with no slot
  until the handlers in flight have answered. `timings` tunes the backoff and the silence. `WorkerErrorInfo.phase` is
  `stream`, `answer` or `handler`.
- `ctx.signal` aborts, with `task-gone`, when the stream says the task is gone (its run ended, its call timed out).
- `rewind.tasks.stream(params)` reads one task stream (`toolset`, `tools`, `namespace`, `slots`, `worker`);
  `tasks.claim`, `tasks.heartbeat` and `ClaimParams` are removed. The types `HandedTask`, `TaskGone` and
  `TaskStreamEvent` replace `Task`, `TaskBatch` and `HeartbeatAck`.
- `runs.start`, `runs.fork`, `runs.cancel` and `runs.get` with `wait` send one request, which the server holds until
  the run ends or is quarantined, or `wait` passes, however long; nothing is asked again once it answered. A connection
  cut while it is held is sent again with the time left; a start or a fork that waits carries an idempotency key (one
  the client draws when none is given). `MAX_REQUEST_WAIT_MS` is removed.
- The statistics take a scope and a window: `runs.stats`, `runs.failures`, `scripts.list` (`q`, `since`, `until`,
  `digest`, `tool`) and `tools.stats` (`q`, `prefix`, `tool`, `since`, `until`); `q` holds at most one `namespace:` term
  (`invalid-query` otherwise), and a key or a token limited to labels is refused (`forbidden`): counters know no
  labels. `scripts.list` loses `labels`, `tools.stats` loses `toolset`; `ScriptSummary.labels` is removed.
- `payloads.get(digest, { run })` requires `run`: a payload is read through the run that holds it.
- `server.info()` answers the node that answered (`node`), the deployment's `partitions` and `nodes`; `roles` and the
  type `ServerRole` are removed.
- The generated types describe the change stream's control-plane events (`script`, `workflow`, `secret`, `namespace`,
  `connection`: `ScriptChanged`, `WorkflowChanged`, `SecretChanged`, `NamespaceChanged`, `ConnectionChanged`), the
  facet of workflows, and the pages of `GET /v1/runs` that may be short with a cursor.
- `Run` and `RunSummary` have `handle`, the run's readable name `<namespace>/<origin>/<suffix>`
  (`dana/triage/06bqx3k9f2mt`), and `scriptTitle`; `id` stays the run's UUID. `Run.parent` and `Run.startedBy` have
  the `handle` of the run they name, a secret's `lastRead` its run's `runHandle`, `PendingApproval` its
  `runHandle`, and a failure group's `latest` (`FailureSample`) its `handle`. Every method of `rewind.runs` that
  takes a run's id takes its handle too, percent-encoded in the path; so do the `parent` and `startedBy` filters of
  `runs.list`.
- `Script` and `ScriptSummary` have `title`, `description` and `titleSource` (`person` or `comment`): the title a
  person gave the script, else the first line of the comment at its top.
  `scripts.setTitle(digest, { title, description })` gives one for the whole organization, `{ title: null }` takes it
  away; it needs the new permission `scripts:write`, which the built-in roles `owner` and `harness` hold. The problem
  slug `invalid-title`.
- `Workflow` and `Run.workflow` have `number`, the version's number among the versions of its workflow;
  `triage@v12` names a version wherever `triage@<version>` does (`runs.start({ workflow })`, `scripts.test`, a
  toolset's `workflows`, a workflow's `parent`).
- The agent tools name runs by their handle and take one wherever they take a run's id. `run_script` and
  `check_script` ask the model to start a script with a comment giving its title (`/** <title> … */`), and
  `check_script` adds a note when the script has none.

## 0.4.0 (2026-11-22)

For the live dashboard and the table of runs ordered from its headers.

- `runs.list({ sort })` takes `oldest` (the first started first), `duration` (the longest first, the runs that have not
  ended last) and `calls` (the most tool calls first), beside `started`, `activity` and `wakes`.
- The generated types (`paths`, `components`) describe `GET /v1/changes`, the change stream: server-sent events `run`,
  each with a run's summary as `GET /v1/runs` lists it, and `resync` (`ChangeEvent`).

## 0.3.0 (2026-11-22)

For named secrets attached to runs, API tools from OpenAPI documents, and calls signed with a key per organization.

- `rewind.secrets` (`list`, `put`, `delete`, `usage`): the organization's secrets, with their labels, who wrote them
  and when, their reads per day and their last read; never a value. A last read's `run` and `namespace` are `null`
  when a toolset's registration read the secret to list a server's tools. `rewind.namespaces.secrets` (`list`, `put`,
  `delete`, `usage`): an end user's own. `isSecretName` checks a name.
- `runs.start`, `runs.fork` and `workflows.create` take `secrets`, the attachments by MCP server or API
  (`{ github: "ns/github", zendesk: "org/zendesk" }`); `Run` and `Workflow` have them. The problem slug
  `secret-required`, and `RewindApiError.attachments`: each attachment at fault and what may be attached instead.
- `toolsets.create` takes `apis` (OpenAPI documents, one tool per operation) and an `auth` per server and API.
- `rewind.signingKeys` (`list`, with the `issuer` and the `jwksUrl`; `rotate`; `jwks`), `rewindIssuer` and
  `rewindJwksUrl`.
- `verifyRewindCall(request, { issuer, audience, labels?, session? })` checks a call Rewind signed (`auth: { type:
"rewind" }`) and answers `{ namespace, labels, run, tool, idempotencyKey }`, or throws a `RewindSignatureError`
  naming the check that failed. It depends on `jose`.
- The types `Secret`, `SecretUsage`, `PutSecret`, `SecretScope`, `LastRead`, `DailyReads`, `WorkflowAttaching`,
  `SigningKey`, `SigningKeyList`, `Jwk`, `Jwks`, `AttachmentProblem`, `AttachmentProblemKind`, `Api`, `ApiSummary`,
  `Auth`, `SecretEncoding`, `SkippedOperation`, `CredentialsExpiredReason`.

Breaking changes:

- `StartRunParams.credentials` is `secrets`, which names secrets rather than giving values;
  `toolsets.setCredentials`, `runs.setCredentials` and `namespaces.setCredentials` are gone: put a secret
  (`secrets.put`, `namespaces.secrets.put`) and attach it.
- A parked call's reason is `credentials_refused` or `secret_missing` (was `credentials_expired`).

For workflow test suites: tests that describe the world a run meets and what the outside must see at the end, the
suite a workflow version carries and every derived version must pass, and tests recorded from past runs.

- `scripts.test({ script?, toolset?, tests?, workflow? })` runs tests against a script and answers a `TestRun`
  (`passed`, and each test's `TestResult`: its `problem`, `stale`, `incompatible` or `failed`, its `failures`, the
  run's status, output or error, its calls per tool and its trace). A test is given inline, by the digest of a stored
  test, or as `{ from, expect }`. With `workflow`, that version's suite runs too, and the script and the toolset
  default to the version's.
- `workflows.create` takes `parent`, `tests`, `update` (`{ name, reason, test?, expect? }`) and `retire`
  (`{ names, reason }`), and saves the version only if every test of its suite passes. `Workflow` has `parent`,
  `tests` (its suite, by name and digest), `retired` and `updated`, with their reasons.
- `workflows.record(name, { query?, runs?, limit? })` records tests from the workflow's completed and failed runs, one
  per path, and answers their digests, names, paths and runs, and the runs it skipped.
- `rewind.tests.get(digest)`: a stored test. A test recorded from a run is read like the run.
- `workflows.list({ script })`: every version of any workflow whose script it is, newest first, on one page.
- The problem slugs `invalid-test` and `tests-failed`; `RewindApiError.tests` holds each test's outcome of a
  `tests-failed` refusal, which `workflows.create` and `runs.fork` (of a run of a workflow) may answer.
- The types of tests, suites and recordings: `Test`, `TestInput`, `TestFrom`, `TestRun`, `TestResult`, `TestProblem`,
  `World`, `ToolRule`, `ToolRules`, `RuleError`, `TestSignal`, `Expectations`, `ExpectedCall`, `ExpectedError`,
  `CallOrder`, `TestOrigin`, `TestUpdate`, `TestRetirement`, `SuiteTest`, `RetiredTest`, `UpdatedTest`, `RecordTests`,
  `RecordedTests`, `RecordedTest`, `SkippedRun`.
- The agent tools `get_workflow({ name })` (the latest version's source and schemas, its suite with each test, and the
  lineage with the reasons tests were updated or retired) and `record_tests({ workflow, query?, runs?, limit? })` (the
  digests of the tests recorded, for `save_workflow`), given with a toolset, like `save_workflow`.
- `save_workflow` takes `tests`, `update`, `retire` and `parent`, and tells the model that its suite must pass, and how
  to retire or update a test with a reason. A refusal (`tests-failed`) shows each test that did not pass, its problem,
  its failures and its trace. `retry_run` shows the same when the suite of a run's workflow refuses the fix.
  `AgentToolResult.tests` holds the outcomes for the harness.

Breaking changes:

- `scripts.test` takes `{ script?, toolset?, tests?, workflow? }` and answers a `TestRun`; the scenario fields
  (`input`, `cassette`, `mocks`, `tools`, `signals`, `expect`) and `ScenarioSignal` are gone: they are a test's.
  `Expectations.calls` is a list of `{ tool, args?, times? }`, no longer a count by tool.
- The agent tool `test_script` takes `{ code, tests?, workflow? }` instead of `{ code, scenario }`; `scenario.from_run`
  is replaced by `record_tests`, whose digests `test_script` and `save_workflow` take.
- `AgentToolsClient` needs `tests`.

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
