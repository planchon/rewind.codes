// Workflows across runs, as a run's journal shows them (archi/03-execution.md, "Workflows across
// runs"): a script starts a run of a workflow with a call of `workflows.<name>.start`, whose
// result is `{ runId }`, signals a run with `workflows.<name>.signal` (`{ run, name, payload }`),
// and the end of a run it started comes back as the signal `rewind:ended:<run id>`, whose payload
// is `{ status, output | error | reason }`.
import type { Payload } from "../api/types";
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
