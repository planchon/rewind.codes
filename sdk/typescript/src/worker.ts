import { RewindApiError, RewindConnectionError, isProblem } from "./errors.js";
import { backoff, sleep } from "./http.js";
import type { TaskFailure, Tasks } from "./resources/tasks.js";
import type { Duration, HandedTask, Labels } from "./types.js";

/** What a handler knows about the call it executes, and what it can do about it. */
export interface HostToolContext {
  /** The task, as the server handed it. */
  readonly task: HandedTask;
  readonly runId: string;
  /**
   * The namespace of the run: the end user the handler acts for. Act on their data only, and reach their runs with
   * `rewind.namespace(ctx.namespace)`.
   */
  readonly namespace: string;
  /** The call's position in the run. */
  readonly seq: number;
  /**
   * From 1. Above 1, an earlier attempt was interrupted (the stream it was handed on closed and did not come back
   * within its grace): the call may already have happened.
   */
  readonly attempt: number;
  /**
   * `{run_id}/{seq}`, the same on every attempt: use it to recognize a call already made, so an
   * interrupted call made again does not repeat its effect.
   */
  readonly idempotencyKey: string;
  /** The run's labels. */
  readonly labels: Labels;
  /** W3C trace context, with the tool call's span as parent. */
  readonly traceparent: string | undefined;
  readonly tracestate: string | undefined;
  /**
   * Aborted when the task no longer waits for its answer: the stream says it is gone (its run ended, its call timed
   * out), or the server refused an answer as `task-gone`. The result of the call is then dropped.
   */
  readonly signal: AbortSignal;
  /**
   * The handler answers later, possibly days later, by the returned token: the task is `deferred` and its slot is
   * free, up to `timeout` (bounded by the run's TTL). After it, the worker no longer answers the task when the handler
   * returns, but still fails it if the handler throws.
   */
  defer(options?: { timeout?: Duration | undefined }): Promise<DeferredTask>;
}

/** A task the handler answers later, from anywhere, by its token. */
export interface DeferredTask {
  /** The task's token: keep it to answer, even from another process (`rewind.tasks.complete`). */
  readonly token: string;
  readonly taskId: string;
  complete(result: unknown): Promise<void>;
  fail(failure: TaskFailure | string): Promise<void>;
}

/** A host tool's implementation: its arguments, validated against the tool's input schema, and a context. */
export type HostToolHandler<Args = any, Result = unknown> = (
  args: Args,
  context: HostToolContext,
) => Result | Promise<Result>;

/**
 * A deliberate failure of a host tool: the script gets a `ToolError` with this message and
 * `data`. With `retryable`, the call is made again later under the `retry` policy.
 */
export class HostToolError extends Error {
  override readonly name = "HostToolError";
  readonly data: unknown;
  readonly retryable: boolean;

  constructor(message: string, options: { data?: unknown; retryable?: boolean; cause?: unknown } = {}) {
    super(message, options.cause === undefined ? undefined : { cause: options.cause });
    this.data = options.data;
    this.retryable = options.retryable ?? false;
  }
}

export interface WorkerErrorInfo {
  /** What failed: the task stream, answering a task, or a handler that threw an unexpected error. */
  phase: "stream" | "answer" | "handler";
  task?: HandedTask | undefined;
}

/** How the worker follows its stream; the defaults suit the server's keep-alive. */
export interface WorkerTimings {
  /** The wait before opening the stream again after its first error, doubled after each one: 500 ms. */
  backoffMs?: number | undefined;
  /** The longest of those waits: 30 s. */
  maxBackoffMs?: number | undefined;
  /**
   * A stream that writes nothing for this long, not even the comment the server writes at least every 15 s, is taken
   * as broken and opened again: 45 s.
   */
  silenceMs?: number | undefined;
  /** A stream the server closes sooner than this after it opened counts as an error: 5 s. */
  minLifeMs?: number | undefined;
}

export interface WorkerOptions {
  /**
   * Only tasks of runs of this toolset. Without it, the tasks of the tools in `tools` whatever the toolset of their
   * run, a toolset derived from another (as `save_workflow` does) included.
   */
  toolset?: string | undefined;
  /** The handlers, by tool name as the toolset declares it. The worker is handed only these tools. */
  tools: Record<string, HostToolHandler>;
  /** At most this many handlers running at once: the slots of its stream. 10 by default, 100 at most. */
  concurrency?: number | undefined;
  /** Stops the worker, as `stop()`, when aborted. */
  signal?: AbortSignal | undefined;
  /** Called with what fails along the way. Logs to `console.error` by default. */
  onError?: ((error: unknown, info: WorkerErrorInfo) => void) | undefined;
  timings?: WorkerTimings | undefined;
}

/** The reason a context's signal aborts when the stream says its task is gone. */
function gone(task: HandedTask): RewindApiError {
  return new RewindApiError({
    type: "urn:rewind:problem:task-gone",
    title: "Gone",
    status: 410,
    detail: `the task of ${task.tool} (run ${task.runId}, call ${task.seq}) no longer waits for its answer`,
  });
}

/**
 * Executes the host tools of a harness: it holds a task stream open (`GET /v1/tasks/stream`) for its tools, is handed
 * each task as it becomes ready, runs the handler, and answers with the result, or the failure when the handler
 * throws. A handler can also defer its answer.
 *
 * The open stream is the liveness of the tasks handed on it: nothing is sent while a handler runs. After a drop, the
 * worker opens the stream again with the same id, after a backoff that starts over once a stream lived; the server
 * keeps the tasks handed to it for their grace, and the handlers in flight go on.
 *
 * One worker with the customer's key serves every end user: each task names the namespace of its run
 * (`ctx.namespace`). A worker holding an end user's token is handed only the tasks of that namespace's runs.
 */
export class HostToolWorker {
  /** Settles when the worker has stopped and its handlers have answered; rejects on a fatal error. */
  readonly done: Promise<void>;
  /** The worker's id on its stream, the same on every reconnection. */
  readonly id: string;

  private readonly tasks: Tasks;
  private readonly options: WorkerOptions;
  private readonly tools: string[];
  private readonly concurrency: number;
  /** The handlers running, by their task's token, each with what aborts its context's signal. */
  private readonly inFlight = new Map<string, { execution: Promise<void>; lose: (reason?: unknown) => void }>();
  private readonly stopping = new AbortController();
  /** Closes the stream being read. */
  private connection: AbortController | null = null;
  private drained: (() => void) | undefined;

  constructor(tasks: Tasks, options: WorkerOptions) {
    this.tasks = tasks;
    this.options = options;
    this.tools = Object.keys(options.tools);
    if (this.tools.length === 0) {
      throw new TypeError("a worker needs at least one host tool handler");
    }
    this.concurrency = options.concurrency ?? 10;
    if (!Number.isInteger(this.concurrency) || this.concurrency < 1 || this.concurrency > 100) {
      throw new RangeError(`invalid concurrency ${this.concurrency}: expected an integer from 1 to 100`);
    }
    this.id = globalThis.crypto.randomUUID();
    if (options.signal?.aborted) {
      this.stopping.abort();
    } else {
      options.signal?.addEventListener("abort", () => void this.stop(), { once: true });
    }
    this.done = this.loop();
    // `done` is there for whoever awaits it; a fatal error is reported through `onError` too.
    this.done.catch(() => undefined);
  }

  /** How many handlers are running. */
  get running(): number {
    return this.inFlight.size;
  }

  /**
   * Stops: no task is handed to the worker any more, and it waits for the handlers in flight to answer. Meanwhile its
   * stream stays open with no slot, so that the server keeps their tasks for it.
   */
  stop(): Promise<void> {
    if (!this.stopping.signal.aborted) {
      this.stopping.abort();
      this.connection?.abort();
    }
    return this.done;
  }

  private async loop(): Promise<void> {
    try {
      await this.follow(() => this.stopping.signal.aborted, this.concurrency);
    } finally {
      // Its tasks stay alive while the handlers finish them: a stream with no slot.
      if (this.inFlight.size > 0) {
        const idle = new Promise<void>((resolve) => {
          this.drained = resolve;
        });
        await Promise.race([this.follow(() => this.inFlight.size === 0, 0).catch(() => undefined), idle]);
        this.connection?.abort();
        while (this.inFlight.size > 0) {
          await Promise.allSettled([...this.inFlight.values()].map((f) => f.execution));
        }
      }
    }
  }

  /** Holds the stream open with `slots` until `over()`, opening it again after a drop. */
  private async follow(over: () => boolean, slots: number): Promise<void> {
    const timings = this.options.timings ?? {};
    const minLifeMs = timings.minLifeMs ?? 5_000;
    let failures = 0;
    while (!over()) {
      const current = new AbortController();
      this.connection = current;
      let openedAt: number | null = null;
      try {
        let silence: ReturnType<typeof setTimeout> | undefined;
        const heard = () => {
          clearTimeout(silence);
          silence = setTimeout(
            () => current.abort(new RewindConnectionError("the task stream went silent")),
            timings.silenceMs ?? 45_000,
          );
        };
        try {
          heard();
          const events = this.tasks.stream({
            toolset: this.options.toolset,
            tools: this.tools,
            slots,
            worker: this.id,
            signal: current.signal,
            onOpen: () => {
              openedAt = Date.now();
            },
            onBytes: heard,
          });
          for await (const event of events) {
            if (event.type === "task") {
              this.start(event.task);
            } else {
              this.inFlight.get(event.token)?.lose();
            }
          }
        } finally {
          clearTimeout(silence);
        }
        // The server ended the stream (a token that expires, a proxy): opened again at once, unless it closed as soon
        // as it opened.
        if (openedAt !== null && Date.now() - openedAt >= minLifeMs) {
          failures = 0;
          continue;
        }
        if (!over()) {
          throw new RewindConnectionError("the task stream closed as soon as it opened");
        }
      } catch (error) {
        if (over()) {
          return;
        }
        const reason = current.signal.aborted ? (current.signal.reason ?? error) : error;
        this.report(reason, { phase: "stream" });
        if (isFatal(reason)) {
          throw reason;
        }
        if (openedAt !== null && Date.now() - openedAt >= minLifeMs) {
          failures = 0;
        }
        await sleep(
          backoff(failures, timings.backoffMs ?? 500, timings.maxBackoffMs ?? 30_000),
          this.stopping.signal.aborted ? undefined : this.stopping.signal,
        ).catch(() => undefined);
        failures += 1;
      } finally {
        if (this.connection === current) {
          this.connection = null;
        }
      }
    }
  }

  private start(task: HandedTask): void {
    if (this.inFlight.has(task.token)) {
      return;
    }
    const lost = new AbortController();
    // Without a reason, the stream said the task is gone: the reason is `task-gone`, as the server's answer would be.
    const lose = (reason?: unknown) => lost.abort(reason ?? gone(task));
    const execution = this.execute(task, lost.signal, lose)
      .catch((error: unknown) => this.report(error, { phase: "answer", task }))
      .finally(() => {
        this.inFlight.delete(task.token);
        if (this.inFlight.size === 0) {
          this.drained?.();
        }
      });
    this.inFlight.set(task.token, { execution, lose });
  }

  private async execute(task: HandedTask, signal: AbortSignal, lose: (reason?: unknown) => void): Promise<void> {
    const handler = Object.hasOwn(this.options.tools, task.tool) ? this.options.tools[task.tool] : undefined;
    const tasks = this.tasks;
    let deferred: DeferredTask | undefined;
    const context: HostToolContext = {
      task,
      runId: task.runId,
      namespace: task.namespace,
      seq: task.seq,
      attempt: task.attempt,
      idempotencyKey: task.idempotencyKey,
      labels: task.labels,
      traceparent: task.traceparent ?? undefined,
      tracestate: task.tracestate ?? undefined,
      signal,
      defer: async (options = {}) => {
        if (deferred !== undefined) {
          return deferred;
        }
        try {
          await tasks.defer(task.token, { timeout: options.timeout });
        } catch (error) {
          if (isProblem(error, "task-gone")) {
            lose(error);
          }
          throw error;
        }
        deferred = {
          token: task.token,
          taskId: task.id,
          complete: (result) => tasks.complete(task.token, result),
          fail: (failure) => tasks.fail(task.token, typeof failure === "string" ? { message: failure } : failure),
        };
        return deferred;
      },
    };

    let outcome: Outcome;
    try {
      if (handler === undefined) {
        throw new HostToolError(`this worker has no handler for the tool ${task.tool}`);
      }
      outcome = { result: await handler(task.args, context) };
    } catch (error) {
      if (!(error instanceof HostToolError) && !signal.aborted) {
        this.report(error, { phase: "handler", task });
      }
      outcome = { failure: failureOf(error) };
    }
    if (signal.aborted) {
      // The task is gone: the server would refuse the answer.
      return;
    }
    if (deferred !== undefined && "result" in outcome) {
      // Whoever holds the token answers.
      return;
    }
    await this.answer(task, outcome, lose);
  }

  /** Completes or fails the task, again after a network error; a task that is gone is dropped. */
  private async answer(task: HandedTask, outcome: Outcome, lose: (reason?: unknown) => void): Promise<void> {
    let current = jsonOutcome(task, outcome);
    for (let attempt = 0; ; attempt++) {
      try {
        if ("result" in current) {
          await this.tasks.complete(task.token, current.result);
        } else {
          await this.tasks.fail(task.token, current.failure);
        }
        return;
      } catch (error) {
        if (isProblem(error, "task-gone")) {
          // The run ended, or the task was handed again (possibly after an earlier attempt of this answer that did
          // arrive): the server drops the answer.
          lose(error);
          return;
        }
        if ("result" in current && isProblem(error, "payload-too-large")) {
          current = { failure: { message: `the result of ${task.tool} is too large: ${error.detail}` } };
          continue;
        }
        if ("failure" in current && isProblem(error, "payload-too-large") && !isCut(current.failure)) {
          current = { failure: cut(current.failure) };
          continue;
        }
        if ((error instanceof RewindApiError && error.status < 500) || attempt >= 4) {
          throw error;
        }
        await sleep(backoff(attempt, 500, 10_000));
      }
    }
  }

  private report(error: unknown, info: WorkerErrorInfo): void {
    const onError = this.options.onError ?? defaultOnError;
    try {
      onError(error, info);
    } catch {
      // A failing error handler must not stop the worker.
    }
  }
}

type Outcome = { result: unknown } | { failure: TaskFailure };

/** A refusal that asking again will not change: a bad request, a refused key, an unknown toolset. */
function isFatal(error: unknown): boolean {
  return error instanceof RewindApiError && [400, 401, 403, 404].includes(error.status);
}

function failureOf(error: unknown): TaskFailure {
  if (error instanceof HostToolError) {
    return { message: error.message, data: error.data, retryable: error.retryable || undefined };
  }
  if (error instanceof Error) {
    const message = error.message || "the host tool failed";
    return { message: error.name === "Error" ? message : `${error.name}: ${message}` };
  }
  return { message: String(error) };
}

/** Characters of a failure's message kept when the server refuses the failure as too large. */
const FAILURE_MESSAGE = 2_000;

/** The failure without its data, its message cut: what the server accepts of a large failure. */
function cut(failure: TaskFailure): TaskFailure {
  const message =
    failure.message.length > FAILURE_MESSAGE ? `${failure.message.slice(0, FAILURE_MESSAGE)}…` : failure.message;
  return { message, retryable: failure.retryable };
}

function isCut(failure: TaskFailure): boolean {
  return failure.data === undefined && failure.message.length <= FAILURE_MESSAGE + 1;
}

/** An outcome whose values JSON can carry; otherwise a failure saying why. */
function jsonOutcome(task: HandedTask, outcome: Outcome): Outcome {
  if ("result" in outcome) {
    try {
      JSON.stringify(outcome.result ?? null);
      return outcome;
    } catch (error) {
      return { failure: { message: `the result of ${task.tool} is not JSON: ${(error as Error).message}` } };
    }
  }
  try {
    JSON.stringify(outcome.failure);
    return outcome;
  } catch {
    return { failure: { message: outcome.failure.message, retryable: outcome.failure.retryable } };
  }
}

function defaultOnError(error: unknown, info: WorkerErrorInfo): void {
  const task =
    info.task === undefined
      ? ""
      : ` (${info.task.tool}, run ${info.task.runId}, call ${info.task.seq}, namespace ${info.task.namespace})`;
  console.error(`[Rewind worker] ${info.phase}${task}:`, error);
}
