import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { Ptc, PtcApiError, PtcConnectionError, PtcError, isProblem } from "../src/index.js";
import { RUN_ID, TOOLSET, payload, run } from "./fixtures.js";
import { MockServer, json, noContent, problem } from "./mock-server.js";

let server: MockServer;
let ptc: Ptc;

beforeEach(async () => {
  server = await MockServer.start();
  ptc = new Ptc({ url: server.url, apiKey: "ptc_sk_test" });
});

afterEach(async () => {
  await server.close();
});

describe("the client", () => {
  it("needs a URL", () => {
    const saved = process.env["PTC_URL"];
    delete process.env["PTC_URL"];
    try {
      expect(() => new Ptc()).toThrow(PtcError);
    } finally {
      if (saved !== undefined) {
        process.env["PTC_URL"] = saved;
      }
    }
  });

  it("reads the URL and the key from the environment", async () => {
    process.env["PTC_URL"] = `${server.url}/`;
    process.env["PTC_API_KEY"] = "ptc_sk_env";
    try {
      const fromEnv = new Ptc();
      server.on(
        "GET",
        "/v1/server",
        json({ embedded: false, epochs: [1], roles: ["api"], version: "0.1.0", virtualTime: false }),
      );
      const info = await fromEnv.server.info();
      expect(info.version).toBe("0.1.0");
      expect(fromEnv.url).toBe(server.url);
      expect(server.requests[0]?.headers["authorization"]).toBe("Bearer ptc_sk_env");
    } finally {
      delete process.env["PTC_URL"];
      delete process.env["PTC_API_KEY"];
    }
  });

  it("sends the API key as a bearer token, and no key when there is none", async () => {
    server.on(
      "GET",
      "/v1/server",
      json({ embedded: true, epochs: [1], roles: ["api", "worker"], version: "0.1.0", virtualTime: false }),
    );
    await ptc.server.info();
    await new Ptc({ url: server.url, apiKey: "" }).server.info();
    expect(server.requests[0]?.headers["authorization"]).toBe("Bearer ptc_sk_test");
    expect(server.requests[1]?.headers["authorization"]).toBeUndefined();
  });

  it("registers a toolset", async () => {
    const toolset = {
      id: TOOLSET,
      declarations: { dts: "declare module 'ptc:tools' {}", prompt: "github.list_issues({ repo }) → Issue[]" },
      servers: [{ name: "github", tools: 1, transport: "http" as const }],
      tools: [],
    };
    server.on("POST", "/v1/toolsets", json(toolset, 201));
    const created = await ptc.toolsets.create({
      mcp: [{ name: "github", url: "https://mcp.example.com/github" }],
      host: [
        {
          name: "ask_user",
          description: "Ask the end user a question.",
          inputSchema: { type: "object", properties: { question: { type: "string" } }, required: ["question"] },
          policy: { onInterruption: "at_most_once", heartbeatTimeout: "2m" },
        },
      ],
    });
    expect(created.id).toBe(TOOLSET);
    expect(server.requests[0]?.body).toMatchObject({
      host: [{ name: "ask_user", policy: { onInterruption: "at_most_once", heartbeatTimeout: "2m" } }],
    });
    expect(server.requests[0]?.headers["content-type"]).toBe("application/json");
  });

  it("reads declarations, searches tools and sets credentials", async () => {
    server
      .on("GET", "/v1/toolsets/{id}/declarations", (r) => json({ format: r.query.get("format") ?? "dts", text: "x" }))
      .on("GET", "/v1/toolsets/{id}/search", json({ tools: [] }))
      .on("PUT", "/v1/toolsets/{id}/credentials", noContent);
    expect((await ptc.toolsets.declarations(TOOLSET, { format: "prompt" })).format).toBe("prompt");
    await ptc.toolsets.search(TOOLSET, "close issue", { limit: 3 });
    await ptc.toolsets.setCredentials(TOOLSET, { servers: { github: { headers: { Authorization: "Bearer x" } } } });
    const search = server.calls("GET", `/v1/toolsets/${TOOLSET}/search`)[0];
    expect(search?.query.get("query")).toBe("close issue");
    expect(search?.query.get("limit")).toBe("3");
    expect(server.calls("PUT", `/v1/toolsets/${TOOLSET}/credentials`)[0]?.body).toEqual({
      servers: { github: { headers: { Authorization: "Bearer x" } } },
    });
  });

  it("starts a run with its idempotency key, labels, trace context and ttl", async () => {
    server.on("POST", "/v1/runs", json(run({ output: payload({ closed: 3 }) }), 201));
    const started = await ptc.runs.start({
      toolset: TOOLSET,
      script: "export default async function main() { return { closed: 3 }; }",
      input: { repo: "acme/api" },
      wait: "30s",
      idempotencyKey: "toolu_01A09q90qw90lq917835lq9",
      labels: { agent: "triage", conversation: "c-42" },
      traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
      ttl: "7d",
    });
    expect(started.status).toBe("completed");
    const request = server.requests[0];
    expect(request?.headers["idempotency-key"]).toBe("toolu_01A09q90qw90lq917835lq9");
    expect(request?.body).toEqual({
      toolset: TOOLSET,
      script: "export default async function main() { return { closed: 3 }; }",
      input: { repo: "acme/api" },
      wait: "30s",
      labels: { agent: "triage", conversation: "c-42" },
      traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
      ttl: "7d",
    });
  });

  it("refuses an idempotency key the server would refuse", async () => {
    await expect(ptc.runs.start({ script: "x", idempotencyKey: "café" })).rejects.toThrow(/idempotency key/);
    await expect(ptc.runs.start({ script: "x", idempotencyKey: "x".repeat(256) })).rejects.toThrow(PtcError);
    expect(server.requests).toHaveLength(0);
  });

  it("waits longer than one request by asking again until the run ends", async () => {
    let gets = 0;
    server
      .on("POST", "/v1/runs", json(run({ status: "active", waitingFor: [] }), 201))
      .on("GET", "/v1/runs/{id}", () => {
        gets += 1;
        return json(run({ status: gets < 3 ? "active" : "completed", output: payload(1) }));
      });
    const ended = await ptc.runs.start({ script: "x", wait: "5s" });
    expect(ended.status).toBe("completed");
    expect(gets).toBe(3);
    const waits = server.calls("GET", `/v1/runs/${RUN_ID}`).map((r) => r.query.get("wait"));
    expect(waits.every((wait) => wait !== null && /^\d+(ms|s)/.test(wait))).toBe(true);
    expect(server.requests[0]?.body).toMatchObject({ wait: expect.stringMatching(/^(5s|4s\d+ms)$/) });
  });

  it("returns a run still active when the wait passes", async () => {
    server
      .on("POST", "/v1/runs", json(run({ status: "active" }), 201))
      .on("GET", "/v1/runs/{id}", json(run({ status: "active" })));
    const started = await ptc.runs.start({ script: "x", wait: 30 });
    expect(started.status).toBe("active");
  });

  it("turns problems into PtcApiError, with the diagnostics of a refused script", async () => {
    const diagnostics = [
      {
        code: "TS2322",
        message: "Type 'number' is not assignable to type 'string'.",
        severity: "error",
        location: { line: 3, column: 28 },
      },
    ];
    server.on("POST", "/v1/runs", problem(422, "invalid-script", "the script does not type-check", { diagnostics }));
    const error = await ptc.runs.start({ script: "x" }).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(PtcApiError);
    const apiError = error as PtcApiError;
    expect(apiError.status).toBe(422);
    expect(apiError.slug).toBe("invalid-script");
    expect(apiError.diagnostics).toEqual(diagnostics);
    expect(apiError.message).toBe("the script does not type-check");
    expect(isProblem(apiError, "invalid-script")).toBe(true);
  });

  it("reads an error body that is not a problem as about:blank", async () => {
    server.on("GET", "/v1/runs/{id}", () => (response) => {
      response.writeHead(502, { "Content-Type": "text/plain" });
      response.end("bad gateway");
    });
    const error = (await ptc.runs.get(RUN_ID).catch((e: unknown) => e)) as PtcApiError;
    expect(error).toBeInstanceOf(PtcApiError);
    expect(error.problem.type).toBe("about:blank");
    expect(error.slug).toBeUndefined();
    expect(error.detail).toBe("bad gateway");
  });

  it("retries a read after a 503, and a start only with an idempotency key", async () => {
    let attempts = 0;
    server.on("GET", "/v1/runs/{id}", () => {
      attempts += 1;
      return attempts === 1
        ? { ...problem(503, "unavailable", "stopping"), headers: { "Retry-After": "0" } }
        : json(run());
    });
    expect((await ptc.runs.get(RUN_ID)).id).toBe(RUN_ID);
    expect(attempts).toBe(2);

    server.on("POST", "/v1/runs", problem(503, "unavailable", "stopping"));
    await expect(ptc.runs.start({ script: "x" })).rejects.toMatchObject({ status: 503 });
    expect(server.calls("POST", "/v1/runs")).toHaveLength(1);
    await expect(ptc.runs.start({ script: "x", idempotencyKey: "k" })).rejects.toMatchObject({ status: 503 });
    expect(server.calls("POST", "/v1/runs")).toHaveLength(4);
  });

  it("reports an unreachable server as PtcConnectionError", async () => {
    const unreachable = new Ptc({ url: "http://127.0.0.1:1", maxRetries: 0 });
    await expect(unreachable.runs.get(RUN_ID)).rejects.toBeInstanceOf(PtcConnectionError);
  });

  it("stops at once when the request is aborted", async () => {
    server.on("GET", "/v1/runs/{id}", async (request) => {
      await new Promise((resolve) => request.signal.addEventListener("abort", resolve));
      return json(run());
    });
    const controller = new AbortController();
    const pending = ptc.runs.get(RUN_ID, { wait: "30s", signal: controller.signal });
    setTimeout(() => controller.abort(new Error("enough")), 20);
    await expect(pending).rejects.toThrow("enough");
  });

  it("lists runs with repeated labels, and pages through them", async () => {
    server.on("GET", "/v1/runs", (request) =>
      json(
        request.query.get("cursor") === null
          ? { runs: [summary("r1"), summary("r2")], nextCursor: "c2" }
          : { runs: [summary("r3")] },
      ),
    );
    const all: string[] = [];
    for await (const item of ptc.runs.listAll({
      labels: { agent: "triage", conversation: "c,42" },
      status: "failed",
      startedAfter: new Date("2026-11-01T00:00:00Z"),
      quarantined: false,
    })) {
      all.push(item.id);
    }
    expect(all).toEqual(["r1", "r2", "r3"]);
    const first = server.requests[0];
    expect(first?.query.getAll("label")).toEqual(["agent:triage", "conversation:c,42"]);
    expect(first?.query.get("status")).toBe("failed");
    expect(first?.query.get("startedAfter")).toBe("2026-11-01T00:00:00.000Z");
    expect(first?.query.get("quarantined")).toBe("false");
    expect(server.requests[1]?.query.get("cursor")).toBe("c2");
  });

  it("sends signals with their payload and idempotency key", async () => {
    server.on("POST", "/v1/runs/{id}/signals/{name}", (request) =>
      json({ duplicate: false, name: request.params["name"], runId: request.params["id"] }, 202),
    );
    const accepted = await ptc.runs.signal(RUN_ID, "approval/1", { close: true }, { idempotencyKey: "sig-1" });
    expect(accepted).toEqual({ duplicate: false, name: "approval/1", runId: RUN_ID });
    await ptc.runs.signal(RUN_ID, "ping");
    expect(server.requests[0]?.path).toBe(`/v1/runs/${RUN_ID}/signals/approval%2F1`);
    expect(server.requests[0]?.body).toEqual({ close: true });
    expect(server.requests[0]?.headers["idempotency-key"]).toBe("sig-1");
    expect(server.requests[1]?.rawBody).toBe("null");
  });

  it("refuses a signal to a run that has ended", async () => {
    server.on("POST", "/v1/runs/{id}/signals/{name}", problem(409, "run-ended", "the run has ended"));
    const error = await ptc.runs.signal(RUN_ID, "approval", true).catch((e: unknown) => e);
    expect(isProblem(error, "run-ended")).toBe(true);
  });

  it("cancels, forks, releases, gives up and deletes runs", async () => {
    server
      .on("POST", "/v1/runs/{id}/cancel", json(run({ status: "cancelled" }), 202))
      .on("POST", "/v1/runs/{id}/fork", json(run({ id: "fork-1", parent: { run: RUN_ID, at: 5 } }), 201))
      .on("POST", "/v1/runs/{id}/release", json(run({ status: "active" })))
      .on("POST", "/v1/runs/{id}/give-up", json(run({ status: "aborted" })))
      .on("PUT", "/v1/runs/{id}/credentials", noContent)
      .on("DELETE", "/v1/runs/{id}", noContent);
    expect((await ptc.runs.cancel(RUN_ID, { reason: "not needed", wait: "5s" })).status).toBe("cancelled");
    expect(server.requests[0]?.body).toEqual({ reason: "not needed", wait: "5s" });

    const fork = await ptc.runs.fork(RUN_ID, {
      script: "fixed",
      at: 5,
      idempotencyKey: "fork-key",
      labels: { a: "b" },
    });
    expect(fork.parent).toEqual({ run: RUN_ID, at: 5 });
    expect(server.requests[1]?.body).toEqual({ script: "fixed", at: 5, labels: { a: "b" } });
    expect(server.requests[1]?.headers["idempotency-key"]).toBe("fork-key");

    await ptc.runs.release(RUN_ID);
    await ptc.runs.giveUp(RUN_ID);
    await ptc.runs.setCredentials(RUN_ID, { servers: {} });
    await ptc.runs.delete(RUN_ID);
    expect(server.requests.map((r) => `${r.method} ${r.path}`)).toEqual([
      `POST /v1/runs/${RUN_ID}/cancel`,
      `POST /v1/runs/${RUN_ID}/fork`,
      `POST /v1/runs/${RUN_ID}/release`,
      `POST /v1/runs/${RUN_ID}/give-up`,
      `PUT /v1/runs/${RUN_ID}/credentials`,
      `DELETE /v1/runs/${RUN_ID}`,
    ]);
  });

  it("reads the trace, an entry, the journal and a payload", async () => {
    server
      .on("GET", "/v1/runs/{id}/trace", (r) => json({ format: r.query.get("format"), runId: RUN_ID, text: "trace" }))
      .on("GET", "/v1/runs/{id}/entries/{idx}", (r) =>
        json({
          idx: Number(r.params["idx"]),
          recordedAt: "2026-11-12T10:00:00Z",
          event: { type: "timer_fired", seq: 1 },
        }),
      )
      .on("GET", "/v1/runs/{id}/journal", (r) => json({ version: 1, kind: r.query.get("as") }))
      .on("GET", "/v1/payloads/{digest}", json({ big: true }));
    expect((await ptc.runs.trace(RUN_ID)).format).toBe("llm");
    expect((await ptc.runs.entry(RUN_ID, 7)).idx).toBe(7);
    expect(await ptc.runs.journal(RUN_ID, { as: "cassette" })).toEqual({ version: 1, kind: "cassette" });
    expect(await ptc.payloads.get("ab")).toEqual({ big: true });
  });

  it("checks and tests scripts, and reads the scripts executed", async () => {
    server
      .on("POST", "/v1/check", json({ ok: true, diagnostics: [] }))
      .on("POST", "/v1/test", json({ passed: true, status: "completed", calls: {}, failures: [], trace: "" }))
      .on("GET", "/v1/scripts", json({ scripts: [] }))
      .on("GET", "/v1/scripts/{digest}", (r) => json({ digest: r.params["digest"], source: "src" }));
    expect((await ptc.scripts.check({ script: "x", toolset: TOOLSET })).ok).toBe(true);
    expect(server.requests[0]?.body).toEqual({ script: "x", toolset: TOOLSET });
    expect((await ptc.scripts.test({ script: "x", signals: [{ name: "approval", when: "2h" }] })).passed).toBe(true);
    expect((await ptc.scripts.list({ labels: { agent: "a" } })).scripts).toEqual([]);
    expect(server.requests[2]?.query.getAll("label")).toEqual(["agent:a"]);
    expect((await ptc.scripts.get("dd")).source).toBe("src");
  });

  it("manages subscriptions, API keys and the server", async () => {
    server
      .on(
        "POST",
        "/v1/subscriptions",
        json({ id: "s1", url: "https://h", events: [], labels: {}, createdAt: "t", secret: "whsec_x" }, 201),
      )
      .on(
        "GET",
        "/v1/subscriptions",
        json({ subscriptions: [{ id: "s1", url: "https://h", events: [], labels: {}, createdAt: "t" }] }),
      )
      .on("DELETE", "/v1/subscriptions/{id}", noContent)
      .on(
        "POST",
        "/v1/api-keys",
        json({ id: "k1", key: "ptc_sk_new", role: "worker", tenant: "t", allowedLabels: {}, createdAt: "t" }, 201),
      )
      .on("DELETE", "/v1/api-keys/{id}", noContent)
      .on("GET", "/v1/tools/stats", json({ since: "a", until: "b", tools: [] }))
      .on("POST", "/v1/server/stop", { status: 202 });
    expect((await ptc.subscriptions.create({ url: "https://h", events: ["run.completed"] })).secret).toBe("whsec_x");
    expect(await ptc.subscriptions.list()).toHaveLength(1);
    await ptc.subscriptions.delete("s1");
    expect((await ptc.apiKeys.create({ role: "worker", tenant: "t" })).key).toBe("ptc_sk_new");
    await ptc.apiKeys.revoke("k1");
    await ptc.tools.stats({ tool: "github.list_issues", since: "2026-11-11T00:00:00Z" });
    expect(server.calls("GET", "/v1/tools/stats")[0]?.query.get("tool")).toBe("github.list_issues");
    await ptc.server.stop();
  });
});

function summary(id: string) {
  return {
    id,
    status: "failed" as const,
    errors: 0,
    historyLen: 3,
    labels: {},
    lastActivityAt: "2026-11-12T10:00:00Z",
    quarantined: false,
    script: "s",
    startedAt: "2026-11-12T10:00:00Z",
    toolCalls: 1,
    toolset: TOOLSET,
  };
}
