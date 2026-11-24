// Workflows across runs, as a run's history shows them (archi/03-execution.md, "Workflows across
// runs"): code starts a run of a workflow with a call of `workflows.<name>.start`, whose result is
// `{ runId }`, signals a run with `workflows.<name>.signal` (`{ run, name, payload }`), and the end
// of a run it started comes back as the signal `rewind:ended:<run id>`, whose payload is
// `{ status, output | error | reason }`.
//
// And a workflow (archi/01, "Workflows"): its id, its name or what shows for it without one, its
// versions numbered from 1, each with the suite it passed: the tests it inherited from its base,
// added, or replaced, and those of its base it retired, with their reasons.

import type { Call, RunModel } from "./run";
import type {
  ActionRule,
  ActionRules,
  CodeComment,
  ExpectedCall,
  Payload,
  RetiredTest,
  RunWorkflow,
  WorkflowVersion,
} from "./types";

/** The prefix of the signals that carry the end of a run. */
export const ENDED_PREFIX = "rewind:ended:";

/** The run whose end a signal carries, for a signal named `rewind:ended:<run id>`. */
export function endedRunOf(signalName: string): string | null {
  return signalName.startsWith(ENDED_PREFIX) ? signalName.slice(ENDED_PREFIX.length) : null;
}

/** A call of a workflow's built-in actions, `workflows.<name>.start` or `workflows.<name>.signal`. */
export function workflowCallOf(
  action: string,
): { workflow: string; op: "start" | "signal" } | null {
  const match = /^workflows\.(.+)\.(start|signal)$/.exec(action);
  if (!match?.[1] || !match[2]) return null;
  return { workflow: match[1], op: match[2] as "start" | "signal" };
}

/** What a workflow shows as, with its id: its name, else its code's first comment line, else its short id. */
export interface WorkflowNaming {
  id: string;
  name?: string | null;
  shortId?: string;
  /** The comment at the top of its latest version's code. */
  comment?: CodeComment | null;
}

/** `wf-7k2m9q`: how a workflow without a name or a comment shows (`wf_` and its first 6 characters). */
export function shortIdOf(id: string): string {
  return `wf-${id.startsWith("wf_") ? id.slice(3, 9) : id.slice(0, 6)}`;
}

/** How a workflow shows (archi/05, "Names, not ids"). */
export function workflowLabel(workflow: WorkflowNaming): string {
  return workflow.name || workflow.comment?.title || workflow.shortId || shortIdOf(workflow.id);
}

/** Whether a workflow shows by its name, rather than its comment or its short id. */
export function isNamed(workflow: Pick<WorkflowNaming, "name">): boolean {
  return Boolean(workflow.name);
}

/** `triage v12`: a version by its workflow and its number. */
export function versionLabel(workflow: string, version: number): string {
  return `${workflow} v${version}`;
}

/**
 * `triage v12`: the workflow a run is a run of, and its version. Its name when the run was
 * created, else `label` (what the workflow shows as now, when known), else its short id.
 */
export function workflowRef(workflow: RunWorkflow, label?: string): string {
  return versionLabel(workflow.name || label || shortIdOf(workflow.id), workflow.version);
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
export function startedRunOf(call: Call): string | null {
  if (workflowCallOf(call.action)?.op !== "start" || call.end?.outcome !== "ok") return null;
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
 * How the code wrote a call: `triage.start(…)`, `run.signal("approval", …)`, or the action's
 * own name.
 */
export function callName(action: string, args?: Payload): string {
  const call = workflowCallOf(action);
  if (!call) return action;
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
  call: Call;
  detached: boolean;
  end: (RunEndInfo & { idx: number; at: number }) | null;
}

/** The runs a run started, in the order it started them, from its history. */
export function startedRuns(model: RunModel): StartedRun[] {
  const ends = new Map<string, RunEndInfo & { idx: number; at: number }>();
  for (const signal of model.received) {
    const runId = endedRunOf(signal.name);
    if (runId !== null && !ends.has(runId))
      ends.set(runId, { ...readRunEnd(signal.payload), idx: signal.idx, at: signal.at });
  }
  const started: StartedRun[] = [];
  for (const call of model.calls) {
    const workflow = workflowCallOf(call.action);
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
 * The version a reference names among a workflow's versions: its number (`12`, `v12`), or its
 * digest or the start of it (its first 12 digits, as `triage@<digest>` writes it).
 */
export function findVersion(
  versions: readonly WorkflowVersion[],
  ref: string,
): WorkflowVersion | null {
  // A number is short; a digest, or its start, has 12 digits at least.
  const number = ref.length < 12 ? /^v?(\d+)$/.exec(ref)?.[1] : undefined;
  if (number !== undefined) return versions.find((v) => v.version === Number(number)) ?? null;
  const exact = versions.find((v) => v.digest === ref);
  if (exact) return exact;
  if (ref.length < 12) return null;
  const matches = versions.filter((v) => v.digest.startsWith(ref));
  return matches.length === 1 ? (matches[0] ?? null) : null;
}

/**
 * Where a test of a version's suite comes from: kept from its base as it was (`inherited`), new
 * (`added`), or replacing the base's test of the same name (`replaced`). `kept` is a test of a
 * version whose base is not among the versions read: it was inherited or added.
 */
export type SuiteKind = "inherited" | "added" | "replaced" | "kept";

export interface SuitePlace {
  name: string;
  digest: string;
  kind: SuiteKind;
  /** For a replaced test: the base's test it replaced, and why (`null` when none was given). */
  from: string | null;
  reason: string | null;
  /** The run a recorded test was recorded from, read from its name `recorded <run id>`. */
  recordedFrom: string | null;
}

export interface Suite {
  /** The tests of the version, in its order. */
  tests: SuitePlace[];
  /** The base's tests it does not keep, with their reasons. */
  retired: RetiredTest[];
  /** The version it derives from; `null` for a first version, or one not among the versions. */
  base: WorkflowVersion | null;
  /** It has a base that is not among the versions read. */
  baseMissing: boolean;
}

const RECORDED = /^recorded ([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})$/i;

/**
 * The run a test was recorded from, from its name: recording names a test `recorded <run id>`
 * (archi/05, "Tests recorded from runs"). The test document's `from` says it for sure.
 */
export function recordedRunOf(name: string): string | null {
  return RECORDED.exec(name)?.[1] ?? null;
}

/** A version's suite, read against its base's when the base is among `versions`. */
export function suiteOf(version: WorkflowVersion, versions: readonly WorkflowVersion[]): Suite {
  const base = version.base ? findVersion(versions, version.base) : null;
  const baseMissing = Boolean(version.base) && base === null;
  const before = new Map((base?.tests ?? []).map((t) => [t.name, t.digest]));
  const replaced = new Map((version.replaced ?? []).map((r) => [r.name, r]));
  const tests = (version.tests ?? []).map((test): SuitePlace => {
    const recordedFrom = recordedRunOf(test.name);
    const replacement = replaced.get(test.name);
    if (replacement) {
      return {
        ...test,
        kind: "replaced",
        from: replacement.old,
        reason: replacement.reason,
        recordedFrom,
      };
    }
    const entry = { ...test, from: null, reason: null, recordedFrom };
    if (!version.base) return { ...entry, kind: "added" };
    if (base === null) return { ...entry, kind: "kept" };
    const old = before.get(test.name);
    if (old === undefined) return { ...entry, kind: "added" };
    // Replaced without a reason in `replaced`: shown as a replacement all the same.
    return old === test.digest
      ? { ...entry, kind: "inherited" }
      : { ...entry, kind: "replaced", from: old };
  });
  return { tests, retired: version.retired ?? [], base, baseMissing };
}

/** A test's place in a version's suite: one of its tests, or one of the base's it retired. */
export interface TestPlace {
  name: string;
  digest: string;
  kind: SuiteKind | "retired";
  /** For a replaced test: the base's test it replaced. */
  from: string | null;
  /** Why it was replaced or retired; `null` when no reason was recorded. */
  reason: string | null;
  recordedFrom: string | null;
}

/** The tests of a suite in the order Workflows lists them: its tests, then those it retired. */
export function suitePlaces(suite: Suite): TestPlace[] {
  return [
    ...suite.tests,
    ...suite.retired.map(
      (t): TestPlace => ({
        name: t.name,
        digest: t.digest,
        kind: "retired",
        from: null,
        reason: t.reason,
        recordedFrom: recordedRunOf(t.name),
      }),
    ),
  ];
}

/** A workflow's page, by its id, at a version (its latest when none is named). */
export function workflowPath(id: string, version?: number | null): string {
  const path = `/workflows/${encodeURIComponent(id)}`;
  return version ? `${path}?version=${version}` : path;
}

/** A test's page, in the suite of a version of a workflow (the latest when none is named). */
export function testPath(id: string, digest: string, version?: number | null): string {
  const path = `/workflows/${encodeURIComponent(id)}/tests/${digest}`;
  return version ? `${path}?version=${version}` : path;
}

/** How many of each kind a suite has. */
export function suiteCounts(suite: Suite): Record<SuiteKind | "retired", number> {
  const counts = { inherited: 0, added: 0, replaced: 0, kept: 0, retired: suite.retired.length };
  for (const t of suite.tests) counts[t.kind] += 1;
  return counts;
}

/** An action's rules, one or several: always a list. */
export function rulesOf(rules: ActionRules): ActionRule[] {
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

/** When a test sends a signal: once the run waits (the default), or after a duration. */
export function signalWhen(when: string | null | undefined): string {
  return !when || when === "waiting" || when === "blocked" ? "once the run waits" : `after ${when}`;
}

/** What a cassette holds, in short: how it is matched, its calls by action, its signals. */
export interface CassetteSummary {
  matching: string | null;
  calls: number;
  byAction: { action: string; count: number }[];
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
    const record = asRecord(call);
    // Its action, or its tool in a cassette written before the actions were named.
    const action = record?.action ?? record?.tool;
    const name = typeof action === "string" ? action : "?";
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
    byAction: [...counts]
      .map(([action, count]) => ({ action, count }))
      .sort((a, b) => b.count - a.count || a.action.localeCompare(b.action)),
    signals,
  };
}
