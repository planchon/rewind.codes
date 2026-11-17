import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { Rewind, RewindApiError, RewindConnectionError, RewindError, isProblem } from "../src/index.js";
import { RUN_ID, TOOLSET, VERSION, payload, run, toolset, workflow } from "./fixtures.js";
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
        json({ embedded: false, epochs: [1], roles: ["api"], version: "0.1.0", virtualTime: false }),
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
      json({ embedded: true, epochs: [1], roles: ["api", "worker"], version: "0.1.0", virtualTime: false }),
    );
    await rewind.server.info();
    await new Rewind({ url: server.url, apiKey: "" }).server.info();
    expect(server.requests[0]?.headers["authorization"]).toBe("Bearer rewind_sk_test");
    expect(server.requests[1]?.headers["authorization"]).toBeUndefined();
  });

  it("registers a toolset", async () => {
    const toolset = {
      id: TOOLSET,
      declarations: { dts: "declare module 'rewind:tools' {}", prompt: "github.list_issues({ repo }) → Issue[]" },
      servers: [{ name: "github", tools: 1, transport: "http" as const }],
      tools: [],
    };
    server.on("POST", "/v1/toolsets", json(toolset, 201));
    const created = await rewind.toolsets.create({
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
    expect((await rewind.toolsets.declarations(TOOLSET, { format: "prompt" })).format).toBe("prompt");
    await rewind.toolsets.search(TOOLSET, "close issue", { limit: 3 });
    await rewind.toolsets.setCredentials(TOOLSET, { servers: { github: { headers: { Authorization: "Bearer x" } } } });
    const search = server.calls("GET", `/v1/toolsets/${TOOLSET}/search`)[0];
    expect(search?.query.get("query")).toBe("close issue");
    expect(search?.query.get("limit")).toBe("3");
    expect(server.calls("PUT", `/v1/toolsets/${TOOLSET}/credentials`)[0]?.body).toEqual({
      servers: { github: { headers: { Authorization: "Bearer x" } } },
    });
  });

  it("starts a run with its idempotency key, labels, trace context and ttl", async () => {
    server.on("POST", "/v1/runs", json(run({ output: payload({ closed: 3 }) }), 201));
    const started = await rewind.runs.start({
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

  it("starts a run of a workflow, without a script or a toolset", async () => {
    const started = run({
      workflow: { name: "triage", version: VERSION },
      startedBy: { run: "0193a5c4-0000-7000-8000-000000000001", seq: 3, detached: true },
    });
    server.on("POST", "/v1/runs", json(started, 201));
    const answered = await rewind.runs.start({
      workflow: "triage@4be1c07a9d3e",
      input: { repo: "acme/api" },
      idempotencyKey: "start-1",
      labels: { agent: "triage" },
    });
    expect(answered.workflow).toEqual({ name: "triage", version: VERSION });
    expect(answered.startedBy).toEqual({ run: "0193a5c4-0000-7000-8000-000000000001", seq: 3, detached: true });
    expect(server.requests[0]?.body).toEqual({
      workflow: "triage@4be1c07a9d3e",
      input: { repo: "acme/api" },
      labels: { agent: "triage" },
    });

    server.on("POST", "/v1/runs", problem(422, "invalid-input", "the input of triage: repo is required"));
    const error = await rewind.runs.start({ workflow: "triage", input: {} }).catch((e: unknown) => e);
    expect(isProblem(error, "invalid-input")).toBe(true);
    expect((error as RewindApiError).detail).toBe("the input of triage: repo is required");
  });

  it("derives a toolset that pins workflows", async () => {
    const derived = toolset("ab".repeat(32), [workflow()]);
    server.on("POST", "/v1/toolsets", json(derived, 201));
    const created = await rewind.toolsets.create({ base: TOOLSET, workflows: ["triage"] });
    expect(created.workflows).toEqual([
      { name: "triage", version: VERSION, description: "Triage the stale issues of a repository, after approval." },
    ]);
    expect(server.requests[0]?.body).toEqual({ base: TOOLSET, workflows: ["triage"] });
  });

  it("refuses an idempotency key the server would refuse", async () => {
    await expect(rewind.runs.start({ script: "x", idempotencyKey: "café" })).rejects.toThrow(/idempotency key/);
    await expect(rewind.runs.start({ script: "x", idempotencyKey: "x".repeat(256) })).rejects.toThrow(RewindError);
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
    const ended = await rewind.runs.start({ script: "x", wait: "5s" });
    expect(ended.status).toBe("completed");
    expect(gets).toBe(3);
    const waits = server.calls("GET", `/v1/runs/${RUN_ID}`).map((r) => r.query.get("wait"));
    expect(waits.every((wait) => wait !== null && /^\d+(ms|s)/.test(wait))).toBe(true);
    expect(server.requests[0]?.body).toMatchObject({ wait: expect.stringMatching(/^(5s|4s\d+ms)$/) });
  });

  it("splits a long wait for a cancellation as for a start", async () => {
    let gets = 0;
    server.on("POST", "/v1/runs/{id}/cancel", json(run({ status: "active" }), 202)).on("GET", "/v1/runs/{id}", () => {
      gets += 1;
      return json(run({ status: gets < 2 ? "active" : "cancelled" }));
    });
    const cancelled = await rewind.runs.cancel(RUN_ID, { wait: "2m" });
    expect(cancelled.status).toBe("cancelled");
    expect(server.calls("POST", `/v1/runs/${RUN_ID}/cancel`)[0]?.body).toEqual({ wait: "50s" });
    const waits = server.calls("GET", `/v1/runs/${RUN_ID}`).map((r) => r.query.get("wait"));
    expect(waits).toHaveLength(2);
    expect(waits.every((wait) => wait !== null && /^(50s|1m\d)/.test(wait))).toBe(true);
  });

  it("returns a run still active when the wait passes", async () => {
    server
      .on("POST", "/v1/runs", json(run({ status: "active" }), 201))
      .on("GET", "/v1/runs/{id}", json(run({ status: "active" })));
    const started = await rewind.runs.start({ script: "x", wait: 30 });
    expect(started.status).toBe("active");
  });

  it("turns problems into RewindApiError, with the diagnostics of a refused script", async () => {
    const diagnostics = [
      {
        code: "TS2322",
        message: "Type 'number' is not assignable to type 'string'.",
        severity: "error",
        location: { line: 3, column: 28 },
      },
    ];
    server.on("POST", "/v1/runs", problem(422, "invalid-script", "the script does not type-check", { diagnostics }));
    const error = await rewind.runs.start({ script: "x" }).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(RewindApiError);
    const apiError = error as RewindApiError;
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
    await expect(rewind.runs.start({ script: "x" })).rejects.toMatchObject({ status: 503 });
    expect(server.calls("POST", "/v1/runs")).toHaveLength(1);
    await expect(rewind.runs.start({ script: "x", idempotencyKey: "k" })).rejects.toMatchObject({ status: 503 });
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

  it("lists the runs of a workflow, and the runs a run started", async () => {
    server.on(
      "GET",
      "/v1/runs",
      json({ runs: [{ ...summary("child-1"), workflow: "triage", startedBy: RUN_ID }], nextCursor: null }),
    );
    const page = await rewind.runs.list({ workflow: "triage", startedBy: RUN_ID });
    expect(page.runs[0]).toMatchObject({ workflow: "triage", startedBy: RUN_ID });
    expect(server.requests[0]?.query.get("workflow")).toBe("triage");
    expect(server.requests[0]?.query.get("startedBy")).toBe(RUN_ID);
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

  it("searches runs with a query, and reads their statistics and failure groups", async () => {
    server
      .on("GET", "/v1/runs", json({ runs: [summary("r1")] }))
      .on("GET", "/v1/runs/stats", json({ until: "2026-11-12T00:00:00Z", runs: 3 }))
      .on("GET", "/v1/runs/failures", json({ until: "2026-11-12T00:00:00Z", runs: 2, totalGroups: 1, groups: [] }));
    const page = await rewind.runs.list({ q: '@agent:triage -status:completed "rate limit"', sort: "wakes" });
    expect(page.runs.map((run) => run.id)).toEqual(["r1"]);
    expect(server.requests[0]?.query.get("q")).toBe('@agent:triage -status:completed "rate limit"');
    expect(server.requests[0]?.query.get("sort")).toBe("wakes");
    const stats = await rewind.runs.stats({
      q: "waiting:*",
      since: new Date("2026-11-11T00:00:00Z"),
      bucket: "1h",
      top: 5,
    });
    expect(stats.runs).toBe(3);
    const statsQuery = server.requests[1]?.query;
    expect(statsQuery?.get("since")).toBe("2026-11-11T00:00:00.000Z");
    expect(statsQuery?.get("bucket")).toBe("1h");
    expect(statsQuery?.get("top")).toBe("5");
    expect(statsQuery?.get("until")).toBeNull();
    const failures = await rewind.runs.failures({ q: "@env:prod", bucket: 3_600_000, limit: 10 });
    expect(failures.totalGroups).toBe(1);
    expect(server.requests[2]?.query.get("bucket")).toBe("1h");
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
      .on("POST", "/v1/runs/{id}/fork", json(run({ id: "fork-1", parent: { run: RUN_ID, at: 5 } }), 201))
      .on("POST", "/v1/runs/{id}/release", json(run({ status: "active" })))
      .on("POST", "/v1/runs/{id}/give-up", json(run({ status: "aborted" })))
      .on("PUT", "/v1/runs/{id}/credentials", noContent)
      .on("DELETE", "/v1/runs/{id}", noContent);
    expect((await rewind.runs.cancel(RUN_ID, { reason: "not needed", wait: "5s" })).status).toBe("cancelled");
    expect(server.requests[0]?.body).toEqual({ reason: "not needed", wait: "5s" });

    const fork = await rewind.runs.fork(RUN_ID, {
      script: "fixed",
      at: 5,
      idempotencyKey: "fork-key",
      labels: { a: "b" },
    });
    expect(fork.parent).toEqual({ run: RUN_ID, at: 5 });
    expect(server.requests[1]?.body).toEqual({ script: "fixed", at: 5, labels: { a: "b" } });
    expect(server.requests[1]?.headers["idempotency-key"]).toBe("fork-key");

    await rewind.runs.release(RUN_ID);
    await rewind.runs.giveUp(RUN_ID);
    await rewind.runs.setCredentials(RUN_ID, { servers: {} });
    await rewind.runs.delete(RUN_ID);
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
    expect((await rewind.runs.trace(RUN_ID)).format).toBe("llm");
    expect((await rewind.runs.entry(RUN_ID, 7)).idx).toBe(7);
    expect(await rewind.runs.journal(RUN_ID, { as: "cassette" })).toEqual({ version: 1, kind: "cassette" });
    expect(await rewind.payloads.get("ab")).toEqual({ big: true });
    expect(await rewind.payloads.get("ab", { run: RUN_ID })).toEqual({ big: true });
    const reads = server.calls("GET", "/v1/payloads/ab");
    expect(reads.map((read) => read.query.get("run"))).toEqual([null, RUN_ID]);
  });

  it("checks and tests scripts, and reads the scripts executed", async () => {
    server
      .on("POST", "/v1/check", json({ ok: true, diagnostics: [] }))
      .on(
        "POST",
        "/v1/test",
        json({
          passed: true,
          tests: [{ name: "waits", passed: true, status: "active", calls: {}, failures: [], trace: "" }],
        }),
      )
      .on("GET", "/v1/scripts", json({ scripts: [] }))
      .on("GET", "/v1/scripts/{digest}", (r) => json({ digest: r.params["digest"], source: "src" }));
    expect((await rewind.scripts.check({ script: "x", toolset: TOOLSET })).ok).toBe(true);
    expect(server.requests[0]?.body).toEqual({ script: "x", toolset: TOOLSET });
    const tests = [
      { name: "waits", signals: [{ name: "approval", when: "2h" }], expect: { status: "active" as const } },
      "9f".repeat(32),
    ];
    const tested = await rewind.scripts.test({ script: "x", tests, workflow: "triage" });
    expect(tested.passed).toBe(true);
    expect(tested.tests.map((t) => t.name)).toEqual(["waits"]);
    expect(server.requests[1]?.body).toEqual({ script: "x", tests, workflow: "triage" });
    expect((await rewind.scripts.list({ labels: { agent: "a" } })).scripts).toEqual([]);
    expect(server.requests[2]?.query.getAll("label")).toEqual(["agent:a"]);
    expect((await rewind.scripts.get("dd")).source).toBe("src");
  });

  it("saves workflows, lists them and reads their versions", async () => {
    const saved = workflow();
    const definition = {
      name: "triage",
      description: "Triage the stale issues of a repository, after approval.",
      toolset: TOOLSET,
      script: "export default async function main(input: { repo: string }) { return { closed: 0 }; }",
      input: saved.input,
      output: saved.output,
      signals: saved.signals,
    };
    server.on("POST", "/v1/workflows", json(saved, 201));
    expect(await rewind.workflows.create(definition)).toEqual(saved);
    expect(server.calls("POST", "/v1/workflows")[0]?.body).toEqual(definition);

    const older = workflow({ version: "77".repeat(32), createdAt: "2026-11-11T10:00:00Z" });
    server.on("GET", "/v1/workflows/{name}", (request) =>
      request.params["name"] === "triage"
        ? json({ name: "triage", versions: [saved, older] })
        : problem(404, "not-found", `no workflow ${request.params["name"]} for this API key`),
    );
    expect((await rewind.workflows.versions("triage")).map((w) => w.version)).toEqual([VERSION, older.version]);
    const missing = await rewind.workflows.versions("nope").catch((e: unknown) => e);
    expect(isProblem(missing, "not-found")).toBe(true);

    server.on("GET", "/v1/workflows", (request) =>
      request.query.get("after") === null
        ? json({ workflows: [saved], next: "triage" })
        : json({ workflows: [workflow({ name: "digest" })] }),
    );
    const page = await rewind.workflows.list({ limit: 1 });
    expect(page.next).toBe("triage");
    const names: string[] = [];
    for await (const w of rewind.workflows.listAll({ limit: 1 })) {
      names.push(w.name);
    }
    expect(names).toEqual(["triage", "digest"]);
    const lists = server.calls("GET", "/v1/workflows");
    expect(lists.map((r) => [r.query.get("after"), r.query.get("limit")])).toEqual([
      [null, "1"],
      [null, "1"],
      ["triage", "1"],
    ]);

    // The versions whose script it is: the script's digest goes in the query.
    await rewind.workflows.list({ script: saved.script });
    expect(server.calls("GET", "/v1/workflows").at(-1)?.query.get("script")).toBe(saved.script);
  });

  it("refuses a workflow whose script or schemas cannot be used", async () => {
    const definition = { name: "triage", toolset: TOOLSET, script: "x", input: {}, output: {} };
    server.on(
      "POST",
      "/v1/workflows",
      problem(422, "invalid-script", "the script does not type-check", {
        diagnostics: [{ code: "TS2322", message: "Type 'string' is not assignable", severity: "error" }],
      }),
    );
    const refused = await rewind.workflows.create(definition).catch((e: unknown) => e);
    expect(isProblem(refused, "invalid-script")).toBe(true);
    expect((refused as RewindApiError).diagnostics).toHaveLength(1);

    server.on("POST", "/v1/workflows", problem(422, "invalid-workflow", '"class" is a reserved word'));
    const invalid = await rewind.workflows.create({ ...definition, name: "class" }).catch((e: unknown) => e);
    expect(isProblem(invalid, "invalid-workflow")).toBe(true);
    expect((invalid as RewindApiError).detail).toBe('"class" is a reserved word');
    expect((invalid as RewindApiError).tests).toEqual([]);
  });

  it("saves a workflow through the gate of its suite, with the tests it retires and updates", async () => {
    const recorded = "9f".repeat(32);
    const definition = {
      name: "triage",
      toolset: TOOLSET,
      script: "x",
      input: {},
      output: {},
      parent: `triage@${VERSION}`,
      tests: [recorded, { from: recorded, expect: { status: "completed" as const, output: { closed: 0 } } }],
      update: [{ name: "closes once approved", reason: "the message names the repository", expect: { output: {} } }],
      retire: [{ names: ["asks before closing"], reason: "under 5 stale issues, it closes without asking" }],
    };
    const outcomes = [
      { name: "closes once approved", passed: true, status: "completed" as const, calls: {}, failures: [], trace: "" },
      {
        name: "closes nothing without approval",
        passed: false,
        problem: "failed" as const,
        status: "completed" as const,
        output: { closed: 1 },
        calls: { "github.close_issue": 1 },
        failures: ["github.close_issue was called 1 time, expected 0"],
        trace: "#0 run_started",
      },
    ];
    server.on("POST", "/v1/workflows", problem(422, "tests-failed", "1 test of 2 did not pass", { tests: outcomes }));
    const refused = await rewind.workflows.create(definition).catch((e: unknown) => e);
    expect(isProblem(refused, "tests-failed")).toBe(true);
    expect((refused as RewindApiError).tests).toEqual(outcomes);
    expect(server.calls("POST", "/v1/workflows")[0]?.body).toEqual(definition);

    const saved = workflow({
      parent: VERSION,
      tests: [{ name: "closes once approved", digest: "ab".repeat(32) }],
      retired: [{ name: "asks before closing", digest: "cd".repeat(32), reason: "deprecated" }],
      updated: [{ name: "closes once approved", from: "ef".repeat(32), to: "ab".repeat(32), reason: "out of date" }],
    });
    server.on("POST", "/v1/workflows", json(saved, 201));
    expect(await rewind.workflows.create(definition)).toEqual(saved);
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
    server.on("POST", "/v1/workflows/{name}/record", json(recorded));
    expect(await rewind.workflows.record("triage", { query: "status:completed", limit: 20 })).toEqual(recorded);
    await rewind.workflows.record("triage", { runs: [RUN_ID] });
    await rewind.workflows.record("triage");
    expect(server.calls("POST", "/v1/workflows/triage/record").map((r) => r.body)).toEqual([
      { query: "status:completed", limit: 20 },
      { runs: [RUN_ID] },
      {},
    ]);

    const test = {
      name: `recorded ${RUN_ID}`,
      input: { repo: "acme/api" },
      world: { cassette: { v: 1, matching: "by_args", calls: [], signals: [] } },
      expect: { status: "completed" },
      origin: { run: RUN_ID, version: VERSION, namespace: "alice" },
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
        json(
          { id: "k1", key: "rewind_sk_new", name: "w", role: "worker", tenant: "t", allowedLabels: {}, createdAt: "t" },
          201,
        ),
      )
      .on("DELETE", "/v1/api-keys/{id}", noContent)
      .on("GET", "/v1/tools/stats", json({ since: "a", until: "b", tools: [] }))
      .on("POST", "/v1/server/stop", { status: 202 });
    expect((await rewind.subscriptions.create({ url: "https://h", events: ["run.completed"] })).secret).toBe("whsec_x");
    expect(await rewind.subscriptions.list()).toHaveLength(1);
    await rewind.subscriptions.delete("s1");
    expect((await rewind.apiKeys.create({ name: "w", role: "worker", tenant: "t" })).key).toBe("rewind_sk_new");
    await rewind.apiKeys.revoke("k1");
    await rewind.tools.stats({ tool: "github.list_issues", since: "2026-11-11T00:00:00Z" });
    expect(server.calls("GET", "/v1/tools/stats")[0]?.query.get("tool")).toBe("github.list_issues");
    await rewind.server.stop();
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
