import { describe, expect, it } from "vitest";
import {
  labelsTaken,
  readSeries,
  secretNameProblem,
  secretValueProblem,
  stopsNothing,
  totalReads,
} from "./secrets";

const NOW = Date.parse("2026-11-20T15:00:00Z");

describe("secrets", () => {
  it("takes the names the API takes", () => {
    for (const name of ["github", "gh_token-2", "a", "a".repeat(64)]) {
      expect(secretNameProblem(name)).toBeNull();
    }
    expect(secretNameProblem("")).toMatch(/name/);
    expect(secretNameProblem("GitHub")).toMatch(/lowercase/);
    expect(secretNameProblem("2fa")).toMatch(/Start/);
    expect(secretNameProblem("org/github")).toMatch(/Only/);
    expect(secretNameProblem("a".repeat(65))).toMatch(/64/);
  });

  it("refuses an empty value and one above 16 KiB, counted in UTF-8", () => {
    expect(secretValueProblem("")).not.toBeNull();
    expect(secretValueProblem("x".repeat(16 * 1024))).toBeNull();
    expect(secretValueProblem("é".repeat(8 * 1024 + 1))).toMatch(/16 KiB/);
  });

  it("spreads the reads over the last 30 days, a quiet day at 0", () => {
    const series = readSeries(
      [
        { day: "2026-10-01", reads: 99 },
        { day: "2026-10-22", reads: 3 },
        { day: "2026-11-19", reads: 5 },
        { day: "2026-11-20", reads: 7 },
      ],
      NOW,
    );
    expect(series).toHaveLength(30);
    expect(series[0]).toBe(3);
    expect(series.slice(-2)).toEqual([5, 7]);
    expect(series.reduce((a, b) => a + b, 0)).toBe(15);
    expect(
      totalReads([
        { day: "2026-11-19", reads: 5 },
        { day: "2026-11-20", reads: 7 },
      ]),
    ).toBe(12);
  });

  it("gives a secret written under a grant limited to labels those labels", () => {
    expect(labelsTaken({ team: "a" }, { env: "dev" })).toEqual({ team: "a", env: "dev" });
  });

  it("knows when a deletion stops nothing", () => {
    const usage = {
      secret: {} as never,
      activeRuns: [],
      activeTotal: 0,
      waitingRuns: [],
      waitingTotal: 0,
      workflows: [],
    };
    expect(stopsNothing(usage)).toBe(true);
    expect(
      stopsNothing({ ...usage, workflows: [{ name: "triage", version: "v", servers: [] }] }),
    ).toBe(false);
  });
});
