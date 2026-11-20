import { createHash } from "node:crypto";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  AGENT_TOOL_NAMES,
  AgentTools as AgentToolsClass,
  NamespaceRequiredError,
  Rewind,
  RewindApiError,
  RewindError,
  type AgentSecrets,
  type AgentTools,
  type ListedSecret,
  type Run,
} from "../src/index.js";
import { NAMESPACE, RUN_ID, TOOLSET, VERSION, payload, run, toolset, workflow } from "./fixtures.js";
import { MockServer, json, problem, type MockHandler } from "./mock-server.js";

const LABELS = { conversation: "c-42" };

/** The key of the run an agent with `LABELS` starts for a tool call. */
const runKey = (toolCallId: string) =>
  createHash("sha256")
    .update(JSON.stringify([[["conversation", "c-42"]], toolCallId]))
    .digest("hex");
let server: MockServer;
let rewind: Rewind;
let tools: AgentTools;

beforeEach(async () => {
  server = await MockServer.start();
  rewind = new Rewind({ url: server.url, apiKey: "rewind_sk_agent" });
  tools = rewind.namespace(NAMESPACE).agentTools({ toolset: TOOLSET, labels: LABELS });
  server.on(
    "GET",
    "/v1/runs/{id}/trace",
    json({ format: "llm", runId: RUN_ID, text: "#0 run_started\n#1 tool github.get_issue → error" }),
  );
});

afterEach(async () => {
  await server.close();
});

/** A start or a fork answered as the server answers it: a run of the script it was sent. */
function started(overrides: Partial<Run> = {}): MockHandler {
  return (request) => {
    const script = (request.body as { script: string }).script;
    return json(run({ script: createHash("sha256").update(script).digest("hex"), ...overrides }), 201);
  };
}

function serveRun(state: Run): void {
  server.on("GET", "/v1/runs/{id}", (request) =>
    request.params["id"] === state.id ? json(state) : problem(404, "not-found", `no run ${request.params["id"]}`),
  );
}

describe("the agent tools' definitions", () => {
  it("are all there with a toolset, and the tools of the toolset and its workflows only with one", () => {
    expect(tools.definitions.map((d) => d.name)).toEqual([...AGENT_TOOL_NAMES]);
    const withoutToolset = rewind.agentTools({ labels: LABELS });
    for (const name of ["search_tools", "save_workflow", "get_workflow", "record_tests"]) {
      expect(withoutToolset.has(name)).toBe(false);
    }
    expect(withoutToolset.has("run_script")).toBe(true);
    expect(withoutToolset.has("test_script")).toBe(true);
    expect(withoutToolset.has("list_secrets")).toBe(true);
    expect(() => rewind.agentTools({ tools: ["search_tools"] })).toThrow(TypeError);
    expect(() => rewind.agentTools({ tools: ["save_workflow"] })).toThrow("save_workflow needs a toolset");
    expect(() => rewind.agentTools({ tools: ["get_workflow"] })).toThrow("get_workflow needs a toolset");
    expect(() => rewind.agentTools({ tools: ["record_tests"] })).toThrow("record_tests needs a toolset");
    expect(() => rewind.agentTools({ tools: ["nope" as never] })).toThrow(TypeError);
    expect(rewind.agentTools({ tools: ["run_script", "get_run"] }).definitions.map((d) => d.name)).toEqual([
      "run_script",
      "get_run",
    ]);
  });

  it("come in the Anthropic and OpenAI formats", () => {
    const runScript = tools.definitions[0]!;
    expect(tools.anthropic.tools[0]).toEqual({
      name: "run_script",
      description: runScript.description,
      input_schema: runScript.inputSchema,
    });
    expect(tools.openai.tools[0]).toEqual({
      type: "function",
      function: { name: "run_script", description: runScript.description, parameters: runScript.inputSchema },
    });
    expect(tools.openai.responses.tools[0]).toEqual({
      type: "function",
      name: "run_script",
      description: runScript.description,
      parameters: runScript.inputSchema,
      strict: false,
    });
    for (const definition of tools.definitions) {
      expect(definition.inputSchema).toMatchObject({ type: "object", additionalProperties: false });
    }
  });

  it("describe the test document: its world, its signals and its expectations, or a recorded test", () => {
    const testScript = tools.definitions.find((d) => d.name === "test_script")!;
    expect(testScript.inputSchema.required).toEqual(["code"]);
    const tests = testScript.inputSchema.properties?.["tests"] as { items: { anyOf: Array<Record<string, any>> } };
    const [inline, digest, from] = tests.items.anyOf;
    expect(Object.keys(inline?.["properties"])).toEqual([
      "name",
      "input",
      "start",
      "seed",
      "world",
      "signals",
      "expect",
    ]);
    expect(Object.keys(inline?.["properties"].world.properties)).toEqual(["tools", "mocks"]);
    const rule = inline?.["properties"].world.properties.tools.additionalProperties.anyOf[0];
    expect(Object.keys(rule.properties)).toEqual(["args", "returns", "error", "times"]);
    expect(Object.keys(inline?.["properties"].signals.items.properties)).toEqual(["name", "payload", "when"]);
    const expectations = inline?.["properties"].expect;
    expect(Object.keys(expectations.properties)).toEqual(["status", "output", "error", "calls", "order"]);
    expect(Object.keys(expectations.properties.calls.items.properties)).toEqual(["tool", "args", "times"]);
    expect(digest).toMatchObject({ type: "string" });
    expect(from).toMatchObject({ type: "object", required: ["from", "expect"] });

    const save = tools.definitions.find((d) => d.name === "save_workflow")!;
    expect(Object.keys(save.inputSchema.properties ?? {})).toEqual([
      "name",
      "description",
      "code",
      "input",
      "output",
      "signals",
      "tests",
      "update",
      "retire",
      "parent",
      "secrets",
    ]);
    expect(save.description).toContain("saved only if every test passes");
    expect(save.description).toContain("retire the test when the behavior is deprecated");
    expect(save.description).toContain("update it when its state or its expected result is out of date");
  });
});

describe("run_script", () => {
  it("starts the run with the agent's labels and the tool call id, and returns its output and logs", async () => {
    server.on(
      "POST",
      "/v1/runs",
      started({
        output: payload({ closed: 3 }),
        logs: [{ at: "t", idx: 2, level: "info", message: "found 12 issues", callsite: { line: 4, column: 3 } }],
      }),
    );
    const result = await tools.handle(
      "run_script",
      { code: "export default async () => ({ closed: 3 })", input: { repo: "acme/api" } },
      { toolCallId: "toolu_01", traceparent: "00-aa-bb-01" },
    );
    expect(result.isError).toBe(false);
    expect(result.run?.id).toBe(RUN_ID);
    expect(result.content).toBe(
      [
        `Run ${RUN_ID} completed.`,
        "Output:",
        '{"closed":3}',
        "Last log lines:",
        "  [info] at script.ts:4:3 found 12 issues",
      ].join("\n"),
    );
    const request = server.calls("POST", "/v1/runs")[0];
    expect(request?.headers["idempotency-key"]).toBe(runKey("toolu_01"));
    expect(request?.body).toEqual({
      script: "export default async () => ({ closed: 3 })",
      toolset: TOOLSET,
      input: { repo: "acme/api" },
      wait: "30s",
      labels: LABELS,
      traceparent: "00-aa-bb-01",
    });
  });

  it("bounds the output and says how to read the rest", async () => {
    const small = rewind.agentTools({ toolset: TOOLSET, labels: LABELS, outputLimit: 32 });
    const output = { items: "é".repeat(40) };
    serveRun(run({ output: payload(output) }));
    server.on("POST", "/v1/runs", json(run({ output: payload(output) }), 201));
    const result = await small.handle("run_script", { code: "x" });
    const full = JSON.stringify(output);
    expect(result.content).toContain(`Output (${Buffer.byteLength(full)} bytes; the first 32 are shown`);
    expect(result.content).toContain(`get_run({"run_id":"${RUN_ID}","offset":32})`);

    let offset = 0;
    let text = "";
    for (;;) {
      const page = await small.handle("get_run", { run_id: RUN_ID, offset });
      const [, body = "", next] = page.content.split("\n");
      text += body;
      const match = next === undefined ? null : /"offset":(\d+)/.exec(next);
      if (match === null) {
        break;
      }
      offset = Number(match[1]);
    }
    expect(text).toBe(full);
  });

  it("reads an output too large to be inline from its payload", async () => {
    const big = run({ output: payload({ rows: [1, 2, 3] }, false) });
    server.on("POST", "/v1/runs", json(big, 201));
    server.on("GET", "/v1/payloads/{digest}", json({ rows: [1, 2, 3] }));
    const result = await tools.handle("run_script", { code: "x" });
    expect(result.content).toContain('{"rows":[1,2,3]}');
    const reads = server.calls("GET", `/v1/payloads/${big.output!.digest}`);
    expect(reads).toHaveLength(1);
    expect(reads[0]?.query.get("run")).toBe(big.id);
  });

  it("says what a suspended run waits for", async () => {
    const suspended = run({
      status: "active",
      waitingFor: [
        { kind: "signal", name: "approval", callsite: { line: 8, column: 26 }, timeoutAt: "2026-11-15T10:00:00Z" },
        {
          kind: "tool_call",
          seq: 4,
          tool: "github.delete_repo",
          taskId: "t1",
          state: "approval",
          attempt: 1,
          args: payload({}),
        },
        { kind: "timer", seq: 5, fireAt: "2026-11-20T00:00:00Z" },
      ],
    });
    serveRun(suspended);
    server.on("POST", "/v1/runs", json(suspended, 201));
    const result = await tools.handle("run_script", { code: "x", wait: "300ms" });
    expect(result.isError).toBe(false);
    expect(result.content).toBe(
      [
        `Run ${RUN_ID} is suspended, waiting for:`,
        '- the signal "approval" at script.ts:8:26, until 2026-11-15T10:00:00Z (then it gets undefined): send it with signal_run',
        "- the call 4 to github.delete_repo: it waits for the harness's approval",
        "- a timer, firing at 2026-11-20T00:00:00Z",
        `Follow it with get_run({"run_id":"${RUN_ID}"}), send it a signal with signal_run, or cancel it with cancel_run.`,
      ].join("\n"),
    );
    expect(server.requests[0]?.body).toMatchObject({ wait: expect.stringMatching(/^\d+ms$/) });
  });

  it("returns the error, the failed call and the trace of a failed run", async () => {
    server.on(
      "POST",
      "/v1/runs",
      json(
        run({
          status: "failed",
          error: {
            name: "ToolError",
            message: "Not Found",
            location: { line: 12, column: 9 },
            frame: "12 |   const issue = await github.get_issue({ number: 7 });\n   |         ^",
            failedCall: {
              tool: "github.get_issue",
              seq: 1,
              args: payload({ number: 7 }),
              callsite: { line: 12, column: 9 },
              error: { kind: "error", message: "Not Found" },
            },
          },
        }),
        201,
      ),
    );
    const result = await tools.handle("run_script", { code: "x" });
    expect(result.isError).toBe(true);
    expect(result.content).toBe(
      [
        `Run ${RUN_ID} failed.`,
        "ToolError: Not Found at script.ts:12:9",
        "12 |   const issue = await github.get_issue({ number: 7 });\n   |         ^",
        'The call that failed: github.get_issue (call 1 at script.ts:12:9) with {"number":7}',
        "  error: Not Found",
        "Trace up to the failure:",
        "#0 run_started\n#1 tool github.get_issue → error",
      ].join("\n"),
    );
  });

  it("returns the diagnostics of a refused script", async () => {
    server.on(
      "POST",
      "/v1/runs",
      problem(422, "invalid-script", "the script does not type-check", {
        diagnostics: [
          {
            code: "TS2322",
            message: "Type 'number' is not assignable to type 'string'.",
            severity: "error",
            location: { line: 3, column: 28 },
            frame: "3 | github.list_issues({ repo: 42 })\n  |                            ^^",
          },
          {
            code: "rewind/absent-global",
            message: "fetch does not exist",
            severity: "error",
            hint: "use the http tool",
          },
        ],
      }),
    );
    const result = await tools.handle("run_script", { code: "x" });
    expect(result.isError).toBe(true);
    expect(result.content).toBe(
      [
        "The script was refused before anything ran: the script does not type-check",
        "script.ts:3:28 error TS2322: Type 'number' is not assignable to type 'string'.",
        "3 | github.list_issues({ repo: 42 })\n  |                            ^^",
        "",
        "error rewind/absent-global: fetch does not exist",
        "hint: use the http tool",
      ].join("\n"),
    );
  });

  it("derives the idempotency key from the agent's labels and the tool call id", async () => {
    server.on("POST", "/v1/runs", started());
    const id = "call_é".padEnd(300, "x");
    await tools.handle("run_script", { code: "x" }, { toolCallId: id });
    expect(server.requests[0]?.headers["idempotency-key"]).toBe(runKey(id));
    // Two agents whose harnesses number their tool calls alike never share a key.
    server.on("POST", "/v1/runs", (request) => {
      const body = request.body as { script: string; labels: Run["labels"] };
      const script = createHash("sha256").update(body.script).digest("hex");
      return json(run({ script, labels: body.labels }), 201);
    });
    await rewind.agentTools({ labels: { agent: "a" } }).handle("run_script", { code: "x" }, { toolCallId: "call_0" });
    await rewind.agentTools({ labels: { agent: "b" } }).handle("run_script", { code: "x" }, { toolCallId: "call_0" });
    await rewind
      .agentTools({ labels: { b: "2", a: "1" } })
      .handle("run_script", { code: "x" }, { toolCallId: "call_0" });
    // The same vectors as the Rust port (`crates/rewind-agent-tools/src/keys.rs`).
    expect(server.requests.slice(1).map((request) => request.headers["idempotency-key"])).toEqual([
      "60b258a9b3d4fedc3e76a4687e2f18f3a6b35dd43a5afde3743c2484c38598a5",
      "57b1f343b2298f8dca3d00127f9f7b7b280934708c580ea73c339dff509f7eb1",
      "3976d8a6de96175c872c79d0bba946217ef5b466d838926513bd53cdee626186",
    ]);
  });

  it("bounds the model's wait", async () => {
    server.on("POST", "/v1/runs", json(run(), 201));
    const bounded = rewind.agentTools({ labels: LABELS, wait: "10s", maxWait: "20s" });
    await bounded.handle("run_script", { code: "x", wait: "1h" });
    await bounded.handle("run_script", { code: "x" });
    expect(server.requests.map((r) => (r.body as { wait: string }).wait)).toEqual(["20s", "10s"]);
  });

  it("tells the model what is wrong with its arguments", async () => {
    expect(await tools.handle("run_script", {})).toEqual({
      content: "run_script: `code` must be a non-empty string.",
      isError: true,
    });
    expect((await tools.handle("run_script", "{not json")).content).toBe(
      "run_script: the arguments of run_script are not valid JSON.",
    );
    expect((await tools.handle("run_script", { code: "x", wait: "soon" })).content).toMatch(/`wait` is not a duration/);
    expect(server.requests).toHaveLength(0);
  });

  it("throws on faults of the platform and on unknown tools", async () => {
    server.on("POST", "/v1/runs", problem(500, "internal", "the store is down"));
    await expect(tools.handle("run_script", { code: "x" })).rejects.toBeInstanceOf(RewindApiError);
    server.on("POST", "/v1/runs", problem(401, "unauthorized", "unknown key"));
    await expect(tools.handle("run_script", { code: "x" })).rejects.toMatchObject({ status: 401 });
    await expect(tools.handle("delete_everything", {})).rejects.toBeInstanceOf(RewindError);
  });
});

describe("the agent's namespace", () => {
  it("is named on every request of a client bound to it", async () => {
    server
      .on("POST", "/v1/runs", started())
      .on("POST", "/v1/check", json({ ok: true, diagnostics: [] }))
      .on("POST", "/v1/test", json({ passed: true, tests: [] }))
      .on("GET", "/v1/runs", json({ runs: [] }))
      .on("POST", "/v1/workflows/{name}/record", json({ tests: [], skipped: [] }))
      .on(
        "GET",
        "/v1/workflows/{name}",
        json({ name: "triage", versions: [workflow({ tests: [{ name: "t", digest: "ab".repeat(32) }] })] }),
      )
      .on("GET", "/v1/scripts/{digest}", json({ digest: "cd".repeat(32), source: "src" }))
      .on("GET", "/v1/tests/{digest}", json({ name: "t" }));
    serveRun(run());
    await tools.handle("run_script", { code: "x" }, { toolCallId: "call_0" });
    await tools.handle("check_script", { code: "x" });
    await tools.handle("test_script", { code: "x", tests: ["ab"] });
    await tools.handle("list_runs", {});
    await tools.handle("get_run", { run_id: RUN_ID });
    await tools.handle("record_tests", { workflow: "triage" });
    await tools.handle("get_workflow", { name: "triage" });
    expect(server.requests.length).toBeGreaterThanOrEqual(9);
    for (const request of server.requests) {
      expect(request.headers["rewind-namespace"], `${request.method} ${request.path}`).toBe(NAMESPACE);
    }
  });

  it("is the token's, for a client holding an end user's token", async () => {
    server.on("POST", "/v1/runs", started());
    const asUser = new Rewind({ url: server.url, token: "rewind_nt_alice" });
    const result = await asUser.agentTools({ labels: LABELS }).handle("run_script", { code: "x" });
    expect(result.isError).toBe(false);
    expect(server.requests[0]?.headers["authorization"]).toBe("Bearer rewind_nt_alice");
    expect(server.requests[0]?.headers["rewind-namespace"]).toBeUndefined();
  });

  it("missing from a key's client is the harness's to fix: thrown, never shown to the model", async () => {
    server.on("POST", "/v1/runs", (request) =>
      request.headers["rewind-namespace"] === undefined
        ? problem(400, "invalid-request", "the header Rewind-Namespace is required: it names the end user")
        : started()(request),
    );
    const unbound = rewind.agentTools({ labels: LABELS });
    await expect(unbound.handle("run_script", { code: "x" })).rejects.toBeInstanceOf(NamespaceRequiredError);
    expect((await tools.handle("run_script", { code: "x" })).isError).toBe(false);
  });

  it("over a quota is told to the model", async () => {
    server.on(
      "POST",
      "/v1/runs",
      problem(429, "quota-exceeded", "This end user has 5 runs active, their limit: wait for one to end.", {
        quota: "active_runs",
        scope: "namespace",
        limit: 5,
        used: 5,
      }),
    );
    expect(await tools.handle("run_script", { code: "x" })).toEqual({
      content: "run_script failed: This end user has 5 runs active, their limit: wait for one to end.",
      isError: true,
    });
  });
});

describe("a run answered for the tool call's idempotency key", () => {
  const digest = (code: string) => createHash("sha256").update(code).digest("hex");

  it("is reported when it is the run this call asked for", async () => {
    server.on("POST", "/v1/runs", json(run({ script: digest("x"), output: payload(1) })));
    const result = await tools.handle("run_script", { code: "x" }, { toolCallId: "call_0" });
    expect(result.content).toBe(`Run ${RUN_ID} completed.\nOutput:\n1`);
  });

  it("is never shown when another agent's labels started it, nor when another script did", async () => {
    // A harness whose tool call ids repeat (call_0, call_1…) across conversations or turns.
    for (const other of [
      run({ script: digest("x"), labels: { conversation: "someone-else" }, output: payload("their secret") }),
      run({ script: digest("an earlier script"), output: payload("an earlier output") }),
    ]) {
      server.on("POST", "/v1/runs", json(other));
      const started = tools.handle("run_script", { code: "x" }, { toolCallId: "call_0" });
      await expect(started).rejects.toBeInstanceOf(RewindError);
      await expect(started).rejects.toThrow(/tool call id "call_0".*another run/);
    }
  });

  it("is checked by retry_run too", async () => {
    serveRun(run({ status: "failed" }));
    server.on(
      "POST",
      "/v1/runs/{id}/fork",
      json(
        run({ id: "fork-1", script: digest("fixed"), labels: { conversation: "someone-else" }, output: payload(2) }),
      ),
    );
    await expect(
      tools.handle("retry_run", { run_id: RUN_ID, code: "fixed" }, { toolCallId: "call_1" }),
    ).rejects.toThrow(/tool call id "call_1".*another run/);
  });
});

describe("a tool result", () => {
  const BOUND = 512 * 1024;
  const huge = "x".repeat(3_000_000);
  const bytes = (text: string) => Buffer.byteLength(text);

  it("stays bounded whatever the script throws", async () => {
    server.on(
      "POST",
      "/v1/runs",
      json(
        run({
          status: "failed",
          error: {
            name: huge,
            message: huge,
            stack: huge,
            frame: huge,
            failedCall: {
              tool: "github.get_issue",
              seq: 1,
              args: payload({ number: 7 }),
              error: { kind: "error", message: huge, data: huge },
            },
          },
          logs: Array.from({ length: 20 }, (_, idx) => ({ at: "t", idx, level: "info" as const, message: huge })),
        }),
        201,
      ),
    );
    const result = await tools.handle("run_script", { code: "x" });
    expect(bytes(result.content)).toBeLessThan(BOUND);
    expect(result.content).toContain("The call that failed: github.get_issue");
    expect(result.content).toContain("Trace up to the failure:\n#0 run_started");
  });

  it("stays bounded whatever the diagnostics, the test or the server say", async () => {
    const diagnostics = Array.from({ length: 2_000 }, () => ({
      code: "TS2322",
      message: "x".repeat(2_000),
      severity: "error" as const,
      frame: "x".repeat(2_000),
    }));
    server.on("POST", "/v1/runs", problem(422, "invalid-script", huge, { diagnostics }));
    const refused = await tools.handle("run_script", { code: "x" });
    expect(refused.isError).toBe(true);
    expect(bytes(refused.content)).toBeLessThan(BOUND);

    server.on("POST", "/v1/check", json({ ok: false, diagnostics }));
    expect(bytes((await tools.handle("check_script", { code: "x" })).content)).toBeLessThan(BOUND);

    const large = "x".repeat(500_000);
    const failed = {
      name: large,
      passed: false,
      problem: "failed",
      status: "failed",
      calls: {},
      failures: [large, large],
      error: { name: "Error", message: large },
      trace: large,
    };
    server.on("POST", "/v1/test", json({ passed: false, tests: Array.from({ length: 20 }, () => failed) }));
    expect(bytes((await tools.handle("test_script", { code: "x", workflow: "triage" })).content)).toBeLessThan(BOUND);

    server.on("GET", "/v1/runs/{id}", problem(400, "invalid-request", huge));
    expect(bytes((await tools.handle("get_run", { run_id: RUN_ID })).content)).toBeLessThan(BOUND);
  });
});

describe("the tools that take a run id", () => {
  it("refuse a run that does not carry the agent's labels, as if it did not exist", async () => {
    serveRun(run({ labels: { conversation: "someone-else" } }));
    for (const [name, input] of [
      ["get_run", { run_id: RUN_ID }],
      ["signal_run", { run_id: RUN_ID, name: "approval" }],
      ["cancel_run", { run_id: RUN_ID }],
      ["retry_run", { run_id: RUN_ID, code: "x" }],
    ] as const) {
      const result = await tools.handle(name, input);
      expect(result).toEqual({
        content: `${name}: Run ${RUN_ID} does not exist, or is not one of the runs you started.`,
        isError: true,
      });
    }
    expect(server.requests.every((r) => r.method === "GET")).toBe(true);
  });

  it("report a run that does not exist", async () => {
    serveRun(run());
    const result = await tools.handle("get_run", { run_id: "nope" });
    expect(result).toEqual({ content: "get_run failed: no run nope", isError: true });
  });

  it("get_run: the state and the trace, after waiting when asked", async () => {
    serveRun(
      run({
        status: "active",
        waitingFor: [
          {
            kind: "tool_call",
            seq: 2,
            tool: "slack.post",
            taskId: "t",
            state: "claimed",
            attempt: 2,
            args: payload({}),
          },
        ],
      }),
    );
    const result = await tools.handle("get_run", { run_id: RUN_ID, wait: "200ms" });
    expect(result.isError).toBe(false);
    expect(result.content).toBe(
      [
        `Run ${RUN_ID} is still running.`,
        "Waiting for:",
        "- the call 2 to slack.post: in progress (attempt 2)",
        "Trace:",
        "#0 run_started\n#1 tool github.get_issue → error",
        `Follow it with get_run({"run_id":"${RUN_ID}"}), send it a signal with signal_run, or cancel it with cancel_run.`,
      ].join("\n"),
    );
    const gets = server.calls("GET", `/v1/runs/${RUN_ID}`);
    expect(gets.map((r) => r.query.get("wait"))).toEqual([null, expect.stringMatching(/^\d+ms$/)]);
  });

  it("get_run: one entry in full, by pages", async () => {
    serveRun(run());
    const value = { text: "a".repeat(100) };
    server.on("GET", "/v1/runs/{id}/entries/{idx}", (request) =>
      json({
        idx: Number(request.params["idx"]),
        recordedAt: "t",
        event: { type: "complete_run", output: payload(value) },
      }),
    );
    const whole = await tools.handle("get_run", { run_id: RUN_ID, entry: 3 });
    expect(whole.content.startsWith(`Entry 3 of run ${RUN_ID}:\n{"idx":3,`)).toBe(true);
    const small = rewind.agentTools({ labels: LABELS, outputLimit: 64 });
    const page = await small.handle("get_run", { run_id: RUN_ID, entry: 3, offset: 64 });
    expect(page.content).toMatch(new RegExp(`^Entry 3 of run ${RUN_ID}, bytes 64 to 128 of \\d+:`));
    expect(page.content).toContain(`get_run({"run_id":"${RUN_ID}","entry":3,"offset":128})`);
  });

  it("get_run: no output to page through before the run completes", async () => {
    serveRun(run({ status: "active" }));
    const result = await tools.handle("get_run", { run_id: RUN_ID, offset: 0 });
    expect(result).toMatchObject({ isError: true, content: expect.stringContaining("has no output to page through") });
  });

  it("signal_run: sends the signal once per tool call", async () => {
    serveRun(run({ status: "active" }));
    let sent = 0;
    server.on("POST", "/v1/runs/{id}/signals/{name}", (request) => {
      sent += 1;
      return json({ duplicate: sent > 1, name: request.params["name"], runId: RUN_ID }, 202);
    });
    const first = await tools.handle(
      "signal_run",
      { run_id: RUN_ID, name: "approval", payload: { close: true } },
      { toolCallId: "call_7" },
    );
    expect(first.content).toBe(
      `Sent the signal "approval" to run ${RUN_ID}. Read what the run does next with get_run({"run_id":"${RUN_ID}","wait":"30s"}).`,
    );
    const again = await tools.handle("signal_run", { run_id: RUN_ID, name: "approval" }, { toolCallId: "call_7" });
    expect(again.content).toBe(`This signal "approval" was already sent to run ${RUN_ID}: nothing was added.`);
    const signals = server.calls("POST", `/v1/runs/${RUN_ID}/signals/approval`);
    expect(signals.map((r) => r.headers["idempotency-key"])).toEqual(["call_7", "call_7"]);
    expect(signals[0]?.body).toEqual({ close: true });
    expect(signals[1]?.rawBody).toBe("null");
  });

  it("signal_run: a run of a workflow refuses a signal it does not declare", async () => {
    serveRun(run({ status: "active", workflow: { name: "triage", version: VERSION } }));
    server.on(
      "POST",
      "/v1/runs/{id}/signals/{name}",
      problem(
        422,
        "invalid-signal",
        `run ${RUN_ID} is a run of triage, which does not declare the signal "ping" (it declares approval)`,
      ),
    );
    const result = await tools.handle("signal_run", { run_id: RUN_ID, name: "ping" });
    expect(result.isError).toBe(true);
    expect(result.content).toBe(
      `Run ${RUN_ID} refused the signal "ping": run ${RUN_ID} is a run of triage, which does not declare the signal "ping" (it declares approval)`,
    );
  });

  it("get_run: says which workflow a run executes, and which run started it", async () => {
    serveRun(
      run({
        workflow: { name: "triage", version: VERSION },
        startedBy: { run: "parent-1", seq: 2, detached: true },
        output: payload({ closed: 1 }),
      }),
    );
    const result = await tools.handle("get_run", { run_id: RUN_ID });
    expect(result.content.split("\n")[0]).toBe(
      `Run ${RUN_ID} (a run of the workflow triage@4be1c07a9d3e, started by run parent-1, detached) completed.`,
    );
  });

  it("signal_run: a run that has ended refuses it", async () => {
    serveRun(run());
    server.on("POST", "/v1/runs/{id}/signals/{name}", problem(409, "run-ended", `run ${RUN_ID} has ended (completed)`));
    const result = await tools.handle("signal_run", { run_id: RUN_ID, name: "approval" });
    expect(result).toEqual({ content: `signal_run failed: run ${RUN_ID} has ended (completed)`, isError: true });
  });

  it("cancel_run: cancels and returns the run's state", async () => {
    serveRun(run({ status: "active" }));
    server.on(
      "POST",
      "/v1/runs/{id}/cancel",
      json(run({ status: "cancelled", error: { name: "CancelledError", message: "cancelled" } }), 202),
    );
    const result = await tools.handle("cancel_run", { run_id: RUN_ID });
    expect(result.isError).toBe(false);
    expect(result.content).toBe(`Run ${RUN_ID} was cancelled.\nCancelledError: cancelled`);
    expect(server.calls("POST", `/v1/runs/${RUN_ID}/cancel`)[0]?.body).toEqual({ wait: "30s" });
  });

  it("retry_run: forks the run with the corrected script", async () => {
    serveRun(run({ status: "failed" }));
    server.on("POST", "/v1/runs/{id}/fork", started({ id: "fork-1", parent: { run: RUN_ID }, output: payload("ok") }));
    const result = await tools.handle("retry_run", { run_id: RUN_ID, code: "fixed" }, { toolCallId: "call_9" });
    expect(result.content).toBe(`Run fork-1 (a fork of run ${RUN_ID}) completed.\nOutput:\n"ok"`);
    const fork = server.calls("POST", `/v1/runs/${RUN_ID}/fork`)[0];
    expect(fork?.headers["idempotency-key"]).toBe(runKey("call_9"));
    expect(fork?.body).toEqual({ script: "fixed", labels: LABELS, wait: "30s" });
  });

  it("retry_run: says which tests of the workflow's suite refused the fix", async () => {
    serveRun(run({ status: "failed", workflow: { name: "triage", version: VERSION } }));
    const tests = [
      { name: "closes once approved", passed: true, status: "completed", calls: {}, failures: [], trace: "#0 ok" },
      {
        name: "closes nothing without approval",
        passed: false,
        problem: "failed",
        status: "completed",
        output: { closed: 1 },
        calls: { "github.close_issue": 1, "github.list_issues": 1 },
        failures: ["github.close_issue was called 1 time, expected 0"],
        trace: "#0 run_started\n#1 tool github.close_issue",
      },
    ];
    server.on(
      "POST",
      "/v1/runs/{id}/fork",
      problem(422, "tests-failed", "1 test of the suite of triage did not pass against the corrected script", {
        tests,
      }),
    );
    const result = await tools.handle("retry_run", { run_id: RUN_ID, code: "fixed" });
    expect(result.isError).toBe(true);
    expect(result.tests).toEqual(tests);
    expect(result.run?.id).toBe(RUN_ID);
    expect(result.content).toBe(
      [
        `Run ${RUN_ID} was not retried: 1 test of the suite of triage did not pass against the corrected script`,
        "1 of 2 tests did not pass.",
        'PASS "closes once approved"',
        'FAIL "closes nothing without approval" (failed): it ran, and expectations do not hold:',
        "  - github.close_issue was called 1 time, expected 0",
        "  The run completed.",
        '  Output: {"closed":1}',
        "  Calls: github.close_issue ×1, github.list_issues ×1",
        "  Trace:",
        "#0 run_started\n#1 tool github.close_issue",
        'A fix must keep every behavior the suite of triage@4be1c07a9d3e pins: there is no way around it. Fix the script, and check it with test_script({"code":"…","workflow":"triage@4be1c07a9d3e"}) before you try again. If a behavior must change on purpose, save a new version of the workflow with save_workflow, retiring or updating its tests with a reason, and start a new run of it.',
      ].join("\n"),
    );
  });
});

describe("the other tools", () => {
  it("list_runs: the agent's runs only", async () => {
    server.on(
      "GET",
      "/v1/runs",
      json({
        runs: [
          {
            id: "r1",
            status: "failed",
            error: "ToolError",
            errors: 1,
            historyLen: 5,
            labels: LABELS,
            lastActivityAt: "t",
            quarantined: false,
            script: "s",
            startedAt: "2026-11-12T10:00:00Z",
            toolCalls: 2,
            toolset: TOOLSET,
            parent: "r0",
          },
        ],
        nextCursor: "more",
      }),
    );
    const result = await tools.handle("list_runs", { status: "failed" });
    expect(result.content).toBe(
      [
        "1 run with the status failed, most recent first:",
        "- r1: failed (ToolError), started 2026-11-12T10:00:00Z, 2 tool calls, 1 failed, a fork of r0",
        "More runs exist: narrow the list with a status.",
      ].join("\n"),
    );
    const request = server.requests[0];
    expect(request?.query.getAll("label")).toEqual(["conversation:c-42"]);
    expect(request?.query.get("status")).toBe("failed");
    expect((await tools.handle("list_runs", { status: "done" })).isError).toBe(true);
  });

  it("list_runs: says which workflow a run executes, and which run started it", async () => {
    server.on(
      "GET",
      "/v1/runs",
      json({
        runs: [
          {
            id: "r2",
            status: "active",
            errors: 0,
            historyLen: 3,
            labels: LABELS,
            lastActivityAt: "t",
            quarantined: false,
            script: "s",
            startedAt: "2026-11-12T10:00:00Z",
            toolCalls: 1,
            toolset: TOOLSET,
            workflow: "triage",
            startedBy: "r1",
          },
        ],
      }),
    );
    const result = await tools.handle("list_runs", {});
    expect(result.content).toBe(
      [
        "1 run, most recent first:",
        "- r2: active, started 2026-11-12T10:00:00Z, 1 tool call, a run of the workflow triage, started by r1",
      ].join("\n"),
    );
  });

  it("check_script: the diagnostics", async () => {
    server.on("POST", "/v1/check", json({ ok: true, diagnostics: [] }));
    expect(await tools.handle("check_script", { code: "x" })).toEqual({
      content: "The script type-checks.",
      isError: false,
    });
    server.on(
      "POST",
      "/v1/check",
      json({
        ok: false,
        diagnostics: [
          { code: "TS2304", message: "Cannot find name 'foo'.", severity: "error", location: { line: 1, column: 1 } },
          {
            code: "rewind/floating-promise",
            message: "not awaited",
            severity: "warning",
            location: { line: 2, column: 3 },
          },
        ],
      }),
    );
    const failed = await tools.handle("check_script", { code: "x" });
    expect(failed.isError).toBe(true);
    expect(failed.content).toBe(
      [
        "The script does not type-check: 1 error and 1 warning.",
        "script.ts:1:1 error TS2304: Cannot find name 'foo'.",
        "",
        "script.ts:2:3 warning rewind/floating-promise: not awaited",
      ].join("\n"),
    );
    expect(server.requests[0]?.body).toEqual({ script: "x", toolset: TOOLSET });
  });

  it("test_script: runs the tests, and the suite of a workflow, against the script", async () => {
    const recorded = "9f".repeat(32);
    server.on(
      "POST",
      "/v1/test",
      json({
        passed: false,
        tests: [
          {
            name: "closes the stale issues once approved",
            passed: true,
            status: "completed",
            output: { closed: 1 },
            calls: { "github.close_issue": 1 },
            failures: [],
            trace: "#0 run_started (a passing test's trace is not shown)",
          },
          {
            name: `recorded ${RUN_ID}`,
            passed: false,
            problem: "failed",
            status: "completed",
            output: { closed: 2 },
            calls: { "github.close_issue": 2, "github.list_issues": 1 },
            failures: ["the output's closed is 2, expected 3", "github.delete_repo has no recorded response"],
            trace: "#0 run_started\n#1 tool github.list_issues",
          },
          {
            name: "posts to slack",
            passed: false,
            problem: "stale",
            calls: {},
            failures: ["slack.post_message is not one of the tools"],
            trace: "",
          },
          {
            name: "refuses a bad repo",
            passed: false,
            problem: "incompatible",
            calls: {},
            failures: ["the input does not match the input schema of triage: repo must be a string"],
            trace: "",
          },
          {
            name: "fails when GitHub is down",
            passed: false,
            problem: "failed",
            status: "failed",
            error: { name: "ToolError", message: "GitHub is down" },
            calls: { "github.list_issues": 1 },
            failures: ["the run failed with ToolError, expected completed"],
            trace: "#0 run_started\n#1 tool github.list_issues → error",
          },
        ],
      }),
    );
    const tests = [
      {
        name: "closes the stale issues once approved",
        input: { repo: "acme/api" },
        world: {
          tools: {
            "github.list_issues": [{ args: { state: "open" }, returns: [{ number: 4 }] }],
            "github.close_issue": { returns: {} },
          },
        },
        signals: [{ name: "approval", payload: { close: true } }],
        expect: { status: "completed", output: { closed: 1 }, calls: [{ tool: "github.close_issue", times: 1 }] },
      },
      recorded,
      { from: recorded, expect: { status: "completed" } },
    ];
    const result = await tools.handle("test_script", { code: "x", tests, workflow: "triage" });
    expect(result.isError).toBe(true);
    expect(result.tests).toHaveLength(5);
    expect(result.content).toBe(
      [
        "4 of 5 tests did not pass.",
        'PASS "closes the stale issues once approved"',
        `FAIL "recorded ${RUN_ID}" (failed): it ran, and expectations do not hold:`,
        "  - the output's closed is 2, expected 3",
        "  - github.delete_repo has no recorded response",
        "  The run completed.",
        '  Output: {"closed":2}',
        "  Calls: github.close_issue ×2, github.list_issues ×1",
        "  Trace:",
        "#0 run_started\n#1 tool github.list_issues",
        'FAIL "posts to slack" (stale): it names tools, answers or args the toolset no longer has; it was not run.',
        "  - slack.post_message is not one of the tools",
        `FAIL "refuses a bad repo" (incompatible): the workflow's input or signal schemas refuse its input or its signals; it was not run.`,
        "  - the input does not match the input schema of triage: repo must be a string",
        'FAIL "fails when GitHub is down" (failed): it ran, and expectations do not hold:',
        "  - the run failed with ToolError, expected completed",
        "  The run failed.",
        "  ToolError: GitHub is down",
        "  Calls: github.list_issues ×1",
        "  Trace:",
        "#0 run_started\n#1 tool github.list_issues → error",
      ].join("\n"),
    );
    expect(server.calls("POST", "/v1/test")[0]?.body).toEqual({
      script: "x",
      toolset: TOOLSET,
      tests,
      workflow: "triage",
    });
  });

  it("test_script: all passed, and the tests refused before running", async () => {
    server.on(
      "POST",
      "/v1/test",
      json({
        passed: true,
        tests: [
          { name: "a", passed: true, status: "completed", calls: {}, failures: [], trace: "#0" },
          { name: "b", passed: true, status: "active", calls: {}, failures: [], trace: "#0" },
        ],
      }),
    );
    expect(await tools.handle("test_script", { code: "x", workflow: "triage" })).toEqual({
      content: 'All 2 tests passed.\nPASS "a"\nPASS "b"',
      isError: false,
      tests: expect.any(Array),
    });
    server.on("POST", "/v1/test", problem(422, "invalid-test", 'no stored test "00"; record one with record_tests'));
    expect(await tools.handle("test_script", { code: "x", tests: ["00"] })).toEqual({
      content: 'The tests were refused before running: no stored test "00"; record one with record_tests',
      isError: true,
    });
  });

  it("test_script: tells the model what is wrong with its arguments", async () => {
    expect((await tools.handle("test_script", { code: "x" })).content).toBe(
      "test_script: give `tests`, or a `workflow` whose suite runs against the script, or both.",
    );
    expect((await tools.handle("test_script", { code: "x", tests: {} })).content).toBe(
      "test_script: `tests` must be an array of tests: each one an object, or the digest of a recorded test as a string.",
    );
    expect((await tools.handle("test_script", { code: "x", tests: [1] })).isError).toBe(true);
    expect((await tools.handle("test_script", { code: "x", workflow: 3 })).content).toBe(
      "test_script: `workflow` must be a non-empty string.",
    );
    expect(server.requests).toHaveLength(0);
  });

  it("search_tools: the declarations of the matching tools", async () => {
    server.on(
      "GET",
      "/v1/toolsets/{id}/search",
      json({
        tools: [
          {
            name: "github.close_issue",
            description: "Close",
            dts: "close_issue(args: { number: number }): Promise<void>;",
            prompt: "p",
          },
        ],
      }),
    );
    const result = await tools.handle("search_tools", { query: "close" });
    expect(result.content).toBe('1 tool matches "close":\n\nclose_issue(args: { number: number }): Promise<void>;');
    expect(server.requests[0]?.query.get("query")).toBe("close");
  });
});

describe("save_workflow", () => {
  const DERIVED = "de".repeat(32);
  const source =
    'import { signal } from "rewind";\nexport default async function main(input: { repo: string }) {\n  const { close } = await signal("approval");\n  return { closed: close ? 1 : 0 };\n}\n';
  const saved = workflow();
  const definition = {
    name: "triage",
    description: saved.description,
    code: source,
    input: saved.input,
    output: saved.output,
    signals: saved.signals,
  };

  /** Toolsets as the server derives them: each one a new id, numbered. */
  function deriveToolsets(): void {
    let derived = 0;
    server.on("POST", "/v1/toolsets", () => {
      derived += 1;
      return json(toolset(derived === 1 ? DERIVED : `${DERIVED.slice(0, -1)}${derived}`, [saved]), 201);
    });
  }

  it("saves the workflow with the agent's toolset, and tells the model how to import it", async () => {
    server.on("POST", "/v1/workflows", json(saved, 201));
    deriveToolsets();
    const result = await tools.handle("save_workflow", definition, { toolCallId: "call_1" });
    expect(result.isError).toBe(false);
    expect(result.workflow).toEqual(saved);
    expect(result.toolset?.id).toBe(DERIVED);
    expect(result.content).toBe(
      [
        "Saved the workflow triage@4be1c07a9d3e. Your next scripts can import it:",
        'import { triage } from "rewind:workflows";',
        saved.declarations.prompt,
        "Its declaration:",
        saved.declarations.dts,
      ].join("\n"),
    );
    expect(server.calls("POST", "/v1/workflows")[0]?.body).toEqual({
      name: "triage",
      description: saved.description,
      toolset: TOOLSET,
      script: source,
      input: saved.input,
      output: saved.output,
      signals: saved.signals,
    });
    // The toolset derived from the agent's pins this very version.
    expect(server.calls("POST", "/v1/toolsets")[0]?.body).toEqual({
      base: TOOLSET,
      workflows: [`triage@${VERSION}`],
    });
    expect(tools.toolset).toBe(DERIVED);
  });

  it("makes the later scripts run, check, test and search with the derived toolset", async () => {
    server.on("POST", "/v1/workflows", json(saved, 201));
    deriveToolsets();
    server.on("POST", "/v1/runs", started({ output: payload({ closed: 1 }) }));
    server.on("POST", "/v1/check", json({ ok: true, diagnostics: [] }));
    server.on("POST", "/v1/test", json({ passed: true, tests: [] }));
    server.on("GET", "/v1/toolsets/{id}/search", json({ tools: [] }));

    await tools.handle("save_workflow", definition);
    const code =
      'import { triage } from "rewind:workflows";\nexport default async () => (await triage.start({ repo: "a" })).result();';
    const ran = await tools.handle("run_script", { code });
    expect(ran.content).toBe(`Run ${RUN_ID} completed.\nOutput:\n{"closed":1}`);
    await tools.handle("check_script", { code });
    await tools.handle("test_script", { code, workflow: "triage" });
    await tools.handle("search_tools", { query: "triage" });

    expect(server.calls("POST", "/v1/runs")[0]?.body).toMatchObject({ script: code, toolset: DERIVED });
    expect(server.calls("POST", "/v1/check")[0]?.body).toEqual({ script: code, toolset: DERIVED });
    expect(server.calls("POST", "/v1/test")[0]?.body).toMatchObject({ script: code, toolset: DERIVED });
    expect(server.calls("GET", `/v1/toolsets/${DERIVED}/search`)).toHaveLength(1);
    // Other agent tools keep their own toolset.
    expect(rewind.agentTools({ toolset: TOOLSET, labels: LABELS }).toolset).toBe(TOOLSET);
  });

  it("derives each toolset from the previous one, when the model saves several at once", async () => {
    server.on("POST", "/v1/workflows", (request) => {
      const name = (request.body as { name: string }).name;
      return json(workflow({ name, version: name === "triage" ? VERSION : "77".repeat(32) }), 201);
    });
    deriveToolsets();
    const [first, second] = await Promise.all([
      tools.handle("save_workflow", definition),
      tools.handle("save_workflow", { ...definition, name: "digest" }),
    ]);
    expect(first?.isError).toBe(false);
    expect(second?.isError).toBe(false);
    const derivations = server.calls("POST", "/v1/toolsets").map((r) => r.body as { base: string });
    expect(derivations.map((body) => body.base)).toEqual([TOOLSET, DERIVED]);
    expect(tools.toolset).toBe(`${DERIVED.slice(0, -1)}2`);
  });

  it("returns the diagnostics of a refused script, and keeps the toolset", async () => {
    server.on(
      "POST",
      "/v1/workflows",
      problem(422, "invalid-script", "the script does not type-check", {
        diagnostics: [
          {
            code: "TS2345",
            message: "Argument of type '\"aproval\"' is not assignable to parameter of type '\"approval\"'.",
            severity: "error",
            location: { line: 3, column: 34 },
            frame: '3 |   const { close } = await signal("aproval");\n  |                                  ^^^^^^^^^',
          },
        ],
      }),
    );
    const result = await tools.handle("save_workflow", definition);
    expect(result).toEqual({
      content: [
        "The workflow was not saved: the script does not type-check",
        "script.ts:3:34 error TS2345: Argument of type '\"aproval\"' is not assignable to parameter of type '\"approval\"'.",
        '3 |   const { close } = await signal("aproval");\n  |                                  ^^^^^^^^^',
      ].join("\n"),
      isError: true,
    });
    expect(server.calls("POST", "/v1/toolsets")).toHaveLength(0);
    expect(tools.toolset).toBe(TOOLSET);

    server.on("POST", "/v1/runs", started());
    await tools.handle("run_script", { code: "x" });
    expect(server.calls("POST", "/v1/runs")[0]?.body).toMatchObject({ toolset: TOOLSET });
  });

  it("returns why a name or a schema cannot be used", async () => {
    server.on(
      "POST",
      "/v1/workflows",
      problem(422, "invalid-workflow", 'the workflow name "class" is a reserved word of JavaScript'),
    );
    const result = await tools.handle("save_workflow", { ...definition, name: "class" });
    expect(result).toEqual({
      content: 'The workflow was not saved: the workflow name "class" is a reserved word of JavaScript',
      isError: true,
    });
    expect(tools.toolset).toBe(TOOLSET);
  });

  it("says when the workflow was saved but could not be added to the toolset", async () => {
    server.on("POST", "/v1/workflows", json(saved, 201));
    server.on("POST", "/v1/toolsets", problem(422, "invalid-toolset", "a toolset pins 64 workflows at most"));
    const result = await tools.handle("save_workflow", definition);
    expect(result).toMatchObject({
      content:
        "The workflow triage was saved, but your scripts cannot start it yet: adding it to your tools failed: a toolset pins 64 workflows at most",
      isError: true,
      workflow: saved,
    });
    expect(tools.toolset).toBe(TOOLSET);
  });

  it("tells the model what is wrong with its arguments", async () => {
    expect((await tools.handle("save_workflow", { ...definition, input: "string" })).content).toBe(
      "save_workflow: `input` must be a JSON Schema: an object.",
    );
    expect((await tools.handle("save_workflow", { ...definition, signals: [] })).content).toBe(
      "save_workflow: `signals` must be an object of JSON Schemas by signal name.",
    );
    expect((await tools.handle("save_workflow", { ...definition, name: "" })).content).toBe(
      "save_workflow: `name` must be a non-empty string.",
    );
    expect(server.requests).toHaveLength(0);
  });

  it("stays bounded whatever the declaration", async () => {
    const huge = "x".repeat(3_000_000);
    server.on("POST", "/v1/workflows", json(workflow({ declarations: { dts: huge, prompt: huge } }), 201));
    deriveToolsets();
    const result = await tools.handle("save_workflow", definition);
    expect(Buffer.byteLength(result.content)).toBeLessThan(512 * 1024);
    expect(result.content).toContain('import { triage } from "rewind:workflows";');
  });

  it("sends the suite: new tests, those it updates and retires with their reasons, and its parent", async () => {
    server.on("POST", "/v1/workflows", json(saved, 201));
    deriveToolsets();
    const suite = {
      tests: [
        "9f".repeat(32),
        { from: "8e".repeat(32), expect: { status: "completed", output: { closed: 0 } } },
        {
          name: "closes nothing without approval",
          world: { tools: { "github.list_issues": { returns: [] } } },
          signals: [{ name: "approval", payload: { close: false } }],
          expect: { output: { closed: 0 }, calls: [{ tool: "github.close_issue", times: 0 }] },
        },
      ],
      update: [
        { name: "closes once approved", reason: "the message names the repository", expect: { output: { closed: 1 } } },
      ],
      retire: [{ names: ["asks before closing"], reason: "under 5 stale issues, it closes without asking" }],
      parent: `triage@${VERSION.slice(0, 12)}`,
    };
    const result = await tools.handle("save_workflow", { ...definition, ...suite });
    expect(result.isError).toBe(false);
    expect(server.calls("POST", "/v1/workflows")[0]?.body).toEqual({
      name: "triage",
      description: saved.description,
      toolset: TOOLSET,
      script: source,
      input: saved.input,
      output: saved.output,
      signals: saved.signals,
      ...suite,
    });
  });

  it("returns the tests of the suite that refused the save, and keeps the toolset", async () => {
    const tests = [
      {
        name: "closes nothing without approval",
        passed: false,
        problem: "failed",
        status: "completed",
        output: { closed: 1 },
        calls: { "github.close_issue": 1 },
        failures: ["github.close_issue was called 1 time, expected 0"],
        trace: "#0 run_started\n#1 signal approval\n#2 tool github.close_issue",
      },
      { name: "closes once approved", passed: true, status: "completed", calls: {}, failures: [], trace: "#0" },
    ];
    server.on(
      "POST",
      "/v1/workflows",
      problem(422, "tests-failed", "1 test of 2 did not pass: the version was not saved", { tests }),
    );
    const result = await tools.handle("save_workflow", definition);
    expect(result).toEqual({
      content: [
        "The workflow was not saved: 1 test of 2 did not pass: the version was not saved",
        "1 of 2 tests did not pass.",
        'FAIL "closes nothing without approval" (failed): it ran, and expectations do not hold:',
        "  - github.close_issue was called 1 time, expected 0",
        "  The run completed.",
        '  Output: {"closed":1}',
        "  Calls: github.close_issue ×1",
        "  Trace:",
        "#0 run_started\n#1 signal approval\n#2 tool github.close_issue",
        'PASS "closes once approved"',
        'Every test must pass: there is no way around it. If the script is wrong, fix it, and check it with test_script({"code":"…","workflow":"triage"}) before you save it again. If a test pins a behavior you change on purpose, retire it in `retire` (the behavior is deprecated) or update it in `update` (its state or its expected result is out of date), with a reason.',
      ].join("\n"),
      isError: true,
      tests,
    });
    expect(server.calls("POST", "/v1/toolsets")).toHaveLength(0);
    expect(tools.toolset).toBe(TOOLSET);
  });

  it("returns why the suite was refused before any test ran", async () => {
    server.on(
      "POST",
      "/v1/workflows",
      problem(422, "invalid-test", 'retire names "asks first", which is not a test of the parent'),
    );
    const result = await tools.handle("save_workflow", {
      ...definition,
      retire: [{ names: ["asks first"], reason: "deprecated" }],
    });
    expect(result).toEqual({
      content: 'The workflow was not saved: retire names "asks first", which is not a test of the parent',
      isError: true,
    });
  });

  it("asks for a reason to update or retire a test", async () => {
    expect((await tools.handle("save_workflow", { ...definition, retire: [{ names: ["a"] }] })).content).toBe(
      "save_workflow: `retire` must be an array of { names, reason }: the names of inherited tests, and why they no longer hold.",
    );
    expect(
      (await tools.handle("save_workflow", { ...definition, retire: [{ names: ["a"], reason: " " }] })).isError,
    ).toBe(true);
    expect((await tools.handle("save_workflow", { ...definition, update: [{ name: "a", expect: {} }] })).content).toBe(
      "save_workflow: `update` must be an array of { name, reason, test?, expect? }: the name of an inherited test, why it changes, and the test that replaces it or its new expectation.",
    );
    expect((await tools.handle("save_workflow", { ...definition, tests: "abc" })).isError).toBe(true);
    expect((await tools.handle("save_workflow", { ...definition, parent: 4 })).content).toBe(
      "save_workflow: `parent` must be a non-empty string.",
    );
    expect(server.requests).toHaveLength(0);
  });
});

describe("get_workflow", () => {
  const SCRIPT = "cd".repeat(32);
  const PARENT = "e4".repeat(32);
  const FIRST = "a1".repeat(32);
  const INLINE = "4814d92093ac" + "0".repeat(52);
  const RECORDED = "ca978112ca1b" + "0".repeat(52);
  const FOREIGN = "b2".repeat(32);
  const latest = workflow({
    parent: PARENT,
    tests: [
      { name: "closes nothing without approval", digest: INLINE },
      { name: `recorded ${RUN_ID}`, digest: RECORDED },
      { name: "recorded 0193a5c4-0000-7000-8000-000000000000", digest: FOREIGN },
    ],
    updated: [
      {
        name: "closes nothing without approval",
        from: "3e".repeat(32),
        to: INLINE,
        reason: "the message names the repository",
      },
    ],
    retired: [
      {
        name: "asks before closing",
        digest: "2e".repeat(32),
        reason: "under 5 stale issues, it closes without asking",
      },
    ],
  });
  const parent = workflow({
    version: PARENT,
    createdAt: "2026-11-11T10:00:00Z",
    parent: FIRST,
    tests: [{ name: "asks before closing", digest: "2e".repeat(32) }],
  });
  const first = workflow({ version: FIRST, createdAt: "2026-11-10T10:00:00Z", tests: [] });
  const inline = {
    name: "closes nothing without approval",
    input: { repo: "acme/api" },
    world: { tools: { "github.list_issues": [{ returns: [{ number: 4 }] }] } },
    signals: [{ name: "approval", payload: { close: false } }],
    expect: { output: { closed: 0 }, calls: [{ tool: "github.close_issue", times: 0 }] },
  };
  const recordedTest = {
    name: `recorded ${RUN_ID}`,
    input: { repo: "acme/api" },
    seed: "07".repeat(32),
    world: {
      cassette: {
        v: 1,
        matching: "by_args",
        calls: [
          { tool: "github.list_issues", args: { repo: "acme/api" }, outcome: { result: [] } },
          { tool: "github.close_issue", args: { number: 4 }, outcome: { result: {} } },
          { tool: "github.close_issue", args: { number: 5 }, outcome: { result: {} } },
        ],
        signals: [{ name: "approval", payload: { close: true }, elapsed_ms: 3_600_000 }],
      },
    },
    expect: { status: "completed", output: { closed: 2 } },
    origin: { run: RUN_ID, version: PARENT, namespace: NAMESPACE },
  };

  function serveWorkflow(): void {
    server.on("GET", "/v1/workflows/{name}", (request) =>
      request.params["name"] === "triage"
        ? json({ name: "triage", versions: [latest, parent, first] })
        : problem(404, "not-found", `no workflow ${request.params["name"]}`),
    );
    server.on(
      "GET",
      "/v1/scripts/{digest}",
      json({ digest: SCRIPT, source: "export default async () => ({ closed: 0 });\n" }),
    );
    server.on("GET", "/v1/tests/{digest}", (request) => {
      switch (request.params["digest"]) {
        case INLINE:
          return json(inline);
        case RECORDED:
          return json(recordedTest);
        default:
          return problem(403, "forbidden", "reading this test needs runs:read in the namespace it was recorded from");
      }
    });
  }

  it("reads the source, the schemas, every test of the suite and the lineage", async () => {
    serveWorkflow();
    const result = await tools.handle("get_workflow", { name: "triage" });
    expect(result.isError).toBe(false);
    expect(result.workflow).toEqual(latest);
    expect(result.content).toBe(
      [
        "The workflow triage@4be1c07a9d3e, saved 2026-11-12T10:00:00Z: its latest version, of 3 versions.",
        `It derives from the version triage@${PARENT.slice(0, 12)}.`,
        "Description: Triage the stale issues of a repository, after approval.",
        `Input schema: ${JSON.stringify(latest.input)}`,
        `Output schema: ${JSON.stringify(latest.output)}`,
        "Signals:",
        `- "approval": ${JSON.stringify(latest.signals["approval"])}`,
        "Source:",
        "export default async () => ({ closed: 0 });",
        "Its suite, 3 tests, which every version derived from it must pass (give a digest in save_workflow's `tests` or `update` to reuse a test):",
        `Test "closes nothing without approval" (${INLINE}):`,
        JSON.stringify(inline),
        `Test "recorded ${RUN_ID}" (${RECORDED}), recorded from run ${RUN_ID} of triage@${PARENT.slice(0, 12)}:`,
        JSON.stringify({
          ...recordedTest,
          world: {
            cassette: {
              summary:
                "3 recorded calls matched by_args (github.list_issues ×1, github.close_issue ×2), 1 signal (approval)",
            },
          },
        }),
        `Test "recorded 0193a5c4-0000-7000-8000-000000000000" (${FOREIGN}): it cannot be read: reading this test needs runs:read in the namespace it was recorded from`,
        "Lineage, newest first:",
        `- triage@4be1c07a9d3e, saved 2026-11-12T10:00:00Z, from triage@${PARENT.slice(0, 12)}: 3 tests.`,
        '  updated "closes nothing without approval": the message names the repository',
        '  retired "asks before closing": under 5 stale issues, it closes without asking',
        `- triage@${PARENT.slice(0, 12)}, saved 2026-11-11T10:00:00Z, from triage@${FIRST.slice(0, 12)}: 1 test.`,
        `- triage@${FIRST.slice(0, 12)}, saved 2026-11-10T10:00:00Z, the first version: 0 tests.`,
      ].join("\n"),
    );
    expect(server.calls("GET", `/v1/scripts/${SCRIPT}`)).toHaveLength(1);
  });

  it("reports a workflow that does not exist, and stays bounded whatever the tests hold", async () => {
    serveWorkflow();
    expect(await tools.handle("get_workflow", { name: "nope" })).toEqual({
      content: "get_workflow failed: no workflow nope",
      isError: true,
    });
    const huge = "x".repeat(3_000_000);
    server.on("GET", "/v1/scripts/{digest}", json({ digest: SCRIPT, source: huge }));
    server.on("GET", "/v1/tests/{digest}", json({ ...inline, input: { huge } }));
    const result = await tools.handle("get_workflow", { name: "triage" });
    expect(Buffer.byteLength(result.content)).toBeLessThan(512 * 1024);
    expect(result.content).toContain("Lineage, newest first:");
    expect((await tools.handle("get_workflow", {})).content).toBe("get_workflow: `name` must be a non-empty string.");
  });
});

describe("record_tests", () => {
  it("records tests from the workflow's runs, and says how to add them to its suite", async () => {
    server.on(
      "POST",
      "/v1/workflows/{name}/record",
      json({
        tests: [
          {
            digest: "9f".repeat(32),
            name: `recorded ${RUN_ID}`,
            path: "github.list_issues · signal approval · github.close_issue×n → completed",
            run: RUN_ID,
            status: "completed",
          },
          {
            digest: "8e".repeat(32),
            name: "recorded r-2",
            path: "github.list_issues → failed",
            run: "r-2",
            status: "failed",
          },
        ],
        skipped: [{ run: "r-big", reason: "its test would be larger than 4 MiB" }],
      }),
    );
    const result = await tools.handle("record_tests", { workflow: "triage", query: "status:completed", limit: 10 });
    expect(result.isError).toBe(false);
    expect(result.content).toBe(
      [
        "Recorded 2 tests from the runs of triage, one per path:",
        `- ${"9f".repeat(32)} "recorded ${RUN_ID}", from run ${RUN_ID} (completed): github.list_issues · signal approval · github.close_issue×n → completed`,
        `- ${"8e".repeat(32)} "recorded r-2", from run r-2 (failed): github.list_issues → failed`,
        "Skipped 1 run:",
        "- r-big: its test would be larger than 4 MiB",
        'They join no suite yet: pass their digests in save_workflow\'s `tests` to add them to the suite of triage\'s next version, or { "from": "<digest>", "expect": {…} } to give a failed run the expectation it should have met.',
      ].join("\n"),
    );
    await tools.handle("record_tests", { workflow: "triage", runs: [RUN_ID] });
    expect(server.calls("POST", "/v1/workflows/triage/record").map((r) => r.body)).toEqual([
      { query: "status:completed", limit: 10 },
      { runs: [RUN_ID] },
    ]);
  });

  it("says when no run was recorded, and what is wrong with its arguments", async () => {
    server.on("POST", "/v1/workflows/{name}/record", json({ tests: [], skipped: [] }));
    expect((await tools.handle("record_tests", { workflow: "triage" })).content).toBe(
      "No test was recorded: no completed or failed run of triage was selected.",
    );
    server.on("POST", "/v1/workflows/{name}/record", problem(400, "invalid-query", "unknown field stauts"));
    expect(await tools.handle("record_tests", { workflow: "triage", query: "stauts:x" })).toEqual({
      content: "record_tests failed: unknown field stauts",
      isError: true,
    });
    expect((await tools.handle("record_tests", { workflow: "triage", limit: 0 })).content).toBe(
      "record_tests: `limit` must be an integer, 1 or more.",
    );
    expect((await tools.handle("record_tests", { workflow: "triage", runs: "r" })).content).toBe(
      "record_tests: `runs` must be an array of non-empty strings.",
    );
  });
});

describe("the adapters", () => {
  it("answer an Anthropic tool_use block with a tool_result block", async () => {
    server.on(
      "POST",
      "/v1/runs",
      started({ status: "aborted", abort: { kind: "limit_exceeded", message: "the history cap" } }),
    );
    const block = await tools.anthropic.handle({
      type: "tool_use",
      id: "toolu_9",
      name: "run_script",
      input: { code: "x" },
    });
    expect(block).toEqual({
      type: "tool_result",
      tool_use_id: "toolu_9",
      content: `Run ${RUN_ID} was aborted by the host (limit_exceeded): the history cap`,
      is_error: true,
    });
    expect(server.requests[0]?.headers["idempotency-key"]).toBe(runKey("toolu_9"));
  });

  it("answer OpenAI tool calls, whose arguments are a JSON string", async () => {
    server.on("POST", "/v1/check", json({ ok: true, diagnostics: [] }));
    const message = await tools.openai.handle({
      id: "call_1",
      type: "function",
      function: { name: "check_script", arguments: '{"code":"x"}' },
    });
    expect(message).toEqual({ role: "tool", tool_call_id: "call_1", content: "The script type-checks." });
    const output = await tools.openai.responses.handle({
      type: "function_call",
      call_id: "call_2",
      name: "check_script",
      arguments: '{"code":"y"}',
    });
    expect(output).toEqual({ type: "function_call_output", call_id: "call_2", output: "The script type-checks." });
  });
});

describe("secrets", () => {
  /** A secret as the API lists it: its metadata, never its value. */
  function listed(reference: string, labels?: Record<string, string>): ListedSecret {
    const [scope, name] = reference.split("/") as ["org" | "ns", string];
    return {
      name,
      scope,
      reference,
      ...(scope === "ns" ? { namespace: "alice" } : {}),
      ...(labels ? { labels } : {}),
      createdBy: "user:u",
      createdAt: "2026-11-19T10:00:00Z",
      updatedBy: "user:u",
      updatedAt: "2026-11-19T10:00:00Z",
      reads: [],
    };
  }

  function withSecrets(secrets: AgentSecrets | undefined): AgentTools {
    const client = rewind.namespace(NAMESPACE);
    return new AgentToolsClass(
      {
        runs: client.runs,
        scripts: client.scripts,
        toolsets: client.toolsets,
        workflows: client.workflows,
        tests: client.tests,
        payloads: client.payloads,
        secrets,
      },
      { toolset: TOOLSET, labels: LABELS },
    );
  }

  it("list_secrets: what the agent may attach, never a value", async () => {
    const agent = withSecrets({
      list: async () => ({ secrets: [listed("org/zendesk", { env: "prod" }), listed("org/linear")] }),
      ofNamespace: async () => ({ secrets: [listed("ns/github")] }),
    });
    const result = await agent.handle("list_secrets", {});
    expect(result.isError).toBe(false);
    // The same text as the Rust port's (`crates/rewind-agent-tools/tests/tools.rs`).
    expect(result.content).toBe(
      'The secrets you may attach, never a value:\n- org/zendesk, the organization\'s (labels env=prod)\n- org/linear, the organization\'s\n- ns/github, the end user\'s in the namespace alice\nAttach one in `secrets`, by the MCP server or API whose calls need it, such as {"github": "ns/github"}.',
    );
  });

  it("list_secrets: leaves out what the caller may not read, and says when nothing exists", async () => {
    const forbidden = new RewindApiError({
      type: "urn:rewind:problem:forbidden",
      title: "Forbidden",
      status: 403,
      detail: "an end user's token reaches its own namespace's secrets only",
    });
    const token = withSecrets({
      list: async () => {
        throw forbidden;
      },
      ofNamespace: async () => ({ secrets: [listed("ns/github")] }),
    });
    expect((await token.handle("list_secrets", {})).content).toBe(
      'The secrets you may attach, never a value:\n- ns/github, the end user\'s in the namespace alice\nAttach one in `secrets`, by the MCP server or API whose calls need it, such as {"github": "ns/github"}.',
    );
    const none = await withSecrets(undefined).handle("list_secrets", {});
    expect(none).toEqual({
      content:
        "No secret you may attach exists yet: a person puts one first, the end user's in their namespace or the organization's.",
      isError: false,
    });
  });

  it("go with run_script, retry_run and save_workflow", async () => {
    const attached = { github: "ns/github", zendesk: "org/zendesk" };
    server.on("POST", "/v1/runs", started());
    await tools.handle("run_script", { code: "a", secrets: attached }, { toolCallId: "c1" });
    expect((server.calls("POST", "/v1/runs")[0]?.body as { secrets: unknown }).secrets).toEqual(attached);

    serveRun(run({ status: "failed" }));
    server.on("POST", "/v1/runs/{id}/fork", started({ id: "fork-1", parent: { run: RUN_ID } }));
    await tools.handle("retry_run", { run_id: RUN_ID, code: "b", secrets: { github: "org/github" } });
    expect(server.calls("POST", `/v1/runs/${RUN_ID}/fork`)[0]?.body).toEqual({
      script: "b",
      labels: LABELS,
      wait: "30s",
      secrets: { github: "org/github" },
    });

    server.on("POST", "/v1/workflows", json(workflow(), 201));
    server.on("POST", "/v1/toolsets", json(toolset("de".repeat(32), [workflow()]), 201));
    await tools.handle("save_workflow", {
      name: "triage",
      description: "Triage.",
      code: "c",
      input: {},
      output: {},
      secrets: { github: "ns/github" },
    });
    expect((server.calls("POST", "/v1/workflows")[0]?.body as { secrets: unknown }).secrets).toEqual({
      github: "ns/github",
    });

    const refused = await tools.handle("run_script", { code: "a", secrets: { github: 1 } });
    expect(refused.isError).toBe(true);
    expect(refused.content).toMatch(/^run_script: `secrets` must be an object of secrets by MCP server or API/);
  });

  it("a start refused for its secrets tells the model what it may attach", async () => {
    server.on("POST", "/v1/runs", () =>
      problem(
        422,
        // `secret-required` joins the SDK's problem slugs with the ts workstream.
        "secret-required" as never,
        'the script imports github, which needs a secret: attach one in "secrets" ("github": "org/<name>" or "ns/<name>"). The secrets you may attach: ns/github',
      ),
    );
    const result = await tools.handle("run_script", { code: "a" });
    expect(result.isError).toBe(true);
    expect(result.content).toBe(
      'run_script failed: the script imports github, which needs a secret: attach one in "secrets" ("github": "org/<name>" or "ns/<name>"). The secrets you may attach: ns/github',
    );
  });
});
