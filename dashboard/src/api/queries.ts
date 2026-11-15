// Data hooks over the API, cached by TanStack Query. Content-addressed data (scripts, payloads)
// never goes stale. The dashboard reads every namespace of the organization (archi/05).
import {
  keepPreviousData,
  useInfiniteQuery,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useApi } from "../app/session";
import { followRun, type StreamState } from "./events";
import type {
  Entry,
  FailuresQuery,
  ListRunsQuery,
  ListScriptsQuery,
  RunStatsQuery,
  RunStatus,
  ToolStatsQuery,
} from "./types";

/** The client of the views. */
function useScope() {
  return { api: useApi() };
}

/** Runs matching a query, page after page. `makeQuery` resolves relative times at fetch time. */
export function useRuns(
  key: string,
  makeQuery: () => ListRunsQuery,
  refetchMs: number | false = 15_000,
) {
  const { api } = useScope();
  return useInfiniteQuery({
    queryKey: ["runs", key],
    queryFn: ({ pageParam, signal }) =>
      api.runs(pageParam ? { ...makeQuery(), cursor: pageParam } : makeQuery(), signal),
    initialPageParam: "",
    getNextPageParam: (last) => last.nextCursor ?? undefined,
    refetchInterval: refetchMs,
  });
}

/** Counts, series, durations and facets of the runs a query matches. */
export function useRunStats(
  key: string,
  makeQuery: () => RunStatsQuery,
  refetchMs: number | false = false,
) {
  const { api } = useScope();
  return useQuery({
    queryKey: ["run-stats", key],
    queryFn: ({ signal }) => api.runStats(makeQuery(), signal),
    refetchInterval: refetchMs,
    placeholderData: keepPreviousData,
  });
}

/** Failed, cancelled and aborted runs grouped by fingerprint. */
export function useFailures(
  key: string,
  makeQuery: () => FailuresQuery,
  refetchMs: number | false = false,
) {
  const { api } = useScope();
  return useQuery({
    queryKey: ["failures", key],
    queryFn: ({ signal }) => api.failures(makeQuery(), signal),
    refetchInterval: refetchMs,
    placeholderData: keepPreviousData,
  });
}

export function useRun(id: string) {
  const { api } = useScope();
  return useQuery({ queryKey: ["run", id], queryFn: ({ signal }) => api.run(id, signal) });
}

export function useScripts(key: string, query: ListScriptsQuery) {
  const { api } = useScope();
  return useInfiniteQuery({
    queryKey: ["scripts", key],
    queryFn: ({ pageParam, signal }) =>
      api.scripts(pageParam ? { ...query, cursor: pageParam } : query, signal),
    initialPageParam: "",
    getNextPageParam: (last) => last.nextCursor ?? undefined,
    refetchInterval: 30_000,
  });
}

export function useScript(digest: string | undefined) {
  const { api } = useScope();
  return useQuery({
    queryKey: ["script", digest],
    queryFn: ({ signal }) => api.script(digest ?? "", signal),
    enabled: Boolean(digest),
    staleTime: Number.POSITIVE_INFINITY,
  });
}

export function useToolStats(
  key: string,
  makeQuery: () => ToolStatsQuery,
  refetchMs: number | false = 30_000,
) {
  const { api } = useScope();
  return useQuery({
    queryKey: ["tool-stats", key],
    queryFn: ({ signal }) => api.toolStats(makeQuery(), signal),
    refetchInterval: refetchMs,
  });
}

export function usePayloadValue(digest: string, run: string, enabled: boolean) {
  const { api } = useScope();
  return useQuery({
    // A digest names one value, whichever run it is read through.
    queryKey: ["payload", digest],
    queryFn: ({ signal }) => api.payload(digest, run, signal),
    enabled,
    staleTime: Number.POSITIVE_INFINITY,
  });
}

export function useRunEntry(id: string, idx: number, enabled: boolean) {
  const { api } = useScope();
  return useQuery({
    queryKey: ["entry", id, idx],
    queryFn: ({ signal }) => api.entry(id, idx, signal),
    enabled,
    staleTime: Number.POSITIVE_INFINITY,
  });
}

/** The LLM trace, read again when the journal has grown to `historyLen` entries. */
export function useRunTrace(id: string, historyLen: number) {
  const { api } = useScope();
  return useQuery({
    queryKey: ["trace", id, historyLen],
    queryFn: ({ signal }) => api.trace(id, signal),
    placeholderData: keepPreviousData,
  });
}

export type JournalState = StreamState | "ended" | "error";

export interface RunJournal {
  entries: Entry[];
  state: JournalState;
  /** The status the stream ended with. */
  endStatus: RunStatus | null;
  error: unknown;
}

/**
 * The run's journal, read from its event stream and kept up to date while the run goes on.
 * Entries are applied at most every `flushMs`, so a long history loads in a few renders.
 */
export function useRunJournal(id: string, flushMs = 80): RunJournal {
  const { api } = useScope();
  const queryClient = useQueryClient();
  const [journal, setJournal] = useState<RunJournal>({
    entries: [],
    state: "connecting",
    endStatus: null,
    error: null,
  });
  const pending = useRef<Entry[]>([]);

  useEffect(() => {
    const controller = new AbortController();
    let flushTimer: ReturnType<typeof setTimeout> | undefined;
    let refreshTimer: ReturnType<typeof setTimeout> | undefined;
    let lastRefresh = 0;
    pending.current = [];
    setJournal({ entries: [], state: "connecting", endStatus: null, error: null });

    // New entries change the run's state: read it again, at most once a second.
    const refresh = () => {
      refreshTimer = undefined;
      lastRefresh = Date.now();
      void queryClient.invalidateQueries({ queryKey: ["run", id] }, { cancelRefetch: false });
    };
    const scheduleRefresh = () => {
      if (refreshTimer !== undefined) return;
      refreshTimer = setTimeout(refresh, Math.max(0, lastRefresh + 1_000 - Date.now()));
    };

    const flush = () => {
      flushTimer = undefined;
      const batch = pending.current;
      if (batch.length === 0) return;
      pending.current = [];
      setJournal((j) => ({ ...j, entries: j.entries.concat(batch) }));
      scheduleRefresh();
    };

    followRun(
      api,
      id,
      {
        onEntries(batch) {
          pending.current.push(...batch);
          flushTimer ??= setTimeout(flush, flushMs);
        },
        onEnd(status) {
          clearTimeout(flushTimer);
          flush();
          setJournal((j) => ({ ...j, state: "ended", endStatus: status }));
          clearTimeout(refreshTimer);
          refresh();
        },
        onState(state) {
          setJournal((j) => (j.state === state ? j : { ...j, state }));
        },
      },
      controller.signal,
    ).catch((error: unknown) => {
      if (!controller.signal.aborted) setJournal((j) => ({ ...j, state: "error", error }));
    });

    return () => {
      controller.abort();
      clearTimeout(flushTimer);
      clearTimeout(refreshTimer);
    };
  }, [api, id, flushMs, queryClient]);

  return journal;
}
