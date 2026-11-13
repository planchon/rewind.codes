import { RewindApiError, isProblem } from "./errors.js";
import { backoff, sleep } from "./http.js";
import type { TaskFailure, Tasks } from "./resources/tasks.js";
import type { Duration, Labels, Task } from "./types.js";

/** What a handler knows about the call it executes, and what it can do about it. */
export interface HostToolContext {
  /** The claimed task, as the server sent it. */
  readonly task: Task;
  readonly runId: string;
  /** The call's position in the run. */
  readonly seq: number;
  /** From 1. Above 1, an earlier attempt was interrupted: the call may already have happened. */
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
   * Aborted when the claim is lost: the run ended, or the claim expired and the task went to
   * another worker. The result of the call is then dropped.
   */
  readonly signal: AbortSignal;
  /** Sends a heartbeat now. The worker already sends them on its own while the handler runs. */
  heartbeat(): Promise<void>;
  /**
   * The handler answers later, possibly days later, by the returned token: the task is
   * `deferred` and needs no heartbeat, up to `timeout` (bounded by the run's TTL). After it, the
   * worker no longer answers the task when the handler returns, but still fails it if the
   * handler throws.
   */
  defer(options?: { timeout?: Duration | undefined }): Promise<DeferredTask>;
}

/** A task the handler answers later, from anywhere, by its token. */
export interface DeferredTask {
  /** The claim's token: keep it to answer, even from another process (`rewind.tasks.complete`). */
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
  /** What failed: claiming tasks, a heartbeat, answering a task, or a handler that threw an unexpected error. */
  phase: "claim" | "heartbeat" | "answer" | "handler";
  task?: Task | undefined;
}

export interface WorkerOptions {
  /** Only tasks of runs of this toolset. */
  toolset?: string | undefined;
  /** The handlers, by tool name as the toolset declares it. The worker claims only these tools. */
  tools: Record<string, HostToolHandler>;
  /** At most this many handlers running at once. 10 by default. */
  concurrency?: number | undefined;
  /**
   * How long one claim request waits for a task (30 s by default, 60 s at most). `stop()` lets
   * the claim in flight finish, so it takes up to this long.
   */
  pollWait?: Duration | undefined;
  /** Stops the worker, as `stop()`, when aborted. */
  signal?: AbortSignal | undefined;
  /** Called with what fails along the way. Logs to `console.error` by default. */
  onError?: ((error: unknown, info: WorkerErrorInfo) => void) | undefined;
}

/**
 * Executes the host tools of a harness: it long-polls `GET /v1/tasks` for its tools, runs each
 * handler, keeps the claim alive with heartbeats while it runs, and answers with the result, or
 * the failure when the handler throws. A handler can also defer its answer.
 */
export class HostToolWorker {
  /** Settles when the worker has stopped and its handlers have answered; rejects on a fatal error. */
  readonly done: Promise<void>;

  private readonly tasks: Tasks;
  private readonly options: WorkerOptions;
  private readonly tools: string[];
  private readonly concurrency: number;
  private readonly inFlight = new Set<Promise<void>>();
  private readonly stopping = new AbortController();
  private wake: (() => void) | undefined;

  constructor(tasks: Tasks, options: WorkerOptions) {
    this.tasks = tasks;
    this.options = options;
    this.tools = Object.keys(options.tools);
    if (this.tools.length === 0) {
      throw new TypeError("a worker needs at least one host tool handler");
    }
    this.concurrency = options.concurrency ?? 10;
    if (!Number.isInteger(this.concurrency) || this.concurrency < 1) {
      throw new RangeError(`invalid concurrency ${this.concurrency}: expected an integer of 1 or more`);
    }
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
   * Stops claiming tasks, lets the claim in flight finish (its tasks are executed), and waits
   * for the handlers to answer.
   */
  stop(): Promise<void> {
    this.stopping.abort();
    this.notify();
    return this.done;
  }

  private async loop(): Promise<void> {
    let failures = 0;
    try {
      while (!this.stopping.signal.aborted) {
        if (this.inFlight.size >= this.concurrency) {
          await this.slot();
          continue;
        }
        let tasks: Task[];
        try {
          const batch = await this.tasks.claim({
            toolset: this.options.toolset,
            tools: this.tools,
            limit: Math.min(100, this.concurrency - this.inFlight.size),
            wait: this.options.pollWait ?? "30s",
          });
          tasks = batch.tasks;
          failures = 0;
        } catch (error) {
          if (isFatal(error)) {
            this.report(error, { phase: "claim" });
            throw error;
          }
          this.report(error, { phase: "claim" });
          await sleep(backoff(failures, 500, 30_000), this.stopping.signal).catch(() => undefined);
          failures += 1;
          continue;
        }
        for (const task of tasks) {
          this.start(task);
        }
      }
    } finally {
      while (this.inFlight.size > 0) {
        await Promise.allSettled([...this.inFlight]);
      }
    }
  }

  private start(task: Task): void {
    const execution = this.execute(task)
      .catch((error: unknown) => this.report(error, { phase: "answer", task }))
      .finally(() => {
        this.inFlight.delete(execution);
        this.notify();
      });
    this.inFlight.add(execution);
  }

  private async execute(task: Task): Promise<void> {
    const handler = Object.hasOwn(this.options.tools, task.tool) ? this.options.tools[task.tool] : undefined;
    const lost = new AbortController();
    const heartbeats = new Heartbeats(this.tasks, task, lost, (error) =>
      this.report(error, { phase: "heartbeat", task }),
    );
    let deferred: DeferredTask | undefined;
    const context: HostToolContext = {
      task,
      runId: task.runId,
      seq: task.seq,
      attempt: task.attempt,
      idempotencyKey: task.idempotencyKey,
      labels: task.labels,
      traceparent: task.traceparent ?? undefined,
      tracestate: task.tracestate ?? undefined,
      signal: lost.signal,
      heartbeat: async () => {
        await heartbeats.beat();
        if (lost.signal.aborted) {
          throw lost.signal.reason;
        }
      },
      defer: async (options = {}) => {
        if (deferred !== undefined) {
          return deferred;
        }
        heartbeats.stop();
        try {
          await this.tasks.defer(task.token, { timeout: options.timeout });
        } catch (error) {
          if (isProblem(error, "task-gone")) {
            lost.abort(error);
          } else {
            heartbeats.start();
          }
          throw error;
        }
        deferred = {
          token: task.token,
          taskId: task.id,
          complete: (result) => this.tasks.complete(task.token, result),
          fail: (failure) => this.tasks.fail(task.token, typeof failure === "string" ? { message: failure } : failure),
        };
        return deferred;
      },
    };

    let outcome: Outcome;
    heartbeats.start();
    try {
      if (handler === undefined) {
        throw new HostToolError(`this worker has no handler for the tool ${task.tool}`);
      }
      outcome = { result: await handler(task.args, context) };
    } catch (error) {
      if (!(error instanceof HostToolError) && !lost.signal.aborted) {
        this.report(error, { phase: "handler", task });
      }
      outcome = { failure: failureOf(error) };
    } finally {
      heartbeats.stop();
    }
    if (lost.signal.aborted) {
      // The claim is lost: the server would refuse the answer.
      return;
    }
    if (deferred !== undefined && "result" in outcome) {
      // Whoever holds the token answers.
      return;
    }
    await this.answer(task, outcome);
  }

  /** Completes or fails the task, again after a network error; a task that is gone is dropped. */
  private async answer(task: Task, outcome: Outcome): Promise<void> {
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
          // The run ended, or the claim was lost (possibly after an earlier attempt of this
          // answer that did arrive): the server drops the answer.
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

  private slot(): Promise<void> {
    return new Promise((resolve) => {
      this.wake = resolve;
    });
  }

  private notify(): void {
    const wake = this.wake;
    this.wake = undefined;
    wake?.();
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

/** Keeps a claim alive: a heartbeat every third of the heartbeat timeout. */
class Heartbeats {
  private timer: ReturnType<typeof setTimeout> | undefined;
  private running = false;
  private timeoutMs: number;

  constructor(
    private readonly tasks: Tasks,
    private readonly task: Task,
    private readonly lost: AbortController,
    private readonly onError: (error: unknown) => void,
  ) {
    this.timeoutMs = task.heartbeatTimeoutMs;
  }

  start(): void {
    if (this.running || this.lost.signal.aborted) {
      return;
    }
    this.running = true;
    this.schedule();
  }

  stop(): void {
    this.running = false;
    if (this.timer !== undefined) {
      clearTimeout(this.timer);
      this.timer = undefined;
    }
  }

  /** One heartbeat. A task that is gone aborts the context's signal and stops the heartbeats. */
  async beat(): Promise<void> {
    try {
      const ack = await this.tasks.heartbeat(this.task.token);
      this.timeoutMs = ack.heartbeatTimeoutMs;
    } catch (error) {
      if (isProblem(error, "task-gone")) {
        this.stop();
        this.lost.abort(error);
        return;
      }
      throw error;
    }
  }

  private schedule(): void {
    const interval = Math.max(100, Math.floor(this.timeoutMs / 3));
    this.timer = setTimeout(() => {
      this.timer = undefined;
      void this.beat()
        .catch(this.onError)
        .finally(() => {
          if (this.running) {
            this.schedule();
          }
        });
    }, interval);
  }
}

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
function jsonOutcome(task: Task, outcome: Outcome): Outcome {
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
  const task = info.task === undefined ? "" : ` (${info.task.tool}, run ${info.task.runId}, call ${info.task.seq})`;
  console.error(`[Rewind worker] ${info.phase}${task}:`, error);
}
