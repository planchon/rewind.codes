// The steps of a run, for the time cursor of the Run page (archi/05-testing-and-observability.md,
// "Dashboard"). The commit invariant makes a history a sequence of steps: a trigger (the start, a
// tool's result or failure, a timer, a signal, a cancellation) and the entries committed with it,
// until the script pauses again. What the run knew after a step is the journal up to its last
// entry, so the state at the cursor is `buildRunModel(entries up to it)`.
import type { Entry } from "../api/types";
import { CANCELLED_ERROR } from "./ending";
import type { Activity, RunModel } from "./run";

/** The events that wake the script up: each starts a step. */
export const TRIGGERS: ReadonlySet<Entry["event"]["type"]> = new Set([
  "run_started",
  "tool_call_completed",
  "tool_call_failed",
  "timer_fired",
  "signal_received",
  "cancel_requested",
]);

export interface Step {
  /** Its position, from 0. */
  n: number;
  /** The index of its trigger: what `?step=` names. */
  first: number;
  /** The index of its last entry. */
  last: number;
  trigger: Entry;
  /** The entries committed with the trigger, in order. */
  committed: Entry[];
  /** When the trigger was recorded, in milliseconds since the epoch. */
  at: number;
  /**
   * It ends the run: it holds `complete_run`, `fail_run` or `run_aborted`. A `fail_run` whose
   * error is `CancelledError` ends it `cancelled` (archi/02-journal.md, "Run state").
   */
  ends: "completed" | "failed" | "cancelled" | "aborted" | null;
}

/** The steps of a history, in order. `run_aborted` ends the step it follows. */
export function buildSteps(entries: readonly Entry[]): Step[] {
  const steps: Step[] = [];
  for (const entry of entries) {
    const current = steps[steps.length - 1];
    if (!current || TRIGGERS.has(entry.event.type)) {
      steps.push({
        n: steps.length,
        first: entry.idx,
        last: entry.idx,
        trigger: entry,
        committed: [],
        at: Date.parse(entry.recordedAt),
        ends: null,
      });
      continue;
    }
    current.last = entry.idx;
    current.committed.push(entry);
    const type = entry.event.type;
    if (type === "complete_run") current.ends = "completed";
    if (entry.event.type === "fail_run")
      current.ends = entry.event.error.name === CANCELLED_ERROR ? "cancelled" : "failed";
    if (type === "run_aborted") current.ends = "aborted";
  }
  return steps;
}

/** The position of the step that holds an entry; -1 when no step does. */
export function stepOf(steps: readonly Step[], idx: number): number {
  let lo = 0;
  let hi = steps.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const step = steps[mid];
    if (!step) break;
    if (idx < step.first) hi = mid - 1;
    else if (idx > step.last) lo = mid + 1;
    else return mid;
  }
  return -1;
}

/**
 * The step `?step=` names: the one holding that entry. `null` for the last step, the head, which
 * the cursor follows while entries arrive; also while the entry it names has not arrived yet.
 */
export function stepFromParam(steps: readonly Step[], param: string | null): number | null {
  if (param === null || !/^\d+$/.test(param)) return null;
  const at = stepOf(steps, Number(param));
  return at < 0 || at === steps.length - 1 ? null : at;
}

/**
 * Whether the cursor stops at a step when it skips the empty ones: a step that issues nothing,
 * like a result the script does not resume on (it still waits in `Promise.all`), is skipped.
 */
export function stopsAt(steps: readonly Step[], n: number): boolean {
  const step = steps[n];
  if (!step) return false;
  return n === 0 || n === steps.length - 1 || step.committed.length > 0 || step.ends !== null;
}

/** The next step in a direction, skipping the empty ones unless `every`. */
export function moveStep(steps: readonly Step[], from: number, by: 1 | -1, every: boolean): number {
  for (let n = from + by; n >= 0 && n < steps.length; n += by) {
    if (every || stopsAt(steps, n)) return n;
  }
  return from;
}

export type CursorState = "future" | "open" | "done";

/** What an activity is at the cursor (the last entry the cursor includes). */
export function stateAt(activity: Activity, cursor: number): CursorState {
  if (activity.idx > cursor) return "future";
  if (activity.kind === "log") return "done";
  return activity.end !== null && activity.end.idx <= cursor ? "done" : "open";
}

/** A timer that is the timeout of a signal wait: it shows with its wait, never alone. */
export function isTimeout(activity: Activity): boolean {
  return activity.kind === "timer" && activity.timeoutOf !== null;
}

/** The activities a step issued (calls, timers, signal waits), in order. */
export function issuedIn(model: RunModel, step: Step): Activity[] {
  return model.activities.filter(
    (a) => a.kind !== "log" && !isTimeout(a) && a.idx >= step.first && a.idx <= step.last,
  );
}

export type GutterMark =
  /** The await whose value just arrived (or threw, `threw`). */
  | { kind: "resumed"; threw: boolean }
  /** Issued in this step: its rank among what the step issued, from 1. */
  | { kind: "issued"; rank: number }
  /** Still open after the step: the script is paused on it. */
  | { kind: "paused" }
  /** Done in an earlier step. */
  | { kind: "done" }
  /** The line the run failed at, in the step that failed. */
  | { kind: "failed" };

/**
 * The mark of each line at a step: ▸ resumed, ①② issued, ‖ paused, ✓ done, ✕ failed. `startLine`
 * is the line the run starts at (the default export), resumed by the first step.
 */
export function gutterMarks(
  model: RunModel,
  steps: readonly Step[],
  n: number,
  startLine: number | null,
  errorLine: number | null,
): Map<number, GutterMark> {
  const marks = new Map<number, GutterMark>();
  const step = steps[n];
  if (!step) return marks;
  const issued = issuedIn(model, step);
  const trigger = step.trigger.event.type;
  for (const [line, all] of model.byLine) {
    const on = all.filter((a) => a.kind !== "log" && !isTimeout(a));
    if (on.length === 0) continue;
    const resumed = on.find((a) => a.kind !== "log" && a.end?.idx === step.first);
    const now = on.find((a) => issued.includes(a));
    if (resumed) {
      marks.set(line, { kind: "resumed", threw: trigger === "tool_call_failed" });
    } else if (now) {
      marks.set(line, { kind: "issued", rank: issued.indexOf(now) + 1 });
    } else if (on.some((a) => stateAt(a, step.last) === "open")) {
      marks.set(line, { kind: "paused" });
    } else if (on.some((a) => stateAt(a, step.last) === "done")) {
      marks.set(line, { kind: "done" });
    }
  }
  if (n === 0 && startLine !== null && !marks.has(startLine))
    marks.set(startLine, { kind: "resumed", threw: false });
  if (errorLine !== null && step.ends !== null && step.ends !== "completed")
    marks.set(errorLine, { kind: "failed" });
  return marks;
}

const CIRCLED = "①②③④⑤⑥⑦⑧⑨⑩⑪⑫⑬⑭⑮⑯⑰⑱⑲⑳";

/** The symbol of a mark. */
export function markSymbol(mark: GutterMark): string {
  switch (mark.kind) {
    case "resumed":
      return "▸";
    case "issued":
      return CIRCLED[mark.rank - 1] ?? String(mark.rank);
    case "paused":
      return "‖";
    case "done":
      return "✓";
    case "failed":
      return "✕";
  }
}

/** What a mark means, for its title and screen readers. */
export function markMeaning(mark: GutterMark): string {
  switch (mark.kind) {
    case "resumed":
      return mark.threw ? "resumed here: the await threw" : "resumed here: its value just arrived";
    case "issued":
      return `issued in this step (${mark.rank})`;
    case "paused":
      return "paused here: still open after the step";
    case "done":
      return "done in an earlier step";
    case "failed":
      return "the run failed here";
  }
}
