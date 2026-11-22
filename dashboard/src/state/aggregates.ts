// Counts, charts and groups of runs (archi/05, "Live"): the statistics of a query (counts by
// status, series, durations, facets), the failure groups and the tools' statistics. Read again at
// most every 5 s while changes arrive, every 60 s while the change stream is open, and at the
// view's former rate while it is down. The time a query names (`since`, `until`) is read at each
// read: `makeQuery` is called then.
import type {
  FailureGroupPage,
  FailuresQuery,
  RunStats,
  RunStatsQuery,
  ToolStatsPage,
  ToolStatsQuery,
} from "../model/types";
import { type Read, useResource } from "./read";
import { useStore } from "./store";

/** Counts, series, durations and facets of the runs a query matches. */
export function useRunStats(
  key: string,
  makeQuery: () => RunStatsQuery,
  pollMs: number | null = 15_000,
): Read<RunStats> {
  const { api } = useStore();
  return useResource(`stats|${key}`, () => ({
    kind: "aggregate",
    pollMs,
    load: (signal) => api.runStats(makeQuery(), signal),
  }));
}

/** Failed, cancelled and aborted runs grouped by fingerprint. */
export function useFailures(
  key: string,
  makeQuery: () => FailuresQuery,
  pollMs: number | null = 30_000,
): Read<FailureGroupPage> {
  const { api } = useStore();
  return useResource(`failures|${key}`, () => ({
    kind: "aggregate",
    pollMs,
    load: (signal) => api.failures(makeQuery(), signal),
  }));
}

/** Latency, errors and retries per tool. */
export function useToolStats(
  key: string,
  makeQuery: () => ToolStatsQuery,
  pollMs: number | null = 30_000,
): Read<ToolStatsPage> {
  const { api } = useStore();
  return useResource(`tool-stats|${key}`, () => ({
    kind: "aggregate",
    pollMs,
    load: (signal) => api.toolStats(makeQuery(), signal),
  }));
}
