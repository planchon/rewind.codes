// Data hooks over the namespaces: the list (there can be many thousands), one namespace with its
// use day by day, the defaults with the ceiling, and the alerts. They read every namespace of the
// organization: the filter of the run views (`?ns=`) does not narrow them. A write invalidates
// everything under `NAMESPACES`.
import { keepPreviousData, useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { useAccess } from "../app/access";
import { useApi } from "../app/session";
import type { ListNamespacesQuery } from "./types";

export const NAMESPACES = ["namespaces"] as const;

/** The namespaces, page after page, in the order asked; polled while shown. */
export function useNamespaceList(query: Omit<ListNamespacesQuery, "after">, enabled = true) {
  const api = useApi();
  return useInfiniteQuery({
    queryKey: [...NAMESPACES, "list", query],
    queryFn: ({ pageParam, signal }) =>
      api.namespaces(pageParam ? { ...query, after: pageParam } : query, signal),
    initialPageParam: "",
    getNextPageParam: (last) => last.next ?? undefined,
    placeholderData: keepPreviousData,
    refetchInterval: 30_000,
    enabled,
  });
}

/** The namespaces whose name starts with `prefix`, the most active first: what to suggest. */
export function useNamespaceSuggestions(prefix: string | null, limit = 8) {
  const api = useApi();
  return useQuery({
    queryKey: [...NAMESPACES, "suggestions", prefix, limit],
    queryFn: ({ signal }) =>
      api.namespaces({ ...(prefix ? { prefix } : {}), order: "runs", limit }, signal),
    enabled: prefix !== null,
    staleTime: 30_000,
    placeholderData: keepPreviousData,
  });
}

/**
 * The display names the organization gave its namespaces, by id: read once for every view, for
 * whoever reads the namespaces (the others see the ids). Empty while loading.
 */
export function useNamespaceNames(): ReadonlyMap<string, string> {
  const api = useApi();
  const access = useAccess();
  const names = useQuery({
    queryKey: [...NAMESPACES, "names"],
    queryFn: ({ signal }) => api.namespaceNames(signal),
    enabled: access.can("namespaces:read"),
    staleTime: 60_000,
    refetchInterval: 5 * 60_000,
  });
  return useMemo(
    () => new Map((names.data?.namespaces ?? []).map((n) => [n.name, n.displayName])),
    [names.data],
  );
}

/** The default limits, the organization's ceiling and its use of the month. */
export function useNamespaceDefaults(enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: [...NAMESPACES, "defaults"],
    queryFn: ({ signal }) => api.namespaceDefaults(signal),
    refetchInterval: 30_000,
    enabled,
  });
}

/** One namespace over a month (the current one when `null`). */
export function useNamespaceDetail(name: string, month: string | null) {
  const api = useApi();
  return useQuery({
    queryKey: [...NAMESPACES, "detail", name, month],
    queryFn: ({ signal }) => api.namespaceDetail(name, month, signal),
    placeholderData: keepPreviousData,
    refetchInterval: 30_000,
  });
}

/** The alerts of a month (the current one when `null`) across the organization. */
export function useQuotaAlerts(month: string | null, enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: [...NAMESPACES, "alerts", month],
    queryFn: ({ signal }) => api.quotaAlerts(month ? { month } : {}, signal),
    refetchInterval: 30_000,
    enabled,
  });
}

/** A value once it has stopped changing for `ms`: what is typed, asked for once. */
export function useSettled<T>(value: T, ms = 150): T {
  const [settled, setSettled] = useState(value);
  useEffect(() => {
    const timer = setTimeout(() => setSettled(value), ms);
    return () => clearTimeout(timer);
  }, [value, ms]);
  return settled;
}
