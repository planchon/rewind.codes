// Workflows (archi/01, "Workflows"; archi/05, "Workflows"): one entity each, by its id, with an
// optional name unique in its scope, versions numbered from 1 and a suite. The list holds each
// workflow with its latest version and its runs of a window, from the hourly counters: read again
// within 10 s of a run's change, and when the change stream announces a workflow (named,
// described, given a version or tests, promoted, forked, pulled), as is a workflow's page. A test
// is named by its digest: it never changes.
import { useQuery } from "@tanstack/react-query";
import { useCallback } from "react";
import { RANGES, type RangeKey } from "../model/filters";
import type {
  ControlChange,
  ListWorkflowsQuery,
  PatchWorkflow,
  Test,
  Workflow,
  WorkflowSummary,
} from "../model/types";
import { workflowLabel } from "../model/workflows";
import { useAccess } from "./access";
import { type Action, useAction } from "./actions";
import { type PagedRead, usePaged } from "./paged";
import { WORKFLOW_LABELS } from "./queries";
import { type Read, useQueryRead, useResource } from "./read";
import { type Store, useStore } from "./store";

/** Any workflow's change. */
const anyWorkflow = (change: ControlChange) => change.kind === "workflow";

export interface WorkflowListOptions {
  /** What `GET /v1/workflows` takes besides the window: the scope of its counts; `limit`, 50. */
  query: Omit<ListWorkflowsQuery, "since" | "until" | "after">;
  /** The runs counted are those started in this window, up to now: read again at each page. */
  range?: RangeKey | undefined;
  /** One end user's workflows and the organization's (`Rewind-Namespace`); every one's if `null`. */
  namespace: string | null;
  /** `false` reads nothing (a query at fault). */
  enabled?: boolean;
}

export type WorkflowList = PagedRead<WorkflowSummary, null>;

/** The workflows, the most recently run first, page after page. */
export function useWorkflowList({
  query,
  range,
  namespace,
  enabled = true,
}: WorkflowListOptions): WorkflowList {
  const { api } = useStore();
  const { limit, ...filter } = query;
  const key = JSON.stringify([filter, range ?? null, namespace, limit]);
  return usePaged(enabled ? `workflows|${key}` : null, () => ({
    kind: "aggregate",
    controls: anyWorkflow,
    pageSize: limit ?? 50,
    keyOf: (w: WorkflowSummary) => w.id,
    readPage: async (after, pageLimit, signal) => {
      const since = range ? { since: new Date(Date.now() - RANGES[range]).toISOString() } : {};
      const page = await api.workflows(
        { ...filter, ...since, limit: pageLimit, ...(after ? { after } : {}) },
        signal,
        namespace,
      );
      return { rows: page.workflows, next: page.next ?? null, head: null };
    },
  }));
}

/**
 * A workflow, by its id or its name, at its latest version or at version `version`: its versions,
 * its suite and the code of the version shown. `null` reads none.
 */
export function useWorkflow(ref: string | null, version: number | null = null): Read<Workflow> {
  const { api } = useStore();
  const at = ref === null ? null : version ? `${ref}@v${version}` : ref;
  return useResource(at === null ? null : `workflow|${at}`, () => ({
    kind: "control",
    // The change names the workflow by its id; a page reached by its name reads it again too.
    controls: anyWorkflow,
    load: (signal) => api.workflow(at ?? "", signal),
  }));
}

interface Shown {
  label: string;
  /** Its namespace; `null` for the organization's. */
  namespace: string | null;
}

/** Each workflow's label and scope, by id: one read of the list, again on a workflow's change. */
function useShownWorkflows(): Map<string, Shown> | undefined {
  const { api } = useStore();
  const access = useAccess();
  return useQuery({
    queryKey: WORKFLOW_LABELS,
    queryFn: async ({ signal }) => {
      const page = await api.workflows({ limit: 500 }, signal);
      return new Map(
        page.workflows.map((w) => [
          w.id,
          {
            label: workflowLabel({ ...w, comment: w.latest.comment }),
            namespace: w.namespace ?? null,
          },
        ]),
      );
    },
    enabled: access.can("runs:read"),
    staleTime: Number.POSITIVE_INFINITY,
  }).data;
}

/**
 * What each workflow shows as (its name, its comment's first line, or its short id), by id, for
 * the tables and menus that name a run's workflow by its id. Read once, and again when a workflow
 * changes (`queries.ts`).
 */
export function useWorkflowLabels(): (id: string) => string | undefined {
  const data = useShownWorkflows();
  return useCallback((id: string) => data?.get(id)?.label, [data]);
}

/**
 * The scope of each workflow, by id: its namespace, `null` for the organization's, `undefined`
 * when not known. Tells apart a fork from its source, which keeps its name.
 */
export function useWorkflowScopes(): (id: string) => string | null | undefined {
  const data = useShownWorkflows();
  return useCallback((id: string) => data?.get(id)?.namespace, [data]);
}

/** Reads again what shows a workflow: its pages, the lists and the labels. */
async function refreshWorkflows(store: Store): Promise<void> {
  await Promise.all([
    store.queryClient.invalidateQueries({ queryKey: WORKFLOW_LABELS }),
    ...store
      .resourcesUnder("workflow")
      .filter((r) => r.held)
      .map((r) => r.refreshNow()),
  ]);
}

/** Names or describes a workflow (`null` removes one): the same id, versions and runs. */
export function usePatchWorkflow(id: string): Action<PatchWorkflow, Workflow> {
  const store = useStore();
  return useAction(async (body: PatchWorkflow) => {
    const workflow = await store.api.patchWorkflow(id, body);
    await refreshWorkflows(store);
    return workflow;
  });
}

/** Moves a namespace's workflow to the organization. */
export function usePromoteWorkflow(id: string): Action<void, Workflow> {
  const store = useStore();
  return useAction(async () => {
    const workflow = await store.api.promoteWorkflow(id);
    await refreshWorkflows(store);
    return workflow;
  });
}

/** Forks an organization's workflow into a namespace: the fork keeps its name. */
export function useForkWorkflow(id: string): Action<string, Workflow> {
  const store = useStore();
  return useAction(async (namespace: string) => {
    const workflow = await store.api.forkWorkflow(id, namespace);
    await refreshWorkflows(store);
    return workflow;
  });
}

/** Takes the source's latest version into a fork, once the fork's own suite passes. */
export function usePullWorkflow(id: string): Action<void, Workflow> {
  const store = useStore();
  return useAction(async () => {
    const workflow = await store.api.pullWorkflow(id);
    await refreshWorkflows(store);
    return workflow;
  });
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
