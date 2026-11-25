/**
 * Organization isolation (`archi/06-storage-and-scale.md`, "Tenancy, namespaces, identity and access":
 * the organization is part of every storage key). Two organizations of setup A, acme and globex, each made by
 * the operator. Everything acme holds is probed with globex's owner key: runs and their histories,
 * workflows, action sets, payloads (also when both hold the same bytes), harness calls and
 * approvals, subscriptions, keys, roles, tokens and token keys, secrets (the organization's and a
 * namespace's) and signing keys, and signal URLs.
 * Each is not found (404, never 403, which would confirm it exists), and acme's state does not
 * move.
 */
import { randomBytes } from "node:crypto";
import { createKey, mintToken, runProbe, startRun, waitingRun } from "../support/access-helpers.js";
import { follow } from "../support/runs-helpers.js";
import { signingKey, startWebhookReceiver, type WebhookReceiver } from "../support/fixtures.js";
import { handedWithin, CallStream } from "../support/actions-helpers.js";
import {
  Api,
  evidence,
  expect,
  expectOk,
  expectProblem,
  expectRefused,
  newOrg,
  type Reply,
  stacks,
  type Org,
  test,
  unique,
} from "../support/gate.js";

/** A marker only acme's things carry, to search globex's answers for. */
const MARK = `acme${randomBytes(4).toString("hex")}`;
const NS = `${MARK}-alice`;

const HARNESS_ACTIONS = {
  harness: [
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

const LOOKUP_CODE = `import { crm } from "rewind:actions";

export default async function main() {
  const customer = await crm.lookup({ email: "a@${MARK}.test" });
  return { customer };
}
`;

const APPROVAL_CODE = `import { pay } from "rewind:actions";

export default async function main() {
  const refund = await pay.refund({ amount: 42 });
  return { refund };
}
`;

const FAILING_CODE = `export default async function main() {
  throw new Error("${MARK} failed on purpose");
}
`;

const SIGNAL_URL_CODE = `import { signal, signalUrl } from "rewind";

export default async function main() {
  const hook = await signalUrl("paid");
  const paid = await signal<{ amount: number }>("paid");
  return { amount: paid.amount, url: hook.url };
}
`;

/**
 * The same code in both organizations: the same output bytes, the same digest. 100 KiB: above the
 * table tier, a blob (MinIO), whose key names the organization.
 */
const SHARED_CODE = `export default async function main() {
  return { shared: "the same bytes in two organizations", pad: "s".repeat(100 * 1024) };
}
`;

/** acme's own output: 2 KiB, above the inline tier, so a payload record of its run's partition. */
const COMPLETED_CODE = `export default async function main() {
  return { owner: "${MARK}", pad: "p".repeat(2048) };
}
`;

interface Acme {
  org: Org;
  actionSet: string;
  waiting: string;
  completed: { id: string; outputDigest: string; workflow: string };
  shared: { id: string; outputDigest: string };
  failed: string;
  approval: { runId: string; callId: string };
  call: { runId: string; token: string; id: string };
  subscription: string;
  key: { key: string; id: string };
  role: string;
  token: { token: string; id: string };
  kid: string;
  /** The name of its secrets: the organization's, and its namespace NS's. */
  secret: string;
  /** Its signing key's `kid`. */
  signingKid: string;
}

let acme: Acme;
let globex: Org;
let receiver: WebhookReceiver;
let acmeWorker: CallStream | undefined;

async function untilWaiting(api: Api, id: string, match: (w: any) => boolean): Promise<any> {
  for (let i = 0; i < 120; i++) {
    const state = await api.get(`/v1/runs/${id}`);
    const found = (state.json?.waitingFor ?? []).find(match);
    if (found) return found;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`run ${id} never waited as expected`);
}

test.beforeAll(async () => {
  const org = await newOrg("tenancy-acme");
  globex = await newOrg("tenancy-globex");
  receiver = await startWebhookReceiver();
  const actionSet = await org.api.post("/v1/action-sets", HARNESS_ACTIONS);
  expectOk(actionSet);
  const actionSetId = actionSet.json.id as string;

  const waiting = (await waitingRun(org, NS, `${MARK} waits`)).id as string;

  const completed = await startRun(org.api, NS, { wait: "30s", code: COMPLETED_CODE });
  expectOk(completed);
  expect(completed.json.status, completed.text).toBe("completed");

  const shared = await startRun(org.api, NS, { code: SHARED_CODE, wait: "30s" });
  expect(shared.json.status, shared.text).toBe("completed");

  const failed = await startRun(org.api, NS, { code: FAILING_CODE, wait: "30s" });
  expect(failed.json.status, failed.text).toBe("failed");

  const approvalRun = await startRun(org.api, NS, { code: APPROVAL_CODE, actionSet: actionSetId });
  expectOk(approvalRun);
  const approval = await untilWaiting(org.api, approvalRun.json.id, (w) => w.kind === "call" && w.state === "approval");

  // acme's worker holds a stream open for the whole file: the call handed on it stays its own.
  acmeWorker = await CallStream.open(org.api, { actionSet: actionSetId, actions: ["crm.lookup"] });
  const callRun = await startRun(org.api, NS, { code: LOOKUP_CODE, actionSet: actionSetId });
  expectOk(callRun);
  const handed = await acmeWorker.callOf("crm.lookup", callRun.json.id);

  const subscription = await org.api.post("/v1/subscriptions", { url: receiver.url, labels: { owner: MARK } });
  expectOk(subscription, 201);

  const role = `${MARK}-auditor`;
  expectOk(await org.api.put(`/v1/roles/${role}`, { description: `${MARK} role`, permissions: ["runs:read"] }));
  const key = await createKey(org.api, "reader", { name: `${MARK} reader` });
  const token = await mintToken(org.api, NS);
  const signer = await signingKey("EdDSA");
  const registered = await org.api.post("/v1/organization/token-keys", { name: `${MARK} signer`, jwk: signer.publicJwk });
  expectOk(registered, 201);
  const secret = `${MARK}-github`;
  expectOk(await org.api.put(`/v1/secrets/${secret}`, { value: `${MARK}-organization-value`, labels: { owner: MARK } }), 201);
  expectOk(await org.api.put(`/v1/namespaces/${NS}/secrets/${secret}`, { value: `${MARK}-alice-value` }), 201);
  const rotated = await org.api.post("/v1/organization/signing-keys");
  expectOk(rotated, 200);

  acme = {
    org,
    actionSet: actionSetId,
    waiting,
    completed: { id: completed.json.id, outputDigest: completed.json.output.digest, workflow: completed.json.workflow.id },
    shared: { id: shared.json.id, outputDigest: shared.json.output.digest },
    failed: failed.json.id,
    approval: { runId: approvalRun.json.id, callId: approval.callId },
    call: { runId: callRun.json.id, token: handed.token, id: handed.id },
    subscription: subscription.json.id,
    key,
    role,
    token,
    kid: registered.json.kid,
    secret,
    signingKid: rotated.json.kid,
  };
});

test.afterAll(async () => {
  await Promise.all([receiver?.close(), acmeWorker?.close()]);
});

/** Asserts no answer of globex holds anything of acme's. */
function expectNothingOf(reply: Reply, what: string): void {
  expectOk(reply);
  const secrets = [
    MARK,
    acme.org.id,
    acme.waiting,
    acme.completed.id,
    acme.failed,
    acme.actionSet,
    acme.completed.outputDigest,
    acme.completed.workflow,
    acme.subscription,
    acme.key.id,
    acme.kid,
    acme.signingKid,
    acme.approval.runId,
    acme.call.runId,
  ];
  for (const secret of secrets) {
    expect(reply.text.includes(secret), `${what} holds ${secret}: ${reply.text.slice(0, 400)}`).toBe(false);
  }
}

test("each organization sees its own runs, and only them", { tag: "@success" }, async () => {
  const mine = await acme.org.api.get("/v1/runs", { query: { limit: 100 } });
  expectOk(mine, 200);
  const ids = (mine.json.runs as { id: string }[]).map((r) => r.id);
  expect(ids).toEqual(expect.arrayContaining([acme.waiting, acme.completed.id, acme.failed]));
  const theirs = await globex.api.get("/v1/runs", { query: { limit: 100 } });
  expectNothingOf(theirs, "globex's runs");
  const whoami = await globex.api.get("/v1/whoami");
  expect(whoami.json.organizationId).toBe(globex.id);
  evidence(`acme lists its ${ids.length} runs; globex lists none of them`);
});

test("another organization's run is not found, whatever is asked of it", { tag: "@attack" }, async () => {
  const id = acme.waiting;
  const probe = () => runProbe(acme.org.api, id);
  const asGlobex = globex.api;
  const attempts: [string, () => Promise<Reply>][] = [
    ["GET run", () => asGlobex.get(`/v1/runs/${id}`)],
    ["GET run, waiting", () => asGlobex.get(`/v1/runs/${id}`, { query: { wait: "1s" } })],
    ["GET export", () => asGlobex.get(`/v1/runs/${id}/export`)],
    ["GET cassette", () => asGlobex.get(`/v1/runs/${id}/export`, { query: { as: "cassette" } })],
    ["GET event 0", () => asGlobex.get(`/v1/runs/${id}/events/0`)],
    ["GET transcript", () => asGlobex.get(`/v1/runs/${id}/transcript`)],
    ["GET events", () => asGlobex.get(`/v1/runs/${id}/events`)],
    ["signal", () => asGlobex.post(`/v1/runs/${id}/signals/go`, { n: 666 }, { headers: { "idempotency-key": unique("x") } })],
    ["signal in its namespace", () => asGlobex.ns(NS).post(`/v1/runs/${id}/signals/go`, { n: 666 })],
    ["cancel", () => asGlobex.post(`/v1/runs/${id}/cancel`, { reason: "hostile" })],
    ["fork", () => asGlobex.ns(NS).post(`/v1/runs/${id}/fork`, { code: "export default async function main() { return 'stolen'; }" })],
    ["release", () => asGlobex.post(`/v1/runs/${id}/release`)],
    ["give up", () => asGlobex.post(`/v1/runs/${id}/give-up`)],
    ["erase", () => asGlobex.delete(`/v1/runs/${id}`)],
  ];
  const seen: string[] = [];
  for (const [what, attempt] of attempts) {
    const reply = await expectRefused({ attempt, status: 404, slug: "not-found", probe });
    expect(reply.text.includes(MARK), `${what}: the answer names nothing of acme's`).toBe(false);
    seen.push(`${what} → 404`);
  }
  // The run still waits for acme, and goes on when acme signals it.
  expectOk(await acme.org.api.post(`/v1/runs/${id}/signals/go`, { n: 7 }));
  const ended = await acme.org.api.get(`/v1/runs/${id}`, { query: { wait: "30s" } });
  expect(ended.json.status, ended.text).toBe("completed");
  expect(ended.json.output.value).toEqual({ note: `${MARK} waits`, n: 7 });
  evidence(`${seen.length} requests on acme's run with globex's key, each 404 not-found; acme's history unchanged; acme's own signal completes it with n=7`);
});

test("another organization's change stream shows nothing of its runs", { tag: "@attack" }, async () => {
  const theirs = await follow(globex.api, "/v1/changes");
  const mine = await follow(acme.org.api, "/v1/changes");
  try {
    expect(theirs.status, "globex may follow its own changes").toBe(200);
    const run = await startRun(acme.org.api, NS, { wait: "30s", code: COMPLETED_CODE });
    expectOk(run);
    const id = run.json.id as string;
    const announced = await mine.until((e) => e.event === "run" && e.data?.id === id && e.data?.status === "completed", 15_000);
    expect(announced, "acme's own stream announces its run").toBeDefined();
    // The change loop passes every second: two more, and globex's stream would have had it.
    await new Promise((resolve) => setTimeout(resolve, 2_000));
    const leaked = theirs.events.filter((e) => e.data?.id === id || JSON.stringify(e.data ?? "").includes(MARK));
    expect(leaked, "globex's stream names nothing of acme's").toEqual([]);
    evidence(`acme's stream announced ${id} completed; globex's stream, open all along, got ${theirs.events.length} changes, none of acme's`);
  } finally {
    theirs.close();
    mine.close();
  }
});

test("another organization's lists, statistics and fingerprints hold nothing of it", { tag: "@attack" }, async () => {
  const queries: [string, string, Record<string, string>?][] = [
    ["runs", "/v1/runs"],
    ["runs by id", "/v1/runs", { q: acme.completed.id }],
    ["runs by namespace", "/v1/runs", { q: `namespace:${NS}` }],
    ["runs of acme's namespace", "/v1/runs"],
    ["failed runs", "/v1/runs", { status: "failed" }],
    ["stats", "/v1/runs/stats"],
    ["fingerprints", "/v1/runs/failures"],
    ["action statistics", "/v1/actions/stats"],
    ["action sets", "/v1/action-sets"],
    ["workflows", "/v1/workflows"],
    ["namespaces", "/v1/namespaces"],
    ["subscriptions", "/v1/subscriptions"],
    ["keys", "/v1/api-keys"],
    ["roles", "/v1/roles"],
    ["token keys", "/v1/organization/token-keys"],
    ["quota alerts", "/v1/organization/quota-alerts"],
    ["namespace defaults", "/v1/organization/namespace-defaults"],
    ["secrets", "/v1/secrets"],
    ["secrets of acme's namespace's name", `/v1/namespaces/${NS}/secrets`],
    ["signing keys", "/v1/organization/signing-keys"],
  ];
  for (const [what, path, query] of queries) {
    const api = what === "runs of acme's namespace" ? globex.api.ns(NS) : globex.api;
    const reply = await api.get(path, { query });
    if (reply.status === 400 && query?.q) continue; // a query the language refuses leaks nothing either
    expectNothingOf(reply, what);
  }
  // acme's own fingerprints do hold its failure: the check above is not vacuous.
  const own = await acme.org.api.get("/v1/runs/failures");
  expect(own.text).toContain(MARK);
  evidence(`${queries.length} lists of globex (runs, stats, fingerprints, workflows, action stats, action sets, namespaces, subscriptions, keys, roles, token keys, alerts, secrets, signing keys) hold no id, digest or name of acme's`);
});

test("another organization's workflow and action set are not found, and its action set starts nothing", { tag: "@attack" }, async () => {
  const runs = async () => (await globex.api.get("/v1/runs", { query: { limit: 100 } })).json.runs.length;
  expectProblem(await globex.api.get(`/v1/workflows/${acme.completed.workflow}`), 404, "not-found");
  expectProblem(await globex.api.get(`/v1/action-sets/${acme.actionSet}/declarations`), 404, "not-found");
  expectProblem(await globex.api.get(`/v1/action-sets/${acme.actionSet}/search`, { query: { query: "refund" } }), 404, "not-found");
  // acme's secrets: their usage and their deletion are another organization's, not found.
  const acmeSecrets = async () => (await acme.org.api.get("/v1/secrets")).json.secrets.map((s: { reference: string }) => s.reference);
  for (const path of [`/v1/secrets/${acme.secret}/usage`, `/v1/namespaces/${NS}/secrets/${acme.secret}/usage`]) {
    expectProblem(await globex.api.get(path), 404, "not-found");
  }
  for (const path of [`/v1/secrets/${acme.secret}`, `/v1/namespaces/${NS}/secrets/${acme.secret}`]) {
    await expectRefused({ attempt: () => globex.api.delete(path), status: 404, slug: "not-found", probe: acmeSecrets });
  }
  expect(await acmeSecrets()).toEqual([`org/${acme.secret}`]);
  expect((await acme.org.api.get(`/v1/namespaces/${NS}/secrets`)).json.secrets.map((s: { reference: string }) => s.reference)).toEqual([`ns/${acme.secret}`]);
  await expectRefused({ attempt: () => startRun(globex.api, "globex-ns", { code: LOOKUP_CODE, actionSet: acme.actionSet }), status: 404, slug: "not-found", probe: runs });
  await expectRefused({ attempt: () => globex.api.ns("globex-ns").post("/v1/check", { code: LOOKUP_CODE, actionSet: acme.actionSet }), status: 404, slug: "not-found" });
  const derived = await globex.api.post("/v1/action-sets", { base: acme.actionSet, harness: [] });
  expectProblem(derived, 404, "not-found");
  // acme still reads them.
  expectOk(await acme.org.api.get(`/v1/workflows/${acme.completed.workflow}`), 200);
  expectOk(await acme.org.api.get(`/v1/action-sets/${acme.actionSet}/declarations`), 200);
  evidence("globex: acme's workflow, its action set's declarations and search, secrets' usage and deletion (the organization's and a namespace's), a run, a check and a derived action set from acme's action set id → 404; acme still reads them and keeps its secrets");
});

test("another organization's workflow is not found, and starts nothing", { tag: "@attack" }, async () => {
  const name = `${MARK}_flow`;
  const saved = await acme.org.api.post("/v1/workflows", {
    name,
    actionSet: acme.actionSet,
    code: `export default async function main(input: { n: number }) {\n  return { n: input.n };\n}\n`,
    input: { type: "object", properties: { n: { type: "number" } }, required: ["n"] },
    output: { type: "object", properties: { n: { type: "number" } }, required: ["n"] },
  });
  expectOk(saved);
  const runs = async () => (await globex.api.get("/v1/runs", { query: { limit: 100 } })).json.runs.length;
  expectProblem(await globex.api.get(`/v1/workflows/${name}`), 404, "not-found");
  await expectRefused({ attempt: () => globex.api.ns("globex-ns").post("/v1/runs", { workflow: name, input: { n: 1 } }), status: 404, slug: "not-found", probe: runs });
  const digest = String(saved.json.version.digest);
  expect(digest).toMatch(/^[0-9a-f]{64}$/);
  await expectRefused({ attempt: () => globex.api.ns("globex-ns").post("/v1/runs", { workflow: `${name}@${digest}`, input: { n: 1 } }), status: 404, slug: "not-found", probe: runs });
  expectOk(await acme.org.api.get(`/v1/workflows/${name}`), 200);
  evidence("globex: GET acme's workflow, and a run of it by name and by its exact version digest → 404; nothing started");
});

test("a payload is read only through a run of the reader's organization, even when both hold the same bytes", { tag: "@attack" }, async () => {
  // Payloads up to 1 KiB stay in their event, and GET /v1/payloads serves the larger ones through
  // their run: acme's output is a payload record of its run, the shared one a blob.
  const digest = acme.completed.outputDigest;
  expect(digest).toMatch(/^[0-9a-f]{64}$/);
  // Without its run, a payload is read by no one: the request is malformed, whoever asks.
  expectProblem(await globex.api.get(`/v1/payloads/${digest}`), 400, "invalid-request");
  expectProblem(await acme.org.api.get(`/v1/payloads/${digest}`), 400, "invalid-request");
  expectProblem(await globex.api.get(`/v1/payloads/${digest}`, { query: { run: acme.completed.id } }), 404, "not-found");
  // The same bytes in globex: its own run reads them, acme's run still does not lend them.
  const mine = await startRun(globex.api, "globex-ns", { code: SHARED_CODE, wait: "30s" });
  expect(mine.json.status, mine.text).toBe("completed");
  expect(mine.json.output.digest).toBe(acme.shared.outputDigest);
  const own = await globex.api.get(`/v1/payloads/${mine.json.output.digest}`, { query: { run: mine.json.id } });
  expectOk(own, 200);
  expectProblem(await globex.api.get(`/v1/payloads/${acme.shared.outputDigest}`, { query: { run: acme.shared.id } }), 404, "not-found");
  // acme reads its own.
  expectOk(await acme.org.api.get(`/v1/payloads/${digest}`, { query: { run: acme.completed.id } }), 200);
  expectOk(await acme.org.api.get(`/v1/payloads/${acme.shared.outputDigest}`, { query: { run: acme.shared.id } }), 200);
  evidence("globex: acme's 2 KiB payload without a run → 400, through acme's run → 404; with the same 100 KiB blob, through its own run → 200, through acme's run → 404");
});

test("another organization's harness call cannot be handed, answered nor approved", { tag: "@attack" }, async () => {
  const forged = `${acme.call.token.slice(0, -4)}${acme.call.token.endsWith("aaaa") ? "bbbb" : "aaaa"}`;
  // A token never issued, for comparison: globex must not tell acme's token from it.
  const unknownAnswer = await globex.api.post(`/v1/calls/${forged}/defer`, {});
  expect([404, 410], unknownAnswer.text).toContain(unknownAnswer.status);
  const callProbe = () => runProbe(acme.org.api, acme.call.runId);
  for (const [what, attempt] of [
    ["complete", () => globex.api.post(`/v1/calls/${acme.call.token}/complete`, { result: { id: "stolen" } })],
    ["fail", () => globex.api.post(`/v1/calls/${acme.call.token}/fail`, { message: "hostile", retryable: false })],
    ["defer", () => globex.api.post(`/v1/calls/${acme.call.token}/defer`, {})],
  ] as [string, () => Promise<Reply>][]) {
    await expectRefused({ attempt, status: unknownAnswer.status, slug: unknownAnswer.slug!, probe: callProbe });
    void what;
  }
  // A stream of globex's for acme's action set is handed none of acme's calls, or is not found.
  const opened = await CallStream.tryOpen(globex.api, { actionSet: acme.actionSet, actions: ["crm.lookup", "pay.refund"] });
  if (opened instanceof CallStream) {
    await new Promise((resolve) => setTimeout(resolve, 1_000));
    await opened.close();
    expect(opened.handed).toEqual([]);
  } else {
    expectProblem(opened, 404, "not-found");
  }
  // Nor is one for acme's actions by name, whatever their action set.
  expect(await handedWithin(globex.api, { actions: ["crm.lookup", "pay.refund"] })).toEqual([]);
  // The approval waiting in acme cannot be decided by globex.
  const approvalProbe = () => runProbe(acme.org.api, acme.approval.runId);
  await expectRefused({ attempt: () => globex.api.post(`/v1/calls/${acme.approval.callId}/approve`), status: 404, slug: "not-found", probe: approvalProbe });
  await expectRefused({ attempt: () => globex.api.post(`/v1/calls/${acme.approval.callId}/reject`, { message: "hostile" }), status: 404, slug: "not-found", probe: approvalProbe });
  // acme still answers both.
  expectOk(await acme.org.api.post(`/v1/calls/${acme.call.token}/complete`, { result: { id: "cus_1" } }));
  const done = await acme.org.api.get(`/v1/runs/${acme.call.runId}`, { query: { wait: "30s" } });
  expect(done.json.status, done.text).toBe("completed");
  expect(done.json.output.value).toEqual({ customer: { id: "cus_1" } });
  expectOk(await acme.org.api.post(`/v1/calls/${acme.approval.callId}/reject`, { message: "not today" }));
  evidence(
    `globex with acme's call token: complete, fail, defer → ${unknownAnswer.status} ${unknownAnswer.slug ?? ""}, as for a token never issued; globex's call streams are handed nothing; approve and reject of acme's parked call → 404; acme then completes its call (cus_1) and rejects its approval`,
  );
});

test("another organization's subscriptions, keys, roles, tokens and token keys are not found", { tag: "@attack" }, async () => {
  const subscriptions = async () => (await acme.org.api.get("/v1/subscriptions")).json;
  await expectRefused({ attempt: () => globex.api.delete(`/v1/subscriptions/${acme.subscription}`), status: 404, slug: "not-found", probe: subscriptions });

  const keys = async () => (await acme.org.api.get("/v1/api-keys")).json.keys.map((k: { id: string; revokedAt?: string | null }) => `${k.id}:${k.revokedAt ?? ""}`);
  await expectRefused({ attempt: () => globex.api.delete(`/v1/api-keys/${acme.key.id}`), status: 404, slug: "not-found", probe: keys });
  expectOk(await new Api(stacks().a.url, acme.key.key).get("/v1/runs"), 200);

  const roles = async () => (await acme.org.api.get("/v1/roles")).json;
  await expectRefused({ attempt: () => globex.api.delete(`/v1/roles/${acme.role}`), status: 404, slug: "not-found", probe: roles });
  // acme's role is no role of globex: a key of it cannot be made there.
  const borrowed = await globex.api.post("/v1/api-keys", { name: "borrowed role", role: acme.role });
  expectProblem(borrowed, 400, "invalid-request");

  await expectRefused({ attempt: () => globex.api.delete(`/v1/tokens/${acme.token.id}`), status: 404, slug: "not-found" });
  expectOk(await new Api(stacks().a.url, acme.token.token).get("/v1/runs"), 200);

  const tokenKeys = async () => (await acme.org.api.get("/v1/organization/token-keys")).json;
  await expectRefused({ attempt: () => globex.api.delete(`/v1/organization/token-keys/${acme.kid}`), status: 404, slug: "not-found", probe: tokenKeys });

  // A namespace's name is the organization's own: globex erasing "the same name" erases nothing of acme's.
  const acmeRuns = async () => (await acme.org.api.ns(NS).get("/v1/runs", { query: { limit: 100 } })).json.runs.length;
  const before = await acmeRuns();
  const erase = await globex.api.delete(`/v1/namespaces/${NS}`);
  expect([202, 404], erase.text).toContain(erase.status);
  expect(await acmeRuns()).toBe(before);
  const detail = await acme.org.api.get(`/v1/namespaces/${NS}`);
  expectOk(detail, 200);
  evidence("globex: acme's subscription, key, role, opaque token and token key → 404, each still works for acme; a key of acme's role → 400; erasing a namespace of the same name leaves acme's runs");
});

test("an end user's token of one organization reaches nothing of another", { tag: "@attack" }, async () => {
  const globexRun = await startRun(globex.api, "globex-ns", { wait: "30s" });
  expect(globexRun.json.status).toBe("completed");
  const asAcmeToken = new Api(stacks().a.url, acme.token.token);
  expectProblem(await asAcmeToken.get(`/v1/runs/${globexRun.json.id}`), 404, "not-found");
  expectProblem(await asAcmeToken.get(`/v1/runs/${globexRun.json.id}/export`), 404, "not-found");
  expectProblem(await asAcmeToken.post(`/v1/runs/${globexRun.json.id}/cancel`, {}), 404, "not-found");
  const listed = await asAcmeToken.get("/v1/runs");
  expectOk(listed, 200);
  expect(listed.text).not.toContain(globexRun.json.id);
  evidence("acme's end user's token: globex's run, its export and a cancel → 404; its list holds none of globex's runs");
});

test("a signal URL reaches only the run that made it, at its exact token", { tag: "@attack" }, async () => {
  const started = await startRun(acme.org.api, NS, { code: SIGNAL_URL_CODE });
  expectOk(started);
  const id = started.json.id as string;
  await untilWaiting(acme.org.api, id, (w) => w.kind === "signal" && w.name === "paid");
  const exported = await acme.org.api.get(`/v1/runs/${id}/export`);
  const found = /\/hooks\/([A-Za-z0-9_.~-]+)/.exec(exported.text);
  expect(found, "the history holds the signal URL").not.toBeNull();
  const token = found![1]!;
  const hooks = new Api(stacks().a.url);
  const probe = () => runProbe(acme.org.api, id);
  const flipped = `${token.slice(0, -1)}${token.endsWith("A") ? "B" : "A"}`;
  for (const guess of [flipped, token.slice(0, -1), `${token}x`, token.toUpperCase() === token ? token.toLowerCase() : token.toUpperCase(), "../hooks", "%2e%2e"]) {
    await expectRefused({ attempt: () => hooks.post(`/hooks/${guess}`, { amount: 666 }), status: 404, slug: "not-found", probe });
  }
  // With globex's key, the real URL is the same unauthenticated door: the payload reaches acme's run, nothing else.
  const delivered = await hooks.as(globex.ownerKey).post(`/hooks/${token}`, { amount: 12 });
  expect(delivered.status, delivered.text).toBeLessThan(300);
  const ended = await acme.org.api.get(`/v1/runs/${id}`, { query: { wait: "30s" } });
  expect(ended.json.status, ended.text).toBe("completed");
  expect(ended.json.output.value.amount).toBe(12);
  // Once the run has ended, its token takes nothing more.
  const late = await hooks.post(`/hooks/${token}`, { amount: 99 });
  expect([404, 409, 410], late.text).toContain(late.status);
  evidence(`six guesses near the token (one character changed, cut, lengthened, case, path tricks) → 404 and the run still waits; the exact token delivers amount 12; after the run ended → ${late.status}`);
});
