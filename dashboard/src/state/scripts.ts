// Scripts: named by their digest; a script's source never changes, its title and description do.
// The list of the scripts the runs ran, each with its runs by outcome, from the hourly counters, is
// read again within 10 s of a run's change; a `script` event (a script run for the first time, or
// given a title) reads again the lists, the script and the titles.
import { useQuery } from "@tanstack/react-query";
import { useCallback } from "react";
import { RANGES, type RangeKey } from "../model/filters";
import type { ListScriptsQuery, Script, ScriptSummary, SetScriptTitle } from "../model/types";
import { useAccess } from "./access";
import { type Action, useAction } from "./actions";
import { type PagedRead, usePaged } from "./paged";
import { SCRIPT_TITLES } from "./queries";
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
  /** What `GET /v1/scripts` filters by: one tool; `limit` is the page size (50). */
  query: Omit<ListScriptsQuery, "cursor" | "since" | "until">;
  /**
   * The scripts of the runs started in this window, up to now: read again at each page, the
   * server rounding it out to whole hours. Every run's without it.
   */
  range?: RangeKey | undefined;
  /** One end user's scripts, counted over their runs (`Rewind-Namespace`); every one's if `null`. */
  namespace: string | null;
  /** `false` reads nothing (a query at fault). */
  enabled?: boolean;
}

export type ScriptList = PagedRead<ScriptSummary, null>;

/** The distinct scripts the runs ran, the most recently run first, page after page. */
export function useScriptList({
  query,
  range,
  namespace,
  enabled = true,
}: ScriptListOptions): ScriptList {
  const { api } = useStore();
  const { limit, ...filter } = query;
  const key = JSON.stringify([filter, range ?? null, namespace, limit]);
  return usePaged(enabled ? `scripts|${key}` : null, () => ({
    kind: "aggregate",
    controls: (change) => change.kind === "script",
    pageSize: limit ?? 50,
    keyOf: (s: ScriptSummary) => s.digest,
    readPage: async (after, pageLimit, signal) => {
      const since = range ? { since: new Date(Date.now() - RANGES[range]).toISOString() } : {};
      const page = await api.scripts(
        { ...filter, ...since, limit: pageLimit, ...(after ? { cursor: after } : {}) },
        signal,
        namespace,
      );
      return { rows: page.scripts, next: page.nextCursor ?? null, head: null };
    },
  }));
}

/**
 * The title of each script the runs ran, by digest, for the menus that list scripts by their
 * digest (a facet's values): a script without a title has none here. The list of scripts comes
 * from the statistics, which a grant limited to labels does not read: its menus show digests.
 */
export function useScriptTitles(): (digest: string) => string | undefined {
  const { api } = useStore();
  const { statistics } = useAccess();
  const titles = useQuery({
    queryKey: SCRIPT_TITLES,
    queryFn: async ({ signal }) => {
      const page = await api.scripts({ limit: 500 }, signal);
      return new Map(page.scripts.flatMap((s) => (s.title ? [[s.digest, s.title] as const] : [])));
    },
    enabled: statistics,
    // Read again on a `script` change only (`queries.ts`).
    staleTime: Number.POSITIVE_INFINITY,
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
      store.queryClient.invalidateQueries({ queryKey: SCRIPT_TITLES }),
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
