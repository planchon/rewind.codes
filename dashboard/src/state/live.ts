// What is on screen, and when it is read again (archi/05, "Live"). The tab follows the
// organization's runs through one change stream: a `run` event updates the run everywhere at once,
// without a request; the searches on screen are read again a second after the last change, their
// counts and charts at most every 5 s. While the stream is open, what is on screen is also read
// again every 60 s; while it is down (or not open yet), each view reads at its former rate (10 to
// 30 s). A reopened stream, and a `resync`, read again everything on screen.
import type { RunSummary } from "../model/types";
import { type ChangeStream, type ChangeStreamOptions, followChanges } from "../sources/changes";
import type { Api } from "../sources/client";

/** `idle` until a view that follows the runs is shown: the stream opens then. */
export type ConnectionState = "idle" | "connecting" | "live" | "reconnecting";

/** Something on screen that the server's changes may make stale. */
export interface Watched {
  /**
   * A search's runs, a count or a chart, a run's details; `polled`: what no run's change touches
   * (the workflows), read on the clock only.
   */
  readonly kind: "search" | "aggregate" | "detail" | "polled";
  /** How often it is read while the stream is down; `null`: only when asked. */
  readonly pollMs: number | null;
  /** When it last asked the server, in ms since the epoch. */
  readonly lastReadAt: number;
  /** Reads it again; a read already on its way is followed by one more. */
  refresh(): void;
  /** A run's details: whether a change of `runId` makes them stale. */
  touches?(runId: string): boolean;
}

export interface LiveTimings {
  /** Searches are read again this long after the last change… */
  searchDebounceMs: number;
  /** …and at the latest this long after the first of a burst. */
  searchMaxWaitMs: number;
  /** Counts and charts are read again at most this often while changes arrive. */
  aggregateEveryMs: number;
  /** What is on screen is read again this often while the stream is open. */
  connectedEveryMs: number;
  /** How often the reads due are looked for. */
  tickMs: number;
}

export const LIVE_TIMINGS: LiveTimings = {
  searchDebounceMs: 1_000,
  searchMaxWaitMs: 5_000,
  aggregateEveryMs: 5_000,
  connectedEveryMs: 60_000,
  tickMs: 1_000,
};

export type FollowChanges = typeof followChanges;

export interface LiveOptions {
  timings?: Partial<LiveTimings>;
  /** Opens the change stream: `followChanges` by default. */
  follow?: FollowChanges;
  stream?: ChangeStreamOptions;
}

export class Live {
  private state: ConnectionState = "idle";
  private readonly listeners = new Set<() => void>();
  private readonly watched = new Set<Watched>();
  private stream: ChangeStream | null = null;
  private ticker: ReturnType<typeof setInterval> | undefined;
  private searchTimer: ReturnType<typeof setTimeout> | undefined;
  private burstFrom: number | null = null;
  private aggregateTimer: ReturnType<typeof setTimeout> | undefined;
  private lastAggregateAt = 0;
  private disposed = false;
  private readonly timings: LiveTimings;
  private readonly follow: FollowChanges;

  constructor(
    private readonly api: Api,
    /** Applies a run's new summary to what the tab holds. */
    private readonly apply: (run: RunSummary) => void,
    private readonly options: LiveOptions = {},
  ) {
    this.timings = { ...LIVE_TIMINGS, ...options.timings };
    this.follow = options.follow ?? followChanges;
  }

  getState = (): ConnectionState => this.state;

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  /** Shows `item` on screen until the answer is called; opens the stream if it is not. */
  watch(item: Watched): () => void {
    this.watched.add(item);
    this.start();
    return () => {
      this.watched.delete(item);
    };
  }

  /** Opens the change stream, once. */
  start(): void {
    if (this.stream || this.disposed) return;
    this.stream = this.follow(
      this.api,
      {
        onOpen: (reopened) => {
          // The first opening follows the first reads of what is shown; a later one may have
          // missed changes.
          if (reopened) this.refresh(() => true);
        },
        onRun: (run) => {
          this.apply(run);
          this.changed(run.id);
        },
        onResync: () => this.refresh(() => true),
        onState: (state) => this.setState(state),
      },
      this.options.stream,
    );
    this.ticker = setInterval(() => this.tick(), this.timings.tickMs);
  }

  /** Opens the stream again at once: the token it was opened with was replaced. */
  reopen(): void {
    this.stream?.reopen();
  }

  dispose(): void {
    this.disposed = true;
    this.stream?.close();
    this.stream = null;
    clearInterval(this.ticker);
    clearTimeout(this.searchTimer);
    clearTimeout(this.aggregateTimer);
    this.watched.clear();
    this.listeners.clear();
  }

  private setState(state: ConnectionState): void {
    if (this.state === state) return;
    this.state = state;
    for (const listener of [...this.listeners]) listener();
  }

  private refresh(which: (item: Watched) => boolean): void {
    for (const item of [...this.watched]) if (which(item)) item.refresh();
  }

  /** A run changed: the searches a second later, the counts within 5 s, its details now. */
  private changed(runId: string): void {
    const now = Date.now();
    const t = this.timings;
    this.burstFrom ??= now;
    clearTimeout(this.searchTimer);
    const due = Math.min(now + t.searchDebounceMs, this.burstFrom + t.searchMaxWaitMs);
    this.searchTimer = setTimeout(
      () => {
        this.searchTimer = undefined;
        this.burstFrom = null;
        this.refresh((item) => item.kind === "search");
      },
      Math.max(0, due - now),
    );
    if (this.aggregateTimer === undefined) {
      this.aggregateTimer = setTimeout(
        () => {
          this.aggregateTimer = undefined;
          this.lastAggregateAt = Date.now();
          this.refresh((item) => item.kind === "aggregate");
        },
        Math.max(0, this.lastAggregateAt + t.aggregateEveryMs - now),
      );
    }
    this.refresh((item) => item.kind === "detail" && (item.touches?.(runId) ?? false));
  }

  /** Reads what is due: every 60 s while the stream is open, at each view's rate otherwise. */
  private tick(): void {
    if (typeof document !== "undefined" && document.hidden) return;
    const now = Date.now();
    const open = this.state === "live";
    for (const item of [...this.watched]) {
      const every = open ? this.timings.connectedEveryMs : item.pollMs;
      if (every !== null && now - item.lastReadAt >= every) item.refresh();
    }
  }
}
