// The query and the window of the views that search runs, kept in the URL: the query follows the
// operator from one view to another, and a link names a search.
import { useCallback, useEffect } from "react";
import { useSearchParams } from "react-router";
import { type RangeKey, rangeOf } from "../model/filters";
import { legacyQuery, withQuery } from "../model/query";

/**
 * The query of the URL (`q`). An older link's filters (`?status=failed&label=…`) are read as the
 * query they mean, and the URL is rewritten to it.
 */
export function useSearchQuery() {
  const [params, setParams] = useSearchParams();
  const legacy = legacyQuery(params);
  const q = legacy ?? params.get("q") ?? "";
  useEffect(() => {
    if (legacy !== null) setParams(withQuery(params, legacy), { replace: true });
  }, [legacy, params, setParams]);
  const setQ = useCallback(
    (next: string) => setParams((current) => withQuery(current, next)),
    [setParams],
  );
  return { q, setQ, params, setParams };
}

export const WINDOWS: readonly RangeKey[] = ["1h", "24h", "7d", "30d"];

/** The width of the buckets of each window: from 12 to 30 bars. */
export const BUCKETS: Record<RangeKey, { bucket: string; ms: number }> = {
  "1h": { bucket: "5m", ms: 5 * 60_000 },
  "24h": { bucket: "1h", ms: 3_600_000 },
  "7d": { bucket: "6h", ms: 6 * 3_600_000 },
  "30d": { bucket: "1d", ms: 86_400_000 },
};

export const WINDOW_WORDS: Record<RangeKey, string> = {
  "1h": "the last hour",
  "24h": "the last 24 hours",
  "7d": "the last 7 days",
  "30d": "the last 30 days",
};

/** The window of the URL (`window`), 24 hours by default. */
export function useWindow(): [RangeKey, (w: RangeKey) => void] {
  const [params, setParams] = useSearchParams();
  const win: RangeKey = rangeOf(params.get("window")) ?? "24h";
  const setWin = useCallback(
    (w: RangeKey) =>
      setParams((current) => {
        const next = new URLSearchParams(current);
        if (w === "24h") next.delete("window");
        else next.set("window", w);
        return next;
      }),
    [setParams],
  );
  return [win, setWin];
}

/** `started:>now-24h`: the runs of a window, as a query term. */
export function windowTerm(field: "started" | "ended", win: RangeKey): string {
  return `${field}:>now-${win}`;
}

/** How an older link's parameters become a view's search. */
export interface OlderSearch {
  /** The parameters it had. */
  names: readonly string[];
  /** The search they mean, the link's own joined. */
  read: (params: URLSearchParams) => string;
}

/**
 * The search of a view that is not the runs' (Tools, Scripts, Namespaces), kept in the URL under
 * `param`: an older link's parameters become it, and the URL is rewritten to it.
 */
export function useViewSearch(param: string, older?: OlderSearch) {
  const [params, setParams] = useSearchParams();
  const old = older?.names.some((name) => params.has(name)) ? older.read(params) : null;
  const q = old ?? params.get(param) ?? "";
  const write = useCallback(
    (current: URLSearchParams, next: string) => {
      const out = new URLSearchParams(current);
      for (const name of older?.names ?? []) out.delete(name);
      if (next.trim()) out.set(param, next.trim());
      else out.delete(param);
      return out;
    },
    [older, param],
  );
  useEffect(() => {
    if (old !== null) setParams((current) => write(current, old), { replace: true });
  }, [old, setParams, write]);
  const setQ = useCallback(
    (next: string) => setParams((current) => write(current, next)),
    [setParams, write],
  );
  return { q, setQ };
}
