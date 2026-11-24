// One-line descriptions of the events of a history and of activities, for the history, the timeline and
// the notes beside the source.

import type { Outcome } from "../../components/Status";
import { formatBytes, formatDuration, shortRunId } from "../../model/format";
import type { Activity } from "../../model/run";
import type { HistoryEvent } from "../../model/types";
import { callName, endedRunOf, readRunEnd, workflowCallOf } from "../../model/workflows";

export function activityTitle(a: Activity): string {
  switch (a.kind) {
    case "call":
      return `#${a.seq} ${callName(a.action, a.args)}`;
    case "timer":
      return `#${a.seq} ${a.timeoutOf !== null ? "timeout" : "sleep"} ${formatDuration(a.durationMs)}`;
    case "signal": {
      // `run.result()`: a wait on the end of a run this run started.
      const ended = endedRunOf(a.name);
      return ended !== null ? `result() of run ${shortRunId(ended)}` : `signal "${a.name}"`;
    }
    case "log":
      return `console.${a.level}`;
  }
}

/**
 * What became of an activity. One still open when the run ended (`runEnded`) was cancelled: a
 * terminal status clears everything pending (archi/02-history.md, "Run state").
 */
export function activityOutcome(a: Activity, runEnded = false): Outcome {
  if (a.kind === "log") return "ok";
  if (a.end === null) return runEnded ? "cancelled" : "open";
  // The end of a run this run waited for: `result()` rejects unless that run completed.
  if (a.kind === "signal" && a.end.outcome === "received" && endedRunOf(a.name) !== null) {
    const status = readRunEnd(a.end.payload).status;
    if (status === "failed" || status === "aborted") return "failed";
    if (status === "cancelled") return "cancelled";
  }
  return a.end.outcome;
}

/** How long it took, or has been going on. */
export function activityDuration(a: Activity, now: number): number | null {
  if (a.kind === "log") return null;
  return (a.end?.at ?? now) - a.at;
}

function args(preview: string): string {
  return preview.length > 72 ? `${preview.slice(0, 72)}…` : preview;
}

/** The line of the history that says an event. */
export function eventSummary(entry: HistoryEvent): string {
  const e = entry.event;
  switch (e.type) {
    case "run_started":
      return `${e.mode} run, epoch ${e.epoch}, input ${args(e.input.preview)}`;
    case "run_aborted":
      return `${e.reason.kind}: ${e.reason.message}`;
    case "schedule_call":
      return workflowCallOf(e.action)
        ? `#${e.seq} ${callName(e.action, e.args)} ${args(e.args.preview)}`
        : `#${e.seq} ${e.action}(${args(e.args.preview)})`;
    case "call_completed":
      return `#${e.seq} → ${args(e.result.preview)} (${formatBytes(e.result.bytes)})`;
    case "call_failed":
      return `#${e.seq} ✕ ${e.error.kind}: ${e.error.message}`;
    case "start_timer":
      return `#${e.seq} ${formatDuration(e.durationMs)}, fires ${e.fireAt}`;
    case "cancel_timer":
      return `#${e.seq} cancelled`;
    case "timer_fired":
      return `#${e.seq} fired`;
    case "complete_run":
      return `→ ${args(e.output.preview)}`;
    case "fail_run":
      return `${e.error.name}: ${e.error.message}`;
    case "signal_received": {
      const ended = endedRunOf(e.name);
      if (ended === null) return `"${e.name}" ${args(e.payload.preview)}`;
      const end = readRunEnd(e.payload);
      return `end of run ${shortRunId(ended)}: ${end.status ?? "ended"}${end.message ? `, ${end.message}` : ""}`;
    }
    case "cancel_requested":
      return `${e.reason}${e.message ? `: ${e.message}` : ""}`;
    case "log":
      return `[${e.level}] ${e.message}`;
    case "signal_awaited": {
      const ended = endedRunOf(e.name);
      const what = ended === null ? `"${e.name}"` : `result() of run ${shortRunId(ended)}`;
      return `${what}${e.timeout != null ? `, timeout timer #${e.timeout}` : ""}`;
    }
  }
}

/** The family of an event (archi/02-history.md). */
export function eventFamily(
  entry: HistoryEvent,
): "lifecycle" | "command" | "completion" | "annotation" {
  switch (entry.event.type) {
    case "run_started":
    case "run_aborted":
      return "lifecycle";
    case "schedule_call":
    case "start_timer":
    case "cancel_timer":
    case "complete_run":
    case "fail_run":
      return "command";
    case "log":
    case "signal_awaited":
      return "annotation";
    default:
      return "completion";
  }
}

/** The line of the code an event comes from, when it has one. */
export function eventLine(entry: HistoryEvent): number | null {
  const e = entry.event;
  return "callsite" in e && e.callsite ? e.callsite.line : null;
}
