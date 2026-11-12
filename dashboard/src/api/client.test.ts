import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { createWorld } from "../mocks/fixtures";
import { createHandlers } from "../mocks/handlers";
import { server } from "../test/server";
import { Api, ApiError, toProblem } from "./client";

const NOW = Date.parse("2026-11-12T10:00:00Z");
const ORIGIN = window.location.origin;

function api(key: string | null = null) {
  return new Api({ baseUrl: ORIGIN, key: () => key });
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
          roles: ["api"],
          embedded: false,
          virtualTime: false,
        });
      }),
    );
    await api("ptc_sk_test").server();
    expect(seen).toBe("Bearer ptc_sk_test");
  });

  it("sends no Authorization header without a key", async () => {
    let seen: string | null = "unset";
    server.use(
      http.get("*/v1/server", ({ request }) => {
        seen = request.headers.get("Authorization");
        return HttpResponse.json({
          version: "0.1.0",
          epochs: [1],
          roles: ["api"],
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
      ...createHandlers(createWorld({ now: NOW, requireKey: "ptc_sk_good" }), { now: () => NOW }),
    );
    const error = await api("ptc_sk_bad")
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
        type: "urn:ptc:problem:not-found",
        title: "Not found",
        detail: "No run x.",
      }),
    ).toMatchObject({
      status: 404,
      detail: "No run x.",
    });
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
