// Filters of the lists, kept in the URL so a filtered view can be shared, and turned into the
// API's query parameters.
import type { ListRunsQuery, ListScriptsQuery, RunStatus, ToolStatsQuery } from "../api/types";
import { RUN_STATUSES } from "../api/types";

export const RANGES = {
  "1h": 3_600_000,
  "24h": 86_400_000,
  "7d": 7 * 86_400_000,
  "30d": 30 * 86_400_000,
} as const;

export type RangeKey = keyof typeof RANGES;

function isRange(value: string | null): value is RangeKey {
  return value !== null && value in RANGES;
}

export interface RunFilters {
  status: RunStatus | null;
  labels: string[];
  script: string | null;
  tool: string | null;
  /** A tool failure kind or an error name. */
  error: string | null;
  quarantined: boolean | null;
  parent: string | null;
  /** A window ending now, or explicit bounds (RFC 3339). */
  range: RangeKey | null;
  after: string | null;
  before: string | null;
}

export const NO_RUN_FILTERS: RunFilters = {
  status: null,
  labels: [],
  script: null,
  tool: null,
  error: null,
  quarantined: null,
  parent: null,
  range: null,
  after: null,
  before: null,
};

function text(params: URLSearchParams, key: string): string | null {
  const value = params.get(key)?.trim();
  return value ? value : null;
}

export function readRunFilters(params: URLSearchParams): RunFilters {
  const status = params.get("status");
  const quarantined = params.get("quarantined");
  const range = params.get("range");
  return {
    status: RUN_STATUSES.includes(status as RunStatus) ? (status as RunStatus) : null,
    labels: params.getAll("label").filter((l) => l.includes(":")),
    script: text(params, "script"),
    tool: text(params, "tool"),
    error: text(params, "error"),
    quarantined: quarantined === "yes" ? true : quarantined === "no" ? false : null,
    parent: text(params, "parent"),
    range: isRange(range) ? range : null,
    after: text(params, "after"),
    before: text(params, "before"),
  };
}

export function writeRunFilters(filters: RunFilters): URLSearchParams {
  const params = new URLSearchParams();
  if (filters.status) params.set("status", filters.status);
  for (const label of filters.labels) params.append("label", label);
  if (filters.script) params.set("script", filters.script);
  if (filters.tool) params.set("tool", filters.tool);
  if (filters.error) params.set("error", filters.error);
  if (filters.quarantined !== null) params.set("quarantined", filters.quarantined ? "yes" : "no");
  if (filters.parent) params.set("parent", filters.parent);
  if (filters.range) params.set("range", filters.range);
  else {
    if (filters.after) params.set("after", filters.after);
    if (filters.before) params.set("before", filters.before);
  }
  return params;
}

export function countRunFilters(filters: RunFilters): number {
  return [...writeRunFilters(filters).keys()].length;
}

/** The query of `GET /v1/runs`; a range is resolved against `now`. */
export function toRunsQuery(filters: RunFilters, now: number, limit = 50): ListRunsQuery {
  const query: ListRunsQuery = { limit };
  if (filters.status) query.status = filters.status;
  if (filters.labels.length > 0) query.label = filters.labels;
  if (filters.script) query.script = filters.script;
  if (filters.tool) query.tool = filters.tool;
  if (filters.error) query.error = filters.error;
  if (filters.quarantined !== null) query.quarantined = filters.quarantined;
  if (filters.parent) query.parent = filters.parent;
  if (filters.range) {
    query.startedAfter = new Date(now - RANGES[filters.range]).toISOString();
  } else {
    if (filters.after) query.startedAfter = filters.after;
    if (filters.before) query.startedBefore = filters.before;
  }
  return query;
}

export interface ScriptFilters {
  labels: string[];
  tool: string | null;
}

export function readScriptFilters(params: URLSearchParams): ScriptFilters {
  return {
    labels: params.getAll("label").filter((l) => l.includes(":")),
    tool: text(params, "tool"),
  };
}

export function writeScriptFilters(filters: ScriptFilters): URLSearchParams {
  const params = new URLSearchParams();
  for (const label of filters.labels) params.append("label", label);
  if (filters.tool) params.set("tool", filters.tool);
  return params;
}

export function toScriptsQuery(filters: ScriptFilters, limit = 50): ListScriptsQuery {
  const query: ListScriptsQuery = { limit };
  if (filters.labels.length > 0) query.label = filters.labels;
  if (filters.tool) query.tool = filters.tool;
  return query;
}

export interface ToolFilters {
  window: RangeKey;
  toolset: string | null;
}

export function readToolFilters(params: URLSearchParams): ToolFilters {
  const window = params.get("window");
  return { window: isRange(window) ? window : "24h", toolset: text(params, "toolset") };
}

export function writeToolFilters(filters: ToolFilters): URLSearchParams {
  const params = new URLSearchParams();
  if (filters.window !== "24h") params.set("window", filters.window);
  if (filters.toolset) params.set("toolset", filters.toolset);
  return params;
}

export function toToolStatsQuery(filters: ToolFilters, now: number): ToolStatsQuery {
  const query: ToolStatsQuery = {
    since: new Date(now - RANGES[filters.window]).toISOString(),
    until: new Date(now).toISOString(),
  };
  if (filters.toolset) query.toolset = filters.toolset;
  return query;
}
