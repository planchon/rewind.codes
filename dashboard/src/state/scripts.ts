// Scripts: named by their digest, a script's source never changes. The list of the scripts the runs
// ran, each with its runs by outcome, is read again within 5 s of a run's change, every 60 s while
// the change stream is open, and every 30 s while it is down.
import { useQuery } from "@tanstack/react-query";
import type { ListScriptsQuery, Script, ScriptSummary } from "../model/types";
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
