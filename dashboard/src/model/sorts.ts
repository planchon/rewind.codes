// The orders of the runs (`GET /v1/runs?sort=`), as the URL of a view keeps them (`sort`), and
// what the table's headers say of each.
import type { RunSort } from "./types";

export const RUN_SORTS: readonly RunSort[] = [
  "started",
  "oldest",
  "activity",
  "wakes",
  "duration",
  "calls",
];

/** What each order puts first, as a header's menu offers it. */
export const SORT_WORDS: Record<RunSort, string> = {
  started: "Newest first",
  oldest: "Oldest first",
  activity: "Latest activity first",
  wakes: "Soonest to wake first",
  duration: "Longest first",
  calls: "Most calls first",
};

/** The order a URL names (`sort`), the most recently started first by default. */
export function readSort(params: URLSearchParams): RunSort {
  const sort = params.get("sort");
  return (RUN_SORTS as readonly string[]).includes(sort ?? "") ? (sort as RunSort) : "started";
}

/** A URL's parameters with an order, the default one left out. */
export function withSort(params: URLSearchParams, sort: RunSort): URLSearchParams {
  const next = new URLSearchParams(params);
  if (sort === "started") next.delete("sort");
  else next.set("sort", sort);
  return next;
}

/** Whether an order goes up its column (the first, the soonest) rather than down. */
export function ascending(sort: RunSort): boolean {
  return sort === "oldest" || sort === "wakes";
}
