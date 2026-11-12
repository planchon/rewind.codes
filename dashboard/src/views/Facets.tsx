// The counts of each value among the runs a query matches, beside the results. A click asks for
// the value, a second excludes it, a third removes it.
import type { Facets as FacetCounts, FacetValue } from "../api/types";
import { formatCount, shortDigest } from "../model/format";
import { type FacetState, facetState, facetValues } from "../model/query";

interface Group {
  /** The facet: a field, or a label's key written `@key`. */
  field: string;
  title: string;
  values: FacetValue[];
  /** How a value is shown. */
  show?: (value: string) => string;
}

const STATE_WORD: Record<"include" | "exclude", string> = {
  include: "asked for; click to exclude it",
  exclude: "excluded; click to remove it",
};

function FacetGroup({
  group,
  q,
  onToggle,
}: {
  group: Group;
  q: string;
  onToggle: (field: string, value: string) => void;
}) {
  // The values the query names stay, even when no run has them any more (an excluded value).
  const named = facetValues(q, group.field).filter(
    (n) => !group.values.some((v) => v.value === n.value),
  );
  const rows: { value: string; runs: number | null; state: FacetState }[] = [
    ...group.values.map((v) => ({
      value: v.value,
      runs: v.runs,
      state: facetState(q, group.field, v.value),
    })),
    ...named.map((n) => ({ value: n.value, runs: null, state: n.state })),
  ];
  if (rows.length === 0) return null;
  return (
    <section className="facet" aria-label={group.title}>
      <h3>{group.title}</h3>
      <ul>
        {rows.map((r) => {
          const shown = group.show?.(r.value) ?? r.value;
          return (
            <li key={r.value}>
              <button
                type="button"
                className={`facet-value${r.state ? ` ${r.state}` : ""}`}
                aria-pressed={r.state === "include"}
                aria-label={`${group.title} ${shown}${r.runs !== null ? `, ${r.runs} runs` : ""}${r.state ? `, ${r.state === "include" ? "asked for" : "excluded"}` : ""}`}
                title={
                  r.state ? STATE_WORD[r.state] : `Only runs with ${shown}; click again to exclude`
                }
                onClick={() => onToggle(group.field, r.value)}
              >
                <span className="facet-name mono">
                  {r.state === "exclude" && <span aria-hidden="true">−</span>}
                  {shown}
                </span>
                <span className="num muted">{r.runs === null ? "–" : formatCount(r.runs)}</span>
              </button>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

export function FacetList({
  facets,
  q,
  onToggle,
}: {
  facets: FacetCounts | undefined;
  q: string;
  onToggle: (field: string, value: string) => void;
}) {
  const groups: Group[] = [
    { field: "status", title: "Status", values: facets?.statuses ?? [] },
    ...(facets?.labels ?? []).slice(0, 6).map((l) => ({
      field: `@${l.key}`,
      title: `@${l.key}`,
      values: l.values.slice(0, 6),
    })),
    { field: "tool", title: "Tool", values: facets?.tools ?? [] },
    { field: "error", title: "Error", values: facets?.errors ?? [] },
    { field: "waiting", title: "Waiting for", values: facets?.waiting ?? [] },
    { field: "signal", title: "Signal", values: facets?.signals ?? [] },
    { field: "script", title: "Script", values: facets?.scripts ?? [], show: shortDigest },
  ];
  return (
    <aside className="facets" aria-label="Facets">
      {groups.map((g) => (
        <FacetGroup key={g.field} group={g} q={q} onToggle={onToggle} />
      ))}
    </aside>
  );
}
