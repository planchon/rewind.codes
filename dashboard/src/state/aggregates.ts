// Counts, charts and groups of runs (archi/05, "Statistics" and "Live"): the statistics of a scope
// (counts by status, series, durations, facets), the fingerprints and the actions' statistics,
// from the hourly counters. Read again at most every 10 s while runs change, and once when the
// change stream reopens; never on a timer. The time a query names (`since`, `until`) is read at
// each read: `makeQuery` is called then.
//
// These routes take a scope and a window only: the query sent holds the view's one `namespace:`
// term, or none, and the server's fault at that term is placed back where the view's query has it.
// Counters know no labels: a grant limited to labels reads none of them, and its views are told so
// without asking the server, which would answer 403.
import { useMemo } from "react";
import { statsScope } from "../model/scope";
import type {
  ActionStatsPage,
  ActionStatsQuery,
  FailuresQuery,
  FingerprintPage,
  RunStats,
  RunStatsQuery,
} from "../model/types";
import { ApiError } from "../sources/client";
import { useAccess } from "./access";
import { type Read, toRead, useResource } from "./read";
import { useStore } from "./store";

/** What the server answers a grant limited to labels on these routes. */
const REFUSED = new ApiError(403, {
  type: "urn:rewind:problem:forbidden",
  title: "Forbidden",
  status: 403,
  detail:
    "Statistics come from counters, which know no labels: a grant limited to labels cannot read them. Search the runs instead.",
});

/** A read of the statistics, or their refusal when the caller's grant is limited to labels. */
function useCounted<T>(key: string, load: (signal: AbortSignal) => Promise<T>): Read<T> {
  const { statistics } = useAccess();
  const read = useResource(statistics ? key : null, () => ({ kind: "aggregate", load }));
  const refused = useMemo(
    () => toRead<T>({ status: "error", data: undefined, error: REFUSED }, false, async () => {}),
    [],
  );
  return statistics ? read : refused;
}

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
  return useCounted(`stats|${key}`, (signal) =>
    scoped(makeQuery(), (query) => api.runStats(query, signal)),
  );
}

/** Failed, cancelled and aborted runs of a scope, by fingerprint. */
export function useFailures(key: string, makeQuery: () => FailuresQuery): Read<FingerprintPage> {
  const { api } = useStore();
  return useCounted(`failures|${key}`, (signal) =>
    scoped(makeQuery(), (query) => api.failures(query, signal)),
  );
}

/** Latency, errors and redeliveries per action, over the calls of a scope. */
export function useActionStats(
  key: string,
  makeQuery: () => ActionStatsQuery,
): Read<ActionStatsPage> {
  const { api } = useStore();
  return useCounted(`action-stats|${key}`, (signal) =>
    scoped(makeQuery(), (query) => api.actionStats(query, signal)),
  );
}
