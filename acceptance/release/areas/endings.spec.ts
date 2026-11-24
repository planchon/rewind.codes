/**
 * Area "Endings and limits" of the release gate, on setup A (deployed): how a script ends
 * (`archi/03-execution.md`, "How a script ends", "Cancellation") and the resource limits that
 * bound a hostile or careless script ("Resource limits"): CPU per call into the script, heap,
 * array buffers, commands and bytes per activation, payload sizes. Every limit attack ends with a
 * canary run, so a script that ends a worker shows here.
 */
import { type McpFixture, startMcpFixture } from "../support/fixtures.js";
import { type Api, evidence, expect, expectHealthy, expectOk, newTenant, type Tenant, test, unique } from "../support/gate.js";
import { ended, events, expectFailed, expectLimit, started, waitingFor } from "../support/runs-helpers.js";

let tenant: Tenant;
let mcp: McpFixture;
let actionSet: string;
let api: Api;

test.beforeAll(async () => {
  tenant = await newTenant("endings");
  mcp = await startMcpFixture();
  const created = await tenant.api.post("/v1/action-sets", { mcp: [{ name: "fx", url: mcp.url }] });
  expectOk(created);
  actionSet = created.json.id;
  api = tenant.api.ns("endings");
});

test.afterAll(async () => {
  await mcp?.close();
});

/** Starts a script (type-checked unless `check: false`) and waits for its end. */
async function runToEnd(script: string, options: { check?: boolean; input?: unknown; timeoutMs?: number } = {}) {
  const run = await started(api, { actionSet, script, input: options.input ?? {}, check: options.check });
  return ended(api, run.id, options.timeoutMs ?? 90_000);
}

const echoes = (text: string) => mcp.calls.filter((c) => c.tool === "echo" && c.args.text === text);

// ---------------------------------------------------------------------------------------------
// Success

test("main's return value is the output, and a ActionError the script catches lets it go on", { tag: "@success" }, async () => {
  const run = await runToEnd(`import { fx } from "rewind:actions";
import { ActionError } from "rewind";
export default async function main() {
  try {
    await fx.fail({ reason: "card declined" });
    return "not reached";
  } catch (error) {
    if (error instanceof ActionError) return { action: error.action, message: error.message, outcomeUnknown: error.outcomeUnknown };
    throw error;
  }
}
`);
  expect(run.status, JSON.stringify(run)).toBe("completed");
  expect(run.output.value).toMatchObject({ action: "fx.fail", outcomeUnknown: false });
  expect(run.output.value.message).toContain("card declined");
});

test("a forgotten await: main returns first, and the run completes once the call has its outcome", { tag: "@success" }, async () => {
  const text = unique("forgotten-");
  const run = await runToEnd(
    `import { fx } from "rewind:actions";
export default async function main() {
  fx.slow({ ms: 800 }); // no await
  fx.echo({ text: "${text}" }); // no await
  return "done";
}
`,
    { check: false },
  );
  expect(run.status, JSON.stringify(run)).toBe("completed");
  expect(run.output.value).toBe("done");
  expect(echoes(text)).toHaveLength(1);
  const types = await events(api, run.id);
  expect(types.filter((t) => t === "call_completed")).toHaveLength(2);
  expect(types.at(-1)).toBe("complete_run");
  expect(types.indexOf("complete_run")).toBeGreaterThan(types.lastIndexOf("call_completed"));
});

test("a script that catches its CancelledError cleans up within its grace and completes", { tag: "@success" }, async () => {
  const run = await started(api, {
    script: `import { sleep, CancelledError } from "rewind";
export default async function main() {
  try {
    await sleep("30d");
    return "slept";
  } catch (error) {
    if (error instanceof CancelledError) return \`cleaned up after: \${error.message}\`;
    throw error;
  }
}
`,
  });
  await waitingFor(api, run.id, "timer");
  expectOk(await api.post(`/v1/runs/${run.id}/cancel`, { reason: "user asked" }), 202);
  const done = await ended(api, run.id);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value).toBe("cleaned up after: run cancelled: user asked");
  expect(await events(api, run.id)).toEqual(["run_started", "start_timer", "cancel_requested", "complete_run"]);
});

test("an output above 256 KiB is kept whole and read on demand through its run", { tag: "@success" }, async () => {
  const run = await runToEnd(`export default async function main() {
  return { rows: Array.from({ length: 3000 }, (_, n) => ({ n, text: "a row of a large report ".repeat(4) })) };
}
`);
  expect(run.status).toBe("completed");
  expect(run.output.bytes).toBeGreaterThan(256 * 1024);
  expect(run.output.value, "left out above 256 KiB").toBeUndefined();
  const payload = await api.get(`/v1/payloads/${run.output.digest}`, { query: { run: run.id } });
  expectOk(payload, 200);
  const value = payload.json?.value ?? payload.json;
  expect(value.rows).toHaveLength(3000);
  expect(value.rows[2999].n).toBe(2999);
});

// ---------------------------------------------------------------------------------------------
// Failures

test("main throwing fails the run with its error, its location and stack in script.ts", { tag: "@failure" }, async () => {
  const run = await runToEnd(`export default async function main() {
  const order: { id?: string } = {};
  if (!order.id) throw new Error("the order has no id");
  return order.id;
}
`);
  expectFailed(run, "Error", "the order has no id");
  expect(run.error.location).toMatchObject({ line: 3 });
  expect(run.error.stack ?? "").toContain("script.ts");
  expect(await events(api, run.id)).toEqual(["run_started", "fail_run"]);

  const typeError = await runToEnd(
    `export default async function main() { const o: any = undefined; return o.x; }\n`,
  );
  expectFailed(typeError, "TypeError", "Cannot read properties of undefined");
});

test("a forgotten await whose call fails fails the run with the ActionError, naming the call", { tag: "@failure" }, async () => {
  const run = await runToEnd(
    `import { fx } from "rewind:actions";
export default async function main() {
  fx.fail({ reason: "card declined" }); // no await
  return "charged";
}
`,
    { check: false },
  );
  expectFailed(run, "ActionError", "fx.fail failed");
  expect(run.output ?? undefined).toBeUndefined();
  expect(run.error.failedCall).toMatchObject({ seq: 1, action: "fx.fail", callsite: { line: 3 } });
  expect(run.error.failedCall.args.value).toEqual({ reason: "card declined" });
  expect(await events(api, run.id)).toEqual(["run_started", "schedule_call", "call_failed", "fail_run"]);
});

test("a rejection left unhandled fails the run, even when a handler comes later", { tag: "@failure" }, async () => {
  const run = await runToEnd(
    `import { fx } from "rewind:actions";
export default async function main() {
  const charge = fx.fail({ reason: "late" }); // rejected before it is awaited
  await fx.slow({ ms: 300 });
  try {
    await charge;
  } catch {
    return "handled too late";
  }
}
`,
    { check: false },
  );
  expectFailed(run, "ActionError", "fx.fail failed");
  expect(run.error.failedCall).toMatchObject({ action: "fx.fail" });
  const types = await events(api, run.id);
  expect(types.at(-1)).toBe("fail_run");
});

test("a script nothing can wake fails with DeadlockError", { tag: "@failure" }, async () => {
  const run = await runToEnd(`export default async function main() {
  await new Promise(() => {}); // nothing can ever resolve it
}
`);
  expectFailed(run, "DeadlockError", "nothing can wake the script");
  expect(await events(api, run.id)).toEqual(["run_started", "fail_run"]);
});

test("a run whose script fails waits for the call in flight, and records its outcome", { tag: "@failure" }, async () => {
  const run = await runToEnd(
    `import { fx } from "rewind:actions";
export default async function main() {
  fx.slow({ ms: 600 });
  throw new Error("gave up");
}
`,
    { check: false },
  );
  expectFailed(run, "Error", "gave up");
  expect(await events(api, run.id)).toEqual(["run_started", "schedule_call", "call_completed", "fail_run"]);
});

test("top-level code that throws aborts the run with invalid_script at its first activation", { tag: "@failure" }, async () => {
  const run = await runToEnd(`const limit = JSON.parse("{not json");
export default async function main() {
  return limit;
}
`);
  expect(run.status, JSON.stringify(run)).toBe("aborted");
  expect(run.abort).toMatchObject({ kind: "invalid_script" });
  expect(run.abort.message).toContain("SyntaxError");
});

test("a cleanup that needs another activation exceeds the cancellation grace: aborted, its call never sent", { tag: "@failure" }, async () => {
  const text = unique("cleanup-");
  const run = await started(api, {
    actionSet,
    script: `import { sleep } from "rewind";
import { fx } from "rewind:actions";
export default async function main() {
  try {
    await sleep("30d");
  } catch {
    await fx.echo({ text: "${text}" }); // needs another activation: too late
    return "cleaned up";
  }
}
`,
  });
  await waitingFor(api, run.id, "timer");
  expectOk(await api.post(`/v1/runs/${run.id}/cancel`, { reason: "user asked" }), 202);
  const done = await ended(api, run.id);
  expect(done.status, JSON.stringify(done)).toBe("aborted");
  expect(done.abort).toMatchObject({ kind: "cancelled" });
  expect(done.abort.message).toContain("cancellation grace");
  expect(await events(api, run.id)).toEqual(["run_started", "start_timer", "cancel_requested", "run_aborted"]);
  expect(echoes(text)).toHaveLength(0);
});

test("an output above 8 MiB aborts the run with limit_exceeded, and nothing above the limit is kept", { tag: "@failure" }, async () => {
  const run = await runToEnd(`export default async function main() { return "x".repeat(8 * 1024 * 1024); }\n`);
  expectLimit(run, "the payload of complete_run is");
  expect(run.abort.message).toContain("above the limit of 8388608 bytes");
  expect(await events(api, run.id)).toEqual(["run_started", "run_aborted"]);
});

test("tool arguments above 8 MiB abort the run before the call is sent", { tag: "@failure" }, async () => {
  const before = mcp.calls.length;
  const run = await runToEnd(`import { fx } from "rewind:actions";
export default async function main() {
  await fx.echo({ text: "x".repeat(8 * 1024 * 1024) });
}
`);
  expectLimit(run, "the payload of schedule_call is");
  expect(await events(api, run.id)).toEqual(["run_started", "run_aborted"]);
  expect(mcp.calls.length).toBe(before);
});

// ---------------------------------------------------------------------------------------------
// Attacks: exhaustion. The run ends; both workers keep serving.

test("an infinite loop in main is stopped after 5 s of CPU, and the workers keep serving", { tag: "@attack" }, async () => {
  const run = await runToEnd(`export default async function main() { for (;;) {} }\n`);
  expectLimit(run, "more than 5000ms of CPU time reacting to one trigger");
  expect(await events(api, run.id)).toEqual(["run_started", "run_aborted"]);
  await expectHealthy(tenant);
  evidence(`aborted: ${run.abort.message}`);
});

test("an infinite loop in a timer's delivery is stopped too, the trigger committed with the abort", { tag: "@attack" }, async () => {
  const run = await runToEnd(`import { sleep } from "rewind";
export default async function main() {
  await sleep(1);
  for (;;) {}
}
`);
  expectLimit(run, "more than 5000ms of CPU time");
  expect(await events(api, run.id)).toEqual(["run_started", "start_timer", "timer_fired", "run_aborted"]);
  await expectHealthy(tenant);
});

test("catastrophic regex backtracking is stopped by the CPU limit", { tag: "@attack" }, async () => {
  const run = await runToEnd(`export default async function main() {
  return /^(a+)+$/.test("a".repeat(40) + "b");
}
`);
  expectLimit(run, "more than 5000ms of CPU time");
  await expectHealthy(tenant);
});

test("a heap bomb is stopped at 128 MiB of heap", { tag: "@attack" }, async () => {
  const run = await runToEnd(`export default async function main() {
  const hoard: number[][] = [];
  for (;;) hoard.push(new Array(100_000).fill(1));
}
`);
  expectLimit(run, "more than 128 MiB of heap");
  expect(await events(api, run.id)).toEqual(["run_started", "run_aborted"]);
  await expectHealthy(tenant);
});

test("built-ins asked for more than the heap holds end at once: a huge fill, a huge string", { tag: "@attack" }, async () => {
  const fill = await runToEnd(`export default async function main() {
  const filled = new Array(2 ** 27).fill(1.5);
  return filled.length;
}
`);
  expectLimit(fill, "more than 128 MiB of heap");
  const text = await runToEnd(`export default async function main() {
  const s = "xy".repeat(2 ** 27);
  return s.toUpperCase().length;
}
`);
  expectLimit(text, "more than 128 MiB of heap");
  await expectHealthy(tenant);
});

test("array buffers past their budget throw a catchable RangeError, resizable ones included", { tag: "@attack" }, async () => {
  const caught = await runToEnd(`export default async function main() {
  const kept: ArrayBuffer[] = [];
  let hoarded = "";
  try {
    for (;;) kept.push(new ArrayBuffer(16 * 1024 * 1024));
  } catch (error) {
    hoarded = \`\${(error as Error).name}: \${(error as Error).message}\`;
  }
  let resizable = "";
  try {
    new ArrayBuffer(1, { maxByteLength: 2 ** 30 });
    resizable = "made";
  } catch (error) {
    resizable = \`\${(error as Error).name}: \${(error as Error).message}\`;
  }
  return { hoarded, resizable, kept: kept.length };
}
`);
  expect(caught.status, JSON.stringify(caught)).toBe("completed");
  expect(caught.output.value.hoarded).toBe("RangeError: Array buffer allocation failed");
  expect(caught.output.value.resizable).toBe("RangeError: Array buffer allocation failed");
  expect(caught.output.value.kept).toBeLessThanOrEqual(8);
  const uncaught = await runToEnd(`export default async function main() { return new ArrayBuffer(2 ** 36).byteLength; }\n`);
  expectFailed(uncaught, "RangeError", "Array buffer allocation failed");
  await expectHealthy(tenant);
});

test("10,001 timers in one activation exceed the commands' bound, and none is started", { tag: "@attack" }, async () => {
  const run = await runToEnd(`import { sleep } from "rewind";
export default async function main() {
  await Promise.all(Array.from({ length: 10_001 }, () => sleep(1)));
}
`);
  expectLimit(run, "more than 10000 commands in one activation");
  expect(await events(api, run.id)).toEqual(["run_started", "run_aborted"]);
  expect(run.waitingFor ?? []).toEqual([]);
  await expectHealthy(tenant);
});

test("infinite recursion throws a RangeError the script could catch, and nothing else breaks", { tag: "@attack" }, async () => {
  const run = await runToEnd(`function deeper(n: number): number {
  return deeper(n + 1) + 1;
}
export default async function main() {
  return deeper(0);
}
`);
  expectFailed(run, "RangeError", "Maximum call stack size exceeded");
  await expectHealthy(tenant);
});

test("logs past 64 MiB in one activation abort the run, and none of them is recorded", { tag: "@attack" }, async () => {
  const run = await runToEnd(`export default async function main() {
  const line = "x".repeat(1024 * 1024);
  for (let i = 0; i < 100; i++) console.log(line);
}
`);
  expectLimit(run, "emitted more than 64 MiB of commands and logs reacting to one trigger");
  expect(await events(api, run.id)).toEqual(["run_started", "run_aborted"]);
  expect(run.logs ?? []).toEqual([]);
  await expectHealthy(tenant);
});
