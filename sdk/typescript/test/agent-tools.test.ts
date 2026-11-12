import { createHash } from "node:crypto";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { AGENT_TOOL_NAMES, Ptc, PtcApiError, PtcError, type AgentTools, type Run } from "../src/index.js";
import { RUN_ID, TOOLSET, payload, run } from "./fixtures.js";
import { MockServer, json, problem } from "./mock-server.js";

const LABELS = { conversation: "c-42" };
let server: MockServer;
let ptc: Ptc;
let tools: AgentTools;

beforeEach(async () => {
  server = await MockServer.start();
  ptc = new Ptc({ url: server.url, apiKey: "ptc_sk_agent" });
  tools = ptc.agentTools({ toolset: TOOLSET, labels: LABELS });
  server.on(
    "GET",
    "/v1/runs/{id}/trace",
    json({ format: "llm", runId: RUN_ID, text: "#0 run_started\n#1 tool github.get_issue → error" }),
  );
});

afterEach(async () => {
  await server.close();
});

function serveRun(state: Run): void {
  server.on("GET", "/v1/runs/{id}", (request) =>
    request.params["id"] === state.id ? json(state) : problem(404, "not-found", `no run ${request.params["id"]}`),
  );
}

describe("the agent tools' definitions", () => {
  it("are all there with a toolset, and search_tools only with one", () => {
    expect(tools.definitions.map((d) => d.name)).toEqual([...AGENT_TOOL_NAMES]);
    const withoutToolset = ptc.agentTools({ labels: LABELS });
    expect(withoutToolset.has("search_tools")).toBe(false);
    expect(withoutToolset.has("run_script")).toBe(true);
    expect(() => ptc.agentTools({ tools: ["search_tools"] })).toThrow(TypeError);
    expect(() => ptc.agentTools({ tools: ["nope" as never] })).toThrow(TypeError);
    expect(ptc.agentTools({ tools: ["run_script", "get_run"] }).definitions.map((d) => d.name)).toEqual([
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
});

describe("run_script", () => {
  it("starts the run with the agent's labels and the tool call id, and returns its output and logs", async () => {
    server.on(
      "POST",
      "/v1/runs",
      json(
        run({
          output: payload({ closed: 3 }),
          logs: [{ at: "t", idx: 2, level: "info", message: "found 12 issues", callsite: { line: 4, column: 3 } }],
        }),
        201,
      ),
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
    expect(request?.headers["idempotency-key"]).toBe("toolu_01");
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
    const small = ptc.agentTools({ toolset: TOOLSET, labels: LABELS, outputLimit: 32 });
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
    expect(server.calls("GET", `/v1/payloads/${big.output!.digest}`)).toHaveLength(1);
  });

  it("says what a suspended run waits for", async () => {
    const suspended = run({
      status: "active",
      waitingFor: [
        { kind: "signal", name: "approval", callsite: { line: 8, column: 26 }, timeoutAt: "2026-11-15T10:00:00Z" },
        { kind: "tool_call", seq: 4, tool: "github.delete_repo", taskId: "t1", state: "approval", attempt: 1 },
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
          { code: "ptc/absent-global", message: "fetch does not exist", severity: "error", hint: "use the http tool" },
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
        "error ptc/absent-global: fetch does not exist",
        "hint: use the http tool",
      ].join("\n"),
    );
  });

  it("hashes a tool call id the server would refuse as an idempotency key", async () => {
    server.on("POST", "/v1/runs", json(run(), 201));
    const id = "call_é".padEnd(300, "x");
    await tools.handle("run_script", { code: "x" }, { toolCallId: id });
    expect(server.requests[0]?.headers["idempotency-key"]).toBe(createHash("sha256").update(id).digest("hex"));
  });

  it("bounds the model's wait", async () => {
    server.on("POST", "/v1/runs", json(run(), 201));
    const bounded = ptc.agentTools({ labels: LABELS, wait: "10s", maxWait: "20s" });
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
    await expect(tools.handle("run_script", { code: "x" })).rejects.toBeInstanceOf(PtcApiError);
    server.on("POST", "/v1/runs", problem(401, "unauthorized", "unknown key"));
    await expect(tools.handle("run_script", { code: "x" })).rejects.toMatchObject({ status: 401 });
    await expect(tools.handle("delete_everything", {})).rejects.toBeInstanceOf(PtcError);
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
        waitingFor: [{ kind: "tool_call", seq: 2, tool: "slack.post", taskId: "t", state: "claimed", attempt: 2 }],
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
    const small = ptc.agentTools({ labels: LABELS, outputLimit: 64 });
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
    expect(server.calls("POST", `/v1/runs/${RUN_ID}/cancel`)[0]?.body).toEqual({ wait: 30_000 });
  });

  it("retry_run: forks the run with the corrected script", async () => {
    serveRun(run({ status: "failed" }));
    server.on(
      "POST",
      "/v1/runs/{id}/fork",
      json(run({ id: "fork-1", parent: { run: RUN_ID }, output: payload("ok") }), 201),
    );
    const result = await tools.handle("retry_run", { run_id: RUN_ID, code: "fixed" }, { toolCallId: "call_9" });
    expect(result.content).toBe(`Run fork-1 (a fork of run ${RUN_ID}) completed.\nOutput:\n"ok"`);
    const fork = server.calls("POST", `/v1/runs/${RUN_ID}/fork`)[0];
    expect(fork?.headers["idempotency-key"]).toBe("call_9");
    expect(fork?.body).toEqual({ script: "fixed", labels: LABELS, wait: "30s" });
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
            code: "ptc/floating-promise",
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
        "script.ts:2:3 warning ptc/floating-promise: not awaited",
      ].join("\n"),
    );
    expect(server.requests[0]?.body).toEqual({ script: "x", toolset: TOOLSET });
  });

  it("test_script: runs the scenario, against a recorded run when asked", async () => {
    serveRun(run());
    server.on("GET", "/v1/runs/{id}/journal", (request) => json({ version: 1, kind: request.query.get("as") }));
    server.on(
      "POST",
      "/v1/test",
      json({
        passed: false,
        status: "completed",
        output: { closed: 2 },
        calls: { "github.close_issue": 2 },
        failures: ["the output's closed is 2, expected 3"],
        trace: "#0 run_started",
      }),
    );
    const result = await tools.handle("test_script", {
      code: "x",
      scenario: {
        input: { repo: "acme/api" },
        from_run: RUN_ID,
        tools: { "github.list_issues": [] },
        signals: [{ name: "approval", payload: { close: true } }],
        expect: { status: "completed", output: { closed: 3 } },
      },
    });
    expect(result.isError).toBe(true);
    expect(result.content).toBe(
      [
        "The test failed: 1 expectation do not hold:",
        "- the output's closed is 2, expected 3",
        "The run completed.",
        'Output: {"closed":2}',
        "Calls: github.close_issue ×2",
        "Trace:",
        "#0 run_started",
      ].join("\n"),
    );
    expect(server.calls("POST", "/v1/test")[0]?.body).toEqual({
      script: "x",
      toolset: TOOLSET,
      input: { repo: "acme/api" },
      tools: { "github.list_issues": [] },
      signals: [{ name: "approval", payload: { close: true } }],
      expect: { status: "completed", output: { closed: 3 } },
      cassette: { version: 1, kind: "cassette" },
    });
    expect((await tools.handle("test_script", { code: "x", scenario: [] })).content).toBe(
      "test_script: `scenario` must be an object.",
    );
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

describe("the adapters", () => {
  it("answer an Anthropic tool_use block with a tool_result block", async () => {
    server.on(
      "POST",
      "/v1/runs",
      json(run({ status: "aborted", abort: { kind: "limit_exceeded", message: "the history cap" } }), 201),
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
    expect(server.requests[0]?.headers["idempotency-key"]).toBe("toolu_9");
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
