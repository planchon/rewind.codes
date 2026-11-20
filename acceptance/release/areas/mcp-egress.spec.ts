/**
 * MCP servers, APIs, the `http` tool and egress (`archi/04-tools.md`, "Routes", "API tools",
 * "Authenticating calls", "Secrets", "Signed calls", "The `http` tool and egress", "Where MCP
 * servers and APIs may be"; `archi/06-storage-and-scale.md`, "Security").
 *
 * A server that takes a secret gets the one the run attached (`org/<name>`, `ns/<name>`), never
 * another; a deleted secret parks the call, a 401 too, and putting the secret again sends it with
 * the same idempotency key. A signed server checks every request with the SDK's
 * `verifyRewindCall` against the keys setup A publishes. An API is read from its OpenAPI document
 * (a deployed server calls APIs over https only: their calls are proven by the CLI area, on the
 * embedded server).
 *
 * Success paths run on setup A, which lets MCP servers on 127.0.0.1 through, as an operator allows
 * an internal host. Egress attacks run on setup B, a deployed server's defaults: an MCP server or
 * a webhook at a loopback, private, link-local or metadata address is refused, and the target, a
 * sink on 127.0.0.1, never receives a byte. The `http` tool refuses those addresses on every
 * deployed server, A included.
 */
import { existsSync } from "node:fs";
import { createServer, type IncomingMessage, type Server as HttpServer } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type RewindCall, RewindSignatureError, verifyRewindCall, verifyWebhook } from "@rewind.codes/sdk";
import { createLocalJWKSet, decodeProtectedHeader, jwtVerify } from "jose";
import { eventually } from "../../support/processes.js";
import {
  type Delivery,
  defaultTools,
  type FixtureTool,
  type McpFixture,
  type Redirector,
  type Sink,
  startApiFixture,
  startMcpFixture,
  startRedirector,
  startSink,
  startWebhookReceiver,
  type WebhookReceiver,
} from "../support/fixtures.js";
import {
  allow5xx,
  canary,
  evidence,
  expect,
  expectOk,
  expectProblem,
  expectRefused,
  newTenant,
  stacks,
  type Tenant,
  test,
  unique,
} from "../support/gate.js";
import { detail, ended, startBroken, startRun, waitingCall } from "../support/tools-helpers.js";

let a: Tenant;
let b: Tenant;
let mcp: McpFixture;
let receiver: WebhookReceiver;
let secret: string;
let sink: Sink;
let redirector: Redirector;

test.beforeAll(async () => {
  [a, b, mcp, receiver, sink, redirector] = await Promise.all([
    newTenant("mcp-egress"),
    newTenant("egress-strict", stacks().b),
    startMcpFixture(),
    startWebhookReceiver(),
    startSink(),
    startRedirector(),
  ]);
  const subscription = await a.api.post("/v1/subscriptions", { url: receiver.url, events: ["tool.schema_changed", "credentials.expired"] });
  expectOk(subscription, 201);
  secret = subscription.json.secret;
});

test.afterAll(async () => {
  await Promise.all([mcp?.close(), receiver?.close(), sink?.close(), redirector?.close()]);
});

/** Registers a toolset of MCP servers on tenant `t`; the raw answer. */
function register(t: Tenant, servers: Record<string, unknown>[], extra: Record<string, unknown> = {}) {
  return t.api.post("/v1/toolsets", { mcp: servers, ...extra });
}

async function toolsetOf(t: Tenant, servers: Record<string, unknown>[], extra: Record<string, unknown> = {}): Promise<string> {
  const reply = await register(t, servers, extra);
  expectOk(reply);
  return reply.json.id;
}

const LOOKUP = `import { fx } from "rewind:tools";

export default async function main(input: { id: string }) {
  const customer = await fx.lookup({ id: input.id });
  return { name: customer.name, tier: customer.tier };
}
`;

/** An MCP server's `auth` taking the attached secret in `Authorization`. */
const BEARER = { type: "secret", header: "authorization", format: "Bearer {secret}" };

/** A script that waits for `go` between two calls of `fx.lookup`. */
const TWO_LOOKUPS = `import { fx } from "rewind:tools";
import { signal } from "rewind";

export default async function main(input: { id: string }) {
  const first = await fx.lookup({ id: input.id });
  await signal("go");
  const second = await fx.lookup({ id: input.id });
  return [first.name, second.name];
}
`;

/** The deliveries of `type` about `runId`, verified with the subscription's secret. */
function deliveries(type: string, runId: string): { delivery: Delivery; event: any }[] {
  return receiver.deliveries
    .map((delivery) => ({ delivery, event: JSON.parse(delivery.body) }))
    .filter(({ event }) => event.type === type && event.data?.runId === runId);
}

// ---------------------------------------------------------------------------------------------
// Success

test("a script calls an MCP server over streamable HTTP, with the idempotency key and trace context in _meta", { tag: "@success" }, async () => {
  const toolset = await toolsetOf(a, [{ name: "fx", url: mcp.url }]);
  const before = mcp.calls.length;
  const run = await startRun(a.api.ns("alice"), { toolset, script: LOOKUP, input: { id: "42" }, wait: "30s" });
  const done = run.status === "completed" ? run : await ended(a.api.ns("alice"), run.id);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value).toEqual({ name: "Customer 42", tier: "gold" });
  const calls = mcp.calls.slice(before);
  expect(calls.map((c) => [c.tool, c.args])).toEqual([["lookup", { id: "42" }]]);
  expect(calls[0]!.meta?.["codes.rewind/idempotency-key"]).toBe(`${run.id}/1`);
  expect(String(calls[0]!.meta?.traceparent).split("-")[1]).toBe(done.traceId);
  evidence(`one tools/call with _meta ${JSON.stringify(calls[0]!.meta)}`);
});

test("MCP content without an output schema is read with text() and json()", { tag: "@success" }, async () => {
  const toolset = await toolsetOf(a, [{ name: "fx", url: mcp.url }]);
  const script = `import { fx } from "rewind:tools";

export default async function main() {
  const plain = await fx.echo({ text: "hello" });
  const structured = await fx.echo({ text: JSON.stringify({ n: 7 }) });
  return { text: plain.text(), n: structured.json<{ n: number }>().n };
}
`;
  const run = await startRun(a.api.ns("alice"), { toolset, script, wait: "30s" });
  const done = run.status === "completed" ? run : await ended(a.api.ns("alice"), run.id);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value).toEqual({ text: "hello", n: 7 });
});

test("an MCP tool's error reaches the script as a ToolError it can catch", { tag: "@success" }, async () => {
  const toolset = await toolsetOf(a, [{ name: "fx", url: mcp.url }]);
  const script = `import { fx } from "rewind:tools";
import { ToolError } from "rewind";

export default async function main() {
  try {
    await fx.fail({ reason: "quota" });
    return "no error";
  } catch (error) {
    if (!(error instanceof ToolError)) throw error;
    return { tool: error.tool, message: error.message };
  }
}
`;
  const done = await ended(a.api.ns("alice"), (await startRun(a.api.ns("alice"), { toolset, script })).id);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value.tool).toBe("fx.fail");
  expect(done.output.value.message).toContain("failed: quota");
});

test("each run calls with the secret it attached: Alice's, Bob's or the organization's, never another's", { tag: "@success" }, async () => {
  const server = await startMcpFixture();
  try {
    const values = { listing: `${canary()}-listing`, organization: `${canary()}-organization`, alice: `${canary()}-alice`, bob: `${canary()}-bob` };
    const name = unique("fx-");
    expectOk(await a.api.put(`/v1/secrets/${name}-listing`, { value: values.listing }), 201);
    expectOk(await a.api.put(`/v1/secrets/${name}`, { value: values.organization }), 201);
    expectOk(await a.api.put(`/v1/namespaces/alice/secrets/${name}`, { value: values.alice }), 201);
    expectOk(await a.api.put(`/v1/namespaces/bob/secrets/${name}`, { value: values.bob }), 201);
    // The listing carries the organization's secret attached for it; no run keeps it.
    const toolset = await toolsetOf(a, [{ name: "fx", url: server.url, auth: BEARER }], { secrets: { fx: `org/${name}-listing` } });
    expect(server.requests.filter((r) => r.body.includes('"tools/list"')).map((r) => r.headers.authorization)).toContain(`Bearer ${values.listing}`);
    const seenBy = async (namespace: string, attached: string) => {
      const before = server.calls.length;
      const run = await startRun(a.api.ns(namespace), { toolset, script: LOOKUP, input: { id: namespace }, secrets: { fx: attached }, wait: "30s" });
      const done = run.status === "completed" ? run : await ended(a.api.ns(namespace), run.id);
      expect(done.status, JSON.stringify(done)).toBe("completed");
      expect(done.secrets).toEqual({ fx: attached });
      const calls = server.calls.slice(before);
      expect(calls).toHaveLength(1);
      expect(calls[0]!.meta?.["codes.rewind/idempotency-key"]).toBe(`${run.id}/1`);
      return calls[0]!.headers.authorization;
    };
    expect(await seenBy("alice", `ns/${name}`)).toBe(`Bearer ${values.alice}`);
    expect(await seenBy("bob", `ns/${name}`)).toBe(`Bearer ${values.bob}`);
    expect(await seenBy("carol", `org/${name}`)).toBe(`Bearer ${values.organization}`);
    // Dave has none of his own: ns/ is refused before any run exists, with no fallback.
    const refused = await a.api.ns("dave").post("/v1/runs", { toolset, script: LOOKUP, input: { id: "dave" }, secrets: { fx: `ns/${name}` } });
    expectProblem(refused, 422, "secret-required");
    expect(refused.json.attachments).toEqual([
      expect.objectContaining({ server: "fx", problem: "unknown", secret: `ns/${name}`, attachable: expect.arrayContaining([`org/${name}`]) }),
    ]);
    const nothing = await a.api.ns("alice").post("/v1/runs", { toolset, script: LOOKUP, input: { id: "alice" } });
    expectProblem(nothing, 422, "secret-required");
    expect(nothing.json.attachments[0]).toMatchObject({ server: "fx", problem: "missing" });
    expect(server.calls.filter((c) => c.args.id === "dave"), "nothing was sent for Dave").toEqual([]);
    evidence("alice → her ns/ secret, bob → his, carol → org/; dave's ns/ (absent) and a start attaching nothing → 422 secret-required before any run; no answer of Rewind held a value");
  } finally {
    await server.close();
  }
});

test("a secret deleted mid-run parks the call, and putting it again sends it with the same key", { tag: "@success" }, async () => {
  const server = await startMcpFixture();
  try {
    const name = unique("gone-");
    expectOk(await a.api.put(`/v1/namespaces/erin/secrets/${name}`, { value: `${canary()}-erin` }), 201);
    const toolset = await toolsetOf(a, [{ name: "fx", url: server.url, auth: BEARER }]);
    const run = await startRun(a.api.ns("erin"), { toolset, script: TWO_LOOKUPS, input: { id: "erin" }, secrets: { fx: `ns/${name}` } });
    await waitingSignal(a.api.ns("erin"), run.id, "go");
    expectOk(await a.api.delete(`/v1/namespaces/erin/secrets/${name}`), 204);
    expectOk(await a.api.post(`/v1/runs/${run.id}/signals/go`, {}));
    const parked = await waitingCall(a.api.ns("erin"), run.id, "parked");
    expect(parked.parked).toBe("secret_missing");
    const hook = await eventually("credentials.expired (missing) is delivered", () => deliveries("credentials.expired", run.id)[0], 60_000);
    expect(verifyWebhook(hook.delivery.body, hook.delivery.headers as never, secret).type).toBe("credentials.expired");
    expect(hook.event.data).toMatchObject({ runId: run.id, namespace: "erin", tool: "fx.lookup", server: "fx", reason: "missing", secret: `ns/${name}`, taskId: parked.taskId });
    expect(hook.event.data.message).toContain(`PUT /v1/namespaces/erin/secrets/${name}`);
    const key = `${run.id}/${parked.seq}`;
    expect(server.calls.filter((c) => c.meta?.["codes.rewind/idempotency-key"] === key), "nothing was sent while parked").toEqual([]);
    expectOk(await a.api.put(`/v1/namespaces/erin/secrets/${name}`, { value: `${canary()}-erin-again` }), 201);
    const done = await ended(a.api.ns("erin"), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    const sent = server.calls.filter((c) => c.meta?.["codes.rewind/idempotency-key"] === key);
    expect(sent.map((c) => c.headers.authorization)).toEqual([`Bearer ${canary()}-erin-again`]);
    evidence(`deleted mid-run → parked secret_missing, credentials.expired (missing) naming the PUT to make; put again → sent once with ${key}`);
  } finally {
    await server.close();
  }
});

test("a call refused with 401 is parked, and sent again with the same key once the secret is put again", { tag: "@success" }, async () => {
  const server = await startMcpFixture();
  const gate = await startAuthGate(server.url, `Bearer ${canary()}-v1`);
  try {
    const name = unique("gate-");
    expectOk(await a.api.put(`/v1/secrets/${name}`, { value: `${canary()}-v1` }), 201);
    const toolset = await toolsetOf(a, [{ name: "fx", url: gate.url, auth: BEARER }], { secrets: { fx: `org/${name}` } });
    // The token expires during the run's pause.
    gate.accept = `Bearer ${canary()}-v2`;
    const run = await startRun(a.api.ns("carol"), { toolset, script: LOOKUP, input: { id: "7" }, secrets: { fx: `org/${name}` } });
    const parked = await waitingCall(a.api.ns("carol"), run.id, "parked");
    expect(parked.parked).toBe("credentials_refused");
    const hook = await eventually("credentials.expired is delivered", () => deliveries("credentials.expired", run.id)[0], 60_000);
    expect(verifyWebhook(hook.delivery.body, hook.delivery.headers as never, secret).type).toBe("credentials.expired");
    expect(hook.event.data).toMatchObject({ runId: run.id, namespace: "carol", tool: "fx.lookup", toolset, reason: "refused", secret: `org/${name}` });
    expect(hook.delivery.body).not.toContain(canary());
    const usage = await a.api.get(`/v1/secrets/${name}/usage`);
    expectOk(usage, 200);
    expect(usage.json.waitingRuns).toContain(run.id);
    expectOk(await a.api.put(`/v1/secrets/${name}`, { value: `${canary()}-v2` }), 200);
    const done = await ended(a.api.ns("carol"), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    const keys = server.calls.filter((c) => c.args.id === "7").map((c) => c.meta?.["codes.rewind/idempotency-key"]);
    expect(keys).toEqual([`${run.id}/1`]);
    expect(gate.refused).toBeGreaterThan(0);
    evidence(`401 → parked credentials_refused, credentials.expired (refused, org/${name}); the usage lists the waiting run; put again → sent with ${run.id}/1`);
  } finally {
    await gate.close();
    await server.close();
  }
});

test("a signed MCP server checks every request with verifyRewindCall and learns the end user; a replaced key still verifies", { tag: "@success" }, async () => {
  const issuer = `${stacks().a.url}/organizations/${a.id}`;
  const verified = new Map<string, RewindCall>();
  const state = { labels: { env: "prod" } as Record<string, string>, audience: "" };
  const whoami: FixtureTool = {
    name: "whoami",
    description: "Who calls, from the signature.",
    inputSchema: { type: "object" },
    handler: (_args, call) => {
      const found = verified.get(String(call.meta?.["codes.rewind/idempotency-key"]));
      return { content: [{ type: "text", text: JSON.stringify({ namespace: found?.namespace ?? null, labels: found?.labels ?? null }) }] };
    },
  };
  const server = await startMcpFixture([whoami], {
    check: async (request) => {
      try {
        const call = await verifyRewindCall(request, { issuer, audience: state.audience, labels: state.labels, session: true });
        if (call) verified.set(call.idempotencyKey, call);
        return undefined;
      } catch (error) {
        if (error instanceof RewindSignatureError) return `${error.check}: ${error.message}`;
        throw error;
      }
    },
  });
  state.audience = server.url;
  const script = `import { crm } from "rewind:tools";

export default async function main() {
  return (await crm.whoami({})).json();
}
`;
  try {
    const toolset = await toolsetOf(a, [{ name: "crm", url: server.url, auth: { type: "rewind" } }]);
    expect(server.requests.length).toBeGreaterThan(0);
    expect(server.requests.filter((r) => r.refused), "the listing's requests verified").toEqual([]);
    const keys = await a.api.get("/v1/organization/signing-keys");
    expectOk(keys, 200);
    expect(keys.json).toMatchObject({ issuer, jwksUrl: `${issuer}/jwks.json` });
    const first = keys.json.keys[0].kid as string;

    const run = await startRun(a.api.ns("alice"), { toolset, script, labels: { env: "prod" }, wait: "30s" });
    const done = run.status === "completed" ? run : await ended(a.api.ns("alice"), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    expect(done.output.value).toEqual({ namespace: "alice", labels: { env: "prod" } });
    const call = verified.get(`${run.id}/1`)!;
    expect(call).toMatchObject({ namespace: "alice", run: run.id, tool: "crm.whoami", idempotencyKey: `${run.id}/1` });
    const signed = server.requests.find((r) => r.body.includes(`"${run.id}/1"`))!;
    const token = signed.headers.authorization!.replace(/^Bearer /, "");
    expect(decodeProtectedHeader(token)).toMatchObject({ alg: "EdDSA", kid: first });
    const jwks = await fetch(`${issuer}/jwks.json`);
    expect(jwks.status).toBe(200);
    expect(jwks.headers.get("cache-control")).toBe("max-age=300");

    // Replaced: the old key stops signing and stays published; what it signed still verifies.
    const rotated = await a.api.post("/v1/organization/signing-keys");
    expectOk(rotated, 200);
    expect(rotated.json.kid).not.toBe(first);
    const published = (await (await fetch(`${issuer}/jwks.json`)).json()) as { keys: { kid: string }[] };
    expect(published.keys.map((k) => k.kid)).toEqual([rotated.json.kid, first]);
    const again = await jwtVerify(token, createLocalJWKSet(published as never), { issuer, audience: server.url });
    expect(again.payload.sub).toBe("alice");
    const listed = await a.api.get("/v1/organization/signing-keys");
    expect(listed.json.keys.map((k: { kid: string; retiredAt?: string }) => [k.kid, Boolean(k.retiredAt)])).toEqual([
      [rotated.json.kid, false],
      [first, true],
    ]);

    // Another environment's run: the server refuses its labels (401), the call is parked, and the
    // harness approves it once the server takes them; it is sent again with the same key. The SDK
    // reads the keys again for an unknown kid at most every 10 s: past that, the new key is known.
    await new Promise((resolve) => setTimeout(resolve, 11_000));
    const dev = await startRun(a.api.ns("bob"), { toolset, script, labels: { env: "dev" } });
    const parked = await waitingCall(a.api.ns("bob"), dev.id, "parked");
    expect(parked.parked).toBe("credentials_refused");
    const hook = await eventually("credentials.expired (refused) is delivered", () => deliveries("credentials.expired", dev.id)[0], 60_000);
    expect(hook.event.data).toMatchObject({ runId: dev.id, reason: "refused", server: "crm" });
    expect(hook.event.data.secret).toBeUndefined();
    state.labels = {};
    expectOk(await a.api.post(`/v1/tasks/${parked.taskId}/approve`));
    const devDone = await ended(a.api.ns("bob"), dev.id);
    expect(devDone.status, JSON.stringify(devDone)).toBe("completed");
    expect(devDone.output.value).toEqual({ namespace: "bob", labels: { env: "dev" } });
    const devKey = `${dev.id}/${parked.seq}`;
    const devCalls = server.requests.filter((r) => r.body.includes(`"${devKey}"`));
    expect(devCalls.map((r) => (r.refused ?? "").split(":")[0])).toEqual(["labels", ""]);
    expect(decodeProtectedHeader(devCalls[1]!.headers.authorization!.replace(/^Bearer /, "")).kid).toBe(rotated.json.kid);
    evidence(`${server.requests.length} requests signed and verified by verifyRewindCall against ${issuer}/jwks.json; sub=alice from the signature; the replaced key ${first} still verifies; an env=dev run refused (labels) → parked, approved → sent again with ${devKey} under ${rotated.json.kid}`);
  } finally {
    await server.close();
  }
});

test("an API is read from its OpenAPI document by URL: one tool per operation, the others skipped, its key attached at start", { tag: "@success" }, async () => {
  const fixture = await startApiFixture({ server: "https://measures.example.com/v1" });
  try {
    const reply = await a.api.post("/v1/toolsets", { apis: [{ name: "dax", openapi: fixture.documentUrl }] });
    expectOk(reply);
    const toolset = reply.json;
    expect(fixture.documentReads).toBe(1);
    expect(toolset.tools.map((t: { name: string }) => t.name)).toEqual(["dax.createMeasure", "dax.getMeasure", "webhook"]);
    expect(toolset.apis).toEqual([
      expect.objectContaining({
        name: "dax",
        baseUrl: "https://measures.example.com/v1",
        tools: 2,
        auth: expect.objectContaining({ type: "secret", header: expect.stringMatching(/^x-api-key$/i) }),
      }),
    ]);
    expect(toolset.skipped).toEqual([expect.objectContaining({ method: "put", path: "/measures/{id}" })]);
    const create = toolset.tools.find((t: { name: string }) => t.name === "dax.createMeasure");
    expect(create.policy.onInterruption, "a POST declaring Idempotency-Key may be sent again").toBe("retry");
    // The declarations type-check scripts before they run.
    const wrong = `import { dax } from "rewind:tools";\nexport default async function main() { return await dax.getMeasure({ id: 42 }); }\n`;
    const checked = await a.api.ns("alice").post("/v1/check", { script: wrong, toolset: toolset.id });
    expectOk(checked, 200);
    expect(JSON.stringify(checked.json)).toContain("TS2322");
    // Its key is a secret the run attaches: none attached, the start is refused.
    const right = `import { dax } from "rewind:tools";\nexport default async function main() { return await dax.getMeasure({ id: "42" }); }\n`;
    const refused = await a.api.ns("alice").post("/v1/runs", { script: right, toolset: toolset.id });
    expectProblem(refused, 422, "secret-required");
    expect(refused.json.attachments[0]).toMatchObject({ server: "dax", problem: "missing" });
    expect(fixture.requests, "the API itself was never called").toEqual([]);
    evidence(`the document read once from ${fixture.documentUrl}; dax.createMeasure and dax.getMeasure pinned, PUT /measures/{id} skipped (text/csv); auth from its apiKey scheme; a wrong argument → TS2322; no key attached → 422 secret-required`);
  } finally {
    await fixture.close();
  }
});

test("a tool whose schema changed parks the call, tells the harness, and goes on once approved", { tag: "@success" }, async () => {
  const server = await startMcpFixture();
  try {
    const toolset = await toolsetOf(a, [{ name: "fx", url: server.url }]);
    // The server's lookup now requires `customer`: the pinned call no longer matches.
    server.setTools(
      defaultTools().map((tool) =>
        tool.name === "lookup"
          ? { ...tool, inputSchema: { type: "object", properties: { customer: { type: "string" } }, required: ["customer"] } }
          : tool,
      ),
    );
    const run = await startRun(a.api.ns("alice"), { toolset, script: LOOKUP, input: { id: "9" } });
    const parked = await waitingCall(a.api.ns("alice"), run.id, "parked");
    expect(parked.parked).toBe("schema_changed");
    expect(server.calls.filter((c) => c.tool === "lookup"), "nothing is sent while parked").toHaveLength(0);
    const hook = await eventually("tool.schema_changed is delivered", () => deliveries("tool.schema_changed", run.id)[0], 60_000);
    expect(verifyWebhook(hook.delivery.body, hook.delivery.headers as never, secret).type).toBe("tool.schema_changed");
    expect(hook.event.data).toMatchObject({ runId: run.id, namespace: "alice", tool: "fx.lookup", taskId: parked.taskId });
    expectOk(await a.api.post(`/v1/tasks/${parked.taskId}/approve`));
    const done = await ended(a.api.ns("alice"), run.id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    expect(server.calls.filter((c) => c.tool === "lookup").map((c) => c.args)).toEqual([{ id: "9" }]);
  } finally {
    await server.close();
  }
});

test("a stdio MCP server runs where the operator allows it", { tag: "@success" }, async () => {
  const toolset = await toolsetOf(a, [{ name: "everything", command: "npx", args: ["-y", "@modelcontextprotocol/server-everything@2.2.1", "stdio"] }]);
  const script = `import { everything } from "rewind:tools";

export default async function main() {
  const echoed = await everything.echo({ message: "over stdio" });
  return echoed.text();
}
`;
  const run = await startRun(a.api.ns("alice"), { toolset, script, wait: "60s" });
  const done = run.status === "completed" ? run : await ended(a.api.ns("alice"), run.id, 120_000);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value).toContain("over stdio");
});

test("fetch reaches a public host of the http tool's allow-list", { tag: "@success" }, async () => {
  const toolset = await a.api.post("/v1/toolsets", { http: { allow: ["rewind.codes"] } });
  expectOk(toolset);
  const script = `export default async function main() {
  const response = await fetch("https://rewind.codes/");
  const body = await response.text();
  return { status: response.status, ok: response.ok, url: response.url, html: body.includes("<html") || body.includes("<!doctype") || body.includes("<!DOCTYPE") };
}
`;
  const run = await startRun(a.api.ns("alice"), { toolset: toolset.json.id, script, wait: "60s" });
  const done = run.status === "completed" || run.status === "failed" ? run : await ended(a.api.ns("alice"), run.id, 90_000);
  expect(done.status, `the http tool reached https://rewind.codes/ (this check needs the internet): ${JSON.stringify(done.error ?? done)}`).toBe("completed");
  expect(done.output.value).toMatchObject({ status: 200, ok: true, html: true });
});

// ---------------------------------------------------------------------------------------------
// Failure

test("an MCP server that cannot be reached fails the registration with toolset-unavailable", { tag: "@failure" }, async () => {
  const closed = await startSink();
  const url = `${closed.origin}/mcp`;
  await closed.close();
  allow5xx("toolset-unavailable");
  const reply = await register(a, [{ name: "gone", url }]);
  expectProblem(reply, 502, "toolset-unavailable");
});

test("a deployed server calls APIs over https only", { tag: "@failure" }, async () => {
  const fixture = await startApiFixture();
  try {
    // The document given inline: one read from an internal address is refused without a word of
    // why (08), which the check would not see.
    const document = await (await fetch(fixture.documentUrl)).json();
    const reply = await a.api.post("/v1/toolsets", { apis: [{ name: "dax", openapi: document }] });
    expectProblem(reply, 422, "invalid-toolset");
    expect(detail(reply)).toContain("a deployed server calls APIs over https");
    expect(fixture.requests).toEqual([]);
    evidence(`an API whose base URL is ${fixture.base} → 422 invalid-toolset: ${detail(reply)}`);
  } finally {
    await fixture.close();
  }
});

test("invalid toolsets are refused with invalid-toolset", { tag: "@failure" }, async () => {
  const host = (name: string) => ({ name, description: "d", inputSchema: { type: "object" } });
  const cases: [string, Record<string, unknown>][] = [
    ["a duplicate host tool", { host: [host("crm.find"), host("crm.find")] }],
    ["a reserved word as a host tool", { host: [host("default")] }],
    ["a reserved word as a group", { host: [host("class.find")] }],
    ["the reserved namespace workflows", { host: [host("workflows.find")] }],
    ["a server with both url and command", { mcp: [{ name: "fx", url: mcp.url, command: "true" }] }],
    ["a server with neither url nor command", { mcp: [{ name: "fx" }] }],
  ];
  for (const [what, body] of cases) {
    const reply = await a.api.post("/v1/toolsets", body);
    expect(reply.status, `${what}: ${reply.text}`).toBe(422);
    expectProblem(reply, 422, "invalid-toolset");
  }
  evidence(cases.map(([what]) => what).join("; "));
});

test("a toolset whose input schema is not a JSON Schema is refused with invalid-toolset", { tag: "@failure" }, async () => {
  // api/README.md: `invalid-toolset` (422) is "an invalid schema".
  for (const inputSchema of [{ type: 12 }, { type: "object", properties: { id: { type: "strng" } } }, { type: "object", required: "id" }]) {
    const reply = await a.api.post("/v1/toolsets", { host: [{ name: "crm.find", description: "d", inputSchema }] });
    expect(reply.status, `${JSON.stringify(inputSchema)}: ${reply.text.slice(0, 300)}`).toBe(422);
    expectProblem(reply, 422, "invalid-toolset");
  }
});

test("arguments outside an MCP tool's schema throw a TypeError, and the server receives nothing", { tag: "@failure" }, async () => {
  const toolset = await toolsetOf(a, [{ name: "fx", url: mcp.url }]);
  const before = mcp.calls.length;
  const script = `import { fx } from "rewind:tools";

export default async function main() {
  return await fx.lookup({ id: 42 } as any);
}
`;
  const done = await ended(a.api.ns("alice"), (await startRun(a.api.ns("alice"), { toolset, script })).id);
  expect(done.status).toBe("failed");
  expect(done.error.name).toBe("TypeError");
  expect(mcp.calls.length).toBe(before);
});

test("an MCP call past its execution timeout fails with kind timeout", { tag: "@failure" }, async () => {
  const toolset = await toolsetOf(a, [{ name: "fx", url: mcp.url, policies: { slow: { timeout: "1s", onInterruption: "at_most_once" } } }]);
  const script = `import { fx } from "rewind:tools";

export default async function main() {
  return (await fx.slow({ ms: 5000 })).text();
}
`;
  const done = await ended(a.api.ns("alice"), (await startRun(a.api.ns("alice"), { toolset, script })).id);
  expect(done.status, JSON.stringify(done)).toBe("failed");
  expect(done.error.failedCall).toMatchObject({ tool: "fx.slow", error: { kind: "timeout" } });
});

test("stdio servers are refused where the operator does not allow them, and nothing is started", { tag: "@failure" }, async () => {
  const marker = join(tmpdir(), `rewind-gate-stdio-${unique()}`);
  const reply = await register(b, [{ name: "shell", command: "/bin/sh", args: ["-c", `touch ${marker}`] }]);
  expectProblem(reply, 422, "invalid-toolset");
  await new Promise((resolve) => setTimeout(resolve, 500));
  expect(existsSync(marker), "the command never ran").toBe(false);
});

// ---------------------------------------------------------------------------------------------
// Attack: MCP servers at internal addresses, on a deployed server's defaults (setup B)

let internalSecret: Promise<void> | undefined;

/**
 * Registers an MCP server at `url` on B, listed with the organization's secret holding the canary:
 * refused, the sink untouched, the detail naming the reason.
 */
async function refusedOnB(url: string, reason: RegExp): Promise<string> {
  internalSecret ??= b.api.put("/v1/secrets/internal", { value: canary() }).then((reply) => expectOk(reply));
  await internalSecret;
  allow5xx("toolset-unavailable");
  const reply = await expectRefused({
    attempt: () => register(b, [{ name: "internal", url, auth: BEARER }], { secrets: { internal: "org/internal" } }),
    status: 502,
    slug: "toolset-unavailable",
    probe: () => sink.hits.length,
  });
  expect(detail(reply)).toMatch(reason);
  expect(reply.text).not.toContain(canary());
  return `${url} → 502 toolset-unavailable: ${detail(reply)}`;
}

test("an MCP server on loopback is refused, in every spelling", { tag: "@attack" }, async () => {
  const port = sink.port;
  const spelled = [
    `http://127.0.0.1:${port}/mcp`,
    `http://127.1:${port}/mcp`,
    `http://2130706433:${port}/mcp`,
    `http://0x7f000001:${port}/mcp`,
    `http://0177.0.0.1:${port}/mcp`,
    `http://[::1]:${port}/mcp`,
    `http://[::ffff:127.0.0.1]:${port}/mcp`,
    `http://localhost:${port}/mcp`,
    `http://LOCALHOST.:${port}/mcp`,
  ];
  const seen: string[] = [];
  for (const url of spelled) seen.push(await refusedOnB(url, /loopback/));
  expect(sink.hits).toEqual([]);
  evidence(seen.join("\n"));
});

test("an MCP server at a private, shared or unspecified address is refused", { tag: "@attack" }, async () => {
  const seen: string[] = [];
  for (const [url, reason] of [
    ["http://10.0.0.1/mcp", /private/],
    ["http://172.16.0.1/mcp", /private/],
    ["http://192.168.1.1/mcp", /private/],
    ["http://100.64.0.1/mcp", /shared/],
    ["http://0.0.0.0/mcp", /this network/],
    ["http://[::]/mcp", /unspecified/],
    ["http://[fd12:3456::1]/mcp", /unique local/],
  ] as const) {
    seen.push(await refusedOnB(url, reason));
  }
  evidence(seen.join("\n"));
});

test("an MCP server at a cloud metadata endpoint is refused", { tag: "@attack" }, async () => {
  const seen: string[] = [];
  for (const [url, reason] of [
    ["http://169.254.169.254/latest/meta-data/", /link-local/],
    ["http://100.100.100.200/latest/meta-data/", /shared/],
    ["http://[fd00:ec2::254]/latest/meta-data/", /unique local/],
    ["http://[::ffff:169.254.169.254]/latest/meta-data/", /link-local/],
    ["http://[64:ff9b::a9fe:a9fe]/latest/meta-data/", /link-local/],
  ] as const) {
    seen.push(await refusedOnB(url, reason));
  }
  evidence(seen.join("\n"));
});

test("an MCP server that redirects to another origin is refused, and its secret never follows", { tag: "@attack" }, async () => {
  const target = await startMcpFixture();
  try {
    const url = redirector.to(target.url, 307);
    const name = unique("hop-");
    expectOk(await a.api.put(`/v1/secrets/${name}`, { value: `${canary()}-hop` }), 201);
    allow5xx("toolset-unavailable");
    const reply = await expectRefused({
      attempt: () => register(a, [{ name: "hop", url, auth: BEARER }], { secrets: { hop: `org/${name}` } }),
      status: 502,
      slug: "toolset-unavailable",
      probe: () => target.methods.length,
    });
    expect(target.methods, "the other origin received nothing").toEqual([]);
    expect(redirector.hits.length).toBeGreaterThan(0);
    expect(reply.text).not.toContain(canary());
    evidence(`307 to ${target.url} → 502 toolset-unavailable: ${detail(reply)}`);
  } finally {
    await target.close();
  }
});

test("what an internal MCP server answers is never quoted to the tenant", { tag: "@attack" }, async () => {
  const marker = `INTERNAL-ONLY-${unique()}`;
  const broken = await startBroken(500, `stack trace with ${marker} and db password`);
  try {
    allow5xx("toolset-unavailable");
    const reply = await register(a, [{ name: "internal", url: broken.url }]);
    expectProblem(reply, 502, "toolset-unavailable");
    expect(broken.hits).toBeGreaterThan(0);
    expect(reply.text).not.toContain(marker);
    evidence(`the internal server answered 500 with a body; the tenant got: ${detail(reply)}`);
  } finally {
    await broken.close();
  }
});

test("no secret's value is stored in clear: a dump of setup A's database holds none", { tag: "@attack" }, async () => {
  const { spawnSync } = await import("node:child_process");
  const postgres = stacks().a.postgres;
  expect(postgres, "setup A runs Postgres").toBeDefined();
  // At least this file's secrets are there, written above.
  const listed = await a.api.get("/v1/secrets");
  expect(listed.json.secrets.length).toBeGreaterThan(0);
  const dump = spawnSync("docker", ["exec", postgres!.container, "pg_dump", "-U", "rewind", "-d", "postgres", "--data-only"], {
    encoding: "utf8",
    maxBuffer: 1 << 30,
  });
  expect(dump.status, dump.stderr).toBe(0);
  expect(dump.stdout).toContain("secrets");
  const hex = Buffer.from(canary()).toString("hex");
  expect(dump.stdout.includes(canary()), "a value in clear").toBe(false);
  expect(dump.stdout.toLowerCase().includes(hex), "a value in clear, as bytes").toBe(false);
  evidence(`pg_dump of setup A (${(dump.stdout.length / 1e6).toFixed(1)} MB) holds the canary neither as text nor as hex: secrets are sealed under the tenant's data key`);
});

// ---------------------------------------------------------------------------------------------
// Attack: the http tool

test("fetch to an internal address is refused even when the allow-list names it", { tag: "@attack" }, async () => {
  for (const t of [a, b]) {
    const toolset = await t.api.post("/v1/toolsets", { http: { allow: ["127.0.0.1", "localhost", "169.254.169.254", "10.0.0.1"] } });
    expectOk(toolset);
    const script = `import { ToolError } from "rewind";

export default async function main(input: { urls: string[] }) {
  const refused: string[] = [];
  for (const url of input.urls) {
    try {
      await fetch(url);
      refused.push("SENT " + url);
    } catch (error) {
      if (!(error instanceof ToolError)) throw error;
      refused.push(error.message);
    }
  }
  return refused;
}
`;
    const urls = [`${sink.origin}/secret`, `http://localhost:${sink.port}/secret`, "http://169.254.169.254/latest/meta-data/", "http://10.0.0.1/"];
    const before = sink.hits.length;
    const done = await ended(t.api.ns("alice"), (await startRun(t.api.ns("alice"), { toolset: toolset.json.id, script, input: { urls } })).id);
    expect(done.status, JSON.stringify(done)).toBe("completed");
    const messages: string[] = done.output.value;
    expect(messages).toHaveLength(urls.length);
    for (const message of messages) expect(message).toMatch(/loopback|link-local|private/);
    expect(sink.hits.length, "the sink received nothing").toBe(before);
    evidence(messages.join("\n"));
  }
});

test("fetch outside the allow-list, or with another scheme, is refused", { tag: "@attack" }, async () => {
  const toolset = await a.api.post("/v1/toolsets", { http: { allow: ["rewind.codes"] } });
  expectOk(toolset);
  const script = `import { ToolError } from "rewind";

export default async function main(input: { urls: string[] }) {
  const out: string[] = [];
  for (const url of input.urls) {
    try {
      await fetch(url);
      out.push("SENT " + url);
    } catch (error) {
      // A refused host is a ToolError; a scheme fetch does not speak is refused before any call.
      out.push(\`\${error instanceof ToolError ? "ToolError" : (error as Error).name}: \${(error as Error).message}\`);
    }
  }
  return out;
}
`;
  const urls = ["https://example.org/", "https://rewind.codes.evil.example/", "https://evil.example/?x=rewind.codes", "file:///etc/passwd", "ftp://rewind.codes/"];
  const done = await ended(a.api.ns("alice"), (await startRun(a.api.ns("alice"), { toolset: toolset.json.id, script, input: { urls } })).id);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  const [org, suffix, query, file, ftp] = done.output.value as string[];
  for (const refused of [org, suffix, query]) expect(refused).toMatch(/^ToolError: .*not in the toolset's allow-list/);
  for (const refused of [file, ftp]) expect(refused).toMatch(/^(ToolError|TypeError): .*only sends http and https/);
  // Nothing reached the network: the journal holds no completed http call.
  const journal = await a.api.ns("alice").get(`/v1/runs/${done.id}/journal`);
  expect(JSON.stringify(journal.json)).not.toContain('"status":200');
  evidence((done.output.value as string[]).join("\n"));
});

// ---------------------------------------------------------------------------------------------
// Attack: lifecycle webhooks to internal addresses

test("a webhook subscription to an internal or plain-HTTP address is refused on a deployed server", { tag: "@attack" }, async () => {
  const list = async () => (await b.api.get("/v1/subscriptions")).json;
  const seen: string[] = [];
  for (const url of [
    `${sink.origin}/hooks`,
    `https://127.0.0.1:${sink.port}/hooks`,
    "https://169.254.169.254/latest/meta-data/",
    "https://10.1.2.3/hooks",
    "https://[::1]/hooks",
    "http://example.com/hooks",
    "not a url",
  ]) {
    const reply = await expectRefused({ attempt: () => b.api.post("/v1/subscriptions", { url }), status: 400, slug: "invalid-request", probe: list });
    seen.push(`${url} → 400 invalid-request: ${detail(reply)}`);
  }
  expect(sink.hits).toEqual([]);
  evidence(seen.join("\n"));
});

// ---------------------------------------------------------------------------------------------

/** Waits until the run waits for the signal `name`. */
async function waitingSignal(api: typeof a.api, id: string, name: string): Promise<void> {
  await eventually(
    `run ${id} waits for ${name}`,
    async () => {
      const reply = await api.get(`/v1/runs/${id}`);
      return (reply.json?.waitingFor ?? []).some((w: { kind: string; name?: string }) => w.kind === "signal" && w.name === name) ? true : undefined;
    },
    60_000,
  );
}

/**
 * A proxy in front of an MCP server that answers 401 unless the request's `Authorization` is the
 * one it accepts now: a server whose token expired.
 */
async function startAuthGate(target: string, accept: string) {
  const state = { accept, refused: 0 };
  const server: HttpServer = createServer(async (req: IncomingMessage, res) => {
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(chunk as Buffer);
    if (req.headers.authorization !== state.accept) {
      state.refused += 1;
      res.writeHead(401, { "content-type": "text/plain" }).end("token expired");
      return;
    }
    const headers: Record<string, string> = {};
    for (const name of ["content-type", "accept", "mcp-protocol-version", "mcp-session-id", "authorization"]) {
      const value = req.headers[name];
      if (typeof value === "string") headers[name] = value;
    }
    const answer = await fetch(target, { method: req.method, headers, body: req.method === "POST" ? Buffer.concat(chunks) : undefined });
    const out: Record<string, string> = {};
    answer.headers.forEach((value, name) => {
      if (["content-type", "mcp-session-id"].includes(name)) out[name] = value;
    });
    res.writeHead(answer.status, out).end(Buffer.from(await answer.arrayBuffer()));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as AddressInfo).port;
  return {
    url: `http://127.0.0.1:${port}/mcp`,
    get accept() {
      return state.accept;
    },
    set accept(value: string) {
      state.accept = value;
    },
    get refused() {
      return state.refused;
    },
    close: () => {
      server.closeAllConnections();
      return new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}
