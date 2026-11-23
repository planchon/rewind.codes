// Runs: the searches of runs and their tables, one run's details and journal, and the values its
// journal holds. A run's new summary, from the change stream, shows at once in every table that
// shows it; the searches are read again a second after the last change.
import { useQuery } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useRef, useSyncExternalStore } from "react";
import type { Entry, Run, RunSort, RunSummary } from "../model/types";
import type { JournalSnapshot } from "./journal";
import { type Read, useQueryRead, useResource } from "./read";
import type { ReadStatus } from "./resource";
import type { PendingAnswer, RunSearch, SearchSnapshot } from "./search";
import { useStore } from "./store";

export type { JournalSnapshot as RunJournal, JournalState } from "./journal";
export type { PendingAnswer } from "./search";

export interface RunSearchOptions {
  /** The query, read by the server. */
  q: string;
  /** The order; the most recently started first by default. */
  sort?: RunSort;
  /** Runs per page; 50 by default. */
  pageSize?: number;
  /** How often the runs are read while the change stream is down; 15 s by default. */
  pollMs?: number | null;
}

export interface RunSearchView {
  status: ReadStatus;
  /** The runs shown, in the order shown: each the same object until it changes. */
  runs: readonly RunSummary[];
  /** The failure of the first read, when nothing can be shown. */
  error: unknown;
  /** The rows are the previous search's, dimmed, until the new one answers. */
  stale: boolean;
  hasMore: boolean;
  loadingMore: boolean;
  /** Reads as many runs as shown, and a page more. */
  loadMore: () => void;
  /** An answer the table holds back while it is scrolled ("N new · Show"). */
  pending: PendingAnswer | null;
  /** Shows the answer held back. */
  showPending: () => void;
  /** Says whether the table is scrolled to its top: answers apply at once while it is. */
  setAtTop: (atTop: boolean) => void;
}

/** The runs a query matches, in a table (archi/08, "Taken for the live dashboard"). */
export function useRunSearch(options: RunSearchOptions): RunSearchView {
  const store = useStore();
  const sort = options.sort ?? "started";
  const pageSize = options.pageSize ?? 50;
  const pollMs = options.pollMs === undefined ? 15_000 : options.pollMs;
  const key = JSON.stringify([options.q, sort, pageSize]);
  const search = store.search(key, { q: options.q, sort, pageSize, pollMs });
  // Whether the table is at its top is the view's: a new search learns it from the view, which
  // hears of it only when the table crosses its top.
  const atTop = useRef(true);
  search.setAtTop(atTop.current);
  const setAtTop = useCallback(
    (top: boolean) => {
      atTop.current = top;
      search.setAtTop(top);
    },
    [search],
  );
  useEffect(() => search.retain(), [search]);
  const snap = useSyncExternalStore(search.subscribe, search.getSnapshot, search.getSnapshot);
  // A changed search keeps the previous rows, dimmed, until its first answer.
  const last = useRef<{ snap: SearchSnapshot; search: RunSearch } | null>(null);
  if (snap.status !== "pending") last.current = { snap, search };
  const previous = snap.status === "pending" ? last.current : null;
  return useMemo(() => {
    const shown = previous?.snap ?? snap;
    return {
      status: shown.status,
      runs: shown.runs,
      error: shown.error,
      stale: previous !== null,
      hasMore: previous ? false : shown.hasMore,
      loadingMore: shown.loadingMore,
      loadMore: search.loadMore,
      pending: previous ? null : shown.pending,
      showPending: search.showPending,
      setAtTop,
    };
  }, [snap, previous, search, setAtTop]);
}

/**
 * One run's details; `enabled` false until they are needed (a card that opens). Read again when
 * the run changes, as its journal grows, and every minute.
 */
export function useRun(id: string, enabled = true): Read<Run> {
  const store = useStore();
  useEffect(() => (enabled && id ? store.follow(id) : undefined), [store, id, enabled]);
  return useResource(enabled && id ? `run|${id}` : null, () => ({
    kind: "detail",
    runId: id,
    pollMs: 30_000,
    load: (signal) => store.api.run(id, signal),
  }));
}

/** The run's summary as the tab holds it, from a table or the change stream; `undefined` if none. */
export function useRunSummary(id: string): RunSummary | undefined {
  const { runs } = useStore();
  const subscribe = useMemo(
    () => (listener: () => void) => {
      const subscription = runs.collection.subscribeChanges((changes) => {
        if (changes.some((c) => c.key === id)) listener();
      });
      return () => subscription.unsubscribe();
    },
    [runs, id],
  );
  const get = () => runs.get(id);
  return useSyncExternalStore(subscribe, get, get);
}

/** The run's journal, from its event stream, kept up to date while the run goes on. */
export function useRunJournal(id: string): JournalSnapshot {
  const store = useStore();
  const journal = store.journal(id);
  useEffect(() => journal.retain(), [journal]);
  return useSyncExternalStore(journal.subscribe, journal.getSnapshot, journal.getSnapshot);
}

/** One entry of the journal, whole (every value, whatever its size). */
export function useRunEntry(id: string, idx: number, enabled: boolean): Read<Entry> {
  const { api } = useStore();
  return useQueryRead(
    useQuery({
      queryKey: ["entry", id, idx],
      queryFn: ({ signal }) => api.entry(id, idx, signal),
      enabled,
      staleTime: Number.POSITIVE_INFINITY,
    }),
  );
}

/** A payload's value, read through the run whose journal holds it; a digest names one value. */
export function usePayloadValue(digest: string, run: string, enabled: boolean): Read<unknown> {
  const { api } = useStore();
  return useQueryRead(
    useQuery({
      queryKey: ["payload", digest],
      queryFn: ({ signal }) => api.payload(digest, run, signal),
      enabled,
      staleTime: Number.POSITIVE_INFINITY,
    }),
  );
}

/** The run's journal as a file to save: the journal document, or a cassette of its calls. */
export function useRunExport(): (id: string, as: "journal" | "cassette") => Promise<Blob> {
  const { api } = useStore();
  return (id, as) => api.journal(id, as);
}
