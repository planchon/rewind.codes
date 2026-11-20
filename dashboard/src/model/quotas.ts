// Namespaces and their quotas (archi/06, "Namespaces" and "Quotas"), as the dashboard shows them:
// what a namespace used against its limit, the share and its level (orange from 80 %, red at
// 100 %), durations as the API writes them, the month's cumulative CPU time, and the limits
// typed in a form. Pure: the server enforces every rule.
import type { DayUsage, Duration, QuotaKind, Quotas, QuotaUsage } from "../api/types";
import { formatCount, formatDuration } from "./format";

/** The quotas counted over the month or now, each shown as a meter. */
export type Metered = "activeRuns" | "runsPerMonth" | "cpuPerMonth" | "scripts";
/** Every quota, as `Quotas` names it. */
export type QuotaKey = keyof Quotas;

export interface MeteredSpec {
  key: Metered;
  kind: QuotaKind;
  /** A column's title. */
  title: string;
  /** What the use counts, in a sentence. */
  noun: string;
}

/** The meters, in the order of the Namespaces view's columns. */
export const METERED: readonly MeteredSpec[] = [
  { key: "activeRuns", kind: "active_runs", title: "Active runs", noun: "active runs" },
  { key: "runsPerMonth", kind: "runs_per_month", title: "Runs this month", noun: "runs" },
  { key: "cpuPerMonth", kind: "cpu_per_month", title: "CPU this month", noun: "CPU time" },
  { key: "scripts", kind: "scripts", title: "Scripts", noun: "distinct scripts" },
];

export interface QuotaField {
  key: QuotaKey;
  label: string;
  /** A count, or a duration written `2h`, `30m`, `7d`. */
  unit: "count" | "duration";
  hint: string;
}

/** The six quotas, in the order a form asks for them. */
export const QUOTA_FIELDS: readonly QuotaField[] = [
  {
    key: "activeRuns",
    label: "Active runs",
    unit: "count",
    hint: "At once; one more is refused (429). 0 blocks the end user.",
  },
  {
    key: "runsPerMonth",
    label: "Runs per month",
    unit: "count",
    hint: "Forks included; once reached, starts are refused for the rest of the month.",
  },
  {
    key: "cpuPerMonth",
    label: "CPU per month",
    unit: "duration",
    hint: "Never refused: alerts at 80 % and 100 %.",
  },
  {
    key: "scripts",
    label: "Scripts",
    unit: "count",
    hint: "Distinct scripts with a run kept; a new one above it is refused.",
  },
  {
    key: "maxTtl",
    label: "Longest TTL",
    unit: "duration",
    hint: "The TTL a run may ask for; the default TTL is lowered to it.",
  },
  {
    key: "maxScriptLines",
    label: "Longest script",
    unit: "count",
    hint: "In lines; a longer script is refused (422).",
  },
];

/** A quota as an alert or a refusal names it, in words. */
export const QUOTA_NAMES: Record<QuotaKind, string> = {
  cpu_per_month: "CPU",
  active_runs: "active runs",
  runs_per_month: "runs this month",
  scripts: "scripts",
  max_ttl: "longest TTL",
  max_script_lines: "longest script",
};

/** The field of `Quotas` each quota is. */
export const QUOTA_KEYS: Record<QuotaKind, QuotaKey> = {
  cpu_per_month: "cpuPerMonth",
  active_runs: "activeRuns",
  runs_per_month: "runsPerMonth",
  scripts: "scripts",
  max_ttl: "maxTtl",
  max_script_lines: "maxScriptLines",
};

const UNITS: Record<string, number> = {
  ms: 1,
  s: 1_000,
  m: 60_000,
  h: 3_600_000,
  d: 86_400_000,
  w: 604_800_000,
};

/**
 * A duration of the API in milliseconds: a number of milliseconds, or numbers and units
 * (`1h30m`, `1.5h`, `500ms`), as the server reads them; `null` when it cannot be read.
 */
export function durationMs(value: Duration | null | undefined): number | null {
  if (value === null || value === undefined) return null;
  if (typeof value === "number") return Number.isFinite(value) && value >= 0 ? value : null;
  const text = value.trim();
  if (text === "") return null;
  if (/^\d+$/.test(text)) return Number(text);
  let total = 0;
  let rest = text;
  while (rest !== "") {
    const m = /^(\d+(?:\.\d+)?)(ms|s|m|h|d|w)\s*/.exec(rest);
    if (!m) return null;
    total += Number(m[1]) * (UNITS[m[2] ?? ""] ?? 0);
    rest = rest.slice(m[0].length);
  }
  return Math.round(total);
}

/** A duration as the API writes it, the shortest form: `1h30m`, `500ms`, `0ms`. */
export function apiDuration(ms: number): string {
  let rest = Math.max(0, Math.round(ms));
  if (rest === 0) return "0ms";
  let out = "";
  for (const unit of ["w", "d", "h", "m", "s", "ms"]) {
    const size = UNITS[unit] ?? 1;
    if (rest >= size) {
      out += `${Math.floor(rest / size)}${unit}`;
      rest %= size;
    }
  }
  return out;
}

/** CPU time, coarse: `42s`, `41m`, `3h 12m`, `410h`. */
export function formatCpu(ms: number): string {
  const s = Math.floor(ms / 1_000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  const rest = m % 60;
  if (h >= 100 || rest === 0) return `${h}h`;
  return `${h}h ${String(rest).padStart(2, "0")}m`;
}

/** What a namespace, or the organization, used of a metered quota: milliseconds for CPU. */
export function usedOf(usage: QuotaUsage, key: Metered): number {
  switch (key) {
    case "activeRuns":
      return usage.activeRuns;
    case "runsPerMonth":
      return usage.runsStarted;
    case "cpuPerMonth":
      return durationMs(usage.cpu) ?? 0;
    case "scripts":
      return usage.scripts;
  }
}

/** A limit in its unit (milliseconds for a duration); `null` when the quota does not limit. */
export function limitOf(quotas: Quotas | undefined, key: QuotaKey): number | null {
  const value = quotas?.[key];
  if (value === null || value === undefined) return null;
  if (key === "cpuPerMonth" || key === "maxTtl") return durationMs(value);
  return typeof value === "number" ? value : null;
}

/** A use or a limit, as people read it. */
export function formatQuota(key: QuotaKey, value: number): string {
  if (key === "cpuPerMonth") return formatCpu(value);
  if (key === "maxTtl") return formatDuration(value);
  if (key === "maxScriptLines") return `${formatCount(value)} lines`;
  return formatCount(value);
}

/** The share of a limit used; `null` without a limit. A limit of 0 is reached at once. */
export function shareOf(used: number, limit: number | null): number | null {
  if (limit === null) return null;
  if (limit === 0) return used > 0 ? Number.POSITIVE_INFINITY : 1;
  return used / limit;
}

export type Level = "none" | "ok" | "warning" | "critical";

/** Orange from 80 %, red at 100 %: the thresholds of the alerts. */
export function levelOf(share: number | null): Level {
  if (share === null) return "none";
  if (share >= 1) return "critical";
  if (share >= 0.8) return "warning";
  return "ok";
}

/** A share as a whole percentage, never rounded up across a threshold: `79%`, `160%`. */
export function formatShare(share: number): string {
  if (!Number.isFinite(share)) return "over";
  return `${Math.floor(share * 100)}%`;
}

/** The highest share of a namespace's metered limits, and which quota it is. */
export function highestShare(
  usage: QuotaUsage,
  limits: Quotas,
): { key: Metered; share: number } | null {
  let best: { key: Metered; share: number } | null = null;
  for (const m of METERED) {
    const share = shareOf(usedOf(usage, m.key), limitOf(limits, m.key));
    if (share !== null && (best === null || share > best.share)) best = { key: m.key, share };
  }
  return best;
}

// Months: the quotas count calendar months, in UTC.

/** `2026-11`, the month of a time, in UTC. */
export function monthOf(ms: number): string {
  const d = new Date(ms);
  return `${d.getUTCFullYear()}-${String(d.getUTCMonth() + 1).padStart(2, "0")}`;
}

function monthParts(month: string): [number, number] | null {
  const m = /^(\d{4})-(\d{2})$/.exec(month);
  if (!m) return null;
  const month0 = Number(m[2]) - 1;
  return month0 >= 0 && month0 < 12 ? [Number(m[1]), month0] : null;
}

/** Whether a text is a month as the API writes it, `2026-11`. */
export function isMonth(text: string): boolean {
  return monthParts(text) !== null;
}

/** `November 2026`. */
export function monthLabel(month: string): string {
  const parts = monthParts(month);
  if (!parts) return month;
  return new Date(Date.UTC(parts[0], parts[1], 1)).toLocaleDateString("en-US", {
    month: "long",
    year: "numeric",
    timeZone: "UTC",
  });
}

/** `Nov 9`: a day of a month. */
export function dayLabel(month: string, day: number): string {
  const parts = monthParts(month);
  if (!parts) return String(day);
  return new Date(Date.UTC(parts[0], parts[1], day)).toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
    timeZone: "UTC",
  });
}

/** `Nov 9`: the UTC day of a time. */
export function shortDate(ms: number): string {
  return new Date(ms).toLocaleDateString("en-US", {
    month: "short",
    day: "numeric",
    timeZone: "UTC",
  });
}

/** The first instant of the month and that of the next, in milliseconds, in UTC. */
export function monthBounds(month: string): [number, number] {
  const parts = monthParts(month) ?? [1970, 0];
  return [Date.UTC(parts[0], parts[1], 1), Date.UTC(parts[0], parts[1] + 1, 1)];
}

export function daysInMonth(month: string): number {
  const parts = monthParts(month);
  if (!parts) return 30;
  return new Date(Date.UTC(parts[0], parts[1] + 1, 0)).getUTCDate();
}

/** The month `by` months after (or before) this one. */
export function shiftMonth(month: string, by: number): string {
  const parts = monthParts(month);
  if (!parts) return month;
  return monthOf(Date.UTC(parts[0], parts[1] + by, 1));
}

export interface CumulativePoint {
  /** The day of the month, from 1. */
  day: number;
  /** CPU time from the 1st to the end of this day, in milliseconds. */
  ms: number;
}

/**
 * The month's CPU time added up day after day, from the days with some use: one point per day
 * up to today for the current month, to the last day for a past one.
 */
export function cumulativeCpu(
  days: readonly DayUsage[],
  month: string,
  now: number,
): CumulativePoint[] {
  const current = monthOf(now);
  if (month > current) return [];
  const last = month === current ? new Date(now).getUTCDate() : daysInMonth(month);
  const perDay = new Map<number, number>();
  for (const d of days) {
    if (!d.day.startsWith(`${month}-`)) continue;
    const day = Number(d.day.slice(8, 10));
    perDay.set(day, (perDay.get(day) ?? 0) + (durationMs(d.cpu) ?? 0));
  }
  const points: CumulativePoint[] = [];
  let total = 0;
  for (let day = 1; day <= last; day += 1) {
    total += perDay.get(day) ?? 0;
    points.push({ day, ms: total });
  }
  return points;
}

/** The first day the cumulative use reached `percent` % of the limit, or `null`. */
export function crossingDay(
  points: readonly CumulativePoint[],
  limit: number,
  percent: number,
): CumulativePoint | null {
  return points.find((p) => p.ms * 100 >= limit * percent) ?? null;
}

// Names and limits as they are typed.

/** Why a text cannot be a namespace's name, or `null` when it can (as the server checks it). */
export function namespaceNameProblem(name: string): string | null {
  if (name.length === 0) return "Type a namespace.";
  if (name.length > 128) return "A namespace has at most 128 characters.";
  if (!/^[A-Za-z0-9._:@-]+$/.test(name))
    return "A namespace is made of letters, digits, -, _, ., : and @.";
  return null;
}

/** A limit as a form shows it: `10`, `2h`; empty without one. */
export function quotaText(quotas: Quotas | undefined, key: QuotaKey): string {
  const value = limitOf(quotas, key);
  if (value === null) return "";
  return key === "cpuPerMonth" || key === "maxTtl" ? apiDuration(value) : String(value);
}

export type QuotaTexts = Record<QuotaKey, string>;

export function quotaTexts(quotas: Quotas | undefined): QuotaTexts {
  return Object.fromEntries(
    QUOTA_FIELDS.map((f) => [f.key, quotaText(quotas, f.key)]),
  ) as QuotaTexts;
}

/** Reads the limits typed in a form; an empty field sets none. */
export function parseQuotas(
  texts: QuotaTexts,
): { quotas: Quotas; errors: null } | { quotas: null; errors: Partial<QuotaTexts> } {
  const quotas: Record<string, number | string> = {};
  const errors: Partial<QuotaTexts> = {};
  for (const field of QUOTA_FIELDS) {
    const text = texts[field.key].trim();
    if (text === "") continue;
    if (field.unit === "count") {
      if (!/^\d+$/.test(text) || !Number.isSafeInteger(Number(text))) {
        errors[field.key] = "A whole number, 0 or more.";
        continue;
      }
      quotas[field.key] = Number(text);
    } else {
      const ms = durationMs(text);
      if (ms === null || /^\d+$/.test(text)) {
        errors[field.key] = "A duration, as 30m, 2h or 7d.";
        continue;
      }
      quotas[field.key] = apiDuration(ms);
    }
  }
  return Object.keys(errors).length > 0
    ? { quotas: null, errors }
    : { quotas: quotas as Quotas, errors: null };
}
