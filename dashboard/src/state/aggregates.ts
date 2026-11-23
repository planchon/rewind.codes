// Counts, charts and groups of runs (archi/05, "Statistics" and "Live"): the statistics of a scope
// (counts by status, series, durations, facets), the failure groups and the tools' statistics,
// from the hourly counters. Read again at most every 10 s while runs change, and once when the
// change stream reopens; never on a timer. The time a query names (`since`, `until`) is read at
// each read: `makeQuery` is called then.
//
// These routes take a scope and a window only: the query sent holds the view's one `namespace:`
// term, or none, and the server's fault at that term is placed back where the view's query has it.
import { statsScope } from "../model/scope";
import type {
  FailureGroupPage,
  FailuresQuery,
  RunStats,
  RunStatsQuery,
  ToolStatsPage,
  ToolStatsQuery,
} from "../model/types";
import { ApiError } from "../sources/client";
import { type Read, useResource } from "./read";
import { useStore } from "./store";

/** `read` of the query cut down to its scope; a fault at the scope's term placed in the view's query. */
async function scoped<Q extends { q?: string | null }, T>(
  query: Q,
  read: (query: Q) => Promise<T>,
): Promise<T> {
  const scope = statsScope(query.q ?? undefined);
  try {
    // A `q` left undefined is not sent.
    return await read({ ...query, q: scope.q });
  } catch (error) {
    const span = error instanceof ApiError ? error.problem.query : undefined;
    if (!(error instanceof ApiError) || error.slug !== "invalid-query" || !span) throw error;
    throw new ApiError(error.status, {
      ...error.problem,
      query: { start: span.start + scope.offset, end: span.end + scope.offset },
    });
  }
}

/** Counts, series, durations and facets of the runs of a scope. */
export function useRunStats(key: string, makeQuery: () => RunStatsQuery): Read<RunStats> {
  const { api } = useStore();
  return useResource(`stats|${key}`, () => ({
    kind: "aggregate",
    load: (signal) => scoped(makeQuery(), (query) => api.runStats(query, signal)),
  }));
}

/** Failed, cancelled and aborted runs of a scope, grouped by fingerprint. */
export function useFailures(key: string, makeQuery: () => FailuresQuery): Read<FailureGroupPage> {
  const { api } = useStore();
  return useResource(`failures|${key}`, () => ({
    kind: "aggregate",
    load: (signal) => scoped(makeQuery(), (query) => api.failures(query, signal)),
  }));
}

/** Latency, errors and retries per tool, over the calls of a scope. */
export function useToolStats(key: string, makeQuery: () => ToolStatsQuery): Read<ToolStatsPage> {
  const { api } = useStore();
  return useResource(`tool-stats|${key}`, () => ({
    kind: "aggregate",
    load: (signal) => scoped(makeQuery(), (query) => api.toolStats(query, signal)),
  }));
}
