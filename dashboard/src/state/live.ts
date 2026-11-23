// What is on screen, and when it is read again (archi/05, "Live"): from the change stream only,
// never on a timer. The tab follows the organization through one change stream: a `run` event
// updates the run everywhere at once, without a request; the searches on screen are read again a
// second after the last change of a run (at most 5 s after the first of a burst), their counts and
// charts at most every 10 s, a run's details when that run changes. A script, a workflow, a secret,
// a namespace or a connection that changes reads again the views that show it. While the stream is
// down, the views keep what they show; a reopened stream, and a `resync`, read once everything on
// screen.
import type { ControlChange, RunSummary } from "../model/types";
import { type ChangeStream, type ChangeStreamOptions, followChanges } from "../sources/changes";
import type { Api } from "../sources/client";

/** `idle` until a view that follows the organization is shown: the stream opens then. */
export type ConnectionState = "idle" | "connecting" | "live" | "reconnecting";

/** Something on screen that the server's changes may make stale. */
export interface Watched {
  /**
   * `search`: a search's runs, read again after a run's change; `aggregate`: a count or a chart,
   * at most every 10 s after a run's change; `detail`: a run's details, when that run changes;
   * `control`: what no run's change touches (the workflows, the organization's lists), read again
   * only for the control-plane changes it shows.
   */
  readonly kind: "search" | "aggregate" | "detail" | "control";
  /** Reads it again; a read already on its way is followed by one more. */
  refresh(): void;
  /** A run's details: whether a change of `runId` makes them stale. */
  touches?(runId: string): boolean;
  /** Whether it shows what a control-plane change changed. */
  controls?(change: ControlChange): boolean;
}

export interface LiveTimings {
  /** Searches are read again this long after the last change… */
  searchDebounceMs: number;
  /** …and at the latest this long after the first of a burst. */
  searchMaxWaitMs: number;
  /** Counts and charts are read again at most this often while changes arrive. */
  aggregateEveryMs: number;
}

export const LIVE_TIMINGS: LiveTimings = {
  searchDebounceMs: 1_000,
  searchMaxWaitMs: 5_000,
  aggregateEveryMs: 10_000,
};

export type FollowChanges = typeof followChanges;

export interface LiveOptions {
  timings?: Partial<LiveTimings>;
  /** Opens the change stream: `followChanges` by default. */
  follow?: FollowChanges;
  stream?: ChangeStreamOptions;
}

/** What the store does with the changes, beside the values it watches. */
export interface LiveTargets {
  /** Applies a run's new summary to what the tab holds. */
  apply(run: RunSummary): void;
  /** A control-plane change: the reads outside the watched values that show it. */
  control?(change: ControlChange): void;
  /** The counts are due: the reads outside the watched values that move with the runs. */
  aggregates?(): void;
  /** A reopened stream or a `resync`: the reads outside the watched values, once. */
  reread?(): void;
}

export class Live {
  private state: ConnectionState = "idle";
  private readonly listeners = new Set<() => void>();
  private readonly watched = new Set<Watched>();
  private stream: ChangeStream | null = null;
  private searchTimer: ReturnType<typeof setTimeout> | undefined;
  private burstFrom: number | null = null;
  private aggregateTimer: ReturnType<typeof setTimeout> | undefined;
  private lastAggregateAt = 0;
  private disposed = false;
  private readonly timings: LiveTimings;
  private readonly follow: FollowChanges;

  constructor(
    private readonly api: Api,
    private readonly targets: LiveTargets,
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

  /**
   * Shows `item` on screen until the answer is called. `open`: it follows the organization, and
   * opens the stream if it is not; otherwise it is only read again when the stream, opened by
   * another view, reopens or resyncs.
   */
  watch(item: Watched, open = true): () => void {
    this.watched.add(item);
    if (open) this.start();
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
          if (reopened) this.rereadAll();
        },
        onRun: (run) => {
          this.targets.apply(run);
          this.changed(run.id);
        },
        onControl: (change) => this.controlChanged(change),
        onResync: () => this.rereadAll(),
        onState: (state) => this.setState(state),
      },
      this.options.stream,
    );
  }

  /** Opens the stream again at once: the token it was opened with was replaced. */
  reopen(): void {
    this.stream?.reopen();
  }

  dispose(): void {
    this.disposed = true;
    this.stream?.close();
    this.stream = null;
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

  /** Everything on screen, once. */
  private rereadAll(): void {
    this.refresh(() => true);
    this.targets.reread?.();
  }

  /** A run changed: the searches a second later, the counts within 10 s, its details now. */
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
          this.targets.aggregates?.();
        },
        Math.max(0, this.lastAggregateAt + t.aggregateEveryMs - now),
      );
    }
    this.refresh((item) => item.kind === "detail" && (item.touches?.(runId) ?? false));
  }

  /** A control-plane change: what shows it, now. */
  private controlChanged(change: ControlChange): void {
    this.refresh((item) => item.controls?.(change) ?? false);
    this.targets.control?.(change);
  }
}
