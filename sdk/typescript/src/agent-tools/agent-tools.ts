import { toMillis } from "../duration.js";
import { NamespaceRequiredError, RewindApiError, RewindError } from "../errors.js";
import { isValidIdempotencyKey } from "../http.js";
import { hasLabels } from "../params.js";
import type { Payloads } from "../resources/admin.js";
import type { Runs } from "../resources/runs.js";
import type { Scripts } from "../resources/scripts.js";
import type { Toolsets } from "../resources/toolsets.js";
import type { Duration, Labels, Run, RunStatus, ScenarioSignal, TestRequest } from "../types.js";
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
  definitionOf,
  isAgentToolName,
  type AgentToolDefinition,
  type AgentToolName,
} from "./definitions.js";
import {
  args,
  describeCheck,
  describeMatches,
  describePage,
  describeRefusal,
  describeRun,
  describeRuns,
  describeTest,
  entryText,
  json,
  limitsFor,
  type Limits,
} from "./render.js";

/**
 * The parts of the client the agent tools use. Its namespace is the agent's: take the client from
 * `rewind.namespace(user.id)`, or one holding the end user's token.
 */
export interface AgentToolsClient {
  runs: Runs;
  scripts: Scripts;
  toolsets: Toolsets;
  payloads: Payloads;
}

export interface AgentToolsOptions {
  /** The toolset the model's scripts import from `rewind:tools`. `search_tools` needs one. */
  toolset?: string | undefined;
  /**
   * Set on every run the model starts. They scope `list_runs`, and the tools that take a run id
   * refuse a run that does not carry them. An API key or an end user's token limited to these
   * labels makes the scope hold on the server too.
   */
  labels?: Labels | undefined;
  /** The tools to give the model. All of them by default; `search_tools` only with a toolset. */
  tools?: readonly AgentToolName[] | undefined;
  /** How long `run_script`, `retry_run` and `cancel_run` wait for the run to end. 30 s by default. */
  wait?: Duration | undefined;
  /** The longest wait the model may ask for. 2 minutes by default. */
  maxWait?: Duration | undefined;
  /** The time to live of the runs the model starts. The server's default (30 days) otherwise. */
  ttl?: Duration | undefined;
  /** W3C trace context of the LLM call that wrote the script, or a function giving the current one. */
  traceparent?: string | (() => string | undefined) | undefined;
  tracestate?: string | (() => string | undefined) | undefined;
  /** The most of a run's output one answer holds, in bytes. 256 KiB by default. */
  outputLimit?: number | undefined;
}

/** Per call: the model's tool call id, used as the idempotency key, and the call's trace context. */
export interface HandleOptions {
  /**
   * The model's tool call id. It becomes the `Idempotency-Key` of `run_script`, `retry_run` and
   * `signal_run`, so handling the same tool call again returns the same run or delivers one signal.
   */
  toolCallId?: string | undefined;
  traceparent?: string | undefined;
  tracestate?: string | undefined;
  signal?: AbortSignal | undefined;
}

/** What a tool call returns to the model. */
export interface AgentToolResult {
  /** The text for the model, bounded. */
  content: string;
  /** The call failed, or what it ran failed: the model should look at it. */
  isError: boolean;
  /** The run the call started or read, for the harness (pending approvals, for example). */
  run?: Run | undefined;
}

class InputError extends Error {}

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
    const names =
      options.tools ?? AGENT_TOOL_NAMES.filter((name) => name !== "search_tools" || options.toolset !== undefined);
    for (const name of names) {
      if (!isAgentToolName(name)) {
        throw new TypeError(
          `unknown agent tool ${JSON.stringify(name)}: expected one of ${AGENT_TOOL_NAMES.join(", ")}`,
        );
      }
    }
    if (names.includes("search_tools") && options.toolset === undefined) {
      throw new TypeError("search_tools needs a toolset");
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
   * JSON string. Mistakes the model can fix (a bad argument, a refused script, an unknown run)
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
    try {
      result = await this.dispatch(name, parseInput(name, input), options);
    } catch (error) {
      result = failure(name, error, this.limits);
    }
    // Each part is bounded; this bounds what a script, a server or a tool wrote unbounded.
    return { ...result, content: truncateMiddle(result.content, this.limits.total) };
  }

  private dispatch(name: AgentToolName, values: Values, options: HandleOptions): Promise<AgentToolResult> {
    switch (name) {
      case "run_script":
        return this.runScript(values, options);
      case "check_script":
        return this.checkScript(values, options);
      case "test_script":
        return this.testScript(values, options);
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
      case "search_tools":
        return this.searchTools(values, options);
    }
  }

  private async runScript(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const code = requireString(input, "code");
    const wait = this.waitOf(input["wait"]);
    const idempotencyKey = await runIdempotencyKey(this.options.labels, options.toolCallId);
    const run = await this.client.runs.start({
      script: code,
      toolset: this.options.toolset,
      input: input["input"],
      wait,
      idempotencyKey,
      labels: this.options.labels,
      ttl: this.options.ttl,
      ...this.traceContext(options),
      signal: options.signal,
    });
    await this.checkAnswered(run, code, idempotencyKey, options.toolCallId);
    return this.report(run, { errorWhenFailed: true, trace: run.status === "failed", signal: options.signal });
  }

  private async checkScript(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const result = await this.client.scripts.check({
      script: requireString(input, "code"),
      toolset: this.options.toolset,
      signal: options.signal,
    });
    return { content: describeCheck(result, this.limits), isError: !result.ok };
  }

  private async testScript(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const code = requireString(input, "code");
    const scenario = input["scenario"];
    if (!isObject(scenario)) {
      throw new InputError("`scenario` must be an object.");
    }
    const request: TestRequest = { script: code };
    if (this.options.toolset !== undefined) {
      request.toolset = this.options.toolset;
    }
    if (scenario["input"] !== undefined) {
      request.input = scenario["input"];
    }
    if (scenario["tools"] !== undefined) {
      if (!isObject(scenario["tools"])) {
        throw new InputError("`scenario.tools` must be an object of responses by tool name.");
      }
      request.tools = scenario["tools"];
    }
    if (scenario["mocks"] !== undefined) {
      request.mocks = requireString(scenario, "mocks", "scenario.mocks");
    }
    if (scenario["signals"] !== undefined) {
      if (
        !Array.isArray(scenario["signals"]) ||
        !scenario["signals"].every((s) => isObject(s) && typeof s["name"] === "string")
      ) {
        throw new InputError("`scenario.signals` must be an array of { name, payload?, when? }.");
      }
      request.signals = scenario["signals"] as ScenarioSignal[];
    }
    if (scenario["expect"] !== undefined) {
      if (!isObject(scenario["expect"])) {
        throw new InputError("`scenario.expect` must be an object.");
      }
      request.expect = scenario["expect"];
    }
    if (scenario["from_run"] !== undefined) {
      const runId = requireString(scenario, "from_run", "scenario.from_run");
      await this.scopedRun(runId, options.signal);
      request.cassette = await this.client.runs.journal(runId, { as: "cassette", signal: options.signal });
    }
    const result = await this.client.scripts.test(request, { signal: options.signal });
    return { content: describeTest(result, this.limits), isError: !result.passed };
  }

  private async getRun(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const runId = requireString(input, "run_id");
    const entry = optionalIndex(input, "entry");
    const offset = optionalIndex(input, "offset");
    const wait = input["wait"] === undefined ? undefined : this.waitOf(input["wait"]);
    const { signal } = options;
    let run = await this.scopedRun(runId, signal);
    if (wait !== undefined && entry === undefined) {
      run = await this.client.runs.get(runId, { wait, signal });
    }
    if (entry !== undefined) {
      const text = entryText(await this.client.runs.entry(runId, entry, { signal }));
      const content = describePage(
        `Entry ${entry} of run ${runId}`,
        text,
        offset ?? 0,
        this.limits.output,
        (next) => `get_run(${args({ run_id: runId, entry, offset: next })})`,
      );
      return { content, isError: false, run };
    }
    if (offset !== undefined) {
      if (run.status !== "completed") {
        throw new InputError(`run ${runId} has no output to page through: it is ${run.status}.`);
      }
      const content = describePage(
        `Output of run ${runId}`,
        await this.outputOf(run, signal),
        offset,
        this.limits.output,
        (next) => `get_run(${args({ run_id: runId, offset: next })})`,
      );
      return { content, isError: false, run };
    }
    return this.report(run, { errorWhenFailed: false, trace: true, signal });
  }

  private async listRuns(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const status = input["status"];
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
    const accepted = await this.client.runs.signal(runId, name, input["payload"] ?? null, {
      idempotencyKey: await idempotencyKeyOf(options.toolCallId),
      signal: options.signal,
    });
    const content = accepted.duplicate
      ? `This signal ${JSON.stringify(name)} was already sent to run ${runId}: nothing was added.`
      : `Sent the signal ${JSON.stringify(name)} to run ${runId}. Read what the run does next with get_run(${args({ run_id: runId, wait: "30s" })}).`;
    return { content, isError: false, run };
  }

  private async cancelRun(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const runId = requireString(input, "run_id");
    await this.scopedRun(runId, options.signal);
    const run = await this.client.runs.cancel(runId, { wait: this.defaultWaitMs, signal: options.signal });
    return this.report(run, { errorWhenFailed: false, trace: false, signal: options.signal });
  }

  private async retryRun(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const runId = requireString(input, "run_id");
    const code = requireString(input, "code");
    await this.scopedRun(runId, options.signal);
    const idempotencyKey = await runIdempotencyKey(this.options.labels, options.toolCallId);
    const run = await this.client.runs.fork(runId, {
      script: code,
      labels: this.options.labels,
      wait: this.defaultWaitMs,
      idempotencyKey,
      ttl: this.options.ttl,
      ...this.traceContext(options),
      signal: options.signal,
    });
    await this.checkAnswered(run, code, idempotencyKey, options.toolCallId);
    return this.report(run, { errorWhenFailed: true, trace: run.status === "failed", signal: options.signal });
  }

  private async searchTools(input: Values, options: HandleOptions): Promise<AgentToolResult> {
    const query = requireString(input, "query");
    const toolset = this.options.toolset;
    if (toolset === undefined) {
      throw new RewindError("search_tools needs a toolset");
    }
    const results = await this.client.toolsets.search(toolset, query, { signal: options.signal });
    return { content: describeMatches(query, results.tools), isError: false };
  }

  /** A run's report: its output when it completed, its trace when asked. */
  private async report(
    run: Run,
    options: { errorWhenFailed: boolean; trace: boolean; signal: AbortSignal | undefined },
  ): Promise<AgentToolResult> {
    const output = run.status === "completed" ? await this.outputOf(run, options.signal) : undefined;
    const trace = options.trace ? (await this.client.runs.trace(run.id, { signal: options.signal })).text : undefined;
    const ended = run.status === "failed" || run.status === "cancelled" || run.status === "aborted";
    return {
      content: describeRun(run, { output, trace }, this.limits),
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
   * or the run of an earlier script. Such a run is the harness's fault, never shown to the model:
   * it must carry the agent's labels and be a run of this very script.
   */
  private async checkAnswered(
    run: Run,
    code: string,
    idempotencyKey: string | undefined,
    toolCallId: string | undefined,
  ): Promise<void> {
    if (idempotencyKey === undefined) {
      return;
    }
    if (hasLabels(run.labels, this.options.labels) && run.script === (await sha256Hex(code))) {
      return;
    }
    throw new RewindError(
      `the tool call id ${JSON.stringify(toolCallId)} is already the idempotency key of another run (of another script, or with other labels): give each tool call a unique id`,
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

function failure(name: AgentToolName, error: unknown, limits: Limits): AgentToolResult {
  if (error instanceof InputError) {
    return { content: `${name}: ${error.message}`, isError: true };
  }
  if (error instanceof NamespaceRequiredError) {
    // The harness's configuration: its client names no end user. The model cannot fix it.
    throw error;
  }
  if (error instanceof RewindApiError) {
    if (error.slug === "invalid-script") {
      return { content: describeRefusal(error.detail, error.diagnostics, limits), isError: true };
    }
    if (error.status !== 401 && error.status < 500) {
      return { content: `${name} failed: ${truncateEnd(error.detail, limits.value)}`, isError: true };
    }
  }
  throw error;
}

function requireString(values: Values, key: string, label = key): string {
  const value = values[key];
  if (typeof value !== "string" || value === "") {
    throw new InputError(`\`${label}\` must be a non-empty string.`);
  }
  return value;
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
 * The idempotency key of the run a tool call starts (`run_script`, `retry_run`): the SHA-256 of
 * `JSON.stringify([labels, toolCallId])`, the labels as `[key, value]` pairs sorted by key (by
 * code point). A run's key is unique per tenant, and harnesses number their tool calls alike
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

/** The SHA-256 of a text's UTF-8, in hexadecimal: a script's digest, as the server computes it. */
async function sha256Hex(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
