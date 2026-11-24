// The dashboard's state: the TanStack DB collections of one key, or one member in one organization,
// over the TanStack Query client of that identity, and what decides when they are read again
// (archi/07, "Dashboard": layers). Views read it through the hooks of `state/<domain>` files and
// call their actions; only `state/` calls the API, through `sources/`.
import { MutationCache, QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { createContext, type ReactNode, useContext, useEffect, useRef, useState } from "react";
import type { RunSummary } from "../model/types";
import { type Api, ApiError } from "../sources/client";
import { type WritableCollection, writableCollection } from "./collections";
import { History } from "./history";
import type { Kept } from "./kept";
import { Live, type LiveOptions } from "./live";
import { LIVE_QUERIES, MOVED_BY_RUNS, staleOn } from "./queries";
import { Resource, type ResourceInit } from "./resource";
import { RunSearch, type SearchRequest } from "./search";

export interface StoreOptions {
  /** Told of every failed read: a refused token sends back to the sign-in. */
  onError?: (error: unknown) => void;
  live?: LiveOptions;
}

export class Store {
  /** Each run's summary once, by id: what every table of runs shows. */
  readonly runs: WritableCollection<RunSummary>;
  readonly live: Live;
  private readonly searches = new Map<string, RunSearch>();
  private readonly resources = new Map<string, Resource<unknown>>();
  private readonly histories = new Map<string, History>();
  private readonly kept = new Map<string, Kept>();
  /** The runs whose page is open: their changes are kept even when no search holds them. */
  private readonly followed = new Map<string, number>();
  private disposed = false;

  constructor(
    readonly api: Api,
    readonly queryClient: QueryClient,
    private readonly options: StoreOptions = {},
  ) {
    this.runs = writableCollection<RunSummary>("runs", (r) => r.id);
    this.live = new Live(
      api,
      {
        apply: (run) => this.applyRun(run),
        control: (change) => this.stale(staleOn(change)),
        aggregates: () => this.stale(MOVED_BY_RUNS),
        reread: () => this.stale(LIVE_QUERIES),
      },
      options.live,
    );
  }

  /** Reads again the reads of TanStack Query on screen whose key starts with one of `keys`. */
  private stale(keys: readonly (readonly unknown[])[]): void {
    if (this.disposed) return;
    for (const queryKey of keys) void this.queryClient.invalidateQueries({ queryKey });
  }

  /** A read failed. */
  failed(error: unknown): void {
    this.options.onError?.(error);
  }

  /**
   * Writes summaries, each unless the run holds a later one: an answer read before a change
   * arrived does not take it back. `readAt`, when the answer's read began: a run the change stream
   * announced since then keeps the stream's summary, unless the answer's history is longer (a
   * quarantine or a task's state changes a summary without lengthening its history).
   */
  upsertRuns(summaries: readonly RunSummary[], readAt?: number): void {
    this.runs.upsert(
      summaries.filter((s) => {
        const held = this.runs.raw(s.id);
        if (held === undefined || s.historyLen > held.historyLen) return true;
        if (s.historyLen < held.historyLen) return false;
        const streamed = this.streamedAt.get(s.id);
        return readAt === undefined || streamed === undefined || streamed < readAt;
      }),
    );
  }

  /** When the change stream last announced each run the tab holds. */
  private readonly streamedAt = new Map<string, number>();

  /** A run's summary from the change stream: applied where the run is shown. */
  private applyRun(run: RunSummary): void {
    if (this.runs.raw(run.id) === undefined && !this.followed.has(run.id)) return;
    this.upsertRuns([run]);
    const now = Date.now();
    this.streamedAt.set(run.id, now);
    // Only a read begun before an announcement needs it: the old ones go.
    if (this.streamedAt.size > 1_000)
      for (const [id, at] of this.streamedAt) if (now - at > 5 * 60_000) this.streamedAt.delete(id);
  }

  /** A run's history grew or ended: its details are read again. */
  runTouched(runId: string): void {
    for (const resource of this.resources.values())
      if (resource.held && resource.touches(runId)) resource.refresh();
  }

  /** Keeps the changes of a run whose page is open. */
  follow(runId: string): () => void {
    this.followed.set(runId, (this.followed.get(runId) ?? 0) + 1);
    let done = false;
    return () => {
      if (done) return;
      done = true;
      const n = (this.followed.get(runId) ?? 1) - 1;
      if (n > 0) this.followed.set(runId, n);
      else this.followed.delete(runId);
    };
  }

  search(key: string, request: SearchRequest): RunSearch {
    let search = this.searches.get(key);
    if (!search) {
      search = new RunSearch(this, key, request, () => {
        this.searches.delete(key);
        this.sweep();
      });
      this.searches.set(key, search);
    }
    return search;
  }

  resource<T>(key: string, init: () => ResourceInit<T>): Resource<T> {
    let resource = this.resources.get(key) as Resource<T> | undefined;
    if (!resource) {
      const made = new Resource<T>(this, key, init(), () => this.resources.delete(key));
      this.resources.set(key, made as Resource<unknown>);
      resource = made;
    }
    return resource;
  }

  /** The resources whose key starts with `prefix`: what a write may have changed. */
  resourcesUnder(prefix: string): Resource<unknown>[] {
    return [...this.resources.entries()]
      .filter(([key]) => key.startsWith(prefix))
      .map(([, resource]) => resource);
  }

  /**
   * What a domain keeps by key besides the runs (the organization's members, a person's
   * connections): made once, dropped a while after its last view lets it go, disposed with the
   * store.
   */
  keep<K extends Kept>(key: string, make: (gone: () => void) => K): K {
    let kept = this.kept.get(key) as K | undefined;
    if (!kept) {
      kept = make(() => this.kept.delete(key));
      this.kept.set(key, kept);
    }
    return kept;
  }

  /** What `keep` holds under `key`, without making it. */
  peek<K extends Kept>(key: string): K | undefined {
    return this.kept.get(key) as K | undefined;
  }

  history(runId: string): History {
    let history = this.histories.get(runId);
    if (!history) {
      history = new History(this, runId, () => this.histories.delete(runId));
      this.histories.set(runId, history);
    }
    return history;
  }

  /** Drops the summaries no search holds and no open page follows. */
  private sweep(): void {
    if (this.disposed) return;
    const searches = [...this.searches.values()];
    const gone = [...this.runs.keys()].filter(
      (id) => !this.followed.has(id) && !searches.some((s) => s.holds(id)),
    );
    this.runs.remove(gone);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.live.dispose();
    for (const s of this.searches.values()) s.dispose();
    for (const r of this.resources.values()) r.dispose();
    for (const h of this.histories.values()) h.dispose();
    for (const k of this.kept.values()) k.dispose();
    this.searches.clear();
    this.resources.clear();
    this.histories.clear();
    this.kept.clear();
    void this.runs.collection.cleanup();
  }
}

const StoreContext = createContext<Store | null>(null);

/** The store of `state/`: for its hooks only. */
export function useStore(): Store {
  const store = useContext(StoreContext);
  if (!store) throw new Error("useStore is used outside StateProvider");
  return store;
}

/** A failed read or write, as the one told of it sees it. */
export interface Failure {
  store: Store;
  /** The key of the TanStack Query read that failed; none for a write or a collection's read. */
  key?: readonly unknown[];
}

export interface StateProviderProps {
  /** The client of the identity: the key, or the member in their organization. */
  api: Api;
  /** The Bearer token now: a new one opens the change stream again with it. */
  token: string | null;
  /** Told of every failed read and write: a refused token sends back to the sign-in. */
  onError?: (error: unknown, failure: Failure) => void;
  /** For tests: the change stream's timings and source. */
  live?: LiveOptions;
  children: ReactNode;
}

/** A client error will not go away by asking again. */
function retry(count: number, error: unknown): boolean {
  if (error instanceof ApiError && error.status >= 400 && error.status < 500) return false;
  return count < 2;
}

/**
 * The state of one identity, with a TanStack Query client of its own: what one key or member
 * read is never shown to another, not even while the next one's first answers are on their way.
 * The change stream opens with the first view that follows the runs, and again with each new
 * token.
 */
export function StateProvider({ api, token, onError, live, children }: StateProviderProps) {
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;
  const [store] = useState(() => {
    const failed = (error: unknown, key?: readonly unknown[]) =>
      onErrorRef.current?.(error, key ? { store: made, key } : { store: made });
    const queryClient = new QueryClient({
      queryCache: new QueryCache({ onError: (error, query) => failed(error, query.queryKey) }),
      mutationCache: new MutationCache({ onError: (error) => failed(error) }),
      defaultOptions: { queries: { retry, refetchOnWindowFocus: false } },
    });
    const made: Store = new Store(api, queryClient, {
      onError: (error) => failed(error),
      ...(live ? { live } : {}),
    });
    return made;
  });
  const disposal = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => {
    // React's strict mode unmounts and mounts again at once: the store stays.
    clearTimeout(disposal.current);
    return () => {
      disposal.current = setTimeout(() => store.dispose(), 0);
    };
  }, [store]);
  const firstToken = useRef(token);
  useEffect(() => {
    if (token === firstToken.current) return;
    firstToken.current = token;
    store.live.reopen();
  }, [store, token]);
  return (
    <QueryClientProvider client={store.queryClient}>
      <StoreContext.Provider value={store}>{children}</StoreContext.Provider>
    </QueryClientProvider>
  );
}
