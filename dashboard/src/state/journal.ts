// A run's journal, read from its event stream (`sources/runEvents.ts`) into a collection of its
// entries, by index, and kept up to date while the run goes on. Entries are applied at most every
// 80 ms, so a long history loads in a few renders. A journal kept after its page closed shows at
// once when the page opens again, and its stream resumes from the next entry.
import type { Entry, RunStatus } from "../model/types";
import { followRun, type StreamState } from "../sources/runEvents";
import { type WritableCollection, writableCollection } from "./collections";
import { Kept } from "./kept";
import type { Store } from "./store";

export type JournalState = StreamState | "ended" | "error";

export interface JournalSnapshot {
  entries: Entry[];
  state: JournalState;
  /** The status the stream ended with. */
  endStatus: RunStatus | null;
  error: unknown;
}

const FLUSH_MS = 80;

export class Journal extends Kept {
  private snapshot: JournalSnapshot = {
    entries: [],
    state: "connecting",
    endStatus: null,
    error: null,
  };
  private readonly entries: WritableCollection<Entry>;
  private controller: AbortController | null = null;
  private pending: Entry[] = [];
  private flushTimer: ReturnType<typeof setTimeout> | undefined;

  constructor(
    private readonly store: Store,
    readonly runId: string,
    gone: () => void,
  ) {
    super(gone);
    this.entries = writableCollection<Entry>(`journal:${runId}`, (e) => String(e.idx));
  }

  getSnapshot = (): JournalSnapshot => this.snapshot;

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
    void this.entries.collection.cleanup();
  }

  private set(change: Partial<JournalSnapshot>): void {
    this.snapshot = { ...this.snapshot, ...change };
    this.notify();
  }

  private flush = (): void => {
    clearTimeout(this.flushTimer);
    this.flushTimer = undefined;
    const batch = this.pending;
    if (batch.length === 0) return;
    this.pending = [];
    this.entries.upsert(batch);
    this.set({ entries: this.snapshot.entries.concat(batch) });
    // New entries change the run's state: its details are read again.
    this.store.runTouched(this.runId);
  };

  private follow(): void {
    const controller = new AbortController();
    this.controller = controller;
    followRun(
      this.store.api,
      this.runId,
      {
        onEntries: (batch) => {
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
      { from: this.snapshot.entries.length + this.pending.length },
    ).catch((error: unknown) => {
      if (controller.signal.aborted) return;
      this.controller = null;
      this.set({ state: "error", error });
    });
  }
}
