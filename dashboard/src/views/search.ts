// The query and the window of the views that search runs, kept in the URL: the query follows the
// operator from one view to another, and a link names a search.
import { useCallback, useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router";
import { useSettled } from "../components/bits";
import { RANGES, type RangeKey } from "../model/filters";
import { formatCount } from "../model/format";
import { type FieldValues, legacyQuery, TEXT_VALUES, withQuery } from "../model/query";
import { useAccess } from "../state/access";
import { useNamespaceSuggestions } from "../state/namespaces";

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

/**
 * The query of a list that is not of runs (Scripts, Tools, Namespaces), in the URL (`q`). An older
 * link's parameters, which `legacy` reads as a query, are rewritten to it.
 */
export function useListQuery(
  legacy: (params: URLSearchParams) => string | null,
  legacyNames: readonly string[],
) {
  const [params, setParams] = useSearchParams();
  const old = legacy(params);
  const q = old ?? params.get("q") ?? "";
  const setQ = useCallback(
    (next: string, replace = false) =>
      setParams(
        (current) => {
          const p = new URLSearchParams(current);
          for (const name of legacyNames) p.delete(name);
          if (next.trim()) p.set("q", next.trim());
          else p.delete("q");
          return p;
        },
        { replace },
      ),
    [setParams, legacyNames],
  );
  useEffect(() => {
    if (old !== null) setQ(old, true);
  }, [old, setQ]);
  return { q, setQ, params, setParams };
}

export const WINDOWS: readonly RangeKey[] = ["1h", "24h", "7d", "30d"];

/**
 * The width of the buckets of each window: whole hours, since the statistics are kept per hour
 * (archi/05, "Statistics"); from 24 to 30 bars past the last hour.
 */
export const BUCKETS: Record<RangeKey, { bucket: string; ms: number }> = {
  "1h": { bucket: "1h", ms: 3_600_000 },
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
  const raw = params.get("window");
  const win: RangeKey = raw !== null && Object.hasOwn(RANGES, raw) ? (raw as RangeKey) : "24h";
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

/**
 * The namespaces to suggest as a name is typed: those whose name starts with it, the most active
 * this month first, for whoever reads the namespaces. The facets already count the namespaces of
 * the current query's runs; these add the end users the facets leave out.
 */
function useNamespaceLookup(typed: string | null) {
  const access = useAccess();
  const prefix = useSettled(typed, 150);
  const found = useNamespaceSuggestions(access.can("namespaces:read") ? prefix : null);
  return useMemo(
    () =>
      (found.data?.namespaces ?? []).map((ns) => ({
        value: ns.name,
        detail: `${ns.displayName ? `${ns.displayName} · ` : ""}${formatCount(ns.usage.runsStarted)} runs this month`,
        ...(ns.displayName ? { also: ns.displayName } : {}),
      })),
    [found.data],
  );
}

/** The values the runs' query bar suggests beside the facets: the namespaces, looked up. */
export function useRunQueryValues(): {
  values: FieldValues;
  onTyping: (typed: { field: string; value: string } | null) => void;
} {
  const [typed, setTyped] = useState<string | null>(null);
  const namespaces = useNamespaceLookup(typed);
  const onTyping = useCallback(
    (t: { field: string; value: string } | null) =>
      setTyped(t?.field === "namespace" ? t.value : null),
    [],
  );
  const values = useMemo(() => ({ namespace: namespaces }), [namespaces]);
  return { values, onTyping };
}

/** The values the Namespaces' bar suggests for its free text: the names that start with it. */
export function useNamespaceNameValues(): {
  values: FieldValues;
  onTyping: (typed: { field: string; value: string } | null) => void;
} {
  const [typed, setTyped] = useState<string | null>(null);
  const namespaces = useNamespaceLookup(typed);
  const onTyping = useCallback(
    (t: { field: string; value: string } | null) =>
      setTyped(t?.field === TEXT_VALUES && t.value !== "" ? t.value : null),
    [],
  );
  const values = useMemo(() => ({ [TEXT_VALUES]: namespaces }), [namespaces]);
  return { values, onTyping };
}
