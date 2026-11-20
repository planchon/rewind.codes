import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  Rewind,
  RewindApiError,
  RewindError,
  isProblem,
  isSecretName,
  rewindIssuer,
  rewindJwksUrl,
  type Secret,
  type SecretUsage,
  type SigningKeyList,
} from "../src/index.js";
import { NAMESPACE, run } from "./fixtures.js";
import { MockServer, json, noContent, problem } from "./mock-server.js";

let server: MockServer;
let rewind: Rewind;

beforeEach(async () => {
  server = await MockServer.start();
  rewind = new Rewind({ url: server.url, apiKey: "rewind_sk_backend" });
});

afterEach(async () => {
  await server.close();
});

function secret(name: string, overrides: Partial<Secret> = {}): Secret {
  return {
    name,
    scope: "org",
    reference: `org/${name}`,
    labels: { env: "prod" },
    createdBy: "user:user_01ADA",
    createdAt: "2026-11-01T09:00:00Z",
    updatedBy: "key:k1",
    updatedAt: "2026-11-18T09:00:00Z",
    reads: [{ day: "2026-11-19", reads: 42 }],
    lastRead: { run: "run-1", namespace: "alice", at: "2026-11-19T10:00:00Z" },
    ...overrides,
  };
}

describe("the organization's secrets", () => {
  it("lists them, never with a value", async () => {
    server.on("GET", "/v1/secrets", json({ secrets: [secret("zendesk")] }));
    const secrets = await rewind.secrets.list();
    expect(secrets.map((s) => s.reference)).toEqual(["org/zendesk"]);
    expect(secrets[0]).not.toHaveProperty("value");
  });

  it("puts a value with its labels, and sends a put again after a 503", async () => {
    let attempts = 0;
    server.on("PUT", "/v1/secrets/{name}", (request) => {
      attempts += 1;
      return attempts === 1
        ? problem(503, "unavailable", "busy")
        : json(secret(request.params["name"] ?? "", { labels: { env: "prod" } }), 201);
    });
    const put = await rewind.secrets.put("zendesk", { value: "zd_live_123", labels: { env: "prod" } });
    expect(put.reference).toBe("org/zendesk");
    const [, second] = server.calls("PUT", "/v1/secrets/zendesk");
    expect(second?.body).toEqual({ value: "zd_live_123", labels: { env: "prod" } });
    expect(second?.headers["authorization"]).toBe("Bearer rewind_sk_backend");
  });

  it("refuses a name the server would refuse, before sending anything", async () => {
    expect(isSecretName("github")).toBe(true);
    expect(isSecretName("gh_token-2")).toBe(true);
    for (const name of ["", "GitHub", "2fa", "-x", "a".repeat(65), "with space", "org/github"]) {
      expect(isSecretName(name)).toBe(false);
      await expect(rewind.secrets.put(name, { value: "x" })).rejects.toThrow(RewindError);
    }
    expect(server.requests).toHaveLength(0);
  });

  it("deletes one, and reads first what deleting it would stop", async () => {
    const usage: SecretUsage = {
      secret: secret("zendesk"),
      activeRuns: ["run-1", "run-2"],
      activeTotal: 2,
      waitingRuns: ["run-2"],
      waitingTotal: 1,
      workflows: [{ name: "support", version: "9f2c4e1a07b3", servers: ["zendesk"] }],
    };
    server.on("GET", "/v1/secrets/{name}/usage", json(usage));
    server.on("DELETE", "/v1/secrets/{name}", noContent);
    expect(await rewind.secrets.usage("zendesk")).toEqual(usage);
    await rewind.secrets.delete("zendesk");
    expect(server.calls("DELETE", "/v1/secrets/zendesk")).toHaveLength(1);
  });

  it("surfaces a refusal to an end user's token", async () => {
    server.on("PUT", "/v1/secrets/{name}", problem(403, "forbidden", "an end user's token writes its namespace's"));
    const error = await rewind.secrets.put("zendesk", { value: "x" }).catch((e: unknown) => e);
    expect(isProblem(error, "forbidden")).toBe(true);
  });
});

describe("a namespace's secrets", () => {
  it("puts, lists, reads the usage of and deletes the end user's own", async () => {
    server.on("PUT", "/v1/namespaces/{name}/secrets/{secret}", (request) =>
      json(
        secret(request.params["secret"] ?? "", {
          scope: "ns",
          reference: `ns/${request.params["secret"]}`,
          namespace: request.params["name"],
          labels: {},
        }),
        201,
      ),
    );
    server.on("GET", "/v1/namespaces/{name}/secrets", (request) =>
      json({
        secrets: [secret("github", { scope: "ns", reference: "ns/github", namespace: request.params["name"] })],
      }),
    );
    server.on("GET", "/v1/namespaces/{name}/secrets/{secret}/usage", (request) =>
      json({
        secret: secret(request.params["secret"] ?? "", { scope: "ns" }),
        activeRuns: [],
        activeTotal: 0,
        waitingRuns: [],
        waitingTotal: 0,
        workflows: [],
      }),
    );
    server.on("DELETE", "/v1/namespaces/{name}/secrets/{secret}", noContent);

    const put = await rewind.namespaces.secrets.put("alice", "github", { value: "ghp_alice" });
    expect(put.reference).toBe("ns/github");
    const [sent] = server.calls("PUT", "/v1/namespaces/alice/secrets/github");
    // Only the value: a namespace's secret takes no labels.
    expect(sent?.body).toEqual({ value: "ghp_alice" });

    const listed = await rewind.namespaces.secrets.list("alice");
    expect(listed[0]?.namespace).toBe("alice");
    expect((await rewind.namespaces.secrets.usage("alice", "github")).activeTotal).toBe(0);
    await rewind.namespaces.secrets.delete("alice", "github");
    expect(server.calls("DELETE", "/v1/namespaces/alice/secrets/github")).toHaveLength(1);
  });

  it("refuses a namespace or a secret name the server would refuse", async () => {
    await expect(rewind.namespaces.secrets.put("bad name", "github", { value: "x" })).rejects.toThrow(/namespace/);
    await expect(rewind.namespaces.secrets.delete("alice", "Bad")).rejects.toThrow(/secret name/);
    expect(server.requests).toHaveLength(0);
  });
});

describe("attachments", () => {
  it("sends the attachments of a start and of a fork, names and never values", async () => {
    server.on("POST", "/v1/runs", json(run({ secrets: { github: "ns/github" } }), 201));
    server.on("POST", "/v1/runs/{id}/fork", json(run({ id: "run-2" }), 201));
    const started = await rewind.namespace(NAMESPACE).runs.start({
      toolset: "ts",
      script: "export default async () => 1",
      secrets: { github: "ns/github", zendesk: "org/zendesk" },
    });
    expect(started.secrets).toEqual({ github: "ns/github" });
    expect(server.calls("POST", "/v1/runs")[0]?.body).toMatchObject({
      secrets: { github: "ns/github", zendesk: "org/zendesk" },
    });

    await rewind.runs.fork("run-1", { script: "export default async () => 2", secrets: { github: "org/github" } });
    expect(server.calls("POST", "/v1/runs/run-1/fork")[0]?.body).toMatchObject({ secrets: { github: "org/github" } });
  });

  it("saves a workflow with the secrets its runs attach", async () => {
    server.on("POST", "/v1/workflows", (request) => json({ ...(request.body as object), version: "abc" }, 201));
    await rewind.workflows.create({
      name: "triage",
      script: "export default async () => 1",
      toolset: "ts",
      input: { type: "object" },
      output: { type: "object" },
      secrets: { github: "ns/github" },
    });
    expect(server.calls("POST", "/v1/workflows")[0]?.body).toMatchObject({ secrets: { github: "ns/github" } });
  });

  it("surfaces secret-required with each attachment at fault and what may be attached", async () => {
    const attachments = [
      { server: "github", problem: "missing", secret: null, attachable: ["ns/github", "org/github"] },
      { server: "zendesk", problem: "forbidden", secret: "org/zendesk", attachable: [] },
    ];
    server.on(
      "POST",
      "/v1/runs",
      problem(422, "secret-required", "github needs a secret: attach ns/github or org/github", { attachments }),
    );
    const error = await rewind
      .namespace(NAMESPACE)
      .runs.start({ toolset: "ts", script: "export default async () => 1" })
      .catch((e: unknown) => e);
    expect(error).toBeInstanceOf(RewindApiError);
    expect(isProblem(error, "secret-required")).toBe(true);
    expect((error as RewindApiError).attachments).toEqual(attachments);
    expect((error as RewindApiError).message).toContain("attach ns/github");
  });
});

describe("signing keys", () => {
  const keys: SigningKeyList = {
    issuer: "https://api.rewind.codes/organizations/org_01ACME",
    jwksUrl: "https://api.rewind.codes/organizations/org_01ACME/jwks.json",
    keys: [
      { kid: "3f9a1c2b4d5e6f70", createdAt: "2026-11-20T09:00:00Z", retiredAt: null },
      { kid: "0a1b2c3d4e5f6071", createdAt: "2026-10-01T09:00:00Z", retiredAt: "2026-11-20T09:00:00Z" },
    ],
  };

  it("lists the keys with the issuer and the address of the public keys", async () => {
    server.on("GET", "/v1/organization/signing-keys", json(keys));
    expect(await rewind.signingKeys.list()).toEqual(keys);
  });

  it("rotates the key once, never sending the request again", async () => {
    server.on("POST", "/v1/organization/signing-keys", problem(503, "unavailable", "busy"));
    await expect(rewind.signingKeys.rotate()).rejects.toThrow(RewindApiError);
    expect(server.calls("POST", "/v1/organization/signing-keys")).toHaveLength(1);
    server.on("POST", "/v1/organization/signing-keys", json(keys.keys[0], 201));
    expect((await rewind.signingKeys.rotate()).kid).toBe("3f9a1c2b4d5e6f70");
  });

  it("reads an organization's public keys", async () => {
    const jwks = { keys: [{ kty: "OKP", crv: "Ed25519", x: "abc", kid: "3f9a", alg: "EdDSA", use: "sig" }] };
    server.on("GET", "/organizations/{tenant}/jwks.json", json(jwks));
    expect(await rewind.signingKeys.jwks("org_01ACME")).toEqual(jwks);
  });

  it("builds the issuer and the address of its public keys", () => {
    const issuer = rewindIssuer("https://api.rewind.codes/", "org_01ACME");
    expect(issuer).toBe("https://api.rewind.codes/organizations/org_01ACME");
    expect(rewindJwksUrl(issuer)).toBe("https://api.rewind.codes/organizations/org_01ACME/jwks.json");
    expect(rewindJwksUrl(`${issuer}/`)).toBe(rewindJwksUrl(issuer));
  });
});
