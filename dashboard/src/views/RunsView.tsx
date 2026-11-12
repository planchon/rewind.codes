// Runs: every run the key may read, searched with the query language. The query lives in the URL;
// facets beside the results count each value among the runs it matches.
import { RefreshCw } from "lucide-react";
import { useSearchParams } from "react-router";
import { queryFault } from "../api/client";
import { useRunStats, useRuns } from "../api/queries";
import type { RunSort } from "../api/types";
import { Empty } from "../components/bits";
import { QueryBar } from "../components/QueryBar";
import { SaveSearch } from "../components/SaveSearch";
import { fieldTerm, labelTerm, toggleFacet, withTerm } from "../model/query";
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

      <QueryBar value={q} onSubmit={setQ} facets={stats.data?.facets} fault={fault}>
        <SaveSearch q={q} view="runs" />
      </QueryBar>

      <div className="search-body">
        <FacetList
          facets={fault ? undefined : stats.data?.facets}
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
            empty={q ? "No run matches this query." : "No run yet."}
          />
        )}
      </div>
    </div>
  );
}
