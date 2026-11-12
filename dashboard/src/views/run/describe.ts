// One-line descriptions of journal entries and activities, for the journal, the timeline and
// the notes beside the source.
import type { Entry } from "../../api/types";
import type { Outcome } from "../../components/Status";
import { formatBytes, formatDuration } from "../../model/format";
import type { Activity } from "../../model/run";

export function activityTitle(a: Activity): string {
  switch (a.kind) {
    case "tool_call":
      return `#${a.seq} ${a.tool}`;
    case "timer":
      return `#${a.seq} ${a.timeoutOf !== null ? "timeout" : "sleep"} ${formatDuration(a.durationMs)}`;
    case "signal":
      return `signal "${a.name}"`;
    case "log":
      return `console.${a.level}`;
  }
}

export function activityOutcome(a: Activity): Outcome {
  if (a.kind === "log") return "ok";
  if (a.end === null) return "open";
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

/** The journal line of an entry. */
export function entrySummary(entry: Entry): string {
  const e = entry.event;
  switch (e.type) {
    case "run_started":
      return `${e.mode} run, epoch ${e.epoch}, input ${args(e.input.preview)}`;
    case "run_aborted":
      return `${e.reason.kind}: ${e.reason.message}`;
    case "schedule_tool_call":
      return `#${e.seq} ${e.tool}(${args(e.args.preview)})`;
    case "tool_call_completed":
      return `#${e.seq} → ${args(e.result.preview)} (${formatBytes(e.result.bytes)})`;
    case "tool_call_failed":
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
    case "signal_received":
      return `"${e.name}" ${args(e.payload.preview)}`;
    case "cancel_requested":
      return `${e.reason}${e.message ? `: ${e.message}` : ""}`;
    case "log":
      return `[${e.level}] ${e.message}`;
    case "signal_awaited":
      return `"${e.name}"${e.timeout != null ? `, timeout timer #${e.timeout}` : ""}`;
  }
}

/** The family of an entry's event (archi/02-journal.md). */
export function entryFamily(entry: Entry): "lifecycle" | "command" | "completion" | "annotation" {
  switch (entry.event.type) {
    case "run_started":
    case "run_aborted":
      return "lifecycle";
    case "schedule_tool_call":
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

/** The script line of an entry, when it has one. */
export function entryLine(entry: Entry): number | null {
  const e = entry.event;
  return "callsite" in e && e.callsite ? e.callsite.line : null;
}
