// Workflows across runs, as a run's journal shows them (archi/03-execution.md, "Workflows across
// runs"): a script starts a run of a workflow with a call of `workflows.<name>.start`, whose
// result is `{ runId }`, signals a run with `workflows.<name>.signal` (`{ run, name, payload }`),
// and the end of a run it started comes back as the signal `rewind:ended:<run id>`, whose payload
// is `{ status, output | error | reason }`.
//
// And a workflow's versions, each with its suite (archi/05, "Suites and the gate"): the tests it
// passed, which it inherited from its parent, added, or updated, and those of the parent it
// retired, with their reasons.
import type {
  ExpectedCall,
  Payload,
  RetiredTest,
  ToolRule,
  ToolRules,
  Workflow,
} from "../api/types";
import type { RunModel, ToolCall } from "./run";

/** The prefix of the signals that carry the end of a run. */
export const ENDED_PREFIX = "rewind:ended:";

/** The run whose end a signal carries, for a signal named `rewind:ended:<run id>`. */
export function endedRunOf(signalName: string): string | null {
  return signalName.startsWith(ENDED_PREFIX) ? signalName.slice(ENDED_PREFIX.length) : null;
}

/** A call of a workflow's built-in tools, `workflows.<name>.start` or `workflows.<name>.signal`. */
export function workflowCallOf(tool: string): { workflow: string; op: "start" | "signal" } | null {
  const match = /^workflows\.(.+)\.(start|signal)$/.exec(tool);
  if (!match?.[1] || !match[2]) return null;
  return { workflow: match[1], op: match[2] as "start" | "signal" };
}

/** `triage@9f2c4e1a07b3`: a workflow's name and the first 12 hex digits of its version. */
export function workflowRef(workflow: { name: string; version: string }): string {
  return `${workflow.name}@${workflow.version.slice(0, 12)}`;
}

/** A payload's value: inline, or read from its preview when it is whole JSON. */
function payloadValue(payload: Payload): unknown {
  if ("value" in payload) return payload.value;
  try {
    return JSON.parse(payload.preview);
  } catch {
    return undefined;
  }
}

function field(payload: Payload, key: string): unknown {
  const value = payloadValue(payload);
  if (typeof value === "object" && value !== null && !Array.isArray(value)) {
    return (value as Record<string, unknown>)[key];
  }
  // A preview cut short: payloads are canonical JSON, a string field reads as `"key":"…"`.
  const match = new RegExp(`"${key}":"((?:[^"\\\\]|\\\\.)*)"`).exec(payload.preview);
  if (!match) return undefined;
  try {
    return JSON.parse(`"${match[1]}"`);
  } catch {
    return undefined;
  }
}

/** Whether a `workflows.<name>.start` call started its run detached. */
function isDetached(args: Payload): boolean {
  const value = payloadValue(args);
  if (typeof value === "object" && value !== null) {
    return (value as Record<string, unknown>).detached === true;
  }
  // Sorted keys: `detached` comes first when it is there.
  return args.preview.startsWith('{"detached":true');
}

/** The run a `workflows.<name>.start` call started, once it answered. */
export function startedRunOf(call: ToolCall): string | null {
  if (workflowCallOf(call.tool)?.op !== "start" || call.end?.outcome !== "ok") return null;
  const runId = field(call.end.result, "runId");
  return typeof runId === "string" ? runId : null;
}

/** The run a `workflows.<name>.signal` call sends its signal to. */
export function signalTargetOf(args: Payload): string | null {
  const run = field(args, "run");
  return typeof run === "string" ? run : null;
}

/** The signal a `workflows.<name>.signal` call sends. */
export function signalNameOf(args: Payload): string | null {
  const name = field(args, "name");
  return typeof name === "string" ? name : null;
}

/**
 * How the script wrote a call: `triage.start(…)`, `run.signal("approval", …)`, or the tool's
 * own name.
 */
export function callName(tool: string, args?: Payload): string {
  const call = workflowCallOf(tool);
  if (!call) return tool;
  if (call.op === "start") return `${call.workflow}.start(…)`;
  const name = args ? signalNameOf(args) : null;
  return name === null ? "run.signal(…)" : `run.signal(${JSON.stringify(name)}, …)`;
}

export type EndStatus = "completed" | "failed" | "cancelled" | "aborted";

/** How a run ended, from the payload of its `rewind:ended:` signal. */
export interface RunEndInfo {
  status: EndStatus | null;
  /** The error (`Name: message`) or the abort's reason (`kind: message`). */
  message: string | null;
}

function described(value: unknown, first: string): string | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  const message = typeof v.message === "string" ? v.message : "";
  const head = typeof v[first] === "string" ? (v[first] as string) : null;
  if (head === null) return message || null;
  return message ? `${head}: ${message}` : head;
}

export function readRunEnd(payload: Payload): RunEndInfo {
  const value = payloadValue(payload);
  if (typeof value === "object" && value !== null && !Array.isArray(value)) {
    const v = value as Record<string, unknown>;
    const status =
      v.status === "completed" ||
      v.status === "failed" ||
      v.status === "cancelled" ||
      v.status === "aborted"
        ? v.status
        : null;
    const message =
      status === "aborted"
        ? described(v.reason, "kind")
        : status === "failed" || status === "cancelled"
          ? described(v.error, "name")
          : null;
    return { status, message };
  }
  // A preview cut short: its keys are sorted, the first one says how the run ended.
  const preview = payload.preview;
  if (preview.startsWith('{"output":')) return { status: "completed", message: null };
  if (preview.startsWith('{"reason":')) return { status: "aborted", message: null };
  if (preview.startsWith('{"error":')) {
    const cancelled = preview.includes('"name":"CancelledError"');
    return { status: cancelled ? "cancelled" : "failed", message: null };
  }
  return { status: null, message: null };
}

/** A run this run started: the call that started it and, once it came back, its end. */
export interface StartedRun {
  runId: string;
  workflow: string;
  call: ToolCall;
  detached: boolean;
  end: (RunEndInfo & { idx: number; at: number }) | null;
}

/** The runs a run started, in the order it started them, from its journal. */
export function startedRuns(model: RunModel): StartedRun[] {
  const ends = new Map<string, RunEndInfo & { idx: number; at: number }>();
  for (const signal of model.received) {
    const runId = endedRunOf(signal.name);
    if (runId !== null && !ends.has(runId))
      ends.set(runId, { ...readRunEnd(signal.payload), idx: signal.idx, at: signal.at });
  }
  const started: StartedRun[] = [];
  for (const call of model.calls) {
    const workflow = workflowCallOf(call.tool);
    const runId = startedRunOf(call);
    if (workflow === null || runId === null) continue;
    started.push({
      runId,
      workflow: workflow.workflow,
      call,
      detached: isDetached(call.args),
      end: ends.get(runId) ?? null,
    });
  }
  return started;
}

// ---------------------------------------------------------------------------------------------
// Versions and suites

/**
 * The version a reference names among a workflow's versions: its digest, or the start of it (its
 * first 12 digits, as `name@<version>` writes it).
 */
export function findVersion(versions: readonly Workflow[], ref: string): Workflow | null {
  const exact = versions.find((v) => v.version === ref);
  if (exact) return exact;
  if (ref.length < 12) return null;
  const matches = versions.filter((v) => v.version.startsWith(ref));
  return matches.length === 1 ? (matches[0] ?? null) : null;
}

/**
 * Where a test of a version's suite comes from: kept from its parent as it was (`inherited`), new
 * (`added`), or replacing the parent's test of the same name (`updated`). `kept` is a test of a
 * version whose parent is not among the versions read: it was inherited or added.
 */
export type SuiteKind = "inherited" | "added" | "updated" | "kept";

export interface SuiteEntry {
  name: string;
  digest: string;
  kind: SuiteKind;
  /** For an updated test: the parent's test it replaced, and why (`null` when none was given). */
  from: string | null;
  reason: string | null;
  /** The run a recorded test was recorded from, read from its name `recorded <run id>`. */
  recordedFrom: string | null;
}

export interface Suite {
  /** The tests of the version, in its order. */
  tests: SuiteEntry[];
  /** The parent's tests it does not keep, with their reasons. */
  retired: RetiredTest[];
  /** The version it derives from; `null` for a first version, or one not among the versions. */
  parent: Workflow | null;
  /** It has a parent that is not among the versions read. */
  parentMissing: boolean;
}

const RECORDED = /^recorded ([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/i;

/**
 * The run a test was recorded from, from its name: recording names a test `recorded <run id>`
 * (archi/05, "Tests recorded from runs"). The test document's `origin` says it for sure.
 */
export function recordedRunOf(name: string): string | null {
  return RECORDED.exec(name)?.[1] ?? null;
}

/** A version's suite, read against its parent's when the parent is among `versions`. */
export function suiteOf(version: Workflow, versions: readonly Workflow[]): Suite {
  const parent = version.parent ? findVersion(versions, version.parent) : null;
  const parentMissing = Boolean(version.parent) && parent === null;
  const before = new Map((parent?.tests ?? []).map((t) => [t.name, t.digest]));
  const updated = new Map((version.updated ?? []).map((u) => [u.name, u]));
  const tests = (version.tests ?? []).map((test): SuiteEntry => {
    const recordedFrom = recordedRunOf(test.name);
    const update = updated.get(test.name);
    if (update) {
      return { ...test, kind: "updated", from: update.from, reason: update.reason, recordedFrom };
    }
    const entry = { ...test, from: null, reason: null, recordedFrom };
    if (!version.parent) return { ...entry, kind: "added" };
    if (parent === null) return { ...entry, kind: "kept" };
    const old = before.get(test.name);
    if (old === undefined) return { ...entry, kind: "added" };
    // Replaced without a reason in `updated`: shown as an update all the same.
    return old === test.digest
      ? { ...entry, kind: "inherited" }
      : { ...entry, kind: "updated", from: old };
  });
  return { tests, retired: version.retired ?? [], parent, parentMissing };
}

/** How many of each kind a suite has. */
export function suiteCounts(suite: Suite): Record<SuiteKind | "retired", number> {
  const counts = { inherited: 0, added: 0, updated: 0, kept: 0, retired: suite.retired.length };
  for (const t of suite.tests) counts[t.kind] += 1;
  return counts;
}

/** A tool's rules, one or several: always a list. */
export function rulesOf(rules: ToolRules): ToolRule[] {
  return Array.isArray(rules) ? rules : [rules];
}

/** How many calls a rule answers. */
export function ruleTimes(times: number | null | undefined): string {
  if (times === null || times === undefined) return "every call";
  return times === 1 ? "1 call" : `${times} calls`;
}

/** How many calls an expectation asks for. */
export function expectedTimes(call: ExpectedCall): string {
  const times = call.times;
  if (times === null || times === undefined) return "at least once";
  if (times === 0) return "never";
  return times === 1 ? "exactly once" : `exactly ${times} times`;
}

/** When a test sends a signal: once the run is blocked (the default), or after a duration. */
export function signalWhen(when: string | null | undefined): string {
  return !when || when === "blocked" ? "once the run is blocked" : `after ${when}`;
}

/** What a cassette holds, in short: how it is matched, its calls by tool, its signals. */
export interface CassetteSummary {
  matching: string | null;
  calls: number;
  byTool: { tool: string; count: number }[];
  signals: { name: string; at: number | null }[];
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

/** A cassette document (`{ v, matching, calls, signals }`), summed up. */
export function summarizeCassette(cassette: unknown): CassetteSummary | null {
  const doc = asRecord(cassette);
  if (!doc) return null;
  const calls = Array.isArray(doc.calls) ? doc.calls : [];
  const counts = new Map<string, number>();
  for (const call of calls) {
    const tool = asRecord(call)?.tool;
    const name = typeof tool === "string" ? tool : "?";
    counts.set(name, (counts.get(name) ?? 0) + 1);
  }
  const signals = (Array.isArray(doc.signals) ? doc.signals : []).map((s) => {
    const signal = asRecord(s);
    const at = signal?.elapsed_ms;
    return {
      name: typeof signal?.name === "string" ? signal.name : "?",
      at: typeof at === "number" ? at : null,
    };
  });
  return {
    matching: typeof doc.matching === "string" ? doc.matching : null,
    calls: calls.length,
    byTool: [...counts]
      .map(([tool, count]) => ({ tool, count }))
      .sort((a, b) => b.count - a.count || a.tool.localeCompare(b.tool)),
    signals,
  };
}
