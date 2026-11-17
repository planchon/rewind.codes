// The windows of the views that read statistics, and the API's query parameters of the Tools and
// Scripts searches (model/searches.ts). The runs are searched with the query language
// (model/query.ts).
import type { ListScriptsQuery, ToolStatsQuery } from "../api/types";
import type { ScriptSearch } from "./searches";

export const RANGES = {
  "1h": 3_600_000,
  "24h": 86_400_000,
  "7d": 7 * 86_400_000,
  "30d": 30 * 86_400_000,
} as const;

export type RangeKey = keyof typeof RANGES;

/** The window a link names, if it names one. */
export function rangeOf(value: string | null): RangeKey | null {
  // Own keys only: `"toString" in RANGES` holds too, and its "range" is a function.
  return value !== null && Object.hasOwn(RANGES, value) ? (value as RangeKey) : null;
}

export function toScriptsQuery(
  search: Pick<ScriptSearch, "labels" | "tool">,
  limit = 50,
): ListScriptsQuery {
  const query: ListScriptsQuery = { limit };
  if (search.labels.length > 0) query.label = search.labels;
  if (search.tool) query.tool = search.tool;
  return query;
}

export function toToolStatsQuery(
  window: RangeKey,
  toolset: string | null,
  now: number,
): ToolStatsQuery {
  const query: ToolStatsQuery = {
    since: new Date(now - RANGES[window]).toISOString(),
    until: new Date(now).toISOString(),
  };
  if (toolset) query.toolset = toolset;
  return query;
}
