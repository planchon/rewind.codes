// What a waiting run waits for, in words (archi/08, "Waiting is a status"): the status says
// `waiting`, and its detail, shown on hover, says for what: a signal, a timer, a harness action's
// call, an approval, a deferred answer, a parked call, a quarantine.
import { formatDuration } from "./format";
import type { RunStatus, RunWait, WaitKind } from "./types";

/** Each kind, as a person says it. */
export const WAIT_WORDS: Record<WaitKind, string> = {
  signal: "signal",
  timer: "timer",
  harness: "harness action",
  approval: "approval",
  deferred: "deferred answer",
  parked: "parked call",
  quarantined: "quarantine",
};

/** A status as the chip and its card write it: `Waiting`, `Running`. */
export function statusWord(status: RunStatus): string {
  return status.charAt(0).toUpperCase() + status.slice(1);
}

/** In how long a time comes: `2d 23h`, or `now` once it has passed. */
function inTime(at: string, now: number): string {
  const ms = Date.parse(at) - now;
  return ms > 1_000 ? `in ${formatDuration(ms)}` : "now";
}

/**
 * One wait, as the status's card says it: `signal approval, times out in 2d 23h`, `timer, fires
 * in 9d 23h`, `harness action ask_user`, `approval of crm.delete_account`.
 */
export function waitDetail(w: RunWait, now: number): string {
  const name = w.name ?? null;
  switch (w.kind) {
    case "signal":
      return `signal ${name ?? "?"}${w.until ? `, times out ${inTime(w.until, now)}` : ""}`;
    case "timer":
      return w.until ? `timer, fires ${inTime(w.until, now)}` : "timer";
    case "harness":
      return `harness action ${name ?? "?"}`;
    case "approval":
      return `approval of ${name ?? "a call"}`;
    case "deferred":
      return `deferred answer to ${name ?? "a call"}`;
    case "parked":
      return `parked call ${name ?? "?"}`;
    case "quarantined":
      return "quarantine";
  }
}

/** The status's card: `Waiting · signal approval, times out in 2d 23h`, one line per wait. */
export function statusDetail(status: RunStatus, waits: readonly RunWait[], now: number): string[] {
  if (waits.length === 0) return [statusWord(status)];
  return waits.map((w) => `${statusWord(status)} · ${waitDetail(w, now)}`);
}
