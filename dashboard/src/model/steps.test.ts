import { describe, expect, it } from "vitest";
import { triageWorkflowJournal } from "../mocks/fixtures";
import { buildRunModel } from "./run";
import {
  buildSteps,
  gutterMarks,
  markSymbol,
  moveStep,
  stateAt,
  stepFromParam,
  stepOf,
  stopsAt,
} from "./steps";

const T0 = Date.parse("2026-01-01T00:00:00Z");
const entries = triageWorkflowJournal(T0).entries;
const model = buildRunModel(entries);
const steps = buildSteps(entries);

/** The marks of every line at a step, as `line symbol`. */
function marksAt(n: number): string[] {
  return [...gutterMarks(model, steps, n, 4, null).entries()]
    .sort(([a], [b]) => a - b)
    .map(([line, mark]) => `${line} ${markSymbol(mark)}`);
}

describe("the steps of a run", () => {
  it("cut the triage journal into its eight triggers and what each committed", () => {
    expect(entries).toHaveLength(18);
    expect(steps.map((s) => [s.first, s.last])).toEqual([
      [0, 1],
      [2, 3],
      [4, 6],
      [7, 10],
      [11, 11],
      [12, 13],
      [14, 15],
      [16, 17],
    ]);
    expect(steps.map((s) => s.trigger.event.type)).toEqual([
      "run_started",
      "tool_call_completed",
      "tool_call_completed",
      "signal_received",
      "tool_call_completed",
      "tool_call_completed",
      "timer_fired",
      "tool_call_completed",
    ]);
    expect(steps[7]?.ends).toBe("completed");
    expect(stepOf(steps, 9)).toBe(3);
    expect(stepOf(steps, 18)).toBe(-1);
  });

  it("end the last step with run_aborted", () => {
    const aborted = [
      ...entries.slice(0, 3),
      {
        idx: 3,
        recordedAt: entries[2]?.recordedAt ?? "",
        event: {
          type: "run_aborted" as const,
          reason: { kind: "limit_exceeded" as const, message: "cpu" },
        },
      },
    ];
    const s = buildSteps(aborted);
    expect(s.map((x) => [x.first, x.last, x.ends])).toEqual([
      [0, 1, null],
      [2, 3, "aborted"],
    ]);
  });

  it("skip a step that issues nothing, unless every step is asked for", () => {
    // Step 5 delivers #4: the script still waits in Promise.all for #5.
    expect(stopsAt(steps, 4)).toBe(false);
    expect(moveStep(steps, 3, 1, false)).toBe(5);
    expect(moveStep(steps, 5, -1, false)).toBe(3);
    expect(moveStep(steps, 3, 1, true)).toBe(4);
    expect(moveStep(steps, 7, 1, false)).toBe(7);
    expect(moveStep(steps, 0, -1, false)).toBe(0);
  });

  it("read ?step= as the step holding that entry, the last step as the head", () => {
    expect(stepFromParam(steps, "7")).toBe(3);
    expect(stepFromParam(steps, "9")).toBe(3);
    expect(stepFromParam(steps, "16")).toBeNull();
    expect(stepFromParam(steps, "99")).toBeNull();
    expect(stepFromParam(steps, "x")).toBeNull();
    expect(stepFromParam(steps, null)).toBeNull();
  });

  it("mark each line at a step: resumed, issued, paused, done", () => {
    // The start: main resumes, #1 is issued.
    expect(marksAt(0)).toEqual(["4 ▸", "5 ①"]);
    // #2's result: the timer of the timeout and the wait are issued on line 10.
    expect(marksAt(2)).toEqual(["5 ✓", "8 ▸", "10 ①"]);
    // The signal: the wait resumes, two calls are issued on line 13.
    expect(marksAt(3)).toEqual(["5 ✓", "8 ✓", "10 ▸", "13 ①"]);
    // #4 arrives, #5 is still open: the line is resumed, and still paused.
    expect(marksAt(4)).toEqual(["5 ✓", "8 ✓", "10 ✓", "13 ▸"]);
    expect(marksAt(5)).toEqual(["5 ✓", "8 ✓", "10 ✓", "13 ▸", "15 ①"]);
    expect(marksAt(7)).toEqual(["5 ✓", "8 ✓", "10 ✓", "13 ✓", "15 ✓", "16 ▸"]);
  });

  it("know what is open, done or yet to come at the cursor", () => {
    const [list, , close4, close5] = model.calls;
    if (!list || !close4 || !close5) throw new Error("calls");
    expect(stateAt(list, 1)).toBe("open");
    expect(stateAt(list, 2)).toBe("done");
    expect(stateAt(close4, 6)).toBe("future");
    expect(stateAt(close5, 11)).toBe("open");
    expect(stateAt(close5, 12)).toBe("done");
  });

  it("mark the line a run failed at in the step that failed", () => {
    const failed = [
      ...entries.slice(0, 2),
      {
        idx: 2,
        recordedAt: entries[1]?.recordedAt ?? "",
        event: {
          type: "tool_call_failed" as const,
          seq: 1,
          error: { kind: "error" as const, message: "no" },
        },
      },
      {
        idx: 3,
        recordedAt: entries[1]?.recordedAt ?? "",
        event: { type: "fail_run" as const, error: { name: "ToolError", message: "no" } },
      },
    ];
    const m = buildRunModel(failed);
    const s = buildSteps(failed);
    const marks = gutterMarks(m, s, 1, 4, 5);
    expect(marks.get(5)).toEqual({ kind: "failed" });
    expect(gutterMarks(m, s, 0, 4, 5).get(5)).toEqual({ kind: "issued", rank: 1 });
  });
});
