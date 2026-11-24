import { durationParam, toMillis } from "../duration.js";
import { RewindApiError, RewindConnectionError, RewindError } from "../errors.js";
import { backoff, checkIdempotencyKey, messageOf, sleep, throwIfAborted, type Transport } from "../http.js";
import { labelParams, timeParam, type RequestOptions } from "../params.js";
import { sseMessages } from "../sse.js";
import type {
  Duration,
  FingerprintPage,
  HistoryEvent,
  Labels,
  Run,
  RunPage,
  RunStats,
  RunStatus,
  RunSummary,
  SignalAccepted,
  StreamMessage,
  Transcript,
} from "../types.js";

/** How long after a wait's deadline the client gives up a request the server has not answered. */
const ANSWER_GRACE_MS = 10_000;

/** A request held at least this long before its connection broke was cut: it is sent again at once. */
const CUT_AFTER_MS = 1_000;

/** The longest delay of a timer; a deadline further away is the server's alone. */
const MAX_TIMER_MS = 2 ** 31 - 1;

/**
 * What a run executes: code and its action set. The run is a run of the workflow that already has this code in the
 * namespace, or among the organization's workflows; else the start creates a workflow of the namespace, with this code
 * as its version 1.
 */
export interface CodeTarget {
  /** The TypeScript source: a single ES module whose default export is the entry point. */
  code: string;
  /** The action set the code imports from `rewind:actions`. Without one, `rewind:actions` is empty. */
  actionSet?: string | undefined;
  workflow?: undefined;
}

/** What a run executes: a version of a workflow, with its code, its action set and its secrets. */
export interface WorkflowTarget {
  /**
   * The workflow, by its id or its name (the namespace's first, then the organization's): its latest version, or
   * `triage@v12` (its number) or `triage@<digest>` (12 hex digits at least) for one version. The input is checked
   * against its input schema (`invalid-input`).
   */
  workflow: string;
  code?: undefined;
  actionSet?: undefined;
}

/** `runs.start`: code and its action set, or a workflow; and how the run starts. */
export type StartRunParams = (CodeTarget | WorkflowTarget) & StartRunOptions;

export interface StartRunOptions extends RequestOptions {
  /** The input of `main`. `{}` when absent. */
  input?: unknown;
  /**
   * Wait up to this long for the run to end: the server holds the one request until the run ends or is quarantined,
   * or until `wait` passes. The run is returned as it is then: ended, or still active with what it waits for.
   */
  wait?: Duration | undefined;
  /**
   * A request repeated with the same key returns the run the first one started. With `wait` and no key, the client
   * draws one, so that a connection cut while the server holds the request is sent again safely.
   */
  idempotencyKey?: string | undefined;
  /** Run metadata: filters runs, scopes `list_runs`, API keys and webhooks. */
  labels?: Labels | undefined;
  /** W3C trace context of the caller: the run appears under it in the caller's trace. */
  traceparent?: string | undefined;
  tracestate?: string | undefined;
  /** The run's time to live: it is cancelled after it. 30 days by default. */
  ttl?: Duration | undefined;
  /** Type-check the code before starting it. `true` by default. A workflow's version is not checked again. */
  check?: boolean | undefined;
  /**
   * The secrets the run reads, by MCP server or API: `{ github: "ns/github", zendesk: "org/zendesk" }`
   * (`org/<name>`, the organization's; `ns/<name>`, the run's namespace's). Names, never values. Required for each
   * server the code imports whose `auth` takes a secret; a run of a workflow gives none, its version's are read. A
   * secret missing, unknown or not the caller's to use refuses the start before any run exists: `secret-required`,
   * whose `secrets` say which and what the caller may use instead (`usable`).
   */
  secrets?: Record<string, string> | undefined;
}

export interface ForkRunParams extends RequestOptions {
  /** The corrected TypeScript source. */
  code: string;
  /** Go live from this event, even if the commands still match. */
  at?: number | undefined;
  /** The fork's input. The source's when absent. */
  input?: unknown;
  labels?: Labels | undefined;
  wait?: Duration | undefined;
  idempotencyKey?: string | undefined;
  traceparent?: string | undefined;
  tracestate?: string | undefined;
  ttl?: Duration | undefined;
  /**
   * The fork's secrets, by MCP server or API, as `runs.start` takes them. The source's when absent; either way they
   * are checked against the caller, since the code is new code.
   */
  secrets?: Record<string, string> | undefined;
  /** Type-check the corrected code first. `true` by default. */
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
  /**
   * `started` (the default: the most recently started first), `oldest` (the first started
   * first), `activity` (the most recent activity first), `wakes` (the soonest to wake by itself
   * first), `duration` (the longest first, the runs that have not ended last), or `calls` (the
   * most calls first).
   */
  sort?: "started" | "oldest" | "activity" | "wakes" | "duration" | "calls" | undefined;
  /**
   * Only runs of this status. The runs that have not ended, and what a waiting run waits for, are terms of `q`:
   * `status:active`, `waiting:approval`.
   */
  status?: RunStatus | undefined;
  /** Only quarantined runs (`waiting:quarantined`), or only runs that are not. */
  quarantined?: boolean | undefined;
  /** Only runs carrying all these labels. */
  labels?: Labels | undefined;
  /** Only runs that called this action. */
  action?: string | undefined;
  /** Only runs whose calls failed with this kind, or that failed with this error name. */
  error?: string | undefined;
  /** Only forks of this run: its handle or its id. */
  source?: string | undefined;
  /** Only runs of this workflow: its id or its name. */
  workflow?: string | undefined;
  /** Only runs this run started (`triage.start(…)` in its code): its handle or its id. */
  startedBy?: string | undefined;
  startedAfter?: Date | string | undefined;
  startedBefore?: Date | string | undefined;
  /**
   * At most this many runs (50 by default, 500 at most). A page may hold fewer, or none, and still have a
   * `nextCursor`: the server stopped at its scan budget. Only a page without `nextCursor` is the last.
   */
  limit?: number | undefined;
  cursor?: string | undefined;
}

export interface RunStatsParams extends RequestOptions {
  /**
   * The scope: one term `namespace:<name>`, or nothing for every namespace the client reaches (the client's own
   * namespace, or an end user's token, narrows it too). Any other term is refused with `invalid-query`: statistics
   * come from hourly counters, by namespace and window only.
   */
  q?: string | undefined;
  /** Runs started at or after this time, rounded down to the hour. No lower bound by default. */
  since?: Date | string | undefined;
  /** Runs started before this time, rounded up to the hour. Now by default. */
  until?: Date | string | undefined;
  /**
   * The width of the series' intervals, from `since`, which it requires: whole hours, a shorter width is rounded up. No
   * series without it.
   */
  interval?: Duration | undefined;
  /** Values per facet (10 by default, 50 at most). */
  top?: number | undefined;
}

export interface FailuresParams extends RequestOptions {
  /** The scope, as `stats` takes it: one term `namespace:<name>`, or nothing. */
  q?: string | undefined;
  /** Runs ended at or after this time, rounded down to the hour. No lower bound by default. */
  since?: Date | string | undefined;
  /** Runs ended before this time, rounded up to the hour. Now by default. */
  until?: Date | string | undefined;
  /** The width of each fingerprint's series' intervals, from `since`, which it requires: whole hours. */
  interval?: Duration | undefined;
  /** Fingerprints at most (50 by default, 500 at most). */
  limit?: number | undefined;
}

export interface EventsParams extends RequestOptions {
  /** Keep the stream open and send events as they are committed, until the run ends. */
  follow?: boolean | undefined;
  /** The first event to send, by its position. 0 by default. */
  from?: number | undefined;
  /** With `follow`: how many times to reconnect after the connection drops. 5 by default. */
  maxReconnects?: number | undefined;
}

/**
 * Runs: start, follow, steer and read them. A client bound to a namespace, or holding an end user's token, reaches
 * that namespace's runs only; a key's client without one reads every namespace of the organization.
 *
 * Every method that takes a run's `id` takes its handle too (`Run.handle`, `dana/triage/06bqx3k9f2mt`), which is
 * percent-encoded in the path.
 */
export class Runs {
  constructor(private readonly transport: Transport) {}

  /**
   * Starts a run of code, or of a workflow, in the client's namespace: the end user it acts for
   * (`rewind.namespace(name)`, or an end user's token). A key's client without a namespace is refused
   * (`NamespaceRequiredError`). Code is compiled, checked and type-checked first: code that cannot run is refused with
   * `invalid-code` (a `RewindApiError` with `diagnostics`) before any run exists. A workflow's input that does not match
   * its schema is refused with `invalid-input`. With `wait`, the server holds the one request until the run ends or is
   * quarantined, or until `wait` passes, whichever comes first. The run's `workflow` names the workflow it is a run
   * of, and its version.
   */
  async start(params: StartRunParams): Promise<Run> {
    const { signal, idempotencyKey, wait, ...body } = params;
    checkIdempotencyKey(idempotencyKey);
    const send = (key: string | undefined, held: string | undefined, sendSignal: AbortSignal | undefined) =>
      this.transport.send(
        () =>
          this.transport.api.POST("/v1/runs", {
            params: { header: { "Idempotency-Key": key, ...this.transport.namespaceHeader } },
            body: { ...body, wait: held },
            signal: sendSignal,
          }),
        { retry: key !== undefined && wait === undefined, createsInNamespace: true, signal: sendSignal },
      );
    if (wait === undefined) {
      return send(idempotencyKey, undefined, signal);
    }
    // The key makes a request sent again after a cut answer the same run.
    const key = idempotencyKey ?? globalThis.crypto.randomUUID();
    const start = (held: string | undefined, sendSignal: AbortSignal | undefined) => send(key, held, sendSignal);
    return this.held(wait, signal, start, start);
  }

  /** A run's state. With `wait`, the server holds the one request until the run ends or `wait` passes. */
  async get(id: string, options: RequestOptions & { wait?: Duration | undefined } = {}): Promise<Run> {
    const { signal, wait } = options;
    if (wait === undefined) {
      return this.fetchRun(id, undefined, signal, true);
    }
    const read = (held: string | undefined, sendSignal: AbortSignal | undefined) =>
      this.fetchRun(id, held, sendSignal, false);
    return this.held(wait, signal, read, read);
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
              action: params.action,
              error: params.error,
              quarantined: params.quarantined,
              source: params.source,
              workflow: params.workflow,
              startedBy: params.startedBy,
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
              interval: params.interval === undefined ? undefined : durationParam(params.interval),
              top: params.top,
            },
          },
          signal,
        }),
      { retry: true, signal },
    );
  }

  /** The failed, cancelled and aborted runs a query matches, grouped by fingerprint. */
  failures(params: FailuresParams = {}): Promise<FingerprintPage> {
    const { signal } = params;
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/failures", {
          params: {
            query: {
              q: params.q,
              since: timeParam(params.since),
              until: timeParam(params.until),
              interval: params.interval === undefined ? undefined : durationParam(params.interval),
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
   * Sends a signal: it resolves the code's `signal(name)`, now or the next time the code
   * waits for that name. With an idempotency key, the same key sent twice delivers one signal.
   * A run that has ended refuses it (`run-ended`). A name starting with `rewind:` is refused with
   * `invalid-signal`, and so are, for a run of a workflow, a signal it does not declare and a
   * payload that does not match its schema.
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
   * Cancels a run: every pending await of the code rejects with `CancelledError`, and the
   * code gets one grace step to clean up. With `wait`, returns the run's state after it, waiting
   * as `start` does.
   */
  async cancel(
    id: string,
    options: RequestOptions & { reason?: string | undefined; wait?: Duration | undefined } = {},
  ): Promise<Run> {
    const { signal, reason, wait } = options;
    const cancel = (held: string | undefined, sendSignal: AbortSignal | undefined) =>
      this.transport.send(
        () =>
          this.transport.api.POST("/v1/runs/{id}/cancel", {
            params: { path: { id } },
            body: { reason, wait: held },
            signal: sendSignal,
          }),
        { retry: false, signal: sendSignal },
      );
    if (wait === undefined) {
      return cancel(undefined, signal);
    }
    // Once the cancellation was sent, a cut connection waits through a read of the run.
    const read = (held: string | undefined, sendSignal: AbortSignal | undefined) =>
      this.fetchRun(id, held, sendSignal, false);
    return this.held(wait, signal, cancel, read);
  }

  /**
   * Forks a run with corrected code (`retry_run`): a new run, in its source's namespace, that reuses the source's
   * recorded results while its commands match, and continues live at the first difference, at `at`, or at the end of
   * the source's history. Its `source` names the run. For a run of a workflow, the suite of the run's version runs
   * against the corrected code first: if a test does not pass, no fork is created, and the refusal is `tests-failed`,
   * whose `tests` holds each test's outcome. The corrected code does not become a version of the workflow
   * (`workflows.createVersion` makes one).
   */
  async fork(id: string, params: ForkRunParams): Promise<Run> {
    const { signal, idempotencyKey, wait, ...body } = params;
    checkIdempotencyKey(idempotencyKey);
    const send = (key: string | undefined, held: string | undefined, sendSignal: AbortSignal | undefined) =>
      this.transport.send(
        () =>
          this.transport.api.POST("/v1/runs/{id}/fork", {
            params: { path: { id }, header: { "Idempotency-Key": key } },
            body: { ...body, wait: held },
            signal: sendSignal,
          }),
        { retry: key !== undefined && wait === undefined, signal: sendSignal },
      );
    if (wait === undefined) {
      return send(idempotencyKey, undefined, signal);
    }
    const key = idempotencyKey ?? globalThis.crypto.randomUUID();
    const fork = (held: string | undefined, sendSignal: AbortSignal | undefined) => send(key, held, sendSignal);
    return this.held(wait, signal, fork, fork);
  }

  /** The run's transcript: the run as text for a model, a header then one line per event. */
  transcript(id: string, options: RequestOptions = {}): Promise<Transcript> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/{id}/transcript", {
          params: { path: { id } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /** One event of the run's history in full, by its position: every payload of the event has its value, whatever its size. */
  event(id: string, idx: number, options: RequestOptions = {}): Promise<HistoryEvent> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/{id}/events/{idx}", {
          params: { path: { id, idx } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /**
   * The run's history as a stream: the committed events from `from`, then, with `follow`, each
   * new event as it is committed, and `end` when the run has ended. Payload values above 256 KiB
   * are left out. With `follow`, a dropped connection is resumed after the last event received.
   */
  async *events(id: string, params: EventsParams = {}): AsyncGenerator<StreamMessage> {
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
        for await (const message of sseMessages(body, signal)) {
          const event = JSON.parse(message.data) as StreamMessage;
          if (event.type === "event") {
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

  /**
   * Exports the run, as a document of the stored format v1: `run` (the default), the run export, its code, action set
   * and history, which replays on its own; or `cassette`, its calls and signals, for tests.
   */
  export(
    id: string,
    options: RequestOptions & { as?: "run" | "cassette" | undefined } = {},
  ): Promise<Record<string, unknown>> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/{id}/export", {
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

  /** Gives up a quarantined run: Rewind ends it (`run_aborted`). */
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

  /** Erases a run: its history, its archive, its summary and its payloads. */
  async delete(id: string, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () => this.transport.api.DELETE("/v1/runs/{id}", { params: { path: { id } }, signal: options.signal }),
      { retry: true, signal: options.signal },
    );
  }

  private fetchRun(
    id: string,
    wait: string | undefined,
    signal: AbortSignal | undefined,
    retry: boolean,
  ): Promise<Run> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/runs/{id}", {
          params: { path: { id }, query: { wait } },
          signal,
        }),
      { retry, signal },
    );
  }

  /**
   * One request the server holds until the run ends or is quarantined, or until `wait` passes, and what it answered:
   * nothing is asked again once the server has answered. A connection cut while the request is held (by a proxy, or
   * by the HTTP client's own timeout: Node's `fetch` gives up waiting for an answer after 5 minutes) is sent again at
   * once with the time left, through `again`; a failure that comes at once is sent again after a backoff, up to the
   * client's `maxRetries`. A request the server has not answered shortly after the wait's deadline is given up, and
   * the run is read once as it is.
   */
  private async held(
    whole: Duration,
    signal: AbortSignal | undefined,
    first: (wait: string | undefined, signal: AbortSignal | undefined) => Promise<Run>,
    again: (wait: string | undefined, signal: AbortSignal | undefined) => Promise<Run>,
  ): Promise<Run> {
    const deadline = Date.now() + toMillis(whole);
    let request = first;
    let resent = false;
    let failures = 0;
    for (;;) {
      throwIfAborted(signal);
      const left = deadline - Date.now();
      // The first request carries the wait as asked; one sent again, the time left.
      const wait = !resent ? durationParam(whole) : left >= 10 ? durationParam(left) : undefined;
      const sent = new AbortController();
      const abort = () => sent.abort(signal?.reason);
      signal?.addEventListener("abort", abort, { once: true });
      let late = false;
      const giveUp =
        wait !== undefined && left + ANSWER_GRACE_MS <= MAX_TIMER_MS
          ? setTimeout(() => {
              late = true;
              sent.abort(new RewindConnectionError(`the server did not answer within the wait of ${wait}`));
            }, left + ANSWER_GRACE_MS)
          : undefined;
      const sentAt = Date.now();
      try {
        return await request(wait, sent.signal);
      } catch (error) {
        throwIfAborted(signal);
        if (late) {
          // The wait is over: the run as it is now.
          return await again(undefined, signal);
        }
        if (!resendable(error)) {
          throw error;
        }
        if (Date.now() - sentAt < CUT_AFTER_MS || error instanceof RewindApiError) {
          if (failures >= this.transport.maxRetries) {
            throw error;
          }
          await sleep(backoff(failures), signal);
          failures += 1;
        }
        request = again;
        resent = true;
      } finally {
        clearTimeout(giveUp);
        signal?.removeEventListener("abort", abort);
      }
    }
  }
}

/** A failure of a held request that a new request may get past: the connection, or a server that is stopping. */
function resendable(error: unknown): boolean {
  return error instanceof RewindConnectionError || (error instanceof RewindApiError && error.status === 503);
}

/** A failure a new connection may get past: anything but a problem the API answered, unless a 5xx or a 429. */
function reconnectable(error: unknown): boolean {
  return !(error instanceof RewindApiError) || error.status >= 500 || error.status === 429;
}
