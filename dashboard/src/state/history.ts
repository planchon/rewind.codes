// A run's history, read from its event stream (`sources/runEvents.ts`) into a collection of its
// events, by position, and kept up to date while the run goes on. Events are applied at most every
// 80 ms, so a long history loads in a few renders. A history kept after its page closed shows at
// once when the page opens again, and its stream resumes from the next event.
import type { HistoryEvent, RunStatus } from "../model/types";
import { followRun, type StreamState } from "../sources/runEvents";
import { type WritableCollection, writableCollection } from "./collections";
import { Kept } from "./kept";
import type { Store } from "./store";

export type HistoryState = StreamState | "ended" | "error";

export interface HistorySnapshot {
  events: HistoryEvent[];
  state: HistoryState;
  /** The status the stream ended with. */
  endStatus: RunStatus | null;
  error: unknown;
}

const FLUSH_MS = 80;

export class History extends Kept {
  private snapshot: HistorySnapshot = {
    events: [],
    state: "connecting",
    endStatus: null,
    error: null,
  };
  private readonly events: WritableCollection<HistoryEvent>;
  private controller: AbortController | null = null;
  private pending: HistoryEvent[] = [];
  private flushTimer: ReturnType<typeof setTimeout> | undefined;

  constructor(
    private readonly store: Store,
    readonly runId: string,
    gone: () => void,
  ) {
    super(gone);
    this.events = writableCollection<HistoryEvent>(`history:${runId}`, (e) => String(e.idx));
  }

  getSnapshot = (): HistorySnapshot => this.snapshot;

  protected override onRetained(): void {
    if (this.snapshot.state === "ended" || this.controller) return;
    this.follow();
  }

  protected override onReleased(): void {
    this.flush();
    this.controller?.abort();
    this.controller = null;
    if (this.snapshot.state !== "ended" && this.snapshot.state !== "error")
      this.set({ state: "connecting" });
  }

  override dispose(): void {
    super.dispose();
    clearTimeout(this.flushTimer);
    this.controller?.abort();
    this.controller = null;
    void this.events.collection.cleanup();
  }

  private set(change: Partial<HistorySnapshot>): void {
    this.snapshot = { ...this.snapshot, ...change };
    this.notify();
  }

  private flush = (): void => {
    clearTimeout(this.flushTimer);
    this.flushTimer = undefined;
    const batch = this.pending;
    if (batch.length === 0) return;
    this.pending = [];
    this.events.upsert(batch);
    this.set({ events: this.snapshot.events.concat(batch) });
    // New events change the run's state: its details are read again.
    this.store.runTouched(this.runId);
  };

  private follow(): void {
    const controller = new AbortController();
    this.controller = controller;
    followRun(
      this.store.api,
      this.runId,
      {
        onEvents: (batch) => {
          this.pending.push(...batch);
          this.flushTimer ??= setTimeout(this.flush, FLUSH_MS);
        },
        onEnd: (status) => {
          this.flush();
          this.controller = null;
          this.set({ state: "ended", endStatus: status });
          this.store.runTouched(this.runId);
        },
        onState: (state) => {
          if (this.snapshot.state !== state && this.snapshot.state !== "ended") this.set({ state });
        },
      },
      controller.signal,
      { from: this.snapshot.events.length + this.pending.length },
    ).catch((error: unknown) => {
      if (controller.signal.aborted) return;
      this.controller = null;
      this.set({ state: "error", error });
    });
  }
}
