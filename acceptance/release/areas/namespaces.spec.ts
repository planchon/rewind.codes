/**
 * Area "Namespaces and tokens" of the release gate (`archi/06-storage-and-scale.md`, "Namespaces",
 * "Quotas", "Who calls", "What the caller may do"; `api/README.md`).
 *
 * A namespace isolates one end user of a customer: Alice's agent holds a token of Alice's
 * namespace and reaches nothing of Bob's, whatever it sends or whatever script its model writes.
 * Each end user has quotas, secrets of their own that only their runs attach, and can be erased.
 */
import { eventually } from "../../support/processes.js";
import { hmacConfusedToken, type SigningKey, signingKey, startMcpFixture, type TokenAlg, unsignedToken } from "../support/fixtures.js";
import {
  Api,
  canary,
  evidence,
  expect,
  expectOk,
  expectProblem,
  expectRefused,
  newTenant,
  operator,
  type Reply,
  stacks,
  type Tenant,
  test,
  unique,
} from "../support/gate.js";
import { namespaceState, runState, short, start, TOKEN_ALL, TOKEN_DEFAULT, untilWaiting, waiting } from "../support/namespaces-helpers.js";
import { TaskStream } from "../support/tools-helpers.js";

const now = () => Math.floor(Date.now() / 1000);

/** Mints an opaque token of `namespace`; returns its secret and id. */
async function mint(owner: Api, namespace: string, body: Record<string, unknown> = {}): Promise<{ token: string; id: string; json: any }> {
  const reply = await owner.post(`/v1/namespaces/${encodeURIComponent(namespace)}/tokens`, body);
  expectOk(reply, 201);
  expect(reply.json.token).toMatch(/^rewind_nt_/);
  return { token: reply.json.token, id: reply.json.id, json: reply.json };
}

/** Registers a token key; returns it with its `kid`. */
async function register(owner: Api, alg: TokenAlg, permissions?: string[]): Promise<SigningKey> {
  const key = await signingKey(alg);
  const reply = await owner.post("/v1/organization/token-keys", { name: `${alg} key`, jwk: key.publicJwk, permissions });
  expectOk(reply, 201);
  expect(reply.json.kid).toMatch(/^rk_/);
  key.kid = reply.json.kid;
  return key;
}

// =============================================================================================
// The namespace a key's request acts for

test.describe("the Rewind-Namespace header", () => {
  let tenant: Tenant;
  /** The same on setup B, one node. */
  let strict: Tenant;
  test.beforeAll(async () => {
    tenant = await newTenant("ns-header");
    strict = await newTenant("ns-header", stacks().b);
  });

  test("a key must name the namespace to start a run, check or test a script", { tag: "@failure" }, async () => {
    const script = short(unique("no-header"));
    const before = await tenant.api.get("/v1/namespaces");
    for (const [api, path] of [
      [tenant.api, "/v1/runs"],
      [tenant.api, "/v1/check"],
      [strict.api, "/v1/test"],
    ] as const) {
      const reply = await api.post(path, { script });
      expectProblem(reply, 400, "invalid-request");
      expect(reply.json.detail).toContain("Rewind-Namespace");
    }
    const after = await tenant.api.get("/v1/namespaces");
    expect(after.json.total).toBe(before.json.total);
    evidence("POST /v1/runs, /v1/check and /v1/test without Rewind-Namespace → 400 invalid-request naming the header; no namespace created");
  });

  test("a key's header narrows its reads, and a fork stays in its parent's namespace", { tag: "@success" }, async () => {
    const alice = tenant.api.ns("alice");
    const bob = tenant.api.ns("bob");
    const hers = await start(alice, { script: waiting(unique("hers")) });
    const his = await start(bob, { script: waiting(unique("his")) });
    expect(hers.namespace).toBe("alice");
    expect(his.namespace).toBe("bob");
    await untilWaiting(tenant.api, hers.id);
    // Another namespace's header misses the run, for every request.
    for (const reply of [
      await bob.get(`/v1/runs/${hers.id}`),
      await bob.post(`/v1/runs/${hers.id}/signals/approval`, { ok: true }),
      await bob.post(`/v1/runs/${hers.id}/cancel`, {}),
      await bob.post(`/v1/runs/${hers.id}/fork`, { script: waiting(unique("fork")) }),
    ]) {
      expectProblem(reply, 404, "not-found");
    }
    // A fork takes its parent's namespace, with the header or without it.
    const fork = await tenant.api.post(`/v1/runs/${hers.id}/fork`, { script: waiting(unique("fork")) });
    expectOk(fork);
    expect(fork.json.namespace).toBe("alice");
    // Reads: that namespace with the header, every namespace without it.
    await eventually("the read model lists the three runs", async () => {
      const all = (await tenant.api.get("/v1/runs")).json.runs.map((r: any) => r.id);
      const mine = (await alice.get("/v1/runs")).json.runs.map((r: any) => r.namespace);
      const theirs = (await bob.get("/v1/runs")).json.runs.map((r: any) => r.namespace);
      return all.length >= 3 && mine.length === 2 && theirs.length === 1 ? true : undefined;
    });
    const mine = (await alice.get("/v1/runs")).json.runs;
    expect(mine.every((r: any) => r.namespace === "alice")).toBe(true);
    expect(mine.map((r: any) => r.id)).not.toContain(his.id);
  });
});

// =============================================================================================
// Opaque tokens

test.describe("opaque tokens", () => {
  let tenant: Tenant;
  test.beforeAll(async () => {
    tenant = await newTenant("ns-opaque");
  });

  test("a minted token holds the default permissions, in its namespace only, and creates it", { tag: "@success" }, async () => {
    const namespace = unique("carol-");
    expect(await namespaceState(tenant.api, namespace)).toEqual({ exists: false });
    const minted = await mint(tenant.api, namespace);
    expect(minted.json.permissions).toEqual(TOKEN_DEFAULT);
    expect(minted.json.namespace).toBe(namespace);
    const lifetime = Date.parse(minted.json.expiresAt) - Date.parse(minted.json.createdAt);
    expect(lifetime).toBeGreaterThan(59 * 60_000);
    expect(lifetime).toBeLessThanOrEqual(60 * 60_000 + 1000);
    expect(await namespaceState(tenant.api, namespace)).toMatchObject({ exists: true, tokens: 1 });

    const carol = tenant.api.as(minted.token);
    const who = await carol.get("/v1/whoami");
    expectOk(who, 200);
    expect(who.json.tenant).toBe(tenant.id);
    expect(who.json.operator).toBe(false);
    expect(who.json.permissions).toEqual(TOKEN_DEFAULT);
    expect(who.json.principal).toMatchObject({ kind: "token", token: "opaque", namespace, id: minted.id });

    const run = await start(carol, { script: short(unique("carol")), wait: "20s" });
    expect(run.namespace).toBe(namespace);
    expect(run.status).toBe("completed");
    const erase = await carol.delete(`/v1/runs/${run.id}`);
    expectProblem(erase, 403, "forbidden");
    expect(erase.json.detail).toContain("runs:delete");
    expectProblem(await carol.get("/v1/namespaces"), 403, "forbidden");
    expectProblem(await carol.get("/v1/api-keys"), 403, "forbidden");
  });

  test("a token with chosen permissions and labels: its runs carry the labels, and it holds nothing more", { tag: "@success" }, async () => {
    const namespace = unique("dave-");
    const minted = await mint(tenant.api, namespace, { permissions: ["runs:read", "runs:start"], labels: { agent: "triage" } });
    expect(minted.json.permissions).toEqual(["runs:read", "runs:start"]);
    const dave = tenant.api.as(minted.token);
    const run = await start(dave, { script: waiting(unique("dave")) });
    expect(run.labels).toEqual({ agent: "triage" });
    await untilWaiting(tenant.api, run.id);
    const signal = await dave.post(`/v1/runs/${run.id}/signals/approval`, { ok: true });
    expectProblem(signal, 403, "forbidden");
    expect(signal.json.detail).toContain("runs:control");
    const contradicting = await dave.post("/v1/runs", { script: short(unique("dave")), labels: { agent: "other" } });
    expectProblem(contradicting, 403, "forbidden");
  });

  test("a token never holds an administration permission, more than its minter holds, or more than 30 days", { tag: "@attack" }, async () => {
    const namespace = unique("erin-");
    await expectRefused({
      attempt: () => tenant.api.post(`/v1/namespaces/${namespace}/tokens`, { permissions: ["namespaces:read"] }),
      status: 400,
      slug: "invalid-request",
      probe: () => namespaceState(tenant.api, namespace),
    });
    await expectRefused({
      attempt: () => tenant.api.post(`/v1/namespaces/${namespace}/tokens`, { permissions: ["api_keys:write"] }),
      status: 400,
      slug: "invalid-request",
      probe: () => namespaceState(tenant.api, namespace),
    });
    await expectRefused({
      attempt: () => tenant.api.post(`/v1/namespaces/${namespace}/tokens`, { ttl: "31d" }),
      status: 400,
      slug: "invalid-request",
      probe: () => namespaceState(tenant.api, namespace),
    });
    // A key whose role mints read-only tokens gives no more than it holds.
    const role = unique("minter-");
    expectOk(await tenant.api.put(`/v1/roles/${role}`, { description: "Mints read-only tokens.", permissions: ["runs:read", "tokens:write"] }));
    const minter = tenant.api.as((await tenant.key(role)).key);
    const given = await minter.post(`/v1/namespaces/${namespace}/tokens`, {});
    expectOk(given, 201);
    expect(given.json.permissions).toEqual(["runs:read"]);
    await expectRefused({
      attempt: () => minter.post(`/v1/namespaces/${namespace}/tokens`, { permissions: ["runs:read", "runs:start"] }),
      status: 403,
      slug: "forbidden",
      probe: () => namespaceState(tenant.api, namespace),
    });
    const reader = tenant.api.as((await tenant.key("reader")).key);
    await expectRefused({
      attempt: () => reader.post(`/v1/namespaces/${namespace}/tokens`, {}),
      status: 403,
      slug: "forbidden",
      probe: () => namespaceState(tenant.api, namespace),
    });
    evidence("administration permission or 31d → 400; more than the minter holds, or a reader minting → 403; no token minted");
  });

  test("an expired token is refused", { tag: "@attack" }, async () => {
    const namespace = unique("fay-");
    const minted = await mint(tenant.api, namespace, { ttl: "1s" });
    const fay = tenant.api.as(minted.token);
    expectOk(await fay.get("/v1/whoami"), 200);
    await new Promise((resolve) => setTimeout(resolve, 1500));
    await expectRefused({
      attempt: () => fay.post("/v1/runs", { script: short(unique("fay")) }),
      status: 401,
      slug: "unauthorized",
      probe: () => namespaceState(tenant.api, namespace),
    });
    evidence("a token past its 1 s TTL → 401 unauthorized; no run created");
  });

  test("a revoked token is refused from the next request, and revoking a namespace's tokens refuses the rest", { tag: "@attack" }, async () => {
    const namespace = unique("gus-");
    const first = await mint(tenant.api, namespace);
    const second = await mint(tenant.api, namespace);
    const third = await mint(tenant.api, namespace);
    expectOk(await tenant.api.as(first.token).get("/v1/whoami"), 200);
    expectOk(await tenant.api.delete(`/v1/tokens/${first.id}`), 204);
    await expectRefused({
      attempt: () => tenant.api.as(first.token).post("/v1/runs", { script: short(unique("gus")) }),
      status: 401,
      slug: "unauthorized",
      probe: () => namespaceState(tenant.api, namespace),
    });
    expectOk(await tenant.api.as(second.token).get("/v1/whoami"), 200);
    const revoked = await tenant.api.delete(`/v1/namespaces/${namespace}/tokens`);
    expectOk(revoked, 200);
    expect(revoked.json.revoked).toBe(2);
    for (const token of [second.token, third.token]) {
      expectProblem(await tenant.api.as(token).get("/v1/whoami"), 401, "unauthorized");
    }
    expect(await namespaceState(tenant.api, namespace)).toMatchObject({ tokens: 0 });
    expectProblem(await tenant.api.delete(`/v1/tokens/${"0".repeat(64)}`), 404, "not-found");
    evidence("DELETE /v1/tokens/{id} and DELETE /v1/namespaces/{name}/tokens → the tokens get 401 from the next request");
  });

  test("a token is checked on every request: one revoked closes nothing of another namespace", { tag: "@success" }, async () => {
    const a = unique("hal-");
    const b = unique("ivy-");
    const hal = await mint(tenant.api, a);
    const ivy = await mint(tenant.api, b);
    expectOk(await tenant.api.delete(`/v1/namespaces/${a}/tokens`), 200);
    expectProblem(await tenant.api.as(hal.token).get("/v1/whoami"), 401, "unauthorized");
    expectOk(await tenant.api.as(ivy.token).get("/v1/whoami"), 200);
  });
});

// =============================================================================================
// Tokens the customer signs

test.describe("signed tokens", () => {
  let tenant: Tenant;
  let other: Tenant;
  let ed: SigningKey;
  /** A key of another tenant. */
  let foreign: SigningKey;

  test.beforeAll(async () => {
    tenant = await newTenant("ns-signed");
    other = await newTenant("ns-signed-other");
    ed = await register(tenant.api, "EdDSA", ["runs:read", "runs:start"]);
    foreign = await register(other.api, "EdDSA", ["runs:read", "runs:start", "runs:control"]);
  });

  for (const alg of ["EdDSA", "ES256", "RS256"] as const) {
    test(`a token signed with a registered ${alg} key acts in its namespace, with its labels`, { tag: "@success" }, async () => {
      const key = await register(tenant.api, alg, ["runs:read", "runs:start"]);
      const listed = await tenant.api.get("/v1/organization/token-keys");
      expectOk(listed, 200);
      const found = listed.json.keys.find((k: any) => k.kid === key.kid);
      expect(found).toMatchObject({ algorithm: alg, permissions: ["runs:read", "runs:start"] });
      const namespace = unique(`user-${alg.toLowerCase()}-`);
      const token = await key.sign({ sub: namespace, jti: "j-1", labels: { agent: "triage" } });
      const client = tenant.api.as(token);
      const who = await client.get("/v1/whoami");
      expectOk(who, 200);
      expect(who.json.principal).toMatchObject({ kind: "token", token: "signed", namespace, kid: key.kid, jti: "j-1", labels: { agent: "triage" } });
      expect(who.json.permissions).toEqual(["runs:read", "runs:start"]);
      const run = await start(client, { script: short(unique(alg)), wait: "20s" });
      expect(run.namespace).toBe(namespace);
      expect(run.labels).toEqual({ agent: "triage" });
      expect(run.status).toBe("completed");
    });
  }

  /** Forged and invalid tokens: each is refused (401) and starts nothing. */
  const forgeries: { name: string; token: () => Promise<{ token: string; sub: string }> }[] = [
    {
      name: "a token with alg none and no signature",
      token: async () => ({ token: unsignedToken({ sub: "mallory" }, ed.kid!), sub: "mallory" }),
    },
    {
      name: "a token signed with HS256 whose secret is the registered public key",
      token: async () => ({ token: await hmacConfusedToken({ sub: "mallory" }, ed.kid!, ed.publicJwk), sub: "mallory" }),
    },
    {
      name: "a token naming a kid nobody registered",
      token: async () => {
        const key = await signingKey("EdDSA");
        return { token: await key.sign({ sub: "mallory" }, { kid: "rk_unknown0000000000" }), sub: "mallory" };
      },
    },
    {
      name: "a token naming a registered kid, signed with another private key",
      token: async () => {
        const key = await signingKey("EdDSA");
        return { token: await key.sign({ sub: "mallory" }, { kid: ed.kid }), sub: "mallory" };
      },
    },
    {
      name: "a token whose header names another algorithm than its key's",
      token: async () => ({ token: await ed.sign({ sub: "mallory" }, { alg: "ES256" }).catch(() => forgedAlg(ed)), sub: "mallory" }),
    },
    {
      name: "an expired token",
      token: async () => ({ token: await ed.sign({ sub: "mallory", iat: now() - 3600, exp: now() - 600 }), sub: "mallory" }),
    },
    {
      name: "a token living more than 30 days",
      token: async () => ({ token: await ed.sign({ sub: "mallory", exp: now() + 31 * 86_400 }), sub: "mallory" }),
    },
    {
      name: "a token for another audience",
      token: async () => ({ token: await ed.sign({ sub: "mallory", aud: "someone-else" }), sub: "mallory" }),
    },
    {
      name: "a token asking for a permission its key may not give",
      token: async () => ({ token: await ed.sign({ sub: "mallory", permissions: ["runs:read", "runs:start", "runs:delete"] }), sub: "mallory" }),
    },
    {
      name: "a token asking for an administration permission",
      token: async () => ({ token: await ed.sign({ sub: "mallory", permissions: ["api_keys:write"] }), sub: "mallory" }),
    },
    {
      name: "a token whose sub is not a namespace name",
      token: async () => ({ token: await ed.sign({ sub: "not a namespace" }), sub: "not-a-namespace" }),
    },
    {
      name: "a token without exp",
      token: async () => ({ token: await ed.sign({ sub: "mallory", exp: undefined }), sub: "mallory" }),
    },
  ];

  for (const forgery of forgeries) {
    test(`refused: ${forgery.name}`, { tag: "@attack" }, async () => {
      const { token, sub } = await forgery.token();
      await expectRefused({
        attempt: () => tenant.api.as(token).post("/v1/runs", { script: short(unique("forged")) }),
        status: 401,
        slug: "unauthorized",
        probe: () => namespaceState(tenant.api, sub),
      });
      expectProblem(await tenant.api.as(token).get("/v1/whoami"), 401, "unauthorized");
      evidence(`${forgery.name} → 401 unauthorized; no run, no namespace`);
    });
  }

  test("refused: a token of a deleted key", { tag: "@attack" }, async () => {
    const key = await register(tenant.api, "ES256", ["runs:read", "runs:start"]);
    const namespace = unique("judy-");
    const token = await key.sign({ sub: namespace });
    expectOk(await tenant.api.as(token).get("/v1/whoami"), 200);
    expectOk(await tenant.api.delete(`/v1/organization/token-keys/${key.kid}`), 204);
    await expectRefused({
      attempt: () => tenant.api.as(token).post("/v1/runs", { script: short(unique("judy")) }),
      status: 401,
      slug: "unauthorized",
      probe: () => namespaceState(tenant.api, namespace),
    });
    expectProblem(await tenant.api.delete(`/v1/organization/token-keys/${key.kid}`), 404, "not-found");
    evidence("a key deleted → every token it signed gets 401");
  });

  test("refused: a signed token issued before its namespace's tokens were revoked", { tag: "@attack" }, async () => {
    const namespace = unique("kim-");
    const early = await ed.sign({ sub: namespace, iat: now() - 5 });
    expectOk(await tenant.api.as(early).get("/v1/whoami"), 200);
    const revoked = await tenant.api.delete(`/v1/namespaces/${namespace}/tokens`);
    expectOk(revoked, 200);
    await expectRefused({
      attempt: () => tenant.api.as(early).post("/v1/runs", { script: short(unique("kim")) }),
      status: 401,
      slug: "unauthorized",
      probe: () => namespaceState(tenant.api, namespace),
    });
    const late = await ed.sign({ sub: namespace, iat: now() + 2 });
    expectOk(await tenant.api.as(late).get("/v1/whoami"), 200);
    evidence("iat before tokensRevokedBefore → 401; a token issued after → accepted");
  });

  test("a token signed with another tenant's key reaches that tenant only", { tag: "@attack" }, async () => {
    // A run of this tenant's Alice.
    const hers = await start(tenant.api.ns("alice"), { script: waiting(unique("alice")) });
    await untilWaiting(tenant.api, hers.id);
    // The other customer signs a token for its own "alice".
    const token = await foreign.sign({ sub: "alice", permissions: ["runs:read", "runs:start", "runs:control"] });
    const intruder = tenant.api.as(token);
    const who = await intruder.get("/v1/whoami");
    expectOk(who, 200);
    expect(who.json.tenant).toBe(other.id);
    await expectRefused({
      attempt: () => intruder.get(`/v1/runs/${hers.id}`),
      status: 404,
      slug: "not-found",
      probe: () => runState(tenant.api, hers.id),
    });
    await expectRefused({
      attempt: () => intruder.post(`/v1/runs/${hers.id}/signals/approval`, { ok: true }),
      status: 404,
      slug: "not-found",
      probe: () => runState(tenant.api, hers.id),
    });
    const listed = await intruder.get("/v1/runs");
    expectOk(listed, 200);
    expect(listed.json.runs.map((r: any) => r.id)).not.toContain(hers.id);
    evidence("another tenant's rk_ key → its own tenant: this tenant's alice run is 404, never listed");
  });
});

/** A JWT whose header says ES256 while signed with an Ed25519 key (jose refuses to make it). */
async function forgedAlg(key: SigningKey): Promise<string> {
  const token = await key.sign({ sub: "mallory" });
  const [, body, signature] = token.split(".");
  const header = Buffer.from(JSON.stringify({ alg: "ES256", typ: "JWT", kid: key.kid })).toString("base64url");
  return `${header}.${body}.${signature}`;
}

// =============================================================================================
// Alice and Bob, two end users of one customer

test.describe("Alice and Bob", () => {
  let tenant: Tenant;
  /** Alice's token, with every permission a token may hold. */
  let alice: Api;
  /** Alice's worker: `tasks:work` only. */
  let aliceWorker: Api;
  let asking: string;
  let tickets: string;
  const bob: Record<string, any> = {};
  const hers: Record<string, any> = {};

  const ASK = (marker: string) => `// ${marker}
import { support } from "rewind:tools";

export default async function main() {
  return await support.ask({ question: "${marker}" });
}
`;
  const TICKET = `import { signal } from "rewind";

export default async function main(input: { id: string }) {
  const decision = await signal("approve");
  return { approved: decision.ok, id: input.id };
}
`;

  test.beforeAll(async () => {
    tenant = await newTenant("ns-alice-bob");
    const owner = tenant.api;
    const toolset = await owner.post("/v1/toolsets", {
      host: [
        {
          name: "support.ask",
          description: "Asks the end user a question.",
          inputSchema: { type: "object", properties: { question: { type: "string" } }, required: ["question"] },
          outputSchema: { type: "string" },
        },
      ],
    });
    expectOk(toolset);
    asking = toolset.json.id;
    const workflow = await owner.post("/v1/workflows", {
      name: "ticket",
      description: "A ticket waiting for approval.",
      toolset: asking,
      script: TICKET,
      input: { type: "object", properties: { id: { type: "string" } }, required: ["id"] },
      output: { type: "object", properties: { approved: { type: "boolean" }, id: { type: "string" } }, required: ["approved", "id"] },
      signals: { approve: { type: "object", properties: { ok: { type: "boolean" } }, required: ["ok"] } },
    });
    expectOk(workflow);
    const pinned = await owner.post("/v1/toolsets", { workflows: ["ticket"] });
    expectOk(pinned);
    tickets = pinned.json.id;

    // Bob's runs, started by the customer: one waits for a signal, one for a host tool, one is
    // a run of the workflow `ticket`.
    const asBob = owner.ns("bob");
    bob.waiting = await start(asBob, { script: waiting(unique("bob")), input: { secret: unique("bob-input-"), padding: "b".repeat(300 * 1024) } });
    bob.asking = await start(asBob, { script: ASK(unique("bob-asks")), toolset: asking });
    bob.ticket = await start(asBob, { workflow: "ticket", input: { id: unique("bob-ticket-") } });
    // Alice's.
    const asAlice = owner.ns("alice");
    hers.waiting = await start(asAlice, { script: waiting(unique("alice")), input: { secret: unique("alice-input-"), padding: "a".repeat(300 * 1024) } });
    hers.asking = await start(asAlice, { script: ASK(unique("alice-asks")), toolset: asking });
    for (const run of [bob.waiting, bob.asking, bob.ticket, hers.waiting, hers.asking]) {
      Object.assign(run, await untilWaiting(owner, run.id));
    }
    alice = owner.as((await mint(owner, "alice", { permissions: TOKEN_ALL })).token);
    aliceWorker = owner.as((await mint(owner, "alice", { permissions: ["tasks:work"] })).token);
  });

  /** Every request of Alice's token on Bob's waiting run: each must be 404, and leave it unchanged. */
  const onBobsRun: { what: string; send: () => Promise<import("../support/gate.js").Reply> }[] = [
    { what: "GET /v1/runs/{bob}", send: () => alice.get(`/v1/runs/${bob.waiting.id}`) },
    { what: "GET /v1/runs/{bob}/trace", send: () => alice.get(`/v1/runs/${bob.waiting.id}/trace`, { query: { format: "llm" } }) },
    { what: "GET /v1/runs/{bob}/entries/0", send: () => alice.get(`/v1/runs/${bob.waiting.id}/entries/0`) },
    { what: "GET /v1/runs/{bob}/journal", send: () => alice.get(`/v1/runs/${bob.waiting.id}/journal`) },
    { what: "GET /v1/runs/{bob}/journal?as=cassette", send: () => alice.get(`/v1/runs/${bob.waiting.id}/journal`, { query: { as: "cassette" } }) },
    { what: "GET /v1/runs/{bob}/events", send: () => alice.get(`/v1/runs/${bob.waiting.id}/events`) },
    { what: "GET /v1/scripts/{bob's script}", send: () => alice.get(`/v1/scripts/${bob.waiting.script}`) },
    { what: "GET /v1/payloads/{bob's input}?run={bob}", send: () => alice.get(`/v1/payloads/${bob.waiting.input.digest}`, { query: { run: bob.waiting.id } }) },
    { what: "POST /v1/runs/{bob}/signals/approval", send: () => alice.post(`/v1/runs/${bob.waiting.id}/signals/approval`, { ok: true }) },
    { what: "POST /v1/runs/{bob}/cancel", send: () => alice.post(`/v1/runs/${bob.waiting.id}/cancel`, { reason: "mallory" }) },
    { what: "POST /v1/runs/{bob}/fork", send: () => alice.post(`/v1/runs/${bob.waiting.id}/fork`, { script: waiting(unique("fork")) }) },
    { what: "POST /v1/runs/{bob}/release", send: () => alice.post(`/v1/runs/${bob.waiting.id}/release`) },
    { what: "POST /v1/runs/{bob}/give-up", send: () => alice.post(`/v1/runs/${bob.waiting.id}/give-up`) },
    { what: "GET /v1/namespaces/bob/secrets", send: () => alice.get("/v1/namespaces/bob/secrets") },
    { what: "PUT /v1/namespaces/bob/secrets/github", send: () => alice.put("/v1/namespaces/bob/secrets/github", { value: "mallory's" }) },
    { what: "DELETE /v1/runs/{bob}", send: () => alice.delete(`/v1/runs/${bob.waiting.id}`) },
  ];

  for (const request of onBobsRun) {
    test(`Alice's token: ${request.what} is 404, and Bob's run is unchanged`, { tag: "@attack" }, async () => {
      await expectRefused({ attempt: request.send, status: 404, slug: "not-found", probe: () => runState(tenant.api, bob.waiting.id) });
      evidence(`${request.what} with Alice's token → 404 not-found (not 403: Bob's run does not exist for her)`);
    });
  }

  test("Alice's token cannot approve or reject the task of Bob's run", { tag: "@attack" }, async () => {
    const task = bob.asking.waitingFor.find((w: any) => w.kind === "tool_call");
    expect(task?.taskId, JSON.stringify(bob.asking.waitingFor)).toBeTruthy();
    for (const action of ["approve", "reject"]) {
      await expectRefused({
        attempt: () => alice.post(`/v1/tasks/${task.taskId}/${action}`, {}),
        status: 404,
        slug: "not-found",
        probe: () => runState(tenant.api, bob.asking.id),
      });
    }
    evidence("POST /v1/tasks/{bob's task}/approve|reject with Alice's token → 404");
  });

  test("a header naming Bob with Alice's token is refused", { tag: "@attack" }, async () => {
    const astray = alice.ns("bob");
    await expectRefused({ attempt: () => astray.get("/v1/runs"), status: 403, slug: "forbidden" });
    await expectRefused({
      attempt: () => astray.post("/v1/runs", { script: short(unique("astray")) }),
      status: 403,
      slug: "forbidden",
      probe: () => namespaceState(tenant.api, "bob"),
    });
    // Her own namespace may be repeated.
    expectOk(await alice.ns("alice").get(`/v1/runs/${hers.waiting.id}`), 200);
    evidence("Rewind-Namespace: bob with Alice's token → 403 forbidden; nothing started in bob");
  });

  test("Alice's lists, statistics and scripts hold her namespace only", { tag: "@success" }, async () => {
    const page = await eventually("Alice's runs are listed", async () => {
      const reply = await alice.get("/v1/runs");
      const ids = (reply.json?.runs ?? []).map((r: any) => r.id);
      return ids.includes(hers.waiting.id) && ids.includes(hers.asking.id) ? reply.json : undefined;
    });
    expect(page.runs.every((r: any) => r.namespace === "alice")).toBe(true);
    const bobs = [bob.waiting.id, bob.asking.id, bob.ticket.id];
    expect(page.runs.map((r: any) => r.id).filter((id: string) => bobs.includes(id))).toEqual([]);
    const searched = await alice.get("/v1/runs", { query: { q: `namespace:bob` } });
    expect(searched.status).toBeLessThan(500);
    if (searched.status === 200) expect(searched.json.runs.map((r: any) => r.id).filter((id: string) => bobs.includes(id))).toEqual([]);
    const stats = await alice.get("/v1/runs/stats");
    expectOk(stats, 200);
    // Her statistics are her namespace's counters: another namespace's are refused.
    for (const path of ["/v1/runs/stats", "/v1/runs/failures", "/v1/scripts", "/v1/tools/stats"]) {
      expectProblem(await alice.get(path, { query: { q: "namespace:bob" } }), 403, "forbidden");
    }
    const scripts = await alice.get("/v1/scripts");
    expectOk(scripts, 200);
    const digests = scripts.json.scripts.map((s: any) => s.digest);
    expect(digests).not.toContain(bob.waiting.script);
    expect(digests).not.toContain(bob.asking.script);
    const failures = await alice.get("/v1/runs/failures");
    expectOk(failures, 200);
    expect(JSON.stringify(failures.json)).not.toContain(bob.waiting.id);
  });

  test("Alice's worker token is handed her tasks only, and cannot answer Bob's", { tag: "@attack" }, async () => {
    const query = { toolset: asking, tools: ["support.ask"], slots: 10 };
    // Her stream is limited to her namespace: naming Bob's is refused.
    const named = await TaskStream.attempt(aliceWorker, { ...query, namespace: "bob" });
    expect(named instanceof TaskStream, "a stream of Bob's namespace for Alice's token").toBe(false);
    expectProblem(named as Reply, 403, "forbidden");
    const hersStream = await TaskStream.open(aliceWorker, query);
    // The customer's worker is handed Bob's; Alice's token cannot answer it.
    const customerWorker = tenant.api.as((await tenant.key("worker")).key);
    const theirs = await TaskStream.open(customerWorker, query);
    try {
      const mine = await hersStream.task("Alice's task", (t) => t.runId === hers.asking.id);
      expect(mine.namespace).toBe("alice");
      const bobs = await theirs.task("Bob's task", (t) => t.runId === bob.asking.id);
      expect(hersStream.handed.map((t) => t.runId), "Alice's stream is handed nothing of Bob's").toEqual([hers.asking.id]);
      for (const [path, body] of [
        [`/v1/tasks/${bobs.token}/complete`, { result: "answered by mallory" }],
        [`/v1/tasks/${bobs.token}/fail`, { message: "failed by mallory" }],
        [`/v1/tasks/${bobs.token}/defer`, {}],
      ] as const) {
        await expectRefused({
          attempt: () => aliceWorker.post(path, body),
          status: 410,
          slug: "task-gone",
          probe: () => runState(tenant.api, bob.asking.id),
        });
      }
      // Her own answer reaches her run.
      expectOk(await aliceWorker.post(`/v1/tasks/${mine.token}/complete`, { result: "yes" }));
      const ended = await tenant.api.get(`/v1/runs/${hers.asking.id}`, { query: { wait: "20s" } });
      expect(ended.json.status).toBe("completed");
      expect(ended.json.output.value).toBe("yes");
    } finally {
      await Promise.all([hersStream.close(), theirs.close()]);
    }
    evidence("Alice's tasks:work token: a stream of bob → 403; her stream is handed only her run's task; complete/fail/defer of Bob's task token → 410 task-gone");
  });

  test("payloads are read only through Alice's own runs", { tag: "@attack" }, async () => {
    const herInput = hers.waiting.input.digest;
    const hisInput = bob.waiting.input.digest;
    expectProblem(await alice.get(`/v1/payloads/${herInput}`), 403, "forbidden");
    expect(hers.waiting.input.value, "a value above 256 KiB is left out of the run").toBeUndefined();
    const read = await alice.get(`/v1/payloads/${herInput}`, { query: { run: hers.waiting.id } });
    expectOk(read, 200);
    expect(JSON.stringify(read.json)).toContain(hers.waiting.input.preview.slice(0, 20));
    await expectRefused({ attempt: () => alice.get(`/v1/payloads/${hisInput}`, { query: { run: hers.waiting.id } }), status: 404, slug: "not-found" });
    await expectRefused({ attempt: () => alice.get(`/v1/payloads/${hisInput}`, { query: { run: bob.waiting.id } }), status: 404, slug: "not-found" });
    evidence("a token must name its run (403 without), a payload its run does not hold → 404, a run of Bob's → 404");
  });

  test("a script Alice runs cannot signal Bob's run, and the runs it starts are hers", { tag: "@attack" }, async () => {
    const script = `import { ticket } from "rewind:workflows";

export default async function main(input: { target: string }) {
  let signalled = "no";
  try {
    await ticket.get(input.target).signal("approve", { ok: true });
    signalled = "yes";
  } catch (error) {
    signalled = \`refused: \${String(error)}\`;
  }
  const child = await ticket.start({ id: "alice-child" }, { detached: true });
  return { signalled, child: child.id };
}
`;
    const before = await runState(tenant.api, bob.ticket.id);
    const run = await start(alice, { script, toolset: tickets, input: { target: bob.ticket.id }, wait: "30s" });
    expect(run.status, JSON.stringify(run)).toBe("completed");
    expect(String(run.output.value.signalled)).toMatch(/^refused:/);
    expect(await runState(tenant.api, bob.ticket.id)).toEqual(before);
    const child = await tenant.api.get(`/v1/runs/${run.output.value.child}`);
    expectOk(child, 200);
    expect(child.json.namespace).toBe("alice");
    evidence(`ticket.get(bob's run).signal() in Alice's script → ${run.output.value.signalled}; the run it started is in alice`);
  });
});

// =============================================================================================
// Quotas: a tenant per check, so that limits never leak into another

test.describe("quotas", () => {
  test("maxScriptLines refuses a longer script at start, fork, check and test; an exception lifts it", { tag: "@failure" }, async () => {
    const tenant = await newTenant("ns-lines");
    expectOk(await tenant.api.put("/v1/organization/namespace-defaults", { defaults: { maxScriptLines: 3 } }));
    const alice = tenant.api.ns("alice");
    const long = waiting(unique("long"));
    const tooLong = (reply: import("../support/gate.js").Reply) => {
      expectProblem(reply, 422, "invalid-script");
      expect(reply.json.diagnostics?.[0]?.code).toBe("rewind/max-script-lines");
    };
    tooLong(await alice.post("/v1/runs", { script: long }));
    tooLong(await alice.post("/v1/check", { script: long }));
    const strict = await newTenant("ns-lines", stacks().b);
    expectOk(await strict.api.put("/v1/organization/namespace-defaults", { defaults: { maxScriptLines: 3 } }));
    tooLong(await strict.api.ns("alice").post("/v1/test", { script: long }));
    const ok = await start(alice, { script: short(unique("ok")), wait: "20s" });
    tooLong(await alice.post(`/v1/runs/${ok.id}/fork`, { script: long }));
    expect(await namespaceState(tenant.api, "alice")).toMatchObject({ runsStarted: 1 });
    expectOk(await tenant.api.put("/v1/namespaces/vip", { exceptions: { maxScriptLines: 100 } }), 200);
    await start(tenant.api.ns("vip"), { script: long });
  });

  test("maxTtl refuses a longer TTL with 400, and lowers the default TTL", { tag: "@failure" }, async () => {
    const tenant = await newTenant("ns-ttl");
    expectOk(await tenant.api.put("/v1/namespaces/alice", { exceptions: { maxTtl: "1h" } }), 200);
    const alice = tenant.api.ns("alice");
    await expectRefused({
      attempt: () => alice.post("/v1/runs", { script: short(unique("ttl")), ttl: "2h" }),
      status: 400,
      slug: "invalid-request",
      probe: () => namespaceState(tenant.api, "alice"),
    });
    const run = await start(alice, { script: short(unique("ttl")), wait: "20s" });
    expect(Date.parse(run.expiresAt) - Date.parse(run.createdAt)).toBeLessThanOrEqual(3_600_000 + 1000);
    expectProblem(await alice.post(`/v1/runs/${run.id}/fork`, { script: short(unique("fork")), ttl: "2h" }), 400, "invalid-request");
  });

  test("runsPerMonth refuses with 429, the quota's fields and Retry-After, never naming the namespace", { tag: "@failure" }, async () => {
    const tenant = await newTenant("ns-monthly");
    const namespace = unique("monthly-");
    expectOk(await tenant.api.put(`/v1/namespaces/${namespace}`, { exceptions: { runsPerMonth: 1 } }), 200);
    const api = tenant.api.ns(namespace);
    const first = await api.post("/v1/runs", { script: short(unique("m")), wait: "20s" }, { headers: { "idempotency-key": "first" } });
    expectOk(first, 201);
    const refused = await expectRefused({
      attempt: () => api.post("/v1/runs", { script: short(unique("m")) }),
      status: 429,
      slug: "quota-exceeded",
      probe: () => namespaceState(tenant.api, namespace),
    });
    expect(refused.json).toMatchObject({ quota: "runs_per_month", scope: "namespace", limit: 1, used: 1 });
    expect(Date.parse(refused.json.resetsAt)).toBeGreaterThan(Date.now());
    const retryAfter = Number(refused.headers.get("retry-after"));
    expect(retryAfter).toBeGreaterThan(0);
    expect(retryAfter).toBeLessThanOrEqual(31 * 86_400);
    expect(refused.json.detail).not.toContain(namespace);
    // The first request, retried with its key, still returns its run.
    const retried = await api.post("/v1/runs", { script: short(unique("m")) }, { headers: { "idempotency-key": "first" } });
    expectOk(retried);
    expect(retried.json.id).toBe(first.json.id);
    evidence(`429 quota-exceeded runs_per_month, Retry-After ${retryAfter}s; the retry with its key returns ${first.json.id}`);
  });

  test("activeRuns refuses a start past the limit; a retry with its key still returns its run; the refusal raises an alert", { tag: "@failure" }, async () => {
    const tenant = await newTenant("ns-active");
    const namespace = unique("busy-");
    expectOk(await tenant.api.put(`/v1/namespaces/${namespace}`, { exceptions: { activeRuns: 1 } }), 200);
    const api = tenant.api.ns(namespace);
    const first = await api.post("/v1/runs", { script: waiting(unique("a")) }, { headers: { "idempotency-key": "k1" } });
    expectOk(first, 201);
    await untilWaiting(tenant.api, first.json.id);
    const refused = await expectRefused({
      attempt: () => api.post("/v1/runs", { script: waiting(unique("a")) }, { headers: { "idempotency-key": "k2" } }),
      status: 429,
      slug: "quota-exceeded",
      probe: () => namespaceState(tenant.api, namespace),
    });
    expect(refused.json).toMatchObject({ quota: "active_runs", scope: "namespace", limit: 1, used: 1 });
    expect(refused.json.resetsAt ?? null).toBeNull();
    const retried = await api.post("/v1/runs", { script: waiting(unique("a")) }, { headers: { "idempotency-key": "k1" } });
    expectOk(retried);
    expect(retried.json.id).toBe(first.json.id);
    // A fork is a start too.
    expectProblem(await tenant.api.post(`/v1/runs/${first.json.id}/fork`, { script: waiting(unique("f")) }), 429, "quota-exceeded");
    const alerts = await eventually("the refusal raised an alert", async () => {
      const reply = await tenant.api.get("/v1/organization/quota-alerts", { query: { namespace } });
      return reply.json?.alerts?.length > 0 ? reply.json.alerts : undefined;
    });
    expect(alerts[0]).toMatchObject({ quota: "active_runs", threshold: 100, namespace });
  });

  test("scripts refuses a script new to the namespace", { tag: "@failure" }, async () => {
    const tenant = await newTenant("ns-scripts");
    const namespace = unique("one-script-");
    expectOk(await tenant.api.put(`/v1/namespaces/${namespace}`, { exceptions: { scripts: 1 } }), 200);
    const api = tenant.api.ns(namespace);
    const script = short(unique("only"));
    await start(api, { script, wait: "20s" });
    await start(api, { script, wait: "20s" });
    const refused = await expectRefused({
      attempt: () => api.post("/v1/runs", { script: short(unique("another")) }),
      status: 429,
      slug: "quota-exceeded",
      probe: () => namespaceState(tenant.api, namespace),
    });
    expect(refused.json).toMatchObject({ quota: "scripts", scope: "namespace", limit: 1 });
  });

  test("the operator's ceiling bounds the tenant's totals across its namespaces", { tag: "@failure" }, async () => {
    const tenant = await newTenant("ns-ceiling");
    expectOk(await operator().put(`/v1/organizations/${tenant.id}/quotas`, { ceiling: { activeRuns: 1 } }), 204);
    await start(tenant.api.ns("first"), { script: waiting(unique("c")) });
    const runs = async () => (await namespaceState(tenant.api, "second")) as { runsStarted?: number; activeRuns?: number };
    const refused = await expectRefused({
      attempt: () => tenant.api.ns("second").post("/v1/runs", { script: waiting(unique("c")) }),
      status: 429,
      slug: "quota-exceeded",
      probe: async () => {
        const state = await runs();
        return { runsStarted: state.runsStarted ?? 0, activeRuns: state.activeRuns ?? 0 };
      },
    });
    expect(refused.json).toMatchObject({ quota: "active_runs", scope: "organization", limit: 1 });
    const defaults = await tenant.api.get("/v1/organization/namespace-defaults");
    expectOk(defaults, 200);
    expect(defaults.json.ceiling.activeRuns).toBe(1);
    // The customer cannot raise its own ceiling.
    expectProblem(await tenant.api.put(`/v1/organizations/${tenant.id}/quotas`, { ceiling: { activeRuns: 1000 } }), 403, "forbidden");
  });

  test("a start refused by a quota is a first use: the namespace exists, with no run", { tag: "@failure" }, async () => {
    // archi/08 (namespaces): `create_run` creates the namespace even when the run is then
    // refused, since a refused start is a first use and the refusal's alert belongs to it.
    const tenant = await newTenant("ns-refused");
    expectOk(await operator().put(`/v1/organizations/${tenant.id}/quotas`, { ceiling: { activeRuns: 0 } }), 204);
    const namespace = unique("refused-");
    const refused = await tenant.api.ns(namespace).post("/v1/runs", { script: short(unique("never")) });
    expectProblem(refused, 429, "quota-exceeded");
    expect(refused.json.scope).toBe("organization");
    expectOk(await tenant.api.get(`/v1/namespaces/${namespace}`), 200);
    const runs = await tenant.api.ns(namespace).get("/v1/runs");
    expectOk(runs, 200);
    expect(runs.json.runs).toEqual([]);
    evidence("a start refused by the tenant's ceiling: 429 at scope organization; the namespace exists, with no run (08)");
  });

  test("crossing 80 % and 100 % of runsPerMonth raises alerts, listed per namespace and month", { tag: "@success" }, async () => {
    const tenant = await newTenant("ns-alerts");
    const namespace = unique("team:busy-");
    expectOk(await tenant.api.put(`/v1/namespaces/${namespace}`, { exceptions: { runsPerMonth: 2 } }), 200);
    for (const marker of ["one", "two"]) await start(tenant.api.ns(namespace), { script: short(unique(marker)) });
    await start(tenant.api.ns("idle"), { script: short(unique("idle")) });
    const detail = await eventually("the namespace has its alerts", async () => {
      const reply = await tenant.api.get(`/v1/namespaces/${encodeURIComponent(namespace)}`);
      return reply.json?.alerts?.length >= 2 ? reply.json : undefined;
    });
    expect(detail.alerts.map((a: any) => a.threshold).sort()).toEqual([100, 80]);
    expect(detail.namespace.usage.runsStarted).toBe(2);
    const listed = await tenant.api.get("/v1/organization/quota-alerts", { query: { namespace } });
    expect(listed.json.alerts).toHaveLength(2);
    expect(listed.json.alerts.every((a: any) => a.quota === "runs_per_month" && a.namespace === namespace)).toBe(true);
    const idle = await tenant.api.get("/v1/organization/quota-alerts", { query: { namespace: "idle" } });
    expect(idle.json.alerts).toEqual([]);
    const above = await tenant.api.get("/v1/namespaces", { query: { above: 0.8 } });
    expect(above.json.namespaces.map((n: any) => n.name)).toEqual([namespace]);
    expect(above.json.total).toBe(1);
  });

  test("defaults and exceptions combine: an exception wins, and leaving it out gives the default back", { tag: "@success" }, async () => {
    const tenant = await newTenant("ns-defaults");
    expectOk(await tenant.api.put("/v1/organization/namespace-defaults", { defaults: { runsPerMonth: 1000 } }));
    const set = await tenant.api.put("/v1/namespaces/vip", { exceptions: { runsPerMonth: 2 } });
    expectOk(set, 200);
    expect(set.json.namespace.limits.runsPerMonth).toBe(2);
    const reset = await tenant.api.put("/v1/namespaces/vip", {});
    expectOk(reset, 200);
    expect(reset.json.namespace.exceptions.runsPerMonth ?? null).toBeNull();
    expect(reset.json.namespace.limits.runsPerMonth).toBe(1000);
    const reader = tenant.api.as((await tenant.key("reader")).key);
    expectProblem(await reader.put("/v1/namespaces/vip", { exceptions: { activeRuns: 1000 } }), 403, "forbidden");
    expectProblem(await reader.put("/v1/organization/namespace-defaults", { defaults: {} }), 403, "forbidden");
  });
});

// =============================================================================================
// Secrets per end user

test.describe("secrets per namespace", () => {
  /** An MCP server whose calls carry the secret attached for it in `Authorization`. */
  const keyed = (url: string) => ({ name: "fx", url, auth: { type: "secret", header: "authorization", format: "Bearer {secret}" } });
  const ECHO = `import { fx } from "rewind:tools";\n\nexport default async function main() {\n  return (await fx.echo({ text: "hello" })).text();\n}\n`;

  test("an end user's secret reaches the calls of their own runs only, and never comes back", { tag: "@attack" }, async () => {
    const tenant = await newTenant("ns-secrets");
    const mcp = await startMcpFixture();
    try {
      const toolset = await tenant.api.post("/v1/toolsets", { mcp: [keyed(mcp.url)] });
      expectOk(toolset);
      const id = toolset.json.id;
      const hers = `${canary()}-alice`;
      const organization = `${canary()}-organization`;
      expectOk(await tenant.api.put("/v1/secrets/fx", { value: organization }), 201);
      // Alice's token puts her own secret, which only her runs attach.
      const aliceToken = tenant.api.as((await mint(tenant.api, "alice", { permissions: ["runs:start", "runs:read", "secrets:read", "secrets:write"] })).token);
      const put = await aliceToken.put("/v1/namespaces/alice/secrets/fx", { value: hers });
      expectOk(put, 201);
      expect(put.json).toMatchObject({ reference: "ns/fx", namespace: "alice", createdBy: "token:alice" });
      const run = await start(aliceToken, { script: ECHO, toolset: id, secrets: { fx: "ns/fx" }, wait: "30s" });
      expect(run.status, JSON.stringify(run)).toBe("completed");
      expect(run.secrets).toEqual({ fx: "ns/fx" });
      const calls = mcp.calls.filter((c) => c.tool === "echo");
      expect(calls.map((c) => c.headers.authorization)).toEqual([`Bearer ${hers}`]);
      expect(calls[0]!.meta?.["codes.rewind/idempotency-key"]).toBe(`${run.id}/1`);

      // Bob has none: his run attaching ns/fx is refused before it exists, and never falls back
      // on the organization's fx nor on Alice's.
      const started = (namespace: string) => async () => ((await namespaceState(tenant.api, namespace)) as { runsStarted?: number }).runsStarted ?? 0;
      const refused = await expectRefused({
        attempt: () => tenant.api.ns("bob").post("/v1/runs", { script: ECHO, toolset: id, secrets: { fx: "ns/fx" } }),
        status: 422,
        slug: "secret-required",
        probe: started("bob"),
      });
      expect(refused.json.attachments[0]).toMatchObject({ server: "fx", problem: "unknown", secret: "ns/fx" });
      expect(refused.json.attachments[0].attachable).toEqual(["org/fx"]);
      // Nothing attached at all: refused too.
      const missing = await expectRefused({
        attempt: () => tenant.api.ns("alice").post("/v1/runs", { script: ECHO, toolset: id }),
        status: 422,
        slug: "secret-required",
        probe: started("alice"),
      });
      expect(missing.json.attachments[0]).toMatchObject({ server: "fx", problem: "missing" });
      expect(mcp.calls.filter((c) => c.tool === "echo")).toHaveLength(1);

      // Alice's token reaches her namespace's secrets only.
      await expectRefused({ attempt: () => aliceToken.get("/v1/namespaces/bob/secrets"), status: 404, slug: "not-found" });
      await expectRefused({ attempt: () => aliceToken.put("/v1/namespaces/bob/secrets/fx", { value: "mallory" }), status: 404, slug: "not-found" });
      await expectRefused({ attempt: () => aliceToken.get("/v1/secrets"), status: 403, slug: "forbidden" });
      await expectRefused({ attempt: () => aliceToken.put("/v1/secrets/fx", { value: "mallory" }), status: 403, slug: "forbidden" });

      // Nothing Rewind answers holds a value: the recorder flags any answer with the canary.
      for (const path of [
        `/v1/runs/${run.id}`,
        `/v1/runs/${run.id}/journal`,
        `/v1/runs/${run.id}/journal?as=cassette`,
        `/v1/runs/${run.id}/trace?format=llm`,
        `/v1/runs/${run.id}/entries/1`,
        `/v1/runs/${run.id}/events`,
        "/v1/namespaces/alice",
        "/v1/namespaces/alice/secrets",
        "/v1/namespaces/alice/secrets/fx/usage",
        "/v1/secrets",
        "/v1/secrets/fx/usage",
        "/v1/toolsets",
      ]) {
        const reply = await tenant.api.get(path);
        expect(reply.status, `${path}: ${reply.text.slice(0, 300)}`).toBe(200);
        expect(reply.text.includes(canary()), `${path} holds a secret's value`).toBe(false);
      }
      const usage = await tenant.api.get("/v1/namespaces/alice/secrets/fx/usage");
      expect(usage.json.secret.lastRead).toMatchObject({ run: run.id, namespace: "alice" });
      evidence(`Alice's secret reached her MCP call only (Bearer <canary>-alice, key ${run.id}/1); Bob's run attaching ns/fx → 422 secret-required (unknown, attachable org/fx), no fallback; Alice's token on Bob's secrets → 404, on the organization's → 403; no answer holds a value`);
    } finally {
      await mcp.close();
    }
  });

  test("a token attaches its own namespace's secrets, and the organization's only with secrets:use", { tag: "@attack" }, async () => {
    const tenant = await newTenant("ns-secrets-use");
    const mcp = await startMcpFixture();
    try {
      const toolset = await tenant.api.post("/v1/toolsets", { mcp: [keyed(mcp.url)] });
      expectOk(toolset);
      const id = toolset.json.id;
      expectOk(await tenant.api.put("/v1/secrets/fx", { value: `${canary()}-org` }), 201);
      expectOk(await tenant.api.put("/v1/namespaces/carol/secrets/fx", { value: `${canary()}-carol` }), 201);
      const plain = tenant.api.as((await mint(tenant.api, "carol")).token);
      const runs = async () => ((await namespaceState(tenant.api, "carol")) as { runsStarted?: number }).runsStarted ?? 0;
      const refused = await expectRefused({
        attempt: () => plain.post("/v1/runs", { script: ECHO, toolset: id, secrets: { fx: "org/fx" } }),
        status: 422,
        slug: "secret-required",
        probe: runs,
      });
      expect(refused.json.attachments[0]).toMatchObject({ server: "fx", problem: "forbidden", secret: "org/fx" });
      expect(refused.json.attachments[0].attachable).toEqual(["ns/fx"]);
      const own = await start(plain, { script: ECHO, toolset: id, secrets: { fx: "ns/fx" }, wait: "30s" });
      expect(own.status, JSON.stringify(own)).toBe("completed");
      const using = tenant.api.as((await mint(tenant.api, "carol", { permissions: ["runs:start", "runs:read", "secrets:use"] })).token);
      const shared = await start(using, { script: ECHO, toolset: id, secrets: { fx: "org/fx" }, wait: "30s" });
      expect(shared.status, JSON.stringify(shared)).toBe("completed");
      expect(mcp.calls.filter((c) => c.tool === "echo").map((c) => c.headers.authorization)).toEqual([
        `Bearer ${canary()}-carol`,
        `Bearer ${canary()}-org`,
      ]);
      evidence("a token without secrets:use: org/fx → 422 secret-required (forbidden, attachable ns/fx), ns/fx → completed; with secrets:use, org/fx → completed");
    } finally {
      await mcp.close();
    }
  });

  test("the dashboard lists the secrets and what reads them, and never shows a value", { tag: "@attack" }, async ({ page }) => {
    const tenant = await newTenant("ns-secrets-dashboard");
    const mcp = await startMcpFixture();
    try {
      const toolset = await tenant.api.post("/v1/toolsets", { mcp: [keyed(mcp.url)] });
      expectOk(toolset);
      expectOk(await tenant.api.put("/v1/secrets/shared-fx", { value: `${canary()}-dashboard-org`, labels: { env: "prod" } }), 201);
      expectOk(await tenant.api.put("/v1/namespaces/alice/secrets/fx", { value: `${canary()}-dashboard-alice` }), 201);
      const run = await start(tenant.api.ns("alice"), { script: ECHO, toolset: toolset.json.id, secrets: { fx: "ns/fx" }, wait: "30s" });
      expect(run.status, JSON.stringify(run)).toBe("completed");

      await page.goto(`${stacks().a.url}/`);
      await page.getByPlaceholder("rewind_sk_…").fill(tenant.ownerKey);
      await page.getByRole("button", { name: "Continue" }).click();
      const views = page.getByRole("navigation", { name: "Views" });
      await views.getByRole("link", { name: "Secrets" }).click();
      await expect(page.getByText("org/shared-fx")).toBeVisible();
      await expect(page.getByText("prod").first()).toBeVisible();
      // A new value is typed in a password field, never offered back.
      await page.getByRole("button", { name: /new secret/i }).click();
      const form = page.getByRole("form", { name: "New secret" });
      await expect(form.locator('input[type="password"]')).toHaveCount(1);
      await page.keyboard.press("Escape");
      const secretsView = await page.content();
      expect(secretsView.includes(canary()), "the Secrets view shows a value").toBe(false);

      // Alice's page: her secret, read by her run.
      await page.goto(`${stacks().a.url}/namespaces/alice`);
      await expect(page.getByText("ns/fx")).toBeVisible();
      const hers = await page.content();
      expect(hers.includes(canary()), "a namespace's page shows a value").toBe(false);
      // The run's page names what it attached, never a value.
      await page.goto(`${stacks().a.url}/runs/${run.id}`);
      await expect(page.getByText("ns/fx").first()).toBeVisible();
      expect((await page.content()).includes(canary()), "a run's page shows a value").toBe(false);
      evidence("signed in with the owner key: Secrets lists org/shared-fx (env=prod), its value typed in a password field; Alice's page lists ns/fx; the run's page names ns/fx; no page holds a value");
    } finally {
      await mcp.close();
    }
  });
});

// =============================================================================================
// Erasing an end user

test.describe("erasure", () => {
  test("erasing a namespace answers 202, cancels its runs, revokes its tokens, and takes nothing more", { tag: "@success" }, async () => {
    const tenant = await newTenant("ns-erase");
    const namespace = unique("leaving-");
    const api = tenant.api.ns(namespace);
    const run = await start(api, { script: waiting(unique("leaving")) });
    await untilWaiting(tenant.api, run.id);
    const token = await mint(tenant.api, namespace);
    expectOk(await tenant.api.as(token.token).get("/v1/whoami"), 200);
    expectOk(await tenant.api.put(`/v1/namespaces/${namespace}/secrets/github`, { value: `${canary()}-leaving` }), 201);

    expectOk(await tenant.api.delete(`/v1/namespaces/${namespace}`), 202);
    const cancelled = await tenant.api.get(`/v1/runs/${run.id}`, { query: { wait: "20s" } });
    expect(cancelled.json?.status ?? "erased", cancelled.text).toMatch(/^(cancelled|erased)$/);
    expectProblem(await tenant.api.as(token.token).get("/v1/whoami"), 401, "unauthorized");
    expectProblem(await api.post("/v1/runs", { script: short(unique("again")) }), 409, "namespace-deleting");
    expectProblem(await tenant.api.post(`/v1/namespaces/${namespace}/tokens`, {}), 409, "namespace-deleting");
    expectProblem(await tenant.api.put(`/v1/namespaces/${namespace}`, {}), 409, "namespace-deleting");
    // Its runs and secrets go with it, in the background: the lifecycle stage
    // proves nothing is left. Meanwhile none is written.
    expectProblem(await tenant.api.put(`/v1/namespaces/${namespace}/secrets/github`, { value: "again" }), 409, "namespace-deleting");
    const detail = await tenant.api.get(`/v1/namespaces/${namespace}`);
    if (detail.status === 200) expect(detail.json.namespace.deletingSince).toBeTruthy();
    else expectProblem(detail, 404, "not-found");
    expect([202, 404]).toContain((await tenant.api.delete(`/v1/namespaces/${namespace}`)).status);
    expectProblem(await tenant.api.delete(`/v1/namespaces/${unique("nobody-")}`), 404, "not-found");
    evidence("DELETE /v1/namespaces/{name} → 202; its run cancelled, its token 401, starts, tokens and secrets 409 namespace-deleting");
  });
});
