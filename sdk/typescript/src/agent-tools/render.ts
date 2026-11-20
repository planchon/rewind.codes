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
  RecordedTests,
  Run,
  RunStatus,
  RunSummary,
  Test,
  TestProblem,
  TestResult,
  ToolMatch,
  Waiting,
  Workflow,
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
  const about = [
    run.parent ? `a fork of run ${run.parent.run}` : undefined,
    run.workflow ? `a run of the workflow ${workflowRef(run.workflow)}` : undefined,
    run.startedBy ? `started by run ${run.startedBy.run}${run.startedBy.detached ? ", detached" : ""}` : undefined,
  ].filter((part) => part !== undefined);
  const name = `Run ${run.id}${about.length === 0 ? "" : ` (${about.join(", ")})`}`;
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
          if (waiting.parked === "credentials_refused")
            return `${call}: parked, the server refused the credentials; the harness must renew them`;
          if (waiting.parked === "secret_missing")
            return `${call}: parked, the secret the run attached no longer exists; the harness must put it again`;
          return `${call}: parked, the tool changed since the toolset was pinned; the harness decides whether it goes on`;
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

/** A workflow saved: how the model's scripts use it, and its declaration. */
export function describeWorkflow(workflow: Workflow, limits: Limits): string {
  const lines = [
    `Saved the workflow ${workflowRef(workflow)}. Your next scripts can import it:`,
    `import { ${workflow.name} } from "rewind:workflows";`,
  ];
  const prompt = workflow.declarations.prompt.trim();
  if (prompt !== "") {
    lines.push(truncateEnd(prompt, limits.value));
  }
  const dts = workflow.declarations.dts.trim();
  if (dts !== "") {
    lines.push("Its declaration:", truncateEnd(dts, limits.trace));
  }
  return lines.join("\n");
}

/** A workflow refused: why, and the diagnostics of its script. */
export function describeWorkflowRefusal(detail: string, diagnostics: Diagnostic[], limits: Limits): string {
  const text = `The workflow was not saved: ${truncateEnd(detail, limits.value)}`;
  return diagnostics.length === 0 ? text : `${text}\n${diagnosticsText(diagnostics, limits)}`;
}

/** `triage@9f2c4e1a07b3`: a workflow's name and the first 12 digits of its version. */
export function workflowRef(workflow: { name: string; version: string }): string {
  return `${workflow.name}@${workflow.version.slice(0, 12)}`;
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

/**
 * The outcome of tests: a line per test that passed; for each one that did not, its problem, its failures, how its
 * run ended, its calls and its trace. The traces share the room of one trace.
 */
export function describeTestResults(results: TestResult[], limits: Limits): string {
  if (results.length === 0) {
    return "No test ran.";
  }
  const failing = results.filter((result) => !result.passed);
  const lines = [
    failing.length === 0
      ? results.length === 1
        ? "The test passed."
        : `All ${results.length} tests passed.`
      : `${failing.length} of ${plural(results.length, "test")} did not pass.`,
  ];
  const traced = failing.filter((result) => (result.trace ?? "").trim() !== "").length;
  const traceRoom = Math.max(limits.line, Math.floor(limits.trace / Math.max(1, traced)));
  for (const result of results) {
    const name = JSON.stringify(truncateEnd(result.name, limits.line));
    if (result.passed) {
      lines.push(`PASS ${name}`);
      continue;
    }
    lines.push(`FAIL ${name}${result.problem ? ` (${result.problem}): ${problemPhrase(result.problem)}` : ""}`);
    lines.push(...result.failures.map((failure) => `  - ${truncateEnd(failure, limits.value)}`));
    if (result.status === undefined || result.status === null) {
      continue;
    }
    lines.push(`  The run ${statusPhrase(result.status)}.`);
    if (result.output !== undefined && result.status === "completed") {
      lines.push(`  Output: ${truncateEnd(json(result.output), limits.value)}`);
    }
    if (result.error) {
      lines.push(
        `  ${truncateEnd(result.error.name, limits.line)}: ${truncateEnd(result.error.message, limits.value)}`,
      );
    }
    if (result.abort) {
      lines.push(`  Aborted (${result.abort.kind}): ${truncateEnd(result.abort.message, limits.value)}`);
    }
    const calls = Object.entries(result.calls ?? {});
    lines.push(
      calls.length === 0
        ? "  No tool was called."
        : `  Calls: ${calls.map(([tool, count]) => `${tool} ×${count}`).join(", ")}`,
    );
    const trace = (result.trace ?? "").trimEnd();
    if (trace.trim() !== "") {
      lines.push("  Trace:", truncateMiddle(trace, traceRoom));
    }
  }
  return lines.join("\n");
}

function problemPhrase(problem: TestProblem): string {
  switch (problem) {
    case "stale":
      return "it names tools, answers or args the toolset no longer has; it was not run.";
    case "incompatible":
      return "the workflow's input or signal schemas refuse its input or its signals; it was not run.";
    case "failed":
      return "it ran, and expectations do not hold:";
  }
}

/** Tests that refused a save of a workflow: the outcome of each, and what the model may do. */
export function describeSaveRefused(detail: string, results: TestResult[], workflow: string, limits: Limits): string {
  return [
    `The workflow was not saved: ${truncateEnd(detail, limits.value)}`,
    describeTestResults(results, limits),
    `Every test must pass: there is no way around it. If the script is wrong, fix it, and check it with test_script(${args({ code: "…", workflow })}) before you save it again. If a test pins a behavior you change on purpose, retire it in \`retire\` (the behavior is deprecated) or update it in \`update\` (its state or its expected result is out of date), with a reason.`,
  ].join("\n");
}

/** Tests that refused the fork of a run of a workflow: the outcome of each, and what the model may do. */
export function describeRetryRefused(
  runId: string,
  detail: string,
  results: TestResult[],
  workflow: string | undefined,
  limits: Limits,
): string {
  const suite = workflow === undefined ? "the suite of its workflow" : `the suite of ${workflow}`;
  const check = workflow === undefined ? "test_script" : `test_script(${args({ code: "…", workflow })})`;
  return [
    `Run ${runId} was not retried: ${truncateEnd(detail, limits.value)}`,
    describeTestResults(results, limits),
    `A fix must keep every behavior ${suite} pins: there is no way around it. Fix the script, and check it with ${check} before you try again. If a behavior must change on purpose, save a new version of the workflow with save_workflow, retiring or updating its tests with a reason, and start a new run of it.`,
  ].join("\n");
}

/** What `get_workflow` reads: the latest version, its suite with each test, and the lineage. */
export interface WorkflowDetails {
  /** Every version, newest first. */
  versions: Workflow[];
  /** The source of the latest version. */
  source: string;
  /** The tests of the latest version's suite, in its order: the test, or why it cannot be read. */
  tests: Array<{ name: string; digest: string; test?: Test | undefined; unreadable?: string | undefined }>;
}

/** A workflow, for the model to repair or extend it: its source, schemas, suite and lineage. */
export function describeWorkflowDetails(details: WorkflowDetails, limits: Limits): string {
  const [latest] = details.versions;
  if (latest === undefined) {
    return "The workflow has no version.";
  }
  const lines = [
    `The workflow ${workflowRef(latest)}, saved ${latest.createdAt}: its latest version, of ${plural(details.versions.length, "version")}.`,
  ];
  if (latest.parent) {
    lines.push(`It derives from the version ${versionRef(latest.parent, details.versions)}.`);
  }
  if (latest.description) {
    lines.push(`Description: ${truncateEnd(latest.description, limits.value)}`);
  }
  lines.push(`Input schema: ${truncateEnd(json(latest.input), limits.value)}`);
  lines.push(`Output schema: ${truncateEnd(json(latest.output), limits.value)}`);
  const signals = Object.entries(latest.signals);
  if (signals.length === 0) {
    lines.push("Signals: none.");
  } else {
    lines.push("Signals:");
    for (const [name, schema] of signals) {
      lines.push(`- ${JSON.stringify(name)}: ${truncateEnd(json(schema), limits.value)}`);
    }
  }
  lines.push("Source:", truncateMiddle(details.source.trimEnd(), limits.trace));

  if (details.tests.length === 0) {
    lines.push("Its suite is empty: it has no test.");
  } else {
    lines.push(
      `Its suite, ${plural(details.tests.length, "test")}, which every version derived from it must pass (give a digest in save_workflow's \`tests\` or \`update\` to reuse a test):`,
    );
    const room = Math.max(limits.value, Math.floor(limits.output / details.tests.length));
    for (const entry of details.tests) {
      const head = `Test ${JSON.stringify(truncateEnd(entry.name, limits.line))} (${entry.digest})`;
      if (entry.test === undefined) {
        lines.push(`${head}: it cannot be read: ${truncateEnd(entry.unreadable ?? "", limits.value)}`);
        continue;
      }
      const origin = entry.test.origin
        ? `, recorded from run ${entry.test.origin.run} of ${versionRef(entry.test.origin.version, details.versions)}`
        : "";
      lines.push(`${head}${origin}:`, truncateEnd(json(readableTest(entry.test)), room));
    }
  }

  lines.push("Lineage, newest first:");
  for (const version of details.versions) {
    const parts = [`- ${workflowRef(version)}, saved ${version.createdAt}`];
    parts.push(version.parent ? `, from ${versionRef(version.parent, details.versions)}` : ", the first version");
    parts.push(`: ${plural((version.tests ?? []).length, "test")}`);
    lines.push(`${parts.join("")}.`);
    for (const updated of version.updated ?? []) {
      lines.push(
        `  updated ${JSON.stringify(truncateEnd(updated.name, limits.line))}: ${truncateEnd(updated.reason, limits.value)}`,
      );
    }
    for (const retired of version.retired ?? []) {
      lines.push(
        `  retired ${JSON.stringify(truncateEnd(retired.name, limits.line))}: ${truncateEnd(retired.reason, limits.value)}`,
      );
    }
  }
  return lines.join("\n");
}

/** `triage@4be1c07a9d3e` when the version is one of `versions`, otherwise its first 12 digits. */
function versionRef(version: string, versions: Workflow[]): string {
  const known = versions.find((candidate) => candidate.version === version);
  return known ? workflowRef(known) : version.slice(0, 12);
}

/** A test as the model reads it: its cassette summarized, since a recorded run's calls are long and rarely useful. */
function readableTest(test: Test): Test {
  const cassette = test.world?.cassette;
  if (cassette === undefined || cassette === null) {
    return test;
  }
  return { ...test, world: { ...test.world, cassette: { summary: cassetteSummary(cassette) } } };
}

/** `12 calls matched by_args (github.list_issues ×3, github.close_issue ×9), 1 signal (approval)`. */
export function cassetteSummary(cassette: Record<string, unknown>): string {
  const calls = Array.isArray(cassette["calls"]) ? (cassette["calls"] as unknown[]) : [];
  const signals = Array.isArray(cassette["signals"]) ? (cassette["signals"] as unknown[]) : [];
  const matching = typeof cassette["matching"] === "string" ? ` matched ${cassette["matching"]}` : "";
  const byTool = new Map<string, number>();
  for (const call of calls) {
    const tool = isRecord(call) && typeof call["tool"] === "string" ? call["tool"] : "?";
    byTool.set(tool, (byTool.get(tool) ?? 0) + 1);
  }
  const tools = [...byTool].map(([tool, count]) => `${tool} ×${count}`).join(", ");
  const names = [...new Set(signals.map((s) => (isRecord(s) && typeof s["name"] === "string" ? s["name"] : "?")))];
  return [
    `${plural(calls.length, "recorded call")}${matching}${tools === "" ? "" : ` (${tools})`}`,
    `${plural(signals.length, "signal")}${names.length === 0 ? "" : ` (${names.join(", ")})`}`,
  ].join(", ");
}

/** The tests a recording stored, and the runs it left out. */
export function describeRecorded(workflow: string, recorded: RecordedTests, limits: Limits): string {
  const lines: string[] = [];
  if (recorded.tests.length === 0) {
    lines.push(`No test was recorded: no completed or failed run of ${workflow} was selected.`);
  } else {
    lines.push(`Recorded ${plural(recorded.tests.length, "test")} from the runs of ${workflow}, one per path:`);
    for (const test of recorded.tests) {
      lines.push(
        `- ${test.digest} ${JSON.stringify(test.name)}, from run ${test.run} (${test.status}): ${truncateEnd(test.path, limits.value)}`,
      );
    }
  }
  if (recorded.skipped.length > 0) {
    lines.push(`Skipped ${plural(recorded.skipped.length, "run")}:`);
    for (const skipped of recorded.skipped) {
      lines.push(`- ${skipped.run}: ${truncateEnd(skipped.reason, limits.value)}`);
    }
  }
  if (recorded.tests.length > 0) {
    lines.push(
      `They join no suite yet: pass their digests in save_workflow's \`tests\` to add them to the suite of ${workflow}'s next version, or { "from": "<digest>", "expect": {…} } to give a failed run the expectation it should have met.`,
    );
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
    if (run.workflow) {
      parts.push(`a run of the workflow ${run.workflow}`);
    }
    if (run.startedBy) {
      parts.push(`started by ${run.startedBy}`);
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
      return "was still waiting at the end of the test";
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

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
