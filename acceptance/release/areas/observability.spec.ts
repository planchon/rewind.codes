/**
 * Observability (`archi/05-testing-and-observability.md`, "Observability"), on setup A: the read
 * model searched with the query language, statistics, failure groups, scripts, tool statistics,
 * payloads read on demand; one OpenTelemetry trace per run in the Collector; lifecycle webhooks
 * signed with Standard Webhooks, and `quota.alert`. The attacks: the query language as an
 * injection vector, logs and payloads kept out of text search, forged deliveries, and a payload
 * read through a run that does not hold it.
 */
import { createHash } from "node:crypto";
import { verifyWebhook } from "@rewind.codes/sdk";
import { flattenSpans, HarnessSpan, parseTraceparent } from "../../support/otlp.js";
import { eventually } from "../../support/processes.js";
import { type McpFixture, startMcpFixture, startWebhookReceiver, type WebhookReceiver } from "../support/fixtures.js";
import { evidence, expect, expectOk, expectProblem, expectRefused, newTenant, stacks, type Tenant, test, unique } from "../support/gate.js";
import { ended, startRun, waitingCall } from "../support/tools-helpers.js";
import { readFileSync, existsSync } from "node:fs";

let t: Tenant;
let mcp: McpFixture;
let receiver: WebhookReceiver;
let secret: string;
let toolset: string;

test.beforeAll(async () => {
  [t, mcp, receiver] = await Promise.all([newTenant("observability"), startMcpFixture(), startWebhookReceiver()]);
  const subscription = await t.api.post("/v1/subscriptions", { url: receiver.url });
  expectOk(subscription, 201);
  secret = subscription.json.secret;
  const created = await t.api.post("/v1/toolsets", { mcp: [{ name: "fx", url: mcp.url }] });
  expectOk(created);
  toolset = created.json.id;
});

test.afterAll(async () => {
  await Promise.all([mcp?.close(), receiver?.close()]);
});

const alice = () => t.api.ns("alice");

const SCRIPTS = {
  lookup: `import { fx } from "rewind:tools";

export default async function main() {
  return (await fx.lookup({ id: "7" })).name;
}
`,
  rateLimited: `export default async function main() {
  throw new Error("rate limit reached for 42");
}
`,
  waiting: `import { signal } from "rewind";

export default async function main() {
  return await signal<{ ok: boolean }>("approval");
}
`,
  toolFails: `import { fx } from "rewind:tools";

export default async function main() {
  await fx.fail({ reason: "denied" });
}
`,
};

/** The ids of the runs a query matches, sorted. */
async function query(q: string, extra: Record<string, string | string[]> = {}): Promise<string[]> {
  const reply = await t.api.get("/v1/runs", { query: { q, limit: 100, ...extra } });
  expectOk(reply);
  return (reply.json.runs as { id: string }[]).map((r) => r.id).sort();
}

/** Starts the runs of a query case, labelled `case:<c>`, and waits until the read model has them settled. */
async function corpus(c: string) {
  const start = (script: string, labels: Record<string, string>) => startRun(alice(), { toolset, script, labels: { case: c, ...labels } });
  const [completed, rateLimited, waiting, toolFails] = await Promise.all([
    start(SCRIPTS.lookup, { agent: "triage" }),
    start(SCRIPTS.rateLimited, { agent: "billing" }),
    start(SCRIPTS.waiting, { agent: "triage" }),
    start(SCRIPTS.toolFails, { agent: "support-bot" }),
  ]);
  const ids = { completed: completed.id as string, rateLimited: rateLimited.id as string, waiting: waiting.id as string, toolFails: toolFails.id as string };
  await eventually(
    "the read model has every run settled",
    async () => {
      const reply = await t.api.get("/v1/runs", { query: { q: `@case:${c}`, limit: 100 } });
      const runs = reply.json.runs as { id: string; status: string; waiting?: unknown }[];
      const status = Object.fromEntries(runs.map((r) => [r.id, r.status]));
      const settled =
        status[ids.completed] === "completed" &&
        status[ids.rateLimited] === "failed" &&
        status[ids.toolFails] === "failed" &&
        status[ids.waiting] === "active" &&
        (await query(`@case:${c} waiting:signal`)).length === 1;
      return settled ? true : undefined;
    },
    60_000,
    250,
  );
  return ids;
}

// ---------------------------------------------------------------------------------------------
// Success

test("the query language finds runs by label, status, tool, error, wait and text", { tag: "@success" }, async () => {
  const c = unique("q");
  const r = await corpus(c);
  const all = Object.values(r).sort();
  const sorted = (...ids: string[]) => ids.sort();
  const cases: [string, string[]][] = [
    [`@case:${c}`, all],
    [`@case:${c} @agent:triage`, sorted(r.completed, r.waiting)],
    [`@case:${c} @agent:tri*`, sorted(r.completed, r.waiting)],
    [`@case:${c} @agent:*`, all],
    [`@case:${c} @agent:triage,billing`, sorted(r.completed, r.waiting, r.rateLimited)],
    [`@case:${c} status:failed`, sorted(r.rateLimited, r.toolFails)],
    [`@case:${c} -status:failed`, sorted(r.completed, r.waiting)],
    [`@case:${c} status:completed,active`, sorted(r.completed, r.waiting)],
    [`@case:${c} tool:fx.lookup`, [r.completed]],
    [`@case:${c} tool:fx.*`, sorted(r.completed, r.toolFails)],
    [`@case:${c} error:ToolError`, [r.toolFails]],
    [`@case:${c} error:Error`, [r.rateLimited]],
    [`@case:${c} error:error`, [r.toolFails]],
    [`@case:${c} waiting:signal`, [r.waiting]],
    [`@case:${c} waiting:*`, [r.waiting]],
    [`@case:${c} signal:approval`, [r.waiting]],
    [`@case:${c} "rate limit"`, [r.rateLimited]],
    [`@case:${c} rate`, [r.rateLimited]],
    [`@case:${c} namespace:alice`, all],
    [`@case:${c} namespace:bob`, []],
    [`@case:${c} calls:>=1`, sorted(r.completed, r.toolFails)],
    [`@case:${c} errors:>0`, [r.toolFails]],
    [`@case:${c} id:${r.waiting}`, [r.waiting]],
    [`@case:${c} started:>now-1h`, all],
    [`@case:${c} started:<now-1h`, []],
  ];
  for (const [q, expected] of cases) {
    expect(await query(q), q).toEqual(expected);
  }
  // The same filters as parameters.
  expect(await query(`@case:${c}`, { label: ["agent:triage"], status: "failed" })).toEqual([]);
  expect(await query(`@case:${c}`, { tool: "fx.lookup" })).toEqual([r.completed]);
  expect(await query(`@case:${c}`, { error: "ToolError" })).toEqual([r.toolFails]);
  evidence(`${cases.length} queries answered exactly`);
});

test("statistics count the runs a query matches, by status and by wait", { tag: "@success" }, async () => {
  const c = unique("s");
  await corpus(c);
  const reply = await t.api.get("/v1/runs/stats", { query: { q: `@case:${c}` } });
  expectOk(reply);
  expect(reply.json.runs).toBe(4);
  expect(reply.json.statuses).toMatchObject({ completed: 1, failed: 2, active: 1, cancelled: 0, aborted: 0 });
  expect(reply.json.waiting.signal).toBe(1);
});

test("failures are grouped by fingerprint, numbers and quoted strings set aside", { tag: "@success" }, async () => {
  const c = unique("f");
  const fail = (order: number, quoted: string) =>
    startRun(alice(), { script: `export default async function main() {\n  throw new Error('order ${order} of "${quoted}" not found');\n}\n`, labels: { case: c } });
  const runs = await Promise.all([fail(1234, "ada"), fail(99, "grace"), startRun(alice(), { script: SCRIPTS.rateLimited, labels: { case: c } })]);
  for (const run of runs) await ended(alice(), run.id);
  const groups = await eventually(
    "the failures are grouped",
    async () => {
      const reply = await t.api.get("/v1/runs/failures", { query: { q: `@case:${c}` } });
      expectOk(reply);
      return reply.json.runs === 3 ? reply.json : undefined;
    },
    60_000,
  );
  expect(groups.totalGroups).toBe(2);
  const orders = groups.groups.find((g: any) => g.runs === 2);
  expect(orders.error).toBe("Error");
  expect(orders.signature).toContain("<n>");
  expect(orders.signature).not.toContain("1234");
  expect(orders.fingerprint).toMatch(/^[0-9a-f]{16}$/);
  expect(await query(`@case:${c} fingerprint:${orders.fingerprint}`)).toEqual([runs[0]!.id, runs[1]!.id].sort());
  evidence(`signature: ${orders.signature}`);
});

test("scripts are listed once per source, with their runs and tools, and read whole", { tag: "@success" }, async () => {
  const c = unique("sc");
  const source = `import { fx } from "rewind:tools";\n// ${c}\nexport default async function main() {\n  return (await fx.lookup({ id: "1" })).tier;\n}\n`;
  const runs = await Promise.all([1, 2].map(() => startRun(alice(), { toolset, script: source, labels: { case: c } })));
  for (const run of runs) expect((await ended(alice(), run.id)).status).toBe("completed");
  const digest = createHash("sha256").update(source).digest("hex");
  expect(runs[0]!.script).toBe(digest);
  const listed = await eventually(
    "the script is listed with both runs",
    async () => {
      const reply = await t.api.get("/v1/scripts", { query: { label: [`case:${c}`] } });
      expectOk(reply);
      const found = (reply.json.scripts as any[]).find((s) => s.digest === digest);
      return found?.runs.completed === 2 ? reply.json.scripts : undefined;
    },
    60_000,
  );
  expect(listed).toHaveLength(1);
  expect(listed[0].tools).toEqual(["fx.lookup"]);
  expect(listed[0].headline).toBe(`import { fx } from "rewind:tools";`);
  const one = await t.api.get(`/v1/scripts/${digest}`);
  expectOk(one);
  expect(one.json.source).toBe(source);
});

test("tool statistics count each tool's calls, errors and their kinds", { tag: "@success" }, async () => {
  const server = await startMcpFixture();
  try {
    const own = await t.api.post("/v1/toolsets", { mcp: [{ name: "st", url: server.url }] });
    expectOk(own);
    const script = `import { st } from "rewind:tools";
import { ToolError } from "rewind";

export default async function main() {
  for (let n = 0; n < 3; n++) await st.lookup({ id: String(n) });
  for (let n = 0; n < 2; n++) {
    try { await st.fail({ reason: "no" }); } catch (error) { if (!(error instanceof ToolError)) throw error; }
  }
  return "done";
}
`;
    const run = await startRun(alice(), { toolset: own.json.id, script });
    expect((await ended(alice(), run.id)).status).toBe("completed");
    const stats = await eventually(
      "the tool statistics count the calls",
      async () => {
        const reply = await t.api.get("/v1/tools/stats", { query: { toolset: own.json.id } });
        expectOk(reply);
        const by = Object.fromEntries((reply.json.tools as any[]).map((s) => [s.tool, s]));
        return by["st.lookup"]?.calls === 3 && by["st.fail"]?.calls === 2 ? by : undefined;
      },
      60_000,
    );
    expect(stats["st.lookup"]).toMatchObject({ calls: 3, errors: 0, retries: 0 });
    expect(stats["st.fail"]).toMatchObject({ calls: 2, errors: 2, errorKinds: { error: 2 } });
  } finally {
    await server.close();
  }
});

test("a payload above 256 KiB is left out of the answer and read on demand through its run", { tag: "@success" }, async () => {
  const run = await startRun(alice(), { script: `export default async function main() {\n  return "x".repeat(300 * 1024);\n}\n` });
  const done = await ended(alice(), run.id);
  expect(done.status).toBe("completed");
  expect(done.output.bytes).toBe(300 * 1024 + 2);
  expect(done.output.value).toBeUndefined();
  expect(done.output.preview.length).toBeLessThan(1024);
  const payload = await t.api.get(`/v1/payloads/${done.output.digest}`, { query: { run: run.id } });
  expectOk(payload);
  expect(payload.json).toBe("x".repeat(300 * 1024));
});

test("each run is one trace in the Collector, under the harness's span", { tag: "@success" }, async () => {
  const collector = stacks().a.collector;
  expect(collector, "setup A has a Collector").toBeDefined();
  const harness = new HarnessSpan(`chat ${unique()}`);
  const script = `import { fx } from "rewind:tools";
import { sleep } from "rewind";

export default async function main() {
  const customer = await fx.lookup({ id: "trace" });
  await sleep("200ms");
  console.log("looked up", customer.id);
  return customer.tier;
}
`;
  const run = await startRun(alice(), { toolset, script, traceparent: harness.traceparent });
  expect(run.traceId).toBe(harness.traceId);
  expect((await ended(alice(), run.id)).status).toBe("completed");
  const mine = await eventually(
    "the run's spans are in the Collector",
    () => {
      if (!existsSync(collector!.traces)) return undefined;
      const lines = readFileSync(collector!.traces, "utf8").split("\n").filter((l) => l.trim() !== "").map((l) => JSON.parse(l));
      const spans = flattenSpans(lines).filter((s) => s.attributes["rewind.run_id"] === run.id);
      const names = new Set(spans.map((s) => s.name));
      return names.has("run") && names.has("fx.lookup") && names.has("timer") && names.has("activation") ? spans : undefined;
    },
    90_000,
    500,
  );
  expect(new Set(mine.map((s) => s.traceId))).toEqual(new Set([harness.traceId]));
  const runSpan = mine.find((s) => s.name === "run" && s.scope === "rewind.run")!;
  expect(runSpan.parentSpanId).toBe(harness.spanId);
  const call = mine.find((s) => s.name === "fx.lookup" && s.scope === "rewind.run")!;
  expect(call.parentSpanId).toBe(runSpan.spanId);
  expect(call.attributes["rewind.tool"]).toBe("fx.lookup");
  expect(mine.find((s) => s.name === "timer")!.parentSpanId).toBe(runSpan.spanId);
  for (const activation of mine.filter((s) => s.name === "activation")) expect(activation.parentSpanId).toBe(runSpan.spanId);
  // The MCP server got the tool call span as the parent of its own work.
  const received = mcp.calls.find((c) => c.args.id === "trace");
  expect(parseTraceparent(String(received?.meta?.traceparent))).toEqual({ traceId: harness.traceId, parentId: call.spanId });
  evidence(`${mine.length} spans in trace ${harness.traceId}: ${[...new Set(mine.map((s) => s.name))].join(", ")}`);
});

test("lifecycle webhooks arrive signed: completed, failed, suspended and cancelled", { tag: "@success" }, async () => {
  const c = unique("w");
  const completed = await startRun(alice(), { script: `export default async function main() { return 1; }\n`, labels: { case: c } });
  const failed = await startRun(alice(), { script: SCRIPTS.rateLimited, labels: { case: c } });
  const waiting = await startRun(alice(), { script: SCRIPTS.waiting, labels: { case: c } });
  await ended(alice(), completed.id);
  await ended(alice(), failed.id);
  await eventually("the waiting run is suspended", async () => ((await alice().get(`/v1/runs/${waiting.id}`)).json.waitingFor?.length ? true : undefined));
  expectOk(await alice().post(`/v1/runs/${waiting.id}/cancel`, {}));
  await ended(alice(), waiting.id);
  const expected: [string, string][] = [
    ["run.completed", completed.id],
    ["run.failed", failed.id],
    ["run.suspended", waiting.id],
    ["run.cancelled", waiting.id],
  ];
  const got = await eventually(
    "every lifecycle event is delivered",
    () => {
      const found = expected.map(([type, id]) => receiver.deliveries.find((d) => {
        const event = JSON.parse(d.body);
        return event.type === type && event.data?.run?.id === id;
      }));
      return found.every(Boolean) ? found : undefined;
    },
    90_000,
    250,
  );
  const ids = new Set<string>();
  for (const delivery of got) {
    const event = verifyWebhook(delivery!.body, delivery!.headers as never, secret);
    expect((event as any).data.run.namespace).toBe("alice");
    expect((event as any).data.run.labels).toEqual({ case: c });
    expect(delivery!.headers["webhook-signature"]).toMatch(/^v1,/);
    ids.add(delivery!.headers["webhook-id"]!);
  }
  expect(ids.size, "each delivery has its own webhook-id").toBe(4);
});

test("quota.alert is sent when a namespace crosses 80 % and 100 % of its runs per month, not before", { tag: "@success" }, async () => {
  const namespace = `quota-${unique()}`;
  expectOk(await t.api.put(`/v1/namespaces/${namespace}`, { exceptions: { runsPerMonth: 4 } }));
  const one = async (n: number) => {
    const run = await startRun(t.api.ns(namespace), { script: `export default async function main() { return ${n}; }\n` });
    await ended(t.api.ns(namespace), run.id);
  };
  const alerts = async () => {
    const listed = await t.api.get("/v1/organization/quota-alerts", { query: { namespace } });
    expectOk(listed);
    return (listed.json.alerts as any[]).map((a) => [a.quota, a.threshold]).sort();
  };
  // Three runs of four are 75 %: no threshold is crossed yet.
  for (const n of [1, 2, 3]) await one(n);
  expect(await alerts(), "3 of 4 runs is below 80 %").toEqual([]);
  // The fourth crosses 80 % and 100 % at once.
  await one(4);
  const alert = await eventually(
    "quota.alert at 100 % is delivered",
    () =>
      receiver.deliveries.find((d) => {
        const event = JSON.parse(d.body);
        return event.type === "quota.alert" && event.data?.namespace === namespace && event.data?.threshold === 100;
      }),
    90_000,
    250,
  );
  const event = verifyWebhook(alert.body, alert.headers as never, secret) as any;
  expect(event.data).toMatchObject({ namespace, quota: "runs_per_month", threshold: 100, used: 4, limit: 4 });
  expect(await alerts()).toEqual([
    ["runs_per_month", 100],
    ["runs_per_month", 80],
  ]);
});

// ---------------------------------------------------------------------------------------------
// Failure

test("a query that cannot be read is refused with the position of the term at fault", { tag: "@failure" }, async () => {
  for (const [q, term] of [
    ["status:failed nosuchfield:x", "nosuchfield:x"],
    ["status:", "status:"],
    ["status:failed started:>yesterday", "started:>yesterday"],
    ["calls:>=many", "calls:>=many"],
    ['"unterminated', '"unterminated'],
  ] as const) {
    const reply = await t.api.get("/v1/runs", { query: { q } });
    expectProblem(reply, 400, "invalid-query");
    const span = reply.json.query;
    expect(span, `${q}: ${reply.text}`).toBeDefined();
    expect(q.slice(span.start, span.end), `${q}: ${reply.text}`).toContain(term.slice(0, Math.min(term.length, 6)));
  }
});

test("statistics and failures refuse the same unreadable query", { tag: "@failure" }, async () => {
  expectProblem(await t.api.get("/v1/runs/stats", { query: { q: "status:nope" } }), 400, "invalid-query");
  expectProblem(await t.api.get("/v1/runs/failures", { query: { q: "fingerprint:" } }), 400, "invalid-query");
});

test("a list's parameters out of range are refused as invalid-request", { tag: "@failure" }, async () => {
  expectProblem(await t.api.get("/v1/runs", { query: { limit: -1 } }), 400, "invalid-request");
  expectProblem(await t.api.get("/v1/runs", { query: { status: "sleeping" } }), 400, "invalid-request");
  expectProblem(await t.api.get("/v1/runs/stats", { query: { since: "last tuesday" } }), 400, "invalid-request");
});

// ---------------------------------------------------------------------------------------------
// Attack

test("the query language is no injection vector: SQL and wildcards are text", { tag: "@attack" }, async () => {
  const c = unique("i");
  const r = await corpus(c);
  const all = Object.values(r).sort();
  const scoped = (q: string) => query(`@case:${c} ${q}`);
  const attempts: [string, string[]][] = [
    [`"'; DROP TABLE run_summaries; --"`, []],
    [`"' OR '1'='1"`, []],
    [`@agent:%`, []],
    [`@agent:tri_ge`, []],
    [`@agent:%triage%`, []],
    [`"%"`, []],
    [`"_"`, []],
    [`tool:%`, []],
    [`tool:fx._ookup`, []],
    [`"\\"; SELECT 1; --"`, []],
  ];
  const seen: string[] = [];
  for (const [q, expected] of attempts) {
    const reply = await t.api.get("/v1/runs", { query: { q: `@case:${c} ${q}`, limit: 100 } });
    if (reply.status === 400) {
      expectProblem(reply, 400, "invalid-query");
      seen.push(`${q} → 400 invalid-query`);
      continue;
    }
    expectOk(reply, 200);
    expect((reply.json.runs as any[]).map((x) => x.id), q).toEqual(expected);
    seen.push(`${q} → 200, no run`);
  }
  // Labels given as parameters are literal too.
  expect(await query(`@case:${c}`, { label: ["agent:%"] })).toEqual([]);
  expect(await query(`@case:${c}`, { label: ["agent:tri_ge"] })).toEqual([]);
  // Everything is still there.
  expect(await scoped("")).toEqual(all);
  evidence(seen.join("\n"));
});

test("logs and payloads are never searched", { tag: "@attack" }, async () => {
  const needle = `needle${unique()}`;
  const run = await startRun(alice(), {
    script: `export default async function main(input: { secret: string }) {\n  console.log("the secret is", input.secret);\n  return { echoed: input.secret };\n}\n`,
    input: { secret: needle },
    labels: { case: needle.slice(0, 10) },
  });
  const done = await ended(alice(), run.id);
  expect(done.status).toBe("completed");
  // Indexed: the run shows in the read model by its label.
  await eventually("the run is in the read model", async () => ((await query(`@case:${needle.slice(0, 10)}`)).length === 1 ? true : undefined));
  expect(await query(needle)).toEqual([]);
  expect(await query(`"${needle}"`)).toEqual([]);
  evidence(`a value logged and returned by a run: no run matches "${needle}"`);
});

test("a forged or replayed delivery fails verification", { tag: "@attack" }, async () => {
  const run = await startRun(alice(), { script: `export default async function main() { return "signed"; }\n` });
  await ended(alice(), run.id);
  const delivery = await eventually("run.completed is delivered", () =>
    receiver.deliveries.find((d) => {
      const event = JSON.parse(d.body);
      return event.type === "run.completed" && event.data?.run?.id === run.id;
    }),
  );
  expect(() => verifyWebhook(delivery.body, delivery.headers as never, secret)).not.toThrow();
  const tampered = delivery.body.replace('"signed"', '"forged"');
  expect(tampered).not.toBe(delivery.body);
  expect(() => verifyWebhook(tampered, delivery.headers as never, secret)).toThrow();
  const stale = { ...delivery.headers, "webhook-timestamp": String(Math.floor(Date.now() / 1000) - 3600) };
  expect(() => verifyWebhook(delivery.body, stale as never, secret)).toThrow();
  const otherSecret = `whsec_${Buffer.from("not the subscription's secret").toString("base64")}`;
  expect(() => verifyWebhook(delivery.body, delivery.headers as never, otherSecret)).toThrow();
  // The secret is never listed again.
  const listed = await t.api.get("/v1/subscriptions");
  expectOk(listed);
  expect(listed.text).not.toContain(secret);
  evidence("tampered body, an hour-old timestamp and another secret: each refused by verifyWebhook; the secret is not listed");
});

test("a payload is read only through a run that holds it", { tag: "@attack" }, async () => {
  const big = await startRun(alice(), { script: `export default async function main() {\n  return "y".repeat(300 * 1024);\n}\n` });
  const holder = await ended(alice(), big.id);
  const other = await ended(alice(), (await startRun(alice(), { script: `export default async function main() { return 0; }\n` })).id);
  const digest = holder.output.digest;
  await expectRefused({
    attempt: () => alice().get(`/v1/payloads/${digest}`, { query: { run: other.id } }),
    status: 404,
    slug: "not-found",
  });
  await expectRefused({
    attempt: () => alice().get(`/v1/payloads/${digest}`, { query: { run: "00000000-0000-7000-8000-000000000000" } }),
    status: 404,
    slug: "not-found",
  });
  // With the Rewind-Namespace header, `run` is required (api/README.md; the status is not
  // written there: the server answers forbidden, naming what to do).
  await expectRefused({ attempt: () => alice().get(`/v1/payloads/${digest}`), status: 403, slug: "forbidden" });
  expectOk(await alice().get(`/v1/payloads/${digest}`, { query: { run: holder.id } }));
  evidence("through a run that does not hold it: 404; without a run under a namespace: 403; through its run: 200");
});
