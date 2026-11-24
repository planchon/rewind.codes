import { describe, expect, it } from "vitest";
import { createWorld, WORKFLOWS } from "./fixtures";
import { QueryError } from "./query";
import { fingerprints, listRuns, listWorkflows, runStats, signature } from "./readmodel";

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
    expect(ids("@agent:triage action:github.close_issue")).toHaveLength(7);
    expect(ids("@agent:triage -@env:test action:github.*").length).toBeGreaterThan(4);
    expect(ids(`"reading 'text'"`)).toHaveLength(1);
    expect(ids("waiting:signal signal:approval")).toHaveLength(1);
    expect(ids("status:failed,aborted error:TypeError,limit_exceeded")).toHaveLength(2);
    const recent = ids("started:>now-1h");
    expect(recent.map((r) => r.labels.conversation)).toEqual(
      expect.arrayContaining(["c-81", "c-85"]),
    );
    expect(recent.every((r) => Date.parse(r.startedAt) > NOW - 3_600_000)).toBe(true);
    expect(ids("waiting:quarantined")).toHaveLength(1);
    expect(ids("calls:>=5 mode:live").length).toBeGreaterThan(0);
    expect(ids("mode:test")).toHaveLength(2);
    expect(ids("workflow:triage")).toHaveLength(2);
    expect(ids(`workflow:${WORKFLOWS.triage}`)).toHaveLength(2);
  });

  it("gives each run the status its waits say", () => {
    const runs = (q: string) => listRuns(world, { q, limit: 500 }, NOW).runs;
    expect(runs("status:queued")).toHaveLength(1);
    // A call Rewind makes itself is in flight: running, waiting for nothing.
    const running = runs("status:running");
    expect(running).toHaveLength(1);
    expect(running[0]?.waitingFor).toBeUndefined();
    const waiting = runs("status:waiting");
    expect(waiting.every((r) => (r.waitingFor ?? []).length > 0)).toBe(true);
    expect(new Set(waiting.flatMap((r) => (r.waitingFor ?? []).map((w) => w.kind)))).toEqual(
      new Set(["signal", "timer", "harness", "approval", "parked", "quarantined"]),
    );
    expect(runs("status:active")).toHaveLength(2 + waiting.length);
  });

  it("reads the namespace of runs: an exact name, never a pattern", () => {
    const runs = (q: string) => listRuns(world, { q, limit: 500 }, NOW).runs;
    const bob = world.runs.filter((r) => r.namespace === "bob").length;
    expect(runs("namespace:bob")).toHaveLength(bob);
    expect(runs("namespace:bob").every((r) => r.namespace === "bob")).toBe(true);
    expect(runs("namespace:bob,alice").length).toBeGreaterThan(bob);
    expect(runs("-namespace:bob").some((r) => r.namespace === "bob")).toBe(false);
    expect(refusal("namespace:bo*").message).toBe("namespace: * is not accepted here");
    expect(refusal("namespace:a/b").message).toMatch(/is not a namespace/);
  });

  it("names the term it cannot read, in characters", () => {
    expect(refusal("@env:prod stauts:failed")).toMatchObject({ start: 10, end: 23 });
    expect(refusal("@who:😀 stauts:x").start).toBe(7);
    expect(refusal("status:lost").message).toBe(
      'status: "lost" is not one of queued, running, waiting, completed, failed, cancelled, aborted, active',
    );
    expect(refusal('action:x "rate').message).toMatch(/a quote is not closed/);
    expect(refusal("action_set:9f2c").message).toMatch(/at least 8 hexadecimal digits/);
    expect(refusal("tool:github.*").message).toMatch(/unknown field "tool"/);
  });

  it("gives statistics over a window and the runs waiting now", () => {
    const since = new Date(NOW - 7 * 86_400_000).toISOString();
    const stats = runStats(world, { since, interval: "1d" }, NOW);
    expect(stats.series).toHaveLength(7);
    const counted = stats.series?.reduce(
      (n, b) => n + Object.values(b.statuses).reduce((a, c) => a + c, 0),
      0,
    );
    expect(counted).toBe(stats.runs);
    expect(stats.waiting).toEqual({
      signal: 1,
      timer: 1,
      harness: 1,
      approval: 1,
      deferred: 0,
      parked: 1,
      quarantined: 1,
    });
    expect(stats.facets.labels.map((l) => l.key)).toContain("agent");
    expect(stats.durations?.p50).toBeGreaterThan(0);
  });

  it("counts failures by fingerprint, across workflows", () => {
    expect(signature("ActionError", 'no answer within 5s for "x" at 2026-11-01T10:00:00Z')).toBe(
      "ActionError: no answer within <n>s for <str> at <time>",
    );
    const page = fingerprints(world, {}, NOW);
    expect(page.totalFingerprints).toBe(page.fingerprints.length);
    expect(page.runs).toBe(page.fingerprints.reduce((n, g) => n + g.runs, 0));
    const timeout = page.fingerprints.find((g) => g.signature.includes("no answer within"));
    expect(timeout?.error).toBe("ActionError");
    expect(timeout?.line).toBe(5);
    // Its latest run by its handle, as every reference to a run.
    for (const g of page.fingerprints)
      expect(g.latest.handle).toMatch(/^[^/]+\/[^/]+\/[0-9a-z]{12,25}$/);
  });

  it("lists the workflows, each with its runs of the window", () => {
    const all = listWorkflows(world, { query: { limit: 500 }, namespace: null, labels: {} }, NOW);
    const byId = new Map(all.workflows.map((w) => [w.id, w]));
    expect(byId.get(WORKFLOWS.triage)).toMatchObject({ name: "triage", latest: { version: 3 } });
    expect(byId.get(WORKFLOWS.triage)?.namespace).toBeUndefined();
    expect(byId.get(WORKFLOWS.triageFork)).toMatchObject({
      namespace: "dana",
      source: { workflow: WORKFLOWS.triage, version: 2 },
    });
    expect(byId.get(WORKFLOWS.forecast)?.latest.comment?.title).toBe(
      "One-shot lookup started by the support agent.",
    );
    expect(byId.get(WORKFLOWS.chenDigest)).toMatchObject({
      namespace: "chen",
      shortId: `wf-${WORKFLOWS.chenDigest.slice(3, 9)}`,
    });
    // A namespace's workflows and the organization's, counted over its runs.
    const alice = listWorkflows(
      world,
      { query: { q: "namespace:alice", limit: 500 }, namespace: "alice", labels: {} },
      NOW,
    );
    expect(alice.workflows.every((w) => w.namespace === undefined || w.namespace === "alice")).toBe(
      true,
    );
    expect(alice.workflows.some((w) => w.id === WORKFLOWS.chenDigest)).toBe(false);
    // Paged by id.
    const first = listWorkflows(world, { query: { limit: 3 }, namespace: null, labels: {} }, NOW);
    expect(first.workflows).toHaveLength(3);
    const next = listWorkflows(
      world,
      { query: { limit: 500, after: first.next ?? "" }, namespace: null, labels: {} },
      NOW,
    );
    expect(first.workflows.length + next.workflows.length).toBe(all.workflows.length);
  });
});
