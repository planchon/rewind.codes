import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { Rewind, RewindApiError, RewindConnectionError, RewindError, isProblem } from "../src/index.js";
import {
  ACTION_SET,
  DIGEST,
  HANDLE,
  RUN_ID,
  WORKFLOW_ID,
  actionSet,
  payload,
  run,
  version,
  workflow,
} from "./fixtures.js";
import { MockServer, json, noContent, problem } from "./mock-server.js";

let server: MockServer;
let rewind: Rewind;

beforeEach(async () => {
  server = await MockServer.start();
  rewind = new Rewind({ url: server.url, apiKey: "rewind_sk_test" });
});

afterEach(async () => {
  await server.close();
});

describe("the client", () => {
  it("needs a URL", () => {
    const saved = process.env["REWIND_URL"];
    delete process.env["REWIND_URL"];
    try {
      expect(() => new Rewind()).toThrow(RewindError);
    } finally {
      if (saved !== undefined) {
        process.env["REWIND_URL"] = saved;
      }
    }
  });

  it("reads the URL and the key from the environment", async () => {
    process.env["REWIND_URL"] = `${server.url}/`;
    process.env["REWIND_API_KEY"] = "rewind_sk_env";
    try {
      const fromEnv = new Rewind();
      server.on(
        "GET",
        "/v1/server",
        json({
          embedded: false,
          epochs: [1],
          node: "node-1",
          partitions: 64,
          nodes: 2,
          version: "0.1.0",
          virtualTime: false,
        }),
      );
      const info = await fromEnv.server.info();
      expect(info.version).toBe("0.1.0");
      expect(fromEnv.url).toBe(server.url);
      expect(server.requests[0]?.headers["authorization"]).toBe("Bearer rewind_sk_env");
    } finally {
      delete process.env["REWIND_URL"];
      delete process.env["REWIND_API_KEY"];
    }
  });

  it("sends the API key as a bearer token, and no key when there is none", async () => {
    server.on(
      "GET",
      "/v1/server",
      json({
        embedded: true,
        epochs: [1],
        node: "node-1",
        partitions: 1,
        nodes: 1,
        version: "0.1.0",
        virtualTime: false,
      }),
    );
    await rewind.server.info();
    await new Rewind({ url: server.url, apiKey: "" }).server.info();
    expect(server.requests[0]?.headers["authorization"]).toBe("Bearer rewind_sk_test");
    expect(server.requests[1]?.headers["authorization"]).toBeUndefined();
  });

  it("registers an action set", async () => {
    server.on("POST", "/v1/action-sets", json(actionSet(ACTION_SET), 201));
    const created = await rewind.actionSets.create({
      mcp: [{ name: "github", url: "https://mcp.example.com/github" }],
      harness: [
        {
          name: "ask_user",
          description: "Ask the end user a question.",
          inputSchema: { type: "object", properties: { question: { type: "string" } }, required: ["question"] },
          policy: { idempotent: false, reconnectTimeout: "2m" },
        },
      ],
    });
    expect(created.id).toBe(ACTION_SET);
    expect(server.requests[0]?.body).toMatchObject({
      harness: [{ name: "ask_user", policy: { idempotent: false, reconnectTimeout: "2m" } }],
    });
    expect(server.requests[0]?.headers["content-type"]).toBe("application/json");
  });

  it("reads declarations and searches actions", async () => {
    server
      .on("GET", "/v1/action-sets/{id}/declarations", (r) =>
        json({ format: r.query.get("format") ?? "dts", text: "x" }),
      )
      .on("GET", "/v1/action-sets/{id}/search", json({ actions: [] }));
    expect((await rewind.actionSets.declarations(ACTION_SET, { format: "prompt" })).format).toBe("prompt");
    await rewind.actionSets.search(ACTION_SET, "close issue", { limit: 3 });
    const search = server.calls("GET", `/v1/action-sets/${ACTION_SET}/search`)[0];
    expect(search?.query.get("query")).toBe("close issue");
    expect(search?.query.get("limit")).toBe("3");
  });

  it("starts a run with its idempotency key, labels, trace context and ttl", async () => {
    server.on("POST", "/v1/runs", json(run({ output: payload({ closed: 3 }) }), 201));
    const started = await rewind.runs.start({
      actionSet: ACTION_SET,
      code: "export default async function main() { return { closed: 3 }; }",
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
      actionSet: ACTION_SET,
      code: "export default async function main() { return { closed: 3 }; }",
      input: { repo: "acme/api" },
      wait: "30s",
      labels: { agent: "triage", conversation: "c-42" },
      traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
      ttl: "7d",
    });
  });

  it("starts a run of a workflow, without code or an action set", async () => {
    const started = run({
      workflow: { id: WORKFLOW_ID, name: "triage", version: 3, digest: DIGEST },
      startedBy: {
        run: "0193a5c4-0000-7000-8000-000000000001",
        handle: "user_2fQ81/digest/06bqx3k9f2ms",
        seq: 3,
        detached: true,
      },
    });
    server.on("POST", "/v1/runs", json(started, 201));
    const answered = await rewind.runs.start({
      workflow: "triage@v3",
      input: { repo: "acme/api" },
      idempotencyKey: "start-1",
      labels: { agent: "triage" },
    });
    expect(answered.workflow).toEqual({ id: WORKFLOW_ID, name: "triage", version: 3, digest: DIGEST });
    expect(answered.startedBy).toEqual({
      run: "0193a5c4-0000-7000-8000-000000000001",
      handle: "user_2fQ81/digest/06bqx3k9f2ms",
      seq: 3,
      detached: true,
    });
    expect(server.requests[0]?.body).toEqual({
      workflow: "triage@v3",
      input: { repo: "acme/api" },
      labels: { agent: "triage" },
    });

    server.on("POST", "/v1/runs", problem(422, "invalid-input", "the input of triage: repo is required"));
    const error = await rewind.runs.start({ workflow: "triage", input: {} }).catch((e: unknown) => e);
    expect(isProblem(error, "invalid-input")).toBe(true);
    expect((error as RewindApiError).detail).toBe("the input of triage: repo is required");
  });

  it("derives an action set that pins workflows", async () => {
    const derived = actionSet("ab".repeat(32), [workflow()]);
    server.on("POST", "/v1/action-sets", json(derived, 201));
    const created = await rewind.actionSets.create({ base: ACTION_SET, workflows: ["triage"] });
    expect(created.workflows).toEqual([
      {
        name: "triage",
        id: WORKFLOW_ID,
        version: 3,
        digest: DIGEST,
        description: "Triage the stale issues of a repository, after approval.",
      },
    ]);
    expect(server.requests[0]?.body).toEqual({ base: ACTION_SET, workflows: ["triage"] });
  });

  it("refuses an idempotency key the server would refuse", async () => {
    await expect(rewind.runs.start({ code: "x", idempotencyKey: "café" })).rejects.toThrow(/idempotency key/);
    await expect(rewind.runs.start({ code: "x", idempotencyKey: "x".repeat(256) })).rejects.toThrow(RewindError);
    expect(server.requests).toHaveLength(0);
  });

  it("waits with one request the server holds until the run ends, however long the wait", async () => {
    server.on("POST", "/v1/runs", json(run({ status: "completed", output: payload(1) }), 201));
    const ended = await rewind.runs.start({ code: "x", wait: "2h" });
    expect(ended.status).toBe("completed");
    expect(server.requests).toHaveLength(1);
    expect(server.requests[0]?.body).toMatchObject({ wait: "2h" });
    // A start that waits carries a key, so that a cut connection may be sent again.
    expect(server.requests[0]?.headers["idempotency-key"]).toMatch(/^[0-9a-f-]{36}$/);

    server.on("POST", "/v1/runs/{id}/cancel", json(run({ status: "cancelled" }), 202));
    const cancelled = await rewind.runs.cancel(RUN_ID, { wait: "10m" });
    expect(cancelled.status).toBe("cancelled");
    expect(server.calls("POST", `/v1/runs/${RUN_ID}/cancel`)[0]?.body).toEqual({ wait: "10m" });
    expect(server.calls("GET", `/v1/runs/${RUN_ID}`)).toHaveLength(0);
  });

  it("returns the run as the server answered it, still waiting when the wait passed, without asking again", async () => {
    server
      .on("POST", "/v1/runs", json(run({ status: "active" }), 201))
      .on("GET", "/v1/runs/{id}", json(run({ status: "active" })));
    const started = await rewind.runs.start({ code: "x", wait: 30 });
    expect(started.status).toBe("active");
    const read = await rewind.runs.get(RUN_ID, { wait: "1h" });
    expect(read.status).toBe("active");
    expect(server.requests).toHaveLength(2);
  });

  it("sends a held request again with the time left when its connection is cut", async () => {
    let gets = 0;
    server.on("GET", "/v1/runs/{id}", (request) => {
      gets += 1;
      if (gets === 1) {
        // A proxy cuts the connection while the server holds the request.
        return (response) =>
          new Promise<void>((resolve) =>
            setTimeout(() => {
              response.destroy();
              resolve();
            }, 1_100),
          );
      }
      return json(run({ status: "completed", labels: { wait: request.query.get("wait") ?? "" } }));
    });
    const ended = await rewind.runs.get(RUN_ID, { wait: "1m" });
    expect(ended.status).toBe("completed");
    const waits = server.calls("GET", `/v1/runs/${RUN_ID}`).map((r) => r.query.get("wait"));
    expect(waits).toHaveLength(2);
    expect(waits[0]).toBe("1m");
    expect(waits[1]).toMatch(/^58s\d+ms$/);

    // Cut before the server held it: sent again after a backoff, `maxRetries` times, then thrown.
    const once = new Rewind({ url: server.url, maxRetries: 1 });
    server.on("GET", "/v1/runs/{id}", () => (response) => {
      response.destroy();
    });
    await expect(once.runs.get(RUN_ID, { wait: "30s" })).rejects.toBeInstanceOf(RewindConnectionError);
    expect(server.calls("GET", `/v1/runs/${RUN_ID}`)).toHaveLength(4);
  });

  it("sends a start that waits again with its key after a cut, and a cancellation as a read", async () => {
    let posts = 0;
    server.on("POST", "/v1/runs", () => {
      posts += 1;
      return posts === 1
        ? (response) => new Promise<void>((resolve) => setTimeout(() => (response.destroy(), resolve()), 1_100))
        : json(run({ status: "completed" }));
    });
    expect((await rewind.runs.start({ code: "x", wait: "1m", idempotencyKey: "k-1" })).status).toBe("completed");
    expect(server.calls("POST", "/v1/runs").map((r) => r.headers["idempotency-key"])).toEqual(["k-1", "k-1"]);

    server
      .on(
        "POST",
        "/v1/runs/{id}/cancel",
        () => (response) => new Promise<void>((resolve) => setTimeout(() => (response.destroy(), resolve()), 1_100)),
      )
      .on("GET", "/v1/runs/{id}", json(run({ status: "cancelled" })));
    expect((await rewind.runs.cancel(RUN_ID, { wait: "1m" })).status).toBe("cancelled");
    expect(server.calls("POST", `/v1/runs/${RUN_ID}/cancel`)).toHaveLength(1);
    expect(server.calls("GET", `/v1/runs/${RUN_ID}`)[0]?.query.get("wait")).toMatch(/^58s\d+ms$/);
  });

  it("turns problems into RewindApiError, with the diagnostics of refused code", async () => {
    const diagnostics = [
      {
        code: "TS2322",
        message: "Type 'number' is not assignable to type 'string'.",
        severity: "error",
        location: { line: 3, column: 28 },
      },
    ];
    server.on("POST", "/v1/runs", problem(422, "invalid-code", "the code does not type-check", { diagnostics }));
    const error = await rewind.runs.start({ code: "x" }).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(RewindApiError);
    const apiError = error as RewindApiError;
    expect(apiError.status).toBe(422);
    expect(apiError.slug).toBe("invalid-code");
    expect(apiError.diagnostics).toEqual(diagnostics);
    expect(apiError.message).toBe("the code does not type-check");
    expect(isProblem(apiError, "invalid-code")).toBe(true);
  });

  it("reads an error body that is not a problem as about:blank", async () => {
    server.on("GET", "/v1/runs/{id}", () => (response) => {
      response.writeHead(502, { "Content-Type": "text/plain" });
      response.end("bad gateway");
    });
    const error = (await rewind.runs.get(RUN_ID).catch((e: unknown) => e)) as RewindApiError;
    expect(error).toBeInstanceOf(RewindApiError);
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
    expect((await rewind.runs.get(RUN_ID)).id).toBe(RUN_ID);
    expect(attempts).toBe(2);

    server.on("POST", "/v1/runs", problem(503, "unavailable", "stopping"));
    await expect(rewind.runs.start({ code: "x" })).rejects.toMatchObject({ status: 503 });
    expect(server.calls("POST", "/v1/runs")).toHaveLength(1);
    await expect(rewind.runs.start({ code: "x", idempotencyKey: "k" })).rejects.toMatchObject({ status: 503 });
    expect(server.calls("POST", "/v1/runs")).toHaveLength(4);
  });

  it("reports an unreachable server as RewindConnectionError", async () => {
    const unreachable = new Rewind({ url: "http://127.0.0.1:1", maxRetries: 0 });
    await expect(unreachable.runs.get(RUN_ID)).rejects.toBeInstanceOf(RewindConnectionError);
  });

  it("stops at once when the request is aborted", async () => {
    server.on("GET", "/v1/runs/{id}", async (request) => {
      await new Promise((resolve) => request.signal.addEventListener("abort", resolve));
      return json(run());
    });
    const controller = new AbortController();
    const pending = rewind.runs.get(RUN_ID, { wait: "30s", signal: controller.signal });
    setTimeout(() => controller.abort(new Error("enough")), 20);
    await expect(pending).rejects.toThrow("enough");
  });

  it("lists the runs of a workflow, the runs a run started, and the forks of a run", async () => {
    const child = { id: WORKFLOW_ID, name: "triage", version: 3, digest: DIGEST };
    server.on(
      "GET",
      "/v1/runs",
      json({ runs: [{ ...summary("child-1"), workflow: child, startedBy: RUN_ID }], nextCursor: null }),
    );
    const page = await rewind.runs.list({ workflow: "triage", startedBy: RUN_ID, source: HANDLE, action: "github.x" });
    expect(page.runs[0]).toMatchObject({ workflow: child, startedBy: RUN_ID });
    expect(server.requests[0]?.query.get("workflow")).toBe("triage");
    expect(server.requests[0]?.query.get("startedBy")).toBe(RUN_ID);
    expect(server.requests[0]?.query.get("source")).toBe(HANDLE);
    expect(server.requests[0]?.query.get("action")).toBe("github.x");
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
    for await (const item of rewind.runs.listAll({
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

  it("searches runs with a query, and reads their statistics and fingerprints", async () => {
    server
      .on("GET", "/v1/runs", json({ runs: [summary("r1")] }))
      .on("GET", "/v1/runs/stats", json({ until: "2026-11-12T00:00:00Z", runs: 3 }))
      .on(
        "GET",
        "/v1/runs/failures",
        json({ until: "2026-11-12T00:00:00Z", runs: 2, totalFingerprints: 1, fingerprints: [] }),
      );
    const page = await rewind.runs.list({ q: '@agent:triage -status:completed "rate limit"', sort: "wakes" });
    expect(page.runs.map((run) => run.id)).toEqual(["r1"]);
    expect(server.requests[0]?.query.get("q")).toBe('@agent:triage -status:completed "rate limit"');
    expect(server.requests[0]?.query.get("sort")).toBe("wakes");
    const stats = await rewind.runs.stats({
      q: "namespace:alice",
      since: new Date("2026-11-11T00:00:00Z"),
      interval: "1h",
      top: 5,
    });
    expect(stats.runs).toBe(3);
    const statsQuery = server.requests[1]?.query;
    expect(statsQuery?.get("since")).toBe("2026-11-11T00:00:00.000Z");
    expect(statsQuery?.get("interval")).toBe("1h");
    expect(statsQuery?.get("top")).toBe("5");
    expect(statsQuery?.get("until")).toBeNull();
    const failures = await rewind.runs.failures({ q: "namespace:alice", interval: 3_600_000, limit: 10 });
    expect(failures.totalFingerprints).toBe(1);
    expect(server.requests[2]?.query.get("interval")).toBe("1h");
    expect(server.requests[2]?.query.get("limit")).toBe("10");
  });

  it("sends signals with their payload and idempotency key", async () => {
    server.on("POST", "/v1/runs/{id}/signals/{name}", (request) =>
      json({ duplicate: false, name: request.params["name"], runId: request.params["id"] }, 202),
    );
    const accepted = await rewind.runs.signal(RUN_ID, "approval/1", { close: true }, { idempotencyKey: "sig-1" });
    expect(accepted).toEqual({ duplicate: false, name: "approval/1", runId: RUN_ID });
    await rewind.runs.signal(RUN_ID, "ping");
    expect(server.requests[0]?.path).toBe(`/v1/runs/${RUN_ID}/signals/approval%2F1`);
    expect(server.requests[0]?.body).toEqual({ close: true });
    expect(server.requests[0]?.headers["idempotency-key"]).toBe("sig-1");
    expect(server.requests[1]?.rawBody).toBe("null");
  });

  it("refuses a signal to a run that has ended", async () => {
    server.on("POST", "/v1/runs/{id}/signals/{name}", problem(409, "run-ended", "the run has ended"));
    const error = await rewind.runs.signal(RUN_ID, "approval", true).catch((e: unknown) => e);
    expect(isProblem(error, "run-ended")).toBe(true);
  });

  it("refuses a signal the run's workflow does not declare", async () => {
    const detail = `run ${RUN_ID} is a run of triage, which does not declare the signal "ping" (it declares approval)`;
    server.on("POST", "/v1/runs/{id}/signals/{name}", problem(422, "invalid-signal", detail));
    const error = await rewind.runs.signal(RUN_ID, "ping").catch((e: unknown) => e);
    expect(isProblem(error, "invalid-signal")).toBe(true);
    expect((error as RewindApiError).detail).toBe(detail);
  });

  it("cancels, forks, releases, gives up and deletes runs", async () => {
    server
      .on("POST", "/v1/runs/{id}/cancel", json(run({ status: "cancelled" }), 202))
      .on(
        "POST",
        "/v1/runs/{id}/fork",
        json(run({ id: "fork-1", source: { run: RUN_ID, handle: HANDLE, at: 5 } }), 201),
      )
      .on("POST", "/v1/runs/{id}/release", json(run({ status: "active" })))
      .on("POST", "/v1/runs/{id}/give-up", json(run({ status: "aborted" })))
      .on("DELETE", "/v1/runs/{id}", noContent);
    expect((await rewind.runs.cancel(RUN_ID, { reason: "not needed", wait: "5s" })).status).toBe("cancelled");
    expect(server.requests[0]?.body).toEqual({ reason: "not needed", wait: "5s" });

    const fork = await rewind.runs.fork(RUN_ID, {
      code: "fixed",
      at: 5,
      idempotencyKey: "fork-key",
      labels: { a: "b" },
    });
    expect(fork.source).toEqual({ run: RUN_ID, handle: HANDLE, at: 5 });
    expect(server.requests[1]?.body).toEqual({ code: "fixed", at: 5, labels: { a: "b" } });
    expect(server.requests[1]?.headers["idempotency-key"]).toBe("fork-key");

    await rewind.runs.release(RUN_ID);
    await rewind.runs.giveUp(RUN_ID);
    await rewind.runs.delete(RUN_ID);
    expect(server.requests.map((r) => `${r.method} ${r.path}`)).toEqual([
      `POST /v1/runs/${RUN_ID}/cancel`,
      `POST /v1/runs/${RUN_ID}/fork`,
      `POST /v1/runs/${RUN_ID}/release`,
      `POST /v1/runs/${RUN_ID}/give-up`,
      `DELETE /v1/runs/${RUN_ID}`,
    ]);
  });

  it("names a run by its handle wherever it takes its id, percent-encoded in the path", async () => {
    server
      .on("GET", "/v1/runs/{id}", (r) => json(run({ id: RUN_ID, handle: r.params["id"]! })))
      .on("POST", "/v1/runs/{id}/signals/{name}", json({ runId: RUN_ID, name: "approval", duplicate: false }, 202))
      .on("POST", "/v1/runs/{id}/cancel", json(run({ status: "cancelled" }), 202))
      .on("POST", "/v1/runs/{id}/fork", json(run({ id: "fork-1", source: { run: RUN_ID, handle: HANDLE } }), 201))
      .on("GET", "/v1/runs/{id}/transcript", json({ runId: RUN_ID, text: "transcript" }))
      .on(
        "GET",
        "/v1/runs/{id}/events/{idx}",
        json({ idx: 1, recordedAt: "t", event: { type: "timer_fired", seq: 1 } }),
      )
      .on("GET", "/v1/runs/{id}/export", json({ v: 1 }))
      .on("POST", "/v1/runs/{id}/release", json(run({ status: "active" })))
      .on("POST", "/v1/runs/{id}/give-up", json(run({ status: "aborted" })))
      .on("DELETE", "/v1/runs/{id}", noContent);
    expect((await rewind.runs.get(HANDLE)).handle).toBe(HANDLE);
    await rewind.runs.signal(HANDLE, "approval");
    await rewind.runs.cancel(HANDLE);
    await rewind.runs.fork(HANDLE, { code: "fixed" });
    await rewind.runs.transcript(HANDLE);
    await rewind.runs.event(HANDLE, 1);
    await rewind.runs.export(HANDLE);
    await rewind.runs.release(HANDLE);
    await rewind.runs.giveUp(HANDLE);
    await rewind.runs.delete(HANDLE);
    const encoded = encodeURIComponent(HANDLE);
    expect(encoded).toBe("user_2fQ81%2Ftriage%2F06bqx3k9f2mt");
    expect(server.requests.map((r) => `${r.method} ${r.path}`)).toEqual([
      `GET /v1/runs/${encoded}`,
      `POST /v1/runs/${encoded}/signals/approval`,
      `POST /v1/runs/${encoded}/cancel`,
      `POST /v1/runs/${encoded}/fork`,
      `GET /v1/runs/${encoded}/transcript`,
      `GET /v1/runs/${encoded}/events/1`,
      `GET /v1/runs/${encoded}/export`,
      `POST /v1/runs/${encoded}/release`,
      `POST /v1/runs/${encoded}/give-up`,
      `DELETE /v1/runs/${encoded}`,
    ]);
  });

  it("reads the transcript, an event, the export and a payload", async () => {
    server
      .on("GET", "/v1/runs/{id}/transcript", (r) => json({ runId: RUN_ID, text: `transcript ${r.query.size}` }))
      .on("GET", "/v1/runs/{id}/events/{idx}", (r) =>
        json({
          idx: Number(r.params["idx"]),
          recordedAt: "2026-11-12T10:00:00Z",
          event: { type: "timer_fired", seq: 1 },
        }),
      )
      .on("GET", "/v1/runs/{id}/export", (r) => json({ v: 1, kind: r.query.get("as") }))
      .on("GET", "/v1/payloads/{digest}", json({ big: true }));
    expect(await rewind.runs.transcript(RUN_ID)).toEqual({ runId: RUN_ID, text: "transcript 0" });
    expect((await rewind.runs.event(RUN_ID, 7)).idx).toBe(7);
    expect(await rewind.runs.export(RUN_ID, { as: "cassette" })).toEqual({ v: 1, kind: "cassette" });
    expect(await rewind.runs.export(RUN_ID)).toEqual({ v: 1, kind: null });
    // A payload is read through the run that holds it.
    expect(await rewind.payloads.get("ab", { run: RUN_ID })).toEqual({ big: true });
    const reads = server.calls("GET", "/v1/payloads/ab");
    expect(reads.map((read) => read.query.get("run"))).toEqual([RUN_ID]);
  });

  it("checks and tests code, without creating anything", async () => {
    const found = { id: WORKFLOW_ID, name: "triage", version: 3, digest: DIGEST };
    server.on("POST", "/v1/check", json({ ok: true, diagnostics: [], workflow: found })).on(
      "POST",
      "/v1/test",
      json({
        passed: true,
        tests: [{ name: "waits", passed: true, status: "active", calls: {}, failures: [], transcript: "" }],
      }),
    );
    const checked = await rewind.workflows.check({ code: "x", actionSet: ACTION_SET });
    expect(checked.ok).toBe(true);
    expect(checked.workflow).toEqual(found);
    expect(server.requests[0]?.body).toEqual({ code: "x", actionSet: ACTION_SET });
    const tests = [
      { name: "waits", signals: [{ name: "approval", when: "2h" }], expect: { status: "active" as const } },
      "9f".repeat(32),
      { test: "9f".repeat(32), expect: { status: "completed" as const } },
    ];
    const tested = await rewind.workflows.test({ code: "x", tests, workflow: "triage@v3" });
    expect(tested.passed).toBe(true);
    expect(tested.tests.map((t) => t.name)).toEqual(["waits"]);
    expect(server.requests[1]?.body).toEqual({ code: "x", tests, workflow: "triage@v3" });
  });

  it("creates a workflow, in a namespace or the organization's, and refuses a name taken", async () => {
    const created = workflow();
    const definition = {
      name: "triage",
      description: "Triage the stale issues of a repository, after approval.",
      actionSet: ACTION_SET,
      code: "export default async function main(input: { repo: string }) { return { closed: 0 }; }",
      input: created.version.input,
      output: created.version.output,
      signals: created.version.signals,
    };
    server.on("POST", "/v1/workflows", json(created, 201));
    expect(await rewind.workflows.create(definition)).toEqual(created);
    await rewind.namespace("alice").workflows.create(definition);
    const posts = server.calls("POST", "/v1/workflows");
    expect(posts.map((r) => r.body)).toEqual([definition, definition]);
    expect(posts.map((r) => r.headers["rewind-namespace"])).toEqual([undefined, "alice"]);

    server.on("POST", "/v1/workflows", problem(409, "name-taken", "the organization already has a workflow triage"));
    const taken = await rewind.workflows.create(definition).catch((e: unknown) => e);
    expect(isProblem(taken, "name-taken")).toBe(true);
    expect((taken as RewindApiError).status).toBe(409);
  });

  it("refuses a workflow whose code or schemas cannot be used", async () => {
    const definition = { name: "triage", actionSet: ACTION_SET, code: "x", input: {}, output: {} };
    server.on(
      "POST",
      "/v1/workflows",
      problem(422, "invalid-code", "the code does not type-check", {
        diagnostics: [{ code: "TS2322", message: "Type 'string' is not assignable", severity: "error" }],
      }),
    );
    const refused = await rewind.workflows.create(definition).catch((e: unknown) => e);
    expect(isProblem(refused, "invalid-code")).toBe(true);
    expect((refused as RewindApiError).diagnostics).toHaveLength(1);

    server.on("POST", "/v1/workflows", problem(422, "invalid-workflow", '"class" is a reserved word'));
    const invalid = await rewind.workflows.create({ ...definition, name: "class" }).catch((e: unknown) => e);
    expect(isProblem(invalid, "invalid-workflow")).toBe(true);
    expect((invalid as RewindApiError).detail).toBe('"class" is a reserved word');
    expect((invalid as RewindApiError).tests).toEqual([]);
  });

  it("reads a workflow by its id, its name or one version, and lists them", async () => {
    const latest = workflow();
    const older = version({ version: 2, digest: "77".repeat(32), createdAt: "2026-11-11T10:00:00Z" });
    server.on("GET", "/v1/workflows/{id}", (request) =>
      request.params["id"] === "nope"
        ? problem(404, "not-found", "no workflow nope in this namespace or the organization")
        : json({ ...latest, versions: [latest.version, older] }),
    );
    const read = await rewind.workflows.get("triage");
    expect(read.versions.map((v) => v.version)).toEqual([3, 2]);
    await rewind.workflows.get(WORKFLOW_ID);
    await rewind.workflows.get("triage@v2");
    expect(server.requests.map((r) => r.path)).toEqual([
      "/v1/workflows/triage",
      `/v1/workflows/${WORKFLOW_ID}`,
      "/v1/workflows/triage%40v2",
    ]);
    const missing = await rewind.workflows.get("nope").catch((e: unknown) => e);
    expect(isProblem(missing, "not-found")).toBe(true);

    const listed = (id: string, name: string | null) => ({
      id,
      shortId: `wf-${id.slice(3, 9)}`,
      name,
      createdAt: "2026-11-12T10:00:00Z",
      latest: version(),
      runs: { pending: 0, active: 1, completed: 4, failed: 0, cancelled: 0, aborted: 0 },
      actions: ["github.list_issues"],
    });
    server.on("GET", "/v1/workflows", (request) =>
      request.query.get("after") === null
        ? json({ workflows: [listed(WORKFLOW_ID, "triage")], next: WORKFLOW_ID })
        : json({ workflows: [listed("wf_000000000001", null)] }),
    );
    const page = await rewind.workflows.list({ limit: 1, scope: "organization" });
    expect(page.next).toBe(WORKFLOW_ID);
    const ids: string[] = [];
    for await (const w of rewind.workflows.listAll({ limit: 1, code: "cd".repeat(32), q: "namespace:alice" })) {
      ids.push(w.id);
    }
    expect(ids).toEqual([WORKFLOW_ID, "wf_000000000001"]);
    const lists = server.calls("GET", "/v1/workflows");
    expect(lists.map((r) => [r.query.get("after"), r.query.get("limit"), r.query.get("scope")])).toEqual([
      [null, "1", "organization"],
      [null, "1", null],
      [WORKFLOW_ID, "1", null],
    ]);
    expect(lists[1]?.query.get("code")).toBe("cd".repeat(32));
    expect(lists[1]?.query.get("q")).toBe("namespace:alice");
  });

  it("names and describes a workflow, which keeps its id and versions", async () => {
    server.on("PATCH", "/v1/workflows/{id}", (request) => json(workflow(request.body as object)));
    const named = await rewind.workflows.update(WORKFLOW_ID, { name: "repo_triage", description: null });
    expect(named.name).toBe("repo_triage");
    expect(named.id).toBe(WORKFLOW_ID);
    expect(server.requests[0]?.method).toBe("PATCH");
    expect(server.requests[0]?.body).toEqual({ name: "repo_triage", description: null });
  });

  it("makes a version through the gate of its suite, with the tests it retires and replaces", async () => {
    const recorded = "9f".repeat(32);
    const next = {
      code: "x",
      tests: [recorded, { test: recorded, expect: { status: "completed" as const, output: { closed: 0 } } }],
      replace: [{ name: "closes once approved", reason: "the message names the repository", expect: { output: {} } }],
      retire: [{ names: ["asks before closing"], reason: "under 5 stale issues, it closes without asking" }],
    };
    const outcomes = [
      {
        name: "closes once approved",
        passed: true,
        status: "completed" as const,
        calls: {},
        failures: [],
        transcript: "",
      },
      {
        name: "closes nothing without approval",
        passed: false,
        problem: "failed" as const,
        status: "completed" as const,
        output: { closed: 1 },
        calls: { "github.close_issue": 1 },
        failures: ["github.close_issue was called 1 time, expected 0"],
        transcript: "#0 run_started",
      },
    ];
    server.on(
      "POST",
      "/v1/workflows/{id}/versions",
      problem(422, "tests-failed", "1 test of 2 did not pass", { tests: outcomes }),
    );
    const refused = await rewind.workflows.createVersion("triage", next).catch((e: unknown) => e);
    expect(isProblem(refused, "tests-failed")).toBe(true);
    expect((refused as RewindApiError).tests).toEqual(outcomes);
    expect(server.calls("POST", "/v1/workflows/triage/versions")[0]?.body).toEqual(next);

    const made = workflow({
      version: version({
        version: 4,
        base: DIGEST,
        tests: [{ name: "closes once approved", digest: "ab".repeat(32) }],
        retired: [{ name: "asks before closing", digest: "cd".repeat(32), reason: "deprecated" }],
        replaced: [{ name: "closes once approved", old: "ef".repeat(32), new: "ab".repeat(32), reason: "out of date" }],
      }),
    });
    server.on("POST", "/v1/workflows/{id}/versions", json(made, 201));
    expect((await rewind.workflows.createVersion("triage", next)).version.version).toBe(4);
  });

  it("adds tests to a suite without a version", async () => {
    server.on("POST", "/v1/workflows/{id}/tests", json(workflow()));
    const tests = { tests: ["9f".repeat(32)], retire: [{ names: ["old"], reason: "deprecated" }] };
    await rewind.workflows.addTests(WORKFLOW_ID, tests);
    expect(server.calls("POST", `/v1/workflows/${WORKFLOW_ID}/tests`)[0]?.body).toEqual(tests);
  });

  it("promotes a workflow, forks it into a namespace and pulls its source", async () => {
    const fork = workflow({ id: "wf_000000000002", namespace: "alice", source: { workflow: WORKFLOW_ID, version: 3 } });
    server
      .on("POST", "/v1/workflows/{id}/promote", json(workflow()))
      .on("POST", "/v1/workflows/{id}/fork", json(fork, 201))
      .on("POST", "/v1/workflows/{id}/pull", json(fork));
    expect((await rewind.workflows.promote(WORKFLOW_ID)).namespace).toBeUndefined();
    const alice = rewind.namespace("alice");
    expect((await alice.workflows.fork("triage@v3")).source).toEqual({ workflow: WORKFLOW_ID, version: 3 });
    await alice.workflows.pull(fork.id);
    expect(server.requests.map((r) => [`${r.method} ${r.path}`, r.headers["rewind-namespace"]])).toEqual([
      [`POST /v1/workflows/${WORKFLOW_ID}/promote`, undefined],
      ["POST /v1/workflows/triage%40v3/fork", "alice"],
      [`POST /v1/workflows/${fork.id}/pull`, "alice"],
    ]);

    server
      .on("POST", "/v1/workflows/{id}/fork", problem(409, "not-forkable", "a namespace's workflow is not forkable"))
      .on("POST", "/v1/workflows/{id}/pull", problem(409, "not-a-fork", "triage is not a fork"));
    expect(isProblem(await alice.workflows.fork("mine").catch((e: unknown) => e), "not-forkable")).toBe(true);
    expect(isProblem(await alice.workflows.pull("triage").catch((e: unknown) => e), "not-a-fork")).toBe(true);
  });

  it("records tests from the runs of a workflow, and reads a stored test", async () => {
    const recorded = {
      tests: [
        {
          digest: "9f".repeat(32),
          name: `recorded ${RUN_ID}`,
          path: "github.list_issues · signal approval · github.close_issue×n → completed",
          run: RUN_ID,
          status: "completed",
        },
      ],
      skipped: [{ run: "r-big", reason: "its test would be larger than 4 MiB" }],
    };
    server.on("POST", "/v1/workflows/{id}/record", json(recorded));
    expect(await rewind.workflows.record("triage", { query: "status:completed", limit: 20 })).toEqual(recorded);
    await rewind.workflows.record(WORKFLOW_ID, { runs: [RUN_ID] });
    await rewind.workflows.record("triage");
    expect(server.calls("POST", "/v1/workflows/triage/record").map((r) => r.body)).toEqual([
      { query: "status:completed", limit: 20 },
      {},
    ]);
    expect(server.calls("POST", `/v1/workflows/${WORKFLOW_ID}/record`)[0]?.body).toEqual({ runs: [RUN_ID] });

    const test = {
      name: `recorded ${RUN_ID}`,
      input: { repo: "acme/api" },
      world: { cassette: { v: 1, matching: "by_args", calls: [], signals: [] } },
      expect: { status: "completed" },
      from: { run: RUN_ID, version: DIGEST, namespace: "alice" },
    };
    server.on("GET", "/v1/tests/{digest}", (request) =>
      request.params["digest"] === "9f".repeat(32)
        ? json(test)
        : problem(403, "forbidden", "reading this test needs runs:read in the namespace it was recorded from"),
    );
    expect(await rewind.tests.get("9f".repeat(32))).toEqual(test);
    const forbidden = await rewind.tests.get("00".repeat(32)).catch((e: unknown) => e);
    expect(isProblem(forbidden, "forbidden")).toBe(true);
  });

  it("manages subscriptions, API keys and the server", async () => {
    server
      .on(
        "POST",
        "/v1/subscriptions",
        json({ id: "s1", url: "https://h", types: [], labels: {}, createdAt: "t", secret: "whsec_x" }, 201),
      )
      .on(
        "GET",
        "/v1/subscriptions",
        json({ subscriptions: [{ id: "s1", url: "https://h", types: [], labels: {}, createdAt: "t" }] }),
      )
      .on("DELETE", "/v1/subscriptions/{id}", noContent)
      .on(
        "POST",
        "/v1/api-keys",
        json(
          {
            id: "k1",
            key: "rewind_sk_new",
            name: "w",
            role: "worker",
            organizationId: "org_1",
            allowedLabels: {},
            createdAt: "t",
          },
          201,
        ),
      )
      .on("DELETE", "/v1/api-keys/{id}", noContent)
      .on("GET", "/v1/actions/stats", json({ since: "a", until: "b", actions: [] }))
      .on("POST", "/v1/server/stop", { status: 202 });
    expect(
      (await rewind.subscriptions.create({ url: "https://h", types: ["run.completed", "run.waiting"] })).secret,
    ).toBe("whsec_x");
    expect(server.calls("POST", "/v1/subscriptions")[0]?.body).toEqual({
      url: "https://h",
      types: ["run.completed", "run.waiting"],
    });
    expect(await rewind.subscriptions.list()).toHaveLength(1);
    await rewind.subscriptions.delete("s1");
    expect((await rewind.apiKeys.create({ name: "w", role: "worker", organizationId: "org_1" })).key).toBe(
      "rewind_sk_new",
    );
    await rewind.apiKeys.revoke("k1");
    await rewind.actions.stats({ q: "namespace:alice", group: "github", since: "2026-11-11T00:00:00Z" });
    const actionStats = server.calls("GET", "/v1/actions/stats")[0]?.query;
    expect(actionStats?.get("group")).toBe("github");
    expect(actionStats?.get("q")).toBe("namespace:alice");
    await rewind.actions.stats({ action: "github.close_issue", actionSet: ACTION_SET });
    expect(server.calls("GET", "/v1/actions/stats")[1]?.query.get("action")).toBe("github.close_issue");
    expect(server.calls("GET", "/v1/actions/stats")[1]?.query.get("actionSet")).toBe(ACTION_SET);
    await rewind.server.stop();
  });
});

function summary(id: string) {
  return {
    id,
    handle: `user_2fQ81/triage/${id}`,
    namespace: "user_2fQ81",
    status: "failed" as const,
    calls: 1,
    errors: 0,
    historyLen: 3,
    labels: {},
    lastActivityAt: "2026-11-12T10:00:00Z",
    quarantined: false,
    codeDigest: "ab".repeat(32),
    startedAt: "2026-11-12T10:00:00Z",
    actionSet: ACTION_SET,
  };
}
