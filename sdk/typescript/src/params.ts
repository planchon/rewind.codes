import type { Labels } from "./types.js";

/** Labels as the API's query strings take them: one `key:value` per label. */
export function labelParams(labels: Labels | undefined): string[] | undefined {
  if (labels === undefined) {
    return undefined;
  }
  const entries = Object.entries(labels);
  return entries.length === 0 ? undefined : entries.map(([key, value]) => `${key}:${value}`);
}

/** A time as the API takes it: RFC 3339. */
export function timeParam(time: Date | string | undefined): string | undefined {
  if (time === undefined) {
    return undefined;
  }
  return typeof time === "string" ? time : time.toISOString();
}

/** Whether `labels` carries every label of `scope`. */
export function hasLabels(labels: Labels, scope: Labels | undefined): boolean {
  if (scope === undefined) {
    return true;
  }
  return Object.entries(scope).every(([key, value]) => labels[key] === value);
}
