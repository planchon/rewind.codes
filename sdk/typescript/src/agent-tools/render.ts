/**
 * What the agent tools say to the model: plain text, short, bounded, and pointing at the next
 * useful call.
 */
import type {
  Callsite,
  CheckResult,
  Diagnostic,
  Entry,
  FailedCall,
  Payload,
  Run,
  RunStatus,
  RunSummary,
  TestResult,
  ToolMatch,
  Waiting,
} from "../types.js";
import { sliceUtf8, truncateEnd, truncateMiddle, utf8Length } from "./bound.js";

/** The bounds of each part of a tool result, in bytes. */
export interface Limits {
  /** A run's output, and each page of `get_run`. */
  output: number;
  /** A run's LLM trace. */
  trace: number;
  /** A value shown in passing: a failed call's arguments, a test's output. */
  value: number;
  /** One log line, a stack. */
  line: number;
  /** A whole result: room for the parts above, and a last bound on any text left unbounded. */
  total: number;
}

export function limitsFor(output: number): Limits {
  const trace = Math.max(4096, Math.floor(output / 2));
  return {
    output,
    trace,
    value: Math.max(1024, Math.floor(output / 64)),
    line: 2048,
    total: output + trace + 64 * 1024,
  };
}

/** The parts of a run's report that take a request of their own. */
export interface RunExtras {
  /** The output as compact JSON, when the run completed. */
  output?: string | undefined;
  /** The LLM trace. */
  trace?: string | undefined;
}

/** A run's state for the model: its outcome, or what it waits for. */
export function describeRun(run: Run, extras: RunExtras, limits: Limits): string {
  const lines: string[] = [headline(run, limits)];
  switch (run.status) {
    case "completed":
      if (extras.output !== undefined) {
        lines.push(...outputSection(run.id, extras.output, limits.output));
      }
      break;
    case "failed":
    case "cancelled":
      if (run.error) {
        lines.push(...errorSection(run.error, limits));
      }
      break;
    case "active":
      lines.push(...waitingSection(run));
      break;
    case "aborted":
    case "pending":
      break;
  }
  if (extras.trace !== undefined) {
    lines.push(run.status === "failed" ? "Trace up to the failure:" : "Trace:");
    lines.push(truncateMiddle(extras.trace.trimEnd(), limits.trace));
  } else if (run.logs && run.logs.length > 0) {
    lines.push("Last log lines:");
    for (const log of run.logs) {
      lines.push(`  [${log.level}]${at(log.callsite)} ${truncateEnd(log.message, limits.line)}`);
    }
  }
  if (run.status === "active" || run.status === "pending") {
    lines.push(
      `Follow it with get_run(${args({ run_id: run.id })}), send it a signal with signal_run, or cancel it with cancel_run.`,
    );
  }
  return lines.join("\n");
}

function headline(run: Run, limits: Limits): string {
  const fork = run.parent ? ` (a fork of run ${run.parent.run})` : "";
  const name = `Run ${run.id}${fork}`;
  switch (run.status) {
    case "completed":
      return `${name} completed.`;
    case "failed":
      return `${name} failed.`;
    case "cancelled":
      return `${name} was cancelled.`;
    case "aborted":
      return run.abort
        ? `${name} was aborted by the host (${run.abort.kind}): ${truncateEnd(run.abort.message, limits.value)}`
        : `${name} was aborted by the host.`;
    case "pending":
      return `${name} is pending: it has not started executing yet.`;
    case "active":
      if (run.quarantine) {
        return `${name} is quarantined (${run.quarantine.cause}): ${truncateEnd(run.quarantine.reason, limits.value)}. It waits for the platform to be fixed; it is not a fault of the script.`;
      }
      return isSuspended(run) ? `${name} is suspended, waiting for:` : `${name} is still running.`;
  }
}

/** Waiting for something other than tool calls in flight: a signal, a timer, the harness. */
export function isSuspended(run: Run): boolean {
  return (run.waitingFor ?? []).some(
    (waiting) => waiting.kind !== "tool_call" || !["ready", "claimed", "done"].includes(waiting.state),
  );
}

function waitingSection(run: Run): string[] {
  const waiting = run.waitingFor ?? [];
  if (waiting.length === 0 || run.quarantine) {
    return [];
  }
  const lines = isSuspended(run) ? [] : ["Waiting for:"];
  for (const item of waiting) {
    lines.push(`- ${describeWaiting(item)}`);
  }
  return lines;
}

function describeWaiting(waiting: Waiting): string {
  switch (waiting.kind) {
    case "signal": {
      const timeout = waiting.timeoutAt ? `, until ${waiting.timeoutAt} (then it gets undefined)` : "";
      return `the signal ${JSON.stringify(waiting.name)}${at(waiting.callsite)}${timeout}: send it with signal_run`;
    }
    case "timer":
      return `a timer${at(waiting.callsite)}, firing at ${waiting.fireAt}`;
    case "tool_call": {
      const call = `the call ${waiting.seq} to ${waiting.tool}${at(waiting.callsite)}`;
      const attempt = waiting.attempt > 1 ? ` (attempt ${waiting.attempt})` : "";
      switch (waiting.state) {
        case "approval":
          return `${call}: it waits for the harness's approval`;
        case "parked":
          return waiting.parked === "credentials_expired"
            ? `${call}: parked, the server refused the credentials; the harness must renew them`
            : `${call}: parked, the tool changed since the toolset was pinned; the harness decides whether it goes on`;
        case "deferred":
          return `${call}: waiting for its answer${attempt}`;
        case "claimed":
          return `${call}: in progress${attempt}`;
        case "ready":
          return `${call}: queued${attempt}`;
        case "done":
          return `${call}: answered, about to be delivered`;
      }
    }
  }
}

function outputSection(runId: string, output: string, limit: number): string[] {
  const slice = sliceUtf8(output, 0, limit);
  if (slice.end === slice.total) {
    return ["Output:", output];
  }
  return [
    `Output (${slice.total} bytes; the first ${slice.end} are shown, read the rest with get_run(${args({ run_id: runId, offset: slice.end })})):`,
    slice.text,
  ];
}

function errorSection(error: NonNullable<Run["error"]>, limits: Limits): string[] {
  const lines = [
    `${truncateEnd(error.name, limits.line)}: ${truncateEnd(error.message, limits.value)}${at(error.location)}`,
  ];
  if (error.frame) {
    lines.push(truncateEnd(error.frame.trimEnd(), limits.value));
  }
  if (error.failedCall) {
    lines.push(...failedCallSection(error.failedCall, limits));
  }
  if (error.stack) {
    lines.push("Stack:", truncateEnd(error.stack.trimEnd(), limits.line));
  }
  return lines;
}

function failedCallSection(call: FailedCall, limits: Limits): string[] {
  return [
    `The call that failed: ${call.tool} (call ${call.seq}${at(call.callsite)}) with ${payloadText(call.args, limits.value)}`,
    `  ${call.error.kind}: ${truncateEnd(call.error.message, limits.value)}${call.error.data === undefined ? "" : ` ${truncateEnd(json(call.error.data), limits.value)}`}`,
  ];
}

/** A page of a long text: the run's output, or an entry. */
export function describePage(
  title: string,
  text: string,
  offset: number,
  limit: number,
  next: (offset: number) => string,
): string {
  const slice = sliceUtf8(text, offset, limit);
  if (slice.start === 0 && slice.end === slice.total) {
    return `${title}:\n${text}`;
  }
  const lines = [`${title}, bytes ${slice.start} to ${slice.end} of ${slice.total}:`, slice.text];
  if (slice.end < slice.total) {
    lines.push(`Read the rest with ${next(slice.end)}.`);
  }
  return lines.join("\n");
}

/** One journal entry in full. */
export function entryText(entry: Entry): string {
  return json({ idx: entry.idx, recordedAt: entry.recordedAt, event: entry.event });
}

/** The diagnostics of a check, for the model. */
export function describeCheck(result: CheckResult, limits: Limits): string {
  const errors = result.diagnostics.filter((d) => d.severity === "error").length;
  const warnings = result.diagnostics.length - errors;
  const counts = [plural(errors, "error"), plural(warnings, "warning")].filter((part) => !part.startsWith("0 "));
  if (result.ok) {
    return warnings === 0
      ? "The script type-checks."
      : `The script type-checks, with ${counts.join(" and ")}:\n${diagnosticsText(result.diagnostics, limits)}`;
  }
  return `The script does not type-check: ${counts.join(" and ")}.\n${diagnosticsText(result.diagnostics, limits)}`;
}

/** A script refused before any run exists. */
export function describeRefusal(detail: string, diagnostics: Diagnostic[], limits: Limits): string {
  const text = `The script was refused before anything ran: ${truncateEnd(detail, limits.value)}`;
  return diagnostics.length === 0 ? text : `${text}\n${diagnosticsText(diagnostics, limits)}`;
}

/** The diagnostics, in order, as many as the room of a trace holds. */
export function diagnosticsText(diagnostics: Diagnostic[], limits: Limits): string {
  const shown: string[] = [];
  let bytes = 0;
  for (const d of diagnostics) {
    const location = d.location ? `script.ts:${d.location.line}:${d.location.column} ` : "";
    const lines = [`${location}${d.severity} ${d.code}: ${truncateEnd(d.message, limits.value)}`];
    if (d.frame) {
      lines.push(truncateEnd(d.frame.trimEnd(), limits.value));
    }
    if (d.hint) {
      lines.push(`hint: ${truncateEnd(d.hint, limits.line)}`);
    }
    const text = lines.join("\n");
    bytes += utf8Length(text) + 2;
    if (bytes > limits.trace && shown.length > 0) {
      shown.push(`… and ${plural(diagnostics.length - shown.length, "more diagnostic")}.`);
      break;
    }
    shown.push(text);
  }
  return shown.join("\n\n");
}

/** The outcome of a scenario. */
export function describeTest(result: TestResult, limits: Limits): string {
  const lines: string[] = [];
  if (result.passed) {
    lines.push("The test passed.");
  } else if (result.failures.length === 0) {
    lines.push("The test failed.");
  } else {
    lines.push(`The test failed: ${plural(result.failures.length, "expectation")} do not hold:`);
    lines.push(...result.failures.map((failure) => `- ${truncateEnd(failure, limits.value)}`));
  }
  lines.push(`The run ${statusPhrase(result.status)}.`);
  if (result.output !== undefined && result.status === "completed") {
    lines.push(`Output: ${truncateEnd(json(result.output), limits.output)}`);
  }
  if (result.error) {
    lines.push(`${truncateEnd(result.error.name, limits.line)}: ${truncateEnd(result.error.message, limits.value)}`);
  }
  if (result.abort) {
    lines.push(`Aborted (${result.abort.kind}): ${truncateEnd(result.abort.message, limits.value)}`);
  }
  const calls = Object.entries(result.calls);
  lines.push(
    calls.length === 0
      ? "No tool was called."
      : `Calls: ${calls.map(([tool, count]) => `${tool} ×${count}`).join(", ")}`,
  );
  if (result.trace.trim() !== "") {
    lines.push("Trace:", truncateMiddle(result.trace.trimEnd(), limits.trace));
  }
  return lines.join("\n");
}

/** A list of runs. */
export function describeRuns(runs: RunSummary[], more: boolean, status: RunStatus | undefined): string {
  const which = status === undefined ? "" : ` with the status ${status}`;
  if (runs.length === 0) {
    return `No runs${which}.`;
  }
  const lines = [`${plural(runs.length, "run")}${which}, most recent first:`];
  for (const run of runs) {
    const parts = [`${run.id}: ${run.status}${run.error ? ` (${run.error})` : ""}`];
    if (run.quarantined) {
      parts.push("quarantined");
    }
    parts.push(`started ${run.startedAt}`);
    parts.push(plural(run.toolCalls, "tool call"));
    if (run.errors > 0) {
      parts.push(`${run.errors} failed`);
    }
    if (run.parent) {
      parts.push(`a fork of ${run.parent}`);
    }
    lines.push(`- ${parts.join(", ")}`);
  }
  if (more) {
    lines.push("More runs exist: narrow the list with a status.");
  }
  return lines.join("\n");
}

/** The tools matching a search. */
export function describeMatches(query: string, matches: ToolMatch[]): string {
  if (matches.length === 0) {
    return `No tool matches ${JSON.stringify(query)}.`;
  }
  return [
    `${plural(matches.length, "tool")} ${matches.length === 1 ? "matches" : "match"} ${JSON.stringify(query)}:`,
    ...matches.map((m) => m.dts.trimEnd()),
  ].join("\n\n");
}

function statusPhrase(status: RunStatus): string {
  switch (status) {
    case "completed":
      return "completed";
    case "failed":
      return "failed";
    case "cancelled":
      return "was cancelled";
    case "aborted":
      return "was aborted";
    case "pending":
      return "never started";
    case "active":
      return "was still waiting at the end of the scenario";
  }
}

function payloadText(payload: Payload, limit: number): string {
  return "value" in payload ? truncateEnd(json(payload.value), limit) : `${payload.preview}… (${payload.bytes} bytes)`;
}

function at(callsite: Callsite | null | undefined): string {
  return callsite ? ` at script.ts:${callsite.line}:${callsite.column}` : "";
}

/** A tool call as the model would write it. */
export function args(values: Record<string, unknown>): string {
  return JSON.stringify(values);
}

export function json(value: unknown): string {
  return JSON.stringify(value === undefined ? null : value) ?? "null";
}

function plural(count: number, noun: string): string {
  return `${count} ${noun}${count === 1 ? "" : "s"}`;
}
