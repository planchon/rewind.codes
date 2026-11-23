import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { createWorld } from "../mocks/fixtures";
import { createHandlers } from "../mocks/handlers";
import { server } from "../test/server";
import { Api, ApiError, toProblem } from "./client";

const NOW = Date.parse("2026-11-12T10:00:00Z");
const ORIGIN = window.location.origin;

function api(key: string | null = null) {
  return new Api({ baseUrl: ORIGIN, token: () => key });
}

describe("Api", () => {
  it("sends the key as a Bearer token", async () => {
    let seen: string | null = null;
    server.use(
      http.get("*/v1/server", ({ request }) => {
        seen = request.headers.get("Authorization");
        return HttpResponse.json({
          version: "0.1.0",
          epochs: [1],
          node: "node-1",
          partitions: 64,
          nodes: 1,
          embedded: false,
          virtualTime: false,
        });
      }),
    );
    await api("rewind_sk_test").server();
    expect(seen).toBe("Bearer rewind_sk_test");
  });

  it("sends no Authorization header without a key", async () => {
    let seen: string | null = "unset";
    server.use(
      http.get("*/v1/server", ({ request }) => {
        seen = request.headers.get("Authorization");
        return HttpResponse.json({
          version: "0.1.0",
          epochs: [1],
          node: "node-1",
          partitions: 1,
          nodes: 1,
          embedded: true,
          virtualTime: false,
        });
      }),
    );
    await api(null).server();
    expect(seen).toBeNull();
  });

  it("throws the problem the server answered", async () => {
    server.use(
      ...createHandlers(createWorld({ now: NOW, requireKey: "rewind_sk_good" }), {
        now: () => NOW,
      }),
    );
    const error = await api("rewind_sk_bad")
      .runs({})
      .catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ApiError);
    expect((error as ApiError).status).toBe(401);
    expect((error as ApiError).slug).toBe("unauthorized");
    expect((error as ApiError).message).toMatch(/unknown or revoked/);
  });

  it("repeats list parameters, as the API reads them", async () => {
    let query = "";
    server.use(
      http.get("*/v1/runs", ({ request }) => {
        query = new URL(request.url).search;
        return HttpResponse.json({ runs: [] });
      }),
    );
    await api().runs({
      label: ["agent:triage", "conversation:c,42"],
      status: "failed",
      quarantined: true,
    });
    const params = new URLSearchParams(query);
    expect(params.getAll("label")).toEqual(["agent:triage", "conversation:c,42"]);
    expect(params.get("status")).toBe("failed");
    expect(params.get("quarantined")).toBe("true");
  });

  it("reads a body that is not a problem as about:blank", () => {
    expect(toProblem(502, "<html>Bad gateway</html>", "Bad Gateway")).toMatchObject({
      type: "about:blank",
      title: "Bad Gateway",
      status: 502,
    });
    expect(
      toProblem(404, {
        type: "urn:rewind:problem:not-found",
        title: "Not found",
        detail: "No run x.",
      }),
    ).toMatchObject({
      status: 404,
      detail: "No run x.",
    });
  });

  it("renews a refused token once, and sends the request again with the new one", async () => {
    const seen: (string | null)[] = [];
    server.use(
      http.get("*/v1/whoami", ({ request }) => {
        const auth = request.headers.get("Authorization");
        seen.push(auth);
        if (auth !== "Bearer fresh")
          return HttpResponse.json(
            { type: "urn:rewind:problem:unauthorized", title: "Unauthorized", status: 401 },
            { status: 401 },
          );
        return HttpResponse.json({ operator: false, permissions: [] });
      }),
    );
    let token = "stale";
    let renewals = 0;
    const client = new Api({
      baseUrl: ORIGIN,
      token: () => token,
      renew: async () => {
        renewals += 1;
        token = "fresh";
        return true;
      },
    });
    expect(await client.whoami()).toEqual({ operator: false, permissions: [] });
    expect(seen).toEqual(["Bearer stale", "Bearer fresh"]);
    expect(renewals).toBe(1);

    // A token refused even renewed: the 401 is the answer.
    token = "revoked";
    const refused = new Api({ baseUrl: ORIGIN, token: () => token, renew: async () => true });
    const error = await refused.whoami().catch((e: unknown) => e);
    expect((error as ApiError).status).toBe(401);
  });

  it("sends /auth requests as JSON, without the token", async () => {
    let seen: { type: string | null; auth: string | null; body: unknown } | null = null;
    server.use(
      http.post("*/auth/session", async ({ request }) => {
        seen = {
          type: request.headers.get("Content-Type"),
          auth: request.headers.get("Authorization"),
          body: await request.json(),
        };
        return HttpResponse.json(
          {
            status: "authenticated",
            accessToken: "at",
            expiresAt: "2026-11-14T10:05:00Z",
            user: { id: "user_1", email: "ada@example.net" },
            organizationId: "org_1",
          },
          { headers: { Date: new Date(Date.now() + 60_000).toUTCString() } },
        );
      }),
    );
    const answer = await api("rewind_sk_test").session("org_1");
    expect(seen).toEqual({
      type: "application/json",
      auth: null,
      body: { organizationId: "org_1" },
    });
    expect(answer.state.status).toBe("authenticated");
    // The server's clock is a minute ahead.
    expect(answer.skewMs).toBeGreaterThan(55_000);
    expect(answer.skewMs).toBeLessThan(61_000);
  });

  it("reads a server without sign-in as one that takes a key", async () => {
    server.use(...createHandlers(createWorld({ now: NOW }), { now: () => NOW }));
    expect(await api().authConfig()).toBeNull();
    server.use(...createHandlers(createWorld({ now: NOW, workos: true }), { now: () => NOW }));
    expect(await api().authConfig()).toEqual({ methods: ["github", "magic_auth"], waitlist: true });
  });

  it("downloads the journal document", async () => {
    const world = createWorld({ now: NOW });
    server.use(...createHandlers(world, { now: () => NOW }));
    const run = world.runs[0];
    const blob = await api().journal(run?.id ?? "", "journal");
    const doc = JSON.parse(await blob.text());
    expect(doc).toMatchObject({ v: 1, run: run?.id });
    expect(doc.entries.length).toBe(run?.journal.entries.length);
  });
});
