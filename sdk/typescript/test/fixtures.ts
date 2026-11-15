import { createHash } from "node:crypto";

import type { Payload, Run, Task, Toolset, Workflow } from "../src/types.js";

export const RUN_ID = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10";
export const TOOLSET = "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4";
/** The end user the runs and tasks of the tests belong to. */
export const NAMESPACE = "user_2fQ81";
/** The version of the workflow `triage`. */
export const VERSION = "4be1c07a9d3e" + "0".repeat(52);

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
    script: "ab".repeat(32),
    toolset: TOOLSET,
    traceId: "4bf92f3577b34da6a3ce929d0e0e4736",
    ...overrides,
  };
}

export function task(overrides: Partial<Task> = {}): Task {
  return {
    id: "task-1",
    token: "tok-1",
    tool: "ask_user",
    args: { question: "Proceed?" },
    attempt: 1,
    seq: 3,
    runId: RUN_ID,
    namespace: NAMESPACE,
    idempotencyKey: `${RUN_ID}/3`,
    labels: { conversation: "c-42" },
    heartbeatTimeoutMs: 60_000,
    traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
    ...overrides,
  };
}

export function workflow(overrides: Partial<Workflow> = {}): Workflow {
  return {
    name: "triage",
    version: VERSION,
    description: "Triage the stale issues of a repository, after approval.",
    toolset: TOOLSET,
    script: "cd".repeat(32),
    input: { type: "object", properties: { repo: { type: "string" } }, required: ["repo"] },
    output: { type: "object", properties: { closed: { type: "integer" } }, required: ["closed"] },
    signals: { approval: { type: "object", properties: { close: { type: "boolean" } }, required: ["close"] } },
    labels: { conversation: "c-42" },
    createdAt: "2026-11-12T10:00:00Z",
    declarations: {
      dts: 'declare module "rewind:workflows" {\n  export const triage: Workflow<{ repo: string }, { closed: number }, { approval: { close: boolean } }>;\n}',
      prompt:
        'triage.start({ repo: string }) → run · run.signal("approval", { close: boolean }) · run.result() → { closed: number } · Triage the stale issues of a repository, after approval.',
    },
    ...overrides,
  };
}

/** A toolset as the API answers it, with the workflows it pins. */
export function toolset(id: string, workflows: Workflow[] = []): Toolset {
  return {
    id,
    declarations: { dts: "declare module 'rewind:tools' {}", prompt: "github.list_issues({ repo }) → Issue[]" },
    servers: [{ name: "github", tools: 1, transport: "http" }],
    tools: [],
    workflows: workflows.map((w) => ({ name: w.name, version: w.version, description: w.description ?? null })),
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
