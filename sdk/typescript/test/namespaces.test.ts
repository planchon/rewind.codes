import type { JsonWebKey } from "node:crypto";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  NamespaceRequiredError,
  Rewind,
  RewindApiError,
  RewindConnectionError,
  RewindError,
  isNamespaceName,
  isProblem,
  type Namespace,
  type NamespaceDetail,
  type Quotas,
} from "../src/index.js";
import { NAMESPACE, RUN_ID, TOOLSET, run } from "./fixtures.js";
import { MockServer, json, noContent, problem, type MockRequest } from "./mock-server.js";

let server: MockServer;
let rewind: Rewind;

beforeEach(async () => {
  server = await MockServer.start();
  rewind = new Rewind({ url: server.url, apiKey: "rewind_sk_backend" });
});

afterEach(async () => {
  await server.close();
});

const SCRIPT = "export default async function main() { return 1; }";

/** `POST /v1/runs` as the server answers it: a key's request must name the namespace. */
function serveStart(): void {
  server.on("POST", "/v1/runs", (request) =>
    request.headers["rewind-namespace"] === undefined &&
    request.headers["authorization"]?.startsWith("Bearer rewind_sk_")
      ? problem(
          400,
          "invalid-request",
          "the header Rewind-Namespace is required: it names the end user the run acts for",
        )
      : json(run({ namespace: String(request.headers["rewind-namespace"] ?? NAMESPACE) }), 201),
  );
}

const namespaceOf = (request: MockRequest | undefined) => request?.headers["rewind-namespace"];

function namespace(name: string, overrides: Partial<Namespace> = {}): Namespace {
  return {
    name,
    limits: { activeRuns: 5, runsPerMonth: 1_000 },
    exceptions: {},
    usage: { cpu: "12m", activeRuns: 1, runsStarted: 812, scripts: 4 },
    createdAt: "2026-11-01T09:00:00Z",
    lastActiveAt: "2026-11-14T10:00:00Z",
    ...overrides,
  };
}

function detail(name: string, exceptions: Quotas = {}): NamespaceDetail {
  return {
    namespace: namespace(name, { exceptions }),
    month: "2026-11",
    days: [{ day: "2026-11-14", cpu: "2m", runsStarted: 30 }],
    alerts: [
      {
        namespace: name,
        quota: "runs_per_month",
        threshold: 80,
        used: 812,
        limit: 1_000,
        period: "2026-11",
        raisedAt: "2026-11-14T09:00:00Z",
      },
    ],
    tokens: 2,
  };
}

describe("a client bound to a namespace", () => {
  it("names the end user on every request, and keeps the client's key and settings", async () => {
    serveStart();
    server
      .on("POST", "/v1/check", json({ ok: true, diagnostics: [] }))
      .on("POST", "/v1/test", json({ passed: true, status: "completed", calls: {}, failures: [], trace: "" }))
      .on("GET", "/v1/runs", json({ runs: [] }))
      .on("GET", "/v1/runs/{id}", json(run()));
    const alice = new Rewind({
      url: server.url,
      apiKey: "rewind_sk_backend",
      headers: { "X-Tenant-App": "crm" },
    }).namespace("alice");
    const started = await alice.runs.start({ toolset: TOOLSET, script: SCRIPT });
    await alice.scripts.check({ script: SCRIPT });
    await alice.scripts.test({ script: SCRIPT });
    await alice.runs.list();
    await alice.runs.get(RUN_ID);

    expect(started.namespace).toBe("alice");
    expect(server.requests.map((request) => namespaceOf(request))).toEqual([
      "alice",
      "alice",
      "alice",
      "alice",
      "alice",
    ]);
    for (const request of server.requests) {
      expect(request.headers["authorization"]).toBe("Bearer rewind_sk_backend");
      expect(request.headers["x-tenant-app"]).toBe("crm");
    }
    expect(alice.url).toBe(server.url);
  });

  it("leaves the client it came from unbound, and binds again to another end user", async () => {
    server.on("GET", "/v1/runs", json({ runs: [] }));
    await rewind.namespace("alice").namespace("bob").runs.list();
    await rewind.runs.list();
    expect(namespaceOf(server.requests[0])).toBe("bob");
    expect(namespaceOf(server.requests[1])).toBeUndefined();
  });

  it("refuses a name the server would refuse, before any request", () => {
    for (const name of ["", "*", "a b", "a/b", "é", "x".repeat(129)]) {
      expect(() => rewind.namespace(name), name).toThrow(RewindError);
      expect(isNamespaceName(name)).toBe(false);
    }
    for (const name of ["alice", "user_2fQ81", "a.b:c@d-e", "x".repeat(128)]) {
      expect(isNamespaceName(name)).toBe(true);
    }
    expect(() => rewind.namespace("a b")).toThrow('invalid namespace "a b"');
    expect(server.requests).toHaveLength(0);
  });
});

describe("a key's client without a namespace", () => {
  it("is refused by the server when it starts a run, with an error that says what to do", async () => {
    serveStart();
    const error = await rewind.runs.start({ toolset: TOOLSET, script: SCRIPT }).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(NamespaceRequiredError);
    expect(error).toBeInstanceOf(RewindApiError);
    expect(isProblem(error, "invalid-request")).toBe(true);
    const refused = error as NamespaceRequiredError;
    expect(refused.status).toBe(400);
    expect(refused.name).toBe("NamespaceRequiredError");
    expect(refused.detail).toBe("the header Rewind-Namespace is required: it names the end user the run acts for");
    expect(refused.message).toContain("the header Rewind-Namespace is required");
    expect(refused.message).toContain("rewind.namespace(endUserId)");
    expect(refused.message).toContain("new Rewind({ token })");
    // A refusal is not sent again.
    expect(server.calls("POST", "/v1/runs")).toHaveLength(1);
  });

  it("is refused alike when it checks or tests a script", async () => {
    const refusal = problem(400, "invalid-request", "the header Rewind-Namespace is required: it names the end user");
    server.on("POST", "/v1/check", refusal).on("POST", "/v1/test", refusal);
    await expect(rewind.scripts.check({ script: SCRIPT })).rejects.toBeInstanceOf(NamespaceRequiredError);
    await expect(rewind.scripts.test({ script: SCRIPT })).rejects.toBeInstanceOf(NamespaceRequiredError);
  });

  it("gets other refusals as they are", async () => {
    server.on("POST", "/v1/runs", problem(400, "invalid-request", "ttl: expected a duration"));
    const error = await rewind.runs.start({ script: SCRIPT }).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(RewindApiError);
    expect(error).not.toBeInstanceOf(NamespaceRequiredError);
    expect((error as RewindApiError).message).toBe("ttl: expected a duration");
  });

  it("reads every namespace of the tenant", async () => {
    server.on("GET", "/v1/runs", json({ runs: [] }));
    await rewind.runs.list();
    expect(namespaceOf(server.requests[0])).toBeUndefined();
  });
});

describe("a client holding an end user's token", () => {
  it("sends the token, never the key of the environment, and starts runs without naming the namespace", async () => {
    serveStart();
    process.env["REWIND_API_KEY"] = "rewind_sk_env";
    try {
      const asUser = new Rewind({ url: server.url, token: "rewind_nt_alice" });
      await asUser.runs.start({ toolset: TOOLSET, script: SCRIPT });
    } finally {
      delete process.env["REWIND_API_KEY"];
    }
    const request = server.requests[0];
    expect(request?.headers["authorization"]).toBe("Bearer rewind_nt_alice");
    expect(namespaceOf(request)).toBeUndefined();
  });

  it("takes a token the customer signed", async () => {
    server.on("GET", "/v1/runs", json({ runs: [] }));
    const jwt = "eyJhbGciOiJFZERTQSIsImtpZCI6InJrXzAxIn0.eyJzdWIiOiJhbGljZSJ9.c2lnbmF0dXJl";
    await new Rewind({ url: server.url, token: jwt }).runs.list();
    expect(server.requests[0]?.headers["authorization"]).toBe(`Bearer ${jwt}`);
  });

  it("is not also given a key", () => {
    expect(() => new Rewind({ url: server.url, apiKey: "rewind_sk_x", token: "rewind_nt_y" })).toThrow(
      "pass an API key or an end user's token, not both",
    );
  });

  it("gets the server's 403 when it names another end user", async () => {
    server.on("GET", "/v1/runs", (request) =>
      request.headers["rewind-namespace"] === "bob"
        ? problem(403, "forbidden", "this token acts for another namespace")
        : json({ runs: [] }),
    );
    const asAlice = new Rewind({ url: server.url, token: "rewind_nt_alice" });
    await asAlice.namespace("alice").runs.list();
    await expect(asAlice.namespace("bob").runs.list()).rejects.toSatisfy((e) => isProblem(e, "forbidden"));
  });
});

describe("rewind.namespaces", () => {
  it("lists the namespaces: a prefix, those above a share of a limit, an order, a month and pages", async () => {
    server.on("GET", "/v1/namespaces", json({ month: "2026-11", namespaces: [namespace("alice")], next: "c2" }));
    const page = await rewind.namespaces.list({
      prefix: "al",
      above: 0.8,
      order: "runs",
      month: "2026-11",
      limit: 20,
      after: "c1",
    });
    expect(page.namespaces[0]?.usage.runsStarted).toBe(812);
    expect(page.next).toBe("c2");
    const query = server.requests[0]?.query;
    expect(Object.fromEntries(query ?? [])).toEqual({
      prefix: "al",
      above: "0.8",
      order: "runs",
      month: "2026-11",
      limit: "20",
      after: "c1",
    });
  });

  it("lists every namespace, page after page", async () => {
    server.on("GET", "/v1/namespaces", (request) =>
      request.query.get("after") === null
        ? json({ month: "2026-11", namespaces: [namespace("a"), namespace("b")], next: "p2" })
        : json({ month: "2026-11", namespaces: [namespace("c")] }),
    );
    const names: string[] = [];
    for await (const ns of rewind.namespaces.listAll({ order: "name" })) {
      names.push(ns.name);
    }
    expect(names).toEqual(["a", "b", "c"]);
    expect(server.requests.map((r) => r.query.get("after"))).toEqual([null, "p2"]);
    expect(server.requests.every((r) => r.query.get("order") === "name")).toBe(true);
  });

  it("reads one namespace with its use per day and its alerts, sets its exceptions and erases it", async () => {
    server
      .on("GET", "/v1/namespaces/{name}", (r) => json(detail(r.params["name"] ?? "")))
      .on("PUT", "/v1/namespaces/{name}", (r) =>
        json(detail(r.params["name"] ?? "", (r.body as { exceptions: Quotas }).exceptions)),
      )
      .on("DELETE", "/v1/namespaces/{name}", { status: 202 });

    const read = await rewind.namespaces.get("user:alice@example", { month: "2026-10" });
    expect(read.days[0]?.runsStarted).toBe(30);
    expect(read.alerts[0]?.quota).toBe("runs_per_month");
    expect(server.requests[0]?.params["name"]).toBe("user:alice@example");
    expect(server.requests[0]?.query.get("month")).toBe("2026-10");

    const blocked = await rewind.namespaces.put("alice", { exceptions: { activeRuns: 0 } });
    expect(blocked.namespace.exceptions).toEqual({ activeRuns: 0 });
    expect(server.calls("PUT", "/v1/namespaces/alice")[0]?.body).toEqual({ exceptions: { activeRuns: 0 } });

    await rewind.namespaces.put("bob");
    expect(server.calls("PUT", "/v1/namespaces/bob")[0]?.body).toEqual({});

    await rewind.namespaces.delete("alice");
    expect(server.calls("DELETE", "/v1/namespaces/alice")).toHaveLength(1);
  });

  it("gives an end user a display name, and removes it", async () => {
    server.on("PATCH", "/v1/namespaces/{name}", (r) => {
      const { displayName } = r.body as { displayName: string | null };
      const answer = detail(r.params["name"] ?? "");
      return json(displayName ? { ...answer, namespace: { ...answer.namespace, displayName } } : answer);
    });

    const named = await rewind.namespaces.rename("user_2fQ81", "Acme Corp");
    expect(named.namespace.displayName).toBe("Acme Corp");
    expect(server.calls("PATCH", "/v1/namespaces/user_2fQ81")[0]?.body).toEqual({ displayName: "Acme Corp" });

    const unnamed = await rewind.namespaces.rename("user_2fQ81", null);
    expect(unnamed.namespace.displayName).toBeUndefined();
    expect(server.calls("PATCH", "/v1/namespaces/user_2fQ81")[1]?.body).toEqual({ displayName: null });
    await expect(rewind.namespaces.rename("a/b", "x")).rejects.toThrow(RewindError);
  });

  it("sets an end user's credentials for a toolset", async () => {
    server.on("PUT", "/v1/namespaces/{name}/toolsets/{id}/credentials", noContent);
    await rewind.namespaces.setCredentials("alice", TOOLSET, {
      servers: { github: { headers: { Authorization: "Bearer gho_alice" } } },
    });
    const request = server.calls("PUT", `/v1/namespaces/alice/toolsets/${TOOLSET}/credentials`)[0];
    expect(request?.body).toEqual({ servers: { github: { headers: { Authorization: "Bearer gho_alice" } } } });
  });

  it("reads and sets the default limits", async () => {
    const defaults = {
      defaults: { activeRuns: 5, maxTtl: "7d" },
      ceiling: { activeRuns: 500 },
      month: "2026-11",
      usage: { cpu: "3h", activeRuns: 40, runsStarted: 9_000, scripts: 120 },
    };
    server
      .on("GET", "/v1/organization/namespace-defaults", json(defaults))
      .on("PUT", "/v1/organization/namespace-defaults", (r) => json({ ...defaults, ...(r.body as object) }));
    expect((await rewind.namespaces.defaults.get({ month: "2026-11" })).ceiling.activeRuns).toBe(500);
    expect(server.requests[0]?.query.get("month")).toBe("2026-11");
    const set = await rewind.namespaces.defaults.put({ defaults: { activeRuns: 3, cpuPerMonth: "1h" } });
    expect(set.defaults).toEqual({ activeRuns: 3, cpuPerMonth: "1h" });
    expect(server.calls("PUT", "/v1/organization/namespace-defaults")[0]?.body).toEqual({
      defaults: { activeRuns: 3, cpuPerMonth: "1h" },
    });
  });

  it("lists the quota alerts of a month, of one namespace or of all", async () => {
    server.on("GET", "/v1/organization/quota-alerts", (r) =>
      json({
        month: r.query.get("month") ?? "2026-11",
        alerts: [
          {
            namespace: null,
            quota: "cpu_per_month",
            threshold: 100,
            used: 3_600_000,
            limit: 3_600_000,
            period: "2026-11",
            raisedAt: "2026-11-14T08:00:00Z",
            sentAt: "2026-11-14T08:00:02Z",
          },
        ],
      }),
    );
    const all = await rewind.namespaces.alerts();
    expect(all.alerts[0]?.namespace).toBeNull();
    await rewind.namespaces.alerts({ month: "2026-10", namespace: "alice" });
    expect(server.requests[0]?.query.toString()).toBe("");
    expect(Object.fromEntries(server.requests[1]?.query ?? [])).toEqual({ month: "2026-10", namespace: "alice" });
  });

  it("refuses a name the server would refuse, before any request", async () => {
    await expect(rewind.namespaces.get("a/b")).rejects.toBeInstanceOf(RewindError);
    await expect(rewind.namespaces.tokens.create("*")).rejects.toBeInstanceOf(RewindError);
    await expect(rewind.namespaces.alerts({ namespace: "" })).rejects.toBeInstanceOf(RewindError);
    expect(server.requests).toHaveLength(0);
  });
});

describe("end users' tokens", () => {
  const minted = {
    id: "9f".repeat(32),
    namespace: "alice",
    permissions: ["runs:read", "runs:start"],
    labels: { conversation: "c-42" },
    expiresAt: "2026-11-14T11:00:00Z",
    createdAt: "2026-11-14T10:00:00Z",
    token: "rewind_nt_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG",
  };

  it("mints a token of a namespace, which a client then holds", async () => {
    server.on("POST", "/v1/namespaces/{name}/tokens", json(minted, 201)).on("GET", "/v1/runs", json({ runs: [] }));
    const { token, id } = await rewind.namespaces.tokens.create("alice", {
      ttl: "1h",
      permissions: ["runs:read", "runs:start"],
      labels: { conversation: "c-42" },
    });
    expect(id).toBe(minted.id);
    expect(server.calls("POST", "/v1/namespaces/alice/tokens")[0]?.body).toEqual({
      ttl: "1h",
      permissions: ["runs:read", "runs:start"],
      labels: { conversation: "c-42" },
    });
    await new Rewind({ url: server.url, token }).runs.list();
    expect(server.calls("GET", "/v1/runs")[0]?.headers["authorization"]).toBe(`Bearer ${minted.token}`);
  });

  it("is minted once: a lost answer is not sent again", async () => {
    server.on("POST", "/v1/namespaces/{name}/tokens", problem(503, "unavailable", "restarting"));
    await expect(rewind.namespaces.tokens.create("alice")).rejects.toSatisfy((e) => isProblem(e, "unavailable"));
    expect(server.requests).toHaveLength(1);
    expect(server.requests[0]?.body).toEqual({});
  });

  it("refuses an answer without the secret", async () => {
    server.on("POST", "/v1/namespaces/{name}/tokens", json({ ...minted, token: undefined }, 201));
    await expect(rewind.namespaces.tokens.create("alice")).rejects.toBeInstanceOf(RewindConnectionError);
  });

  it("refuses a namespace being erased", async () => {
    server.on(
      "POST",
      "/v1/namespaces/{name}/tokens",
      problem(409, "namespace-deleting", "the namespace alice is being erased: it takes no token"),
    );
    await expect(rewind.namespaces.tokens.create("alice")).rejects.toSatisfy((e) => isProblem(e, "namespace-deleting"));
  });

  it("revokes one token, or every token of a namespace", async () => {
    server
      .on("DELETE", "/v1/tokens/{id}", noContent)
      .on("DELETE", "/v1/namespaces/{name}/tokens", json({ revoked: 3, revokedBefore: "2026-11-14T10:05:00Z" }));
    await rewind.namespaces.tokens.revoke(minted.id);
    const revoked = await rewind.namespaces.tokens.revokeAll("alice");
    expect(revoked).toEqual({ revoked: 3, revokedBefore: "2026-11-14T10:05:00Z" });
    expect(server.calls("DELETE", `/v1/tokens/${minted.id}`)).toHaveLength(1);
    expect(server.calls("DELETE", "/v1/namespaces/alice/tokens")).toHaveLength(1);
  });
});

describe("token keys", () => {
  // An interface without an index signature, as `jose`'s `JWK` and WebCrypto's `JsonWebKey` are.
  const jwk: JsonWebKey = { kty: "OKP", crv: "Ed25519", x: "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo" };
  const key = {
    kid: "rk_01JD5Z",
    name: "backend",
    algorithm: "EdDSA" as const,
    jwk,
    permissions: ["runs:read" as const, "runs:start" as const],
    createdBy: "key:k1",
    createdAt: "2026-11-14T10:00:00Z",
  };

  it("registers a public key, lists the keys and deletes one", async () => {
    server
      .on("POST", "/v1/organization/token-keys", json(key, 201))
      .on("GET", "/v1/organization/token-keys", json({ keys: [key] }))
      .on("DELETE", "/v1/organization/token-keys/{kid}", noContent);
    const created = await rewind.tokenKeys.create({ name: "backend", jwk, permissions: ["runs:read", "runs:start"] });
    expect(created.kid).toBe("rk_01JD5Z");
    expect(server.requests[0]?.body).toEqual({ name: "backend", jwk, permissions: ["runs:read", "runs:start"] });
    expect((await rewind.tokenKeys.list()).map((k) => k.kid)).toEqual(["rk_01JD5Z"]);
    await rewind.tokenKeys.delete("rk_01JD5Z");
    expect(server.calls("DELETE", "/v1/organization/token-keys/rk_01JD5Z")).toHaveLength(1);
  });
});

describe("a quota reached", () => {
  it("says which quota, at which scope, its limit, the use and when it frees", async () => {
    server.on("POST", "/v1/runs", {
      ...problem(429, "quota-exceeded", "This end user started 1000 runs this month, their limit.", {
        quota: "runs_per_month",
        scope: "namespace",
        limit: 1_000,
        used: 1_000,
        resetsAt: "2026-12-01T00:00:00Z",
      }),
      headers: { "Retry-After": "1440000" },
    });
    const error = await rewind
      .namespace("alice")
      .runs.start({ script: SCRIPT, idempotencyKey: "k1" })
      .catch((e: unknown) => e);
    expect(isProblem(error, "quota-exceeded")).toBe(true);
    const reached = error as RewindApiError;
    expect(reached.status).toBe(429);
    expect(reached.message).toBe("This end user started 1000 runs this month, their limit.");
    expect(reached.quota).toEqual({
      quota: "runs_per_month",
      scope: "namespace",
      limit: 1_000,
      used: 1_000,
      resetsAt: "2026-12-01T00:00:00Z",
    });
    // A 429 is not sent again, even with an idempotency key.
    expect(server.requests).toHaveLength(1);
  });

  it("is absent from other problems", async () => {
    server.on("GET", "/v1/runs/{id}", problem(404, "not-found", "no run"));
    const error = (await rewind.runs.get(RUN_ID).catch((e: unknown) => e)) as RewindApiError;
    expect(error.quota).toBeUndefined();
  });
});
