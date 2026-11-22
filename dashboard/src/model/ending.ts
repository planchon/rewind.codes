// How a run ended, in the words of the dashboard. The model does not change: a cancelled run is a
// `fail_run` whose error is `CancelledError` (archi/02-journal.md, "Run state"), and a cancellation
// is `cancel_requested` rejecting every pending await (archi/03-execution.md, "Cancellation").
// What changes is how it is told: a cancellation is not a failure, and says why it happened (its
// deadline, a request, the script itself) and what the run was waiting for then.

import { formatDuration, shortRunId } from "./format";
import type { Activity, RunModel } from "./run";
import type { CancelReason, RunStatus } from "./types";
import { endedRunOf } from "./workflows";

export type EndKind = "completed" | "failed" | "cancelled" | "aborted";

/** The error name of every cancellation. */
export const CANCELLED_ERROR = "CancelledError";

export function endKindOf(status: RunStatus): EndKind | null {
  switch (status) {
    case "completed":
    case "failed":
    case "cancelled":
    case "aborted":
      return status;
    default:
      return null;
  }
}

/** Why a cancelled run was cancelled. */
export type CancelCause =
  /** Its deadline passed: the scheduler requested the cancellation (`ttl`). */
  | { kind: "ttl"; ttlMs: number | null }
  /** Someone asked (`POST /v1/runs/{id}/cancel`, or the end of the run that started it). */
  | { kind: "requested"; message: string | null }
  /** No cancellation was requested: the script threw `CancelledError` itself. */
  | { kind: "script" }
  /** Not known from what was read (a run's summary without its journal). */
  | { kind: "unknown" };

export interface CancelFacts {
  /** The `cancel_requested` of the journal, when it was read. */
  cancel?: { reason: CancelReason; message: string | null } | null;
  /** Whether the journal was read: without a `cancel_requested`, the script threw it. */
  journalRead?: boolean;
  createdAt?: string | null;
  expiresAt?: string | null;
  endedAt?: string | null;
}

export function cancelCause(facts: CancelFacts): CancelCause {
  const ttlMs =
    facts.createdAt && facts.expiresAt
      ? Date.parse(facts.expiresAt) - Date.parse(facts.createdAt)
      : null;
  if (facts.cancel) {
    return facts.cancel.reason === "ttl"
      ? { kind: "ttl", ttlMs }
      : { kind: "requested", message: facts.cancel.message };
  }
  if (facts.journalRead) return { kind: "script" };
  // From a run's details alone: it ended at or after its deadline.
  if (facts.expiresAt && facts.endedAt && Date.parse(facts.endedAt) >= Date.parse(facts.expiresAt))
    return { kind: "ttl", ttlMs };
  return { kind: "unknown" };
}

/** The headline of an end: "Cancelled at its deadline", "Failed", "Aborted by the host". */
export function endTitle(kind: EndKind, cause: CancelCause | null = null): string {
  switch (kind) {
    case "completed":
      return "Completed";
    case "failed":
      return "Failed";
    case "aborted":
      return "Aborted by the host";
    case "cancelled":
      switch (cause?.kind) {
        case "ttl":
          return "Cancelled at its deadline";
        case "requested":
          return "Cancelled on request";
        case "script":
          return "Cancelled by its script";
        default:
          return "Cancelled";
      }
  }
}

/** Why it was cancelled, as a sentence. */
export function causeText(cause: CancelCause): string | null {
  switch (cause.kind) {
    case "ttl":
      return cause.ttlMs !== null
        ? `Its deadline passed: a TTL of ${formatDuration(cause.ttlMs)} from its start.`
        : "Its deadline passed.";
    case "requested":
      return cause.message
        ? `Cancellation was requested: “${cause.message}”.`
        : "Cancellation was requested, without a message.";
    case "script":
      return "The script threw CancelledError itself; no cancellation was requested.";
    case "unknown":
      return null;
  }
}

/** What the run was doing when it ended: the awaits still pending, each with its line. */
export interface Pending {
  activity: Activity;
  /** `it was awaiting signal "notify"`, `#2 ask_user was in flight`, `it was sleeping 3d…`. */
  text: string;
  /** What was awaited: `signal "notify"`, `#2 ask_user`, `the 3d sleep (timer #3)`. */
  what: string;
  line: number | null;
}

/** The awaits pending just before entry `idx` (the cancellation, or the end). */
export function pendingBefore(model: RunModel, idx: number): Pending[] {
  return model.activities
    .filter((a) => {
      if (a.kind === "log" || a.idx >= idx) return false;
      if (a.kind === "timer" && a.timeoutOf !== null) return false;
      return a.end === null || a.end.idx >= idx;
    })
    .map((a) => ({
      activity: a,
      text: pendingText(a),
      what: pendingWhat(a),
      line: a.callsite?.line ?? null,
    }));
}

function pendingWhat(a: Activity): string {
  switch (a.kind) {
    case "tool_call":
      return `#${a.seq} ${a.tool}`;
    case "timer":
      return `the ${formatDuration(a.durationMs)} sleep (timer #${a.seq})`;
    case "signal": {
      const ended = endedRunOf(a.name);
      return ended !== null ? `the end of run ${shortRunId(ended)}` : `signal "${a.name}"`;
    }
    case "log":
      return "";
  }
}

function pendingText(a: Activity): string {
  switch (a.kind) {
    case "tool_call":
      return `#${a.seq} ${a.tool} was in flight`;
    case "timer":
      return `it was sleeping ${formatDuration(a.durationMs)} (timer #${a.seq})`;
    case "signal": {
      const ended = endedRunOf(a.name);
      return ended !== null
        ? `it was awaiting the end of run ${shortRunId(ended)}`
        : `it was awaiting signal "${a.name}"`;
    }
    case "log":
      return "";
  }
}

/** Whether an error, or a failure group's, is a cancellation. */
export function isCancellation(errorName: string | null | undefined): boolean {
  return errorName === CANCELLED_ERROR;
}
