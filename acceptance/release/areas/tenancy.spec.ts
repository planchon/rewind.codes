/**
 * Tenant isolation (`archi/06-storage-and-scale.md`, "Tenancy, namespaces, identity and access":
 * the tenant is part of every storage key). Two tenants of setup A, acme and globex, each made by
 * the operator. Everything acme holds is probed with globex's owner key: runs and their journals,
 * scripts, toolsets, payloads (also when both hold the same bytes), host-tool tasks and
 * approvals, subscriptions, keys, roles, tokens and token keys, workflows, and inbound webhooks.
 * Each is not found (404, never 403, which would confirm it exists), and acme's state does not
 * move.
 */
import { randomBytes } from "node:crypto";
import { createKey, mintToken, runProbe, startRun, waitingRun } from "../support/access-helpers.js";
import { signingKey, startWebhookReceiver, type WebhookReceiver } from "../support/fixtures.js";
import {
  Api,
  evidence,
  expect,
  expectOk,
  expectProblem,
  expectRefused,
  newTenant,
  type Reply,
  stacks,
  type Tenant,
  test,
  unique,
} from "../support/gate.js";

/** A marker only acme's things carry, to search globex's answers for. */
const MARK = `acme${randomBytes(4).toString("hex")}`;
const NS = `${MARK}-alice`;

const HOST_TOOLS = {
  host: [
    {
      name: "crm.lookup",
      description: `Find a customer of ${MARK} by email.`,
      inputSchema: { type: "object", properties: { email: { type: "string" } }, required: ["email"] },
    },
    {
      name: "pay.refund",
      description: `Refund a customer of ${MARK}. A person approves every refund.`,
      inputSchema: { type: "object", properties: { amount: { type: "number" } }, required: ["amount"] },
      policy: { approval: true },
    },
  ],
};

const TASK_SCRIPT = `import { crm } from "rewind:tools";

export default async function main() {
  const customer = await crm.lookup({ email: "a@${MARK}.test" });
  return { customer };
}
`;

const APPROVAL_SCRIPT = `import { pay } from "rewind:tools";

export default async function main() {
  const refund = await pay.refund({ amount: 42 });
  return { refund };
}
`;

const FAILING_SCRIPT = `export default async function main() {
  throw new Error("${MARK} failed on purpose");
}
`;

const WEBHOOK_SCRIPT = `import { signal, webhook } from "rewind";

export default async function main() {
  const hook = await webhook("paid");
  const paid = await signal<{ amount: number }>("paid");
  return { amount: paid.amount, url: hook.url };
}
`;

/**
 * The same script in both tenants: the same output bytes, the same digest. 100 KiB: above the
 * table tier, a blob (MinIO), whose key names the tenant.
 */
const SHARED_SCRIPT = `export default async function main() {
  return { shared: "the same bytes in two tenants", pad: "s".repeat(100 * 1024) };
}
`;

/** acme's own output: 2 KiB, above the inline tier, so a row of the payloads table. */
const COMPLETED_SCRIPT = `export default async function main() {
  return { owner: "${MARK}", pad: "p".repeat(2048) };
}
`;

interface Acme {
  tenant: Tenant;
  toolset: string;
  waiting: string;
  completed: { id: string; outputDigest: string; script: string };
  shared: { id: string; outputDigest: string };
  failed: string;
  approval: { runId: string; taskId: string };
  task: { runId: string; token: string; id: string };
  subscription: string;
  key: { key: string; id: string };
  role: string;
  token: { token: string; id: string };
  kid: string;
}

let acme: Acme;
let globex: Tenant;
let receiver: WebhookReceiver;

async function untilWaiting(api: Api, id: string, match: (w: any) => boolean): Promise<any> {
  for (let attempt = 0; attempt < 120; attempt++) {
    const state = await api.get(`/v1/runs/${id}`);
    const found = (state.json?.waitingFor ?? []).find(match);
    if (found) return found;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`run ${id} never waited as expected`);
}

test.beforeAll(async () => {
  const tenant = await newTenant("tenancy-acme");
  globex = await newTenant("tenancy-globex");
  receiver = await startWebhookReceiver();
  const toolset = await tenant.api.post("/v1/toolsets", HOST_TOOLS);
  expectOk(toolset);
  const toolsetId = toolset.json.id as string;

  const waiting = (await waitingRun(tenant, NS, `${MARK} waits`)).id as string;

  const completed = await startRun(tenant.api, NS, { wait: "30s", script: COMPLETED_SCRIPT });
  expectOk(completed);
  expect(completed.json.status, completed.text).toBe("completed");

  const shared = await startRun(tenant.api, NS, { script: SHARED_SCRIPT, wait: "30s" });
  expect(shared.json.status, shared.text).toBe("completed");

  const failed = await startRun(tenant.api, NS, { script: FAILING_SCRIPT, wait: "30s" });
  expect(failed.json.status, failed.text).toBe("failed");

  const approvalRun = await startRun(tenant.api, NS, { script: APPROVAL_SCRIPT, toolset: toolsetId });
  expectOk(approvalRun);
  const approval = await untilWaiting(tenant.api, approvalRun.json.id, (w) => w.kind === "tool_call" && w.state === "approval");

  const taskRun = await startRun(tenant.api, NS, { script: TASK_SCRIPT, toolset: toolsetId });
  expectOk(taskRun);
  let claimed: any;
  for (let attempt = 0; attempt < 10 && !claimed; attempt++) {
    const batch = await tenant.api.get("/v1/tasks", { query: { toolset: toolsetId, tool: "crm.lookup", wait: "5s" } });
    expectOk(batch, 200);
    claimed = (batch.json.tasks as any[]).find((t) => t.runId === taskRun.json.id);
  }
  expect(claimed, "acme claims its own task").toBeDefined();

  const subscription = await tenant.api.post("/v1/subscriptions", { url: receiver.url, labels: { owner: MARK } });
  expectOk(subscription, 201);

  const role = `${MARK}-auditor`;
  expectOk(await tenant.api.put(`/v1/roles/${role}`, { description: `${MARK} role`, permissions: ["runs:read"] }));
  const key = await createKey(tenant.api, "reader", { name: `${MARK} reader` });
  const token = await mintToken(tenant.api, NS);
  const signer = await signingKey("EdDSA");
  const registered = await tenant.api.post("/v1/organization/token-keys", { name: `${MARK} signer`, jwk: signer.publicJwk });
  expectOk(registered, 201);

  acme = {
    tenant,
    toolset: toolsetId,
    waiting,
    completed: { id: completed.json.id, outputDigest: completed.json.output.digest, script: completed.json.script },
    shared: { id: shared.json.id, outputDigest: shared.json.output.digest },
    failed: failed.json.id,
    approval: { runId: approvalRun.json.id, taskId: approval.taskId },
    task: { runId: taskRun.json.id, token: claimed.token, id: claimed.id },
    subscription: subscription.json.id,
    key,
    role,
    token,
    kid: registered.json.kid,
  };
});

test.afterAll(async () => {
  await receiver?.close();
});

/** Asserts no answer of globex holds anything of acme's. */
function expectNothingOf(reply: Reply, what: string): void {
  expectOk(reply);
  const secrets = [
    MARK,
    acme.tenant.id,
    acme.waiting,
    acme.completed.id,
    acme.failed,
    acme.toolset,
    acme.completed.outputDigest,
    acme.subscription,
    acme.key.id,
    acme.kid,
    acme.approval.runId,
    acme.task.runId,
  ];
  for (const secret of secrets) {
    expect(reply.text.includes(secret), `${what} holds ${secret}: ${reply.text.slice(0, 400)}`).toBe(false);
  }
}

test("each tenant sees its own runs, and only them", { tag: "@success" }, async () => {
  const mine = await acme.tenant.api.get("/v1/runs", { query: { limit: 100 } });
  expectOk(mine, 200);
  const ids = (mine.json.runs as { id: string }[]).map((r) => r.id);
  expect(ids).toEqual(expect.arrayContaining([acme.waiting, acme.completed.id, acme.failed]));
  const theirs = await globex.api.get("/v1/runs", { query: { limit: 100 } });
  expectNothingOf(theirs, "globex's runs");
  const whoami = await globex.api.get("/v1/whoami");
  expect(whoami.json.tenant).toBe(globex.id);
  evidence(`acme lists its ${ids.length} runs; globex lists none of them`);
});

test("another tenant's run is not found, whatever is asked of it", { tag: "@attack" }, async () => {
  const id = acme.waiting;
  const probe = () => runProbe(acme.tenant.api, id);
  const asGlobex = globex.api;
  const attempts: [string, () => Promise<Reply>][] = [
    ["GET run", () => asGlobex.get(`/v1/runs/${id}`)],
    ["GET run, waiting", () => asGlobex.get(`/v1/runs/${id}`, { query: { wait: "1s" } })],
    ["GET journal", () => asGlobex.get(`/v1/runs/${id}/journal`)],
    ["GET cassette", () => asGlobex.get(`/v1/runs/${id}/journal`, { query: { as: "cassette" } })],
    ["GET entry 0", () => asGlobex.get(`/v1/runs/${id}/entries/0`)],
    ["GET trace", () => asGlobex.get(`/v1/runs/${id}/trace`)],
    ["GET LLM trace", () => asGlobex.get(`/v1/runs/${id}/trace`, { query: { format: "llm" } })],
    ["GET events", () => asGlobex.get(`/v1/runs/${id}/events`)],
    ["signal", () => asGlobex.post(`/v1/runs/${id}/signals/go`, { n: 666 }, { headers: { "idempotency-key": unique("x") } })],
    ["signal in its namespace", () => asGlobex.ns(NS).post(`/v1/runs/${id}/signals/go`, { n: 666 })],
    ["cancel", () => asGlobex.post(`/v1/runs/${id}/cancel`, { reason: "hostile" })],
    ["fork", () => asGlobex.ns(NS).post(`/v1/runs/${id}/fork`, { script: "export default async function main() { return 'stolen'; }" })],
    ["release", () => asGlobex.post(`/v1/runs/${id}/release`)],
    ["give up", () => asGlobex.post(`/v1/runs/${id}/give-up`)],
    ["set credentials", () => asGlobex.put(`/v1/runs/${id}/credentials`, { servers: {} })],
    ["erase", () => asGlobex.delete(`/v1/runs/${id}`)],
  ];
  const seen: string[] = [];
  for (const [what, attempt] of attempts) {
    const reply = await expectRefused({ attempt, status: 404, slug: "not-found", probe });
    expect(reply.text.includes(MARK), `${what}: the answer names nothing of acme's`).toBe(false);
    seen.push(`${what} → 404`);
  }
  // The run still waits for acme, and goes on when acme signals it.
  expectOk(await acme.tenant.api.post(`/v1/runs/${id}/signals/go`, { n: 7 }));
  const ended = await acme.tenant.api.get(`/v1/runs/${id}`, { query: { wait: "30s" } });
  expect(ended.json.status, ended.text).toBe("completed");
  expect(ended.json.output.value).toEqual({ note: `${MARK} waits`, n: 7 });
  evidence(`${seen.length} requests on acme's run with globex's key, each 404 not-found; acme's journal unchanged; acme's own signal completes it with n=7`);
});

test("another tenant's lists, statistics and failure groups hold nothing of it", { tag: "@attack" }, async () => {
  const queries: [string, string, Record<string, string>?][] = [
    ["runs", "/v1/runs"],
    ["runs by id", "/v1/runs", { q: acme.completed.id }],
    ["runs by namespace", "/v1/runs", { q: `namespace:${NS}` }],
    ["runs of acme's namespace", "/v1/runs"],
    ["failed runs", "/v1/runs", { status: "failed" }],
    ["stats", "/v1/runs/stats"],
    ["failure groups", "/v1/runs/failures"],
    ["scripts", "/v1/scripts"],
    ["tool statistics", "/v1/tools/stats"],
    ["toolsets", "/v1/toolsets"],
    ["workflows", "/v1/workflows"],
    ["namespaces", "/v1/namespaces"],
    ["subscriptions", "/v1/subscriptions"],
    ["keys", "/v1/api-keys"],
    ["roles", "/v1/roles"],
    ["token keys", "/v1/organization/token-keys"],
    ["quota alerts", "/v1/organization/quota-alerts"],
    ["namespace defaults", "/v1/organization/namespace-defaults"],
  ];
  for (const [what, path, query] of queries) {
    const api = what === "runs of acme's namespace" ? globex.api.ns(NS) : globex.api;
    const reply = await api.get(path, { query });
    if (reply.status === 400 && query?.q) continue; // a query the language refuses leaks nothing either
    expectNothingOf(reply, what);
  }
  // acme's own failure groups do hold its failure: the check above is not vacuous.
  const own = await acme.tenant.api.get("/v1/runs/failures");
  expect(own.text).toContain(MARK);
  evidence(`${queries.length} lists of globex (runs, stats, failures, scripts, tool stats, toolsets, namespaces, subscriptions, keys, roles, token keys, alerts) hold no id, digest or name of acme's`);
});

test("another tenant's script and toolset are not found, and its toolset starts nothing", { tag: "@attack" }, async () => {
  const runs = async () => (await globex.api.get("/v1/runs", { query: { limit: 100 } })).json.runs.length;
  expectProblem(await globex.api.get(`/v1/scripts/${acme.completed.script}`), 404, "not-found");
  expectProblem(await globex.api.get(`/v1/toolsets/${acme.toolset}/declarations`), 404, "not-found");
  expectProblem(await globex.api.get(`/v1/toolsets/${acme.toolset}/search`, { query: { query: "refund" } }), 404, "not-found");
  expectProblem(await globex.api.put(`/v1/toolsets/${acme.toolset}/credentials`, { servers: {} }), 404, "not-found");
  expectProblem(await globex.api.put(`/v1/namespaces/${NS}/toolsets/${acme.toolset}/credentials`, { servers: {} }), 404, "not-found");
  await expectRefused({ attempt: () => startRun(globex.api, "globex-ns", { script: TASK_SCRIPT, toolset: acme.toolset }), status: 404, slug: "not-found", probe: runs });
  await expectRefused({ attempt: () => globex.api.ns("globex-ns").post("/v1/check", { script: TASK_SCRIPT, toolset: acme.toolset }), status: 404, slug: "not-found" });
  const derived = await globex.api.post("/v1/toolsets", { base: acme.toolset, host: [] });
  expectProblem(derived, 404, "not-found");
  // acme still reads them.
  expectOk(await acme.tenant.api.get(`/v1/scripts/${acme.completed.script}`), 200);
  expectOk(await acme.tenant.api.get(`/v1/toolsets/${acme.toolset}/declarations`), 200);
  evidence("globex: acme's script, toolset declarations, search, credentials (toolset and namespace), a run, a check and a derived toolset from acme's toolset id → 404; acme still reads them");
});

test("another tenant's workflow is not found, and starts nothing", { tag: "@attack" }, async () => {
  const name = `${MARK}_flow`;
  const saved = await acme.tenant.api.post("/v1/workflows", {
    name,
    toolset: acme.toolset,
    script: `export default async function main(input: { n: number }) {\n  return { n: input.n };\n}\n`,
    input: { type: "object", properties: { n: { type: "number" } }, required: ["n"] },
    output: { type: "object", properties: { n: { type: "number" } }, required: ["n"] },
  });
  expectOk(saved);
  const runs = async () => (await globex.api.get("/v1/runs", { query: { limit: 100 } })).json.runs.length;
  expectProblem(await globex.api.get(`/v1/workflows/${name}`), 404, "not-found");
  await expectRefused({ attempt: () => globex.api.ns("globex-ns").post("/v1/runs", { workflow: name, input: { n: 1 } }), status: 404, slug: "not-found", probe: runs });
  const version = String(saved.json.version);
  await expectRefused({ attempt: () => globex.api.ns("globex-ns").post("/v1/runs", { workflow: `${name}@${version}`, input: { n: 1 } }), status: 404, slug: "not-found", probe: runs });
  expectOk(await acme.tenant.api.get(`/v1/workflows/${name}`), 200);
  evidence("globex: GET acme's workflow, and a run of it by name and by its exact version digest → 404; nothing started");
});

test("a payload is read only through a run of the reader's tenant, even when both hold the same bytes", { tag: "@attack" }, async () => {
  // Payloads up to 1 KiB stay in their event, and GET /v1/payloads serves the larger ones: acme's
  // output is a row of the table tier, the shared one a blob.
  const digest = acme.completed.outputDigest;
  expect(digest).toMatch(/^[0-9a-f]{64}$/);
  expectProblem(await globex.api.get(`/v1/payloads/${digest}`), 404, "not-found");
  expectProblem(await globex.api.get(`/v1/payloads/${digest}`, { query: { run: acme.completed.id } }), 404, "not-found");
  // The same bytes in globex: its own run reads them, acme's run still does not lend them.
  const mine = await startRun(globex.api, "globex-ns", { script: SHARED_SCRIPT, wait: "30s" });
  expect(mine.json.status, mine.text).toBe("completed");
  expect(mine.json.output.digest).toBe(acme.shared.outputDigest);
  const own = await globex.api.get(`/v1/payloads/${mine.json.output.digest}`, { query: { run: mine.json.id } });
  expectOk(own, 200);
  expectProblem(await globex.api.get(`/v1/payloads/${acme.shared.outputDigest}`, { query: { run: acme.shared.id } }), 404, "not-found");
  // acme reads its own.
  expectOk(await acme.tenant.api.get(`/v1/payloads/${digest}`, { query: { run: acme.completed.id } }), 200);
  expectOk(await acme.tenant.api.get(`/v1/payloads/${acme.shared.outputDigest}`, { query: { run: acme.shared.id } }), 200);
  evidence("globex: acme's 2 KiB payload by digest, and through acme's run → 404; with the same 100 KiB blob, through its own run → 200, through acme's run → 404");
});

test("another tenant's host-tool task cannot be claimed, answered, heartbeated nor approved", { tag: "@attack" }, async () => {
  const forged = `${acme.task.token.slice(0, -4)}${acme.task.token.endsWith("aaaa") ? "bbbb" : "aaaa"}`;
  // A token never issued, for comparison: globex must not tell acme's token from it.
  const unknownAnswer = await globex.api.post(`/v1/tasks/${forged}/heartbeat`);
  expect([404, 410], unknownAnswer.text).toContain(unknownAnswer.status);
  const taskProbe = () => runProbe(acme.tenant.api, acme.task.runId);
  for (const [what, attempt] of [
    ["heartbeat", () => globex.api.post(`/v1/tasks/${acme.task.token}/heartbeat`)],
    ["complete", () => globex.api.post(`/v1/tasks/${acme.task.token}/complete`, { result: { id: "stolen" } })],
    ["fail", () => globex.api.post(`/v1/tasks/${acme.task.token}/fail`, { message: "hostile", retryable: false })],
    ["defer", () => globex.api.post(`/v1/tasks/${acme.task.token}/defer`, {})],
  ] as [string, () => Promise<Reply>][]) {
    await expectRefused({ attempt, status: unknownAnswer.status, slug: unknownAnswer.slug!, probe: taskProbe });
    void what;
  }
  // globex's claim never hands acme's tasks out.
  const claimed = await globex.api.get("/v1/tasks", { query: { toolset: acme.toolset, wait: "1s" } });
  if (claimed.status === 200) {
    expect((claimed.json.tasks as unknown[]).length).toBe(0);
  } else {
    expectProblem(claimed, 404, "not-found");
  }
  // The approval waiting in acme cannot be decided by globex.
  const approvalProbe = () => runProbe(acme.tenant.api, acme.approval.runId);
  await expectRefused({ attempt: () => globex.api.post(`/v1/tasks/${acme.approval.taskId}/approve`), status: 404, slug: "not-found", probe: approvalProbe });
  await expectRefused({ attempt: () => globex.api.post(`/v1/tasks/${acme.approval.taskId}/reject`, { message: "hostile" }), status: 404, slug: "not-found", probe: approvalProbe });
  // acme still answers both.
  expectOk(await acme.tenant.api.post(`/v1/tasks/${acme.task.token}/complete`, { result: { id: "cus_1" } }));
  const done = await acme.tenant.api.get(`/v1/runs/${acme.task.runId}`, { query: { wait: "30s" } });
  expect(done.json.status, done.text).toBe("completed");
  expect(done.json.output.value).toEqual({ customer: { id: "cus_1" } });
  expectOk(await acme.tenant.api.post(`/v1/tasks/${acme.approval.taskId}/reject`, { message: "not today" }));
  evidence(
    `globex with acme's task token: heartbeat, complete, fail, defer → ${unknownAnswer.status} ${unknownAnswer.slug ?? ""}, as for a token never issued; globex's claim hands out nothing; approve and reject of acme's parked call → 404; acme then completes its task (cus_1) and rejects its approval`,
  );
});

test("another tenant's subscriptions, keys, roles, tokens and token keys are not found", { tag: "@attack" }, async () => {
  const subscriptions = async () => (await acme.tenant.api.get("/v1/subscriptions")).json;
  await expectRefused({ attempt: () => globex.api.delete(`/v1/subscriptions/${acme.subscription}`), status: 404, slug: "not-found", probe: subscriptions });

  const keys = async () => (await acme.tenant.api.get("/v1/api-keys")).json.keys.map((k: { id: string; revokedAt?: string | null }) => `${k.id}:${k.revokedAt ?? ""}`);
  await expectRefused({ attempt: () => globex.api.delete(`/v1/api-keys/${acme.key.id}`), status: 404, slug: "not-found", probe: keys });
  expectOk(await new Api(stacks().a.url, acme.key.key).get("/v1/runs"), 200);

  const roles = async () => (await acme.tenant.api.get("/v1/roles")).json;
  await expectRefused({ attempt: () => globex.api.delete(`/v1/roles/${acme.role}`), status: 404, slug: "not-found", probe: roles });
  // acme's role is no role of globex: a key of it cannot be made there.
  const borrowed = await globex.api.post("/v1/api-keys", { name: "borrowed role", role: acme.role });
  expectProblem(borrowed, 400, "invalid-request");

  await expectRefused({ attempt: () => globex.api.delete(`/v1/tokens/${acme.token.id}`), status: 404, slug: "not-found" });
  expectOk(await new Api(stacks().a.url, acme.token.token).get("/v1/runs"), 200);

  const tokenKeys = async () => (await acme.tenant.api.get("/v1/organization/token-keys")).json;
  await expectRefused({ attempt: () => globex.api.delete(`/v1/organization/token-keys/${acme.kid}`), status: 404, slug: "not-found", probe: tokenKeys });

  // A namespace's name is the tenant's own: globex erasing "the same name" erases nothing of acme's.
  const acmeRuns = async () => (await acme.tenant.api.ns(NS).get("/v1/runs", { query: { limit: 100 } })).json.runs.length;
  const before = await acmeRuns();
  const erase = await globex.api.delete(`/v1/namespaces/${NS}`);
  expect([202, 404], erase.text).toContain(erase.status);
  expect(await acmeRuns()).toBe(before);
  const detail = await acme.tenant.api.get(`/v1/namespaces/${NS}`);
  expectOk(detail, 200);
  evidence("globex: acme's subscription, key, role, opaque token and token key → 404, each still works for acme; a key of acme's role → 400; erasing a namespace of the same name leaves acme's runs");
});

test("an end user's token of one tenant reaches nothing of another", { tag: "@attack" }, async () => {
  const globexRun = await startRun(globex.api, "globex-ns", { wait: "30s" });
  expect(globexRun.json.status).toBe("completed");
  const asAcmeToken = new Api(stacks().a.url, acme.token.token);
  expectProblem(await asAcmeToken.get(`/v1/runs/${globexRun.json.id}`), 404, "not-found");
  expectProblem(await asAcmeToken.get(`/v1/runs/${globexRun.json.id}/journal`), 404, "not-found");
  expectProblem(await asAcmeToken.post(`/v1/runs/${globexRun.json.id}/cancel`, {}), 404, "not-found");
  const listed = await asAcmeToken.get("/v1/runs");
  expectOk(listed, 200);
  expect(listed.text).not.toContain(globexRun.json.id);
  evidence("acme's end user's token: globex's run, its journal and a cancel → 404; its list holds none of globex's runs");
});

test("an inbound webhook reaches only the run that made it, at its exact token", { tag: "@attack" }, async () => {
  const started = await startRun(acme.tenant.api, NS, { script: WEBHOOK_SCRIPT });
  expectOk(started);
  const id = started.json.id as string;
  await untilWaiting(acme.tenant.api, id, (w) => w.kind === "signal" && w.name === "paid");
  const journal = await acme.tenant.api.get(`/v1/runs/${id}/journal`);
  const found = /\/hooks\/([A-Za-z0-9_.~-]+)/.exec(journal.text);
  expect(found, "the journal holds the webhook's URL").not.toBeNull();
  const token = found![1]!;
  const hooks = new Api(stacks().a.url);
  const probe = () => runProbe(acme.tenant.api, id);
  const flipped = `${token.slice(0, -1)}${token.endsWith("A") ? "B" : "A"}`;
  for (const guess of [flipped, token.slice(0, -1), `${token}x`, token.toUpperCase() === token ? token.toLowerCase() : token.toUpperCase(), "../hooks", "%2e%2e"]) {
    await expectRefused({ attempt: () => hooks.post(`/hooks/${guess}`, { amount: 666 }), status: 404, slug: "not-found", probe });
  }
  // With globex's key, the real URL is the same unauthenticated door: the payload reaches acme's run, nothing else.
  const delivered = await hooks.as(globex.ownerKey).post(`/hooks/${token}`, { amount: 12 });
  expect(delivered.status, delivered.text).toBeLessThan(300);
  const ended = await acme.tenant.api.get(`/v1/runs/${id}`, { query: { wait: "30s" } });
  expect(ended.json.status, ended.text).toBe("completed");
  expect(ended.json.output.value.amount).toBe(12);
  // Once the run has ended, its token takes nothing more.
  const late = await hooks.post(`/hooks/${token}`, { amount: 99 });
  expect([404, 409, 410], late.text).toContain(late.status);
  evidence(`six guesses near the token (one character changed, cut, lengthened, case, path tricks) → 404 and the run still waits; the exact token delivers amount 12; after the run ended → ${late.status}`);
});
