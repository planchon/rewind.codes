// `verifyRewindCall` against JWTs signed as Rewind signs them, with real Ed25519 keys, and the public keys served by
// a mocked server (`{issuer}/jwks.json`).
import { createHash, generateKeyPairSync, type KeyObject } from "node:crypto";
import { readFileSync } from "node:fs";

import { SignJWT, base64url, type JWK } from "jose";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  RewindConnectionError,
  RewindSignatureError,
  verifyRewindCall,
  type ReceivedRequest,
  type SignatureCheck,
} from "../src/index.js";
import { MockServer, json } from "./mock-server.js";

const TENANT = "org_01ACME";
const AUDIENCE = "https://api.example.com";
const URL_CALLED = "https://api.example.com/v1/measures?model=sales";
const BODY = JSON.stringify({ name: "Revenue", expression: "SUM(Sales[Amount])" });

interface SigningKey {
  kid: string;
  privateKey: KeyObject;
  jwk: JWK;
}

/** An Ed25519 key, its `kid` the first 8 bytes of the SHA-256 of the public key, in hex, as Rewind names it. */
function signingKey(): SigningKey {
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const raw = publicKey.export({ format: "jwk" });
  const x = String(raw.x);
  const kid = createHash("sha256").update(base64url.decode(x)).digest("hex").slice(0, 16);
  return { kid, privateKey, jwk: { kty: "OKP", crv: "Ed25519", x, kid, alg: "EdDSA", use: "sig" } };
}

let server: MockServer;
let issuer: string;
let current: SigningKey;
/** The public keys the issuer publishes. */
let published: SigningKey[];

beforeEach(async () => {
  server = await MockServer.start();
  // A new port, so a new issuer, for each test: the keys cached by another test are not this one's.
  issuer = `${server.url}/organizations/${TENANT}`;
  current = signingKey();
  published = [current];
  server.on("GET", `/organizations/${TENANT}/jwks.json`, () => ({
    status: 200,
    body: { keys: published.map((k) => k.jwk) },
    headers: { "Cache-Control": "max-age=300" },
  }));
});

afterEach(async () => {
  vi.useRealTimers();
  await server.close();
});

function sha256(text: string): string {
  return base64url.encode(createHash("sha256").update(text).digest());
}

/** The claims of a call to `dax.createMeasure`, as Rewind signs them now. */
function callClaims(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  const now = Math.floor(Date.now() / 1000);
  return {
    iss: issuer,
    aud: AUDIENCE,
    sub: "user:42",
    labels: { env: "prod", goal: "q3-report" },
    run: "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10",
    tool: "dax.createMeasure",
    idempotency_key: "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10/7",
    jti: "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10/7/1",
    htm: "POST",
    htu: URL_CALLED,
    body: sha256(BODY),
    iat: now,
    exp: now + 60,
    ...overrides,
  };
}

async function sign(
  claims: Record<string, unknown>,
  options: { key?: SigningKey; alg?: string; kid?: string } = {},
): Promise<string> {
  const key = options.key ?? current;
  const defined = Object.fromEntries(Object.entries(claims).filter(([, value]) => value !== undefined));
  return new SignJWT(defined)
    .setProtectedHeader({ alg: options.alg ?? "EdDSA", kid: options.kid ?? key.kid, typ: "JWT" })
    .sign(key.privateKey);
}

/** The request as Express hands it after `express.raw()`. */
function received(token: string, overrides: Partial<ReceivedRequest> = {}): ReceivedRequest {
  return {
    method: "POST",
    url: URL_CALLED,
    headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
    body: Buffer.from(BODY),
    ...overrides,
  };
}

const options = () => ({ issuer, audience: AUDIENCE });

async function refusedCheck(promise: Promise<unknown>): Promise<SignatureCheck> {
  const error = await promise.then(
    () => undefined,
    (e: unknown) => e,
  );
  expect(error).toBeInstanceOf(RewindSignatureError);
  return (error as RewindSignatureError).check;
}

describe("verifyRewindCall", () => {
  it("answers who a signed call acts for", async () => {
    const call = await verifyRewindCall(received(await sign(callClaims())), { ...options(), labels: { env: "prod" } });
    expect(call).toEqual({
      namespace: "user:42",
      labels: { env: "prod", goal: "q3-report" },
      run: "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10",
      tool: "dax.createMeasure",
      idempotencyKey: "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10/7",
    });
  });

  it("takes a Fetch Request, and leaves its body to be read", async () => {
    const request = new Request(URL_CALLED, {
      method: "POST",
      headers: { Authorization: `Bearer ${await sign(callClaims())}`, "Content-Type": "application/json" },
      body: BODY,
    });
    const call = await verifyRewindCall(request, options());
    expect(call.namespace).toBe("user:42");
    expect(await request.json()).toEqual(JSON.parse(BODY));
  });

  it("checks a call without a body, and Node's headers as a list", async () => {
    const claims = callClaims({ htm: "GET", htu: "https://api.example.com/v1/measures/m1", body: undefined });
    const token = await sign(claims);
    const call = await verifyRewindCall(
      {
        method: "get",
        url: "https://API.example.com:443/v1/measures/m1",
        headers: { Authorization: [`Bearer ${token}`] },
      },
      options(),
    );
    expect(call.tool).toBe("dax.createMeasure");
  });

  it("fetches the keys once and keeps them", async () => {
    for (let i = 0; i < 3; i++) {
      await verifyRewindCall(received(await sign(callClaims())), options());
    }
    expect(server.calls("GET", `/organizations/${TENANT}/jwks.json`)).toHaveLength(1);
  });

  describe("refuses, naming the check that failed", () => {
    it("token: no Bearer, or one that is not a JWT", async () => {
      expect(await refusedCheck(verifyRewindCall(received("x", { headers: {} }), options()))).toBe("token");
      expect(await refusedCheck(verifyRewindCall(received("not.a.jwt"), options()))).toBe("token");
    });

    it("alg: not EdDSA", async () => {
      const { createSecretKey } = await import("node:crypto");
      const hmac = createSecretKey(Buffer.alloc(32, 7));
      const token = await new SignJWT(callClaims()).setProtectedHeader({ alg: "HS256", kid: current.kid }).sign(hmac);
      expect(await refusedCheck(verifyRewindCall(received(token), options()))).toBe("alg");
    });

    it("kid: a key the issuer does not publish", async () => {
      const stranger = signingKey();
      expect(
        await refusedCheck(verifyRewindCall(received(await sign(callClaims(), { key: stranger })), options())),
      ).toBe("kid");
    });

    it("signature: signed by another key under the issuer's kid", async () => {
      const forger = signingKey();
      const token = await sign(callClaims(), { key: forger, kid: current.kid });
      expect(await refusedCheck(verifyRewindCall(received(token), options()))).toBe("signature");
    });

    it("signature: a claim changed after signing", async () => {
      const [header, , signature] = (await sign(callClaims())).split(".");
      const payload = base64url.encode(JSON.stringify(callClaims({ sub: "user:666" })));
      expect(await refusedCheck(verifyRewindCall(received(`${header}.${payload}.${signature}`), options()))).toBe(
        "signature",
      );
    });

    it("iss: another organization's issuer, even with keys it publishes", async () => {
      // Another organization signs with its own key, published at its own issuer.
      const other = `${server.url}/organizations/org_01GLOBEX`;
      const otherKey = signingKey();
      server.on("GET", "/organizations/org_01GLOBEX/jwks.json", json({ keys: [otherKey.jwk] }));
      const token = await sign(callClaims({ iss: other }), { key: otherKey });
      expect(await refusedCheck(verifyRewindCall(received(token), options()))).toBe("kid");
      // Signed by this organization's key, but claiming another issuer.
      expect(await refusedCheck(verifyRewindCall(received(await sign(callClaims({ iss: other }))), options()))).toBe(
        "iss",
      );
      // The issuer the server expects decides where the keys come from, never the JWT.
      expect(server.calls("GET", "/organizations/org_01GLOBEX/jwks.json")).toHaveLength(0);
    });

    it("aud: signed for another server", async () => {
      const token = await sign(callClaims({ aud: "https://other.example.com" }));
      expect(await refusedCheck(verifyRewindCall(received(token), options()))).toBe("aud");
    });

    it("exp: expired, beyond 5 s of leeway", async () => {
      const now = Math.floor(Date.now() / 1000);
      const late = await sign(callClaims({ iat: now - 70, exp: now - 10 }));
      expect(await refusedCheck(verifyRewindCall(received(late), options()))).toBe("exp");
      // Within the leeway, it is still accepted.
      const justExpired = await sign(callClaims({ iat: now - 62, exp: now - 2 }));
      await expect(verifyRewindCall(received(justExpired), options())).resolves.toBeTruthy();
    });

    it("iat: issued in the future, beyond 5 s of leeway", async () => {
      const now = Math.floor(Date.now() / 1000);
      const ahead = await sign(callClaims({ iat: now + 30, exp: now + 90 }));
      expect(await refusedCheck(verifyRewindCall(received(ahead), options()))).toBe("iat");
      const skewed = await sign(callClaims({ iat: now + 3, exp: now + 63 }));
      await expect(verifyRewindCall(received(skewed), options())).resolves.toBeTruthy();
    });

    it("htm: replayed with another method", async () => {
      const token = await sign(callClaims());
      expect(await refusedCheck(verifyRewindCall(received(token, { method: "PUT" }), options()))).toBe("htm");
    });

    it("htu: replayed to another URL", async () => {
      const token = await sign(callClaims());
      for (const url of [
        "https://api.example.com/v1/measures?model=finance",
        "https://api.example.com/v1/users",
        "http://api.example.com/v1/measures?model=sales",
      ]) {
        expect(await refusedCheck(verifyRewindCall(received(token, { url }), options()))).toBe("htu");
      }
    });

    it("body: replayed with another body, or a body added", async () => {
      const token = await sign(callClaims());
      const other = JSON.stringify({ name: "Revenue", expression: "DROP TABLE" });
      expect(await refusedCheck(verifyRewindCall(received(token, { body: other }), options()))).toBe("body");
      expect(await refusedCheck(verifyRewindCall(received(token, { body: undefined }), options()))).toBe("body");
      const bodiless = await sign(callClaims({ body: undefined }));
      expect(await refusedCheck(verifyRewindCall(received(bodiless), options()))).toBe("body");
    });

    it("labels: a run without the labels the server expects", async () => {
      const dev = await sign(callClaims({ labels: { env: "dev" } }));
      const error = await verifyRewindCall(received(dev), { ...options(), labels: { env: "prod" } }).catch(
        (e: unknown) => e as RewindSignatureError,
      );
      expect(error).toBeInstanceOf(RewindSignatureError);
      expect((error as RewindSignatureError).check).toBe("labels");
      expect((error as RewindSignatureError).message).toContain("env=dev");
      const unlabelled = await sign(callClaims({ labels: undefined }));
      expect(
        await refusedCheck(verifyRewindCall(received(unlabelled), { ...options(), labels: { env: "prod" } })),
      ).toBe("labels");
      // Without `labels`, the server accepts any run of the organization.
      await expect(verifyRewindCall(received(dev), options())).resolves.toBeTruthy();
    });

    it("call: a request of an MCP session, unless the server accepts the session", async () => {
      const claims = callClaims({
        sub: undefined,
        labels: undefined,
        run: undefined,
        tool: undefined,
        idempotency_key: undefined,
      });
      const token = await sign(claims);
      expect(await refusedCheck(verifyRewindCall(received(token), options()))).toBe("call");
      expect(
        await verifyRewindCall(received(token), { ...options(), session: true, labels: { env: "prod" } }),
      ).toBeNull();
    });
  });

  it("follows a rotation: an unknown kid is fetched again, at most every 10 seconds", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const start = Date.parse("2026-11-20T09:00:00Z");
    vi.setSystemTime(start);
    await verifyRewindCall(received(await sign(callClaims())), options());
    const jwks = () => server.calls("GET", `/organizations/${TENANT}/jwks.json`).length;
    expect(jwks()).toBe(1);

    // The organization replaces its key: the old one stays published 24 hours.
    const previous = current;
    current = signingKey();
    published = [current, previous];

    vi.setSystemTime(start + 2_000);
    expect(await refusedCheck(verifyRewindCall(received(await sign(callClaims())), options()))).toBe("kid");
    expect(jwks()).toBe(1);

    vi.setSystemTime(start + 11_000);
    await expect(verifyRewindCall(received(await sign(callClaims())), options())).resolves.toBeTruthy();
    expect(jwks()).toBe(2);
    // A call the old key signed before the rotation is still accepted.
    const old = await sign(callClaims(), { key: previous });
    await expect(verifyRewindCall(received(old), options())).resolves.toBeTruthy();
    expect(jwks()).toBe(2);
  });

  it("says when the keys cannot be fetched, apart from a refusal", async () => {
    server.on("GET", `/organizations/${TENANT}/jwks.json`, { status: 500, body: { error: "down" } });
    const error = await verifyRewindCall(received(await sign(callClaims())), options()).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(RewindConnectionError);
    expect(error).not.toBeInstanceOf(RewindSignatureError);
  });

  it("refuses a parsed body or a relative URL as a mistake of the caller", async () => {
    const token = await sign(callClaims());
    await expect(
      verifyRewindCall(received(token, { body: JSON.parse(BODY) as never }), options()),
    ).rejects.toBeInstanceOf(TypeError);
    await expect(verifyRewindCall(received(token, { url: "/v1/measures?model=sales" }), options())).rejects.toThrow(
      /absolute URL/,
    );
  });
});

// The calls Rewind's Rust signer makes (`crates/rewind-outbound/tests/sdk_fixture.rs` writes the fixture, and checks
// that it is still what Rewind signs): the two sides must agree on every claim, not only on what this file signs.
describe("verifyRewindCall on calls signed by Rewind", () => {
  interface SignedCall {
    method: string;
    url: string;
    body: string | null;
    authorization: string;
  }
  const fixture = JSON.parse(readFileSync(new URL("./fixtures/rewind-signed-calls.json", import.meta.url), "utf8")) as {
    issuer: string;
    audience: string;
    now: number;
    jwks: { keys: JWK[] };
    calls: Record<"post" | "get", SignedCall>;
  };

  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(new Date((fixture.now + 1) * 1000));
    vi.stubGlobal("fetch", (input: Request | string | URL) => {
      const url = input instanceof Request ? input.url : String(input);
      expect(url).toBe(`${fixture.issuer}/jwks.json`);
      return Promise.resolve(Response.json(fixture.jwks, { headers: { "Cache-Control": "max-age=300" } }));
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  function receivedOf(call: SignedCall, overrides: Partial<ReceivedRequest> = {}): ReceivedRequest {
    return {
      method: call.method.toUpperCase(),
      url: call.url,
      headers: { authorization: call.authorization },
      body: Buffer.from(call.body ?? ""),
      ...overrides,
    };
  }

  const expected = {
    namespace: "user:42",
    labels: { env: "prod", goal: "q3-report" },
    run: "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10",
    tool: "dax.createMeasure",
  };

  it("accepts a POST with its body and query, the fragment left out", async () => {
    const call = await verifyRewindCall(receivedOf(fixture.calls.post), {
      issuer: fixture.issuer,
      audience: fixture.audience,
      labels: { env: "prod" },
    });
    expect(call).toEqual({ ...expected, idempotencyKey: `${expected.run}/7` });
  });

  it("accepts a GET without a body, its path percent-encoded", async () => {
    const call = await verifyRewindCall(receivedOf(fixture.calls.get), {
      issuer: fixture.issuer,
      audience: fixture.audience,
    });
    expect(call).toEqual({ ...expected, idempotencyKey: `${expected.run}/8` });
  });

  it("refuses Rewind's JWT for another body, URL or environment", async () => {
    const options = { issuer: fixture.issuer, audience: fixture.audience };
    expect(
      await refusedCheck(verifyRewindCall(receivedOf(fixture.calls.post, { body: Buffer.from("{}") }), options)),
    ).toBe("body");
    expect(
      await refusedCheck(
        verifyRewindCall(receivedOf(fixture.calls.get, { url: "https://api.example.com/v1/measures/m%201" }), options),
      ),
    ).toBe("htu");
    expect(
      await refusedCheck(verifyRewindCall(receivedOf(fixture.calls.post), { ...options, labels: { env: "dev" } })),
    ).toBe("labels");
  });
});
