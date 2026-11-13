// Filters of the Scripts and Tools lists, kept in the URL so a filtered view can be shared, and
// turned into the API's query parameters. The runs are searched with the query language
// (model/query.ts).
import type { ListScriptsQuery, ToolStatsQuery } from "../api/types";

export const RANGES = {
  "1h": 3_600_000,
  "24h": 86_400_000,
  "7d": 7 * 86_400_000,
  "30d": 30 * 86_400_000,
} as const;

export type RangeKey = keyof typeof RANGES;

function isRange(value: string | null): value is RangeKey {
  // Own keys only: `"toString" in RANGES` holds too, and its "range" is a function.
  return value !== null && Object.hasOwn(RANGES, value);
}

function text(params: URLSearchParams, key: string): string | null {
  const value = params.get(key)?.trim();
  return value ? value : null;
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
