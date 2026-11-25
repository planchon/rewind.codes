/**
 * Harness actions and their calls (`archi/04-tools.md`, "Call lifecycle", "Policies"), on setup A:
 * the SDK's worker, and call streams opened by hand; the open stream as the liveness of its
 * calls, a stream's slots, a worker coming back within its reconnect timeout, a stream that does
 * not come back, `gone`, deferrals, failures, redeliveries, timeouts, approvals; arguments checked
 * against the schema before the harness sees them; and the replays a hostile worker could try with
 * a token or an approval already used.
 *
 * How long a call handed on a closed stream waits for its worker is the action's policy
 * `reconnectTimeout` (archi/04's "Policies", 60 s by default).
 */
import { type HarnessActionContext, HarnessActionError, type Worker } from "@rewind.codes/sdk";
import { setTimeout as delay } from "node:timers/promises";
import { evidence, expect, expectOk, expectProblem, expectRefused, newOrg, type Org, test, unique } from "../support/gate.js";
import { closeCallStreams, ended, handedWithin, handOne, harnessAction, runUntil, startRun, CallStream, waitingCall } from "../support/actions-helpers.js";

let org: Org;

test.beforeAll(async () => {
  org = await newOrg("harness-actions");
});

// A stream a test left open would keep its calls: each test's streams close with it.
test.afterEach(async () => {
  await closeCallStreams();
});

/** An action set of one harness action `<group>.lookup`, with a group of its own so tests never share calls. */
async function lookupActionSet(policy: Record<string, unknown> = {}) {
  const group = `g${unique()}`;
  const action = `${group}.lookup`;
  const created = await org.api.post("/v1/action-sets", { harness: [harnessAction(action, policy)] });
  expectOk(created);
  return { actionSet: created.json.id as string, group, action };
}

/** Code calling `<group>.lookup({ id })` once; `body` handles the call's promise `call`. */
function lookupCode(group: string, body = "return await call;"): string {
  return `import { ${group} } from "rewind:actions";
import { ActionError } from "rewind";

export default async function main(input: { id: string }) {
  const call = ${group}.lookup({ id: input.id });
  ${body}
}
`;
}

/** The code catches the ActionError and returns what it saw. */
const CATCHING = `try {
    return { ok: await call };
  } catch (error) {
    if (!(error instanceof ActionError)) throw error;
    return { name: error.name, message: error.message, action: error.action, data: error.data ?? null, outcomeUnknown: error.outcomeUnknown };
  }`;

type Seen = { args: any; ctx: Pick<HarnessActionContext, "runId" | "namespace" | "seq" | "delivery" | "idempotencyKey" | "labels" | "traceparent"> };

function serve(actionSet: string, action: string, handler: (args: any, ctx: HarnessActionContext, seen: Seen[]) => unknown): { worker: Worker; seen: Seen[] } {
  const seen: Seen[] = [];
  const worker = org.rewind.worker({
    actionSet,
    actions: {
      [action]: async (args, ctx) => {
        seen.push({
          args,
          ctx: {
            runId: ctx.runId,
            namespace: ctx.namespace,
            seq: ctx.seq,
            delivery: ctx.delivery,
            idempotencyKey: ctx.idempotencyKey,
            labels: ctx.labels,
            traceparent: ctx.traceparent,
          },
        });
        return handler(args, ctx, seen);
      },
    },
    onError: () => undefined,
  });
  return { worker, seen };
}

const alice = () => org.api.ns("alice");

// ---------------------------------------------------------------------------------------------
// Success

test("a harness action served by the SDK's worker is called once, with its run's context", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const { worker, seen } = serve(actionSet, action, ({ id }) => ({ id, name: "Ada Lovelace" }));
  try {
    const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-1" }, labels: { suite: "harness-actions" }, wait: "30s" });
    const done = run.status === "completed" ? run : await ended(alice(), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    expect(done.output.value).toEqual({ id: "c-1", name: "Ada Lovelace" });
    expect(seen).toHaveLength(1);
    const ctx = seen[0]!.ctx;
    expect(ctx).toMatchObject({ runId: run.id, namespace: "alice", seq: 1, delivery: 1, idempotencyKey: `${run.id}/1`, labels: { suite: "harness-actions" } });
    // The call span is the parent of the handler's work, in the run's trace.
    expect(ctx.traceparent?.split("-")[1]).toBe(done.traceId);
    evidence(`called once with ${ctx.idempotencyKey} in namespace ${ctx.namespace}`);
  } finally {
    await worker.stop();
  }
});

test("an open stream keeps its call while the handler works, past the reconnect timeout, with nothing sent meanwhile", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet({ reconnectTimeout: "1s" });
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-2" } });
  const { call, stream } = await handOne(org.api, actionSet, action, run.id);
  expect(call).toMatchObject({ runId: run.id, namespace: "alice", seq: 1, delivery: 1, idempotencyKey: `${run.id}/1` });
  expect(call.token).toEqual(expect.any(String));
  // Three times the reconnect timeout, and the stream still open: the call is still this worker's.
  await delay(3000);
  expectOk(await org.api.post(`/v1/calls/${call.token}/complete`, { result: { id: "c-2", name: "kept" } }));
  const done = await ended(alice(), run.id);
  expect(done.status).toBe("completed");
  expect(done.output.value).toEqual({ id: "c-2", name: "kept" });
  expect(done.deliveries ?? []).toEqual([]);
  expect(stream.handed.filter((t) => t.runId === run.id)).toHaveLength(1);
  evidence("a call held 3 s on an open stream (reconnect timeout 1 s), answered by its token: delivery 1, handed once");
});

test("a stream holds at most its slots: the next call is handed once a slot is free", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const stream = await CallStream.open(org.api, { actionSet, actions: [action], slots: 1 });
  const first = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-2a" } });
  const second = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-2b" } });
  const handed = await stream.callOf(action);
  await delay(1500);
  expect(stream.handed, "one slot, one call in flight").toHaveLength(1);
  expectOk(await org.api.post(`/v1/calls/${handed.token}/complete`, { result: { id: handed.args.id, name: "first" } }));
  const next = await stream.call("the other run's call", (t) => t.runId !== handed.runId);
  expect([first.id, second.id].sort()).toEqual([handed.runId, next.runId].sort());
  expectOk(await org.api.post(`/v1/calls/${next.token}/complete`, { result: { id: next.args.id, name: "second" } }));
  for (const run of [first, second]) expect((await ended(alice(), run.id)).status).toBe("completed");
  evidence("slots=1: the second call was handed only once the first was answered");
});

test("a deferred call is answered later, by its token, from another key, after its stream closed", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet({ reconnectTimeout: "1s" });
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-3" } });
  const { call, stream } = await handOne(org.api, actionSet, action, run.id);
  expectOk(await org.api.post(`/v1/calls/${call.token}/defer`, { timeout: "5m" }));
  // Deferred, the call does not need its stream: it waits past the reconnect timeout for its answer.
  await stream.close();
  await delay(2000);
  const wait = await waitingCall(alice(), run.id, "deferred");
  expect(wait.action).toBe(action);
  // Another process of the harness, with a key of role `worker`, answers it.
  const workerKey = await org.key("worker");
  expectOk(await org.api.as(workerKey.key).post(`/v1/calls/${call.token}/complete`, { result: { id: "c-3", name: "answered later" } }));
  const done = await ended(alice(), run.id);
  expect(done.status).toBe("completed");
  expect(done.output.value).toEqual({ id: "c-3", name: "answered later" });
  expect(done.deliveries ?? []).toEqual([]);
});

test("a worker that opens its stream again within the reconnect timeout keeps its call: it is not handed again", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet({ reconnectTimeout: "10s" });
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-3b" } });
  const { call, stream } = await handOne(org.api, actionSet, action, run.id);
  // The connection drops; the worker comes back with its id within the reconnect timeout.
  await stream.close();
  const again = await stream.reopen();
  await delay(1500);
  expect(again.handed, "the call kept is not handed again").toEqual([]);
  expectOk(await org.api.post(`/v1/calls/${call.token}/complete`, { result: { id: "c-3b", name: "after the drop" } }));
  const done = await ended(alice(), run.id);
  expect(done.output.value).toEqual({ id: "c-3b", name: "after the drop" });
  expect(done.deliveries ?? []).toEqual([]);
  evidence(`the stream of ${stream.query.worker} dropped and came back within the reconnect timeout: its token answered, delivery 1`);
});

test("a handler's failure reaches the code as an ActionError with its message and data", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const { worker, seen } = serve(actionSet, action, () => {
    throw new HarnessActionError("card declined", { data: { code: 51 } });
  });
  try {
    const run = await startRun(alice(), { actionSet, code: lookupCode(group, CATCHING), input: { id: "c-4" }, wait: "30s" });
    const done = await ended(alice(), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    expect(done.output.value).toMatchObject({ name: "ActionError", action, data: { code: 51 }, outcomeUnknown: false });
    expect(done.output.value.message).toContain("card declined");
    expect(seen).toHaveLength(1);
  } finally {
    await worker.stop();
  }
});

test("a retryable failure is delivered again with the same idempotency key, and the deliveries are counted until the run ends", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  let release: () => void = () => undefined;
  const released = new Promise<void>((resolve) => (release = resolve));
  const { worker, seen } = serve(actionSet, action, async ({ id }, ctx) => {
    if (ctx.delivery === 1) throw new HarnessActionError("upstream busy", { retryable: true });
    await released;
    return { id, name: "second try" };
  });
  try {
    const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-5" } });
    // The deliveries live in the run's calls: shown while the second delivery is in flight.
    await expect.poll(async () => (await alice().get(`/v1/runs/${run.id}`)).json.deliveries, { timeout: 30_000 }).toEqual([{ seq: 1, deliveries: 2 }]);
    release();
    const done = await ended(alice(), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    expect(done.output.value).toEqual({ id: "c-5", name: "second try" });
    expect(seen.map((s) => s.ctx.delivery)).toEqual([1, 2]);
    expect(new Set(seen.map((s) => s.ctx.idempotencyKey))).toEqual(new Set([`${run.id}/1`]));
    expect(done.deliveries ?? [], "the run's end clears its calls, and their deliveries with them").toEqual([]);
    evidence("delivery 1 failed retryable, delivery 2 with the same key; deliveries [{seq: 1, deliveries: 2}] while the call was in flight, none once the run ended");
  } finally {
    release();
    await worker.stop();
  }
});

test("a call under the approval policy waits for the harness, then runs once approved", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet({ approval: true });
  const { worker, seen } = serve(actionSet, action, ({ id }) => ({ id, name: "approved" }));
  try {
    const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-6" } });
    const wait = await waitingCall(alice(), run.id, "approval");
    expect(wait.args.value).toEqual({ id: "c-6" });
    await delay(1000);
    expect(seen, "nothing is sent before the approval").toHaveLength(0);
    expectOk(await org.api.post(`/v1/calls/${wait.callId}/approve`));
    const done = await ended(alice(), run.id);
    expect(done.status).toBe("completed");
    expect(done.output.value).toEqual({ id: "c-6", name: "approved" });
    expect(seen).toHaveLength(1);
  } finally {
    await worker.stop();
  }
});

test("a rejected call fails with an ActionError carrying the harness's message, and is never sent", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet({ approval: true });
  const { worker, seen } = serve(actionSet, action, () => ({ never: true }));
  try {
    const run = await startRun(alice(), { actionSet, code: lookupCode(group, CATCHING), input: { id: "c-7" } });
    const wait = await waitingCall(alice(), run.id, "approval");
    expectOk(await org.api.post(`/v1/calls/${wait.callId}/reject`, { message: "the customer said no" }));
    const done = await ended(alice(), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    expect(done.output.value).toMatchObject({ name: "ActionError", action });
    expect(done.output.value.message).toContain("the customer said no");
    expect(seen).toHaveLength(0);
  } finally {
    await worker.stop();
  }
});

test("a call whose stream closed and did not come back within its reconnect timeout is handed again, as delivery 2, with the same idempotency key", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet({ reconnectTimeout: "1s" });
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-8" } });
  const { call: first, stream: lost } = await handOne(org.api, actionSet, action, run.id);
  // Another worker holds a stream; the first one dies and never comes back.
  const other = await CallStream.open(org.api, { actionSet, actions: [action] });
  await lost.close();
  const second = await other.callOf(action, run.id, 60_000);
  expect(second.delivery).toBe(2);
  expect(second.idempotencyKey).toBe(first.idempotencyKey);
  expect(second.token).not.toBe(first.token);
  expectOk(await org.api.post(`/v1/calls/${second.token}/complete`, { result: { id: "c-8", name: "delivery 2" } }));
  const done = await ended(alice(), run.id);
  expect(done.status).toBe("completed");
  expect(done.output.value).toEqual({ id: "c-8", name: "delivery 2" });
});

test("a call of an action that is not idempotent whose stream did not come back fails with outcomeUnknown, never handed again", { tag: "@success" }, async () => {
  const { actionSet, group, action } = await lookupActionSet({ reconnectTimeout: "1s", idempotent: false });
  const run = await startRun(alice(), { actionSet, code: lookupCode(group, CATCHING), input: { id: "c-9" } });
  const { stream: lost } = await handOne(org.api, actionSet, action, run.id);
  // Another worker holds a stream: the call is not handed to it.
  const other = await CallStream.open(org.api, { actionSet, actions: [action] });
  await lost.close();
  const done = await ended(alice(), run.id);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value).toMatchObject({ name: "ActionError", action, outcomeUnknown: true });
  expect(other.handed.filter((t) => t.runId === run.id)).toEqual([]);
});

test("a call of an action that is not idempotent whose stream did not come back fails, even when no stream is open", { tag: "@failure" }, async () => {
  // archi/04-tools.md, "Call lifecycle": the stream closed and did not come back within its
  // reconnect timeout → `done` with `outcomeUnknown` at once (`idempotent: false`). The harness's worker died.
  const { actionSet, group, action } = await lookupActionSet({ reconnectTimeout: "1s", idempotent: false });
  const run = await startRun(alice(), { actionSet, code: lookupCode(group, CATCHING), input: { id: "c-9b" } });
  const { stream } = await handOne(org.api, actionSet, action, run.id);
  await stream.close();
  const done = await ended(alice(), run.id, 30_000);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value).toMatchObject({ name: "ActionError", action, outcomeUnknown: true });
});

// ---------------------------------------------------------------------------------------------
// Failure

test("arguments outside the action's schema throw a TypeError at the call site, and no call is handed", { tag: "@failure" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const bad = `import { ${group} } from "rewind:actions";

export default async function main() {
  return await ${group}.lookup({ id: 42 } as any);
}
`;
  const run = await startRun(alice(), { actionSet, code: bad, wait: "30s" });
  const done = run.status === "failed" ? run : await ended(alice(), run.id);
  expect(done.status, JSON.stringify(done)).toBe("failed");
  expect(done.error.name).toBe("TypeError");
  expect(done.error.location?.line).toBe(4);
  expect(await handedWithin(org.api, { actionSet, actions: [action] }), "the harness never sees the call").toEqual([]);
  evidence(`${done.error.name}: ${done.error.message}`);
});

test("a failure the code does not catch fails the run, naming the call and its line", { tag: "@failure" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const { worker } = serve(actionSet, action, () => {
    throw new HarnessActionError("no such customer");
  });
  try {
    const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-10" }, wait: "30s" });
    const done = await ended(alice(), run.id);
    expect(done.status).toBe("failed");
    expect(done.error.name).toBe("ActionError");
    expect(done.error.failedCall).toMatchObject({ seq: 1, action, args: { value: { id: "c-10" } }, error: { kind: "error" }, callsite: { line: 5 } });
    expect(done.error.failedCall.error.message).toContain("no such customer");
  } finally {
    await worker.stop();
  }
});

test("approving a call that does not wait for an approval is refused with call-not-waiting", { tag: "@failure" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-11" } });
  const { call } = await handOne(org.api, actionSet, action, run.id);
  const reply = await org.api.post(`/v1/calls/${call.id}/approve`);
  expectProblem(reply, 409, "call-not-waiting");
  const rejected = await org.api.post(`/v1/calls/${call.id}/reject`, { message: "too late" });
  expectProblem(rejected, 409, "call-not-waiting");
  // The call handed can still be answered.
  expectOk(await org.api.post(`/v1/calls/${call.token}/complete`, { result: { id: "c-11", name: "fine" } }));
  expect((await ended(alice(), run.id)).status).toBe("completed");
});

test("approving or rejecting a call that does not exist is not-found", { tag: "@failure" }, async () => {
  expectProblem(await org.api.post(`/v1/calls/call_${unique()}${unique()}/approve`), 404, "not-found");
  expectProblem(await org.api.post(`/v1/calls/00000000-0000-7000-8000-000000000000/reject`, {}), 404, "not-found");
});

test("an answer without its result is an invalid request, and the call stays handed", { tag: "@failure" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-12" } });
  const { call } = await handOne(org.api, actionSet, action, run.id);
  expectProblem(await org.api.post(`/v1/calls/${call.token}/complete`, { value: 1 }), 400, "invalid-request");
  expectProblem(await org.api.post(`/v1/calls/${call.token}/fail`, { retryable: true }), 400, "invalid-request");
  expectOk(await org.api.post(`/v1/calls/${call.token}/complete`, { result: { id: "c-12", name: "then" } }));
  expect((await ended(alice(), run.id)).output.value).toEqual({ id: "c-12", name: "then" });
});

test("a deferred call nobody answers fails at its timeout", { tag: "@failure" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-13" } });
  const { call } = await handOne(org.api, actionSet, action, run.id);
  expectOk(await org.api.post(`/v1/calls/${call.token}/defer`, { timeout: "1s" }));
  const done = await ended(alice(), run.id);
  expect(done.status, JSON.stringify(done)).toBe("failed");
  expect(done.error.failedCall).toMatchObject({ action, error: { kind: "timeout" } });
});

test("a deferral is bounded by the action's execution timeout", { tag: "@failure" }, async () => {
  // archi/08-decisions.md: a deferral ends at the requested timeout or the action's execution
  // timeout from now, whichever comes first; the worker is told the timeout (`timeoutMs`).
  const { actionSet, group, action } = await lookupActionSet({ timeout: "1s" });
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-14" } });
  const { call } = await handOne(org.api, actionSet, action, run.id);
  expect(call.timeoutMs).toBe(1000);
  expectOk(await org.api.post(`/v1/calls/${call.token}/defer`, { timeout: "10m" }));
  const done = await ended(alice(), run.id);
  expect(done.status, JSON.stringify(done)).toBe("failed");
  expect(done.error.failedCall).toMatchObject({ action, error: { kind: "timeout" } });
});

test("a harness action's failure above 64 KiB is refused as payload-too-large", { tag: "@failure" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-15" } });
  const { call } = await handOne(org.api, actionSet, action, run.id);
  const reply = await org.api.post(`/v1/calls/${call.token}/fail`, { message: "x".repeat(70 * 1024) });
  expectProblem(reply, 413, "payload-too-large");
  expectOk(await org.api.post(`/v1/calls/${call.token}/fail`, { message: "a short reason" }));
  const done = await ended(alice(), run.id);
  expect(done.error.failedCall.error.message).toContain("a short reason");
});

// ---------------------------------------------------------------------------------------------
// Attack

test("a call token used again after its answer is gone", { tag: "@attack" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-16" } });
  const { call } = await handOne(org.api, actionSet, action, run.id);
  expectOk(await org.api.post(`/v1/calls/${call.token}/complete`, { result: { id: "c-16", name: "first answer" } }));
  const first = await ended(alice(), run.id);
  const probe = async () => {
    const r = (await alice().get(`/v1/runs/${run.id}`)).json;
    return { status: r.status, output: r.output?.digest, historyLen: r.historyLen };
  };
  await expectRefused({
    attempt: () => org.api.post(`/v1/calls/${call.token}/complete`, { result: { id: "c-16", name: "forged second answer" } }),
    status: 410,
    slug: "call-gone",
    probe,
  });
  await expectRefused({ attempt: () => org.api.post(`/v1/calls/${call.token}/fail`, { message: "late failure" }), status: 410, slug: "call-gone", probe });
  await expectRefused({ attempt: () => org.api.post(`/v1/calls/${call.token}/defer`, {}), status: 410, slug: "call-gone", probe });
  expect(first.output.value).toEqual({ id: "c-16", name: "first answer" });
  evidence("complete, fail and defer with a used token: 410 call-gone; the output is the first answer");
});

test("an approval cannot be used twice: the action runs once", { tag: "@attack" }, async () => {
  const { actionSet, group, action } = await lookupActionSet({ approval: true });
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-17" } });
  const wait = await waitingCall(alice(), run.id, "approval");
  expectOk(await org.api.post(`/v1/calls/${wait.callId}/approve`));
  // Approved, the call is ready: approving or rejecting it again changes nothing.
  const state = async () => (await alice().get(`/v1/runs/${run.id}`)).json.waitingFor;
  await expectRefused({ attempt: () => org.api.post(`/v1/calls/${wait.callId}/approve`), status: 409, slug: "call-not-waiting", probe: state });
  await expectRefused({ attempt: () => org.api.post(`/v1/calls/${wait.callId}/reject`, {}), status: 409, slug: "call-not-waiting", probe: state });
  const { worker, seen } = serve(actionSet, action, ({ id }) => ({ id, name: "once" }));
  try {
    expect((await ended(alice(), run.id)).output.value).toEqual({ id: "c-17", name: "once" });
    expect(seen).toHaveLength(1);
    // Once the run ended, its call is gone.
    await expectRefused({ attempt: () => org.api.post(`/v1/calls/${wait.callId}/approve`), status: 410, slug: "call-gone", probe: () => seen.length });
    evidence("approve and reject again: 409 call-not-waiting; after the run's end: 410 call-gone; one call");
  } finally {
    await worker.stop();
  }
});

test("the answer of a call handed on a stream gone past its reconnect timeout is refused; the next hand-out's answer stands", { tag: "@attack" }, async () => {
  const { actionSet, group, action } = await lookupActionSet({ reconnectTimeout: "1s" });
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-18" } });
  const { call: lost, stream } = await handOne(org.api, actionSet, action, run.id);
  const other = await CallStream.open(org.api, { actionSet, actions: [action] });
  await stream.close();
  const current = await other.callOf(action, run.id, 60_000);
  await expectRefused({
    attempt: () => org.api.post(`/v1/calls/${lost.token}/complete`, { result: { id: "c-18", name: "from the stream gone" } }),
    status: 410,
    slug: "call-gone",
    probe: async () => (await alice().get(`/v1/runs/${run.id}`)).json.historyLen,
  });
  expectOk(await org.api.post(`/v1/calls/${current.token}/complete`, { result: { id: "c-18", name: "from the next hand-out" } }));
  expect((await ended(alice(), run.id)).output.value).toEqual({ id: "c-18", name: "from the next hand-out" });
  evidence(`the token of delivery ${lost.delivery}, handed on a stream that did not come back → 410 call-gone; delivery ${current.delivery}'s answer stands`);
});

test("an answer for a run that was cancelled is gone, and the run stays cancelled", { tag: "@attack" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-19" } });
  const { call, stream } = await handOne(org.api, actionSet, action, run.id);
  expectOk(await alice().post(`/v1/runs/${run.id}/cancel`, {}));
  const cancelled = await ended(alice(), run.id);
  expect(cancelled.status).toBe("cancelled");
  // The stream says so: the call handed on it is gone.
  await stream.goneOf(call.token);
  await expectRefused({
    attempt: () => org.api.post(`/v1/calls/${call.token}/complete`, { result: { id: "c-19", name: "too late" } }),
    status: 410,
    slug: "call-gone",
    probe: async () => {
      const r = (await alice().get(`/v1/runs/${run.id}`)).json;
      return { status: r.status, historyLen: r.historyLen };
    },
  });
});

test("a forged call token reaches no call", { tag: "@attack" }, async () => {
  const { actionSet, group, action } = await lookupActionSet();
  const run = await startRun(alice(), { actionSet, code: lookupCode(group), input: { id: "c-20" } });
  const { call } = await handOne(org.api, actionSet, action, run.id);
  // The real token with its last character changed, and a token of the right shape made up.
  const last = call.token.at(-1) === "A" ? "B" : "A";
  for (const forged of [`${call.token.slice(0, -1)}${last}`, `rewind_tt_${"0".repeat(64)}`, "rewind_tt_", "../../runs"]) {
    await expectRefused({
      attempt: () => org.api.post(`/v1/calls/${encodeURIComponent(forged)}/complete`, { result: { id: "c-20", name: "forged" } }),
      status: 410,
      slug: "call-gone",
      probe: async () => (await alice().get(`/v1/runs/${run.id}`)).json.historyLen,
    });
  }
  expectOk(await org.api.post(`/v1/calls/${call.token}/complete`, { result: { id: "c-20", name: "genuine" } }));
  expect((await ended(alice(), run.id)).output.value).toEqual({ id: "c-20", name: "genuine" });
  evidence("forged tokens: refused; only the genuine token's answer reached the run");
});
