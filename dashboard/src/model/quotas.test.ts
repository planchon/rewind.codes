import { describe, expect, it } from "vitest";
import {
  apiDuration,
  crossingDay,
  cumulativeCpu,
  daysInMonth,
  durationMs,
  formatCpu,
  formatShare,
  highestShare,
  levelOf,
  limitOf,
  monthLabel,
  monthOf,
  namespaceNameProblem,
  parseQuotas,
  quotaTexts,
  shareOf,
  shiftMonth,
  usedOf,
} from "./quotas";

const HOUR = 3_600_000;
const MINUTE = 60_000;

describe("quotas", () => {
  it("reads durations as the server does, and writes them in their shortest form", () => {
    expect(durationMs("1h30m")).toBe(90 * MINUTE);
    expect(durationMs("1.5h")).toBe(90 * MINUTE);
    expect(durationMs("500ms")).toBe(500);
    expect(durationMs("1200")).toBe(1_200);
    expect(durationMs(1_200)).toBe(1_200);
    expect(durationMs("2 h")).toBeNull();
    expect(durationMs("soon")).toBeNull();
    expect(durationMs(null)).toBeNull();
    expect(apiDuration(90 * MINUTE)).toBe("1h30m");
    expect(apiDuration(7 * 24 * HOUR)).toBe("1w");
    expect(apiDuration(0)).toBe("0ms");
  });

  it("writes CPU time coarsely", () => {
    expect(formatCpu(42_000)).toBe("42s");
    expect(formatCpu(41 * MINUTE + 10_000)).toBe("41m");
    expect(formatCpu(3 * HOUR + 12 * MINUTE)).toBe("3h 12m");
    expect(formatCpu(2 * HOUR)).toBe("2h");
    expect(formatCpu(410 * HOUR + 5 * MINUTE)).toBe("410h");
  });

  it("is orange from 80 % and red at 100 %, a CPU above its quota included", () => {
    expect(levelOf(shareOf(79, 100))).toBe("ok");
    expect(levelOf(shareOf(80, 100))).toBe("warning");
    expect(levelOf(shareOf(100, 100))).toBe("critical");
    expect(levelOf(shareOf(160, 100))).toBe("critical");
    expect(levelOf(shareOf(5, null))).toBe("none");
    // A limit of 0 blocks the end user: reached at once.
    expect(levelOf(shareOf(0, 0))).toBe("critical");
    expect(formatShare(1.6)).toBe("160%");
    expect(formatShare(0.799)).toBe("79%");
  });

  it("reads a namespace's use and limits per quota", () => {
    const usage = { activeRuns: 10, runsStarted: 2_950, cpu: "3h12m", workflows: 412 };
    const limits = { activeRuns: 10, runsPerMonth: 3_000, cpuPerMonth: "2h", workflows: 500 };
    expect(usedOf(usage, "cpuPerMonth")).toBe(3 * HOUR + 12 * MINUTE);
    expect(usedOf(usage, "runsPerMonth")).toBe(2_950);
    expect(limitOf(limits, "cpuPerMonth")).toBe(2 * HOUR);
    expect(limitOf(limits, "maxTtl")).toBeNull();
    expect(highestShare(usage, limits)).toEqual({ key: "cpuPerMonth", share: 1.6 });
  });

  it("adds a month's CPU time up day after day, to today", () => {
    const now = Date.parse("2026-11-05T10:00:00Z");
    const days = [
      { day: "2026-11-01", cpu: "10m", runsStarted: 3 },
      { day: "2026-11-03", cpu: "1h", runsStarted: 9 },
      { day: "2026-10-31", cpu: "5h", runsStarted: 1 },
    ];
    const points = cumulativeCpu(days, "2026-11", now);
    expect(points.map((p) => p.ms / MINUTE)).toEqual([10, 10, 70, 70, 70]);
    expect(crossingDay(points, HOUR, 80)?.day).toBe(3);
    expect(crossingDay(points, 2 * HOUR, 100)).toBeNull();
    // A past month runs to its last day; a month to come has no point.
    expect(cumulativeCpu(days, "2026-10", now)).toHaveLength(31);
    expect(cumulativeCpu(days, "2026-12", now)).toEqual([]);
  });

  it("names and shifts months, in UTC", () => {
    expect(monthOf(Date.parse("2026-11-30T23:59:59Z"))).toBe("2026-11");
    expect(monthLabel("2026-11")).toBe("November 2026");
    expect(shiftMonth("2026-01", -1)).toBe("2025-12");
    expect(daysInMonth("2026-02")).toBe(28);
  });

  it("checks a namespace's name as the server does", () => {
    expect(namespaceNameProblem("user_2fQ81")).toBeNull();
    expect(namespaceNameProblem("ada@example.net:prod")).toBeNull();
    expect(namespaceNameProblem("")).toMatch(/Type/);
    expect(namespaceNameProblem("a b")).toMatch(/letters/);
    expect(namespaceNameProblem("*")).toMatch(/letters/);
    expect(namespaceNameProblem("x".repeat(129))).toMatch(/128/);
  });

  it("reads the limits typed in a form, an empty field setting none", () => {
    const texts = quotaTexts({ activeRuns: 10, cpuPerMonth: "120m" });
    expect(texts.activeRuns).toBe("10");
    expect(texts.cpuPerMonth).toBe("2h");
    expect(texts.maxTtl).toBe("");
    expect(parseQuotas({ ...texts, maxTtl: " 7d ", workflows: "0" })).toEqual({
      quotas: { activeRuns: 10, cpuPerMonth: "2h", workflows: 0, maxTtl: "1w" },
      errors: null,
    });
    const refused = parseQuotas({ ...texts, activeRuns: "ten", cpuPerMonth: "2" });
    expect(refused.quotas).toBeNull();
    expect(Object.keys(refused.errors ?? {}).sort()).toEqual(["activeRuns", "cpuPerMonth"]);
  });
});
