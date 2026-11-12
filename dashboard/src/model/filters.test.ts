import { describe, expect, it } from "vitest";
import { readToolFilters, toToolStatsQuery } from "./filters";

const NOW = Date.parse("2026-11-12T10:00:00Z");

describe("tool filters", () => {
  it("default to the last 24 hours", () => {
    const filters = readToolFilters(new URLSearchParams(""));
    expect(filters).toEqual({ window: "24h", toolset: null });
    expect(toToolStatsQuery(filters, NOW)).toEqual({
      since: "2026-11-11T10:00:00.000Z",
      until: "2026-11-12T10:00:00.000Z",
    });
  });

  it("ignore a window named like a property of every object", () => {
    const filters = readToolFilters(new URLSearchParams("window=constructor"));
    expect(filters.window).toBe("24h");
    expect(() => toToolStatsQuery(filters, NOW)).not.toThrow();
  });
});
