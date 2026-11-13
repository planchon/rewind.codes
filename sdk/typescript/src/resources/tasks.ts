import { durationParam } from "../duration.js";
import type { Transport } from "../http.js";
import type { Duration, HeartbeatAck, TaskBatch } from "../types.js";
import type { RequestOptions } from "./toolsets.js";

export interface ClaimParams extends RequestOptions {
  /** Only tasks of runs of this toolset. */
  toolset?: string | undefined;
  /** Only these tools. All the toolset's host tools when absent. */
  tools?: string[] | undefined;
  /** At most this many tasks (1 by default, 100 at most). */
  limit?: number | undefined;
  /** How long to wait for a task before answering with none (30 s by default, 60 s at most). */
  wait?: Duration | undefined;
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
 * Host-tool tasks, at the level of the API. The worker (`rewind.worker`) drives them; a task
 * deferred by a handler is answered here, by its token, possibly days later.
 */
export class Tasks {
  constructor(private readonly transport: Transport) {}

  /** Claims host-tool tasks (long-poll). Each claim has a token and must be kept alive with heartbeats. */
  claim(params: ClaimParams = {}): Promise<TaskBatch> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/tasks", {
          params: {
            query: {
              toolset: params.toolset,
              tool: params.tools,
              limit: params.limit,
              wait: params.wait === undefined ? undefined : durationParam(params.wait),
            },
          },
          signal,
        }),
      // Never sent again: the lost answer of a claim holds claimed tasks.
      { retry: false, signal },
    );
  }

  /** Keeps a claim alive. A `task-gone` problem (410) means: stop working on the task. */
  heartbeat(token: string, options: RequestOptions = {}): Promise<HeartbeatAck> {
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/tasks/{token}/heartbeat", {
          params: { path: { token } },
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }

  /**
   * The handler answers later, with `complete` or `fail` and the same token: the task is
   * `deferred` and needs no heartbeat, up to `timeout` (bounded by the run's TTL).
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
