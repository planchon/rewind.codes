import { createHash } from "node:crypto";
import { setFlagsFromString } from "node:v8";
import { runInNewContext } from "node:vm";

import type { ActionSet, HandedCall, Payload, Run, Workflow, WorkflowVersion } from "../src/types.js";

export const RUN_ID = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10";
/** The handle of the run `RUN_ID`. */
export const HANDLE = "user_2fQ81/triage/06bqx3k9f2mt";
export const ACTION_SET = "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4";
/** The end user the runs and calls of the tests belong to. */
export const NAMESPACE = "user_2fQ81";
/** The id of the workflow `triage`. */
export const WORKFLOW_ID = "wf_7k2m9q4x8d1c";
/** The digest of the version 3 of the workflow `triage`. */
export const DIGEST = "4be1c07a9d3e" + "0".repeat(52);

/** A payload as the API shows it; `inline: false` leaves the value out, as above 256 KiB. */
export function payload(value: unknown, inline = true): Payload {
  const text = JSON.stringify(value);
  return {
    digest: createHash("sha256").update(text).digest("hex"),
    bytes: Buffer.byteLength(text),
    preview: text.slice(0, 64),
    ...(inline ? { value } : {}),
  };
}

export function run(overrides: Partial<Run> = {}): Run {
  return {
    id: RUN_ID,
    handle: HANDLE,
    namespace: NAMESPACE,
    status: "completed",
    createdAt: "2026-11-12T10:00:00Z",
    updatedAt: "2026-11-12T10:00:01Z",
    expiresAt: "2026-12-12T10:00:00Z",
    epoch: 1,
    historyLen: 4,
    input: payload({}),
    labels: { conversation: "c-42" },
    mode: "live",
    codeDigest: "ab".repeat(32),
    actionSet: ACTION_SET,
    traceId: "4bf92f3577b34da6a3ce929d0e0e4736",
    ...overrides,
  };
}

export function call(overrides: Partial<HandedCall> = {}): HandedCall {
  return {
    id: "call-1",
    token: "tok-1",
    action: "ask_user",
    args: { question: "Proceed?" },
    delivery: 1,
    seq: 3,
    runId: RUN_ID,
    namespace: NAMESPACE,
    idempotencyKey: `${RUN_ID}/3`,
    labels: { conversation: "c-42" },
    traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    ...overrides,
  };
}

/** A version of the workflow `triage`: its version 3, with an interface. */
export function version(overrides: Partial<WorkflowVersion> = {}): WorkflowVersion {
  return {
    version: 3,
    digest: DIGEST,
    codeDigest: "cd".repeat(32),
    actionSet: ACTION_SET,
    input: { type: "object", properties: { repo: { type: "string" } }, required: ["repo"] },
    output: { type: "object", properties: { closed: { type: "integer" } }, required: ["closed"] },
    signals: { approval: { type: "object", properties: { close: { type: "boolean" } }, required: ["close"] } },
    labels: { conversation: "c-42" },
    createdAt: "2026-11-12T10:00:00Z",
    tests: [],
    declarations: {
      dts: 'declare module "rewind:workflows" {\n  export const triage: Workflow<{ repo: string }, { closed: number }, { approval: { close: boolean } }>;\n}',
      prompt:
        'triage.start({ repo: string }) → run · run.signal("approval", { close: boolean }) · run.result() → { closed: number } · Triage the stale issues of a repository, after approval.',
    },
    ...overrides,
  };
}

/** The workflow `triage`, the organization's, shown at its latest version. */
export function workflow(overrides: Partial<Workflow> = {}): Workflow {
  const latest = overrides.version ?? version();
  return {
    id: WORKFLOW_ID,
    shortId: "wf-7k2m9q",
    name: "triage",
    description: "Triage the stale issues of a repository, after approval.",
    labels: {},
    createdAt: "2026-11-12T10:00:00Z",
    code: "export default async function main() {}",
    suite: [],
    version: latest,
    versions: [latest],
    ...overrides,
  };
}

/** An action set as the API answers it, with the workflows it pins. */
export function actionSet(id: string, workflows: Workflow[] = []): ActionSet {
  return {
    id,
    declarations: { dts: "declare module 'rewind:actions' {}", prompt: "github.list_issues({ repo }) → Issue[]" },
    servers: [{ name: "github", actions: 1, transport: "http" }],
    actions: [],
    workflows: workflows.map((w) => ({
      name: w.name ?? w.id,
      id: w.id,
      version: w.version.version,
      digest: w.version.digest,
      description: w.description ?? null,
    })),
  };
}

/** Waits until `condition` holds, checking every few milliseconds. */
export async function until(condition: () => boolean, timeoutMs = 2_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!condition()) {
    if (Date.now() > deadline) {
      throw new Error("the condition did not hold in time");
    }
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

/**
 * A full garbage collection, as memory pressure brings one: what only a weak reference holds is gone after it.
 */
export function collectGarbage(): void {
  setFlagsFromString("--expose-gc");
  const gc = runInNewContext("gc") as () => void;
  gc();
  gc();
}
