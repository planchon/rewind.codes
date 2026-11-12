// Formatting of durations, sizes, times, ids and digests. Pure, so the views stay terse.

const SECOND = 1000;
const MINUTE = 60 * SECOND;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** A duration as people read it: `840µs`, `310ms`, `1.24s`, `4m 05s`, `3d 4h`. */
export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms)) return "–";
  const sign = ms < 0 ? "-" : "";
  const abs = Math.abs(ms);
  if (abs === 0) return "0ms";
  if (abs < 1) return `${sign}${Math.round(abs * 1000)}µs`;
  if (abs < SECOND) return `${sign}${Math.round(abs)}ms`;
  if (abs < 10 * SECOND) return `${sign}${trim((abs / SECOND).toFixed(2))}s`;
  if (abs < MINUTE) return `${sign}${trim((abs / SECOND).toFixed(1))}s`;
  if (abs < HOUR) {
    const m = Math.floor(abs / MINUTE);
    const s = Math.floor((abs % MINUTE) / SECOND);
    return `${sign}${m}m ${pad(s)}s`;
  }
  if (abs < DAY) {
    const h = Math.floor(abs / HOUR);
    const m = Math.floor((abs % HOUR) / MINUTE);
    return `${sign}${h}h ${pad(m)}m`;
  }
  const d = Math.floor(abs / DAY);
  const h = Math.floor((abs % DAY) / HOUR);
  return h === 0 ? `${sign}${d}d` : `${sign}${d}d ${h}h`;
}

/** A compact offset from the start of a run: `+0s`, `+310ms`, `+2.5s`, `+10d 0h`. */
export function formatOffset(ms: number): string {
  if (ms === 0) return "+0s";
  return `+${formatDuration(ms)}`;
}

function trim(fixed: string): string {
  return fixed.replace(/\.?0+$/, "");
}

function pad(n: number): string {
  return String(n).padStart(2, "0");
}

/** A size in bytes: `812 B`, `12.4 KB`, `3.1 MB` (powers of 1024, as the API's limits). */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

const counts = new Intl.NumberFormat("en-US");

export function formatCount(n: number): string {
  return counts.format(n);
}

/** A ratio as a percentage: `0%`, `0.4%`, `12%`, `100%`. */
export function formatPercent(part: number, whole: number): string {
  if (whole === 0) return "–";
  const p = (100 * part) / whole;
  if (p === 0) return "0%";
  if (p < 0.1) return "<0.1%";
  if (p < 10) return `${trim(p.toFixed(1))}%`;
  return `${Math.round(p)}%`;
}

export type TimeZoneMode = "utc" | "local";

/** An absolute time: `2026-11-09 10:02:11` (UTC or local), with milliseconds if asked. */
export function formatTime(ms: number, zone: TimeZoneMode, withMs = false): string {
  const d = new Date(ms);
  const utc = zone === "utc";
  const parts = [
    utc ? d.getUTCFullYear() : d.getFullYear(),
    pad((utc ? d.getUTCMonth() : d.getMonth()) + 1),
    pad(utc ? d.getUTCDate() : d.getDate()),
  ];
  const clock = [
    pad(utc ? d.getUTCHours() : d.getHours()),
    pad(utc ? d.getUTCMinutes() : d.getMinutes()),
    pad(utc ? d.getUTCSeconds() : d.getSeconds()),
  ];
  const millis = withMs
    ? `.${String(utc ? d.getUTCMilliseconds() : d.getMilliseconds()).padStart(3, "0")}`
    : "";
  return `${parts.join("-")} ${clock.join(":")}${millis}`;
}

/** How long ago, coarse: `just now`, `42s ago`, `5m ago`, `3h ago`, `12d ago`; or `in 2d`. */
export function formatRelative(ms: number, now: number): string {
  const delta = now - ms;
  const abs = Math.abs(delta);
  let text: string;
  if (abs < 5 * SECOND) return "just now";
  if (abs < MINUTE) text = `${Math.floor(abs / SECOND)}s`;
  else if (abs < HOUR) text = `${Math.floor(abs / MINUTE)}m`;
  else if (abs < DAY) text = `${Math.floor(abs / HOUR)}h`;
  else text = `${Math.floor(abs / DAY)}d`;
  return delta >= 0 ? `${text} ago` : `in ${text}`;
}

/** The first 12 hex digits of a digest, as the CLI prints them. */
export function shortDigest(digest: string): string {
  return digest.slice(0, 12);
}

/**
 * A run id shortened for tables. A UUID v7 starts with its timestamp, so runs started together
 * share their first digits: keep the start and the random end.
 */
export function shortRunId(id: string): string {
  if (id.length <= 16) return id;
  return `${id.slice(0, 8)}…${id.slice(-6)}`;
}

/** `key:value`, the form the API filters labels with. */
export function labelPair(key: string, value: string): string {
  return `${key}:${value}`;
}

/** Splits `key:value` at its first colon. */
export function splitLabel(pair: string): [string, string] | null {
  const at = pair.indexOf(":");
  if (at <= 0) return null;
  return [pair.slice(0, at), pair.slice(at + 1)];
}
