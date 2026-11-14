// Runs: every run the key may read, searched with the query language. The query lives in the URL;
// facets beside the results count each value among the runs it matches. Over every namespace,
// each run shows its namespace, and the query bar suggests the namespaces as `namespace:` is typed.
import { RefreshCw } from "lucide-react";
import { useCallback, useMemo, useState } from "react";
import { useSearchParams } from "react-router";
import { queryFault } from "../api/client";
import { useNamespaceSuggestions, useSettled } from "../api/namespaces";
import { useRunStats, useRuns } from "../api/queries";
import type { FacetValue, RunPage, RunSort } from "../api/types";
import { useAccess } from "../app/access";
import { useNamespace } from "../app/namespace";
import { Empty } from "../components/bits";
import { QueryBar } from "../components/QueryBar";
import { SaveSearch } from "../components/SaveSearch";
import { formatCount } from "../model/format";
import { type FieldValues, fieldTerm, labelTerm, toggleFacet, withTerm } from "../model/query";
import { FacetList } from "./Facets";
import { Seg } from "./FilterBits";
import { RunTable } from "./RunTable";
import { useSearchQuery } from "./search";

const SORTS: { value: RunSort; label: string; title: string }[] = [
  { value: "started", label: "Started", title: "The most recently started first" },
  { value: "activity", label: "Activity", title: "The most recent activity first" },
  { value: "wakes", label: "Wakes", title: "The soonest to wake by itself first" },
];

function readSort(params: URLSearchParams): RunSort {
  const sort = params.get("sort");
  return sort === "activity" || sort === "wakes" ? sort : "started";
}

/** The namespaces of the runs shown, the most frequent first, until the statistics answer. */
function namespacesShown(pages: readonly RunPage[] | undefined): FacetValue[] {
  const counts = new Map<string, number>();
  for (const page of pages ?? [])
    for (const run of page.runs) counts.set(run.namespace, (counts.get(run.namespace) ?? 0) + 1);
  return [...counts]
    .sort(([a, x], [b, y]) => y - x || a.localeCompare(b))
    .slice(0, 10)
    .map(([value, runs]) => ({ value, runs }));
}

type Typed = { field: string; value: string } | null;

/**
 * The namespaces to suggest after `namespace:`: those whose name starts with what is typed, read
 * from their list as it is typed, else those of the runs shown.
 */
function useNamespaceValues(all: boolean, shown: FacetValue[]) {
  const access = useAccess();
  const [typed, setTyped] = useState<Typed>(null);
  const onTyping = useCallback(
    (next: Typed) =>
      setTyped((prev) =>
        prev?.field === next?.field && prev?.value === next?.value ? prev : next,
      ),
    [],
  );
  const prefix = useSettled(
    all && access.can("namespaces:read") && typed?.field === "namespace" ? typed.value : null,
  );
  const listed = useNamespaceSuggestions(prefix);
  const values = useMemo<FieldValues | undefined>(() => {
    if (!all) return undefined;
    const fromList = (listed.data?.namespaces ?? []).map((n) => ({
      value: n.name,
      detail: `${formatCount(n.usage.runsStarted)} runs this month`,
    }));
    const rest = shown
      .filter((v) => !fromList.some((n) => n.value === v.value))
      .map((v) => ({ value: v.value, count: v.runs }));
    return { namespace: [...fromList, ...rest] };
  }, [all, listed.data, shown]);
  return { values, onTyping };
}

export function RunsView() {
  const { q, setQ } = useSearchQuery();
  const [params, setParams] = useSearchParams();
  const sort = readSort(params);
  const runs = useRuns(`${sort}|${q}`, () => ({
    ...(q ? { q } : {}),
    ...(sort !== "started" ? { sort } : {}),
    limit: 50,
  }));
  const stats = useRunStats(`facets|${q}`, () => ({ ...(q ? { q } : {}), top: 10 }), 30_000);
  const fault = queryFault(runs.error) ?? queryFault(stats.error);
  const all = useNamespace() === null;
  const counted = stats.data?.facets.namespaces;
  const shown = useMemo(
    () => (all ? (counted?.length ? counted : namespacesShown(runs.data?.pages)) : []),
    [all, counted, runs.data?.pages],
  );
  const { values, onTyping } = useNamespaceValues(all, shown);

  const add = (term: string) => setQ(withTerm(q, term));

  return (
    <div className="page">
      <div className="page-head">
        <h1>Runs</h1>
        <Seg<RunSort>
          label="Order"
          value={sort}
          onChange={(v) =>
            setParams((current) => {
              const next = new URLSearchParams(current);
              if (v === "started") next.delete("sort");
              else next.set("sort", v);
              return next;
            })
          }
          options={SORTS}
        />
        <div className="actions">
          <button
            type="button"
            className="btn ghost"
            onClick={() => {
              void runs.refetch();
              void stats.refetch();
            }}
            disabled={runs.isFetching}
          >
            <RefreshCw size={13} className={runs.isFetching ? "spin" : undefined} /> Refresh
          </button>
        </div>
      </div>

      <QueryBar
        value={q}
        onSubmit={setQ}
        facets={stats.data?.facets}
        values={values}
        onTyping={onTyping}
        fault={fault}
      >
        <SaveSearch q={q} view="runs" />
      </QueryBar>

      <div className="search-body">
        <FacetList
          facets={fault ? undefined : stats.data?.facets}
          namespaces={all && !fault ? shown : undefined}
          q={q}
          onToggle={(field, value) => setQ(toggleFacet(q, field, value))}
        />
        {fault ? (
          <Empty>Fix the query to see its runs: the term at fault is underlined above.</Empty>
        ) : (
          <RunTable
            query={runs}
            onPickLabel={(pair) => {
              const at = pair.indexOf(":");
              add(labelTerm(pair.slice(0, at), pair.slice(at + 1)));
            }}
            onPickScript={(digest) => add(fieldTerm("script", digest))}
            showNamespace={all}
            onPickNamespace={(namespace) => add(fieldTerm("namespace", namespace))}
            empty={q ? "No run matches this query." : "No run yet."}
          />
        )}
      </div>
    </div>
  );
}
