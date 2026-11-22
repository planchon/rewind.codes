// Workflows: each by name with its latest version, a workflow's versions (each with its suite), the
// versions whose script is a given one, and the tests (archi/05, "Suites and the gate"). A
// workflow is saved by an agent, never by a run: no run's change touches them, and they are read
// again every 60 s while the change stream is open, every 30 s while it is down. A test is named
// by its digest: it never changes.
import { useQuery } from "@tanstack/react-query";
import type { Test, Workflow } from "../model/types";
import { type PagedRead, usePaged } from "./paged";
import { type Read, useQueryRead, useResource } from "./read";
import { useStore } from "./store";

const POLL = 30_000;

export type WorkflowList = PagedRead<Workflow, null>;

/** The workflows, by name: the latest version of each, page after page. */
export function useWorkflowList(): WorkflowList {
  const { api } = useStore();
  return usePaged("workflows|list", () => ({
    kind: "polled",
    pollMs: POLL,
    pageSize: 50,
    keyOf: (w: Workflow) => w.name,
    readPage: async (after, limit, signal) => {
      const page = await api.workflows({ limit, ...(after ? { after } : {}) }, signal);
      return { rows: page.workflows, next: page.next ?? null, head: null };
    },
  }));
}

/** A workflow's versions, newest first; `null` reads none. */
export function useWorkflowVersions(name: string | null): Read<Workflow[]> {
  const { api } = useStore();
  return useResource(name === null ? null : `workflows|versions|${name}`, () => ({
    kind: "polled",
    pollMs: POLL,
    load: (signal) => api.workflowVersions(name ?? "", signal),
  }));
}

/** The versions of any workflow whose script is `script`, newest first; `enabled` false reads none. */
export function useWorkflowsWithScript(script: string, enabled = true): Read<Workflow[]> {
  const { api } = useStore();
  return useResource(enabled ? `workflows|script|${script}` : null, () => ({
    kind: "polled",
    pollMs: POLL,
    load: async (signal) => (await api.workflows({ script }, signal)).workflows,
  }));
}

/** A stored test; `null` reads none. */
export function useTest(digest: string | null): Read<Test> {
  const { api } = useStore();
  return useQueryRead(
    useQuery({
      queryKey: ["test", digest],
      queryFn: ({ signal }) => api.test(digest ?? "", signal),
      enabled: digest !== null,
      staleTime: Number.POSITIVE_INFINITY,
    }),
  );
}
