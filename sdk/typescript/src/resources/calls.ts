import { RewindConnectionError } from "../errors.js";
import type { Transport } from "../http.js";
import { sseMessages } from "../sse.js";
import type { RequestOptions } from "../params.js";
import type { CallGone, CallStreamMessage, Duration, HandedCall } from "../types.js";

export interface CallStreamParams extends RequestOptions {
  /**
   * Only calls of runs of this action set. Without it, the calls of the actions named in `actions`, whatever the
   * action set of their run.
   */
  actionSet?: string | undefined;
  /** Only these actions. All the harness actions of `actionSet` when absent. */
  actions?: string[] | undefined;
  /** Only calls of runs of this namespace (an end user's token is limited to its own). */
  namespace?: string | undefined;
  /** At most this many calls in flight on the stream (1 by default, 100 at most; 0 hands none). */
  slots?: number | undefined;
  /**
   * The worker's own id, the same on every reconnection: a stream opened again with it within the reconnect timeout
   * keeps the calls handed on the one that closed.
   */
  worker?: string | undefined;
  /** Called once the server has answered and the stream is open. */
  onOpen?: (() => void) | undefined;
  /** Called with each chunk the server writes, comments included: what tells a live stream from a silent one. */
  onBytes?: (() => void) | undefined;
}

/** How a harness action failed, as the workflow's code will see it: a thrown `ActionError`. */
export interface CallFailure {
  /** The message of the `ActionError`, written for the model. */
  message: string;
  /** `ActionError.data`. */
  data?: unknown;
  /** A transient failure: under `idempotent: true`, the call is delivered again later, with backoff. */
  retryable?: boolean | undefined;
}

/**
 * The calls of harness actions, at the level of the API. The worker (`rewind.worker`) follows the call stream and
 * answers; a call deferred by a handler is answered here, by its token, possibly days later. A call waiting for the
 * harness's approval is approved or rejected here, by its id.
 */
export class Calls {
  constructor(private readonly transport: Transport) {}

  /**
   * The calls handed on one call stream (`GET /v1/calls/stream`), until the server ends it or the connection drops:
   * `call`, a call with its token; `gone`, a call handed on it that no longer waits for its answer. The open stream is
   * the liveness of every call handed on it: there is no heartbeat. It is never opened again here: the worker does
   * that, with a backoff after an error. Comments and messages of a later version are skipped.
   */
  async *stream(params: CallStreamParams = {}): AsyncGenerator<CallStreamMessage> {
    const { signal } = params;
    const body = await this.transport.send(
      () =>
        this.transport.api.GET("/v1/calls/stream", {
          params: {
            query: {
              actionSet: params.actionSet,
              actions: params.actions,
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
      throw new RewindConnectionError("the call stream has no body");
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
      if (message.event === "call") {
        yield { type: "call", call: JSON.parse(message.data) as HandedCall };
      } else if (message.event === "gone") {
        yield { type: "gone", token: (JSON.parse(message.data) as CallGone).token };
      }
    }
  }

  /**
   * The handler answers later, with `complete` or `fail` and the same token: the call is `deferred`, its slot on the
   * stream free, up to `timeout` (bounded by the run's TTL).
   */
  async defer(token: string, options: RequestOptions & { timeout?: Duration | undefined } = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.POST("/v1/calls/{token}/defer", {
          params: { path: { token } },
          body: { timeout: options.timeout },
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }

  /** Delivers the action's result to the workflow's code. `call-gone` (410): the result was dropped. */
  async complete(token: string, result: unknown, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.POST("/v1/calls/{token}/complete", {
          params: { path: { token } },
          body: { result: result === undefined ? null : result },
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }

  /** Fails the call: the code gets an `ActionError`, unless the failure is retryable under `idempotent: true`. */
  async fail(token: string, failure: CallFailure, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.POST("/v1/calls/{token}/fail", {
          params: { path: { token } },
          body: failure,
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }

  /**
   * Approves a call that waits for the harness, by its id: a call under the `approval` policy is sent; a call parked by
   * a schema change goes on against the new schema; a signed call its server refused is sent again. A call that does
   * not wait for the harness is `call-not-waiting`.
   */
  async approve(id: string, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () => this.transport.api.POST("/v1/calls/{id}/approve", { params: { path: { id } }, signal: options.signal }),
      { retry: false, signal: options.signal },
    );
  }

  /** Rejects a call that waits for the harness: it fails with an `ActionError` carrying `message`. */
  async reject(id: string, options: RequestOptions & { message?: string | undefined } = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.POST("/v1/calls/{id}/reject", {
          params: { path: { id } },
          body: { message: options.message },
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }
}
