// A list the server answers whole (an organization's members, invitations, roles and keys, a
// person's connections), kept in a query collection of its rows by key. It is read when a view
// first shows it, again when a view comes back to it (unless it was just read), after each write,
// and every few seconds while a view watches it. An equal answer changes nothing, and each row
// keeps its object until it changes; the rows keep the server's order, and a row a write adds
// comes last until the server answers. A failed read keeps the rows read before.
//
// A write whose outcome is known beforehand shows at once, as an optimistic mutation of TanStack
// DB: the rows read after the write replace it, and the server's refusal takes it back.
import { createCollection, createOptimisticAction } from "@tanstack/db";
import { queryCollectionOptions } from "@tanstack/query-db-collection";
import { useEffect, useMemo, useSyncExternalStore } from "react";
import { collectionId, Kept, withRetry } from "./kept";
import { type Read, toRead } from "./read";
import type { ReadStatus, ResourceSnapshot } from "./resource";
import { type Store, useStore } from "./store";

export interface ListInit<T> {
  load: (signal: AbortSignal) => Promise<T[]>;
  /** A row's key: its id, or its name. */
  key: (row: T) => string;
}

/** Within this long of a read, a view that comes back does not read again. */
const FRESH_MS = 2_000;

function sameRows<T>(a: readonly T[] | undefined, b: readonly T[] | undefined): boolean {
  if (a === b) return true;
  if (!a || !b || a.length !== b.length) return false;
  return a.every((row, i) => row === b[i]);
}

/** Something read again after a write: another list, a resource. */
interface Rereadable {
  refreshNow(): Promise<void>;
}

export class KeyedList<T extends object> extends Kept {
  readonly collection;
  /** When the last answer arrived, in ms since the epoch; 0 before the first. */
  answeredAt = 0;
  private lastReadAt = 0;
  private readonly keyOf: (row: T) => string;
  private readonly load: (signal: AbortSignal) => Promise<T[]>;
  /** The server's last answer; `null` before the first. */
  private answer: T[] | null = null;
  private order: string[] = [];
  private error: unknown = null;
  private snapshot: ResourceSnapshot<readonly T[]> = {
    status: "pending",
    data: undefined,
    error: null,
  };
  private reading: Promise<void> | null = null;
  private again = false;
  private readonly unsubscribe: () => void;

  constructor(
    private readonly store: Store,
    key: string,
    init: ListInit<T>,
    gone: () => void,
  ) {
    super(gone);
    this.keyOf = init.key;
    this.load = init.load;
    this.collection = createCollection(
      queryCollectionOptions<T>({
        id: collectionId(`list:${key}`),
        queryKey: ["state", "list", key],
        queryClient: store.queryClient,
        getKey: (row) => this.keyOf(row),
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
    // An empty first answer changes no row.
    const ready = this.collection.onFirstReady(() => this.publish());
    this.unsubscribe = () => {
      subscription.unsubscribe();
      ready();
    };
  }

  getSnapshot = (): ResourceSnapshot<readonly T[]> => this.snapshot;

  /** Reads it again; a read on its way is followed by one more. */
  refresh = (): void => {
    void this.refreshNow();
  };

  /** Reads it again, and answers once the answer is in the rows. */
  async refreshNow(): Promise<void> {
    if (this.disposed) return;
    if (this.reading) {
      this.again = true;
      await this.reading;
      return;
    }
    this.reading = (async () => {
      do {
        this.again = false;
        await this.collection.utils.refetch().catch(() => undefined);
        this.publish();
      } while (this.again && !this.disposed);
    })().finally(() => {
      this.reading = null;
    });
    await this.reading;
  }

  /**
   * Sends a write, and reads the rows again (with `also`, what else it changes) before it counts
   * as done. `apply`, when the outcome is known beforehand, changes the rows at once through the
   * collection's `insert`, `update` and `delete`; a refusal takes the change back, and is thrown.
   */
  async change<R>(
    send: () => Promise<R>,
    apply: ((rows: this["collection"]) => void) | null = null,
    also: readonly Rereadable[] = [],
  ): Promise<R> {
    let answer: R | undefined;
    const persist = async () => {
      answer = await send();
      await Promise.all([this.refreshNow(), ...also.map((r) => r.refreshNow())]);
    };
    const transaction = apply
      ? createOptimisticAction<void>({
          onMutate: () => apply(this.collection),
          mutationFn: persist,
        })()
      : null;
    // A change that touched no row (the row is not held) has nothing to take back.
    if (transaction && transaction.mutations.length > 0) await transaction.when("settled");
    else await persist();
    this.publish();
    return answer as R;
  }

  protected override onRetained(): void {
    // Back on the page: what it shows is read again, unless it was just read.
    if (this.snapshot.status !== "pending" && Date.now() - this.lastReadAt > FRESH_MS) {
      this.refresh();
    }
  }

  override dispose(): void {
    super.dispose();
    this.unsubscribe();
    void this.collection.cleanup();
  }

  private async read(signal: AbortSignal): Promise<T[]> {
    this.lastReadAt = Date.now();
    try {
      const rows = await withRetry(() => this.load(signal), signal);
      this.answer = rows;
      this.order = rows.map(this.keyOf);
      this.error = null;
      this.answeredAt = Date.now();
      return rows;
    } catch (error) {
      if (signal.aborted) throw error;
      this.error = error;
      this.store.failed(error);
      queueMicrotask(() => this.publish());
      // The rows read before stay.
      return this.answer ?? [];
    }
  }

  /** The rows as the collection holds them, optimistic changes included, in the server's order. */
  private rows(): T[] {
    const rows: T[] = [];
    for (const key of this.order) {
      const row = this.collection.get(key);
      if (row !== undefined) rows.push(row);
    }
    const placed = new Set(this.order);
    for (const [key, row] of this.collection.entries()) {
      if (!placed.has(String(key))) rows.push(row);
    }
    return rows;
  }

  private publish(): void {
    if (this.disposed) return;
    const rows = this.answer === null ? undefined : this.rows();
    const status: ReadStatus =
      rows !== undefined ? "ready" : this.error !== null ? "error" : "pending";
    const s = this.snapshot;
    const data = sameRows(s.data, rows) ? s.data : rows;
    if (s.status !== status || s.data !== data || s.error !== this.error) {
      this.snapshot = { status, data, error: this.error };
    }
    // Told even of an equal answer: whoever follows `answeredAt` learns of it.
    this.notify();
  }
}

/** The store's list under `key`, made at its first use. */
export function keyedList<T extends object>(
  store: Store,
  key: string,
  init: (store: Store) => ListInit<T>,
): KeyedList<T> {
  return store.keep(`list:${key}`, (gone) => new KeyedList<T>(store, key, init(store), gone));
}

/** The store's list under `key` if a view made it: what a write reads again, if it is held. */
export function heldList(store: Store, key: string): KeyedList<object> | undefined {
  return store.peek<KeyedList<object>>(`list:${key}`);
}

const PENDING: ResourceSnapshot<never> = { status: "pending", data: undefined, error: null };
const noSubscribe = () => () => {};
const pendingSnapshot = () => PENDING;
const noRefresh = () => Promise.resolve();

/**
 * A list of the store, held while the view shows it; `key` null holds none. `watchMs`: read again
 * this often while the view is shown and the tab visible.
 */
export function useKeyedList<T extends object>(
  key: string | null,
  init: (store: Store) => ListInit<T>,
  watchMs: number | null = null,
): { list: KeyedList<T> | null; read: Read<readonly T[]> } {
  const store = useStore();
  const list = key === null ? null : keyedList(store, key, init);
  useEffect(() => list?.retain(), [list]);
  useEffect(() => {
    if (!list || watchMs === null) return;
    const timer = setInterval(() => {
      if (typeof document === "undefined" || !document.hidden) list.refresh();
    }, watchMs);
    return () => clearInterval(timer);
  }, [list, watchMs]);
  const snap = useSyncExternalStore<ResourceSnapshot<readonly T[]>>(
    list?.subscribe ?? noSubscribe,
    list?.getSnapshot ?? pendingSnapshot,
    list?.getSnapshot ?? pendingSnapshot,
  );
  const read = useMemo(
    () => toRead(snap, false, list ? () => list.refreshNow() : noRefresh),
    [snap, list],
  );
  return { list, read };
}

/** The rows of a list of the store, as a view reads them. */
export function useList<T extends object>(
  key: string | null,
  init: (store: Store) => ListInit<T>,
  watchMs: number | null = null,
): Read<readonly T[]> {
  return useKeyedList(key, init, watchMs).read;
}
