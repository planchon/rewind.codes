// Runs: every run the key may read, filtered by status, label, script, tool, error and time.
import { RefreshCw } from "lucide-react";
import { useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router";
import { useRuns, useToolStats } from "../api/queries";
import { RUN_STATUSES, type RunStatus, TOOL_FAILURE_KINDS } from "../api/types";
import { useSettings } from "../app/settings";
import { StatusGlyph } from "../components/Status";
import {
  countRunFilters,
  RANGES,
  type RangeKey,
  type RunFilters,
  readRunFilters,
  toRunsQuery,
  writeRunFilters,
} from "../model/filters";
import type { TimeZoneMode } from "../model/format";
import { AddFilter, type FieldSpec, FilterChip, Seg } from "./FilterBits";
import { RunTable } from "./RunTable";

const ERROR_NAMES = [
  ...TOOL_FAILURE_KINDS,
  "ToolError",
  "TypeError",
  "DeadlockError",
  "CancelledError",
  "limit_exceeded",
  "non_determinism",
  "invalid_script",
  "internal",
];

const HEX = /^[0-9a-f]{6,64}$/;

/** `datetime-local` values read in the zone the dashboard shows. */
function toIso(local: string, zone: TimeZoneMode): string | null {
  if (!local) return null;
  const ms = zone === "utc" ? Date.parse(`${local}Z`) : new Date(local).getTime();
  return Number.isNaN(ms) ? null : new Date(ms).toISOString();
}

function toLocal(iso: string | null, zone: TimeZoneMode): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const shifted = zone === "utc" ? d : new Date(d.getTime() - d.getTimezoneOffset() * 60_000);
  return shifted.toISOString().slice(0, 16);
}

export function RunsView() {
  const [params, setParams] = useSearchParams();
  const filters = readRunFilters(params);
  const key = writeRunFilters(filters).toString();
  const runs = useRuns(key, () => toRunsQuery(filters, Date.now()));
  const { zone } = useSettings();
  const [custom, setCustom] = useState(filters.after !== null || filters.before !== null);
  const tools = useToolStats("suggestions:7d", () => ({
    since: new Date(Date.now() - RANGES["7d"]).toISOString(),
  }));

  // The filters this view wrote that the URL does not show yet, oldest first. A change applies
  // to the last of them: two changes made before the view renders again both apply.
  const writes = useRef<{ shown: string; pending: { key: string; filters: RunFilters }[] }>({
    shown: key,
    pending: [],
  });
  if (writes.current.shown !== key) {
    const at = writes.current.pending.findIndex((w) => w.key === key);
    // Another URL (back, a link) drops what was pending.
    writes.current = { shown: key, pending: at >= 0 ? writes.current.pending.slice(at + 1) : [] };
  }
  const update = (patch: Partial<RunFilters> | ((current: RunFilters) => Partial<RunFilters>)) => {
    const current = writes.current.pending.at(-1)?.filters ?? filters;
    const next = { ...current, ...(typeof patch === "function" ? patch(current) : patch) };
    const written = writeRunFilters(next);
    writes.current.pending.push({ key: written.toString(), filters: next });
    setParams(written, { replace: false });
  };
  const addLabel = (pair: string) =>
    update((current) =>
      current.labels.includes(pair) ? {} : { labels: [...current.labels, pair] },
    );

  const seenLabels = useMemo(() => {
    const pairs = new Set<string>();
    for (const page of runs.data?.pages ?? [])
      for (const r of page.runs)
        for (const [k, v] of Object.entries(r.labels)) pairs.add(`${k}:${v}`);
    return [...pairs].sort();
  }, [runs.data]);

  const fields: FieldSpec[] = [
    {
      name: "label",
      placeholder: "key:value",
      check: (v) => (v.indexOf(":") > 0 ? null : "A label filter is key:value"),
      suggestions: seenLabels,
    },
    {
      name: "tool",
      placeholder: "github.close_issue",
      suggestions: tools.data?.tools.map((t) => t.tool) ?? [],
    },
    { name: "error", placeholder: "timeout, DeadlockError…", suggestions: ERROR_NAMES },
    {
      name: "script",
      placeholder: "script digest",
      check: (v) => (HEX.test(v) ? null : "A script digest is hexadecimal"),
    },
    { name: "parent", placeholder: "run id: its forks" },
  ];

  const range: RangeKey | "all" | "custom" = custom ? "custom" : (filters.range ?? "all");

  return (
    <div className="page">
      <div className="page-head">
        <h1>Runs</h1>
        <span className="muted">most recently started first</span>
        <div className="actions">
          <button
            type="button"
            className="btn ghost"
            onClick={() => void runs.refetch()}
            disabled={runs.isFetching}
          >
            <RefreshCw size={13} className={runs.isFetching ? "spin" : undefined} /> Refresh
          </button>
        </div>
      </div>

      <section className="filters" aria-label="Filters">
        <Seg<RunStatus | "any">
          label="Status"
          value={filters.status ?? "any"}
          onChange={(v) => update({ status: v === "any" ? null : v })}
          options={[
            { value: "any", label: "Any status" },
            ...RUN_STATUSES.map((s) => ({
              value: s,
              label: (
                <>
                  <span className={`status ${s}`}>
                    <StatusGlyph status={s} size={12} />
                  </span>
                  {s}
                </>
              ),
            })),
          ]}
        />
        <button
          type="button"
          className="btn"
          aria-pressed={filters.quarantined === true}
          onClick={() =>
            update((current) => ({ quarantined: current.quarantined === true ? null : true }))
          }
          style={
            filters.quarantined === true
              ? { background: "var(--warning-soft)", borderColor: "var(--warning-mark)" }
              : undefined
          }
        >
          Quarantined only
        </button>
        <span className="sep" />
        <Seg<RangeKey | "all" | "custom">
          label="Started"
          value={range}
          onChange={(v) => {
            if (v === "custom") {
              setCustom(true);
              update({ range: null });
            } else {
              setCustom(false);
              update({ range: v === "all" ? null : v, after: null, before: null });
            }
          }}
          options={[
            { value: "1h", label: "1h", title: "Started in the last hour" },
            { value: "24h", label: "24h", title: "Started in the last 24 hours" },
            { value: "7d", label: "7d", title: "Started in the last 7 days" },
            { value: "30d", label: "30d", title: "Started in the last 30 days" },
            { value: "all", label: "All time" },
            { value: "custom", label: "Between…" },
          ]}
        />
        {custom && (
          <span className="field">
            <input
              className="input"
              type="datetime-local"
              aria-label={`Started after (${zone === "utc" ? "UTC" : "local time"})`}
              value={toLocal(filters.after, zone)}
              onChange={(e) => update({ after: toIso(e.target.value, zone) })}
            />
            <span className="muted">→</span>
            <input
              className="input"
              type="datetime-local"
              aria-label={`Started before (${zone === "utc" ? "UTC" : "local time"})`}
              value={toLocal(filters.before, zone)}
              onChange={(e) => update({ before: toIso(e.target.value, zone) })}
            />
            <span className="muted">{zone === "utc" ? "UTC" : "local"}</span>
          </span>
        )}
        <span className="sep" />
        <AddFilter
          fields={fields}
          onAdd={(field, value) => {
            if (field === "label") addLabel(value);
            else update({ [field]: value } as Partial<RunFilters>);
          }}
        />
        {(filters.labels.length > 0 ||
          filters.tool ||
          filters.error ||
          filters.script ||
          filters.parent) && (
          <div className="filter-chips" style={{ flexBasis: "100%" }}>
            {filters.labels.map((pair) => (
              <FilterChip
                key={pair}
                name="label"
                value={pair}
                onRemove={() =>
                  update((current) => ({ labels: current.labels.filter((l) => l !== pair) }))
                }
              />
            ))}
            {filters.tool && (
              <FilterChip
                name="tool"
                value={filters.tool}
                onRemove={() => update({ tool: null })}
              />
            )}
            {filters.error && (
              <FilterChip
                name="error"
                value={filters.error}
                onRemove={() => update({ error: null })}
              />
            )}
            {filters.script && (
              <FilterChip
                name="script"
                value={filters.script}
                onRemove={() => update({ script: null })}
              />
            )}
            {filters.parent && (
              <FilterChip
                name="forks of"
                value={filters.parent}
                onRemove={() => update({ parent: null })}
              />
            )}
          </div>
        )}
        {countRunFilters(filters) > 0 && (
          <button
            type="button"
            className="btn ghost small"
            style={{ marginLeft: "auto" }}
            onClick={() => {
              setCustom(false);
              update(readRunFilters(new URLSearchParams()));
            }}
          >
            Clear filters
          </button>
        )}
      </section>

      <RunTable query={runs} onPickLabel={addLabel} onPickScript={(script) => update({ script })} />
    </div>
  );
}
