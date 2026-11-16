/**
 * Area "API hardening" of the release gate, on setups A (deployed, Postgres) and B (strict,
 * Turso): what a careless or hostile caller sends to the HTTP API, and the answer
 * `api/README.md` gives. Every refusal is a problem with its slug, never a 5xx, and changes
 * nothing.
 *
 * - Bodies: above the server's limit (413), not JSON (415), invalid or deeply nested JSON (400).
 * - Paths and names: ids with path tricks, NUL, unicode or 10 kB; namespace names outside their
 *   alphabet (400).
 * - Queries: SQL and the wildcards of `LIKE` in labels and `q` match only what they say.
 * - Headers: an idempotency key reused with another body, repeated singleton headers, a
 *   cookie instead of a Bearer, oversized values.
 * - Routes: `/hooks` guessed or replayed, `/auth` without WorkOS, the operator's endpoints.
 */
import { randomBytes } from "node:crypto";
import { eventually } from "../../support/processes.js";
import { evidence, expect, expectHealthy, expectOk, expectProblem, expectRefused, newTenant, type Reply, stacks, type Tenant, test, unique } from "../support/gate.js";
import { expectNo5xx, namespaceState, rawRequest, runState, short, start, untilWaiting, waiting } from "../support/namespaces-helpers.js";
import type { StackInfo } from "../support/stack.js";

const SETUPS: { name: string; stack: () => StackInfo }[] = [
  { name: "A, deployed", stack: () => stacks().a },
  { name: "B, strict", stack: () => stacks().b },
];

/** A script that hands out a webhook URL in its logs, then waits for one signal through it. */
const HOOKED = (marker: string) => `// ${marker}
import { signal, webhook } from "rewind";

export default async function main() {
  const hook = await webhook("ping");
  console.log(\`hook: \${hook.url}\`);
  const event = await signal<{ n: number }>("ping");
  return { got: event };
}
`;

/** The webhook URL a run of `HOOKED` logged. */
async function hookOf(tenant: Tenant, id: string): Promise<string> {
  return eventually(`run ${id} logs its webhook`, async () => {
    const reply = await tenant.api.get(`/v1/runs/${id}`);
    const line = (reply.json?.logs ?? []).map((l: any) => String(l.message)).find((m: string) => m.startsWith("hook: "));
    return line ? line.slice("hook: ".length).trim() : undefined;
  });
}

for (const setup of SETUPS) {
  test.describe(`setup ${setup.name}`, () => {
    let tenant: Tenant;
    let stack: StackInfo;

    test.beforeAll(async () => {
      stack = setup.stack();
      tenant = await newTenant(`hardening-${setup.name[0]}`, stack);
    });

    // -----------------------------------------------------------------------------------------
    // Bodies

    test("a body above the server's limit is refused with 413", { tag: "@attack" }, async () => {
      const body = JSON.stringify({ script: short("big"), input: "a".repeat(17 * 1024 * 1024) });
      const namespace = unique("big-");
      const before = await namespaceState(tenant.api, namespace);
      // Sent with Expect: 100-continue: the server refuses from Content-Length, without reading
      // a body it would drop (a plain upload races its early answer into a reset connection).
      const reply = await rawRequest(
        stack.url,
        "POST",
        "/v1/runs",
        [
          ["authorization", `Bearer ${tenant.ownerKey}`],
          ["rewind-namespace", namespace],
          ["content-type", "application/json"],
        ],
        body,
        { expectContinue: true },
      );
      expect(reply.status, reply.text).toBe(413);
      expect(String(reply.headers["content-type"])).toContain("application/problem+json");
      expect(reply.json?.type).toBe("urn:rewind:problem:payload-too-large");
      expect(await namespaceState(tenant.api, namespace)).toEqual(before);
      evidence("a 17 MiB body → 413 payload-too-large from its Content-Length; no run");
    });

    test("an input above the payload limit is refused with 413, and so is a signal's payload", { tag: "@attack" }, async () => {
      const namespace = unique("input-");
      await expectRefused({
        attempt: () => tenant.api.ns(namespace).post("/v1/runs", { script: short("input"), input: "a".repeat(9 * 1024 * 1024) }),
        status: 413,
        slug: "payload-too-large",
        probe: () => namespaceState(tenant.api, namespace),
      });
      const run = await start(tenant.api.ns("alice"), { script: waiting(unique("payload")) });
      await untilWaiting(tenant.api, run.id);
      await expectRefused({
        attempt: () => tenant.api.post(`/v1/runs/${run.id}/signals/approval`, { ok: true, blob: "a".repeat(9 * 1024 * 1024) }),
        status: 413,
        slug: "payload-too-large",
        probe: () => runState(tenant.api, run.id),
      });
      evidence("a 9 MiB input or signal payload → 413 payload-too-large, above the 8 MiB limit");
    });

    test("a body that is not JSON is refused with 415", { tag: "@failure" }, async () => {
      const json = JSON.stringify({ script: short("media") });
      for (const contentType of ["text/plain", "application/x-www-form-urlencoded", "multipart/form-data; boundary=x", "application/xml"]) {
        for (const [method, path] of [
          ["POST", "/v1/runs"],
          ["POST", "/v1/toolsets"],
          ["PUT", "/v1/namespaces/alice"],
        ] as const) {
          const reply = await tenant.api.ns("alice").request(method, path, { body: json, contentType });
          expectProblem(reply, 415, "unsupported-media-type");
        }
      }
    });

    test("invalid JSON and wrong shapes are refused with 400, never a 5xx", { tag: "@failure" }, async () => {
      const bodies = [
        '{"script":',
        '{"script": "x",}',
        '{"script": NaN}',
        "[1, 2, 3]",
        '"just a string"',
        "null",
        '{"script": 42}',
        '{"script": "export default async () => 1;", "unknown": true}',
        '{"script": "export default async () => 1;", "ttl": "forever"}',
        '{"script": "export default async () => 1;", "labels": {"a": 1}}',
        '\u0000\u0001\u0002',
      ];
      for (const body of bodies) {
        const reply = await tenant.api.ns("alice").request("POST", "/v1/runs", { body, contentType: "application/json" });
        expectNo5xx(reply, body);
        expect([400, 422], `${body} → ${reply.status} ${reply.text.slice(0, 200)}`).toContain(reply.status);
        expect(reply.slug, body).toMatch(/^(invalid-request|invalid-script)$/);
      }
    });

    test("deeply nested JSON is refused, and the server keeps serving", { tag: "@attack" }, async () => {
      const depth = 200_000;
      const nested = `${"[".repeat(depth)}${"]".repeat(depth)}`;
      const run = await start(tenant.api.ns("alice"), { script: waiting(unique("nested")) });
      await untilWaiting(tenant.api, run.id);
      for (const [path, body] of [
        ["/v1/runs", `{"script": "export default async () => 1;", "input": ${nested}}`],
        [`/v1/runs/${run.id}/signals/approval`, nested],
        ["/v1/toolsets", `{"host": [{"name": "deep", "description": "d", "inputSchema": ${nested}}]}`],
      ] as const) {
        const reply = await tenant.api.ns("alice").request("POST", path, { body, contentType: "application/json" });
        expectProblem(reply, 400, "invalid-request");
      }
      expect(await runState(tenant.api, run.id)).toMatchObject({ status: "active" });
      await expectHealthy(tenant);
      evidence(`JSON nested ${depth} deep → 400 invalid-request (recursion limit); a canary run then completes`);
    });

    // -----------------------------------------------------------------------------------------
    // Paths and names

    test("ids with path tricks, NUL, unicode or 10 kB are 404 or 400, never another resource", { tag: "@attack" }, async () => {
      const ids = [
        "..%2F..%2Fv1%2Fapi-keys",
        "%2e%2e",
        "run%00id",
        "%00",
        "r%C3%BCn",
        "%F0%9F%98%80",
        "a".repeat(10_240),
        "%27%20OR%20%271%27%3D%271",
        "00000000-0000-0000-0000-000000000000",
        "%25",
        "%5F",
      ];
      const templates = [
        (id: string) => ["GET", `/v1/runs/${id}`],
        (id: string) => ["GET", `/v1/runs/${id}/journal`],
        (id: string) => ["POST", `/v1/runs/${id}/signals/approval`],
        (id: string) => ["GET", `/v1/scripts/${id}`],
        (id: string) => ["GET", `/v1/payloads/${id}`],
        (id: string) => ["GET", `/v1/toolsets/${id}/declarations`],
        (id: string) => ["DELETE", `/v1/api-keys/${id}`],
        (id: string) => ["POST", `/v1/tasks/${id}/complete`],
        (id: string) => ["DELETE", `/v1/tokens/${id}`],
      ];
      const keys = await tenant.api.get("/v1/api-keys");
      for (const id of ids) {
        for (const template of templates) {
          const [method, path] = template(id);
          const reply = await tenant.api.request(method!, path!, method === "POST" ? { json: { ok: true } } : {});
          expectNo5xx(reply, `${method} ${path!.slice(0, 80)}`);
          expect([400, 404, 410], `${method} ${path!.slice(0, 80)} → ${reply.status} ${reply.text.slice(0, 200)}`).toContain(reply.status);
          expect(reply.headers.get("content-type") ?? "").toContain("problem+json");
        }
      }
      // Dot segments sent as they are reach no other route.
      for (const path of ["/v1/runs/../api-keys", "/v1/runs/x/../../api-keys", "/v1/runs/%2e%2e/%2e%2e/api-keys"]) {
        const reply = await rawRequest(stack.url, "GET", path, [["authorization", `Bearer ${tenant.ownerKey}`]]);
        expect(reply.status, `${path} → ${reply.status} ${reply.text.slice(0, 200)}`).toBe(404);
      }
      expect((await tenant.api.get("/v1/api-keys")).json).toEqual(keys.json);
      evidence(`${ids.length} hostile ids × ${templates.length} endpoints → 400/404/410 problems; dot segments → 404`);
    });

    test("namespace names outside their alphabet are refused with 400", { tag: "@failure" }, async () => {
      const before = (await tenant.api.get("/v1/namespaces")).json.total;
      for (const name of ["a b", "*", "n".repeat(129), "../etc", "a/b", "al,ice", "", "tab\tname", "a;b"]) {
        const reply = await tenant.api.request("POST", "/v1/runs", { json: { script: short("ns") }, headers: { "rewind-namespace": name } });
        expectProblem(reply, 400, "invalid-request");
      }
      for (const name of ["%00", "a%00b", "%E5%90%8D%E5%89%8D", "*", "n".repeat(129), "..%2Fetc", "a%20b", "%2A"]) {
        expectProblem(await tenant.api.put(`/v1/namespaces/${name}`, {}), 400, "invalid-request");
        expectProblem(await tenant.api.post(`/v1/namespaces/${name}/tokens`, {}), 400, "invalid-request");
      }
      expect((await tenant.api.get("/v1/namespaces")).json.total).toBe(before);
      // The longest name and every character of the alphabet are accepted.
      const longest = "n".repeat(128);
      for (const name of [longest, "user@example.net", "team:a.b_c-D9"]) {
        const run = await start(tenant.api.ns(name), { script: short(unique("ok")), wait: "20s" });
        expect(run.namespace).toBe(name);
      }
    });

    // -----------------------------------------------------------------------------------------
    // Queries

    test("label filters and queries with SQL and wildcards match only what they say", { tag: "@attack" }, async () => {
      const own = await newTenant(`hardening-query-${setup.name[0]}`, stack);
      const suite = unique("q");
      const values = ["a_c", "abc", "a%c", "a'c", "a\\c"];
      const ids: Record<string, string> = {};
      for (const value of values) {
        const run = await start(own.api.ns("alice"), { script: short(unique("label")), labels: { suite, team: value }, wait: "20s" });
        ids[value] = run.id;
      }
      await eventually("the read model has the runs", async () => {
        const reply = await own.api.get("/v1/runs", { query: { label: `suite:${suite}` } });
        return reply.json?.runs?.length === values.length ? true : undefined;
      });
      const listed = async (query: Record<string, string | string[]>) => {
        const reply = await own.api.get("/v1/runs", { query });
        expectNo5xx(reply, JSON.stringify(query));
        return reply;
      };
      for (const value of values) {
        const reply = await listed({ label: [`suite:${suite}`, `team:${value}`] });
        expectOk(reply, 200);
        expect(reply.json.runs.map((r: any) => r.id), `label team:${value}`).toEqual([ids[value]]);
      }
      for (const pattern of ["%", "_", "a_", "a%", "%c", "_bc", "*"]) {
        const reply = await listed({ label: [`suite:${suite}`, `team:${pattern}`] });
        expectOk(reply, 200);
        expect(reply.json.runs, `label team:${pattern} matches nothing`).toEqual([]);
      }
      for (const q of [`@team:"a' OR '1'='1"`, `@team:"%"`, `@team:"_"`, `@team:"a_c" OR 1=1`, `@team:"x'); DROP TABLE runs; --"`, `tool:"%"`]) {
        const reply = await listed({ q: `@suite:${suite} ${q}` });
        expect([200, 400], `${q} → ${reply.status} ${reply.text.slice(0, 200)}`).toContain(reply.status);
        if (reply.status === 200) {
          expect(reply.json.runs.length, `q ${q} matches no more than it says`).toBeLessThanOrEqual(1);
        } else {
          expect(reply.slug).toBe("invalid-query");
        }
      }
      // The runs are all still there.
      expect((await listed({ label: `suite:${suite}` })).json.runs).toHaveLength(values.length);
      evidence("labels with _ % ' \\ match exactly one run each; % and _ as values match none; SQL in q matches nothing more");
    });

    test("an unreadable query is refused with the position of the term at fault", { tag: "@failure" }, async () => {
      for (const q of ["status:", "@:", "status:sideways", "@team", "\"unclosed", "startedAfter:yesterday-ish"]) {
        const reply = await tenant.api.get("/v1/runs", { query: { q } });
        expectProblem(reply, 400, "invalid-query");
        expect(reply.json.query, q).toMatchObject({ start: expect.any(Number), end: expect.any(Number) });
      }
    });

    // -----------------------------------------------------------------------------------------
    // Headers

    test("an idempotency key reused with another body returns the first run and starts nothing", { tag: "@attack" }, async () => {
      // The spec says a second request with the key returns the existing run; it is silent on a
      // different body. Checked: no second run, and the first run's script and input stand.
      const namespace = unique("idem-");
      const key = unique("key-");
      const first = await tenant.api.ns(namespace).post("/v1/runs", { script: short("first"), input: { n: 1 }, wait: "20s" }, { headers: { "idempotency-key": key } });
      expectOk(first, 201);
      const second = await tenant.api
        .ns(namespace)
        .post("/v1/runs", { script: `export default async function main() { return "other"; }\n`, input: { n: 2 }, wait: "20s" }, { headers: { "idempotency-key": key } });
      expectOk(second);
      expect(second.json.id).toBe(first.json.id);
      expect(second.json.script).toBe(first.json.script);
      expect(await namespaceState(tenant.api, namespace)).toMatchObject({ runsStarted: 1 });
      const run = await tenant.api.get(`/v1/runs/${first.json.id}`);
      expect(run.json.input.value).toEqual({ n: 1 });
      // The key is per namespace: another namespace starts its own run.
      const elsewhere = await tenant.api.ns(`${namespace}-b`).post("/v1/runs", { script: short("first") }, { headers: { "idempotency-key": key } });
      expectOk(elsewhere, 201);
      expect(elsewhere.json.id).not.toBe(first.json.id);
      evidence(`the key reused with another script and input → ${second.status} with the first run; one run started (spec silent on a different body)`);
    });

    test("repeated singleton headers never act across namespaces or principals", { tag: "@attack" }, async () => {
      // The spec is silent on repeated headers; the property checked is that a request never
      // acts in a namespace or tenant other than one it was entitled to.
      const minted = await tenant.api.post("/v1/namespaces/alice/tokens", {});
      expectOk(minted, 201);
      const aliceToken = minted.json.token as string;
      const body = JSON.stringify({ script: short(unique("dup")) });
      const bobBefore = await namespaceState(tenant.api, "bob");
      const ownFirst = await rawRequest(stack.url, "POST", "/v1/runs", [
        ["authorization", `Bearer ${aliceToken}`],
        ["content-type", "application/json"],
        ["rewind-namespace", "alice"],
        ["rewind-namespace", "bob"],
      ], body);
      expect(ownFirst.status, ownFirst.text).toBeLessThan(500);
      if (ownFirst.status < 300) expect(ownFirst.json.namespace).toBe("alice");
      const otherFirst = await rawRequest(stack.url, "POST", "/v1/runs", [
        ["authorization", `Bearer ${aliceToken}`],
        ["content-type", "application/json"],
        ["rewind-namespace", "bob"],
        ["rewind-namespace", "alice"],
      ], body);
      expect([400, 403], otherFirst.text).toContain(otherFirst.status);
      expect(await namespaceState(tenant.api, "bob")).toEqual(bobBefore);
      // Two keys of two tenants: the request acts as one of them, never as a mix.
      const other = await newTenant(`hardening-dup-${setup.name[0]}`, stack);
      const who = await rawRequest(stack.url, "GET", "/v1/whoami", [
        ["authorization", `Bearer ${tenant.ownerKey}`],
        ["authorization", `Bearer ${other.ownerKey}`],
      ]);
      expect(who.status, who.text).toBeLessThan(500);
      if (who.status === 200) expect([tenant.id, other.id]).toContain(who.json.tenant);
      evidence(
        `Rewind-Namespace twice with Alice's token: alice first → ${ownFirst.status}, bob first → ${otherFirst.status}; Authorization twice → ${who.status}${who.json?.tenant ? ` as ${who.json.tenant === tenant.id ? "the first" : "the second"}` : ""} (spec silent)`,
      );
    });

    test("a cookie is not a credential on /v1", { tag: "@attack" }, async () => {
      const session = `rewind_session=${randomBytes(48).toString("base64url")}`;
      const bare = tenant.api.as(undefined).with({ cookie: session });
      await expectRefused({ attempt: () => bare.get("/v1/runs"), status: 401, slug: "unauthorized" });
      await expectRefused({ attempt: () => bare.get("/v1/whoami"), status: 401, slug: "unauthorized" });
      const namespace = unique("cookie-");
      await expectRefused({
        attempt: () => bare.ns(namespace).post("/v1/runs", { script: short(unique("cookie")) }),
        status: 401,
        slug: "unauthorized",
        probe: () => namespaceState(tenant.api, namespace),
      });
      evidence("a rewind_session cookie without a Bearer → 401 on /v1");
    });

    test("malformed and oversized credentials are refused, never a 5xx", { tag: "@attack" }, async () => {
      const key = tenant.ownerKey;
      const shapes: [string, Record<string, string>][] = [
        ["Basic with the key", { authorization: `Basic ${Buffer.from(`x:${key}`).toString("base64")}` }],
        ["the key without a scheme", { authorization: key }],
        ["a truncated key", { authorization: `Bearer ${key.slice(0, -4)}` }],
        ["the key with a suffix", { authorization: `Bearer ${key}x` }],
        ["an empty Bearer", { authorization: "Bearer " }],
        ["a key of 64 kB", { authorization: `Bearer rewind_sk_${"a".repeat(64 * 1024)}` }],
        ["an opaque token of nonsense", { authorization: `Bearer rewind_nt_${"z".repeat(40)}` }],
        ["a JWT of nonsense", { authorization: "Bearer aaa.bbb.ccc" }],
      ];
      for (const [name, headers] of shapes) {
        const reply = await tenant.api.as(undefined).get("/v1/whoami", { headers });
        expectNo5xx(reply, name);
        expect([400, 401, 431], `${name} → ${reply.status}`).toContain(reply.status);
      }
      const huge = await tenant.api.get("/v1/runs", { headers: { "x-padding": "a".repeat(128 * 1024) } });
      expectNo5xx(huge, "a 128 kB header");
      const longQuery = await tenant.api.get("/v1/runs", { query: { q: "a".repeat(100_000) } });
      expectNo5xx(longQuery, "a 100 kB query");
      expect(longQuery.status).toBeGreaterThanOrEqual(400);
      evidence("Basic, no scheme, truncated, suffixed, empty, 64 kB keys → 401; 128 kB header, 100 kB query → 4xx");
    });

    // -----------------------------------------------------------------------------------------
    // Routes

    test("a guessed /hooks token is 404 and signals nothing", { tag: "@attack" }, async () => {
      const run = await start(tenant.api.ns("alice"), { script: HOOKED(unique("guess")) });
      await untilWaiting(tenant.api, run.id);
      const real = await hookOf(tenant, run.id);
      const token = real.split("/hooks/")[1]!;
      for (const guess of [randomBytes(24).toString("base64url"), `${token.slice(0, -1)}${token.endsWith("A") ? "B" : "A"}`, "%00", "a".repeat(4096), "..%2F..%2Fv1%2Fruns"]) {
        await expectRefused({
          attempt: () => tenant.api.as(undefined).post(`/hooks/${guess}`, { n: 666 }),
          status: 404,
          slug: "not-found",
          probe: () => runState(tenant.api, run.id),
        });
      }
      evidence("random, off-by-one, NUL, 4 kB and dot-segment /hooks tokens → 404; the run's journal unchanged");
    });

    test("a /hooks URL delivers one signal per delivery id", { tag: "@success" }, async () => {
      const run = await start(tenant.api.ns("alice"), { script: HOOKED(unique("hook")) });
      await untilWaiting(tenant.api, run.id);
      const url = await hookOf(tenant, run.id);
      expect(url.startsWith(`${stack.url}/hooks/`), url).toBe(true);
      const path = url.slice(stack.url.length);
      for (let n = 0; n < 2; n++) {
        const reply = await tenant.api.as(undefined).post(path, { n: 7 }, { headers: { "webhook-id": "delivery-1" } });
        expectOk(reply, 202);
      }
      const ended = await eventually(
        "the run completes with the delivery",
        async () => {
          const reply = await tenant.api.get(`/v1/runs/${run.id}`, { query: { wait: "10s" } });
          return reply.json?.status === "active" ? undefined : reply;
        },
        90_000,
      );
      expect(ended.json.status, ended.text).toBe("completed");
      expect(ended.json.output.value).toEqual({ got: { n: 7 } });
    });

    test("an old /hooks token signals nothing once its run has ended", { tag: "@attack" }, async () => {
      const run = await start(tenant.api.ns("alice"), { script: HOOKED(unique("old")) });
      await untilWaiting(tenant.api, run.id);
      const path = (await hookOf(tenant, run.id)).slice(stack.url.length);
      expectOk(await tenant.api.as(undefined).post(path, { n: 1 }), 202);
      const ended = await eventually(
        "the run completes",
        async () => {
          const reply = await tenant.api.get(`/v1/runs/${run.id}`, { query: { wait: "10s" } });
          return reply.json?.status === "active" ? undefined : reply;
        },
        90_000,
      );
      expect(ended.json.status, ended.text).toBe("completed");
      const before = await runState(tenant.api, run.id);
      const replay = await tenant.api.as(undefined).post(path, { n: 2 });
      expect([404, 409], replay.text).toContain(replay.status);
      expect(replay.slug).toMatch(/^(not-found|run-ended)$/);
      expect(await runState(tenant.api, run.id)).toEqual(before);
      evidence(`POST to the webhook of an ended run → ${replay.status} ${replay.slug}; nothing recorded`);
    });

    /** Each route of `/auth`, with a body it takes. */
    const AUTH: [string, string, unknown][] = [
      ["GET", "/auth/config", undefined],
      ["GET", "/auth/github", undefined],
      ["GET", "/auth/callback?code=x&state=y", undefined],
      ["POST", "/auth/magic-auth", { email: "mallory@example.net" }],
      ["POST", "/auth/magic-auth/verify", { email: "mallory@example.net", code: "123456" }],
      ["POST", "/auth/session", {}],
      ["POST", "/auth/organization", { organizationId: "org_01" }],
      ["POST", "/auth/waitlist", { email: "mallory@example.net" }],
      ["POST", "/auth/logout", {}],
      ["POST", "/auth/email-verification", { code: "123456" }],
    ];

    test("/auth is absent on a server without WorkOS", { tag: "@failure" }, async () => {
      const anonymous = tenant.api.as(undefined).with({ cookie: "rewind_session=forged" });
      for (const [method, path, body] of AUTH) {
        const reply = await anonymous.request(method, path, body === undefined ? {} : { json: body });
        expectProblem(reply, 404, "not-found");
        expect(reply.headers.get("set-cookie"), path).toBeNull();
      }
    });

    test("/auth is absent on a server without WorkOS, whatever the body", { tag: "@failure" }, async () => {
      // archi/07, "Sign-in": without the four WORKOS_* the server has no /auth.
      const anonymous = tenant.api.as(undefined);
      const answers: string[] = [];
      for (const [method, path] of AUTH.filter(([method]) => method === "POST")) {
        const reply = await anonymous.request(method, path, { body: '{"unexpected": true, "email": 42}', contentType: "application/json" });
        expectNo5xx(reply, path);
        answers.push(`${path} → ${reply.status} ${reply.slug ?? ""}`);
      }
      evidence(answers.join("; "));
      expect(answers.filter((a) => !a.includes("→ 404 not-found"))).toEqual([]);
    });

    test("the operator's endpoints refuse an owner", { tag: "@attack" }, async () => {
      const keys = async () => (await tenant.api.get("/v1/api-keys")).json;
      await expectRefused({
        attempt: () => tenant.api.put(`/v1/organizations/${tenant.id}/quotas`, { ceiling: { activeRuns: 1_000_000 } }),
        status: 403,
        slug: "forbidden",
        probe: async () => (await tenant.api.get("/v1/organization/namespace-defaults")).json.ceiling,
      });
      await expectRefused({
        attempt: () => tenant.api.put(`/v1/organizations/gate-operator/quotas`, { ceiling: { activeRuns: 0 } }),
        status: 403,
        slug: "forbidden",
      });
      await expectRefused({ attempt: () => tenant.api.post("/v1/server/stop"), status: 403, slug: "forbidden" });
      expectOk(await tenant.api.get("/v1/server"), 200);
      await expectRefused({
        attempt: () => tenant.api.post("/v1/api-keys", { tenant: unique("victim-"), role: "owner", name: "foothold" }),
        status: 403,
        slug: "forbidden",
        probe: keys,
      });
      await expectRefused({ attempt: () => tenant.api.post("/v1/api-keys", { role: "operator", name: "operator" }), status: 403, slug: "forbidden", probe: keys });
      const role = await tenant.api.put("/v1/roles/operator", { description: "mine", permissions: ["runs:read"] });
      expect([400, 403], role.text).toContain(role.status);
      evidence("an owner setting a ceiling, stopping the server, minting in another tenant or an operator key → 403 forbidden");
    });

    test("unknown routes and methods are refused with a problem, never a 5xx", { tag: "@failure" }, async () => {
      expectProblem(await tenant.api.get("/v1/nothing-here"), 404, "not-found");
      const anonymous = await tenant.api.as(undefined).get("/v1/nothing-here");
      expect([401, 404], anonymous.text).toContain(anonymous.status);
      for (const [method, path] of [
        ["DELETE", "/v1/server"],
        ["PATCH", "/v1/runs"],
        ["PUT", "/v1/whoami"],
      ] as const) {
        const reply = await tenant.api.request(method, path);
        expectNo5xx(reply, `${method} ${path}`);
        expect([401, 404, 405], `${method} ${path} → ${reply.status}`).toContain(reply.status);
      }
    });
  });
}
