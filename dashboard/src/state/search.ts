// A search of runs, as the server answers it (archi/08, "Taken for the live dashboard"): its hits,
// which runs it returned and in which order, kept in a query collection; each run's summary once,
// in `runs`. Its table is its hits joined to `runs` by id: a run's change shows in every table at
// once, and which runs a search holds, and their order, comes from the server.
//
// What the table shows of the hits follows the operator (D7): an answer that adds, drops or
// reorders rows applies at once while the table is scrolled to its top; otherwise it waits, counted
// as "N new", until the operator shows it, comes back to the page, or an answer arrives while the
// table is at its top. A run that no longer matches stays until then, with its new status. A read
// after "Load more" asks for as many runs as are shown.
import { createCollection } from "@tanstack/db";
import { queryCollectionOptions } from "@tanstack/query-db-collection";
import type { ListRunsQuery, RunSort, RunSummary } from "../model/types";
import type { Api } from "../sources/client";
import { sameValue } from "./collections";
import { collectionId, Kept, withRetry } from "./kept";
import type { Watched } from "./live";
import type { ReadStatus } from "./resource";
import type { Store } from "./store";

/** The most runs `GET /v1/runs` answers at once. */
export const MAX_PAGE = 500;

export interface SearchRequest {
  /** The query, read by the server only. */
  q: string;
  sort: RunSort;
  /** Runs per page: the first read, and each "Load more". */
  pageSize: number;
  /** How often it is read while the change stream is down. */
  pollMs: number | null;
}

/** An answer not shown yet: how many of its runs the table does not show. */
export interface PendingAnswer {
  added: number;
}

export interface SearchSnapshot {
  status: ReadStatus;
  /** The runs shown, in the order shown. */
  runs: readonly RunSummary[];
  /** The failure of the first read, when nothing could be shown. */
  error: unknown;
  hasMore: boolean;
  loadingMore: boolean;
  pending: PendingAnswer | null;
}

interface Hit {
  id: string;
  position: number;
}

/** Reads `count` runs of the search, page after page past `MAX_PAGE`. */
async function readRuns(
  api: Api,
  request: SearchRequest,
  count: number,
  signal: AbortSignal,
): Promise<{ runs: RunSummary[]; next: string | null }> {
  const runs: RunSummary[] = [];
  const seen = new Set<string>();
  let cursor: string | null = null;
  do {
    const query: ListRunsQuery = {
      ...(request.q ? { q: request.q } : {}),
      ...(request.sort !== "started" ? { sort: request.sort } : {}),
      limit: Math.min(MAX_PAGE, count - runs.length),
      ...(cursor ? { cursor } : {}),
    };
    const page = await api.runs(query, signal);
    for (const run of page.runs) {
      if (seen.has(run.id)) continue;
      seen.add(run.id);
      runs.push(run);
    }
    cursor = page.nextCursor ?? null;
  } while (cursor !== null && runs.length < count);
  return { runs, next: cursor };
}

function sameIds(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((id, i) => id === b[i]);
}

/** The shown runs, then the runs of the answer that come after the last of them. */
function appendTail(shown: readonly string[], answer: readonly string[]): string[] {
  const held = new Set(shown);
  let last = -1;
  for (let i = answer.length - 1; i >= 0; i -= 1) {
    if (held.has(answer[i] as string)) {
      last = i;
      break;
    }
  }
  if (last < 0) return [...answer];
  return [...shown, ...answer.slice(last + 1).filter((id) => !held.has(id))];
}

export class RunSearch extends Kept implements Watched {
  readonly kind = "search" as const;
  readonly pollMs: number | null;
  lastReadAt = 0;
  private count: number;
  private next: string | null = null;
  private answered = false;
  private error: unknown = null;
  /** The server's last answer. */
  private answer: string[] = [];
  /** What the table shows; `null` before the first answer. */
  private shown: string[] | null = null;
  private shownSet = new Set<string>();
  private rows: RunSummary[] = [];
  private atTop = true;
  private loadMoreTo: number | null = null;
  private inflight = 0;
  private again = false;
  /** An answer is being applied: its runs' changes wait for it. */
  private applying = false;
  private snapshot: SearchSnapshot = {
    status: "pending",
    runs: [],
    error: null,
    hasMore: false,
    loadingMore: false,
    pending: null,
  };
  private readonly hits;
  private readonly unsubscribe: () => void;
  private unwatch: (() => void) | null = null;

  constructor(
    private readonly store: Store,
    readonly key: string,
    private readonly request: SearchRequest,
    gone: () => void,
  ) {
    super(gone);
    this.pollMs = request.pollMs;
    this.count = request.pageSize;
    this.hits = createCollection(
      queryCollectionOptions<Hit>({
        id: collectionId(`search:${key}`),
        queryKey: ["state", "search", key],
        queryClient: store.queryClient,
        getKey: (hit) => hit.id,
        gcTime: 0,
        startSync: true,
        staleTime: Number.POSITIVE_INFINITY,
        retry: false,
        refetchOnWindowFocus: false,
        refetchOnReconnect: false,
        queryFn: ({ signal }) => this.read(signal),
      }),
    );
    // The hits live while the search does; a run's change reaches the rows shown.
    const hits = this.hits.subscribeChanges(() => {});
    const runs = store.runs.collection.subscribeChanges((changes) => {
      if (this.applying) return;
      if (changes.some((c) => this.shownSet.has(String(c.key)))) this.update();
    });
    this.unsubscribe = () => {
      hits.unsubscribe();
      runs.unsubscribe();
    };
  }

  getSnapshot = (): SearchSnapshot => this.snapshot;

  /** The runs it holds: those shown and those of the last answer. */
  holds(id: string): boolean {
    return this.shownSet.has(id) || this.answer.includes(id);
  }

  refresh = (): void => {
    if (this.disposed) return;
    if (this.inflight > 0) {
      this.again = true;
      return;
    }
    // A read the collection cancels (its cleanup) is over: nothing to tell.
    this.hits.utils.refetch().catch(() => undefined);
  };

  /** The next page: the search is read again with as many runs as shown, and a page more. */
  loadMore = (): void => {
    if (this.disposed || this.next === null || this.loadMoreTo !== null) return;
    this.count = Math.max(this.count, this.shown?.length ?? 0) + this.request.pageSize;
    this.loadMoreTo = this.count;
    this.update();
    this.refresh();
  };

  /** Shows the answer the table holds back. */
  showPending = (): void => {
    if (this.shown === null || sameIds(this.shown, this.answer)) return;
    this.setShown(this.answer);
    this.update();
  };

  /** Whether the table is scrolled to its top: answers apply at once while it is. */
  setAtTop = (atTop: boolean): void => {
    this.atTop = atTop;
  };

  protected override onRetained(): void {
    this.unwatch = this.store.live.watch(this);
    // Back on the page: the answer held back shows, and the search is read again.
    this.showPending();
    if (this.answered && Date.now() - this.lastReadAt > 2_000) this.refresh();
  }

  protected override onReleased(): void {
    this.unwatch?.();
    this.unwatch = null;
  }

  override dispose(): void {
    super.dispose();
    this.unwatch?.();
    this.unsubscribe();
    void this.hits.cleanup();
  }

  private async read(signal: AbortSignal): Promise<Hit[]> {
    const readAt = Date.now();
    this.lastReadAt = readAt;
    this.inflight += 1;
    const count = this.count;
    try {
      const { runs, next } = await withRetry(
        () => readRuns(this.store.api, this.request, count, signal),
        signal,
      );
      this.next = next;
      this.error = null;
      this.answered = true;
      this.applying = true;
      try {
        this.store.upsertRuns(runs, readAt);
        this.apply(
          runs.map((r) => r.id),
          count,
        );
      } finally {
        this.applying = false;
      }
      return runs.map((r, position) => ({ id: r.id, position }));
    } catch (error) {
      if (signal.aborted) throw error;
      this.error = error;
      this.store.failed(error);
      if (this.loadMoreTo !== null && count >= this.loadMoreTo) this.loadMoreTo = null;
      this.update();
      // The hits read before stay.
      return this.answer.map((id, position) => ({ id, position }));
    } finally {
      this.inflight -= 1;
      if (this.again) {
        this.again = false;
        setTimeout(this.refresh, 0);
      }
    }
  }

  /** An answer: shown at once, or held back (D7). */
  private apply(ids: string[], count: number): void {
    if (!sameIds(ids, this.answer)) this.answer = ids;
    const loadedMore = this.loadMoreTo !== null && count >= this.loadMoreTo;
    if (loadedMore) this.loadMoreTo = null;
    if (this.shown === null || this.atTop) this.setShown(this.answer);
    else if (loadedMore) this.setShown(appendTail(this.shown, this.answer));
    this.update();
  }

  private setShown(ids: readonly string[]): void {
    if (this.shown !== null && sameIds(this.shown, ids)) return;
    this.shown = [...ids];
    this.shownSet = new Set(ids);
  }

  /** The snapshot, rebuilt only for what changed: an unchanged row keeps its object. */
  private update(): void {
    if (this.disposed) return;
    const shown = this.shown ?? [];
    const rows: RunSummary[] = [];
    for (const id of shown) {
      const run = this.store.runs.get(id);
      if (run) rows.push(run);
    }
    if (rows.length === this.rows.length && rows.every((r, i) => r === this.rows[i])) {
      // Unchanged: the same array.
    } else {
      this.rows = rows;
    }
    const pending =
      this.shown === null || sameIds(this.shown, this.answer)
        ? null
        : { added: this.answer.filter((id) => !this.shownSet.has(id)).length };
    const next: SearchSnapshot = {
      status: this.answered ? "ready" : this.error !== null ? "error" : "pending",
      runs: this.rows,
      error: this.answered ? null : this.error,
      hasMore: this.next !== null,
      loadingMore: this.loadMoreTo !== null,
      pending,
    };
    const s = this.snapshot;
    if (
      s.status === next.status &&
      s.runs === next.runs &&
      s.error === next.error &&
      s.hasMore === next.hasMore &&
      s.loadingMore === next.loadingMore &&
      sameValue(s.pending, next.pending)
    )
      return;
    this.snapshot = sameValue(s.pending, next.pending) ? { ...next, pending: s.pending } : next;
    this.notify();
  }
}
