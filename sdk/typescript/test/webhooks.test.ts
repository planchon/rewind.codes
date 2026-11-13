import { createHmac, randomBytes } from "node:crypto";

import { afterEach, describe, expect, it, vi } from "vitest";

import { RewindError, WebhookVerificationFailed, verifyWebhook } from "../src/index.js";

const key = randomBytes(32);
const secret = `whsec_${key.toString("base64")}`;

const body = JSON.stringify({
  type: "credentials.expired",
  timestamp: "2026-11-09T10:02:11Z",
  data: {
    runId: "r1",
    labels: {},
    taskId: "t1",
    seq: 2,
    tool: "github.list_issues",
    server: "github",
    toolset: "ab",
    owner: "toolset",
    parked: "credentials_expired",
  },
});

function sign(id: string, timestamp: number, payload: string, with_ = key): string {
  return `v1,${createHmac("sha256", with_).update(`${id}.${timestamp}.${payload}`).digest("base64")}`;
}

function headers(id: string, timestamp: number, signature: string) {
  return { "webhook-id": id, "webhook-timestamp": String(timestamp), "webhook-signature": signature };
}

const now = () => Math.floor(Date.now() / 1000);

afterEach(() => {
  vi.useRealTimers();
});

describe("webhook verification", () => {
  it("matches the test vector of Standard Webhooks", () => {
    vi.useFakeTimers({ now: 1614265330 * 1000 });
    const vector = '{"test": 2432232314}';
    const signed = headers(
      "msg_p5jXN8AQM9LWM0D4loKWxJek",
      1614265330,
      "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=",
    );
    // The signature holds; the body is just not a lifecycle event.
    expect(() => verifyWebhook(vector, signed, "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw")).toThrow(
      /not a lifecycle event/,
    );
    const tampered = { ...signed, "webhook-signature": "v1,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=" };
    expect(() => verifyWebhook(vector, tampered, "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw")).toThrow(
      /No matching signature/,
    );
  });

  it("returns the event of a delivery signed with the secret", () => {
    const at = now();
    const event = verifyWebhook(body, headers("msg_1", at, sign("msg_1", at, body)), secret);
    expect(event.type).toBe("credentials.expired");
  });

  it("takes the body as bytes, headers as Headers or as Node's header map, and rotated signatures", () => {
    const at = now();
    const signature = `v1,b2xkIHNpZ25hdHVyZQ== ${sign("msg_1", at, body)}`;
    expect(
      verifyWebhook(new TextEncoder().encode(body), new Headers(headers("msg_1", at, signature)), secret).type,
    ).toBe("credentials.expired");
    const nodeHeaders = { "Webhook-Id": "msg_1", "webhook-timestamp": String(at), "webhook-signature": [signature] };
    expect(verifyWebhook(Buffer.from(body), nodeHeaders, secret).type).toBe("credentials.expired");
  });

  it("refuses another secret, another id, a changed body and an old delivery", () => {
    const at = now();
    const signed = headers("msg_1", at, sign("msg_1", at, body));
    const other = `whsec_${randomBytes(32).toString("base64")}`;
    expect(() => verifyWebhook(body, signed, other)).toThrow(WebhookVerificationFailed);
    expect(() => verifyWebhook(body, { ...signed, "webhook-id": "msg_2" }, secret)).toThrow(WebhookVerificationFailed);
    expect(() => verifyWebhook(`${body} `, signed, secret)).toThrow(WebhookVerificationFailed);
    const old = at - 301;
    expect(() => verifyWebhook(body, headers("msg_1", old, sign("msg_1", old, body)), secret)).toThrow(/too old/);
    expect(() => verifyWebhook(body, { "webhook-id": "msg_1" }, secret)).toThrow(/Missing required headers/);
  });

  it("refuses a malformed secret", () => {
    expect(() => verifyWebhook(body, {}, "whsec_not base64!")).toThrow(RewindError);
  });
});
