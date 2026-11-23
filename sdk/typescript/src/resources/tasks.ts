import { RewindConnectionError } from "../errors.js";
import type { Transport } from "../http.js";
import { sseMessages } from "../sse.js";
import type { Duration, HandedTask, TaskGone, TaskStreamEvent } from "../types.js";
import type { RequestOptions } from "./toolsets.js";

export interface TaskStreamParams extends RequestOptions {
  /**
   * Only tasks of runs of this toolset. Without it, the tasks of the tools named in `tools`, whatever the toolset of
   * their run.
   */
  toolset?: string | undefined;
  /** Only these tools. All the host tools of `toolset` when absent. */
  tools?: string[] | undefined;
  /** Only tasks of runs of this namespace (an end user's token is limited to its own). */
  namespace?: string | undefined;
  /** At most this many tasks in flight on the stream (1 by default, 100 at most; 0 hands none). */
  slots?: number | undefined;
  /**
   * The worker's own id, the same on every reconnection: a stream opened again with it within the grace keeps the
   * tasks handed on the one that closed.
   */
  worker?: string | undefined;
  /** Called once the server has answered and the stream is open. */
  onOpen?: (() => void) | undefined;
  /** Called with each chunk the server writes, comments included: what tells a live stream from a silent one. */
  onBytes?: (() => void) | undefined;
}

/** How a host tool failed, as the script will see it: a thrown `ToolError`. */
export interface TaskFailure {
  /** The message of the `ToolError`, written for the model. */
  message: string;
  /** `ToolError.data`. */
  data?: unknown;
  /** A transient failure: with the `retry` policy, the call is made again later, with backoff. */
  retryable?: boolean | undefined;
}

/**
 * Host-tool tasks, at the level of the API. The worker (`rewind.worker`) follows the task stream and answers; a task
 * deferred by a handler is answered here, by its token, possibly days later.
 */
export class Tasks {
  constructor(private readonly transport: Transport) {}

  /**
   * The tasks handed on one task stream (`GET /v1/tasks/stream`), until the server ends it or the connection drops:
   * `task`, a task with its token; `gone`, a task handed on it that no longer waits for its answer. The open stream is
   * the liveness of every task handed on it: there is no heartbeat. It is never opened again here: the worker does
   * that, with a backoff after an error. Comments and events of a later version are skipped.
   */
  async *stream(params: TaskStreamParams = {}): AsyncGenerator<TaskStreamEvent> {
    const { signal } = params;
    const body = await this.transport.send(
      () =>
        this.transport.api.GET("/v1/tasks/stream", {
          params: {
            query: {
              toolset: params.toolset,
              tools: params.tools,
              namespace: params.namespace,
              slots: params.slots,
              worker: params.worker,
            },
          },
          headers: { Accept: "text/event-stream" },
          parseAs: "stream",
          signal,
        }),
      // The worker opens it again after a drop, with its own backoff.
      { retry: false, signal },
    );
    if (body === null) {
      throw new RewindConnectionError("the task stream has no body");
    }
    params.onOpen?.();
    const onBytes = params.onBytes;
    const bytes =
      onBytes === undefined
        ? body
        : body.pipeThrough(
            new TransformStream<Uint8Array, Uint8Array>({
              transform(chunk, controller) {
                onBytes();
                controller.enqueue(chunk);
              },
            }),
          );
    for await (const message of sseMessages(bytes, signal)) {
      if (message.event === "task") {
        yield { type: "task", task: JSON.parse(message.data) as HandedTask };
      } else if (message.event === "gone") {
        yield { type: "gone", token: (JSON.parse(message.data) as TaskGone).token };
      }
    }
  }

  /**
   * The handler answers later, with `complete` or `fail` and the same token: the task is `deferred`, its slot on the
   * stream free, up to `timeout` (bounded by the run's TTL).
   */
  async defer(token: string, options: RequestOptions & { timeout?: Duration | undefined } = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.POST("/v1/tasks/{token}/defer", {
          params: { path: { token } },
          body: { timeout: options.timeout },
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }

  /** Delivers the tool's result to the script. `task-gone` (410): the result was dropped. */
  async complete(token: string, result: unknown, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.POST("/v1/tasks/{token}/complete", {
          params: { path: { token } },
          body: { result: result === undefined ? null : result },
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }

  /** Fails the call: the script gets a `ToolError`, unless the failure is retryable under `retry`. */
  async fail(token: string, failure: TaskFailure, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.POST("/v1/tasks/{token}/fail", {
          params: { path: { token } },
          body: failure,
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }

  /**
   * Approves a task that waits for the harness: a call under the `approval` policy is sent; a
   * call parked by a schema change goes on against the new schema.
   */
  async approve(id: string, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () => this.transport.api.POST("/v1/tasks/{id}/approve", { params: { path: { id } }, signal: options.signal }),
      { retry: false, signal: options.signal },
    );
  }

  /** Rejects a task that waits for the harness: the call fails with a `ToolError` carrying `message`. */
  async reject(id: string, options: RequestOptions & { message?: string | undefined } = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.POST("/v1/tasks/{id}/reject", {
          params: { path: { id } },
          body: { message: options.message },
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }
}
