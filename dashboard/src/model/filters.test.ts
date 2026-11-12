import { describe, expect, it } from "vitest";
import {
  NO_RUN_FILTERS,
  readRunFilters,
  readToolFilters,
  toRunsQuery,
  toToolStatsQuery,
  writeRunFilters,
} from "./filters";

const NOW = Date.parse("2026-11-12T10:00:00Z");

describe("run filters", () => {
  it("round-trip through the URL", () => {
    const filters = {
      ...NO_RUN_FILTERS,
      status: "failed" as const,
      labels: ["agent:triage", "conversation:c-42"],
      tool: "github.close_issue",
      error: "timeout",
      quarantined: false,
      range: "7d" as const,
    };
    const params = writeRunFilters(filters);
    expect(params.toString()).toBe(
      "status=failed&label=agent%3Atriage&label=conversation%3Ac-42&tool=github.close_issue&error=timeout&quarantined=no&range=7d",
    );
    expect(readRunFilters(params)).toEqual(filters);
  });

  it("ignore what they cannot read", () => {
    const filters = readRunFilters(
      new URLSearchParams("status=lost&label=nocolon&range=2y&tool=%20"),
    );
    expect(filters).toEqual(NO_RUN_FILTERS);
  });

  it("become the query of GET /v1/runs, with a range resolved against now", () => {
    const query = toRunsQuery(
      { ...readRunFilters(new URLSearchParams("label=env:prod&range=24h&quarantined=yes")) },
      NOW,
    );
    expect(query).toEqual({
      limit: 50,
      label: ["env:prod"],
      quarantined: true,
      startedAfter: "2026-11-11T10:00:00.000Z",
    });
    const bounded = toRunsQuery(
      { ...NO_RUN_FILTERS, after: "2026-11-01T00:00:00Z", before: "2026-11-02T00:00:00Z" },
      NOW,
    );
    expect(bounded).toMatchObject({
      startedAfter: "2026-11-01T00:00:00Z",
      startedBefore: "2026-11-02T00:00:00Z",
    });
  });
});

describe("tool filters", () => {
  it("default to the last 24 hours", () => {
    const filters = readToolFilters(new URLSearchParams(""));
    expect(filters).toEqual({ window: "24h", toolset: null });
    expect(toToolStatsQuery(filters, NOW)).toEqual({
      since: "2026-11-11T10:00:00.000Z",
      until: "2026-11-12T10:00:00.000Z",
    });
  });
});
