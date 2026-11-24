// What the server's read model answers, computed from the mocked world: run states and statuses,
// run summaries, fingerprints, workflows with their runs, and action statistics. Events recorded
// after the current time are not visible yet, so a run of the world can go on live.

import { buildRunModel, type RunModel } from "../model/run";
import type {
  ActionStats,
  ActionStatsPage,
  ActionStatsQuery,
  FacetValue,
  FailuresQuery,
  Fingerprint,
  FingerprintPage,
  HistoryEvent,
  LabelFacet,
  ListRunsQuery,
  ListWorkflowsQuery,
  Run,
  RunCounts,
  RunPage,
  RunStats,
  RunStatsQuery,
  RunStatus,
  RunSummary,
  RunWait,
  StatusCounts,
  WaitCounts,
  Waiting,
  WaitKind,
  WorkflowPage,
  WorkflowSummary,
} from "../model/types";
import type { RunFixture, World } from "./fixtures";
import { handleOf } from "./handles";
import { fakeDigest } from "./history";
import { compileQuery, type MockRun, parseDuration, scopeOf } from "./query";
import { latestVersion, type MockWorkflow, shortIdOf, visibleVersions } from "./workflows";

export function visibleEvents(fixture: RunFixture, now: number): HistoryEvent[] {
  const events = fixture.history.events;
  let end = 0;
  while (end < events.length && Date.parse(events[end]?.recordedAt ?? "") <= now) end += 1;
  return events.slice(0, end);
}

/** The harness actions: the harness's worker answers them. Rewind calls the others itself. */
export const HARNESS_ACTIONS = new Set(["ask_user"]);

type CallWaiting = Extract<Waiting, { kind: "call" }>;

/**
 * The state of a call that waits on the outside world: a harness action's (`ready`, `claimed`), or
 * any call's approval, deferred answer or parking; `null` for a call Rewind makes itself.
 */
function callWait(
  fixture: RunFixture,
  action: string,
  seq: number,
): Pick<CallWaiting, "state" | "parked"> | null {
  const given = fixture.callStates?.[seq];
  if (given && given.state !== "ready" && given.state !== "claimed")
    return { state: given.state, parked: given.parked ?? null };
  if (HARNESS_ACTIONS.has(action)) return { state: given?.state ?? "claimed", parked: null };
  return null;
}

/** What the run waits for on the outside world, as `Run.waitingFor` lists it. */
function waitsOf(fixture: RunFixture, model: RunModel): Waiting[] {
  if (fixture.quarantine) {
    // Quarantined at its last activity: the step it could not commit.
    const since = new Date(model.lastAt ?? 0).toISOString();
    return [{ kind: "quarantined", cause: fixture.quarantine.cause, since }];
  }
  const waits: Waiting[] = [];
  for (const call of model.calls) {
    if (call.end) continue;
    const wait = callWait(fixture, call.action, call.seq);
    if (!wait) continue;
    waits.push({
      kind: "call",
      seq: call.seq,
      action: call.action,
      args: call.args,
      callsite: call.callsite,
      state: wait.state,
      parked: wait.parked,
      delivery: 1 + (fixture.redeliveries?.[call.seq] ?? 0),
      callId: fakeDigest(`${fixture.id}/${call.seq}`).slice(0, 26),
    });
  }
  for (const timer of model.timers) {
    if (timer.end || timer.timeoutOf !== null) continue;
    waits.push({
      kind: "timer",
      seq: timer.seq,
      fireAt: new Date(timer.fireAt).toISOString(),
      callsite: timer.callsite,
    });
  }
  for (const wait of model.signals) {
    if (wait.end) continue;
    waits.push({
      kind: "signal",
      name: wait.name,
      callsite: wait.callsite,
      timeoutAt: wait.timeoutAt === null ? null : new Date(wait.timeoutAt).toISOString(),
    });
  }
  return waits;
}

/**
 * The run's status, as the partition owner computes it (archi/05, "Read model"): `queued` until a
 * step is committed; `running` while a step is in progress or a call Rewind makes itself is in
 * flight or backing off; `waiting` otherwise, on the outside world, until it ends. A quarantined
 * run is waiting, for `quarantined`.
 */
function statusOf(fixture: RunFixture, model: RunModel, waits: Waiting[]): RunStatus {
  const end = model.end;
  if (end) {
    if (end.outcome === "completed") return "completed";
    if (end.outcome === "aborted") return "aborted";
    return end.error.name === "CancelledError" ? "cancelled" : "failed";
  }
  if (model.entries.length <= 1) return "queued";
  if (fixture.quarantine) return "waiting";
  const own = model.calls.some((c) => !c.end && callWait(fixture, c.action, c.seq) === null);
  if (own) return "running";
  return waits.length > 0 ? "waiting" : "running";
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

export function deriveRun(world: World, fixture: RunFixture, now: number): Run {
  const events = visibleEvents(fixture, now);
  const model = buildRunModel(events);
  const waits = model.end ? [] : waitsOf(fixture, model);
  const status = statusOf(fixture, model, waits);
  const start = model.start?.event;
  const first = events[0]?.recordedAt ?? new Date(now).toISOString();
  const last = events[events.length - 1]?.recordedAt ?? first;
  const run: Run = {
    id: fixture.id,
    handle: handleOf(world, fixture),
    namespace: fixture.namespace,
    status,
    codeDigest: fixture.code.digest,
    actionSet: fixture.actionSet,
    labels: fixture.labels,
    mode: start?.mode ?? "live",
    epoch: start?.epoch ?? 1,
    input: start?.input ?? { digest: fakeDigest("{}"), bytes: 2, preview: "{}", value: {} },
    createdAt: first,
    updatedAt: last,
    expiresAt: start?.expiresAt ?? first,
    historyLen: events.length,
    traceId: fakeDigest(fixture.id).slice(0, 32),
    logs: model.logs.slice(-20).map((l) => ({
      idx: l.idx,
      at: new Date(l.at).toISOString(),
      level: l.level,
      message: l.message,
      callsite: l.callsite,
    })),
  };
  const handleOfId = (id: string) => {
    const other = world.runs.find((r) => r.id === id);
    return other ? handleOf(world, other) : id;
  };
  if (fixture.source)
    run.source = {
      run: fixture.source.run,
      handle: handleOfId(fixture.source.run),
      at: fixture.source.at ?? null,
    };
  if (fixture.workflow) run.workflow = fixture.workflow;
  if (fixture.startedBy)
    run.startedBy = { ...fixture.startedBy, handle: handleOfId(fixture.startedBy.run) };
  if (fixture.secrets) run.secrets = fixture.secrets;
  if (fixture.quarantine && !model.end) run.quarantine = fixture.quarantine;
  const redelivered = Object.entries(fixture.redeliveries ?? {})
    .map(([seq, n]) => ({ seq: Number(seq), deliveries: 1 + n }))
    .filter((d) => model.calls.some((c) => c.seq === d.seq));
  if (redelivered.length > 0) run.deliveries = redelivered;
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
      frame: at ? frame(fixture.code.source, at.line, at.column) : null,
      failedCall:
        failed && failed.end?.outcome === "failed"
          ? {
              seq: failed.seq,
              action: failed.action,
              args: failed.args,
              callsite: failed.callsite,
              error: failed.end.error,
            }
          : null,
    };
  }
  if (status === "waiting") run.waitingFor = waits;
  return run;
}

/**
 * An error's signature: its name and message with what varies (UUIDs, times, long hexadecimal
 * ids, double-quoted strings, numbers) replaced by placeholders, as the engine computes it.
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

/** A wait of `Run.waitingFor` as the run index flattens it: its kind, name and times. */
function runWaits(run: Run, model: RunModel): RunWait[] {
  if (run.status !== "waiting") return [];
  const iso = (ms: number) => new Date(ms).toISOString();
  return (run.waitingFor ?? []).map((w): RunWait => {
    switch (w.kind) {
      case "quarantined":
        return {
          kind: "quarantined",
          seq: null,
          name: null,
          since: run.updatedAt,
          until: null,
          callsite: null,
        };
      case "call": {
        const call = model.calls.find((c) => c.seq === w.seq);
        const kind: WaitKind =
          w.state === "approval" || w.state === "deferred" || w.state === "parked"
            ? w.state
            : "harness";
        return {
          kind,
          seq: w.seq,
          name: w.action,
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

/** What a workflow's runs know of its name now, for `workflow:` and the facets. */
function workflowOf(world: World, id: string | undefined): MockWorkflow | undefined {
  return id ? world.workflows.workflows.find((w) => w.id === id) : undefined;
}

export function summarize(world: World, fixture: RunFixture, now: number): MockRun {
  const run = deriveRun(world, fixture, now);
  const model = buildRunModel(visibleEvents(fixture, now));
  const failedKinds = model.calls.flatMap((c) =>
    c.end?.outcome === "failed" ? [c.end.error.kind] : [],
  );
  const workflow = workflowOf(world, fixture.workflow?.id);
  const summary: MockRun = {
    id: run.id,
    handle: run.handle,
    namespace: run.namespace,
    status: run.status,
    codeDigest: run.codeDigest,
    actionSet: run.actionSet,
    labels: run.labels,
    startedAt: run.createdAt,
    lastActivityAt: run.updatedAt,
    historyLen: run.historyLen,
    calls: model.calls.length,
    errors: failedKinds.length,
    quarantined: run.quarantine != null,
    actions: [...new Set(model.calls.map((c) => c.action))],
    failedKinds,
    mode: run.mode,
    headline: workflow
      ? (visibleVersions(workflow, {}).find((v) => v.codeDigest === fixture.code.digest)?.comment
          ?.title ?? "")
      : "",
    workflowName: workflow?.name ?? null,
  };
  if (run.endedAt) summary.endedAt = run.endedAt;
  if (run.source) summary.source = run.source.run;
  if (run.workflow) summary.workflow = run.workflow;
  if (run.startedBy) summary.startedBy = run.startedBy.run;
  const ending = run.error
    ? { name: run.error.name, message: run.error.message, stack: run.error.stack ?? "" }
    : run.abort
      ? { name: run.abort.kind, message: run.abort.message, stack: "" }
      : null;
  if (ending) {
    summary.error = ending.name;
    summary.errorMessage = ending.message;
    // A stack recorded before the rename says `script.ts`; a new one `workflow.ts`.
    const line = /(?:workflow|script)\.ts:(\d+):\d+/.exec(ending.stack)?.[1];
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
  actions: _a,
  failedKinds: _k,
  mode: _m,
  headline: _h,
  workflowName: _w,
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
    .filter((r) => visibleEvents(r, now).length > 0)
    .sort(
      (a, b) =>
        Date.parse(b.history.events[0]?.recordedAt ?? "") -
        Date.parse(a.history.events[0]?.recordedAt ?? ""),
    );
}

/** The runs that have started and match a query, as summaries; throws a `QueryError`. */
export function matchingRuns(world: World, q: string | undefined, now: number): MockRun[] {
  const predicate = compileQuery(q, now);
  return started(world, now)
    .map((r) => summarize(world, r, now))
    .filter(predicate);
}

/** The run a parameter names: its id or its handle. */
function sameRun(world: World, wanted: string, id: string | null | undefined): boolean {
  if (!id) return false;
  if (wanted === id) return true;
  const run = world.runs.find((r) => r.id === id);
  return run !== undefined && handleOf(world, run) === wanted;
}

export function listRuns(world: World, query: ListRunsQuery, now: number): RunPage {
  const after = query.startedAfter ? Date.parse(query.startedAfter) : null;
  const before = query.startedBefore ? Date.parse(query.startedBefore) : null;
  const matching = matchingRuns(world, query.q, now).filter((s) => {
    const at = Date.parse(s.startedAt);
    if (query.status && s.status !== query.status) return false;
    if (!hasLabels(s.labels, query.label)) return false;
    if (query.action && !s.actions.includes(query.action)) return false;
    if (query.error && s.error !== query.error && !s.failedKinds.includes(query.error as never))
      return false;
    if (query.quarantined !== undefined && s.quarantined !== query.quarantined) return false;
    if (query.source && !sameRun(world, query.source, s.source)) return false;
    if (
      query.workflow &&
      s.workflow?.id !== query.workflow &&
      s.workflowName !== query.workflow &&
      s.workflow?.name !== query.workflow
    )
      return false;
    if (query.startedBy && !sameRun(world, query.startedBy, s.startedBy)) return false;
    if (after !== null && at < after) return false;
    if (before !== null && at >= before) return false;
    return true;
  });
  if (query.sort === "activity") {
    matching.sort((a, b) => b.lastActivityAt.localeCompare(a.lastActivityAt));
  } else if (query.sort === "wakes") {
    const wakes = (r: MockRun) => (r.wakesAt ? Date.parse(r.wakesAt) : Number.POSITIVE_INFINITY);
    matching.sort((a, b) => wakes(a) - wakes(b) || b.startedAt.localeCompare(a.startedAt));
  } else if (query.sort === "oldest") {
    matching.sort((a, b) => a.startedAt.localeCompare(b.startedAt));
  } else if (query.sort === "duration") {
    // The longest first; the runs that have not ended last.
    const duration = (r: MockRun) =>
      r.endedAt ? Date.parse(r.endedAt) - Date.parse(r.startedAt) : Number.NEGATIVE_INFINITY;
    matching.sort((a, b) => duration(b) - duration(a) || b.startedAt.localeCompare(a.startedAt));
  } else if (query.sort === "calls") {
    matching.sort((a, b) => b.calls - a.calls || b.startedAt.localeCompare(a.startedAt));
  }
  const { slice, next } = page(matching, query.limit, query.cursor);
  const runs: RunSummary[] = slice.map(publicSummary);
  return next ? { runs, nextCursor: next } : { runs };
}

function statusCounts(): StatusCounts {
  return {
    queued: 0,
    running: 0,
    waiting: 0,
    completed: 0,
    failed: 0,
    cancelled: 0,
    aborted: 0,
  };
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

/** The window of a stats or failures query, and its intervals. */
function windowOf(query: { since?: string; until?: string; interval?: string }, now: number) {
  const since = query.since ? Date.parse(query.since) : null;
  const until = query.until ? Date.parse(query.until) : now;
  const width = query.interval ? parseDuration(query.interval) : null;
  const intervals =
    since !== null && width ? Math.min(500, Math.max(1, Math.ceil((until - since) / width))) : 0;
  const intervalOf = (at: number) =>
    since !== null && width ? Math.floor((at - since) / width) : -1;
  return { since, until, width, intervals, intervalOf };
}

export function runStats(world: World, query: RunStatsQuery, now: number): RunStats {
  // A scope only, as the counters keep them.
  scopeOf(query.q);
  const all = matchingRuns(world, query.q, now);
  const w = windowOf(query, now);
  const top = Math.min(50, query.top ?? 10);
  const inWindow = all.filter((r) => {
    const at = Date.parse(r.startedAt);
    return (w.since === null || at >= w.since) && at < w.until;
  });
  const statuses = statusCounts();
  for (const r of inWindow) statuses[r.status] += 1;
  const series = Array.from({ length: w.intervals }, (_, i) => ({
    at: new Date((w.since ?? 0) + i * (w.width ?? 0)).toISOString(),
    statuses: statusCounts(),
  }));
  for (const r of inWindow) {
    const interval = series[w.intervalOf(Date.parse(r.startedAt))];
    if (interval) interval.statuses[r.status] += 1;
  }
  const durations = inWindow
    .flatMap((r) => (r.endedAt ? [Date.parse(r.endedAt) - Date.parse(r.startedAt)] : []))
    .sort((a, b) => a - b);
  // The gauges of the runs waiting now, by what they wait for.
  const waiting: WaitCounts = {
    signal: 0,
    timer: 0,
    harness: 0,
    approval: 0,
    deferred: 0,
    parked: 0,
    quarantined: 0,
  };
  for (const r of all)
    if (r.status === "waiting")
      for (const kind of new Set((r.waitingFor ?? []).map((x) => x.kind))) waiting[kind] += 1;

  const byStatus = new Map<string, number>();
  const keys = new Map<string, Map<string, number>>();
  const actions = new Map<string, number>();
  const errors = new Map<string, number>();
  const waits = new Map<string, number>();
  const signals = new Map<string, number>();
  const namespaces = new Map<string, number>();
  const workflows = new Map<string, number>();
  for (const r of inWindow) {
    tally(byStatus, [r.status]);
    tally(namespaces, [r.namespace]);
    if (r.workflow) tally(workflows, [r.workflow.id]);
    for (const [k, v] of Object.entries(r.labels)) {
      let values = keys.get(k);
      if (!values) {
        values = new Map();
        keys.set(k, values);
      }
      tally(values, [v]);
    }
    tally(actions, r.actions);
    tally(errors, [...(r.error ? [r.error] : []), ...r.failedKinds]);
    if (r.status === "waiting") {
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
      workflows: topValues(workflows, top),
      labels,
      actions: topValues(actions, top),
      errors: topValues(errors, top),
      waiting: topValues(waits, top),
      signals: topValues(signals, top),
      namespaces: topValues(namespaces, top),
    },
  };
  if (w.since !== null) stats.since = new Date(w.since).toISOString();
  if (query.interval && w.since !== null) stats.interval = query.interval;
  return stats;
}

const FAILED: readonly RunStatus[] = ["failed", "cancelled", "aborted"];

export function fingerprints(world: World, query: FailuresQuery, now: number): FingerprintPage {
  scopeOf(query.q);
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
  const groups: Fingerprint[] = [...byFingerprint.entries()].map(([fp, list]) => {
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
    const series = Array.from({ length: w.intervals }, () => 0);
    for (const r of list) {
      const i = w.intervalOf(Date.parse(r.endedAt as string));
      if (i >= 0 && i < series.length) series[i] = (series[i] ?? 0) + 1;
    }
    const group: Fingerprint = {
      fingerprint: fp,
      signature: signature(first.error ?? "Error", first.errorMessage ?? ""),
      error: first.error ?? "Error",
      runs: list.length,
      workflows: new Set(list.map((r) => r.workflow?.id ?? r.codeDigest)).size,
      firstEndedAt: ends[0] as string,
      lastEndedAt: ends[ends.length - 1] as string,
      labels: topValues(labels, 5),
      latest: {
        id: latest?.id ?? first.id,
        handle: latest?.handle ?? first.handle,
        status: latest?.status ?? first.status,
        message: latest?.errorMessage ?? "",
      },
      line: line === undefined ? null : Number(line),
    };
    if (w.intervals > 0) group.series = series;
    return group;
  });
  groups.sort((a, b) => b.runs - a.runs || b.lastEndedAt.localeCompare(a.lastEndedAt));
  const limit = Math.min(500, query.limit ?? 50);
  const out: FingerprintPage = {
    until: new Date(w.until).toISOString(),
    runs: runs.length,
    totalFingerprints: groups.length,
    fingerprints: groups.slice(0, limit),
  };
  if (w.since !== null) out.since = new Date(w.since).toISOString();
  if (query.interval && w.since !== null) out.interval = query.interval;
  return out;
}

/** A workflow's runs of a window, from the counters: the active ones together. */
function emptyCounts(): RunCounts {
  return { active: 0, completed: 0, failed: 0, cancelled: 0, aborted: 0 };
}

export interface WorkflowListing {
  query: Omit<ListWorkflowsQuery, "limit"> & { limit?: number };
  /** The namespace `Rewind-Namespace` names: its workflows and the organization's. */
  namespace: string | null;
  /** The labels of the caller's grant: it sees the workflows with a version carrying them. */
  labels: Record<string, string>;
}

/**
 * The workflows the caller sees, each with its latest version and its runs of the window, from
 * the hourly counters (whole hours): the most recently run first, those without a run of the
 * window last. A caller limited to labels gets no counts, since counters carry none.
 */
export function listWorkflows(
  world: World,
  { query, namespace, labels }: WorkflowListing,
  now: number,
): WorkflowPage {
  const hour = 3_600_000;
  const scope = scopeOf(query.q);
  const since =
    Math.floor((query.since ? Date.parse(query.since) : now - 30 * 86_400_000) / hour) * hour;
  const until = Math.ceil((query.until ? Date.parse(query.until) : now) / hour) * hour;
  const counted = Object.keys(labels).length === 0;
  const runs = started(world, now)
    .filter((f) => scope === null || f.namespace === scope)
    .map((f) => summarize(world, f, now))
    .filter((s) => {
      const at = Date.parse(s.startedAt);
      return at >= since && at < until;
    });
  const rows = world.workflows.workflows
    .filter((w) => visibleVersions(w, labels).length > 0)
    .filter((w) => namespace === null || w.namespace === null || w.namespace === namespace)
    .filter((w) => !query.scope || (query.scope === "organization") === (w.namespace === null))
    .filter((w) => !query.code || w.versions.some((v) => v.codeDigest === query.code))
    .map((w): WorkflowSummary => {
      const latest = visibleVersions(w, labels)[0] ?? latestVersion(w);
      const { tests: _t, ...withoutTests } = latest;
      const summary: WorkflowSummary = {
        id: w.id,
        shortId: shortIdOf(w.id),
        ...(w.name ? { name: w.name } : {}),
        ...(w.description ? { description: w.description } : {}),
        ...(w.namespace ? { namespace: w.namespace } : {}),
        ...(w.promotedAt !== null ? { promotedAt: new Date(w.promotedAt).toISOString() } : {}),
        ...(w.source ? { source: w.source } : {}),
        createdAt: new Date(w.createdAt).toISOString(),
        latest: withoutTests,
        runs: emptyCounts(),
        actions: [],
      };
      if (!counted) return summary;
      const mine = runs.filter((r) => r.workflow?.id === w.id);
      const actions = new Set<string>();
      for (const r of mine) {
        const s = r.status;
        summary.runs[s === "queued" || s === "running" || s === "waiting" ? "active" : s] += 1;
        for (const a of r.actions) actions.add(a);
      }
      summary.actions = [...actions].sort();
      const starts = mine.map((r) => r.startedAt).sort();
      if (starts.length > 0) {
        summary.firstRunAt = starts[0] ?? null;
        summary.lastRunAt = starts[starts.length - 1] ?? null;
      }
      return summary;
    })
    .sort(
      (a, b) =>
        (b.lastRunAt ?? "").localeCompare(a.lastRunAt ?? "") ||
        b.createdAt.localeCompare(a.createdAt) ||
        a.id.localeCompare(b.id),
    );
  const from = query.after ? rows.findIndex((w) => w.id === query.after) + 1 : 0;
  const size = Math.min(500, Math.max(1, query.limit ?? 50));
  const slice = rows.slice(from, from + size);
  const last = slice[slice.length - 1];
  return from + size < rows.length && last
    ? { workflows: slice, next: last.id }
    : { workflows: slice };
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return 0;
  const rank = Math.ceil((p / 100) * sorted.length) - 1;
  return sorted[Math.min(sorted.length - 1, Math.max(0, rank))] ?? 0;
}

/** The latency, errors and redeliveries of each action over the calls of a window. */
export function actionStats(world: World, query: ActionStatsQuery, now: number): ActionStatsPage {
  const until = query.until ? Date.parse(query.until) : now;
  const since = query.since ? Date.parse(query.since) : until - 86_400_000;
  const byAction = new Map<
    string,
    { latencies: number[]; calls: number; errors: Record<string, number>; redeliveries: number }
  >();
  const namespace = scopeOf(query.q);
  for (const fixture of world.runs) {
    if (query.actionSet && fixture.actionSet !== query.actionSet) continue;
    if (namespace !== null && fixture.namespace !== namespace) continue;
    const model = buildRunModel(visibleEvents(fixture, now));
    for (const call of model.calls) {
      if (call.at < since || call.at >= until) continue;
      if (query.action && call.action !== query.action) continue;
      if (query.group && !call.action.startsWith(`${query.group}.`)) continue;
      let stats = byAction.get(call.action);
      if (!stats) {
        stats = { latencies: [], calls: 0, errors: {}, redeliveries: 0 };
        byAction.set(call.action, stats);
      }
      stats.calls += 1;
      stats.redeliveries += fixture.redeliveries?.[call.seq] ?? 0;
      if (!call.end) continue;
      stats.latencies.push(call.end.at - call.at);
      if (call.end.outcome === "failed") {
        const kind = call.end.error.kind;
        stats.errors[kind] = (stats.errors[kind] ?? 0) + 1;
      }
    }
  }
  const actions: ActionStats[] = [...byAction.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([action, s]) => {
      const sorted = [...s.latencies].sort((a, b) => a - b);
      return {
        action,
        calls: s.calls,
        errors: Object.values(s.errors).reduce((a, b) => a + b, 0),
        errorKinds: s.errors,
        redeliveries: s.redeliveries,
        latencyMs: {
          p50: percentile(sorted, 50),
          p95: percentile(sorted, 95),
          p99: percentile(sorted, 99),
          max: sorted[sorted.length - 1] ?? 0,
        },
      };
    });
  return { since: new Date(since).toISOString(), until: new Date(until).toISOString(), actions };
}
