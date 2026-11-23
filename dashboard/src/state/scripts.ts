// Scripts: named by their digest, a script's source never changes. The list of the scripts the runs
// ran, each with its runs by outcome, is read again within 5 s of a run's change, every 60 s while
// the change stream is open, and every 30 s while it is down.
import { useQuery } from "@tanstack/react-query";
import { useCallback } from "react";
import type { ListScriptsQuery, Script, ScriptSummary, SetScriptTitle } from "../model/types";
import { type Action, useAction } from "./actions";
import { type PagedRead, usePaged } from "./paged";
import { type Read, useQueryRead } from "./read";
import { useStore } from "./store";

export function useScript(digest: string | undefined): Read<Script> {
  const { api } = useStore();
  return useQueryRead(
    useQuery({
      queryKey: ["script", digest],
      queryFn: ({ signal }) => api.script(digest ?? "", signal),
      enabled: Boolean(digest),
      staleTime: Number.POSITIVE_INFINITY,
    }),
  );
}

export interface ScriptListOptions {
  /** What `GET /v1/scripts` filters by: labels, one tool; `limit` is the page size (50). */
  query: Omit<ListScriptsQuery, "cursor">;
  /** One end user's scripts, counted over their runs (`Rewind-Namespace`); every one's if `null`. */
  namespace: string | null;
  /** `false` reads nothing (a query at fault). */
  enabled?: boolean;
}

export type ScriptList = PagedRead<ScriptSummary, null>;

/** The distinct scripts the runs ran, the most recently run first, page after page. */
export function useScriptList({ query, namespace, enabled = true }: ScriptListOptions): ScriptList {
  const { api } = useStore();
  const { limit, ...filter } = query;
  return usePaged(enabled ? `scripts|${JSON.stringify([filter, namespace, limit])}` : null, () => ({
    kind: "aggregate",
    pollMs: 30_000,
    pageSize: limit ?? 50,
    keyOf: (s: ScriptSummary) => s.digest,
    readPage: async (after, pageLimit, signal) => {
      const page = await api.scripts(
        { ...filter, limit: pageLimit, ...(after ? { cursor: after } : {}) },
        signal,
        namespace,
      );
      return { rows: page.scripts, next: page.nextCursor ?? null, head: null };
    },
  }));
}

const TITLES = ["script-titles"] as const;

/**
 * The title of each script the runs ran, by digest, for the menus that list scripts by their
 * digest (a facet's values): a script without a title has none here.
 */
export function useScriptTitles(): (digest: string) => string | undefined {
  const { api } = useStore();
  const titles = useQuery({
    queryKey: TITLES,
    queryFn: async ({ signal }) => {
      const page = await api.scripts({ limit: 500 }, signal);
      return new Map(page.scripts.flatMap((s) => (s.title ? [[s.digest, s.title] as const] : [])));
    },
    staleTime: 60_000,
  });
  const data = titles.data;
  return useCallback((digest: string) => data?.get(digest), [data]);
}

/**
 * Gives a script a title and a description for the organization, or removes them (`title:
 * null`): the comment's show again. The script, the lists of scripts and the runs are read again.
 */
export function useSetScriptTitle(digest: string): Action<SetScriptTitle, Script> {
  const store = useStore();
  return useAction(async (body: SetScriptTitle) => {
    const script = await store.api.setScriptTitle(digest, body);
    store.queryClient.setQueryData(["script", digest], script);
    await Promise.all([
      store.queryClient.invalidateQueries({ queryKey: TITLES }),
      ...store
        .resourcesUnder("scripts|")
        .filter((r) => r.held)
        .map((r) => r.refreshNow()),
      ...store
        .resourcesUnder("run|")
        .filter((r) => r.held)
        .map((r) => r.refreshNow()),
    ]);
    return script;
  });
}
