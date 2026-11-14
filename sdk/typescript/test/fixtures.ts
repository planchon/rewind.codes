import { createHash } from "node:crypto";

import type { Payload, Run, Task } from "../src/types.js";

export const RUN_ID = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10";
export const TOOLSET = "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4";
/** The end user the runs and tasks of the tests belong to. */
export const NAMESPACE = "user_2fQ81";

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
