// Observability's table of runs within a scope a page forces (a namespace's runs of a month, a
// script's runs): its headers order and filter the runs, the terms they add are chips above it,
// and both are kept in the page's URL (`q`, `sort`). The scope is shown, and cannot be removed.
import { Lock, X } from "lucide-react";
import type { ReactNode } from "react";
import { Link, useSearchParams } from "react-router";
import { Empty } from "../components/bits";
import {
  fieldTerm,
  joinQuery,
  labelTerm,
  queryLink,
  removeTerm,
  splitTerms,
  withTerm,
} from "../model/query";
import { readSort, withSort } from "../model/sorts";
import { useRunStats } from "../state/aggregates";
import { queryFault } from "../state/errors";
import { useRunSearch } from "../state/runs";
import { RunTable, type RunTableControls } from "./RunTable";
import { useSearchQuery } from "./search";

/** A term of the scope, as its chip says it. */
export interface Locked {
  label: ReactNode;
  title: string;
}

/** The scope (locked), then the terms the headers added, each removable. */
export function QueryChips({
  locked,
  q,
  onQuery,
}: {
  locked: readonly Locked[];
  q: string;
  onQuery: (q: string) => void;
}) {
  const terms = splitTerms(q);
  if (locked.length === 0 && terms.length === 0) return null;
  return (
    // biome-ignore lint/a11y/useSemanticElements: the filters of the table, named
    <div className="query-chips" role="group" aria-label="Filters">
      {locked.map((l) => (
        <span key={l.title} className="query-chip locked" title={l.title}>
          <Lock size={11} aria-hidden="true" />
          {l.label}
        </span>
      ))}
      {terms.map((t) => (
        <span key={`${t.start}:${t.raw}`} className="query-chip">
          <span className="mono">{t.raw}</span>
          <button
            type="button"
            className="query-chip-remove"
            aria-label={`Remove the filter ${t.raw}`}
            title="Remove this filter"
            onClick={() => onQuery(removeTerm(q, t))}
          >
            <X size={11} />
          </button>
        </span>
      ))}
      {terms.length > 0 && (
        <button type="button" className="link-btn query-chips-clear" onClick={() => onQuery("")}>
          Clear the filters
        </button>
      )}
    </div>
  );
}

export function ScopedRuns({
  name,
  label,
  title,
  scope,
  locked,
  limit = 20,
  refetchMs = 15_000,
  hideScript = false,
  empty,
}: {
  /** Names the scope's counts in the state: one per page and scope. */
  name: string;
  /** What the section is, for assistive technologies. */
  label: string;
  title: ReactNode;
  /** The terms the page forces. */
  scope: string;
  locked: readonly Locked[];
  limit?: number;
  /** How often the runs are read while the change stream is down. */
  refetchMs?: number;
  hideScript?: boolean;
  /** What the table says when the scope has no run (the filters aside). */
  empty: string;
}) {
  const { q, setQ } = useSearchQuery();
  const [params, setParams] = useSearchParams();
  const sort = readSort(params);
  const asked = joinQuery(scope, q);
  const runs = useRunSearch({ q: asked, sort, pageSize: limit, pollMs: refetchMs });
  const stats = useRunStats(`${name}|facets|${q}`, () => ({ q: asked, top: 10 }), refetchMs);
  const controls: RunTableControls = {
    q,
    onQuery: setQ,
    sort,
    onSort: (next) => setParams((current) => withSort(current, next)),
    facets: stats.data?.facets,
  };
  const add = (term: string) => setQ(withTerm(q, term));
  const fault = queryFault(runs.error);
  return (
    <section className="scoped-runs" aria-label={label}>
      <header className="scoped-runs-head">
        <h2>{title}</h2>
        <Link to={queryLink("/observability", asked, sort !== "started" ? { sort } : {})}>
          Open in Observability →
        </Link>
      </header>
      <QueryChips locked={locked} q={q} onQuery={setQ} />
      {fault ? (
        <Empty>A filter cannot be read: {fault.message}. Remove it above.</Empty>
      ) : (
        <RunTable
          search={runs}
          controls={controls}
          hideScript={hideScript}
          onPickLabel={(pair) => {
            const at = pair.indexOf(":");
            add(labelTerm(pair.slice(0, at), pair.slice(at + 1)));
          }}
          {...(hideScript
            ? {}
            : { onPickScript: (digest: string) => add(fieldTerm("script", digest)) })}
          empty={q ? "No run matches these filters." : empty}
        />
      )}
    </section>
  );
}
