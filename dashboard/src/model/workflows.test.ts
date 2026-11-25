import { describe, expect, it } from "vitest";
import { HistoryBuilder, payload } from "../mocks/history";
import { buildRunModel } from "./run";
import type { WorkflowVersion } from "./types";
import {
  callName,
  endedRunOf,
  expectedTimes,
  findVersion,
  isNamed,
  readRunEnd,
  recordedRunOf,
  rulesOf,
  ruleTimes,
  shortIdOf,
  signalTargetOf,
  signalWhen,
  startedRuns,
  suiteCounts,
  suiteOf,
  suitePlaces,
  summarizeCassette,
  testPath,
  versionLabel,
  workflowCallOf,
  workflowLabel,
  workflowPath,
  workflowRef,
} from "./workflows";

const CHILD = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10";
const OTHER = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e11";

describe("workflows in a history", () => {
  it("names the calls of a workflow's built-ins as the code writes them", () => {
    expect(workflowCallOf("workflows.triage.start")).toEqual({ workflow: "triage", op: "start" });
    expect(workflowCallOf("workflows.triage.signal")).toEqual({ workflow: "triage", op: "signal" });
    expect(workflowCallOf("github.list_issues")).toBeNull();
    expect(callName("workflows.triage.start")).toBe("triage.start(…)");
    const signal = payload({ name: "approval", payload: { close: true }, run: CHILD });
    expect(callName("workflows.triage.signal", signal)).toBe('run.signal("approval", …)');
    expect(signalTargetOf(signal)).toBe(CHILD);
    expect(callName("github.list_issues", signal)).toBe("github.list_issues");
  });

  it("shows a workflow by its name, its comment or its short id, and a version by its number", () => {
    const id = "wf_7k2m9q4x8d1c";
    expect(shortIdOf(id)).toBe("wf-7k2m9q");
    expect(workflowLabel({ id, name: "triage", comment: { title: "Triage" } })).toBe("triage");
    expect(workflowLabel({ id, comment: { title: "Triage new issues" } })).toBe(
      "Triage new issues",
    );
    expect(workflowLabel({ id, shortId: "wf-7k2m9q" })).toBe("wf-7k2m9q");
    expect(workflowLabel({ id })).toBe("wf-7k2m9q");
    expect(isNamed({ name: null })).toBe(false);
    expect(versionLabel("triage", 12)).toBe("triage v12");
    expect(workflowRef({ id, name: "triage", version: 12, digest: "d" })).toBe("triage v12");
    expect(workflowRef({ id, version: 3, digest: "d" }, "Triage new issues")).toBe(
      "Triage new issues v3",
    );
    expect(workflowRef({ id, version: 1, digest: "d" })).toBe("wf-7k2m9q v1");
  });

  it("reads the signal name from a preview when the args were left out", () => {
    const big = payload({ name: "approval", payload: "x".repeat(400), run: CHILD }, 10);
    expect(big.value).toBeUndefined();
    expect(callName("workflows.triage.signal", big)).toBe('run.signal("approval", …)');
  });

  it("reads how a run ended from its end signal", () => {
    expect(endedRunOf(`rewind:ended:${CHILD}`)).toBe(CHILD);
    expect(endedRunOf("approval")).toBeNull();
    expect(readRunEnd(payload({ status: "completed", output: { closed: 2 } }))).toEqual({
      status: "completed",
      message: null,
    });
    expect(
      readRunEnd(payload({ status: "failed", error: { name: "ActionError", message: "403" } })),
    ).toEqual({ status: "failed", message: "ActionError: 403" });
    expect(
      readRunEnd(
        payload({ status: "aborted", reason: { kind: "limit_exceeded", message: "CPU" } }),
      ),
    ).toEqual({ status: "aborted", message: "limit_exceeded: CPU" });
    // Above the inline limit, the preview's first key says how it ended.
    const left = (value: unknown) => payload(value, 10);
    expect(readRunEnd(left({ status: "completed", output: "x".repeat(400) })).status).toBe(
      "completed",
    );
    expect(
      readRunEnd(
        left({
          status: "cancelled",
          error: { name: "CancelledError", message: "cancelled", stack: "x".repeat(400) },
        }),
      ).status,
    ).toBe("cancelled");
    expect(readRunEnd(left({ status: "aborted", reason: { kind: "x".repeat(400) } })).status).toBe(
      "aborted",
    );
  });

  it("lists the runs a run started, with their ends once they came back", () => {
    const journal = new HistoryBuilder(Date.parse("2026-11-12T10:00:00Z"))
      .started({ code: "s", actionSet: "t" })
      .call(1, "workflows.triage.start", { input: { repo: "acme/api" } }, 4)
      .call(2, "workflows.digest.start", { detached: true, input: { repos: [] } }, 5)
      .call(3, "workflows.triage.start", { input: { repo: "acme/web" } }, 4)
      .ok(1, { runId: CHILD })
      .ok(2, { runId: OTHER })
      .fail(3, "unavailable", "the organization has its maximum of active runs")
      .awaitSignal(`rewind:ended:${CHILD}`, 6)
      .signal(`rewind:ended:${CHILD}`, {
        status: "failed",
        error: { name: "ActionError", message: "github.close_issue: 403" },
      });
    const runs = startedRuns(buildRunModel(journal.events));
    expect(
      runs.map((r) => ({ runId: r.runId, workflow: r.workflow, detached: r.detached })),
    ).toEqual([
      { runId: CHILD, workflow: "triage", detached: false },
      { runId: OTHER, workflow: "digest", detached: true },
    ]);
    expect(runs[0]?.end).toMatchObject({
      status: "failed",
      message: "ActionError: github.close_issue: 403",
      idx: 8,
    });
    expect(runs[1]?.end).toBeNull();
  });
});

function version(n: number, overrides: Partial<WorkflowVersion> = {}): WorkflowVersion {
  return {
    version: n,
    digest: String(n).repeat(64).slice(0, 64),
    codeDigest: "c".repeat(64),
    actionSet: "t",
    input: {},
    output: {},
    signals: {},
    labels: {},
    createdAt: "2026-11-01T00:00:00Z",
    ...overrides,
  };
}

describe("a version's suite", () => {
  const first = version(1, {
    tests: [
      { name: "a", digest: "da" },
      { name: "b", digest: "db" },
      { name: "c", digest: "dc" },
    ],
  });
  const recorded = `recorded ${CHILD}`;
  const second = version(2, {
    base: first.digest,
    tests: [
      { name: "a", digest: "da" },
      { name: "b", digest: "db2" },
      { name: recorded, digest: "dr" },
    ],
    replaced: [{ name: "b", old: "db", new: "db2", reason: "the message changed" }],
    retired: [{ name: "c", digest: "dc", reason: "deprecated" }],
  });

  it("says which tests were inherited, added, replaced and retired", () => {
    const suite = suiteOf(second, [second, first]);
    expect(suite.base).toBe(first);
    expect(suite.baseMissing).toBe(false);
    expect(suite.tests.map((t) => [t.name, t.kind, t.from, t.reason])).toEqual([
      ["a", "inherited", null, null],
      ["b", "replaced", "db", "the message changed"],
      [recorded, "added", null, null],
    ]);
    expect(suite.tests[2]?.recordedFrom).toBe(CHILD);
    expect(suite.retired).toEqual([{ name: "c", digest: "dc", reason: "deprecated" }]);
    expect(suiteCounts(suite)).toEqual({
      inherited: 1,
      added: 1,
      replaced: 1,
      kept: 0,
      retired: 1,
    });
  });

  it("lists the tests to go through: the suite's, then those it retired", () => {
    const places = suitePlaces(suiteOf(second, [second, first]));
    expect(places.map((t) => [t.name, t.kind, t.digest, t.reason])).toEqual([
      ["a", "inherited", "da", null],
      ["b", "replaced", "db2", "the message changed"],
      [recorded, "added", "dr", null],
      ["c", "retired", "dc", "deprecated"],
    ]);
  });

  it("names the pages of a workflow and of a test, at a version or the latest", () => {
    expect(workflowPath("wf_7k2m9q4x8d1c")).toBe("/workflows/wf_7k2m9q4x8d1c");
    expect(workflowPath("wf_7k2m9q4x8d1c", 2)).toBe("/workflows/wf_7k2m9q4x8d1c?version=2");
    expect(testPath("wf_7k2m9q4x8d1c", "dc")).toBe("/workflows/wf_7k2m9q4x8d1c/tests/dc");
    expect(testPath("wf_7k2m9q4x8d1c", "dc", 2)).toBe(
      "/workflows/wf_7k2m9q4x8d1c/tests/dc?version=2",
    );
  });

  it("adds every test of a first version", () => {
    expect(suiteOf(first, [second, first]).tests.map((t) => t.kind)).toEqual([
      "added",
      "added",
      "added",
    ]);
  });

  it("cannot tell inherited from added when the base is not among the versions", () => {
    const suite = suiteOf(second, [second]);
    expect(suite.base).toBeNull();
    expect(suite.baseMissing).toBe(true);
    expect(suite.tests.map((t) => t.kind)).toEqual(["kept", "replaced", "kept"]);
  });

  it("shows a test changed without a reason as replaced", () => {
    const silent = version(3, { base: first.digest, tests: [{ name: "a", digest: "da2" }] });
    expect(suiteOf(silent, [silent, first]).tests[0]).toMatchObject({
      kind: "replaced",
      from: "da",
      reason: null,
    });
  });

  it("finds a version by its number, its digest or its first 12 digits", () => {
    expect(findVersion([second, first], "1")).toBe(first);
    expect(findVersion([second, first], "v2")).toBe(second);
    expect(findVersion([second, first], "v9")).toBeNull();
    expect(findVersion([second, first], first.digest)).toBe(first);
    expect(findVersion([second, first], first.digest.slice(0, 12))).toBe(first);
    expect(findVersion([second, first], "111")).toBeNull();
  });

  it("reads the run of a recorded test from its name", () => {
    expect(recordedRunOf(`recorded ${CHILD}`)).toBe(CHILD);
    expect(recordedRunOf("recorded by hand")).toBeNull();
  });
});

describe("a test, in words", () => {
  it("says how many calls a rule answers and an expectation asks for", () => {
    expect(rulesOf({ returns: 1 })).toEqual([{ returns: 1 }]);
    expect(rulesOf([{ returns: 1 }, { returns: 2 }])).toHaveLength(2);
    expect(ruleTimes(undefined)).toBe("every call");
    expect(ruleTimes(1)).toBe("1 call");
    expect(ruleTimes(3)).toBe("3 calls");
    expect(expectedTimes({ action: "a" })).toBe("at least once");
    expect(expectedTimes({ action: "a", times: 0 })).toBe("never");
    expect(expectedTimes({ action: "a", times: 1 })).toBe("exactly once");
    expect(expectedTimes({ action: "a", times: 4 })).toBe("exactly 4 times");
    expect(signalWhen(undefined)).toBe("once the run waits");
    expect(signalWhen("waiting")).toBe("once the run waits");
    expect(signalWhen("2h")).toBe("after 2h");
  });

  it("sums a cassette up", () => {
    expect(
      summarizeCassette({
        v: 1,
        matching: "by_args",
        calls: [
          { action: "github.close_issue", args: {}, outcome: { result: {} } },
          { action: "github.list_issues", args: {}, outcome: { result: [] } },
          { tool: "github.close_issue", args: {}, outcome: { result: {} } },
        ],
        signals: [{ name: "approval", payload: true, elapsed_ms: 60_000 }],
      }),
    ).toEqual({
      matching: "by_args",
      calls: 3,
      byAction: [
        { action: "github.close_issue", count: 2 },
        { action: "github.list_issues", count: 1 },
      ],
      signals: [{ name: "approval", at: 60_000 }],
    });
    expect(summarizeCassette(null)).toBeNull();
  });
});
