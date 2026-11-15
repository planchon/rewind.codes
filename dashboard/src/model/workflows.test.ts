import { describe, expect, it } from "vitest";
import { JournalBuilder, payload } from "../mocks/journal";
import { buildRunModel } from "./run";
import {
  callName,
  endedRunOf,
  readRunEnd,
  signalTargetOf,
  startedRuns,
  workflowCallOf,
  workflowRef,
} from "./workflows";

const CHILD = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10";
const OTHER = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e11";

describe("workflows in a journal", () => {
  it("names the calls of a workflow's built-ins as the script writes them", () => {
    expect(workflowCallOf("workflows.triage.start")).toEqual({ workflow: "triage", op: "start" });
    expect(workflowCallOf("workflows.triage.signal")).toEqual({ workflow: "triage", op: "signal" });
    expect(workflowCallOf("github.list_issues")).toBeNull();
    expect(callName("workflows.triage.start")).toBe("triage.start(…)");
    const signal = payload({ name: "approval", payload: { close: true }, run: CHILD });
    expect(callName("workflows.triage.signal", signal)).toBe('run.signal("approval", …)');
    expect(signalTargetOf(signal)).toBe(CHILD);
    expect(callName("github.list_issues", signal)).toBe("github.list_issues");
    expect(workflowRef({ name: "triage", version: "9f2c4e1a07b3d5f8a1c2e3b4" })).toBe(
      "triage@9f2c4e1a07b3",
    );
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
      readRunEnd(payload({ status: "failed", error: { name: "ToolError", message: "403" } })),
    ).toEqual({ status: "failed", message: "ToolError: 403" });
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
    const journal = new JournalBuilder(Date.parse("2026-11-12T10:00:00Z"))
      .started({ script: "s", toolset: "t" })
      .call(1, "workflows.triage.start", { input: { repo: "acme/api" } }, 4)
      .call(2, "workflows.digest.start", { detached: true, input: { repos: [] } }, 5)
      .call(3, "workflows.triage.start", { input: { repo: "acme/web" } }, 4)
      .ok(1, { runId: CHILD })
      .ok(2, { runId: OTHER })
      .fail(3, "unavailable", "the tenant has its maximum of active runs")
      .awaitSignal(`rewind:ended:${CHILD}`, 6)
      .signal(`rewind:ended:${CHILD}`, {
        status: "failed",
        error: { name: "ToolError", message: "github.close_issue: 403" },
      });
    const runs = startedRuns(buildRunModel(journal.entries));
    expect(
      runs.map((r) => ({ runId: r.runId, workflow: r.workflow, detached: r.detached })),
    ).toEqual([
      { runId: CHILD, workflow: "triage", detached: false },
      { runId: OTHER, workflow: "digest", detached: true },
    ]);
    expect(runs[0]?.end).toMatchObject({
      status: "failed",
      message: "ToolError: github.close_issue: 403",
      idx: 8,
    });
    expect(runs[1]?.end).toBeNull();
  });
});
