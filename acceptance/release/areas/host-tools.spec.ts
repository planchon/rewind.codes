/**
 * Host tools and their tasks (`archi/04-tools.md`, "Tool task lifecycle", "Policies"), on setup A:
 * the SDK's worker, and task streams opened by hand; the open stream as the liveness of its
 * tasks, a stream's slots, a worker coming back within the grace, a stream that does not come
 * back, `gone`, deferrals, failures, retries, timeouts, approvals; arguments checked against the
 * schema before the harness sees them; and the replays a hostile worker could try with a token or
 * an approval already used.
 *
 * The grace a task handed on a closed stream waits for its worker is the tool's policy
 * `heartbeatTimeout` (archi/04's "Grace after a stream closes", 60 s by default).
 */
import { type HostToolContext, HostToolError, type HostToolWorker } from "@rewind.codes/sdk";
import { setTimeout as delay } from "node:timers/promises";
import { evidence, expect, expectOk, expectProblem, expectRefused, newTenant, type Tenant, test, unique } from "../support/gate.js";
import { closeTaskStreams, ended, handedWithin, handOne, hostTool, runUntil, startRun, TaskStream, waitingCall } from "../support/tools-helpers.js";

let tenant: Tenant;

test.beforeAll(async () => {
  tenant = await newTenant("host-tools");
});

// A stream a test left open would keep its tasks: each test's streams close with it.
test.afterEach(async () => {
  await closeTaskStreams();
});

/** A toolset of one host tool `<group>.lookup`, with a group of its own so tests never share tasks. */
async function lookupToolset(policy: Record<string, unknown> = {}) {
  const group = `g${unique()}`;
  const tool = `${group}.lookup`;
  const created = await tenant.api.post("/v1/toolsets", { host: [hostTool(tool, policy)] });
  expectOk(created);
  return { toolset: created.json.id as string, group, tool };
}

/** A script calling `<group>.lookup({ id })` once; `body` handles the call's promise `call`. */
function script(group: string, body = "return await call;"): string {
  return `import { ${group} } from "rewind:tools";
import { ToolError } from "rewind";

export default async function main(input: { id: string }) {
  const call = ${group}.lookup({ id: input.id });
  ${body}
}
`;
}

/** The script catches the ToolError and returns what it saw. */
const CATCHING = `try {
    return { ok: await call };
  } catch (error) {
    if (!(error instanceof ToolError)) throw error;
    return { name: error.name, message: error.message, tool: error.tool, data: error.data ?? null, outcomeUnknown: error.outcomeUnknown };
  }`;

type Seen = { args: any; ctx: Pick<HostToolContext, "runId" | "namespace" | "seq" | "attempt" | "idempotencyKey" | "labels" | "traceparent"> };

function serve(toolset: string, tool: string, handler: (args: any, ctx: HostToolContext, seen: Seen[]) => unknown): { worker: HostToolWorker; seen: Seen[] } {
  const seen: Seen[] = [];
  const worker = tenant.rewind.worker({
    toolset,
    tools: {
      [tool]: async (args, ctx) => {
        seen.push({
          args,
          ctx: {
            runId: ctx.runId,
            namespace: ctx.namespace,
            seq: ctx.seq,
            attempt: ctx.attempt,
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

const alice = () => tenant.api.ns("alice");

// ---------------------------------------------------------------------------------------------
// Success

test("a host tool served by the SDK's worker is called once, with its run's context", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const { worker, seen } = serve(toolset, tool, ({ id }) => ({ id, name: "Ada Lovelace" }));
  try {
    const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-1" }, labels: { suite: "host-tools" }, wait: "30s" });
    const done = run.status === "completed" ? run : await ended(alice(), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    expect(done.output.value).toEqual({ id: "c-1", name: "Ada Lovelace" });
    expect(seen).toHaveLength(1);
    const ctx = seen[0]!.ctx;
    expect(ctx).toMatchObject({ runId: run.id, namespace: "alice", seq: 1, attempt: 1, idempotencyKey: `${run.id}/1`, labels: { suite: "host-tools" } });
    // The tool call span is the parent of the handler's work, in the run's trace.
    expect(ctx.traceparent?.split("-")[1]).toBe(done.traceId);
    evidence(`called once with ${ctx.idempotencyKey} in namespace ${ctx.namespace}`);
  } finally {
    await worker.stop();
  }
});

test("an open stream keeps its task while the handler works, past the grace, with nothing sent meanwhile", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset({ heartbeatTimeout: "1s" });
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-2" } });
  const { task, stream } = await handOne(tenant.api, toolset, tool, run.id);
  expect(task).toMatchObject({ runId: run.id, namespace: "alice", seq: 1, attempt: 1, idempotencyKey: `${run.id}/1` });
  expect(task.token).toEqual(expect.any(String));
  // Three times the grace, and the stream still open: the task is still this worker's.
  await delay(3000);
  expectOk(await tenant.api.post(`/v1/tasks/${task.token}/complete`, { result: { id: "c-2", name: "kept" } }));
  const done = await ended(alice(), run.id);
  expect(done.status).toBe("completed");
  expect(done.output.value).toEqual({ id: "c-2", name: "kept" });
  expect(done.attempts ?? []).toEqual([]);
  expect(stream.handed.filter((t) => t.runId === run.id)).toHaveLength(1);
  evidence("a task held 3 s on an open stream (grace 1 s), answered by its token: attempt 1, handed once");
});

test("a stream holds at most its slots: the next task is handed once a slot is free", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const stream = await TaskStream.open(tenant.api, { toolset, tools: [tool], slots: 1 });
  const first = await startRun(alice(), { toolset, script: script(group), input: { id: "c-2a" } });
  const second = await startRun(alice(), { toolset, script: script(group), input: { id: "c-2b" } });
  const handed = await stream.taskOf(tool);
  await delay(1500);
  expect(stream.handed, "one slot, one task in flight").toHaveLength(1);
  expectOk(await tenant.api.post(`/v1/tasks/${handed.token}/complete`, { result: { id: handed.args.id, name: "first" } }));
  const next = await stream.task("the other run's task", (t) => t.runId !== handed.runId);
  expect([first.id, second.id].sort()).toEqual([handed.runId, next.runId].sort());
  expectOk(await tenant.api.post(`/v1/tasks/${next.token}/complete`, { result: { id: next.args.id, name: "second" } }));
  for (const run of [first, second]) expect((await ended(alice(), run.id)).status).toBe("completed");
  evidence("slots=1: the second task was handed only once the first was answered");
});

test("a deferred task is answered later, by its token, from another key, after its stream closed", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset({ heartbeatTimeout: "1s" });
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-3" } });
  const { task, stream } = await handOne(tenant.api, toolset, tool, run.id);
  expectOk(await tenant.api.post(`/v1/tasks/${task.token}/defer`, { timeout: "5m" }));
  // Deferred, the task does not need its stream: it waits past the grace for its answer.
  await stream.close();
  await delay(2000);
  const wait = await waitingCall(alice(), run.id, "deferred");
  expect(wait.tool).toBe(tool);
  // Another process of the harness, with a key of role `worker`, answers it.
  const workerKey = await tenant.key("worker");
  expectOk(await tenant.api.as(workerKey.key).post(`/v1/tasks/${task.token}/complete`, { result: { id: "c-3", name: "answered later" } }));
  const done = await ended(alice(), run.id);
  expect(done.status).toBe("completed");
  expect(done.output.value).toEqual({ id: "c-3", name: "answered later" });
  expect(done.attempts ?? []).toEqual([]);
});

test("a worker that opens its stream again within the grace keeps its task: it is not handed again", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset({ heartbeatTimeout: "10s" });
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-3b" } });
  const { task, stream } = await handOne(tenant.api, toolset, tool, run.id);
  // The connection drops; the worker comes back with its id within the grace.
  await stream.close();
  const again = await stream.reopen();
  await delay(1500);
  expect(again.handed, "the task kept is not handed again").toEqual([]);
  expectOk(await tenant.api.post(`/v1/tasks/${task.token}/complete`, { result: { id: "c-3b", name: "after the drop" } }));
  const done = await ended(alice(), run.id);
  expect(done.output.value).toEqual({ id: "c-3b", name: "after the drop" });
  expect(done.attempts ?? []).toEqual([]);
  evidence(`the stream of ${stream.query.worker} dropped and came back within the grace: its token answered, attempt 1`);
});

test("a handler's failure reaches the script as a ToolError with its message and data", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const { worker, seen } = serve(toolset, tool, () => {
    throw new HostToolError("card declined", { data: { code: 51 } });
  });
  try {
    const run = await startRun(alice(), { toolset, script: script(group, CATCHING), input: { id: "c-4" }, wait: "30s" });
    const done = await ended(alice(), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    expect(done.output.value).toMatchObject({ name: "ToolError", tool, data: { code: 51 }, outcomeUnknown: false });
    expect(done.output.value.message).toContain("card declined");
    expect(seen).toHaveLength(1);
  } finally {
    await worker.stop();
  }
});

test("a retryable failure is retried with the same idempotency key, and the attempts are counted", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const { worker, seen } = serve(toolset, tool, ({ id }, ctx) => {
    if (ctx.attempt === 1) throw new HostToolError("upstream busy", { retryable: true });
    return { id, name: "second try" };
  });
  try {
    const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-5" } });
    const done = await ended(alice(), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    expect(done.output.value).toEqual({ id: "c-5", name: "second try" });
    expect(seen.map((s) => s.ctx.attempt)).toEqual([1, 2]);
    expect(new Set(seen.map((s) => s.ctx.idempotencyKey))).toEqual(new Set([`${run.id}/1`]));
    await expect.poll(async () => (await alice().get(`/v1/runs/${run.id}`)).json.attempts).toEqual([{ seq: 1, attempts: 2 }]);
  } finally {
    await worker.stop();
  }
});

test("a call under the approval policy waits for the harness, then runs once approved", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset({ approval: true });
  const { worker, seen } = serve(toolset, tool, ({ id }) => ({ id, name: "approved" }));
  try {
    const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-6" } });
    const wait = await waitingCall(alice(), run.id, "approval");
    expect(wait.args.value).toEqual({ id: "c-6" });
    await delay(1000);
    expect(seen, "nothing is sent before the approval").toHaveLength(0);
    expectOk(await tenant.api.post(`/v1/tasks/${wait.taskId}/approve`));
    const done = await ended(alice(), run.id);
    expect(done.status).toBe("completed");
    expect(done.output.value).toEqual({ id: "c-6", name: "approved" });
    expect(seen).toHaveLength(1);
  } finally {
    await worker.stop();
  }
});

test("a rejected call fails with a ToolError carrying the harness's message, and is never sent", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset({ approval: true });
  const { worker, seen } = serve(toolset, tool, () => ({ never: true }));
  try {
    const run = await startRun(alice(), { toolset, script: script(group, CATCHING), input: { id: "c-7" } });
    const wait = await waitingCall(alice(), run.id, "approval");
    expectOk(await tenant.api.post(`/v1/tasks/${wait.taskId}/reject`, { message: "the customer said no" }));
    const done = await ended(alice(), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    expect(done.output.value).toMatchObject({ name: "ToolError", tool });
    expect(done.output.value.message).toContain("the customer said no");
    expect(seen).toHaveLength(0);
  } finally {
    await worker.stop();
  }
});

test("a task whose stream closed and did not come back within its grace is handed again, as attempt 2, with the same idempotency key", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset({ heartbeatTimeout: "1s" });
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-8" } });
  const { task: first, stream: lost } = await handOne(tenant.api, toolset, tool, run.id);
  // Another worker holds a stream; the first one dies and never comes back.
  const other = await TaskStream.open(tenant.api, { toolset, tools: [tool] });
  await lost.close();
  const second = await other.taskOf(tool, run.id, 60_000);
  expect(second.attempt).toBe(2);
  expect(second.idempotencyKey).toBe(first.idempotencyKey);
  expect(second.token).not.toBe(first.token);
  expectOk(await tenant.api.post(`/v1/tasks/${second.token}/complete`, { result: { id: "c-8", name: "attempt 2" } }));
  const done = await ended(alice(), run.id);
  expect(done.status).toBe("completed");
  expect(done.output.value).toEqual({ id: "c-8", name: "attempt 2" });
});

test("a task of an at_most_once tool whose stream did not come back fails with outcomeUnknown, never handed again", { tag: "@success" }, async () => {
  const { toolset, group, tool } = await lookupToolset({ heartbeatTimeout: "1s", onInterruption: "at_most_once" });
  const run = await startRun(alice(), { toolset, script: script(group, CATCHING), input: { id: "c-9" } });
  const { stream: lost } = await handOne(tenant.api, toolset, tool, run.id);
  // Another worker holds a stream: the task is not handed to it.
  const other = await TaskStream.open(tenant.api, { toolset, tools: [tool] });
  await lost.close();
  const done = await ended(alice(), run.id);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value).toMatchObject({ name: "ToolError", tool, outcomeUnknown: true });
  expect(other.handed.filter((t) => t.runId === run.id)).toEqual([]);
});

test("a task of an at_most_once tool whose stream did not come back fails, even when no stream is open", { tag: "@failure" }, async () => {
  // archi/04-tools.md, "Tool task lifecycle": the stream closed and did not come back within its
  // grace → `done` with `outcomeUnknown` at once (`at_most_once`). The harness's worker died.
  const { toolset, group, tool } = await lookupToolset({ heartbeatTimeout: "1s", onInterruption: "at_most_once" });
  const run = await startRun(alice(), { toolset, script: script(group, CATCHING), input: { id: "c-9b" } });
  const { stream } = await handOne(tenant.api, toolset, tool, run.id);
  await stream.close();
  const done = await ended(alice(), run.id, 30_000);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value).toMatchObject({ name: "ToolError", tool, outcomeUnknown: true });
});

// ---------------------------------------------------------------------------------------------
// Failure

test("arguments outside the tool's schema throw a TypeError at the call site, and no task is made", { tag: "@failure" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const bad = `import { ${group} } from "rewind:tools";

export default async function main() {
  return await ${group}.lookup({ id: 42 } as any);
}
`;
  const run = await startRun(alice(), { toolset, script: bad, wait: "30s" });
  const done = run.status === "failed" ? run : await ended(alice(), run.id);
  expect(done.status, JSON.stringify(done)).toBe("failed");
  expect(done.error.name).toBe("TypeError");
  expect(done.error.location?.line).toBe(4);
  expect(await handedWithin(tenant.api, { toolset, tools: [tool] }), "the harness never sees the call").toEqual([]);
  evidence(`${done.error.name}: ${done.error.message}`);
});

test("a failure the script does not catch fails the run, naming the call and its line", { tag: "@failure" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const { worker } = serve(toolset, tool, () => {
    throw new HostToolError("no such customer");
  });
  try {
    const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-10" }, wait: "30s" });
    const done = await ended(alice(), run.id);
    expect(done.status).toBe("failed");
    expect(done.error.name).toBe("ToolError");
    expect(done.error.failedCall).toMatchObject({ seq: 1, tool, args: { value: { id: "c-10" } }, error: { kind: "error" }, callsite: { line: 5 } });
    expect(done.error.failedCall.error.message).toContain("no such customer");
  } finally {
    await worker.stop();
  }
});

test("approving a task that does not wait for an approval is refused with task-not-waiting", { tag: "@failure" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-11" } });
  const { task } = await handOne(tenant.api, toolset, tool, run.id);
  const reply = await tenant.api.post(`/v1/tasks/${task.id}/approve`);
  expectProblem(reply, 409, "task-not-waiting");
  const rejected = await tenant.api.post(`/v1/tasks/${task.id}/reject`, { message: "too late" });
  expectProblem(rejected, 409, "task-not-waiting");
  // The task handed still answers.
  expectOk(await tenant.api.post(`/v1/tasks/${task.token}/complete`, { result: { id: "c-11", name: "fine" } }));
  expect((await ended(alice(), run.id)).status).toBe("completed");
});

test("approving or rejecting a task that does not exist is not-found", { tag: "@failure" }, async () => {
  expectProblem(await tenant.api.post(`/v1/tasks/task_${unique()}${unique()}/approve`), 404, "not-found");
  expectProblem(await tenant.api.post(`/v1/tasks/00000000-0000-7000-8000-000000000000/reject`, {}), 404, "not-found");
});

test("an answer without its result is an invalid request, and the task stays handed", { tag: "@failure" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-12" } });
  const { task } = await handOne(tenant.api, toolset, tool, run.id);
  expectProblem(await tenant.api.post(`/v1/tasks/${task.token}/complete`, { value: 1 }), 400, "invalid-request");
  expectProblem(await tenant.api.post(`/v1/tasks/${task.token}/fail`, { retryable: true }), 400, "invalid-request");
  expectOk(await tenant.api.post(`/v1/tasks/${task.token}/complete`, { result: { id: "c-12", name: "then" } }));
  expect((await ended(alice(), run.id)).output.value).toEqual({ id: "c-12", name: "then" });
});

test("a deferred task nobody answers fails at its timeout", { tag: "@failure" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-13" } });
  const { task } = await handOne(tenant.api, toolset, tool, run.id);
  expectOk(await tenant.api.post(`/v1/tasks/${task.token}/defer`, { timeout: "1s" }));
  const done = await ended(alice(), run.id);
  expect(done.status, JSON.stringify(done)).toBe("failed");
  expect(done.error.failedCall).toMatchObject({ tool, error: { kind: "timeout" } });
});

test("a deferral is bounded by the tool's execution timeout", { tag: "@failure" }, async () => {
  // archi/08-decisions.md: a deferral ends at the requested timeout or the tool's execution
  // timeout from now, whichever comes first; the worker is told the timeout (`timeoutMs`).
  const { toolset, group, tool } = await lookupToolset({ timeout: "1s" });
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-14" } });
  const { task } = await handOne(tenant.api, toolset, tool, run.id);
  expect(task.timeoutMs).toBe(1000);
  expectOk(await tenant.api.post(`/v1/tasks/${task.token}/defer`, { timeout: "10m" }));
  const done = await ended(alice(), run.id);
  expect(done.status, JSON.stringify(done)).toBe("failed");
  expect(done.error.failedCall).toMatchObject({ tool, error: { kind: "timeout" } });
});

test("a host-tool failure above 64 KiB is refused as payload-too-large", { tag: "@failure" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-15" } });
  const { task } = await handOne(tenant.api, toolset, tool, run.id);
  const reply = await tenant.api.post(`/v1/tasks/${task.token}/fail`, { message: "x".repeat(70 * 1024) });
  expectProblem(reply, 413, "payload-too-large");
  expectOk(await tenant.api.post(`/v1/tasks/${task.token}/fail`, { message: "a short reason" }));
  const done = await ended(alice(), run.id);
  expect(done.error.failedCall.error.message).toContain("a short reason");
});

// ---------------------------------------------------------------------------------------------
// Attack

test("a task token used again after its answer is gone", { tag: "@attack" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-16" } });
  const { task } = await handOne(tenant.api, toolset, tool, run.id);
  expectOk(await tenant.api.post(`/v1/tasks/${task.token}/complete`, { result: { id: "c-16", name: "first answer" } }));
  const first = await ended(alice(), run.id);
  const probe = async () => {
    const r = (await alice().get(`/v1/runs/${run.id}`)).json;
    return { status: r.status, output: r.output?.digest, historyLen: r.historyLen };
  };
  await expectRefused({
    attempt: () => tenant.api.post(`/v1/tasks/${task.token}/complete`, { result: { id: "c-16", name: "forged second answer" } }),
    status: 410,
    slug: "task-gone",
    probe,
  });
  await expectRefused({ attempt: () => tenant.api.post(`/v1/tasks/${task.token}/fail`, { message: "late failure" }), status: 410, slug: "task-gone", probe });
  await expectRefused({ attempt: () => tenant.api.post(`/v1/tasks/${task.token}/defer`, {}), status: 410, slug: "task-gone", probe });
  expect(first.output.value).toEqual({ id: "c-16", name: "first answer" });
  evidence("complete, fail and defer with a used token: 410 task-gone; the output is the first answer");
});

test("an approval cannot be used twice: the tool runs once", { tag: "@attack" }, async () => {
  const { toolset, group, tool } = await lookupToolset({ approval: true });
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-17" } });
  const wait = await waitingCall(alice(), run.id, "approval");
  expectOk(await tenant.api.post(`/v1/tasks/${wait.taskId}/approve`));
  // Approved, the task is ready: approving or rejecting it again changes nothing.
  const state = async () => (await alice().get(`/v1/runs/${run.id}`)).json.waitingFor;
  await expectRefused({ attempt: () => tenant.api.post(`/v1/tasks/${wait.taskId}/approve`), status: 409, slug: "task-not-waiting", probe: state });
  await expectRefused({ attempt: () => tenant.api.post(`/v1/tasks/${wait.taskId}/reject`, {}), status: 409, slug: "task-not-waiting", probe: state });
  const { worker, seen } = serve(toolset, tool, ({ id }) => ({ id, name: "once" }));
  try {
    expect((await ended(alice(), run.id)).output.value).toEqual({ id: "c-17", name: "once" });
    expect(seen).toHaveLength(1);
    // Once the run ended, its task is gone.
    await expectRefused({ attempt: () => tenant.api.post(`/v1/tasks/${wait.taskId}/approve`), status: 410, slug: "task-gone", probe: () => seen.length });
    evidence("approve and reject again: 409 task-not-waiting; after the run's end: 410 task-gone; one call");
  } finally {
    await worker.stop();
  }
});

test("the answer of a task handed on a stream gone past its grace is refused; the next hand-out's answer stands", { tag: "@attack" }, async () => {
  const { toolset, group, tool } = await lookupToolset({ heartbeatTimeout: "1s" });
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-18" } });
  const { task: lost, stream } = await handOne(tenant.api, toolset, tool, run.id);
  const other = await TaskStream.open(tenant.api, { toolset, tools: [tool] });
  await stream.close();
  const current = await other.taskOf(tool, run.id, 60_000);
  await expectRefused({
    attempt: () => tenant.api.post(`/v1/tasks/${lost.token}/complete`, { result: { id: "c-18", name: "from the stream gone" } }),
    status: 410,
    slug: "task-gone",
    probe: async () => (await alice().get(`/v1/runs/${run.id}`)).json.historyLen,
  });
  expectOk(await tenant.api.post(`/v1/tasks/${current.token}/complete`, { result: { id: "c-18", name: "from the next hand-out" } }));
  expect((await ended(alice(), run.id)).output.value).toEqual({ id: "c-18", name: "from the next hand-out" });
  evidence(`the token of attempt ${lost.attempt}, handed on a stream that did not come back → 410 task-gone; attempt ${current.attempt}'s answer stands`);
});

test("an answer for a run that was cancelled is gone, and the run stays cancelled", { tag: "@attack" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-19" } });
  const { task, stream } = await handOne(tenant.api, toolset, tool, run.id);
  expectOk(await alice().post(`/v1/runs/${run.id}/cancel`, {}));
  const cancelled = await ended(alice(), run.id);
  expect(cancelled.status).toBe("cancelled");
  // The stream says so: the task handed on it is gone.
  await stream.goneOf(task.token);
  await expectRefused({
    attempt: () => tenant.api.post(`/v1/tasks/${task.token}/complete`, { result: { id: "c-19", name: "too late" } }),
    status: 410,
    slug: "task-gone",
    probe: async () => {
      const r = (await alice().get(`/v1/runs/${run.id}`)).json;
      return { status: r.status, historyLen: r.historyLen };
    },
  });
});

test("a forged task token reaches no task", { tag: "@attack" }, async () => {
  const { toolset, group, tool } = await lookupToolset();
  const run = await startRun(alice(), { toolset, script: script(group), input: { id: "c-20" } });
  const { task } = await handOne(tenant.api, toolset, tool, run.id);
  // The real token with its last character changed, and a token of the right shape made up.
  const last = task.token.at(-1) === "A" ? "B" : "A";
  for (const forged of [`${task.token.slice(0, -1)}${last}`, `rewind_tt_${"0".repeat(64)}`, "rewind_tt_", "../../runs"]) {
    await expectRefused({
      attempt: () => tenant.api.post(`/v1/tasks/${encodeURIComponent(forged)}/complete`, { result: { id: "c-20", name: "forged" } }),
      status: 410,
      slug: "task-gone",
      probe: async () => (await alice().get(`/v1/runs/${run.id}`)).json.historyLen,
    });
  }
  expectOk(await tenant.api.post(`/v1/tasks/${task.token}/complete`, { result: { id: "c-20", name: "genuine" } }));
  expect((await ended(alice(), run.id)).output.value).toEqual({ id: "c-20", name: "genuine" });
  evidence("forged tokens: refused; only the genuine token's answer reached the run");
});
