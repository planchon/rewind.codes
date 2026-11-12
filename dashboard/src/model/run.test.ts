import { describe, expect, it } from "vitest";
import { JournalBuilder } from "../mocks/journal";
import { buildRunModel, endOf, isOpen, labelOf, outcomeOf } from "./run";

const T0 = Date.parse("2026-11-09T10:02:11Z");

function started() {
  return new JournalBuilder(T0).started({ script: "s".repeat(64), toolset: "t".repeat(64) });
}

describe("buildRunModel", () => {
  it("pairs each tool call with its completion, by seq, under Promise.all", () => {
    const j = started()
      .call(1, "github.get_issue", { number: 1 }, 7)
      .call(2, "github.get_issue", { number: 2 }, 7)
      .after(120)
      .ok(2, { title: "b" })
      .after(80)
      .fail(1, "timeout", "no answer in 30s");
    const model = buildRunModel(j.entries);

    expect(model.calls).toHaveLength(2);
    const [first, second] = model.calls;
    expect(first?.end).toMatchObject({ outcome: "failed", idx: 4, at: T0 + 200 });
    expect(first?.end?.outcome === "failed" && first.end.error.kind).toBe("timeout");
    expect(second?.end).toMatchObject({ outcome: "ok", idx: 3, at: T0 + 120 });
    expect(model.byLine.get(7)).toEqual([first, second]);
    // Both the schedule and the completion entries lead to the call.
    expect(model.byIdx.get(1)).toBe(first);
    expect(model.byIdx.get(4)).toBe(first);
    expect(model.byIdx.get(3)).toBe(second);
  });

  it("keeps a call without completion open", () => {
    const model = buildRunModel(started().call(1, "ask_user", { q: "?" }, 3).entries);
    const call = model.calls[0];
    expect(call && isOpen(call)).toBe(true);
    expect(call && outcomeOf(call)).toBe("open");
    expect(call && endOf(call)).toBeNull();
  });

  it("ends timers when they fire or are cancelled", () => {
    const j = started()
      .timer(1, 10 * 86_400_000, 12)
      .timer(2, 5_000, 13)
      .after(1_000)
      .cancelTimer(2)
      .after(10 * 86_400_000)
      .fired(1);
    const model = buildRunModel(j.entries);
    const [sleep, cancelled] = model.timers;
    expect(sleep).toMatchObject({ durationMs: 864_000_000, fireAt: T0 + 864_000_000 });
    expect(sleep?.end?.outcome).toBe("fired");
    expect(cancelled?.end).toMatchObject({ outcome: "cancelled", at: T0 + 1_000 });
    expect(sleep && labelOf(sleep)).toBe("sleep");
  });

  it("resolves a signal wait with the signal it was waiting for, and cancels its timeout", () => {
    const j = started()
      .timer(1, 3 * 86_400_000, 10)
      .awaitSignal("approval", 10, 1)
      .after(60_000)
      .signal("approval", { close: true })
      .cancelTimer(1);
    const model = buildRunModel(j.entries);
    const wait = model.signals[0];
    expect(wait?.timeoutAt).toBe(T0 + 3 * 86_400_000);
    expect(wait?.end).toMatchObject({ outcome: "received", buffered: false, at: T0 + 60_000 });
    expect(model.timers[0]?.timeoutOf).toBe(wait?.idx);
    expect(model.timers[0] && labelOf(model.timers[0])).toBe("timeout");
    expect(model.received[0]?.waitIdx).toBe(wait?.idx);
    expect(model.byIdx.get(model.received[0]?.idx ?? -1)).toBe(wait);
  });

  it("links the timeout timer when the wait is annotated before the timer starts", () => {
    const j = started().awaitSignal("approval", 10, 1).timer(1, 1_000, 10);
    const model = buildRunModel(j.entries);
    expect(model.signals[0]?.timeoutAt).toBe(T0 + 1_000);
    expect(model.timers[0]?.timeoutOf).toBe(1);
  });

  it("times a signal wait out when its timer fires first", () => {
    const j = started()
      .timer(1, 1_000, 10)
      .awaitSignal("approval", 10, 1)
      .after(1_000)
      .fired(1)
      .after(5_000)
      .signal("approval", { late: true });
    const model = buildRunModel(j.entries);
    expect(model.signals[0]?.end).toMatchObject({ outcome: "timed_out", at: T0 + 1_000 });
    // The late signal stays in the mailbox.
    expect(model.received[0]?.waitIdx).toBeNull();
  });

  it("gives a signal sent before the script waits to the next wait (FIFO mailbox)", () => {
    const j = started()
      .signal("approval", { n: 1 })
      .signal("approval", { n: 2 })
      .after(2_000)
      .awaitSignal("approval", 4)
      .awaitSignal("approval", 5);
    const model = buildRunModel(j.entries);
    const [a, b] = model.signals;
    expect(a?.end).toMatchObject({ outcome: "received", buffered: true, idx: 3 });
    expect(a?.end?.outcome === "received" && a.end.payload.value).toEqual({ n: 1 });
    expect(b?.end?.outcome === "received" && b.end.payload.value).toEqual({ n: 2 });
  });

  it("groups logs by line and keeps activities without a callsite apart", () => {
    const j = started()
      .log("info", "3 stale issues", 6)
      .log("warn", "no callsite", null)
      .timer(1, 100, null);
    const model = buildRunModel(j.entries);
    expect(model.logs.map((l) => l.message)).toEqual(["3 stale issues", "no callsite"]);
    expect(model.byLine.get(6)?.map((a) => a.kind)).toEqual(["log"]);
    expect(model.unattributed.map((a) => a.kind)).toEqual(["log", "timer"]);
  });

  it("records how the run ended, and a cancellation", () => {
    const failed = buildRunModel(
      started().cancelRequested("ttl").after(10).failRun("CancelledError", "run expired").entries,
    );
    expect(failed.cancel).toMatchObject({ reason: "ttl", message: null });
    expect(failed.end).toMatchObject({ outcome: "failed", error: { name: "CancelledError" } });

    const aborted = buildRunModel(started().abort("limit_exceeded", "CPU budget").entries);
    expect(aborted.end).toMatchObject({ outcome: "aborted", reason: { kind: "limit_exceeded" } });

    const done = buildRunModel(started().after(5).complete({ closed: 3 }).entries);
    expect(done.end?.outcome === "completed" && done.end.output.value).toEqual({ closed: 3 });
    expect(done.firstAt).toBe(T0);
    expect(done.lastAt).toBe(T0 + 5);
    expect(done.start?.event.mode).toBe("live");
  });
});
