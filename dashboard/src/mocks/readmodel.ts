// What the server's read model answers, computed from the mocked world: run states, run
// summaries, scripts and tool statistics. Entries recorded after the current time are not
// visible yet, so a run of the world can go on live.
import type {
  Entry,
  FacetValue,
  FailureGroup,
  FailureGroupPage,
  FailuresQuery,
  LabelFacet,
  ListRunsQuery,
  ListScriptsQuery,
  Run,
  RunCounts,
  RunPage,
  RunStats,
  RunStatsQuery,
  RunStatus,
  RunSummary,
  RunWait,
  ScriptPage,
  ScriptSummary,
  StatusCounts,
  ToolStats,
  ToolStatsPage,
  ToolStatsQuery,
  WaitCounts,
  Waiting,
  WaitKind,
} from "../api/types";
import { buildRunModel, type RunModel } from "../model/run";
import type { RunFixture, World } from "./fixtures";
import { fakeDigest } from "./journal";
import { compileQuery, type MockRun, parseDuration } from "./query";

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
    namespace: fixture.namespace,
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
  if (fixture.workflow) run.workflow = fixture.workflow;
  if (fixture.startedBy) run.startedBy = fixture.startedBy;
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
        args: call.args,
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

/** Host tools: the harness's worker answers them. The others, Rewind calls itself. */
const HOST_TOOLS = new Set(["ask_user"]);

/**
 * An error's signature: its name and message with what varies (UUIDs, times, long hexadecimal
 * ids, double-quoted strings, numbers) replaced by placeholders, as the projector computes it.
 */
export function signature(name: string, message: string): string {
  const normalized = message
    .replace(/"(?:[^"\\]|\\.)*"/g, "<str>")
    .replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, "<uuid>")
    .replace(/\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:\d{2})?/g, "<time>")
    .replace(/\b[0-9a-f]{8,}\b/gi, "<id>")
    .replace(/\d+(?:\.\d+)?/g, "<n>")
    .replace(/\s+/g, " ")
    .trim();
  return `${name}: ${normalized}`.slice(0, 200);
}

export function fingerprint(sig: string): string {
  return fakeDigest(sig).slice(0, 16);
}

function waitKind(w: Extract<Waiting, { kind: "tool_call" }>): WaitKind {
  if (w.state === "approval") return "approval";
  if (w.state === "parked") return "parked";
  if (w.state === "deferred") return "deferred";
  return HOST_TOOLS.has(w.tool) ? "host" : "call";
}

function runWaits(run: Run, model: RunModel): RunWait[] {
  if (run.status !== "active") return [];
  const iso = (ms: number) => new Date(ms).toISOString();
  return (run.waitingFor ?? []).map((w): RunWait => {
    switch (w.kind) {
      case "tool_call": {
        const call = model.calls.find((c) => c.seq === w.seq);
        return {
          kind: waitKind(w),
          seq: w.seq,
          name: w.tool,
          since: iso(call?.at ?? Date.parse(run.updatedAt)),
          until: null,
          callsite: w.callsite ?? null,
        };
      }
      case "timer": {
        const timer = model.timers.find((t) => t.seq === w.seq);
        return {
          kind: "timer",
          seq: w.seq,
          name: null,
          since: iso(timer?.at ?? Date.parse(run.updatedAt)),
          until: w.fireAt,
          callsite: w.callsite ?? null,
        };
      }
    }
    const wait = model.signals.find((s) => s.name === w.name && s.end === null);
    return {
      kind: "signal",
      seq: null,
      name: w.name,
      since: iso(wait?.at ?? Date.parse(run.updatedAt)),
      until: w.timeoutAt ?? null,
      callsite: w.callsite ?? null,
    };
  });
}

export function summarize(fixture: RunFixture, now: number): MockRun {
  const run = deriveRun(fixture, now);
  const model = buildRunModel(visibleEntries(fixture, now));
  const failedKinds = model.calls.flatMap((c) =>
    c.end?.outcome === "failed" ? [c.end.error.kind] : [],
  );
  const summary: MockRun = {
    id: run.id,
    namespace: run.namespace,
    status: run.status,
    script: run.script,
    headline:
      fixture.script.source
        .split("\n")
        .find((line) => line.trim() !== "")
        ?.trim() ?? "",
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
    mode: run.mode,
  };
  if (run.endedAt) summary.endedAt = run.endedAt;
  if (run.parent) summary.parent = run.parent.run;
  if (run.workflow) summary.workflow = run.workflow.name;
  if (run.startedBy) summary.startedBy = run.startedBy.run;
  const ending = run.error
    ? { name: run.error.name, message: run.error.message, stack: run.error.stack ?? "" }
    : run.abort
      ? { name: run.abort.kind, message: run.abort.message, stack: "" }
      : null;
  if (ending) {
    summary.error = ending.name;
    summary.errorMessage = ending.message;
    const line = /script\.ts:(\d+):\d+/.exec(ending.stack)?.[1];
    if (line) summary.errorLine = Number(line);
    else if (run.error?.location) summary.errorLine = run.error.location.line;
    summary.fingerprint = fingerprint(signature(ending.name, ending.message));
  }
  const waits = runWaits(run, model);
  if (waits.length > 0) {
    summary.waitingFor = waits;
    const wakes = waits.flatMap((w) => (w.until ? [Date.parse(w.until)] : []));
    if (wakes.length > 0) summary.wakesAt = new Date(Math.min(...wakes)).toISOString();
  }
  return summary;
}

/** The fields of a summary the API serves, without what only the mock keeps. */
export function publicSummary({
  tools: _t,
  failedKinds: _k,
  mode: _m,
  ...summary
}: MockRun): RunSummary {
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

/** The runs that have started and match a query, as summaries; throws a `QueryError`. */
export function matchingRuns(world: World, q: string | undefined, now: number): MockRun[] {
  const predicate = compileQuery(q, now);
  return started(world, now)
    .map((r) => summarize(r, now))
    .filter(predicate);
}

export function listRuns(world: World, query: ListRunsQuery, now: number): RunPage {
  const after = query.startedAfter ? Date.parse(query.startedAfter) : null;
  const before = query.startedBefore ? Date.parse(query.startedBefore) : null;
  const matching = matchingRuns(world, query.q, now).filter((s) => {
    const at = Date.parse(s.startedAt);
    if (query.status && s.status !== query.status) return false;
    if (!hasLabels(s.labels, query.label)) return false;
    if (query.tool && !s.tools.includes(query.tool)) return false;
    if (query.script && s.script !== query.script) return false;
    if (query.error && s.error !== query.error && !s.failedKinds.includes(query.error as never))
      return false;
    if (query.quarantined !== undefined && s.quarantined !== query.quarantined) return false;
    if (query.parent && s.parent !== query.parent) return false;
    if (query.workflow && s.workflow !== query.workflow) return false;
    if (query.startedBy && s.startedBy !== query.startedBy) return false;
    if (after !== null && at < after) return false;
    if (before !== null && at >= before) return false;
    return true;
  });
  if (query.sort === "activity") {
    matching.sort((a, b) => b.lastActivityAt.localeCompare(a.lastActivityAt));
  } else if (query.sort === "wakes") {
    const wakes = (r: MockRun) => (r.wakesAt ? Date.parse(r.wakesAt) : Number.POSITIVE_INFINITY);
    matching.sort((a, b) => wakes(a) - wakes(b) || b.startedAt.localeCompare(a.startedAt));
  }
  const { slice, next } = page(matching, query.limit, query.cursor);
  const runs: RunSummary[] = slice.map(publicSummary);
  return next ? { runs, nextCursor: next } : { runs };
}

function statusCounts(): StatusCounts {
  return { pending: 0, active: 0, completed: 0, failed: 0, cancelled: 0, aborted: 0 };
}

/** The most frequent values first, then by value; at most `top`. */
function topValues(counts: Map<string, number>, top: number): FacetValue[] {
  return [...counts.entries()]
    .sort(([a, x], [b, y]) => y - x || a.localeCompare(b))
    .slice(0, top)
    .map(([value, runs]) => ({ value, runs }));
}

function tally(counts: Map<string, number>, values: Iterable<string>) {
  for (const value of new Set(values)) counts.set(value, (counts.get(value) ?? 0) + 1);
}

/** The window of a stats or failures query, and its buckets. */
function windowOf(query: { since?: string; until?: string; bucket?: string }, now: number) {
  const since = query.since ? Date.parse(query.since) : null;
  const until = query.until ? Date.parse(query.until) : now;
  const width = query.bucket ? parseDuration(query.bucket) : null;
  const buckets =
    since !== null && width ? Math.min(500, Math.max(1, Math.ceil((until - since) / width))) : 0;
  const bucketOf = (at: number) =>
    since !== null && width ? Math.floor((at - since) / width) : -1;
  return { since, until, width, buckets, bucketOf };
}

export function runStats(world: World, query: RunStatsQuery, now: number): RunStats {
  const all = matchingRuns(world, query.q, now);
  const w = windowOf(query, now);
  const top = Math.min(50, query.top ?? 10);
  const inWindow = all.filter((r) => {
    const at = Date.parse(r.startedAt);
    return (w.since === null || at >= w.since) && at < w.until;
  });
  const statuses = statusCounts();
  for (const r of inWindow) statuses[r.status] += 1;
  const series = Array.from({ length: w.buckets }, (_, i) => ({
    at: new Date((w.since ?? 0) + i * (w.width ?? 0)).toISOString(),
    statuses: statusCounts(),
  }));
  for (const r of inWindow) {
    const bucket = series[w.bucketOf(Date.parse(r.startedAt))];
    if (bucket) bucket.statuses[r.status] += 1;
  }
  const durations = inWindow
    .flatMap((r) => (r.endedAt ? [Date.parse(r.endedAt) - Date.parse(r.startedAt)] : []))
    .sort((a, b) => a - b);
  const waiting: WaitCounts = {
    signal: 0,
    timer: 0,
    call: 0,
    host: 0,
    approval: 0,
    parked: 0,
    deferred: 0,
  };
  for (const r of all)
    if (r.status === "active")
      for (const kind of new Set((r.waitingFor ?? []).map((x) => x.kind))) waiting[kind] += 1;

  const byStatus = new Map<string, number>();
  const keys = new Map<string, Map<string, number>>();
  const tools = new Map<string, number>();
  const errors = new Map<string, number>();
  const scripts = new Map<string, number>();
  const waits = new Map<string, number>();
  const signals = new Map<string, number>();
  const namespaces = new Map<string, number>();
  for (const r of inWindow) {
    tally(byStatus, [r.status]);
    tally(namespaces, [r.namespace]);
    for (const [k, v] of Object.entries(r.labels)) {
      let values = keys.get(k);
      if (!values) {
        values = new Map();
        keys.set(k, values);
      }
      tally(values, [v]);
    }
    tally(tools, r.tools);
    tally(errors, [...(r.error ? [r.error] : []), ...r.failedKinds]);
    tally(scripts, [r.script]);
    if (r.status === "active") {
      tally(
        waits,
        (r.waitingFor ?? []).map((x) => x.kind),
      );
      tally(
        signals,
        (r.waitingFor ?? []).flatMap((x) => (x.kind === "signal" && x.name ? [x.name] : [])),
      );
    }
  }
  const labels: LabelFacet[] = [...keys.entries()]
    .map(([key, values]) => ({
      key,
      runs: [...values.values()].reduce((a, b) => a + b, 0),
      values: topValues(values, top),
    }))
    .sort((a, b) => b.runs - a.runs || a.key.localeCompare(b.key))
    .slice(0, top);

  const stats: RunStats = {
    until: new Date(w.until).toISOString(),
    runs: inWindow.length,
    statuses,
    series,
    durations:
      durations.length === 0
        ? null
        : {
            count: durations.length,
            p50: percentile(durations, 50),
            p95: percentile(durations, 95),
            p99: percentile(durations, 99),
            max: durations[durations.length - 1] ?? 0,
          },
    waiting,
    quarantined: all.filter((r) => r.quarantined).length,
    facets: {
      statuses: topValues(byStatus, top),
      labels,
      tools: topValues(tools, top),
      errors: topValues(errors, top),
      scripts: topValues(scripts, top),
      waiting: topValues(waits, top),
      signals: topValues(signals, top),
      namespaces: topValues(namespaces, top),
    },
  };
  if (w.since !== null) stats.since = new Date(w.since).toISOString();
  if (query.bucket && w.since !== null) stats.bucket = query.bucket;
  return stats;
}

const FAILED: readonly RunStatus[] = ["failed", "cancelled", "aborted"];

export function failureGroups(world: World, query: FailuresQuery, now: number): FailureGroupPage {
  const w = windowOf(query, now);
  const runs = matchingRuns(world, query.q, now).filter((r) => {
    if (!FAILED.includes(r.status) || !r.endedAt || !r.fingerprint) return false;
    const at = Date.parse(r.endedAt);
    return (w.since === null || at >= w.since) && at < w.until;
  });
  const byFingerprint = new Map<string, MockRun[]>();
  for (const r of runs) {
    const fp = r.fingerprint as string;
    const list = byFingerprint.get(fp);
    if (list) list.push(r);
    else byFingerprint.set(fp, [r]);
  }
  const groups: FailureGroup[] = [...byFingerprint.entries()].map(([fp, list]) => {
    const ends = list.map((r) => r.endedAt as string).sort();
    const latest = [...list].sort((a, b) => (b.endedAt ?? "").localeCompare(a.endedAt ?? ""))[0];
    const first = list[0] as MockRun;
    const lines = new Map<string, number>();
    tally(
      lines,
      list.flatMap((r) => (r.errorLine != null ? [String(r.errorLine)] : [])),
    );
    const line = topValues(lines, 1)[0]?.value;
    const labels = new Map<string, number>();
    for (const r of list)
      tally(
        labels,
        Object.entries(r.labels).map(([k, v]) => `${k}:${v}`),
      );
    const series = Array.from({ length: w.buckets }, () => 0);
    for (const r of list) {
      const i = w.bucketOf(Date.parse(r.endedAt as string));
      if (i >= 0 && i < series.length) series[i] = (series[i] ?? 0) + 1;
    }
    const group: FailureGroup = {
      fingerprint: fp,
      signature: signature(first.error ?? "Error", first.errorMessage ?? ""),
      error: first.error ?? "Error",
      runs: list.length,
      scripts: new Set(list.map((r) => r.script)).size,
      firstEndedAt: ends[0] as string,
      lastEndedAt: ends[ends.length - 1] as string,
      labels: topValues(labels, 5),
      latest: {
        id: latest?.id ?? first.id,
        status: latest?.status ?? first.status,
        message: latest?.errorMessage ?? "",
      },
      line: line === undefined ? null : Number(line),
    };
    if (w.buckets > 0) group.series = series;
    return group;
  });
  groups.sort((a, b) => b.runs - a.runs || b.lastEndedAt.localeCompare(a.lastEndedAt));
  const limit = Math.min(500, query.limit ?? 50);
  const page: FailureGroupPage = {
    until: new Date(w.until).toISOString(),
    runs: runs.length,
    totalGroups: groups.length,
    groups: groups.slice(0, limit),
  };
  if (w.since !== null) page.since = new Date(w.since).toISOString();
  if (query.bucket && w.since !== null) page.bucket = query.bucket;
  return page;
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
