import { toMillis } from "../duration.js";
import { NamespaceRequiredError, RewindApiError, RewindError, isProblem } from "../errors.js";
import { isValidIdempotencyKey } from "../http.js";
import { hasLabels, type RequestOptions } from "../params.js";
import type { components } from "../generated/api.js";
import type { ActionSets } from "../resources/action-sets.js";
import type { Payloads } from "../resources/admin.js";
import type { Runs } from "../resources/runs.js";
import type { Tests } from "../resources/tests.js";
import type { Workflows } from "../resources/workflows.js";
import type {
  ChangeTests,
  CreateVersion,
  Duration,
  Labels,
  PatchWorkflow,
  Run,
  RunStatus,
  RunWorkflow,
  SignalAccepted,
  TestInput,
  TestReplacement,
  TestReport,
  TestRequest,
  TestResult,
  TestRetirement,
  Workflow,
} from "../types.js";
import {
  anthropicTools,
  openaiChatTools,
  openaiResponsesTools,
  type AnthropicTool,
  type AnthropicToolResult,
  type AnthropicToolUse,
  type OpenAIChatTool,
  type OpenAIChatToolCall,
  type OpenAIChatToolMessage,
  type OpenAIFunctionCall,
  type OpenAIFunctionCallOutput,
  type OpenAIResponsesTool,
} from "./adapters.js";
import { OUTPUT_LIMIT, truncateEnd, truncateMiddle } from "./bound.js";
import {
  AGENT_TOOL_NAMES,
  NEEDS_ACTION_SET,
  definitionOf,
  isAgentToolName,
  type AgentToolDefinition,
  type AgentToolName,
} from "./definitions.js";
import {
  COMMENT_NOTE,
  args,
  describeCheck,
  describeGateRefused,
  describeMatches,
  describePage,
  describePatch,
  describeRecorded,
  describeRefusal,
  describeRefused,
  describeRetryRefused,
  describeRun,
  describeRuns,
  describeSecrets,
  describeTestResults,
  describeUpdate,
  describeWorkflowDetails,
  eventText,
  hasComment,
  json,
  limitsFor,
  quoted,
  runWorkflowReference,
  workflowLabel,
  workflowReference,
  type Limits,
  type ReadTest,
  type Updated,
} from "./render.js";

/**
 * The parts of the client the agent tools use. Its namespace is the agent's: take the client from
 * `rewind.namespace(user.id)`, or one holding the end user's token.
 */
export interface AgentToolsClient {
  runs: Runs;
  workflows: Workflows;
  actionSets: ActionSets;
  tests: Tests;
  payloads: Payloads;
  /** What `list_secrets` reads. Without it, `list_secrets` finds no secret. */
  secrets?: AgentSecrets | undefined;
}

/** A secret as the API lists it: its metadata, never its value. */
export type ListedSecret = components["schemas"]["Secret"];

/** What `list_secrets` reads: the secrets the agent may use. */
export interface AgentSecrets {
  /** The organization's secrets the caller sees, never a value: `GET /v1/secrets`. */
  list(options?: RequestOptions): Promise<ListedSecret[]>;
  /**
   * The secrets of the namespace the client acts for, never a value: `GET /v1/namespaces/{name}/secrets`.
   * `undefined` when the client names no namespace.
   */
  ofNamespace?(options?: RequestOptions): Promise<ListedSecret[] | undefined>;
}

export interface AgentToolsOptions {
  /**
   * The action set the model's code imports from `rewind:actions`: its runs, checks and tests, and the versions
   * `update_workflow` makes of new code, use it. `search_actions` needs one. A run of a workflow named by its id or
   * its name uses its version's action set.
   */
  actionSet?: string | undefined;
  /**
   * Set on every run the model starts. They scope `list_runs`, and the tools that take a run id
   * refuse a run that does not carry them. An API key or an end user's token limited to these
   * labels makes the scope hold on the server too.
   */
  labels?: Labels | undefined;
  /** The tools to give the model. All of them by default; `search_actions` only with an action set. */
  tools?: readonly AgentToolName[] | undefined;
  /** How long `run_workflow`, `retry_run` and `cancel_run` wait for the run to end. 30 s by default. */
  wait?: Duration | undefined;
  /** The longest wait the model may ask for. 2 minutes by default. */
  maxWait?: Duration | undefined;
  /** The time to live of the runs the model starts. The server's default (30 days) otherwise. */
  ttl?: Duration | undefined;
  /** W3C trace context of the LLM call that wrote the code, or a function giving the current one. */
  traceparent?: string | (() => string | undefined) | undefined;
  tracestate?: string | (() => string | undefined) | undefined;
  /** The most of a run's output one answer holds, in bytes. 256 KiB by default. */
  outputLimit?: number | undefined;
}

/** Per call: the model's tool call id, used as the idempotency key, and the call's trace context. */
export interface HandleOptions {
  /**
   * The model's tool call id. It becomes the `Idempotency-Key` of `run_workflow`, `retry_run` and
   * `signal_run`, so handling the same tool call again returns the same run or delivers one signal.
   */
  toolCallId?: string | undefined;
  traceparent?: string | undefined;
  tracestate?: string | undefined;
  signal?: AbortSignal | undefined;
}

/** What the checks before anything runs said of a code: `passed`, or `refused` with its diagnostics. */
export type CodeVerdict = "passed" | "refused";

/** What a tool call returns to the model, and to the harness. */
export interface AgentToolResult {
  /** The text for the model, bounded. */
  content: string;
  /** The call failed, or what it ran failed: the model should look at it. */
  isError: boolean;
  /** The run the call started or read, for the harness (pending approvals, for example). */
  run?: Run | undefined;
  /** The workflow `update_workflow` changed, or `get_workflow` read. */
  workflow?: Workflow | undefined;
  /**
   * The outcome of each test: those `test_workflow` ran, or those of the suite that refused a version or a change of
   * tests (`update_workflow`) or a fork (`retry_run`).
   */
  tests?: TestResult[] | undefined;
  /**
   * For a call that hands in a code (`code` of `run_workflow`, `check_workflow`, `test_workflow`, `retry_run`,
   * `update_workflow`): whether the checks before anything runs passed it. Absent when the call carried no code, or
   * failed before the checks (a bad argument, another refusal): what a harness measures as the first-try check rate.
   */
  code?: CodeVerdict | undefined;
}

/** A mistake in the arguments: `{tool}: {message}`. */
class InputError extends Error {}

/** A refusal the handler wrote for the model, beside the checks of a code: a result with `isError`, without a verdict. */
class Refused extends Error {}

/** What a run the tools start runs: a code, or a workflow named by a reference. */
type Started = { code: string } | { workflow: string };

/**
 * The agent tools: ready-made tool definitions for the model, and their handlers. Pick the
 * definitions in the format of the model API (`definitions`, `anthropic`, `openai`), and hand
 * each tool call to `handle`.
 */
export class AgentTools {
  /** The tools, independent of any model API. */
  readonly definitions: AgentToolDefinition[];
  /** The tools and their handling in the format of the Anthropic Messages API. */
  readonly anthropic: {
    readonly tools: AnthropicTool[];
    handle(toolUse: AnthropicToolUse, options?: Omit<HandleOptions, "toolCallId">): Promise<AnthropicToolResult>;
  };
  /** The tools and their handling in the formats of the OpenAI Chat Completions and Responses APIs. */
  readonly openai: {
    readonly tools: OpenAIChatTool[];
    handle(call: OpenAIChatToolCall, options?: Omit<HandleOptions, "toolCallId">): Promise<OpenAIChatToolMessage>;
    readonly responses: {
      readonly tools: OpenAIResponsesTool[];
      handle(call: OpenAIFunctionCall, options?: Omit<HandleOptions, "toolCallId">): Promise<OpenAIFunctionCallOutput>;
    };
  };

  private readonly client: AgentToolsClient;
  private readonly options: AgentToolsOptions;
  private readonly names: ReadonlySet<AgentToolName>;
  private readonly limits: Limits;
  private readonly defaultWaitMs: number;
  private readonly maxWaitMs: number;

  constructor(client: AgentToolsClient, options: AgentToolsOptions = {}) {
    this.client = client;
    this.options = options;
    const hasActionSet = options.actionSet !== undefined;
    const names = options.tools ?? AGENT_TOOL_NAMES.filter((name) => hasActionSet || !NEEDS_ACTION_SET.has(name));
    for (const name of names) {
      if (!isAgentToolName(name)) {
        throw new TypeError(
          `unknown agent tool ${JSON.stringify(name)}: expected one of ${AGENT_TOOL_NAMES.join(", ")}`,
        );
      }
    }
    for (const name of names) {
      if (!hasActionSet && NEEDS_ACTION_SET.has(name)) {
        throw new TypeError(`${name} needs an action set`);
      }
    }
    this.names = new Set(names);
    this.limits = limitsFor(options.outputLimit ?? OUTPUT_LIMIT);
    this.maxWaitMs = toMillis(options.maxWait ?? "2m");
    this.defaultWaitMs = Math.min(toMillis(options.wait ?? "30s"), this.maxWaitMs);
    this.definitions = [...this.names].map(definitionOf);

    this.anthropic = {
      tools: anthropicTools(this.definitions),
      handle: async (toolUse, handleOptions = {}) => {
        const result = await this.handle(toolUse.name, toolUse.input, { ...handleOptions, toolCallId: toolUse.id });
        return {
          type: "tool_result",
          tool_use_id: toolUse.id,
          content: result.content,
          ...(result.isError ? { is_error: true } : {}),
        };
      },
    };
    this.openai = {
      tools: openaiChatTools(this.definitions),
      handle: async (call, handleOptions = {}) => {
        const result = await this.handle(call.function.name, call.function.arguments, {
          ...handleOptions,
          toolCallId: call.id,
        });
        return { role: "tool", tool_call_id: call.id, content: result.content };
      },
      responses: {
        tools: openaiResponsesTools(this.definitions),
        handle: async (call, handleOptions = {}) => {
          const result = await this.handle(call.name, call.arguments, { ...handleOptions, toolCallId: call.call_id });
          return { type: "function_call_output", call_id: call.call_id, output: result.content };
        },
      },
    };
  }

  /** Whether `name` is one of these tools. */
  has(name: string): boolean {
    return isAgentToolName(name) && this.names.has(name);
  }

  /**
   * Executes a tool call of the model. `input` is the call's arguments, as an object or as a
   * JSON string. Mistakes the model can fix (a bad argument, refused code, an unknown run)
   * come back as a result with `isError`; a fault of the platform or of the configuration (an
   * unreachable server, a 401, a 5xx, a key's client that names no namespace) is thrown.
   *
   * @throws {RewindError} when `name` is not one of these tools, or the platform failed.
   */
  async handle(name: string, input: unknown, options: HandleOptions = {}): Promise<AgentToolResult> {
    if (!isAgentToolName(name) || !this.names.has(name)) {
      throw new RewindError(`${JSON.stringify(name)} is not one of these agent tools`);
    }
    let result: AgentToolResult;
    let carried = false;
    try {
      const values = parseInput(name, input);
      carried = carriesCode(name, values);
      result = await this.dispatch(name, values, options);
      if (carried && result.code === undefined) {
        result = { ...result, code: "passed" };
      }
    } catch (error) {
      result = failure(name, error, this.limits);
    }
    // Each part is bounded; this bounds what a code, a server or an action wrote unbounded.
    return { ...result, content: truncateMiddle(result.content, this.limits.total) };
  }

  private dispatch(name: AgentToolName, values: Values, options: HandleOptions): Promise<AgentToolResult> {
    switch (name) {
      case "run_workflow":
        return this.runWorkflow(values, options);
      case "check_workflow":
        return this.checkWorkflow(values, options);
      case "test_workflow":
        return this.testWorkflow(values, options);
      case "get_run":
        return this.getRun(values, options);
      case "list_runs":
        return this.listRuns(values, options);
      case "signal_run":
        return this.signalRun(values, options);
      case "cancel_run":
        return this.cancelRun(values, options);
      case "retry_run":
        return this.retryRun(values, options);
      case "search_actions":
        return this.searchActions(values, options);
      case "list_secrets":
        return this.listSecrets(options);
      case "update_workflow":
        return this.updateWorkflow(values, options);
      case "get_workflow":
        return this.getWorkflow(values, options);
      case "record_tests":
        return this.recordTests(values, options);
    }
  }

  /** `update_workflow` is one of the tools: the results point to it. */
  private get updates(): boolean {
    return this.names.has("update_workflow");
  }

  /** Runs code, or a workflow by its id or its name. */
  private async runWorkflow(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const code = optionalString(input, "code");
    const workflow = optionalString(input, "workflow");
    const wait = this.waitOf(input["wait"]);
    const secrets = optionalSecrets(input);
    if (code !== undefined && workflow !== undefined) {
      throw new InputError(
        "give `code` or `workflow`, not both: `code` runs new code, `workflow` an existing workflow by its id or its name.",
      );
    }
    if (code === undefined && workflow === undefined) {
      throw new InputError("give the `code` to run, or a `workflow` by its id or its name.");
    }
    if (workflow !== undefined && secrets !== undefined && Object.keys(secrets).length > 0) {
      throw new InputError("`secrets` goes with `code`: a run of a workflow uses the secrets its version names.");
    }
    const idempotencyKey = await runIdempotencyKey(this.options.labels, options.toolCallId);
    const common = {
      input: Object.hasOwn(input, "input") ? input["input"] : {},
      wait,
      idempotencyKey,
      labels: this.options.labels,
      ttl: this.options.ttl,
      ...this.traceContext(options),
      signal: options.signal,
    };
    const run =
      code !== undefined
        ? await this.client.runs.start({
            code,
            actionSet: this.options.actionSet,
            ...optionalField("secrets", secrets),
            ...common,
          })
        : await this.client.runs.start({ workflow: workflow as string, ...common });
    await this.checkAnswered(
      run,
      code !== undefined ? { code } : { workflow: workflow as string },
      idempotencyKey,
      options.toolCallId,
    );
    return this.report(run, { errorWhenFailed: true, transcript: run.status === "failed", signal: options.signal });
  }

  /** Diagnostics, the workflow that already has the code, and a note for a code without a comment at its top. */
  private async checkWorkflow(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const code = requireString(input, "code");
    const result = await this.client.workflows.check({
      code,
      actionSet: this.options.actionSet,
      signal: options.signal,
    });
    const checked = describeCheck(result, this.limits);
    const content = hasComment(code) ? checked : `${checked}\n${COMMENT_NOTE}`;
    return { content, isError: !result.ok, code: result.ok ? "passed" : "refused" };
  }

  /** Tests against code, or against a workflow's version; the workflow's suite too. */
  private async testWorkflow(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const code = optionalString(input, "code");
    const tests = optionalTests(input, "tests");
    const workflow = optionalString(input, "workflow");
    if (code === undefined && workflow === undefined) {
      throw new InputError("give the `code` to test, or a `workflow` whose version the tests run against.");
    }
    if (tests === undefined && workflow === undefined) {
      throw new InputError("give `tests`, or a `workflow` whose suite runs against the code, or both.");
    }
    const request: TestRequest = {};
    if (code !== undefined) {
      request.code = code;
      // A workflow's version runs with its own action set.
      if (this.options.actionSet !== undefined) {
        request.actionSet = this.options.actionSet;
      }
    }
    if (tests !== undefined && tests.length > 0) {
      request.tests = tests;
    }
    if (workflow !== undefined) {
      request.workflow = workflow;
    }
    let report: TestReport;
    try {
      report = await this.client.workflows.test(request, { signal: options.signal });
    } catch (error) {
      if (isProblem(error, "invalid-test")) {
        return {
          content: `The tests were refused before running: ${truncateEnd(error.detail, this.limits.value)}`,
          isError: true,
        };
      }
      throw error;
    }
    return { content: describeTestResults(report.tests, this.limits), isError: !report.passed, tests: report.tests };
  }

  private async getRun(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const runId = requireString(input, "run_id");
    const event = optionalIndex(input, "event");
    const offset = optionalIndex(input, "offset");
    const wait = input["wait"] === undefined ? undefined : this.waitOf(input["wait"]);
    const { signal } = options;
    let run = await this.scopedRun(runId, signal);
    if (wait !== undefined && event === undefined) {
      run = await this.client.runs.get(runId, { wait, signal });
    }
    // The run is named by its handle from here on, whatever the model gave.
    const { handle } = run;
    if (event !== undefined) {
      const text = eventText(await this.client.runs.event(runId, event, { signal }));
      const content = describePage(
        `Event ${event} of run ${handle}`,
        text,
        offset ?? 0,
        this.limits.output,
        (next) => `get_run(${args({ run_id: handle, event, offset: next })})`,
      );
      return { content, isError: false, run };
    }
    if (offset !== undefined) {
      if (run.status !== "completed") {
        throw new InputError(`run ${handle} has no output to page through: it is ${run.status}.`);
      }
      const content = describePage(
        `Output of run ${handle}`,
        await this.outputOf(run, signal),
        offset,
        this.limits.output,
        (next) => `get_run(${args({ run_id: handle, offset: next })})`,
      );
      return { content, isError: false, run };
    }
    return this.report(run, { errorWhenFailed: false, transcript: true, signal });
  }

  private async listRuns(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const status = input["status"] ?? undefined;
    if (status !== undefined && !isStatus(status)) {
      throw new InputError("`status` must be one of pending, active, completed, failed, cancelled, aborted.");
    }
    const page = await this.client.runs.list({
      labels: this.options.labels,
      status,
      limit: 50,
      signal: options.signal,
    });
    return { content: describeRuns(page.runs, Boolean(page.nextCursor), status), isError: false };
  }

  private async signalRun(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const runId = requireString(input, "run_id");
    const name = requireString(input, "name");
    const run = await this.scopedRun(runId, options.signal);
    let accepted: SignalAccepted;
    try {
      accepted = await this.client.runs.signal(runId, name, input["payload"] ?? null, {
        idempotencyKey: await idempotencyKeyOf(options.toolCallId),
        signal: options.signal,
      });
    } catch (error) {
      if (isProblem(error, "invalid-signal")) {
        const detail = truncateEnd(error.detail, this.limits.value);
        return { content: `Run ${run.handle} refused the signal ${quoted(name)}: ${detail}`, isError: true, run };
      }
      throw error;
    }
    const { handle } = run;
    const content = accepted.duplicate
      ? `This signal ${quoted(name)} was already sent to run ${handle}: nothing was added.`
      : `Sent the signal ${quoted(name)} to run ${handle}. Read what the run does next with get_run(${args({ run_id: handle, wait: "30s" })}).`;
    return { content, isError: false, run };
  }

  private async cancelRun(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const runId = requireString(input, "run_id");
    await this.scopedRun(runId, options.signal);
    const run = await this.client.runs.cancel(runId, { wait: this.defaultWaitMs, signal: options.signal });
    return this.report(run, { errorWhenFailed: false, transcript: false, signal: options.signal });
  }

  private async retryRun(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const runId = requireString(input, "run_id");
    const code = requireString(input, "code");
    const secrets = optionalSecrets(input);
    const source = await this.scopedRun(runId, options.signal);
    const idempotencyKey = await runIdempotencyKey(this.options.labels, options.toolCallId);
    const labels = this.options.labels;
    let run: Run;
    try {
      run = await this.client.runs.fork(runId, {
        code,
        ...optionalField("labels", labels !== undefined && Object.keys(labels).length > 0 ? labels : undefined),
        wait: this.defaultWaitMs,
        idempotencyKey,
        ttl: this.options.ttl,
        ...optionalField("secrets", secrets),
        ...this.traceContext(options),
        signal: options.signal,
      });
    } catch (error) {
      if (isProblem(error, "tests-failed")) {
        const workflow = source.workflow ? runWorkflowReference(source.workflow) : undefined;
        return {
          content: describeRetryRefused(source.handle, error.detail, error.tests, workflow, this.updates, this.limits),
          isError: true,
          run: source,
          tests: error.tests,
        };
      }
      throw error;
    }
    await this.checkAnswered(run, { code }, idempotencyKey, options.toolCallId);
    return this.report(run, { errorWhenFailed: true, transcript: run.status === "failed", signal: options.signal });
  }

  /**
   * `list_secrets`: the organization's secrets the caller sees, and those of the namespace the client acts for. A list
   * the caller may not read (403) is left out: its key may use none of them.
   */
  private async listSecrets(options: HandleOptions): Promise<AgentToolResult> {
    const secrets = this.client.secrets;
    const readable = async (listing: () => Promise<ListedSecret[] | undefined>) => {
      try {
        return await listing();
      } catch (error) {
        if (error instanceof RewindApiError && error.status === 403) {
          return [];
        }
        throw error;
      }
    };
    const organization = secrets ? ((await readable(() => secrets.list({ signal: options.signal }))) ?? []) : [];
    const ofNamespace = secrets?.ofNamespace
      ? await readable(() => secrets.ofNamespace!({ signal: options.signal }))
      : undefined;
    return { content: describeSecrets(organization, ofNamespace), isError: false };
  }

  private async searchActions(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const query = requireString(input, "query");
    const actionSet = this.options.actionSet;
    if (actionSet === undefined) {
      throw new RewindError("search_actions needs an action set");
    }
    const results = await this.client.actionSets.search(actionSet, query, { signal: options.signal });
    return { content: describeMatches(query, results.actions), isError: false };
  }

  /**
   * Changes a workflow: its name and description (a merge patch), then its next version (code, interface, secrets,
   * with tests through its gate) or its suite alone.
   */
  private async updateWorkflow(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const { signal } = options;
    const reference = requireString(input, "workflow");
    const patch: PatchWorkflow = {
      ...optionalField("name", optionalPatch(input, "name")),
      ...optionalField("description", optionalPatch(input, "description")),
    };
    const code = optionalString(input, "code");
    const schemaInput = optionalSchema(input, "input");
    const schemaOutput = optionalSchema(input, "output");
    const signals = optionalSignals(input);
    const secrets = optionalSecrets(input);
    const tests = optionalTests(input, "tests") ?? [];
    const replace = optionalReplacements(input) ?? [];
    const retire = optionalRetirements(input) ?? [];
    const patched = Object.keys(patch).length > 0;
    const interfaced = schemaInput !== undefined || schemaOutput !== undefined || signals !== undefined;
    const versioned = code !== undefined || interfaced || secrets !== undefined;
    const tested = tests.length > 0 || replace.length > 0 || retire.length > 0;
    if (!patched && !versioned && !tested) {
      throw new InputError(
        "give what changes: a `name` or a `description`; `code`, an interface (`input`, `output`, `signals`) or `secrets` for a new version; or `tests`, `replace` and `retire` for its suite.",
      );
    }
    // The workflow, by its id from here on: a new name does not lose it.
    const before = patched
      ? await this.client.workflows.update(reference, patch, { signal })
      : await this.client.workflows.get(reference, { signal });
    if (!versioned && !tested) {
      return {
        content: describeUpdate(before, patch, { kind: "patched" }, this.limits),
        isError: false,
        workflow: before,
      };
    }
    const latest = before.versions[0] ?? before.version;
    const latestNumber = latest.version;
    // What changes in the suite: the empty lists are left out.
    const suite = {
      ...optionalField("tests", tests.length === 0 ? undefined : tests),
      ...optionalField("replace", replace.length === 0 ? undefined : replace),
      ...optionalField("retire", retire.length === 0 ? undefined : retire),
    };
    let after: Workflow;
    try {
      if (versioned) {
        // Each part of the interface left out keeps its latest version's.
        const version = {
          // New code imports the actions of the tools' action set.
          ...optionalField("actionSet", code === undefined ? undefined : this.options.actionSet),
          ...optionalField("code", code),
          ...(interfaced
            ? {
                ...optionalField("input", schemaInput ?? latest.input ?? undefined),
                ...optionalField("output", schemaOutput ?? latest.output ?? undefined),
                ...optionalField("signals", signals ?? latest.signals ?? undefined),
              }
            : {}),
          ...optionalField("secrets", secrets),
          ...suite,
          // A schema may be `true`, which takes anything.
        } as CreateVersion;
        after = await this.client.workflows.createVersion(before.id, version, { signal });
      } else {
        const change: ChangeTests = suite;
        after = await this.client.workflows.addTests(before.id, change, { signal });
      }
    } catch (error) {
      if (!(error instanceof RewindApiError) || !isRefusal(error)) {
        throw error;
      }
      // What the patch changed stays changed: the model reads it first.
      const patchedText = describePatch(patch);
      const done =
        patchedText === undefined ? undefined : `Updated the workflow ${workflowLabel(before)}: ${patchedText}.`;
      const failed = error.slug === "tests-failed";
      const refusedCode = error.slug === "invalid-code";
      const refused = failed
        ? describeGateRefused(
            error.detail,
            error.tests,
            workflowReference(before),
            versioned,
            latestNumber,
            this.limits,
          )
        : describeRefused(
            refusedCode
              ? "No version was made: the code was refused before anything ran"
              : versioned
                ? "No version was made"
                : "The suite was not changed",
            error.detail,
            error.diagnostics,
            this.limits,
          );
      const content = done === undefined ? refused : `${done}\n${refused}`;
      if (failed || refusedCode) {
        return {
          content,
          isError: true,
          workflow: before,
          ...optionalField("tests", failed ? error.tests : undefined),
          ...optionalField("code", refusedCode ? ("refused" as const) : undefined),
        };
      }
      throw new Refused(content);
    }
    const updated: Updated = versioned
      ? { kind: "version", made: after.version.version !== latestNumber }
      : { kind: "tests" };
    return { content: describeUpdate(after, patch, updated, this.limits), isError: false, workflow: after };
  }

  /**
   * A workflow for the model to repair or extend: who it is, its version's code and interface, its suite with each test
   * (a test it may not read is said so), the tests that left it, and its versions.
   */
  private async getWorkflow(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const reference = requireString(input, "workflow");
    const { signal } = options;
    const workflow = await this.client.workflows.get(reference, { signal });
    const suite = workflow.suite.filter((entry) => !entry.retiredAt);
    const tests = await mapLimited(suite, 8, async ({ name, digest }): Promise<ReadTest> => {
      try {
        return { name, digest, test: await this.client.tests.get(digest, { signal }) };
      } catch (error) {
        if (error instanceof RewindApiError && (error.status === 403 || error.status === 404)) {
          return { name, digest, unreadable: error.detail };
        }
        throw error;
      }
    });
    return {
      content: describeWorkflowDetails({ workflow, tests, updates: this.updates }, this.limits),
      isError: false,
      workflow,
    };
  }

  /** Tests recorded from the workflow's past runs. */
  private async recordTests(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const workflow = requireString(input, "workflow");
    const query = optionalString(input, "query");
    const runs = optionalStrings(input, "runs");
    const limit = input["limit"];
    if (limit !== undefined && limit !== null && (typeof limit !== "number" || !Number.isInteger(limit) || limit < 1)) {
      throw new InputError("`limit` must be an integer, 1 or more.");
    }
    const recorded = await this.client.workflows.record(workflow, {
      query,
      runs,
      limit: typeof limit === "number" ? limit : undefined,
      signal: options.signal,
    });
    return { content: describeRecorded(workflow, recorded, this.updates, this.limits), isError: false };
  }

  /** A run's report: its output when it completed, its transcript when asked. */
  private async report(
    run: Run,
    options: { errorWhenFailed: boolean; transcript: boolean; signal: AbortSignal | undefined },
  ): Promise<AgentToolResult> {
    const output = run.status === "completed" ? await this.outputOf(run, options.signal) : undefined;
    const transcript = options.transcript
      ? (await this.client.runs.transcript(run.id, { signal: options.signal })).text
      : undefined;
    const ended = run.status === "failed" || run.status === "cancelled" || run.status === "aborted";
    return {
      content: describeRun(run, { output, transcript }, this.limits),
      isError: options.errorWhenFailed && ended,
      run,
    };
  }

  /** The run's output as compact JSON, read from its payload when it is too large to be inline. */
  private async outputOf(run: Run, signal: AbortSignal | undefined): Promise<string> {
    const payload = run.output;
    if (payload === null || payload === undefined) {
      return "null";
    }
    const value =
      "value" in payload ? payload.value : await this.client.payloads.get(payload.digest, { run: run.id, signal });
    return json(value);
  }

  /**
   * A key the server already knows returns the run it started, whoever started it. The key of a
   * run is scoped by the agent's labels, so this is a safety net: an agent without labels, or two
   * agents with the same labels whose tool call ids repeat, would get another conversation's run,
   * or the run of an earlier code. Such a run is the harness's fault, never shown to the model:
   * it must carry the agent's labels and be a run of this very code, or of the workflow named.
   */
  private async checkAnswered(
    run: Run,
    started: Started,
    idempotencyKey: string | undefined,
    toolCallId: string | undefined,
  ): Promise<void> {
    if (idempotencyKey === undefined) {
      return;
    }
    const same =
      "code" in started
        ? run.codeDigest === (await sha256Hex(started.code))
        : run.workflow !== undefined && run.workflow !== null && names(run.workflow, started.workflow);
    if (hasLabels(run.labels, this.options.labels) && same) {
      return;
    }
    throw new RewindError(
      `the tool call id ${JSON.stringify(toolCallId)} is already the idempotency key of another run (of another code or workflow, or with other labels): give each tool call a unique id`,
    );
  }

  /** The run, if it carries the agent's labels; otherwise the same answer as a run that does not exist. */
  private async scopedRun(runId: string, signal: AbortSignal | undefined): Promise<Run> {
    const run = await this.client.runs.get(runId, { signal });
    if (!hasLabels(run.labels, this.options.labels)) {
      throw new InputError(`Run ${runId} does not exist, or is not one of the runs you started.`);
    }
    return run;
  }

  /** The model's wait, bounded by `maxWait`, or the default wait. In milliseconds. */
  private waitOf(value: unknown): number {
    if (value === undefined || value === null) {
      return this.defaultWaitMs;
    }
    if (typeof value !== "string" && typeof value !== "number") {
      throw new InputError('`wait` must be a duration such as "30s".');
    }
    let ms: number;
    try {
      ms = toMillis(value);
    } catch (error) {
      throw new InputError(`\`wait\` ${(error as Error).message.replace(/^invalid duration /, "is not a duration: ")}`);
    }
    return Math.min(ms, this.maxWaitMs);
  }

  private traceContext(options: HandleOptions): { traceparent?: string; tracestate?: string } {
    const traceparent = options.traceparent ?? valueOf(this.options.traceparent);
    const tracestate = options.tracestate ?? valueOf(this.options.tracestate);
    return {
      ...(traceparent === undefined ? {} : { traceparent }),
      ...(tracestate === undefined ? {} : { tracestate }),
    };
  }
}

type Values = Record<string, unknown>;

/** The tools that take a code, given one: their result says whether the checks passed it. */
function carriesCode(name: AgentToolName, values: Values): boolean {
  return (
    (name === "run_workflow" ||
      name === "check_workflow" ||
      name === "test_workflow" ||
      name === "retry_run" ||
      name === "update_workflow") &&
    typeof values["code"] === "string" &&
    values["code"] !== ""
  );
}

/**
 * Whether a run's workflow is the one `reference` names: its id or its name, and the version after `@` (`v12`, or the
 * first digits of its digest) when it names one.
 */
function names(workflow: RunWorkflow, reference: string): boolean {
  const at = reference.indexOf("@");
  const base = at === -1 ? reference : reference.slice(0, at);
  const version = at === -1 ? undefined : reference.slice(at + 1);
  const same = workflow.id === base || workflow.name === base;
  if (!same || version === undefined) {
    return same;
  }
  const number = /^v(\d+)$/.exec(version)?.[1];
  return number !== undefined ? workflow.version === Number(number) : workflow.digest.startsWith(version);
}

function parseInput(name: AgentToolName, input: unknown): Values {
  let values = input;
  if (typeof input === "string") {
    try {
      values = input.trim() === "" ? {} : JSON.parse(input);
    } catch {
      throw new InputError(`the arguments of ${name} are not valid JSON.`);
    }
  }
  if (values === undefined || values === null) {
    return {};
  }
  if (!isObject(values)) {
    throw new InputError(`the arguments of ${name} must be an object.`);
  }
  return values;
}

/** A problem the model can fix: a 4xx but 401. */
function isRefusal(error: RewindApiError): boolean {
  return error.status !== 401 && error.status < 500;
}

function failure(name: AgentToolName, error: unknown, limits: Limits): AgentToolResult {
  if (error instanceof InputError) {
    return { content: `${name}: ${error.message}`, isError: true };
  }
  if (error instanceof Refused) {
    return { content: error.message, isError: true };
  }
  if (error instanceof NamespaceRequiredError) {
    // The harness's configuration: its client names no end user. The model cannot fix it.
    throw error;
  }
  if (error instanceof RewindApiError) {
    if (error.slug === "invalid-code") {
      return { content: describeRefusal(error.detail, error.diagnostics, limits), isError: true, code: "refused" };
    }
    if (isRefusal(error)) {
      return { content: `${name} failed: ${truncateEnd(error.detail, limits.value)}`, isError: true };
    }
  }
  throw error;
}

function requireString(values: Values, key: string): string {
  const value = values[key];
  if (typeof value !== "string" || value === "") {
    throw new InputError(`\`${key}\` must be a non-empty string.`);
  }
  return value;
}

function optionalString(values: Values, key: string): string | undefined {
  const value = values[key];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (typeof value !== "string" || value === "") {
    throw new InputError(`\`${key}\` must be a non-empty string.`);
  }
  return value;
}

/** A field of a JSON merge patch: absent leaves it, `null` removes it, a non-empty string gives it. */
function optionalPatch(values: Values, key: string): string | null | undefined {
  if (!Object.hasOwn(values, key)) {
    return undefined;
  }
  const value = values[key];
  if (value === null) {
    return null;
  }
  if (typeof value !== "string" || value === "") {
    throw new InputError(`\`${key}\` must be a non-empty string, or null to remove it.`);
  }
  return value;
}

/** A JSON Schema: an object, or a boolean (`true` takes anything). */
function isSchema(value: unknown): value is Record<string, unknown> | boolean {
  return isObject(value) || typeof value === "boolean";
}

function optionalSchema(values: Values, key: string): Record<string, unknown> | boolean | undefined {
  const value = values[key];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (!isSchema(value)) {
    throw new InputError(`\`${key}\` must be a JSON Schema: an object.`);
  }
  return value;
}

/** `signals`: the JSON Schema of each signal's payload, by its name. */
function optionalSignals(values: Values): Record<string, unknown> | undefined {
  const value = values["signals"];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (!isObject(value) || !Object.values(value).every(isSchema)) {
    throw new InputError("`signals` must be an object of JSON Schemas by signal name.");
  }
  return value;
}

/** `secrets`: the secrets a run reads, by MCP server or API, each `org/<name>` or `ns/<name>`. The API checks them. */
function optionalSecrets(values: Values): Record<string, string> | undefined {
  const value = values["secrets"];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (!isObject(value) || !Object.values(value).every((secret) => typeof secret === "string" && secret !== "")) {
    throw new InputError(
      '`secrets` must be an object of secrets by MCP server or API, each "org/<name>" or "ns/<name>", such as {"github": "ns/github"}.',
    );
  }
  return value as Record<string, string>;
}

function optionalStrings(values: Values, key: string): string[] | undefined {
  const value = values[key];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (!Array.isArray(value) || !value.every((item) => typeof item === "string" && item !== "")) {
    throw new InputError(`\`${key}\` must be an array of non-empty strings.`);
  }
  return value as string[];
}

/** Tests as the model gave them: each an object (in full, or `{ test, expect }`) or a digest. The server checks them. */
function optionalTests(values: Values, key: string): TestInput[] | undefined {
  const value = values[key];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (!Array.isArray(value) || !value.every(isTestInput)) {
    throw new InputError(
      `\`${key}\` must be an array of tests: each one an object, or the digest of a recorded test as a string.`,
    );
  }
  return value as TestInput[];
}

function isTestInput(value: unknown): boolean {
  return (typeof value === "string" && value !== "") || isObject(value);
}

function optionalReplacements(values: Values): TestReplacement[] | undefined {
  const value = values["replace"];
  if (value === undefined || value === null) {
    return undefined;
  }
  const valid =
    Array.isArray(value) &&
    value.every(
      (item) =>
        isObject(item) &&
        typeof item["name"] === "string" &&
        typeof item["reason"] === "string" &&
        item["reason"].trim() !== "" &&
        (item["new"] === undefined || isTestInput(item["new"])) &&
        (item["expect"] === undefined || isObject(item["expect"])),
    );
  if (!valid) {
    throw new InputError(
      "`replace` must be an array of { name, reason, new?, expect? }: the name of a test of the suite, why it changes, and the test that replaces it or its new expectation.",
    );
  }
  return value as TestReplacement[];
}

function optionalRetirements(values: Values): TestRetirement[] | undefined {
  const value = values["retire"];
  if (value === undefined || value === null) {
    return undefined;
  }
  const valid =
    Array.isArray(value) &&
    value.every(
      (item) =>
        isObject(item) &&
        Array.isArray(item["names"]) &&
        item["names"].every((name) => typeof name === "string") &&
        typeof item["reason"] === "string" &&
        item["reason"].trim() !== "",
    );
  if (!valid) {
    throw new InputError(
      "`retire` must be an array of { names, reason }: the names of tests of the suite, and why they no longer hold.",
    );
  }
  return value as TestRetirement[];
}

/** `{ [key]: value }`, or nothing when the value is absent. */
function optionalField<K extends string, V>(key: K, value: V | undefined): Partial<Record<K, V>> {
  return value === undefined ? {} : ({ [key]: value } as Record<K, V>);
}

/** `items` mapped by `map`, at most `limit` at a time, in their order. */
async function mapLimited<T, R>(items: readonly T[], limit: number, map: (item: T) => Promise<R>): Promise<R[]> {
  const results: R[] = new Array(items.length);
  let next = 0;
  const worker = async () => {
    while (next < items.length) {
      const index = next;
      next += 1;
      results[index] = await map(items[index] as T);
    }
  };
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return results;
}

function optionalIndex(values: Values, key: string): number | undefined {
  const value = values[key];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
    throw new InputError(`\`${key}\` must be an integer, 0 or more.`);
  }
  return value;
}

function isObject(value: unknown): value is Values {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isStatus(value: unknown): value is RunStatus {
  return (
    typeof value === "string" && ["pending", "active", "completed", "failed", "cancelled", "aborted"].includes(value)
  );
}

function valueOf(source: string | (() => string | undefined) | undefined): string | undefined {
  return typeof source === "function" ? source() : source;
}

/**
 * The idempotency key of a tool call: its id when the server takes it as one, otherwise the
 * SHA-256 of the id, so the same tool call always gives the same key. `signal_run` uses it: a
 * signal's key is unique per run.
 */
export async function idempotencyKeyOf(toolCallId: string | undefined): Promise<string | undefined> {
  if (toolCallId === undefined || toolCallId === "") {
    return undefined;
  }
  return isValidIdempotencyKey(toolCallId) ? toolCallId : sha256Hex(toolCallId);
}

/**
 * The idempotency key of the run a tool call starts (`run_workflow`, `retry_run`): the SHA-256 of
 * `JSON.stringify([labels, toolCallId])`, the labels as `[key, value]` pairs sorted by key (by
 * code point). A run's key is unique per organization, and harnesses number their tool calls alike
 * (`call_0`, `call_1`…): agents with other labels never share a key. The same tool call of the
 * same agent always gives the same key.
 */
export async function runIdempotencyKey(
  labels: Labels | undefined,
  toolCallId: string | undefined,
): Promise<string | undefined> {
  if (toolCallId === undefined || toolCallId === "") {
    return undefined;
  }
  const scope = Object.entries(labels ?? {}).sort(([a], [b]) => byCodePoint(a, b));
  return sha256Hex(JSON.stringify([scope, toolCallId]));
}

/** Compares two texts by code point, as Rust compares its strings. */
function byCodePoint(a: string, b: string): number {
  const left = Array.from(a);
  const right = Array.from(b);
  for (let index = 0; index < Math.min(left.length, right.length); index += 1) {
    const difference = (left[index]?.codePointAt(0) ?? 0) - (right[index]?.codePointAt(0) ?? 0);
    if (difference !== 0) {
      return difference;
    }
  }
  return left.length - right.length;
}

/** The SHA-256 of a text's UTF-8, in hexadecimal: a code's digest, as the server computes it. */
async function sha256Hex(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
