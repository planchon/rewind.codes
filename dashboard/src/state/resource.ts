// One value read from the server and kept in a query collection of one row: a count, a chart's
// series, the failure groups, a run's details. The collection compares each answer with the row
// it holds: an equal answer changes nothing, and the value keeps its object.
import { createCollection } from "@tanstack/db";
import { queryCollectionOptions } from "@tanstack/query-db-collection";
import type { ControlChange } from "../model/types";
import { collectionId, Kept, withRetry } from "./kept";
import type { Watched } from "./live";
import type { Store } from "./store";

export interface ResourceInit<T> {
  /**
   * `aggregate`: read again within 10 s of a run's change; `detail`: when its run changes;
   * `control`: only for the control-plane changes it shows (`controls`), which no run's change
   * moves (the workflows); `value`: only when its key changes, or asked. Every kind but `value` is
   * read once again when the change stream reopens, or resyncs.
   */
  kind: "aggregate" | "detail" | "control" | "value";
  load: (signal: AbortSignal) => Promise<T>;
  /** For a run's details: the run. */
  runId?: string;
  /** Whether a control-plane change touches what it shows: it is then read again at once. */
  controls?: (change: ControlChange) => boolean;
}

export type ReadStatus = "pending" | "ready" | "error";

export interface ResourceSnapshot<T> {
  status: ReadStatus;
  data: T | undefined;
  /** The last read's failure; with `ready`, the value shown is the one read before it. */
  error: unknown;
}

interface Row<T> {
  id: "value";
  value: T;
}

export class Resource<T> extends Kept implements Watched {
  readonly kind: "aggregate" | "detail" | "control";
  readonly runId: string | null;
  private readonly controlled: ((change: ControlChange) => boolean) | null;
  lastReadAt = 0;
  private load: (signal: AbortSignal) => Promise<T>;
  private inflight = 0;
  private again = false;
  private error: unknown = null;
  /** The value last read: what stays when a read fails, whatever a change shows over it. */
  private lastValue: { value: T } | null = null;
  private snapshot: ResourceSnapshot<T> = { status: "pending", data: undefined, error: null };
  private readonly collection;
  private readonly unsubscribe: () => void;
  private unwatch: (() => void) | null = null;
  private readonly live: boolean;

  constructor(
    private readonly store: Store,
    key: string,
    init: ResourceInit<T>,
    gone: () => void,
  ) {
    super(gone);
    this.kind = init.kind === "value" ? "control" : init.kind;
    this.live = init.kind !== "value";
    this.runId = init.runId ?? null;
    this.controlled = init.controls ?? null;
    this.load = init.load;
    this.collection = createCollection(
      queryCollectionOptions<Row<T>>({
        id: collectionId(`resource:${key}`),
        queryKey: ["state", "resource", key],
        queryClient: store.queryClient,
        getKey: (row) => row.id,
        gcTime: 0,
        startSync: true,
        staleTime: Number.POSITIVE_INFINITY,
        retry: false,
        refetchOnWindowFocus: false,
        refetchOnReconnect: false,
        queryFn: ({ signal }) => this.read(signal),
      }),
    );
    const subscription = this.collection.subscribeChanges(() => this.publish(), {
      includeInitialState: true,
    });
    this.unsubscribe = () => subscription.unsubscribe();
  }

  /** The latest way to read it: a query that names the time it is read at. */
  setLoad(load: (signal: AbortSignal) => Promise<T>): void {
    this.load = load;
  }

  getSnapshot = (): ResourceSnapshot<T> => this.snapshot;

  touches(runId: string): boolean {
    return this.runId === runId;
  }

  controls(change: ControlChange): boolean {
    return this.controlled?.(change) ?? false;
  }

  refresh = (): void => {
    if (this.disposed) return;
    if (this.inflight > 0) {
      this.again = true;
      return;
    }
    this.collection.utils.refetch().catch(() => undefined);
  };

  /** Reads it again, and answers once the read is over. */
  async refreshNow(): Promise<void> {
    if (this.disposed) return;
    await this.collection.utils.refetch().catch(() => undefined);
  }

  /**
   * Shows `change` of the value over what was read, inside the transaction of a write
   * (`actions.ts`): until the transaction ends, undone if it is rolled back. Nothing when no value
   * was read yet, or when the change keeps the value.
   */
  change(change: (value: T) => T): void {
    if (this.disposed) return;
    const held = this.current();
    if (held === undefined) return;
    const next = change(held);
    if (next === held) return;
    this.collection.update("value", (draft) => {
      (draft as Row<T>).value = next;
    });
  }

  protected override onRetained(): void {
    if (this.live) this.unwatch = this.store.live.watch(this);
    // Back on the page: what it shows is read again, unless it was just read.
    if (this.snapshot.status !== "pending" && Date.now() - this.lastReadAt > 2_000) this.refresh();
  }

  protected override onReleased(): void {
    this.unwatch?.();
    this.unwatch = null;
  }

  override dispose(): void {
    super.dispose();
    this.unwatch?.();
    this.unsubscribe();
    void this.collection.cleanup();
  }

  private current(): T | undefined {
    return (this.collection.get("value") as Row<T> | undefined)?.value;
  }

  private async read(signal: AbortSignal): Promise<Row<T>[]> {
    this.lastReadAt = Date.now();
    this.inflight += 1;
    try {
      const value = await withRetry(() => this.load(signal), signal);
      this.lastValue = { value };
      if (this.error !== null) {
        this.error = null;
        queueMicrotask(() => this.publish());
      }
      return [{ id: "value", value }];
    } catch (error) {
      if (signal.aborted) throw error;
      this.error = error;
      this.store.failed(error);
      this.publish();
      // The value read before stays (not a change shown over it, which a refusal may undo).
      const held = this.lastValue;
      return held === null ? [] : [{ id: "value", value: held.value }];
    } finally {
      this.inflight -= 1;
      if (this.again) {
        this.again = false;
        setTimeout(this.refresh, 0);
      }
    }
  }

  private publish(): void {
    if (this.disposed) return;
    const data = this.current();
    const status: ReadStatus =
      data !== undefined ? "ready" : this.error !== null ? "error" : "pending";
    const s = this.snapshot;
    if (s.status === status && s.data === data && s.error === this.error) return;
    this.snapshot = { status, data, error: this.error };
    this.notify();
  }
}
