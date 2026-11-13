import { EventSourceParserStream } from "eventsource-parser/stream";

import { durationParam, toMillis } from "../duration.js";
import { RewindApiError, RewindConnectionError, RewindError } from "../errors.js";
import { backoff, checkIdempotencyKey, messageOf, sleep, throwIfAborted, type Transport } from "../http.js";
import { labelParams, timeParam } from "../params.js";
import {
  isTerminal,
  type Credentials,
  type Duration,
  type Entry,
  type FailureGroupPage,
  type Labels,
  type Run,
  type RunPage,
  type RunStats,
  type RunStatus,
  type RunSummary,
  type SignalAccepted,
  type StreamEvent,
  type Trace,
} from "../types.js";
import type { RequestOptions } from "./toolsets.js";

/**
 * The longest a single request waits for a run on the server. A longer `wait` is split into
 * several requests, so proxies and the HTTP client's own timeouts never cut it.
 */
export const MAX_REQUEST_WAIT_MS = 50_000;

const MIN_REPOLL_MS = 1_000;

export interface StartRunParams extends RequestOptions {
  /** The TypeScript source: a single ES module whose default export is the entry point. */
  script: string;
  /** The toolset the script imports from `rewind:tools`. */
  toolset?: string | undefined;
  /** The input of `main`. `{}` when absent. */
  input?: unknown;
  /**
   * Wait up to this long for the run to end. The run is returned as it is then: ended, or still
   * active with what it waits for.
   */
  wait?: Duration | undefined;
  /** A request repeated with the same key returns the run the first one started. */
  idempotencyKey?: string | undefined;
  /** Run metadata: filters runs, scopes `list_runs`, API keys and webhooks. */
  labels?: Labels | undefined;
  /** W3C trace context of the caller: the run appears under it in the caller's trace. */
  traceparent?: string | undefined;
  tracestate?: string | undefined;
  /** The run's time to live: it is cancelled after it. 30 days by default. */
  ttl?: Duration | undefined;
  /** Type-check the script before starting it. `true` by default. */
  check?: boolean | undefined;
  /** The run's own credentials, used instead of the toolset's. */
  credentials?: Credentials | undefined;
}

export interface ForkRunParams extends RequestOptions {
  /** The corrected TypeScript source. */
  script: string;
  /** Go live from this journal entry, even if the commands still match. */
  at?: number | undefined;
  /** The fork's input. The parent's when absent. */
  input?: unknown;
  labels?: Labels | undefined;
  wait?: Duration | undefined;
  idempotencyKey?: string | undefined;
  traceparent?: string | undefined;
  tracestate?: string | undefined;
  ttl?: Duration | undefined;
  /** Type-check the corrected script first. `true` by default. */
  check?: boolean | undefined;
}

export interface ListRunsParams extends RequestOptions {
  /**
   * A query: terms separated by spaces, all of which must hold, as
   * `@agent:triage status:failed,aborted -@env:test "rate limit"`. Joined to the other
   * parameters by "and". A query that cannot be read is refused with `invalid-query`, whose
   * `query` says where the term at fault is.
   */
  q?: string | undefined;
  /** `started` (the default), `activity`, or `wakes`: the soonest to wake by itself first. */
  sort?: "started" | "activity" | "wakes" | undefined;
  status?: RunStatus | undefined;
  /** Only runs carrying all these labels. */
  labels?: Labels | undefined;
  /** Only runs that called this tool. */
  tool?: string | undefined;
  /** Only runs of this script (digest). */
  script?: string | undefined;
  /** Only runs whose calls failed with this kind, or that failed with this error name. */
  error?: string | undefined;
  quarantined?: boolean | undefined;
  /** Only forks of this run. */
  parent?: string | undefined;
  startedAfter?: Date | string | undefined;
  startedBefore?: Date | string | undefined;
  /** At most this many runs (50 by default, 500 at most). */
  limit?: number | undefined;
  cursor?: string | undefined;
}

export interface RunStatsParams extends RequestOptions {
  /** A query, as in `list`. */
  q?: string | undefined;
  /** Runs started at or after this time. No lower bound by default. */
  since?: Date | string | undefined;
  /** Runs started before this time. Now by default. */
  until?: Date | string | undefined;
  /** The width of the series' buckets, from `since`, which it requires. No series without it. */
  bucket?: Duration | undefined;
  /** Values per facet (10 by default, 50 at most). */
  top?: number | undefined;
}

export interface FailuresParams extends RequestOptions {
  /** A query, as in `list`. */
  q?: string | undefined;
  /** Runs ended at or after this time. No lower bound by default. */
  since?: Date | string | undefined;
  /** Runs ended before this time. Now by default. */
  until?: Date | string | undefined;
  /** The width of each group's series' buckets, from `since`, which it requires. */
  bucket?: Duration | undefined;
  /** Groups at most (50 by default, 500 at most). */
  limit?: number | undefined;
}

export interface EventsParams extends RequestOptions {
  /** Keep the stream open and send entries as they are committed, until the run ends. */
  follow?: boolean | undefined;
  /** The first entry to send. 0 by default. */
  from?: number | undefined;
  /** With `follow`: how many times to reconnect after the connection drops. 5 by default. */
  maxReconnects?: number | undefined;
}

/** Runs: start, follow, steer and read them. */
export class Runs {
  constructor(private readonly transport: Transport) {}

  /**
   * Starts a run. The script is compiled, checked and type-checked first: a script that cannot
   * run is refused with `invalid-script` (a `RewindApiError` with `diagnostics`) before any run
   * exists. With `wait`, answers when the run ends or `wait` passes, whichever comes first.
   */
  async start(params: StartRunParams): Promise<Run> {
    const { signal, idempotencyKey, wait, ...body } = params;
    checkIdempotencyKey(idempotencyKey);
    const asked = Date.now();
    const deadline = deadlineOf(wait, asked);
    const run = await this.transport.send(
      () =>
        this.transport.api.POST("/v1/runs", {
          params: { header: { "Idempotency-Key": idempotencyKey } },
          body: { ...body, wait: firstWait(wait) },
          signal,
        }),
      { retry: idempotencyKey !== undefined, signal },
    );
    return this.waitUntil(run, asked, deadline, signal);
  }

  /** A run's state. With `wait`, waits up to that long for the run to end first. */
  async get(id: string, options: RequestOptions & { wait?: Duration | undefined } = {}): Promise<Run> {
    const asked = Date.now();
    const deadline = deadlineOf(options.wait, asked);
    const run = await this.fetchRun(id, firstWait(options.wait), options.signal);
    return this.waitUntil(run, asked, deadline, options.signal);
  }

  /** A page of runs, most recently started first unless `sort` says otherwise. */
  list(params: ListRunsParams = {}): Promise<RunPage> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs", {
          params: {
            query: {
              q: params.q,
              sort: params.sort,
              status: params.status,
              label: labelParams(params.labels),
              tool: params.tool,
              script: params.script,
              error: params.error,
              quarantined: params.quarantined,
              parent: params.parent,
              startedAfter: timeParam(params.startedAfter),
              startedBefore: timeParam(params.startedBefore),
              limit: params.limit,
              cursor: params.cursor,
            },
          },
          signal,
        }),
      { retry: true, signal },
    );
  }

  /**
   * Statistics of the runs a query matches: counts by status, a series, durations and facets of
   * the runs started in the window; the runs waiting now by what they wait for, and the
   * quarantined ones.
   */
  stats(params: RunStatsParams = {}): Promise<RunStats> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/stats", {
          params: {
            query: {
              q: params.q,
              since: timeParam(params.since),
              until: timeParam(params.until),
              bucket: params.bucket === undefined ? undefined : durationParam(params.bucket),
              top: params.top,
            },
          },
          signal,
        }),
      { retry: true, signal },
    );
  }

  /** The failed, cancelled and aborted runs a query matches, grouped by fingerprint. */
  failures(params: FailuresParams = {}): Promise<FailureGroupPage> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/failures", {
          params: {
            query: {
              q: params.q,
              since: timeParam(params.since),
              until: timeParam(params.until),
              bucket: params.bucket === undefined ? undefined : durationParam(params.bucket),
              limit: params.limit,
            },
          },
          signal,
        }),
      { retry: true, signal },
    );
  }

  /** Every run matching the filter, page after page. */
  async *listAll(params: Omit<ListRunsParams, "cursor"> = {}): AsyncGenerator<RunSummary> {
    let cursor: string | undefined;
    do {
      const page = await this.list({ ...params, cursor });
      yield* page.runs;
      cursor = page.nextCursor ?? undefined;
    } while (cursor !== undefined);
  }

  /**
   * Sends a signal: it resolves the script's `signal(name)`, now or the next time the script
   * waits for that name. With an idempotency key, the same key sent twice delivers one signal.
   * A run that has ended refuses it (`run-ended`).
   */
  signal(
    id: string,
    name: string,
    payload?: unknown,
    options: RequestOptions & { idempotencyKey?: string | undefined } = {},
  ): Promise<SignalAccepted> {
    const { idempotencyKey, signal } = options;
    checkIdempotencyKey(idempotencyKey);
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/runs/{id}/signals/{name}", {
          params: { path: { id, name }, header: { "Idempotency-Key": idempotencyKey } },
          body: payload === undefined ? null : payload,
          signal,
        }),
      { retry: idempotencyKey !== undefined, signal },
    );
  }

  /**
   * Cancels a run: every pending await of the script rejects with `CancelledError`, and the
   * script gets one activation to clean up. With `wait`, returns the run's state after it, waiting
   * as `start` does.
   */
  async cancel(
    id: string,
    options: RequestOptions & { reason?: string | undefined; wait?: Duration | undefined } = {},
  ): Promise<Run> {
    const { signal, reason, wait } = options;
    const asked = Date.now();
    const deadline = deadlineOf(wait, asked);
    const run = await this.transport.send(
      () =>
        this.transport.api.POST("/v1/runs/{id}/cancel", {
          params: { path: { id } },
          body: { reason, wait: firstWait(wait) },
          signal,
        }),
      { retry: false, signal },
    );
    return this.waitUntil(run, asked, deadline, signal);
  }

  /**
   * Forks a run with a corrected script (`retry_run`): a new run that reuses the parent's
   * recorded results while its commands match, and continues live at the first difference, at
   * `at`, or at the end of the parent's history.
   */
  async fork(id: string, params: ForkRunParams): Promise<Run> {
    const { signal, idempotencyKey, wait, ...body } = params;
    checkIdempotencyKey(idempotencyKey);
    const asked = Date.now();
    const deadline = deadlineOf(wait, asked);
    const run = await this.transport.send(
      () =>
        this.transport.api.POST("/v1/runs/{id}/fork", {
          params: { path: { id }, header: { "Idempotency-Key": idempotencyKey } },
          body: { ...body, wait: firstWait(wait) },
          signal,
        }),
      { retry: idempotencyKey !== undefined, signal },
    );
    return this.waitUntil(run, asked, deadline, signal);
  }

  /** The run's trace, written for a model (`format=llm`). */
  trace(id: string, options: RequestOptions = {}): Promise<Trace> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/{id}/trace", {
          params: { path: { id }, query: { format: "llm" } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /** One journal entry in full: every payload of the entry has its value, whatever its size. */
  entry(id: string, idx: number, options: RequestOptions = {}): Promise<Entry> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/{id}/entries/{idx}", {
          params: { path: { id, idx } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * The run's journal as a stream: the committed entries from `from`, then, with `follow`, each
   * new entry as it is committed, and `end` when the run has ended. Payload values above 256 KiB
   * are left out. With `follow`, a dropped connection is resumed after the last entry received.
   */
  async *events(id: string, params: EventsParams = {}): AsyncGenerator<StreamEvent> {
    const { signal, follow = false } = params;
    const maxReconnects = params.maxReconnects ?? 5;
    let next = params.from ?? 0;
    let failures = 0;
    for (;;) {
      throwIfAborted(signal);
      try {
        // Inside the try: a server that cannot be reached for a while (a restart) is reconnected
        // to as a dropped stream is, within `maxReconnects`.
        const body = await this.transport.send(
          () =>
            this.transport.api.GET("/v1/runs/{id}/events", {
              params: { path: { id }, query: { follow: follow || undefined, from: next } },
              headers: { Accept: "text/event-stream" },
              parseAs: "stream",
              signal,
            }),
          { retry: true, signal },
        );
        if (body === null) {
          throw new RewindConnectionError(`the event stream of run ${id} has no body`);
        }
        const messages = body.pipeThrough(new TextDecoderStream()).pipeThrough(new EventSourceParserStream());
        for await (const message of messages) {
          const event = JSON.parse(message.data) as StreamEvent;
          if (event.type === "entry") {
            next = event.idx + 1;
          }
          failures = 0;
          yield event;
          if (event.type === "end") {
            return;
          }
        }
      } catch (error) {
        throwIfAborted(signal);
        if (!follow || failures >= maxReconnects || !reconnectable(error)) {
          throw error instanceof RewindError
            ? error
            : new RewindConnectionError(`the event stream of run ${id} broke: ${messageOf(error)}`, { cause: error });
        }
      }
      if (!follow) {
        throw new RewindConnectionError(`the event stream of run ${id} ended before its end event`);
      }
      if (failures >= maxReconnects) {
        throw new RewindConnectionError(`the event stream of run ${id} closed ${failures + 1} times in a row`);
      }
      await sleep(backoff(failures), signal);
      failures += 1;
    }
  }

  /** Exports the journal document, or a cassette, of the stored format v1. */
  journal(
    id: string,
    options: RequestOptions & { as?: "journal" | "cassette" | undefined } = {},
  ): Promise<Record<string, unknown>> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/{id}/journal", {
          params: { path: { id }, query: { as: options.as } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /** Takes a run out of quarantine: it is activated again. */
  release(id: string, options: RequestOptions = {}): Promise<Run> {
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/runs/{id}/release", {
          params: { path: { id } },
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }

  /** Gives up a quarantined run: the host ends it (`run_aborted`). */
  giveUp(id: string, options: RequestOptions = {}): Promise<Run> {
    return this.transport.send(
      () =>
        this.transport.api.POST("/v1/runs/{id}/give-up", {
          params: { path: { id } },
          signal: options.signal,
        }),
      { retry: false, signal: options.signal },
    );
  }

  /** Sets or renews the run's own credentials. Its calls parked by `credentials.expired` go on. */
  async setCredentials(id: string, credentials: Credentials, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.PUT("/v1/runs/{id}/credentials", {
          params: { path: { id } },
          body: credentials,
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /** Erases a run: its journal, archive, read model rows and payload references. */
  async delete(id: string, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () => this.transport.api.DELETE("/v1/runs/{id}", { params: { path: { id } }, signal: options.signal }),
      { retry: true, signal: options.signal },
    );
  }

  private fetchRun(id: string, wait: string | undefined, signal: AbortSignal | undefined): Promise<Run> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/{id}", {
          params: { path: { id }, query: { wait } },
          signal,
        }),
      { retry: true, signal },
    );
  }

  /**
   * Asks again, one bounded request at a time, until the run ends or the deadline passes. A
   * server that answers before the wait it was asked for is asked again at most once a second.
   */
  private async waitUntil(
    run: Run,
    asked: number,
    deadline: number | undefined,
    signal: AbortSignal | undefined,
  ): Promise<Run> {
    let current = run;
    let askedAt = asked;
    while (deadline !== undefined && !isTerminal(current.status)) {
      const early = MIN_REPOLL_MS - (Date.now() - askedAt);
      if (early > 0) {
        await sleep(Math.min(early, Math.max(0, deadline - Date.now())), signal);
      }
      const wait = requestWait(deadline);
      if (wait === undefined) {
        break;
      }
      askedAt = Date.now();
      current = await this.fetchRun(current.id, wait, signal);
    }
    return current;
  }
}

/** A failure a new connection may get past: anything but a problem the API answered, unless a 5xx or a 429. */
function reconnectable(error: unknown): boolean {
  return !(error instanceof RewindApiError) || error.status >= 500 || error.status === 429;
}

function deadlineOf(wait: Duration | undefined, now: number): number | undefined {
  return wait === undefined ? undefined : now + toMillis(wait);
}

/** What the first request waits: all of `wait`, up to `MAX_REQUEST_WAIT_MS`. */
function firstWait(wait: Duration | undefined): string | undefined {
  return wait === undefined ? undefined : durationParam(Math.min(toMillis(wait), MAX_REQUEST_WAIT_MS));
}

/** What a later request waits: the time left, up to `MAX_REQUEST_WAIT_MS`; none once it is (almost) past. */
function requestWait(deadline: number): string | undefined {
  const left = deadline - Date.now();
  return left < 10 ? undefined : durationParam(Math.min(left, MAX_REQUEST_WAIT_MS));
}
