/**
 * Area "Runs" of the release gate: a run's life through the HTTP API on setup A (deployed).
 * Starts with `wait`, labels, TTL and trace context; idempotent starts and signals; mailboxes,
 * timeouts and sleeps; cancellation; the TTL's deadline; forks; erasure; the journal, the cassette,
 * the event stream, the trace and single entries (`archi/03-execution.md`, `archi/07`'s table,
 * `api/README.md`).
 */
import { type McpFixture, startMcpFixture } from "../support/fixtures.js";
import {
  type Api,
  evidence,
  expect,
  expectOk,
  expectProblem,
  expectRefused,
  newTenant,
  type Tenant,
  test,
  unique,
} from "../support/gate.js";
import { between, ended, events, follow, getRun, journal, parseSse, runIds, started, startRun, waitingFor } from "../support/runs-helpers.js";

let tenant: Tenant;
let mcp: McpFixture;
let toolset: string;
/** The owner's raw HTTP in the namespace `alice`. */
let alice: Api;

const WAITS_FOR_GO = `import { signal } from "rewind";
export default async function main() {
  const go = await signal("go");
  return { go };
}
`;

test.beforeAll(async () => {
  tenant = await newTenant("runs");
  mcp = await startMcpFixture();
  const created = await tenant.api.post("/v1/toolsets", { mcp: [{ name: "fx", url: mcp.url }] });
  expectOk(created);
  toolset = created.json.id;
  alice = tenant.api.ns("alice");
});

test.afterAll(async () => {
  await mcp?.close();
});

/** The calls the fixture received for this tool. */
const callsOf = (tool: string) => mcp.calls.filter((c) => c.tool === tool);

test("a run started with wait completes and answers its output, input, labels and namespace", { tag: "@success" }, async () => {
  const run = await started(alice, {
    script: `export default async function main(input: { a: number; b: number }) {\n  return { sum: input.a + input.b };\n}\n`,
    input: { a: 2, b: 40 },
    labels: { suite: "runs", conversation: "c-1" },
    wait: "30s",
  });
  expect(run.status, JSON.stringify(run)).toBe("completed");
  expect(run.output.value).toEqual({ sum: 42 });
  expect(run.input.value).toEqual({ a: 2, b: 40 });
  expect(run.labels).toEqual({ suite: "runs", conversation: "c-1" });
  expect(run.namespace).toBe("alice");
  expect(run.mode).toBe("live");
  expect(run.epoch).toBeGreaterThanOrEqual(1);
  expect(run.endedAt).toBeTruthy();
  // The default TTL is 30 days (archi/03, "Run TTL").
  expect(Math.round(between(run.createdAt, run.expiresAt) / 3_600_000)).toBe(30 * 24);
});

test("wait answers once it elapses, with the run still active and what it waits for", { tag: "@success" }, async () => {
  const run = await started(alice, { script: WAITS_FOR_GO, wait: "1s" });
  expect(run.status).toBe("active");
  expect(run.waitingFor.map((w: any) => [w.kind, w.name])).toContainEqual(["signal", "go"]);
  await alice.post(`/v1/runs/${run.id}/cancel`, {});
});

test("a TTL given at start sets the deadline, and the trace context makes the run's trace the caller's", { tag: "@success" }, async () => {
  const traceId = "4bf92f3577b34da6a3ce929d0e0e4736";
  const run = await started(alice, {
    script: WAITS_FOR_GO,
    ttl: "2h",
    traceparent: `00-${traceId}-00f067aa0ba902b7-01`,
  });
  expect(between(run.createdAt, run.expiresAt)).toBe(2 * 3_600_000);
  expect(run.traceId).toBe(traceId);
  await alice.post(`/v1/runs/${run.id}/cancel`, {});
});

test("the same Idempotency-Key starts one run per namespace, and a retry runs nothing twice", { tag: "@success" }, async () => {
  const key = unique("start-");
  const body = {
    toolset,
    script: `import { fx } from "rewind:tools";\nexport default async function main() {\n  return await fx.echo({ text: "${key}" });\n}\n`,
    wait: "30s",
  };
  const first = await startRun(alice, body, { key });
  expectOk(first, 201);
  const again = await startRun(alice, body, { key });
  expectOk(again, 200);
  expect(again.json.id).toBe(first.json.id);
  // The key is unique per namespace: Bob's request with it is another run.
  const bob = await startRun(tenant.api.ns("bob"), body, { key });
  expectOk(bob, 201);
  expect(bob.json.id).not.toBe(first.json.id);
  await ended(alice, first.json.id);
  await ended(tenant.api.ns("bob"), bob.json.id);
  // Alice's retry called the tool once; Bob's run called it once.
  expect(mcp.calls.filter((c) => c.tool === "echo" && c.args.text === key)).toHaveLength(2);
});

test("signals wait in their mailbox, come out first in first out, and a timeout gives undefined", { tag: "@success" }, async () => {
  const run = await started(alice, {
    script: `import { signal, sleep } from "rewind";
export default async function main() {
  await sleep("1s");
  const first = await signal("note");
  const second = await signal("note");
  const missing = await signal("never", { timeout: "1s" });
  return { first, second, missing: missing ?? null };
}
`,
  });
  // Both arrive while the script sleeps: nothing is lost.
  for (const text of ["one", "two"]) {
    const sent = await alice.post(`/v1/runs/${run.id}/signals/note`, { text });
    expectOk(sent, 202);
    expect(sent.json).toMatchObject({ runId: run.id, name: "note", duplicate: false });
  }
  const done = await ended(alice, run.id);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value).toEqual({ first: { text: "one" }, second: { text: "two" }, missing: null });
});

test("a signal sent twice with one Idempotency-Key is delivered once", { tag: "@success" }, async () => {
  const run = await started(alice, {
    script: `import { signal } from "rewind";
export default async function main() {
  const first = await signal("go");
  const second = await signal("go", { timeout: "2s" });
  return { first, second: second ?? null };
}
`,
  });
  const key = unique("signal-");
  const sent = await alice.post(`/v1/runs/${run.id}/signals/go`, 1, { headers: { "idempotency-key": key } });
  expectOk(sent, 202);
  expect(sent.json.duplicate).toBe(false);
  const again = await alice.post(`/v1/runs/${run.id}/signals/go`, 1, { headers: { "idempotency-key": key } });
  expectOk(again, 202);
  expect(again.json.duplicate).toBe(true);
  const done = await ended(alice, run.id);
  expect(done.output.value).toEqual({ first: 1, second: null });
});

test("a sleeping run waits for its timer, then resumes on its own", { tag: "@success" }, async () => {
  const run = await started(alice, {
    script: `import { sleep } from "rewind";
export default async function main() {
  const before = Date.now();
  await sleep("1500ms");
  return Date.now() - before;
}
`,
  });
  const waiting = await waitingFor(alice, run.id, "timer");
  const timer = waiting.waitingFor.find((w: any) => w.kind === "timer");
  expect(Date.parse(timer.fireAt) - Date.parse(run.createdAt), "the timer's due time").toBeGreaterThanOrEqual(1000);
  const done = await ended(alice, run.id);
  expect(done.status).toBe("completed");
  // Logical time moved by at least the sleep.
  expect(done.output.value).toBeGreaterThanOrEqual(1500);
  expect(await events(alice, run.id)).toEqual(["run_started", "start_timer", "timer_fired", "complete_run"]);
});

test("a cancelled run ends cancelled with a CancelledError carrying the reason", { tag: "@success" }, async () => {
  const run = await started(alice, { script: WAITS_FOR_GO });
  await waitingFor(alice, run.id, "signal", "go");
  const cancelled = await alice.post(`/v1/runs/${run.id}/cancel`, { reason: "user asked", wait: "20s" });
  expectOk(cancelled, 202);
  const done = cancelled.json.status === "cancelled" ? cancelled.json : await ended(alice, run.id);
  expect(done.status, JSON.stringify(done)).toBe("cancelled");
  expect(done.error).toMatchObject({ name: "CancelledError", message: "run cancelled: user asked" });
  expect(await events(alice, run.id)).toEqual(["run_started", "signal_awaited", "cancel_requested", "fail_run"]);
});

test("a run past its TTL is cancelled through the normal path, reason ttl", { tag: "@success" }, async () => {
  const run = await started(alice, { script: WAITS_FOR_GO, ttl: "2s" });
  const done = await ended(alice, run.id, 60_000);
  expect(done.status, JSON.stringify(done)).toBe("cancelled");
  expect(done.error).toMatchObject({ name: "CancelledError" });
  expect(done.error.message).toContain("its deadline has passed");
  expect(await events(alice, run.id)).toEqual(["run_started", "signal_awaited", "cancel_requested", "fail_run"]);
  evidence(`ended ${between(run.expiresAt, done.endedAt)} ms after its deadline`);
});

test("a fork reuses its parent's recorded calls and seed, and stays in its namespace", { tag: "@success" }, async () => {
  const id = unique("cus-");
  const parent = await started(alice, {
    toolset,
    input: { id },
    wait: "30s",
    script: `import { fx } from "rewind:tools";
export default async function main(input: { id: string }) {
  const customer = await fx.lookup({ id: input.id });
  const draw = Math.random();
  throw new Error(\`no tier for \${customer.name} (\${draw})\`);
}
`,
  });
  expect(parent.status, JSON.stringify(parent)).toBe("failed");
  const draw = Number(/\(([0-9.e-]+)\)$/.exec(parent.error.message)?.[1]);
  expect(Number.isFinite(draw), parent.error.message).toBe(true);
  expect(callsOf("lookup").filter((c) => c.args.id === id)).toHaveLength(1);

  const forked = await alice.post(`/v1/runs/${parent.id}/fork`, {
    wait: "30s",
    script: `import { fx } from "rewind:tools";
export default async function main(input: { id: string }) {
  const customer = await fx.lookup({ id: input.id });
  const draw = Math.random();
  return { name: customer.name, tier: customer.tier, draw };
}
`,
  });
  expectOk(forked, 201);
  const fork = forked.json.status === "completed" ? forked.json : await ended(alice, forked.json.id);
  expect(fork.status, JSON.stringify(fork)).toBe("completed");
  expect(fork.id).not.toBe(parent.id);
  expect(fork.parent).toMatchObject({ run: parent.id });
  expect(fork.namespace).toBe("alice");
  // The recorded call was copied, not made again; the seed is the parent's.
  expect(fork.output.value).toEqual({ name: `Customer ${id}`, tier: "gold", draw });
  expect(callsOf("lookup").filter((c) => c.args.id === id)).toHaveLength(1);
});

test("a fork forced live at an entry, or given another input, calls again under its own key", { tag: "@success" }, async () => {
  const id = unique("cus-");
  const script = `import { fx } from "rewind:tools";
export default async function main(input: { id: string }) {
  return await fx.lookup({ id: input.id });
}
`;
  const parent = await started(alice, { toolset, input: { id }, wait: "30s", script });
  expect(parent.status).toBe("completed");
  const entries = (await journal(alice, parent.id)).entries as { index: number; event: { type: string } }[];
  const scheduled = entries.find((e) => e.event.type === "schedule_tool_call")!;

  const atEntry = await alice.post(`/v1/runs/${parent.id}/fork`, { script, at: scheduled.index, wait: "30s" });
  expectOk(atEntry, 201);
  const live = atEntry.json.status === "completed" ? atEntry.json : await ended(alice, atEntry.json.id);
  expect(live.status, JSON.stringify(live)).toBe("completed");
  expect(live.parent).toMatchObject({ run: parent.id, at: scheduled.index });
  const again = callsOf("lookup").filter((c) => c.args.id === id);
  expect(again).toHaveLength(2);
  expect(String(again[1]!.meta?.["codes.rewind/idempotency-key"])).toMatch(new RegExp(`^${live.id}/`));

  const other = unique("cus-");
  const withInput = await alice.post(`/v1/runs/${parent.id}/fork`, { script, input: { id: other }, wait: "30s" });
  expectOk(withInput, 201);
  const forkedInput = withInput.json.status === "completed" ? withInput.json : await ended(alice, withInput.json.id);
  expect(forkedInput.output.value).toMatchObject({ id: other });
  expect(forkedInput.input.value).toEqual({ id: other });
  expect(callsOf("lookup").filter((c) => c.args.id === other)).toHaveLength(1);
});

test("an erased run is gone: its state, journal and events answer 404", { tag: "@success" }, async () => {
  const run = await started(alice, {
    script: `export default async function main(input: { secret: string }) { return input.secret.length; }\n`,
    input: { secret: "personal data" },
    wait: "30s",
  });
  expect(run.status).toBe("completed");
  const erased = await alice.delete(`/v1/runs/${run.id}`);
  expect(erased.status, erased.text).toBe(204);
  for (const path of [`/v1/runs/${run.id}`, `/v1/runs/${run.id}/journal`, `/v1/runs/${run.id}/trace`, `/v1/runs/${run.id}/entries/0`]) {
    expectProblem(await alice.get(path), 404, "not-found");
  }
  expect(await runIds(alice)).not.toContain(run.id);
});

test("the journal and the cassette export what the run did", { tag: "@success" }, async () => {
  const id = unique("cus-");
  const run = await started(alice, {
    toolset,
    input: { id },
    script: `import { fx } from "rewind:tools";
import { signal } from "rewind";
export default async function main(input: { id: string }) {
  const customer = await fx.lookup({ id: input.id });
  const decision = await signal("decision");
  return { customer: customer.id, decision };
}
`,
  });
  await waitingFor(alice, run.id, "signal", "decision");
  expectOk(await alice.post(`/v1/runs/${run.id}/signals/decision`, { refund: 10 }), 202);
  const done = await ended(alice, run.id);
  expect(done.status).toBe("completed");

  const document = await journal(alice, run.id);
  expect(document.run).toBe(run.id);
  expect(typeof document.script === "string" || typeof document.script === "object").toBe(true);
  expect(document.entries.map((e: any) => e.index)).toEqual(document.entries.map((_: unknown, n: number) => n));
  expect(document.entries.map((e: any) => e.event.type)).toEqual([
    "run_started",
    "schedule_tool_call",
    "tool_call_completed",
    "signal_awaited",
    "signal_received",
    "complete_run",
  ]);
  expect(document.entries.length).toBe(done.historyLen);

  const cassette = await alice.get(`/v1/runs/${run.id}/journal`, { query: { as: "cassette" } });
  expectOk(cassette, 200);
  expect(cassette.text).toContain("fx.lookup");
  expect(cassette.text).toContain(id);
  expect(cassette.text).toContain("decision");
});

test("the change stream announces each change of a run, as GET /v1/runs lists it", { tag: "@success" }, async () => {
  const changes = await follow(tenant.api, "/v1/changes");
  try {
    expect(changes.status).toBe(200);
    expect(changes.contentType).toContain("text/event-stream");
    const run = await started(alice, { script: WAITS_FOR_GO });
    const of = (status: string) => (e: { event: string; data: any }) =>
      e.event === "run" && e.data?.id === run.id && e.data?.status === status;
    expect(await changes.until(of("active"), 15_000), "the stream announces the run active").toBeDefined();
    await waitingFor(alice, run.id, "signal", "go");
    expectOk(await alice.post(`/v1/runs/${run.id}/signals/go`, "now"), 202);
    const done = await changes.until(of("completed"), 15_000);
    expect(done, "the stream announces the run completed").toBeDefined();
    const listed = await alice.get("/v1/runs", { query: { q: run.id } });
    expectOk(listed, 200);
    expect(done!.data).toEqual((listed.json.runs as { id: string }[]).find((r) => r.id === run.id));
    const announced = changes.events.filter((e) => e.event === "run" && e.data?.id === run.id).length;
    evidence(`${announced} run events for ${run.id}, active then completed; the last equals its item of GET /v1/runs`);
  } finally {
    changes.close();
  }
});

test("the event stream sends every entry then the end, from any entry, and follows a live run", { tag: "@success" }, async () => {
  const run = await started(alice, { script: WAITS_FOR_GO });
  await waitingFor(alice, run.id, "signal", "go");

  // Follow: the stream stays open until the run ends.
  const followed = alice.get(`/v1/runs/${run.id}/events`, { query: { follow: 1 } });
  await new Promise((resolve) => setTimeout(resolve, 500));
  expectOk(await alice.post(`/v1/runs/${run.id}/signals/go`, "now"), 202);
  const stream = await followed;
  expectOk(stream, 200);
  expect(stream.headers.get("content-type") ?? "").toContain("text/event-stream");
  const live = parseSse(stream.text);
  expect(live.filter((e) => e.event === "entry").map((e) => e.data.event.type)).toEqual([
    "run_started",
    "signal_awaited",
    "signal_received",
    "complete_run",
  ]);
  expect(live.at(-1)).toMatchObject({ event: "end", data: { status: "completed" } });
  expect(live.filter((e) => e.event === "entry").map((e) => Number(e.id))).toEqual([0, 1, 2, 3]);

  // Without follow, from entry 2.
  const resumed = await alice.get(`/v1/runs/${run.id}/events`, { query: { from: 2 } });
  const tail = parseSse(resumed.text);
  expect(tail.filter((e) => e.event === "entry").map((e) => e.data.idx)).toEqual([2, 3]);
  expect(tail.at(-1)?.event).toBe("end");
});

test("the LLM trace and one entry in full describe the run", { tag: "@success" }, async () => {
  const id = unique("cus-");
  const run = await started(alice, {
    toolset,
    input: { id },
    wait: "30s",
    script: `import { fx } from "rewind:tools";
export default async function main(input: { id: string }) {
  console.log("looking up", input.id);
  return await fx.lookup({ id: input.id });
}
`,
  });
  expect(run.status).toBe("completed");
  expect(run.logs.map((line: any) => line.message)).toEqual([`looking up ${id}`]);
  expect(run.logs[0]).toMatchObject({ level: "info", callsite: { line: 3 } });
  const trace = await alice.get(`/v1/runs/${run.id}/trace`);
  expectOk(trace, 200);
  expect(trace.json).toMatchObject({ runId: run.id, format: "llm" });
  expect(trace.json.text).toContain("fx.lookup");
  expect(trace.json.text).toContain(id);
  // Entry 1 is the log line; entry 2 the call.
  const entry = await alice.get(`/v1/runs/${run.id}/entries/2`);
  expectOk(entry, 200);
  expect(entry.json.idx).toBe(2);
  expect(entry.json.event.type).toBe("schedule_tool_call");
  expect(JSON.stringify(entry.json)).toContain(id);
});

// ---------------------------------------------------------------------------------------------
// Failures

test("a signal or a cancellation to a run that has ended is 409 run-ended", { tag: "@failure" }, async () => {
  const run = await started(alice, { script: `export default async function main() { return 1; }\n`, wait: "30s" });
  expect(run.status).toBe("completed");
  expectProblem(await alice.post(`/v1/runs/${run.id}/signals/go`, true), 409, "run-ended");
  expectProblem(await alice.post(`/v1/runs/${run.id}/cancel`, {}), 409, "run-ended");
});

test("releasing or giving up a run that is not quarantined is 409 not-quarantined", { tag: "@failure" }, async () => {
  const run = await started(alice, { script: WAITS_FOR_GO });
  await waitingFor(alice, run.id, "signal", "go");
  expectProblem(await alice.post(`/v1/runs/${run.id}/release`), 409, "not-quarantined");
  expectProblem(await alice.post(`/v1/runs/${run.id}/give-up`), 409, "not-quarantined");
  expect((await getRun(alice, run.id)).status).toBe("active");
  await alice.post(`/v1/runs/${run.id}/cancel`, {});
});

test("a TTL above the deployment's maximum is 400 invalid-request naming the TTL, and no run starts", { tag: "@failure" }, async () => {
  const before = await runIds(alice);
  const reply = await startRun(alice, { script: WAITS_FOR_GO, ttl: "400d" });
  expectProblem(reply, 400, "invalid-request");
  expect(reply.json.detail).toMatch(/TTL/i);
  expect(await runIds(alice)).toEqual(before);
});

test("starting a run without Rewind-Namespace is 400 invalid-request: there is no default namespace", { tag: "@failure" }, async () => {
  const reply = await startRun(tenant.api, { script: WAITS_FOR_GO });
  expectProblem(reply, 400, "invalid-request");
  expect(reply.json.detail).toMatch(/namespace/i);
});

test("an unknown run is 404 on every endpoint of a run", { tag: "@failure" }, async () => {
  const id = "01900000-0000-7000-8000-000000000000";
  const replies = await Promise.all([
    alice.get(`/v1/runs/${id}`),
    alice.post(`/v1/runs/${id}/signals/go`, 1),
    alice.post(`/v1/runs/${id}/cancel`, {}),
    alice.post(`/v1/runs/${id}/fork`, { script: "export default async function main() {}\n" }),
    alice.get(`/v1/runs/${id}/journal`),
    alice.get(`/v1/runs/${id}/events`),
    alice.get(`/v1/runs/${id}/trace`),
    alice.get(`/v1/runs/${id}/entries/0`),
    alice.post(`/v1/runs/${id}/release`),
    alice.delete(`/v1/runs/${id}`),
  ]);
  for (const reply of replies) expectProblem(reply, 404, "not-found");
});

test("an entry past the end of the journal is 404, and an unknown toolset refuses the start", { tag: "@failure" }, async () => {
  const run = await started(alice, { script: `export default async function main() { return 1; }\n`, wait: "30s" });
  expectProblem(await alice.get(`/v1/runs/${run.id}/entries/${run.historyLen + 5}`), 404, "not-found");
  const unknown = await startRun(alice, { script: WAITS_FOR_GO, toolset: "0".repeat(64) });
  expectProblem(unknown, 404, "not-found");
});

test("an unreadable wait or TTL is 400 invalid-request", { tag: "@failure" }, async () => {
  expectProblem(await startRun(alice, { script: WAITS_FOR_GO, wait: "forever" }), 400, "invalid-request");
  expectProblem(await startRun(alice, { script: WAITS_FOR_GO, ttl: "-1h" }), 400, "invalid-request");
  expectProblem(await alice.get("/v1/runs/whatever", { query: { wait: "soon" } }), 400, "invalid-request");
});

test("an input or a signal payload above 8 MiB is 413 payload-too-large, before anything is stored", { tag: "@failure" }, async () => {
  const before = await runIds(alice);
  const big = "x".repeat(8 * 1024 * 1024 + 16);
  const reply = await startRun(alice, { script: WAITS_FOR_GO, input: { big } });
  expectProblem(reply, 413, "payload-too-large");
  expect(await runIds(alice)).toEqual(before);
});

// ---------------------------------------------------------------------------------------------
// Attacks

test("a start replayed with its Idempotency-Key never runs the script's effects twice", { tag: "@attack" }, async () => {
  const key = unique("replay-");
  const body = {
    toolset,
    wait: "30s",
    script: `import { fx } from "rewind:tools";\nexport default async function main() {\n  return await fx.echo({ text: "${key}" });\n}\n`,
  };
  const replies = await Promise.all(Array.from({ length: 5 }, () => startRun(alice, body, { key })));
  const ids = new Set(replies.map((r) => r.json?.id));
  expect(ids.size, replies.map((r) => `${r.status} ${r.text.slice(0, 200)}`).join("\n")).toBe(1);
  expect(replies.filter((r) => r.status === 201)).toHaveLength(1);
  await ended(alice, [...ids][0]!);
  expect(mcp.calls.filter((c) => c.tool === "echo" && c.args.text === key)).toHaveLength(1);
  evidence("5 concurrent starts with one key: one run created (201), four answered with it (200), one call");
});

test("a forged rewind:ended signal cannot fake the end of a run", { tag: "@attack" }, async () => {
  const run = await started(alice, { script: WAITS_FOR_GO });
  await waitingFor(alice, run.id, "signal", "go");
  for (const name of [`rewind:ended:${run.id}`, "rewind:go", "rewind:"]) {
    await expectRefused({
      attempt: () => alice.post(`/v1/runs/${run.id}/signals/${encodeURIComponent(name)}`, { status: "completed" }),
      status: 422,
      slug: "invalid-signal",
      probe: async () => {
        const state = await getRun(alice, run.id);
        return { status: state.status, historyLen: state.historyLen };
      },
    });
  }
  evidence("422 invalid-signal for every name starting with rewind:; the run's journal unchanged");
  await alice.post(`/v1/runs/${run.id}/cancel`, {});
});

test("a signal payload above 8 MiB is refused and the run's journal does not grow", { tag: "@attack" }, async () => {
  const run = await started(alice, { script: WAITS_FOR_GO });
  await waitingFor(alice, run.id, "signal", "go");
  await expectRefused({
    attempt: () => alice.post(`/v1/runs/${run.id}/signals/go`, "y".repeat(8 * 1024 * 1024 + 16)),
    status: 413,
    slug: "payload-too-large",
    probe: async () => (await getRun(alice, run.id)).historyLen,
  });
  evidence("413 payload-too-large; history length unchanged");
  await alice.post(`/v1/runs/${run.id}/cancel`, {});
});
