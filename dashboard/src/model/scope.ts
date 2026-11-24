// The scope of the statistics (archi/05, "Statistics"): the routes that answer from the hourly
// counters (`GET /v1/runs/stats`, `/v1/runs/failures`, `/v1/workflows`, `/v1/actions/stats`) take
// the tenant, or one namespace named by a single `namespace:` term of one value, and refuse any
// other term (`invalid-query`). A view's query is cut down to that term before it is sent; its
// other terms filter the runs its marks open.
import { operandValues, splitTerms } from "./query";
import type { WaitCounts } from "./types";

/**
 * The runs of a scope waiting now, from the gauge of its active runs: the sum of its waits by
 * kind, a run waiting for two kinds at once counting in each.
 */
export function waitingRuns(waiting: WaitCounts): number {
  return (
    waiting.signal +
    waiting.timer +
    waiting.harness +
    waiting.approval +
    waiting.deferred +
    waiting.parked +
    waiting.quarantined
  );
}

export interface StatsScope {
  /** The one term sent as `q` (`namespace:alice`), or `undefined` for the tenant. */
  q: string | undefined;
  /** Where that term starts in the view's query, in characters: to place a fault the server gives. */
  offset: number;
  /** The view's query holds terms the statistics leave out. */
  narrowed: boolean;
}

/**
 * The scope a query names for the statistics: its namespace when one term `namespace:<name>`
 * names a single namespace, asked for, not excluded; the tenant otherwise.
 */
export function statsScope(q: string | undefined): StatsScope {
  const terms = splitTerms(q ?? "");
  const namespaces = terms.filter((t) => t.kind === "field" && t.field === "namespace");
  const term = namespaces.length === 1 ? namespaces[0] : undefined;
  const values = term ? operandValues(term.operand) : [];
  const value = values[0] ?? "";
  if (!term || term.negated || values.length !== 1 || value === "" || value.includes("*"))
    return { q: undefined, offset: 0, narrowed: terms.length > 0 };
  return {
    q: term.raw,
    offset: [...(q ?? "").slice(0, term.start)].length,
    narrowed: terms.length > 1,
  };
}
