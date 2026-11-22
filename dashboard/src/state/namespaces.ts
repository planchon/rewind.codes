// The namespaces, the end users of the organization: what every view shows of them (their display
// names, the names to suggest as one is typed in a query bar), and the Namespaces views' own
// reads (the list, there can be many thousands; one namespace with its use day by day; the
// defaults with the ceiling; the alerts) and writes. They read every namespace of the
// organization: the filter of the run views does not narrow them. A namespace's use moves with its
// runs: they are read again within 5 s of a run's change, every 60 s while the change stream is
// open, every 30 s while it is down.
//
// A write is shown at once where its answer is known (a name, limits, the defaults, revoked tokens,
// an erasure starting), then every namespace read on screen is read again; a refusal undoes it.
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useMemo } from "react";
import { QUOTA_FIELDS } from "../model/quotas";
import type {
  ListNamespacesQuery,
  Namespace,
  NamespaceDefaults,
  NamespaceDetail,
  NamespaceList,
  NamespaceNames,
  QuotaAlertList,
  Quotas,
  RevokedTokens,
} from "../model/types";
import { useAccess } from "./access";
import { type Action, optimistic, useAction } from "./actions";
import { type Paged, type PagedRead, usePaged } from "./paged";
import { type Read, useQueryRead, useResource } from "./read";
import type { Resource } from "./resource";
import { type Store, useStore } from "./store";

const NAMESPACES = ["namespaces"] as const;
const NAMES = [...NAMESPACES, "names"] as const;
/** The keys of the namespaces' resources in the store. */
const PREFIX = "namespaces|";
const POLL = 30_000;

/** The namespaces whose name starts with `prefix`, the most active first: what to suggest. */
export function useNamespaceSuggestions(prefix: string | null, limit = 8): Read<NamespaceList> {
  const { api } = useStore();
  return useQueryRead(
    useQuery({
      queryKey: [...NAMESPACES, "suggestions", prefix, limit],
      queryFn: ({ signal }) =>
        api.namespaces({ ...(prefix ? { prefix } : {}), order: "runs", limit }, signal),
      enabled: prefix !== null,
      staleTime: 30_000,
      placeholderData: keepPreviousData,
    }),
  );
}

/**
 * The display names the organization gave its namespaces, by id: read once for every view, for
 * whoever reads the namespaces (the others see the ids). Empty while loading.
 */
export function useNamespaceNames(): ReadonlyMap<string, string> {
  const { api } = useStore();
  const access = useAccess();
  const names = useQuery({
    queryKey: NAMES,
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

// The Namespaces views' reads.

/** What the list's first page says besides its namespaces. */
export interface NamespaceListHead {
  /** The month whose use is listed, `2026-11`. */
  month: string;
  /** How many namespaces match, across every page. */
  total: number;
}

export type NamespaceListRead = PagedRead<Namespace, NamespaceListHead>;

/** The namespaces, in the order asked, page after page (`limit` per page, 50 by default). */
export function useNamespaceList(query: Omit<ListNamespacesQuery, "after">): NamespaceListRead {
  const { api } = useStore();
  const { limit, ...filter } = query;
  return usePaged(`${PREFIX}list|${JSON.stringify([filter, limit])}`, () => ({
    kind: "aggregate",
    pollMs: POLL,
    pageSize: limit ?? 50,
    keyOf: (ns: Namespace) => ns.name,
    readPage: async (after, pageLimit, signal) => {
      const page = await api.namespaces(
        { ...filter, limit: pageLimit, ...(after ? { after } : {}) },
        signal,
      );
      return {
        rows: page.namespaces,
        next: page.next ?? null,
        head: { month: page.month, total: page.total },
      };
    },
  }));
}

/** How many namespaces match `query`: one namespace read is enough to count them. */
export function useNamespaceCount(
  query: Omit<ListNamespacesQuery, "after" | "limit">,
): Read<number> {
  const { api } = useStore();
  return useResource(`${PREFIX}count|${JSON.stringify(query)}`, () => ({
    kind: "aggregate",
    pollMs: POLL,
    load: async (signal) => (await api.namespaces({ ...query, limit: 1 }, signal)).total,
  }));
}

/** The default limits, the organization's ceiling and its use of the month. */
export function useNamespaceDefaults(): Read<NamespaceDefaults> {
  const { api } = useStore();
  return useResource(`${PREFIX}defaults`, () => ({
    kind: "aggregate",
    pollMs: POLL,
    load: (signal) => api.namespaceDefaults(signal),
  }));
}

/** One namespace over a month (the current one when `null`): its limits, use, alerts, tokens. */
export function useNamespaceDetail(name: string, month: string | null): Read<NamespaceDetail> {
  const { api } = useStore();
  return useResource(`${PREFIX}detail|${name}|${month ?? "now"}`, () => ({
    kind: "aggregate",
    pollMs: POLL,
    load: (signal) => api.namespaceDetail(name, month, signal),
  }));
}

/** The alerts of a month (the current one when `null`) across the organization. */
export function useQuotaAlerts(month: string | null): Read<QuotaAlertList> {
  const { api } = useStore();
  return useResource(`${PREFIX}alerts|${month ?? "now"}`, () => ({
    kind: "aggregate",
    pollMs: POLL,
    load: (signal) => api.quotaAlerts(month ? { month } : {}, signal),
  }));
}

// The Namespaces views' writes.

/** The namespaces' resources whose key goes on with `rest`. */
function resources<T>(store: Store, rest: string): Resource<T>[] {
  return store.resourcesUnder(`${PREFIX}${rest}`) as unknown as Resource<T>[];
}

/**
 * Shows `change` of a namespace wherever it is shown: its page (every month, with `detail` for the
 * rest of the page) and the lists. `name` null changes every namespace.
 */
function showNamespace(
  store: Store,
  name: string | null,
  change: (ns: Namespace) => Namespace,
  detail: (d: NamespaceDetail) => NamespaceDetail = (d) => d,
): void {
  for (const r of resources<NamespaceDetail>(store, name === null ? "detail|" : `detail|${name}|`))
    r.change((d) => detail({ ...d, namespace: change(d.namespace) }));
  const touched = (ns: Namespace) => name === null || ns.name === name;
  for (const r of resources<Paged<Namespace, NamespaceListHead>>(store, "list|"))
    r.change((page) =>
      page.rows.some(touched)
        ? { ...page, rows: page.rows.map((ns) => (touched(ns) ? change(ns) : ns)) }
        : page,
    );
}

/** Reads again every namespace read on screen, and the names. */
async function reread(store: Store): Promise<void> {
  await Promise.all([
    ...store
      .resourcesUnder(PREFIX)
      .filter((r) => r.held)
      .map((r) => r.refreshNow()),
    store.queryClient.invalidateQueries({ queryKey: NAMESPACES }),
  ]);
}

/** The limits that apply: per quota, the namespace's exception, else the default. */
function limitsOf(exceptions: Quotas, defaults: Quotas): Quotas {
  const limits: Record<string, unknown> = {};
  for (const { key } of QUOTA_FIELDS) {
    const value = exceptions[key] ?? defaults[key];
    if (value !== null && value !== undefined) limits[key] = value;
  }
  return limits as Quotas;
}

/** The names with `name`'s display name set, or removed (`null`). */
function renamed(names: NamespaceNames, name: string, displayName: string | null): NamespaceNames {
  const rest = names.namespaces.filter((n) => n.name !== name);
  if (displayName === null) return { namespaces: rest };
  return {
    namespaces: [...rest, { name, displayName }].sort((a, b) => (a.name < b.name ? -1 : 1)),
  };
}

/** Sets the namespace's display name; `null` removes it, and the views show its id. */
export function useRenameNamespace(name: string): Action<string | null, NamespaceDetail> {
  const store = useStore();
  return useAction((displayName: string | null) => {
    const names = store.queryClient.getQueryData<NamespaceNames>(NAMES);
    return optimistic({
      show: () => {
        showNamespace(store, name, (ns) => ({ ...ns, displayName }));
        if (names) store.queryClient.setQueryData(NAMES, renamed(names, name, displayName));
      },
      undo: () => {
        if (names) store.queryClient.setQueryData(NAMES, names);
      },
      write: () => store.api.patchNamespace(name, { displayName }),
      reread: () => reread(store),
    });
  });
}

/** Replaces the namespace's exceptions to the defaults; its limits follow. */
export function useSetNamespaceLimits(name: string): Action<Quotas, NamespaceDetail> {
  const store = useStore();
  return useAction((exceptions: Quotas) => {
    const defaults =
      resources<NamespaceDefaults>(store, "defaults")[0]?.getSnapshot().data?.defaults ?? null;
    return optimistic({
      show: () =>
        showNamespace(store, name, (ns) => ({
          ...ns,
          exceptions,
          ...(defaults ? { limits: limitsOf(exceptions, defaults) } : {}),
        })),
      write: () => store.api.putNamespace(name, { exceptions }),
      reread: () => reread(store),
    });
  });
}

/** Replaces the default limits; the limits of every namespace without an exception follow. */
export function useSetNamespaceDefaults(): Action<Quotas, NamespaceDefaults> {
  const store = useStore();
  return useAction((defaults: Quotas) =>
    optimistic({
      show: () => {
        for (const r of resources<NamespaceDefaults>(store, "defaults"))
          r.change((d) => ({ ...d, defaults }));
        showNamespace(store, null, (ns) => ({ ...ns, limits: limitsOf(ns.exceptions, defaults) }));
      },
      write: () => store.api.putNamespaceDefaults({ defaults }),
      reread: () => reread(store),
    }),
  );
}

/**
 * Revokes the namespace's opaque tokens and refuses its signed ones issued before now; answers how
 * many were revoked.
 */
export function useRevokeNamespaceTokens(name: string): Action<void, RevokedTokens> {
  const store = useStore();
  return useAction(() =>
    optimistic({
      show: () =>
        showNamespace(
          store,
          name,
          (ns) => ns,
          (d) => ({ ...d, tokens: 0 }),
        ),
      write: () => store.api.revokeNamespaceTokens(name),
      reread: () => reread(store),
    }),
  );
}

/**
 * Erases the end user: the server revokes their tokens and cancels their active runs, then erases
 * their runs, journals, credentials and use in the background. Shown at once as being erased.
 */
export function useEraseNamespace(name: string): Action<void, void> {
  const store = useStore();
  return useAction(() => {
    const since = new Date().toISOString();
    return optimistic({
      show: () =>
        showNamespace(
          store,
          name,
          (ns) => ({ ...ns, deletingSince: ns.deletingSince ?? since }),
          (d) => ({ ...d, tokens: 0 }),
        ),
      write: () => store.api.deleteNamespace(name),
      reread: () => reread(store),
    });
  });
}
