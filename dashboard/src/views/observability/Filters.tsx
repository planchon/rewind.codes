// The filters of Observability's sidebar, as on Vercel's Logs: the timeline, then a group per
// field with its values and their counts. A filter and the query are one (archi/08): a click on a
// value asks for it (its term is added to the query), a second excludes it, a third removes it.
import { Check, Minus } from "lucide-react";
import { useState } from "react";
import { useNamespaceSuggestions, useSettled } from "../../api/namespaces";
import type { Facets, FacetValue } from "../../api/types";
import { useAccess } from "../../app/access";
import { useNamespaceLabel } from "../../components/NamespaceName";
import type { RangeKey } from "../../model/filters";
import { formatCount, shortDigest } from "../../model/format";
import {
  type FacetState,
  facetState,
  facetValues,
  joinQuery,
  removeTerm,
  splitTerms,
} from "../../model/query";
import { WINDOW_WORDS, WINDOWS } from "../search";

/** The window a query's `started:>now-…` term names, if it names one of the timeline's. */
export function timelineOf(q: string): RangeKey | null {
  for (const t of splitTerms(q)) {
    if (t.kind !== "field" || t.field !== "started" || t.negated) continue;
    const m = /^>now-(\w+)$/.exec(t.operand);
    if (m && (WINDOWS as readonly string[]).includes(m[1] ?? "")) return m[1] as RangeKey;
  }
  return null;
}

/** The query with its timeline set to a window, or to any time. */
export function withTimeline(q: string, win: RangeKey | null): string {
  let next = q;
  for (const t of [...splitTerms(q)].reverse()) {
    if (t.kind === "field" && t.field === "started" && !t.negated && t.operand.startsWith(">now-"))
      next = removeTerm(next, t);
  }
  return win ? joinQuery(next, `started:>now-${win}`) : next.trim();
}

const STATE_WORD: Record<"include" | "exclude", string> = {
  include: "asked for; click to exclude it",
  exclude: "excluded; click to remove it",
};

interface Group {
  /** The field, or a label's key written `@key`. */
  field: string;
  title: string;
  values: FacetValue[];
  /** How a value is shown. */
  show?: (value: string) => string;
  /** A status's dot. */
  dot?: boolean;
  /** Open at first. */
  open?: boolean;
  /** Something to type, to find a value the counts do not list. */
  search?: "namespace";
}

function Checkbox({ state }: { state: FacetState }) {
  return (
    <span className={`checkbox${state ? ` ${state}` : ""}`} aria-hidden="true">
      {state === "include" && <Check size={12} strokeWidth={3} />}
      {state === "exclude" && <Minus size={12} strokeWidth={2.5} />}
    </span>
  );
}

/** The namespaces whose name starts with what is typed, the most active first. */
function NamespaceSearch({ onPick }: { onPick: (name: string) => void }) {
  const [typed, setTyped] = useState("");
  const prefix = useSettled(typed.trim() ? typed.trim() : null);
  const found = useNamespaceSuggestions(prefix);
  const names = found.data?.namespaces ?? [];
  return (
    <div className="filter-search">
      <input
        className="filter-search-input"
        aria-label="Find a namespace"
        placeholder="Find a namespace…"
        value={typed}
        onChange={(e) => setTyped(e.target.value)}
        spellCheck={false}
        autoComplete="off"
      />
      {prefix && names.length > 0 && (
        <ul className="filter-search-list" aria-label="Namespaces found">
          {names.map((n) => (
            <li key={n.name}>
              <button
                type="button"
                className="filter-row"
                onClick={() => {
                  onPick(n.name);
                  setTyped("");
                }}
              >
                <Checkbox state={null} />
                <span
                  className="filter-value"
                  title={n.displayName ? `Namespace ${n.name}` : undefined}
                >
                  {n.displayName ?? n.name}
                </span>
                <span className="filter-count">{formatCount(n.usage.runsStarted)}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function FilterGroup({
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
  if (rows.length === 0 && !group.search) return null;
  const picked = rows.filter((r) => r.state !== null).length;
  return (
    <details className="filter-group" open={group.open || picked > 0}>
      <summary>
        <span className="filter-title">{group.title}</span>
        {picked > 0 && <span className="filter-picked">{picked} selected</span>}
      </summary>
      {/* biome-ignore lint/a11y/useSemanticElements: a list of toggles named by its group */}
      <div className="filter-box" role="group" aria-label={group.title}>
        {rows.map((r) => {
          const shown = group.show?.(r.value) ?? r.value;
          return (
            <button
              key={r.value}
              type="button"
              className={`filter-row${r.state ? ` ${r.state}` : ""}`}
              aria-pressed={r.state === "include"}
              aria-label={`${group.title} ${shown}${r.runs !== null ? `, ${r.runs} runs` : ""}${r.state ? `, ${r.state === "include" ? "asked for" : "excluded"}` : ""}`}
              title={`${
                r.state ? STATE_WORD[r.state] : `Only runs with ${shown}; click again to exclude`
              }${shown !== r.value && group.field === "namespace" ? ` (namespace ${r.value})` : ""}`}
              onClick={() => onToggle(group.field, r.value)}
            >
              <Checkbox state={r.state} />
              {group.dot && <span className={`dot ${r.value}`} aria-hidden="true" />}
              <span className="filter-value">{shown}</span>
              <span className="filter-count">{r.runs === null ? "–" : formatCount(r.runs)}</span>
            </button>
          );
        })}
        {group.search === "namespace" && (
          <NamespaceSearch onPick={(name) => onToggle("namespace", name)} />
        )}
      </div>
    </details>
  );
}

export function FilterGroups({
  facets,
  q,
  onToggle,
}: {
  facets: Facets | undefined;
  q: string;
  onToggle: (field: string, value: string) => void;
}) {
  const access = useAccess();
  const label = useNamespaceLabel();
  const groups: Group[] = [
    { field: "status", title: "Status", values: facets?.statuses ?? [], dot: true, open: true },
    {
      field: "namespace",
      title: "Namespace",
      values: facets?.namespaces ?? [],
      open: true,
      show: (name: string) => label(name) ?? name,
      ...(access.can("namespaces:read") ? { search: "namespace" as const } : {}),
    },
    ...(facets?.labels ?? []).slice(0, 6).map((l, i) => ({
      field: `@${l.key}`,
      title: `@${l.key}`,
      values: l.values.slice(0, 8),
      open: i === 0,
    })),
    { field: "tool", title: "Tool", values: facets?.tools ?? [] },
    { field: "error", title: "Error", values: facets?.errors ?? [] },
    { field: "waiting", title: "Waiting for", values: facets?.waiting ?? [] },
    { field: "signal", title: "Signal", values: facets?.signals ?? [] },
    { field: "script", title: "Script", values: facets?.scripts ?? [], show: shortDigest },
  ];
  return (
    <div className="filters-list">
      {groups.map((g) => (
        <FilterGroup key={g.field} group={g} q={q} onToggle={onToggle} />
      ))}
    </div>
  );
}

/** The timeline: the runs started in a window, or at any time. */
export function TimelineSelect({
  value,
  onChange,
  any = true,
}: {
  value: RangeKey | null;
  onChange: (win: RangeKey | null) => void;
  /** Whether "any time" is offered. */
  any?: boolean;
}) {
  return (
    <details className="filter-group" open>
      <summary>
        <span className="filter-title">Timeline</span>
      </summary>
      <select
        className="select"
        aria-label="Timeline"
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value ? (e.target.value as RangeKey) : null)}
      >
        {any && <option value="">Any time</option>}
        {WINDOWS.map((w) => (
          <option key={w} value={w}>
            {WINDOW_WORDS[w].replace(/^the /, "The ")}
          </option>
        ))}
      </select>
    </details>
  );
}
