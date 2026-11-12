// What the server's read model answers, computed from the mocked world: run states, run
// summaries, scripts and tool statistics. Entries recorded after the current time are not
// visible yet, so a run of the world can go on live.
import type {
  Entry,
  ListRunsQuery,
  ListScriptsQuery,
  Run,
  RunCounts,
  RunPage,
  RunStatus,
  RunSummary,
  ScriptPage,
  ScriptSummary,
  ToolStats,
  ToolStatsPage,
  ToolStatsQuery,
  Waiting,
} from "../api/types";
import { buildRunModel, type RunModel } from "../model/run";
import type { RunFixture, World } from "./fixtures";
import { fakeDigest } from "./journal";

export function visibleEntries(fixture: RunFixture, now: number): Entry[] {
  const entries = fixture.journal.entries;
  let end = 0;
  while (end < entries.length && Date.parse(entries[end]?.recordedAt ?? "") <= now) end += 1;
  return entries.slice(0, end);
}

function statusOf(model: RunModel): RunStatus {
  if (model.entries.length === 0) return "pending";
  const end = model.end;
  if (!end) return "active";
  if (end.outcome === "completed") return "completed";
  if (end.outcome === "aborted") return "aborted";
  return end.error.name === "CancelledError" ? "cancelled" : "failed";
}

function frame(source: string, line: number, column: number): string {
  const lines = source.split("\n");
  const out: string[] = [];
  for (let n = Math.max(1, line - 2); n <= Math.min(lines.length, line + 1); n += 1) {
    out.push(`${n === line ? ">" : " "} ${String(n).padStart(3)} | ${lines[n - 1] ?? ""}`);
    if (n === line) out.push(`       | ${" ".repeat(Math.max(0, column - 1))}^`);
  }
  return out.join("\n");
}

export function deriveRun(fixture: RunFixture, now: number): Run {
  const entries = visibleEntries(fixture, now);
  const model = buildRunModel(entries);
  const status = statusOf(model);
  const start = model.start?.event;
  const first = entries[0]?.recordedAt ?? new Date(now).toISOString();
  const last = entries[entries.length - 1]?.recordedAt ?? first;
  const run: Run = {
    id: fixture.id,
    status,
    script: fixture.script.digest,
    toolset: fixture.toolset,
    labels: fixture.labels,
    mode: start?.mode ?? "live",
    epoch: start?.epoch ?? 1,
    input: start?.input ?? { digest: fakeDigest("{}"), bytes: 2, preview: "{}", value: {} },
    createdAt: first,
    updatedAt: last,
    expiresAt: start?.expiresAt ?? first,
    historyLen: entries.length,
    traceId: fakeDigest(fixture.id).slice(0, 32),
    logs: model.logs.slice(-20).map((l) => ({
      idx: l.idx,
      at: new Date(l.at).toISOString(),
      level: l.level,
      message: l.message,
      callsite: l.callsite,
    })),
  };
  if (fixture.parent) run.parent = { run: fixture.parent.run, at: fixture.parent.at ?? null };
  if (fixture.quarantine && status === "active") run.quarantine = fixture.quarantine;
  const end = model.end;
  if (end) run.endedAt = new Date(end.at).toISOString();
  if (end?.outcome === "completed") run.output = end.output;
  if (end?.outcome === "aborted") run.abort = end.reason;
  if (end?.outcome === "failed") {
    const at = fixture.errorAt;
    const failed =
      fixture.failedSeq === undefined
        ? undefined
        : model.calls.find((c) => c.seq === fixture.failedSeq);
    run.error = {
      name: end.error.name,
      message: end.error.message,
      stack: end.error.stack ?? null,
      location: at ?? null,
      frame: at ? frame(fixture.script.source, at.line, at.column) : null,
      failedCall:
        failed && failed.end?.outcome === "failed"
          ? {
              seq: failed.seq,
              tool: failed.tool,
              args: failed.args,
              callsite: failed.callsite,
              error: failed.end.error,
            }
          : null,
    };
  }
  if (status === "active") {
    const waiting: Waiting[] = [];
    for (const call of model.calls) {
      if (call.end) continue;
      const state = fixture.taskStates?.[call.seq];
      waiting.push({
        kind: "tool_call",
        seq: call.seq,
        tool: call.tool,
        callsite: call.callsite,
        state: state?.state ?? "claimed",
        parked: state?.parked ?? null,
        attempt: 1 + (fixture.retries?.[call.seq] ?? 0),
        taskId: fakeDigest(`${fixture.id}/${call.seq}`).slice(0, 26),
      });
    }
    for (const timer of model.timers) {
      if (timer.end || timer.timeoutOf !== null) continue;
      waiting.push({
        kind: "timer",
        seq: timer.seq,
        fireAt: new Date(timer.fireAt).toISOString(),
        callsite: timer.callsite,
      });
    }
    for (const wait of model.signals) {
      if (wait.end) continue;
      waiting.push({
        kind: "signal",
        name: wait.name,
        callsite: wait.callsite,
        timeoutAt: wait.timeoutAt === null ? null : new Date(wait.timeoutAt).toISOString(),
      });
    }
    run.waitingFor = waiting;
  }
  return run;
}

export function summarize(
  fixture: RunFixture,
  now: number,
): RunSummary & { tools: string[]; failedKinds: string[] } {
  const run = deriveRun(fixture, now);
  const model = buildRunModel(visibleEntries(fixture, now));
  const failedKinds = model.calls.flatMap((c) =>
    c.end?.outcome === "failed" ? [c.end.error.kind] : [],
  );
  const summary: RunSummary & { tools: string[]; failedKinds: string[] } = {
    id: run.id,
    status: run.status,
    script: run.script,
    toolset: run.toolset,
    labels: run.labels,
    startedAt: run.createdAt,
    lastActivityAt: run.updatedAt,
    historyLen: run.historyLen,
    toolCalls: model.calls.length,
    errors: failedKinds.length,
    quarantined: run.quarantine != null,
    tools: [...new Set(model.calls.map((c) => c.tool))],
    failedKinds,
  };
  if (run.endedAt) summary.endedAt = run.endedAt;
  if (run.parent) summary.parent = run.parent.run;
  if (run.error) summary.error = run.error.name;
  if (run.abort) summary.error = run.abort.kind;
  return summary;
}

function hasLabels(labels: Record<string, string>, wanted: readonly string[] | undefined): boolean {
  return (wanted ?? []).every((pair) => {
    const at = pair.indexOf(":");
    return at > 0 && labels[pair.slice(0, at)] === pair.slice(at + 1);
  });
}

function page<T>(items: T[], limit: number | undefined, cursor: string | undefined) {
  const offset = cursor ? Number(cursor) : 0;
  const size = Math.min(limit ?? 50, 500);
  const slice = items.slice(offset, offset + size);
  const next = offset + size < items.length ? String(offset + size) : undefined;
  return { slice, next };
}

/** Runs that have started by `now`, most recently started first. */
function started(world: World, now: number): RunFixture[] {
  return world.runs
    .filter((r) => visibleEntries(r, now).length > 0)
    .sort(
      (a, b) =>
        Date.parse(b.journal.entries[0]?.recordedAt ?? "") -
        Date.parse(a.journal.entries[0]?.recordedAt ?? ""),
    );
}

export function listRuns(world: World, query: ListRunsQuery, now: number): RunPage {
  const after = query.startedAfter ? Date.parse(query.startedAfter) : null;
  const before = query.startedBefore ? Date.parse(query.startedBefore) : null;
  const matching = started(world, now)
    .map((r) => summarize(r, now))
    .filter((s) => {
      const at = Date.parse(s.startedAt);
      if (query.status && s.status !== query.status) return false;
      if (!hasLabels(s.labels, query.label)) return false;
      if (query.tool && !s.tools.includes(query.tool)) return false;
      if (query.script && s.script !== query.script) return false;
      if (query.error && s.error !== query.error && !s.failedKinds.includes(query.error))
        return false;
      if (query.quarantined !== undefined && s.quarantined !== query.quarantined) return false;
      if (query.parent && s.parent !== query.parent) return false;
      if (after !== null && at < after) return false;
      if (before !== null && at >= before) return false;
      return true;
    });
  const { slice, next } = page(matching, query.limit, query.cursor);
  const runs: RunSummary[] = slice.map(({ tools: _t, failedKinds: _k, ...summary }) => summary);
  return next ? { runs, nextCursor: next } : { runs };
}

function emptyCounts(): RunCounts {
  return { pending: 0, active: 0, completed: 0, failed: 0, cancelled: 0, aborted: 0 };
}

export function listScripts(world: World, query: ListScriptsQuery, now: number): ScriptPage {
  const byDigest = new Map<string, ScriptSummary>();
  for (const fixture of started(world, now)) {
    const s = summarize(fixture, now);
    if (!hasLabels(s.labels, query.label)) continue;
    let summary = byDigest.get(s.script);
    if (!summary) {
      const headline = fixture.script.source.split("\n").find((l) => l.trim() !== "") ?? "";
      summary = {
        digest: s.script,
        headline,
        firstRunAt: s.startedAt,
        lastRunAt: s.startedAt,
        runs: emptyCounts(),
        tools: [],
        labels: {},
      };
      byDigest.set(s.script, summary);
    }
    summary.runs[s.status] += 1;
    if (s.startedAt < summary.firstRunAt) summary.firstRunAt = s.startedAt;
    if (s.startedAt > summary.lastRunAt) summary.lastRunAt = s.startedAt;
    for (const tool of s.tools) if (!summary.tools.includes(tool)) summary.tools.push(tool);
    const labels = summary.labels ?? {};
    for (const [k, v] of Object.entries(s.labels))
      labels[`${k}:${v}`] = (labels[`${k}:${v}`] ?? 0) + 1;
    summary.labels = labels;
  }
  const scripts = [...byDigest.values()]
    .filter((s) => !query.tool || s.tools.includes(query.tool))
    .map((s) => ({ ...s, tools: [...s.tools].sort() }))
    .sort((a, b) => b.lastRunAt.localeCompare(a.lastRunAt));
  const { slice, next } = page(scripts, query.limit, query.cursor);
  return next ? { scripts: slice, nextCursor: next } : { scripts: slice };
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const rank = Math.ceil((p / 100) * sorted.length) - 1;
  return sorted[Math.min(sorted.length - 1, Math.max(0, rank))] ?? 0;
}

export function toolStats(world: World, query: ToolStatsQuery, now: number): ToolStatsPage {
  const until = query.until ? Date.parse(query.until) : now;
  const since = query.since ? Date.parse(query.since) : until - 86_400_000;
  const byTool = new Map<
    string,
    { latencies: number[]; errors: Record<string, number>; retries: number }
  >();
  for (const fixture of world.runs) {
    if (query.toolset && fixture.toolset !== query.toolset) continue;
    const model = buildRunModel(visibleEntries(fixture, now));
    for (const call of model.calls) {
      if (!call.end || call.end.at < since || call.end.at >= until) continue;
      if (query.tool && call.tool !== query.tool) continue;
      let stats = byTool.get(call.tool);
      if (!stats) {
        stats = { latencies: [], errors: {}, retries: 0 };
        byTool.set(call.tool, stats);
      }
      stats.latencies.push(call.end.at - call.at);
      stats.retries += fixture.retries?.[call.seq] ?? 0;
      if (call.end.outcome === "failed") {
        const kind = call.end.error.kind;
        stats.errors[kind] = (stats.errors[kind] ?? 0) + 1;
      }
    }
  }
  const tools: ToolStats[] = [...byTool.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([tool, s]) => {
      const sorted = [...s.latencies].sort((a, b) => a - b);
      return {
        tool,
        calls: sorted.length,
        errors: Object.values(s.errors).reduce((a, b) => a + b, 0),
        errorKinds: s.errors,
        retries: s.retries,
        latencyMs: {
          p50: percentile(sorted, 50),
          p95: percentile(sorted, 95),
          p99: percentile(sorted, 99),
          max: sorted[sorted.length - 1] ?? 0,
        },
      };
    });
  return { since: new Date(since).toISOString(), until: new Date(until).toISOString(), tools };
}
