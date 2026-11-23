import { describe, expect, it } from "vitest";
import { buildTimeScale } from "./timescale";

const DAY = 86_400_000;

describe("buildTimeScale", () => {
  it("is linear when no gap stands out", () => {
    const scale = buildTimeScale([0, 1_000, 2_500, 4_000]);
    expect(scale.breaks).toEqual([]);
    expect(scale.x(0)).toBe(0);
    expect(scale.x(2_000)).toBeCloseTo(0.5);
    expect(scale.x(4_000)).toBe(1);
  });

  it("compresses a ten-day sleep between millisecond calls", () => {
    const times = [0, 300, 400, 400 + 10 * DAY, 400 + 10 * DAY + 200];
    const scale = buildTimeScale(times, { breakWidth: 0.05 });
    expect(scale.breaks).toHaveLength(1);
    const [gap] = scale.breaks;
    expect(gap).toMatchObject({ from: 400, to: 400 + 10 * DAY });
    expect((gap?.x1 ?? 0) - (gap?.x0 ?? 0)).toBeCloseTo(0.05);
    // The 300 ms call keeps half of the linear width (600 ms of linear time in all).
    expect(scale.x(300)).toBeCloseTo(0.95 * 0.5);
    expect(scale.x(400 + 10 * DAY + 100)).toBeCloseTo(1 - 0.95 / 6);
  });

  it("never compresses a slow tool call", () => {
    // 35 ms, a 1.84 s call, 140 ms: all of it is shown linearly.
    const scale = buildTimeScale([0, 35, 1_875, 2_015]);
    expect(scale.breaks).toEqual([]);
    expect(scale.x(1_875)).toBeCloseTo(1_875 / 2_015);
  });

  it("is monotonic, inside breaks too", () => {
    const times = [0, 50, 5 * DAY, 5 * DAY + 70, 9 * DAY, 9 * DAY + 10];
    const scale = buildTimeScale(times);
    let previous = -1;
    for (let t = 0; t <= 9 * DAY + 10; t += DAY / 7) {
      const x = scale.x(t);
      expect(x).toBeGreaterThanOrEqual(previous);
      previous = x;
    }
    expect(scale.breaks).toHaveLength(2);
  });

  it("does not compress when there is nothing else to show", () => {
    const scale = buildTimeScale([0, 10 * DAY]);
    expect(scale.breaks).toEqual([]);
    expect(scale.x(5 * DAY)).toBeCloseTo(0.5);
  });

  it("handles a single instant", () => {
    const scale = buildTimeScale([42]);
    expect(scale.x(42)).toBe(0);
    expect(scale.ticks[0]).toMatchObject({ label: "+0s", major: true });
  });

  it("labels the start of each section with its full offset", () => {
    const scale = buildTimeScale([0, 300, 400, 400 + 10 * DAY, 400 + 10 * DAY + 200]);
    const majors = scale.ticks.filter((t) => t.major).map((t) => t.label);
    expect(majors).toEqual(["+0s", "+10d"]);
    const minors = scale.ticks.filter((t) => !t.major);
    expect(minors.length).toBeGreaterThan(0);
    for (const tick of scale.ticks) {
      expect(tick.x).toBeGreaterThanOrEqual(0);
      expect(tick.x).toBeLessThanOrEqual(1);
    }
  });

  it("labels nothing inside a break that ends the axis: a run idle until now", () => {
    // Calls within the first second, then waiting for an hour.
    const scale = buildTimeScale([0, 120, 480, 900, 3_600_000]);
    expect(scale.breaks).toHaveLength(1);
    const idle = scale.breaks[0];
    expect(idle?.x1).toBe(1);
    const inside = scale.ticks.filter((t) => idle && t.x > idle.x0 + 1e-9);
    expect(inside).toEqual([]);
    // The ticks of the calls leave room for the break's label.
    const last = scale.ticks[scale.ticks.length - 1];
    expect(idle && last ? idle.x0 - last.x : 0).toBeGreaterThan(0.08);
  });
});
