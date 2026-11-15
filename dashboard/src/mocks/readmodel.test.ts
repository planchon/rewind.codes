import { describe, expect, it } from "vitest";
import { createWorld } from "./fixtures";
import { QueryError } from "./query";
import { failureGroups, listRuns, runStats, signature } from "./readmodel";

const NOW = Date.parse("2026-11-12T10:00:00Z");
const world = createWorld({ now: NOW });

function refusal(q: string): { message: string; start: number; end: number } {
  try {
    listRuns(world, { q }, NOW);
  } catch (e) {
    if (e instanceof QueryError) return { message: e.message, start: e.start, end: e.end };
  }
  throw new Error(`${q} was read`);
}

describe("the mocked read model", () => {
  it("reads the query language as the server does", () => {
    const ids = (q: string) => listRuns(world, { q, limit: 500 }, NOW).runs;
    // Four triage runs, the corpus run and the two runs of the workflow triage.
    expect(ids("@agent:triage tool:github.close_issue")).toHaveLength(7);
    expect(ids("@agent:triage -@env:test tool:github.*").length).toBeGreaterThan(4);
    expect(ids(`"reading 'text'"`)).toHaveLength(1);
    expect(ids("waiting:signal signal:approval")).toHaveLength(1);
    expect(ids("status:failed,aborted error:TypeError,limit_exceeded")).toHaveLength(2);
    const recent = ids("started:>now-1h");
    expect(recent.map((r) => r.labels.conversation)).toEqual(
      expect.arrayContaining(["c-81", "c-85"]),
    );
    expect(recent.every((r) => Date.parse(r.startedAt) > NOW - 3_600_000)).toBe(true);
    expect(ids("quarantined:yes")).toHaveLength(1);
    expect(ids("calls:>=5 mode:live").length).toBeGreaterThan(0);
  });

  it("names the term it cannot read, in characters", () => {
    expect(refusal("@env:prod stauts:failed")).toMatchObject({ start: 10, end: 23 });
    expect(refusal("@who:😀 stauts:x").start).toBe(7);
    expect(refusal("status:lost").message).toBe(
      'status: "lost" is not one of pending, active, completed, failed, cancelled, aborted',
    );
    expect(refusal('tool:x "rate').message).toMatch(/a quote is not closed/);
    expect(refusal("script:9f2c").message).toMatch(/at least 8 hexadecimal digits/);
  });

  it("gives statistics over a window and the runs waiting now", () => {
    const since = new Date(NOW - 7 * 86_400_000).toISOString();
    const stats = runStats(world, { since, bucket: "1d" }, NOW);
    expect(stats.series).toHaveLength(7);
    const counted = stats.series?.reduce(
      (n, b) => n + Object.values(b.statuses).reduce((a, c) => a + c, 0),
      0,
    );
    expect(counted).toBe(stats.runs);
    expect(stats.waiting).toMatchObject({ signal: 1, timer: 1, call: 1, host: 1, approval: 1 });
    expect(stats.facets.labels.map((l) => l.key)).toContain("agent");
    expect(stats.durations?.p50).toBeGreaterThan(0);
  });

  it("groups failures by their signature, across scripts", () => {
    expect(signature("ToolError", 'no answer within 5s for "x" at 2026-11-01T10:00:00Z')).toBe(
      "ToolError: no answer within <n>s for <str> at <time>",
    );
    const page = failureGroups(world, {}, NOW);
    expect(page.totalGroups).toBe(page.groups.length);
    expect(page.runs).toBe(page.groups.reduce((n, g) => n + g.runs, 0));
    const timeout = page.groups.find((g) => g.signature.includes("no answer within"));
    expect(timeout?.error).toBe("ToolError");
    expect(timeout?.line).toBe(5);
  });
});
