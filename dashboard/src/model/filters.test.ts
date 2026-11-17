import { describe, expect, it } from "vitest";
import { rangeOf, toScriptsQuery, toToolStatsQuery } from "./filters";

const NOW = Date.parse("2026-11-12T10:00:00Z");

describe("filters", () => {
  it("read the calls of a window, of one toolset or of all", () => {
    expect(toToolStatsQuery("24h", null, NOW)).toEqual({
      since: "2026-11-11T10:00:00.000Z",
      until: "2026-11-12T10:00:00.000Z",
    });
    expect(toToolStatsQuery("1h", "5f3a", NOW)).toEqual({
      since: "2026-11-12T09:00:00.000Z",
      until: "2026-11-12T10:00:00.000Z",
      toolset: "5f3a",
    });
  });

  it("ignore a window named like a property of every object", () => {
    expect(rangeOf("constructor")).toBeNull();
    expect(rangeOf("toString")).toBeNull();
    expect(rangeOf("7d")).toBe("7d");
  });

  it("ask for the scripts of labels and a tool", () => {
    expect(toScriptsQuery({ labels: [], tool: null })).toEqual({ limit: 50 });
    expect(toScriptsQuery({ labels: ["team:infra"], tool: "github.close_issue" })).toEqual({
      limit: 50,
      label: ["team:infra"],
      tool: "github.close_issue",
    });
  });
});
