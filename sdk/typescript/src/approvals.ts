import type { Payloads } from "./resources/admin.js";
import type { Calls } from "./resources/calls.js";
import type { Callsite, ParkedReason, Payload, Run, Waiting } from "./types.js";

/**
 * A call that waits for the harness: a call of an action under the `approval` policy, or a call parked because the
 * action's schema changed during the run. The harness approves it (the call goes on) or rejects it (the code gets an
 * `ActionError`).
 *
 * A call parked by `credentials_refused` or `secret_missing` is not one: put the secret it uses again
 * (`secrets.put`, `namespaces.secrets.put`), which sends it again. A call whose signature was refused is approved
 * with `calls.approve` once its server accepts the organization's key.
 */
export interface PendingApproval {
  readonly runId: string;
  /** The run's handle, `<namespace>/<origin>/<suffix>`, to show the person who decides. */
  readonly runHandle: string;
  /** For `approve` and `reject`. */
  readonly callId: string;
  /** The action called. */
  readonly action: string;
  /** The call's position in the run. */
  readonly seq: number;
  /** The delivery the call waits in, from 1. */
  readonly delivery: number;
  readonly callsite: Callsite | undefined;
  /** The call's arguments; their value is left out above 256 KiB: see `Approvals.args`. */
  readonly payload: Payload;
  /** `approval`: the action's policy asks for consent. `schema_changed`: the action changed since it was pinned. */
  readonly reason: "approval" | "schema_changed";
}

/**
 * The harness's permission gates. A run that waits for one is `waiting`; its state (`Run.waitingFor`, also in the
 * `run.waiting` webhook) says which call waits.
 */
export class Approvals {
  constructor(
    private readonly calls: Calls,
    private readonly payloads: Payloads,
  ) {}

  /** The calls of a run that wait for the harness's decision. */
  pending(run: Run): PendingApproval[] {
    return pendingApprovals(run);
  }

  /** The call goes on: it is sent, or, after a schema change, continues against the new schema. */
  approve(approval: PendingApproval | string, options: { signal?: AbortSignal } = {}): Promise<void> {
    return this.calls.approve(callIdOf(approval), options);
  }

  /** The call fails: the code gets an `ActionError` with this message, written for the model. */
  reject(approval: PendingApproval | string, message?: string, options: { signal?: AbortSignal } = {}): Promise<void> {
    return this.calls.reject(callIdOf(approval), { ...options, message });
  }

  /**
   * The arguments of the waiting call, to show the person who decides: from the run's state, or
   * read by their digest when they are too large to be in it.
   */
  async args(approval: PendingApproval, options: { signal?: AbortSignal } = {}): Promise<unknown> {
    const { payload } = approval;
    return "value" in payload ? payload.value : this.payloads.get(payload.digest, { ...options, run: approval.runId });
  }
}

/** The calls of a run that wait for the harness's decision. */
export function pendingApprovals(run: Run): PendingApproval[] {
  const pending: PendingApproval[] = [];
  for (const waiting of run.waitingFor ?? []) {
    if (waiting.kind !== "call") {
      continue;
    }
    const reason = approvalReason(waiting);
    if (reason !== undefined) {
      pending.push({
        runId: run.id,
        runHandle: run.handle,
        callId: waiting.callId,
        action: waiting.action,
        seq: waiting.seq,
        delivery: waiting.delivery,
        callsite: waiting.callsite ?? undefined,
        payload: waiting.args,
        reason,
      });
    }
  }
  return pending;
}

function approvalReason(waiting: Extract<Waiting, { kind: "call" }>): PendingApproval["reason"] | undefined {
  if (waiting.state === "approval") {
    return "approval";
  }
  const parked: ParkedReason | null | undefined = waiting.parked;
  return waiting.state === "parked" && parked === "schema_changed" ? "schema_changed" : undefined;
}

function callIdOf(approval: PendingApproval | string): string {
  return typeof approval === "string" ? approval : approval.callId;
}
