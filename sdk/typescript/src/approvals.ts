import { PtcError } from "./errors.js";
import type { Payloads } from "./resources/admin.js";
import type { Runs } from "./resources/runs.js";
import type { Tasks } from "./resources/tasks.js";
import type { Callsite, ParkedReason, Run, Waiting } from "./types.js";

/**
 * A tool call that waits for the harness: a call under the `approval` policy, or a call parked
 * because the tool's schema changed during the run. The harness approves it (the call goes on)
 * or rejects it (the script gets a `ToolError`).
 *
 * A call parked by `credentials_expired` is not one: renew the credentials instead.
 */
export interface PendingApproval {
  readonly runId: string;
  /** For `approve` and `reject`. */
  readonly taskId: string;
  readonly tool: string;
  /** The call's position in the run. */
  readonly seq: number;
  readonly attempt: number;
  readonly callsite: Callsite | undefined;
  /** `approval`: the tool's policy asks for consent. `schema_changed`: the tool changed since it was pinned. */
  readonly reason: "approval" | "schema_changed";
}

/**
 * The harness's permission gates. A run that waits for one is suspended; its state
 * (`Run.waitingFor`, also in the `run.suspended` webhook) says which call waits.
 */
export class Approvals {
  constructor(
    private readonly runs: Runs,
    private readonly tasks: Tasks,
    private readonly payloads: Payloads,
  ) {}

  /** The calls of a run that wait for the harness's decision. */
  pending(run: Run): PendingApproval[] {
    return pendingApprovals(run);
  }

  /** The call goes on: it is sent, or, after a schema change, continues against the new schema. */
  approve(approval: PendingApproval | string, options: { signal?: AbortSignal } = {}): Promise<void> {
    return this.tasks.approve(taskIdOf(approval), options);
  }

  /** The call fails: the script gets a `ToolError` with this message, written for the model. */
  reject(approval: PendingApproval | string, message?: string, options: { signal?: AbortSignal } = {}): Promise<void> {
    return this.tasks.reject(taskIdOf(approval), { ...options, message });
  }

  /**
   * The arguments of the waiting call, to show the person who decides. They are read from the
   * run's journal (the `schedule_tool_call` entry of the call).
   */
  async args(approval: PendingApproval, options: { signal?: AbortSignal } = {}): Promise<unknown> {
    for await (const event of this.runs.events(approval.runId, { signal: options.signal })) {
      if (event.type === "entry" && event.event.type === "schedule_tool_call" && event.event.seq === approval.seq) {
        const { args } = event.event;
        return "value" in args ? args.value : this.payloads.get(args.digest, options);
      }
    }
    throw new PtcError(`run ${approval.runId} has no call ${approval.seq} in its journal`);
  }
}

/** The calls of a run that wait for the harness's decision. */
export function pendingApprovals(run: Run): PendingApproval[] {
  const pending: PendingApproval[] = [];
  for (const waiting of run.waitingFor ?? []) {
    const reason = approvalReason(waiting);
    if (waiting.kind === "tool_call" && reason !== undefined) {
      pending.push({
        runId: run.id,
        taskId: waiting.taskId,
        tool: waiting.tool,
        seq: waiting.seq,
        attempt: waiting.attempt,
        callsite: waiting.callsite ?? undefined,
        reason,
      });
    }
  }
  return pending;
}

function approvalReason(waiting: Waiting): PendingApproval["reason"] | undefined {
  if (waiting.kind !== "tool_call") {
    return undefined;
  }
  if (waiting.state === "approval") {
    return "approval";
  }
  const parked: ParkedReason | null | undefined = waiting.parked;
  return waiting.state === "parked" && parked === "schema_changed" ? "schema_changed" : undefined;
}

function taskIdOf(approval: PendingApproval | string): string {
  return typeof approval === "string" ? approval : approval.taskId;
}
