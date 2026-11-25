/**
 * What the agent tools say to the model: plain text, short, bounded, and pointing at the next useful call, as the Rust
 * agent tools say it (`crates/rewind-agent-tools/src/render.rs`). A workflow is named by its name, else its id, and a
 * version by its number: `triage@v12`.
 */
import type {
  ActionMatch,
  Callsite,
  CheckResult,
  Diagnostic,
  ErrorInfo,
  FailedCall,
  HistoryEvent,
  PatchWorkflow,
  Payload,
  RecordedTests,
  Run,
  RunStatus,
  RunSummary,
  RunWorkflow,
  Test,
  TestProblem,
  TestResult,
  Waiting,
  Workflow,
  WorkflowVersion,
} from "../types.js";
import { sliceUtf8, truncateEnd, truncateMiddle, utf8Length } from "./bound.js";

/** The bounds of each part of a tool result, in bytes. */
export interface Limits {
  /** A run's output, and each page of `get_run`. */
  output: number;
  /** A run's transcript. */
  transcript: number;
  /** A value shown in passing: a failed call's arguments, a test's output. */
  value: number;
  /** One log line, a stack. */
  line: number;
  /** A whole result: room for the parts above, and a last bound on any text left unbounded. */
  total: number;
}

/** The SDK's bounds: a page of output of `output` bytes, and room around it for a transcript. */
export function limitsFor(output: number): Limits {
  const transcript = Math.max(4096, Math.floor(output / 2));
  return {
    output,
    transcript,
    value: Math.max(1024, Math.floor(output / 64)),
    line: 2048,
    total: output + transcript + 64 * 1024,
  };
}

// ---------------------------------------------------------------------------------------------
// Runs

/** The parts of a run's report that take a request of their own. */
export interface RunExtras {
  /** The output as compact JSON, when the run completed. */
  output?: string | undefined;
  /** The transcript. */
  transcript?: string | undefined;
}

/** A run's state for the model: its outcome, or what it waits for. */
export function describeRun(run: Run, extras: RunExtras, limits: Limits): string {
  const lines: string[] = [headline(run, limits)];
  switch (run.status) {
    case "completed":
      if (extras.output !== undefined) {
        lines.push(...outputSection(run.handle, extras.output, limits.output));
      }
      break;
    case "failed":
    case "cancelled":
      if (run.error) {
        lines.push(...errorSection(run.error, limits));
      }
      break;
    case "waiting":
      lines.push(...waitingSection(run));
      break;
    case "aborted":
    case "queued":
    case "running":
      break;
  }
  if (extras.transcript !== undefined) {
    lines.push(run.status === "failed" ? "Transcript up to the failure:" : "Transcript:");
    lines.push(truncateMiddle(extras.transcript.trimEnd(), limits.transcript));
  } else if (run.logs && run.logs.length > 0) {
    lines.push("Last log lines:");
    for (const log of run.logs) {
      lines.push(`  [${log.level}]${at(log.callsite)} ${truncateEnd(log.message, limits.line)}`);
    }
  }
  if (!isEnded(run.status)) {
    lines.push(
      `Follow it with get_run(${args({ run_id: run.handle })}), send it a signal with signal_run, or cancel it with cancel_run.`,
    );
  }
  return lines.join("\n");
}

/** Whether a run has ended: completed, failed, cancelled or aborted. */
function isEnded(status: RunStatus): boolean {
  return status === "completed" || status === "failed" || status === "cancelled" || status === "aborted";
}

function headline(run: Run, limits: Limits): string {
  const about = [
    run.source ? `a fork of run ${run.source.handle}` : undefined,
    run.workflow ? `a run of the workflow ${runWorkflowLabel(run.workflow)}` : undefined,
    run.startedBy ? `started by run ${run.startedBy.handle}${run.startedBy.detached ? ", detached" : ""}` : undefined,
  ].filter((part) => part !== undefined);
  const name = about.length === 0 ? `Run ${run.handle}` : `Run ${run.handle} (${about.join("; ")})`;
  switch (run.status) {
    case "completed":
      return `${name} completed.`;
    case "failed":
      return `${name} failed.`;
    case "cancelled":
      return `${name} was cancelled.`;
    case "aborted":
      return run.abort
        ? `${name} was aborted by the engine (${run.abort.kind}): ${truncateEnd(run.abort.message, limits.value)}`
        : `${name} was aborted by the engine.`;
    case "queued":
      return `${name} is queued: it has not started executing yet.`;
    case "running":
      return `${name} is still running.`;
    case "waiting":
      if (run.quarantine) {
        return `${name} is quarantined (${run.quarantine.cause}): ${truncateEnd(run.quarantine.reason, limits.value)}. It waits for the platform to be fixed; it is not a fault of the code.`;
      }
      return `${name} is waiting for:`;
  }
}

function waitingSection(run: Run): string[] {
  const waiting = run.waitingFor ?? [];
  if (waiting.length === 0 || run.quarantine) {
    return [];
  }
  return waiting.filter((item) => item.kind !== "quarantined").map((item) => `- ${describeWaiting(item)}`);
}

function describeWaiting(waiting: Waiting): string {
  switch (waiting.kind) {
    case "signal": {
      const timeout = waiting.timeoutAt ? `, until ${waiting.timeoutAt} (then it gets undefined)` : "";
      return `the signal ${quoted(waiting.name)}${at(waiting.callsite)}${timeout}: send it with signal_run`;
    }
    case "timer":
      return `a timer${at(waiting.callsite)}, firing at ${waiting.fireAt}`;
    case "call": {
      const call = `the call ${waiting.seq} to ${waiting.action}${at(waiting.callsite)}`;
      const delivery = waiting.delivery > 1 ? ` (delivery ${waiting.delivery})` : "";
      switch (waiting.state) {
        case "approval":
          return `${call}: it waits for the harness's approval`;
        case "parked":
          if (waiting.parked === "credentials_refused") {
            return `${call}: parked, the server refused the credentials; the harness must renew them`;
          }
          if (waiting.parked === "secret_missing") {
            return `${call}: parked, the secret the run uses no longer exists; the harness must put it again`;
          }
          return `${call}: parked, the action changed since the action set was pinned; the harness decides whether it goes on`;
        case "deferred":
          return `${call}: waiting for its answer${delivery}`;
        case "claimed":
          return `${call}: in progress${delivery}`;
        case "ready":
          return `${call}: queued${delivery}`;
        case "done":
          return `${call}: answered, about to be delivered`;
      }
    }
    case "quarantined":
      return "its release from quarantine";
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

function errorSection(error: ErrorInfo, limits: Limits): string[] {
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
  const data =
    call.error.data === undefined || call.error.data === null
      ? ""
      : ` ${truncateEnd(json(call.error.data), limits.value)}`;
  return [
    `The call that failed: ${call.action} (call ${call.seq}${at(call.callsite)}) with ${payloadText(call.args, limits.value)}`,
    `  ${call.error.kind}: ${truncateEnd(call.error.message, limits.value)}${data}`,
  ];
}

/** A page of a long text: the run's output, or an event. */
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

/** One event in full. */
export function eventText(event: HistoryEvent): string {
  return json({ idx: event.idx, recordedAt: event.recordedAt, event: event.event });
}

// ---------------------------------------------------------------------------------------------
// Checks, tests, lists, searches

/** The diagnostics of a check, for the model, and the workflow that already has the code. */
export function describeCheck(result: CheckResult, limits: Limits): string {
  const errors = result.diagnostics.filter((d) => d.severity === "error").length;
  const warnings = result.diagnostics.length - errors;
  const counts = [plural(errors, "error"), plural(warnings, "warning")].filter((part) => !part.startsWith("0 "));
  let text: string;
  if (result.ok) {
    text =
      warnings === 0
        ? "The code type-checks."
        : `The code type-checks, with ${counts.join(" and ")}:\n${diagnosticsText(result.diagnostics, limits)}`;
  } else {
    text = `The code does not type-check: ${counts.join(" and ")}.\n${diagnosticsText(result.diagnostics, limits)}`;
  }
  if (result.workflow) {
    text += `\nThis code is already the version v${result.workflow.version} of the workflow ${workflowOfRunLabel(result.workflow)}: running it is a run of that workflow.`;
  }
  return text;
}

/**
 * What `check_workflow` adds to its answer for a code without a comment at its top: the comment a workflow without a
 * name shows as.
 */
export const COMMENT_NOTE =
  "Note: the code does not start with a comment that says what it does, so its workflow shows as its short id until it has a name. Start it with:\n/** <what it does, in a few words>\n *\n * <more, if needed> */\nIts first line is how the workflow shows; the lines after it describe it.";

/**
 * Whether a code starts with a comment that says what it does, before any code: a block comment (`/** … *\/`) or a run
 * of `//` lines, with a line of text that is not a tag (`@ts-check`). As the platform reads it
 * (`rewind_core::code_doc`).
 */
export function hasComment(source: string): boolean {
  let rest = source.replace(/^﻿/, "").trimStart();
  // A shebang is not the comment.
  if (rest.startsWith("#!")) {
    const end = rest.indexOf("\n");
    rest = end === -1 ? "" : rest.slice(end + 1).trimStart();
  }
  let lines: string[];
  if (rest.startsWith("/*")) {
    const end = rest.indexOf("*/", 2);
    if (end === -1) {
      return false;
    }
    lines = rest
      .slice(2, end)
      .replace(/^\*+/, "")
      .split("\n")
      .map((line) => {
        const trimmed = line.trim();
        return (trimmed.startsWith("*") ? trimmed.slice(1) : trimmed).trim();
      });
  } else if (rest.startsWith("//")) {
    lines = [];
    for (const line of rest.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed.startsWith("//")) {
        break;
      }
      lines.push(trimmed.replace(/^\/+/, "").trim());
    }
  } else {
    return false;
  }
  return lines.some((line) => line !== "" && !line.startsWith("@"));
}

/** A code refused before any run exists. */
export function describeRefusal(detail: string, diagnostics: Diagnostic[], limits: Limits): string {
  return describeRefused("The code was refused before anything ran", detail, diagnostics, limits);
}

/** The diagnostics, in order, as many as the room of a transcript holds. */
export function diagnosticsText(diagnostics: Diagnostic[], limits: Limits): string {
  const shown: string[] = [];
  let bytes = 0;
  for (const d of diagnostics) {
    const location = d.location ? `workflow.ts:${d.location.line}:${d.location.column} ` : "";
    const lines = [`${location}${d.severity} ${d.code}: ${truncateEnd(d.message, limits.value)}`];
    if (d.frame) {
      lines.push(truncateEnd(d.frame.trimEnd(), limits.value));
    }
    if (d.hint) {
      lines.push(`hint: ${truncateEnd(d.hint, limits.line)}`);
    }
    const text = lines.join("\n");
    bytes += utf8Length(text) + 2;
    if (bytes > limits.transcript && shown.length > 0) {
      shown.push(`… and ${plural(diagnostics.length - shown.length, "more diagnostic")}.`);
      break;
    }
    shown.push(text);
  }
  return shown.join("\n\n");
}

/**
 * The outcome of tests: a line per test that passed; for each one that did not, its problem, its failures, how its
 * run ended, its calls and its transcript. The transcripts share the room of one transcript.
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
  const transcribed = failing.filter((result) => (result.transcript ?? "").trim() !== "").length;
  const transcriptRoom = Math.max(limits.line, Math.floor(limits.transcript / Math.max(1, transcribed)));
  for (const result of results) {
    const name = quoted(truncateEnd(result.name, limits.line));
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
    if (result.output !== undefined && result.output !== null && result.status === "completed") {
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
        ? "  No action was called."
        : `  Calls: ${calls.map(([action, count]) => `${action} ×${count}`).join(", ")}`,
    );
    const transcript = (result.transcript ?? "").trimEnd();
    if (transcript.trim() !== "") {
      lines.push("  Transcript:", truncateMiddle(transcript, transcriptRoom));
    }
  }
  return lines.join("\n");
}

function problemPhrase(problem: TestProblem): string {
  switch (problem) {
    case "stale":
      return "it names actions, answers or args the action set no longer has; it was not run.";
    case "incompatible":
      return "the workflow's input or signal schemas refuse its input or its signals; it was not run.";
    case "failed":
      return "it ran, and expectations do not hold:";
  }
}

/**
 * What `update_workflow`'s gate refused: the outcome of each test, and what the model may do. `version` is a version's
 * refusal; otherwise a change of tests', whose tests ran against `latest`, the workflow's latest version.
 */
export function describeGateRefused(
  detail: string,
  results: TestResult[],
  workflow: string,
  version: boolean,
  latest: number,
  limits: Limits,
): string {
  const advice = version
    ? `Every test must pass: there is no way around it. If the code is wrong, fix it, and check it with test_workflow(${args({ code: "…", workflow })}) before you call update_workflow again. If a test pins a behavior you change on purpose, retire it in \`retire\` (the behavior is deprecated) or replace it in \`replace\` (its state or its expected result is out of date), with a reason.`
    : `A test joins the suite only once it passes against the latest version, v${latest}. If the test is wrong, fix it. If the workflow must change to pass it, give its new \`code\` with the test: the version is made with it once the whole suite passes.`;
  return [
    `${version ? "No version was made" : "The suite was not changed"}: ${truncateEnd(detail, limits.value)}`,
    describeTestResults(results, limits),
    advice,
  ].join("\n");
}

/**
 * Tests that refused the fork of a run of a workflow: the outcome of each, and what the model may do. `updates` when
 * `update_workflow` is one of the tools.
 */
export function describeRetryRefused(
  runId: string,
  detail: string,
  results: TestResult[],
  workflow: string | undefined,
  updates: boolean,
  limits: Limits,
): string {
  const suite = workflow === undefined ? "the suite of its workflow" : `the suite of ${workflow}`;
  const check = workflow === undefined ? "test_workflow" : `test_workflow(${args({ code: "…", workflow })})`;
  const onPurpose = updates
    ? "If a behavior must change on purpose, make the workflow's next version with update_workflow, retiring or replacing its tests with a reason, and run it."
    : "If a behavior must change on purpose, the workflow needs a next version that retires or replaces those tests, which these tools do not make: tell the person.";
  return [
    `Run ${runId} was not retried: ${truncateEnd(detail, limits.value)}`,
    describeTestResults(results, limits),
    `A fix must keep every behavior ${suite} pins: there is no way around it. Fix the code, and check it with ${check} before you try again. ${onPurpose}`,
  ].join("\n");
}

/** A refusal: what was not done (`what`), why, and the diagnostics of the code. */
export function describeRefused(what: string, detail: string, diagnostics: Diagnostic[], limits: Limits): string {
  const text = `${what}: ${truncateEnd(detail, limits.value)}`;
  return diagnostics.length === 0 ? text : `${text}\n${diagnosticsText(diagnostics, limits)}`;
}

/** How a run's workflow reads in a run's report: `triage@v3, id wf_7k2m9q4x8d1c`, or `wf_7k2m9q4x8d1c@v1` without a name. */
export function runWorkflowLabel(workflow: RunWorkflow): string {
  return workflow.name
    ? `${workflow.name}@v${workflow.version}, id ${workflow.id}`
    : `${workflow.id}@v${workflow.version}`;
}

/** A run's workflow without its version: `triage (id wf_7k2m9q4x8d1c)`, or `wf_7k2m9q4x8d1c`. */
function workflowOfRunLabel(workflow: RunWorkflow): string {
  return workflow.name ? `${workflow.name} (id ${workflow.id})` : workflow.id;
}

/** What names a run's version in a call: `triage@v3`, or `wf_7k2m9q4x8d1c@v3` without a name. */
export function runWorkflowReference(workflow: RunWorkflow): string {
  return `${workflow.name ?? workflow.id}@v${workflow.version}`;
}

/** What names a workflow in a call: its name, else its id. */
export function workflowReference(workflow: Workflow): string {
  return workflow.name ?? workflow.id;
}

/**
 * A workflow as the model reads it: `triage (id wf_7k2m9q4x8d1c)`; without a name, its id and how it shows: the first
 * line of its latest version's comment, else its short id.
 */
export function workflowLabel(workflow: Workflow): string {
  if (workflow.name) {
    return `${workflow.name} (id ${workflow.id})`;
  }
  const comment = latestVersion(workflow).comment;
  const shown = comment ? quoted(comment.title) : workflow.shortId;
  return `${workflow.id} (no name; it shows as ${shown})`;
}

/** Its latest version the caller sees. */
function latestVersion(workflow: Workflow): WorkflowVersion {
  return workflow.versions[0] ?? workflow.version;
}

/** What `update_workflow` did. */
export type Updated =
  /** Its name or its description only. */
  | { kind: "patched" }
  /** A version was asked for: made, or the latest already had that content. */
  | { kind: "version"; made: boolean }
  /** Its suite changed, without a version. */
  | { kind: "tests" };

/** What a merge patch changed: `named it triage and removed its description`. */
export function describePatch(patch: PatchWorkflow): string | undefined {
  const parts: string[] = [];
  if (patch.name !== undefined) {
    parts.push(patch.name === null ? "removed its name" : `named it ${patch.name}`);
  }
  if (patch.description !== undefined) {
    parts.push(patch.description === null ? "removed its description" : "gave it a new description");
  }
  return parts.length === 0 ? undefined : parts.join(" and ");
}

/** `update_workflow`'s result: the workflow's id, its name, and its version, new or unchanged. */
export function describeUpdate(workflow: Workflow, patch: PatchWorkflow, updated: Updated, limits: Limits): string {
  const label = workflowLabel(workflow);
  const version = workflow.version;
  const lines: string[] = [];
  const patched = describePatch(patch);
  if (patched !== undefined) {
    lines.push(`Updated the workflow ${label}: ${patched}.`);
  }
  switch (updated.kind) {
    case "patched":
      lines.push(`Its version stays v${latestVersion(workflow).version}.`);
      break;
    case "version":
      lines.push(
        updated.made
          ? `Made the version v${version.version} (digest ${shortDigest(version.digest)}) of the workflow ${label}: its suite, ${plural((version.tests ?? []).length, "test")}, passed against it.`
          : `The workflow ${label} already has this content as its version v${version.version}: no version was made.`,
      );
      break;
    case "tests":
      lines.push(
        `Changed the suite of the workflow ${label}: it holds ${plural(workflow.suite.filter((entry) => !entry.retiredAt).length, "test")}, each passing against its version v${latestVersion(workflow).version}, which stays its latest. Its next version keeps them.`,
      );
      break;
  }
  const prompt = version.declarations?.prompt.trim() ?? "";
  if (prompt !== "") {
    lines.push(
      `Code starts it from "rewind:workflows" once an action set pins it: ${truncateEnd(prompt, limits.value)}`,
    );
  }
  lines.push(`Run it with run_workflow(${args({ workflow: workflowReference(workflow) })}).`);
  return lines.join("\n");
}

/** The first 12 digits of a digest. */
function shortDigest(digest: string): string {
  return digest.slice(0, 12);
}

/** A test of a suite as `get_workflow` reads it: the test, or why it cannot be read. */
export interface ReadTest {
  name: string;
  digest: string;
  test?: Test | undefined;
  unreadable?: string | undefined;
}

/** What `get_workflow` reads: the workflow, and each test of its suite. */
export interface WorkflowDetails {
  workflow: Workflow;
  /** The tests of its suite, in its order. */
  tests: ReadTest[];
  /** `update_workflow` is one of the tools. */
  updates: boolean;
}

/** The most versions `get_workflow` lists. */
const LISTED_VERSIONS = 20;

/**
 * A workflow, for the model to repair or extend it: who it is, its version's code and interface, its suite, the tests
 * that left it, and its versions.
 */
export function describeWorkflowDetails(details: WorkflowDetails, limits: Limits): string {
  const { workflow } = details;
  const { version, versions } = workflow;
  const scope = workflow.namespace
    ? `the namespace ${workflow.namespace}`
    : workflow.promotedAt
      ? `the organization (promoted ${workflow.promotedAt})`
      : "the organization";
  const lines = [`The workflow ${workflowLabel(workflow)}, of ${scope}.`];
  if (workflow.source) {
    lines.push(
      `It is a fork of the organization's workflow ${workflow.source.workflow}, whose version v${workflow.source.version} it last took.`,
    );
  }
  const description = workflow.description ?? latestVersion(workflow).comment?.description ?? undefined;
  if (description) {
    lines.push(`Description: ${truncateEnd(description, limits.value)}`);
  }
  const from = version.base ? `, from ${versionRef(version.base, versions)}` : "";
  lines.push(
    `Its version v${version.version} of ${plural(Math.max(1, versions.length), "version")} (digest ${shortDigest(version.digest)}), made ${version.createdAt}${from}.`,
  );
  lines.push(`Action set: ${version.actionSet}`);
  const absent = (schema: unknown) => schema === undefined || schema === null;
  if (absent(version.input) && absent(version.output) && absent(version.signals)) {
    lines.push("Interface: none. It takes any input, returns any output and accepts any signal name.");
  } else {
    const schema = (value: unknown) => truncateEnd(json(absent(value) ? true : value), limits.value);
    lines.push(`Input schema: ${schema(version.input)}`);
    lines.push(`Output schema: ${schema(version.output)}`);
    const signals = Object.entries(version.signals ?? {});
    if (signals.length === 0) {
      lines.push("Signals: none.");
    } else {
      lines.push("Signals:");
      for (const [name, signal] of signals) {
        lines.push(`- ${quoted(name)}: ${truncateEnd(json(signal), limits.value)}`);
      }
    }
  }
  if (Object.keys(version.secrets ?? {}).length > 0) {
    lines.push(`Secrets its runs use: ${json(version.secrets)}`);
  }
  const prompt = version.declarations?.prompt.trim() ?? "";
  if (prompt !== "") {
    lines.push(
      `Code starts it from "rewind:workflows" once an action set pins it: ${truncateEnd(prompt, limits.value)}`,
    );
  }
  lines.push("Code:", truncateMiddle(workflow.code.trimEnd(), limits.transcript));

  if (details.tests.length === 0) {
    lines.push("Its suite is empty: it has no test.");
  } else {
    const reuse = details.updates
      ? "give a digest in update_workflow's `tests` or `replace` to reuse a test"
      : "give a digest in test_workflow's `tests` to run a test against a code";
    lines.push(`Its suite, ${plural(details.tests.length, "test")}, which its next version must pass (${reuse}):`);
    const room = Math.max(limits.value, Math.floor(limits.output / details.tests.length));
    for (const entry of details.tests) {
      const head = `Test ${quoted(truncateEnd(entry.name, limits.line))} (${entry.digest})`;
      if (entry.test === undefined) {
        lines.push(`${head}: it cannot be read: ${truncateEnd(entry.unreadable ?? "", limits.value)}`);
        continue;
      }
      const origin = entry.test.from
        ? `, recorded from run ${entry.test.from.run} of ${versionRef(entry.test.from.version, versions)}`
        : "";
      lines.push(`${head}${origin}:`, truncateEnd(json(readableTest(entry.test)), room));
    }
  }

  const left = workflow.suite.filter((entry) => entry.retiredAt);
  if (left.length > 0) {
    lines.push("Tests that left its suite, and why:");
    for (const entry of left) {
      lines.push(
        `- ${quoted(truncateEnd(entry.name, limits.line))} (${shortDigest(entry.digest)}), left ${entry.retiredAt}: ${truncateEnd(entry.reason ?? "no reason given", limits.value)}`,
      );
    }
  }

  lines.push("Its versions, newest first:");
  for (const listed of versions.slice(0, LISTED_VERSIONS)) {
    const from = listed.base ? `, from ${versionRef(listed.base, versions)}` : ", the first version";
    const changes = [
      ...(listed.replaced ?? []).map((replaced) => `replaced ${quoted(truncateEnd(replaced.name, limits.line))}`),
      ...(listed.retired ?? []).map((retired) => `retired ${quoted(truncateEnd(retired.name, limits.line))}`),
    ];
    lines.push(
      `- v${listed.version}, made ${listed.createdAt}${from}: ${plural((listed.tests ?? []).length, "test")}${changes.length === 0 ? "" : `; ${changes.join(", ")}`}.`,
    );
  }
  if (versions.length > LISTED_VERSIONS) {
    lines.push(`… and ${plural(versions.length - LISTED_VERSIONS, "version")} older.`);
  }
  return lines.join("\n");
}

/** `v3` when the digest is one of `versions`, otherwise its first 12 digits. */
function versionRef(digest: string, versions: WorkflowVersion[]): string {
  const known = versions.find((candidate) => candidate.digest === digest);
  return known ? `v${known.version}` : shortDigest(digest);
}

/**
 * A test as the model reads it, its fields in the order the server writes them: its cassette summarized, since a
 * recorded run's calls are long and rarely useful.
 */
function readableTest(test: Test): Test {
  const cassette = test.world?.cassette;
  if (cassette === undefined || cassette === null) {
    return test;
  }
  return { ...test, world: { ...test.world, cassette: { summary: cassetteSummary(cassette) } } };
}

/** `12 recorded calls matched by_args (github.list_issues ×3, github.close_issue ×9), 1 signal (approval)`. */
export function cassetteSummary(cassette: Record<string, unknown>): string {
  const calls = Array.isArray(cassette["calls"]) ? (cassette["calls"] as unknown[]) : [];
  const signals = Array.isArray(cassette["signals"]) ? (cassette["signals"] as unknown[]) : [];
  const matching = typeof cassette["matching"] === "string" ? ` matched ${cassette["matching"]}` : "";
  // In the order each action is first called.
  const byAction = new Map<string, number>();
  for (const call of calls) {
    const action = isRecord(call) && typeof call["action"] === "string" ? call["action"] : "?";
    byAction.set(action, (byAction.get(action) ?? 0) + 1);
  }
  const actions = [...byAction].map(([action, count]) => `${action} ×${count}`).join(", ");
  const names = [...new Set(signals.map((s) => (isRecord(s) && typeof s["name"] === "string" ? s["name"] : "?")))];
  return [
    `${plural(calls.length, "recorded call")}${matching}${actions === "" ? "" : ` (${actions})`}`,
    `${plural(signals.length, "signal")}${names.length === 0 ? "" : ` (${names.join(", ")})`}`,
  ].join(", ");
}

/** The tests a recording stored, and the runs it left out. `updates` when `update_workflow` is one of the tools. */
export function describeRecorded(workflow: string, recorded: RecordedTests, updates: boolean, limits: Limits): string {
  const lines: string[] = [];
  if (recorded.tests.length === 0) {
    lines.push(`No test was recorded: no completed or failed run of ${workflow} was selected.`);
  } else {
    lines.push(`Recorded ${plural(recorded.tests.length, "test")} from the runs of ${workflow}, one per path:`);
    for (const test of recorded.tests) {
      lines.push(
        `- ${test.digest} ${quoted(test.name)}, from run ${test.run} (${test.status}): ${truncateEnd(test.path, limits.value)}`,
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
      updates
        ? `They join no suite yet: pass their digests in update_workflow's \`tests\` to add them to the suite of ${workflow}, or { "test": "<digest>", "expect": {…} } to give a failed run the expectation it should have met.`
        : 'They join no suite: pass their digests in test_workflow\'s `tests` to run them against a code, or { "test": "<digest>", "expect": {…} } to give a failed run the expectation it should have met.',
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
    const parts = [`${run.handle}: ${run.status}${run.error ? ` (${run.error})` : ""}`];
    if (run.quarantined) {
      parts.push("quarantined");
    }
    parts.push(`started ${run.startedAt}`);
    parts.push(plural(run.calls, "call"));
    if (run.errors > 0) {
      parts.push(`${run.errors} failed`);
    }
    if (run.source) {
      parts.push(`a fork of ${run.source}`);
    }
    if (run.workflow) {
      parts.push(`a run of the workflow ${runWorkflowReference(run.workflow)}`);
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

/** The actions matching a search. */
export function describeMatches(query: string, matches: ActionMatch[]): string {
  if (matches.length === 0) {
    return `No action matches ${quoted(query)}.`;
  }
  return [
    `${plural(matches.length, "action")} ${matches.length === 1 ? "matches" : "match"} ${quoted(query)}:`,
    ...matches.map((m) => m.dts.trimEnd()),
  ].join("\n\n");
}

/**
 * `list_secrets`: the secrets the caller may use, by name, never a value: the organization's with their labels, then
 * the namespace's.
 */
export function describeSecrets(
  organization: readonly ListedSecretView[],
  ofNamespace: readonly ListedSecretView[] | undefined,
): string {
  const lines: string[] = [];
  for (const secret of organization) {
    const labels = Object.entries(secret.labels ?? {})
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .map(([key, value]) => `${key}=${value}`);
    lines.push(
      `- ${secret.reference}, the organization's${labels.length === 0 ? "" : ` (labels ${labels.join(", ")})`}`,
    );
  }
  for (const secret of ofNamespace ?? []) {
    lines.push(`- ${secret.reference}, the end user's in the namespace ${secret.namespace ?? ""}`);
  }
  if (lines.length === 0) {
    return "No secret you may use exists yet: a person puts one first, the end user's in their namespace or the organization's.";
  }
  return `The secrets you may use, never a value:\n${lines.join("\n")}\nName one in \`secrets\`, by the MCP server or API whose calls need it, such as {"github": "ns/github"}.`;
}

/** What `describeSecrets` reads of a secret. */
export interface ListedSecretView {
  reference: string;
  namespace?: string | null | undefined;
  labels?: Record<string, string> | undefined;
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
    case "queued":
      return "never started";
    case "running":
    case "waiting":
      return "was still waiting at the end of the test";
  }
}

function payloadText(payload: Payload, limit: number): string {
  return "value" in payload ? truncateEnd(json(payload.value), limit) : `${payload.preview}… (${payload.bytes} bytes)`;
}

function at(callsite: Callsite | null | undefined): string {
  return callsite ? ` at workflow.ts:${callsite.line}:${callsite.column}` : "";
}

/** A tool call as the model would write it: `JSON.stringify` of an object literal, its keys in order. */
export function args(values: Record<string, unknown>): string {
  return JSON.stringify(values);
}

/** `JSON.stringify(value)`: compact. */
export function json(value: unknown): string {
  return JSON.stringify(value === undefined ? null : value) ?? "null";
}

/** `JSON.stringify` of a string. */
export function quoted(text: string): string {
  return JSON.stringify(text);
}

export function plural(count: number, noun: string): string {
  return `${count} ${noun}${count === 1 ? "" : "s"}`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
