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
  type RunWorkflow,
  type Workflow,
  type WorkflowVersion,
} from "../src/index.js";
import { COMMENT_NOTE, hasComment } from "../src/agent-tools/render.js";
import { HANDLE, NAMESPACE, RUN_ID, payload } from "./fixtures.js";
import { MockServer, json, problem, type MockHandler } from "./mock-server.js";

const LABELS = { conversation: "c-42" };
/** The action set the agent's code imports. */
const ACTION_SET = "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4";
/** The workflow `triage`, at its version 3. */
const ID = "wf_7k2m9q4x8d1c";
const DIGEST = "4be1c07a9d3e" + "0".repeat(52);
const TRIAGE: RunWorkflow = { id: ID, name: "triage", version: 3, digest: DIGEST };

const sha256 = (text: string) => createHash("sha256").update(text).digest("hex");

/** The key of the run an agent with `LABELS` starts for a tool call. */
const runKey = (toolCallId: string) => sha256(JSON.stringify([[["conversation", "c-42"]], toolCallId]));

/** A run as the API answers it. */
function run(overrides: Partial<Run> = {}): Run {
  return {
    id: RUN_ID,
    handle: HANDLE,
    namespace: NAMESPACE,
    status: "completed",
    createdAt: "2026-11-12T10:00:00Z",
    updatedAt: "2026-11-12T10:00:01Z",
    expiresAt: "2026-12-12T10:00:00Z",
    epoch: 1,
    historyLen: 4,
    input: payload({}),
    labels: { conversation: "c-42" },
    mode: "live",
    codeDigest: "ab".repeat(32),
    actionSet: ACTION_SET,
    traceId: "4bf92f3577b34da6a3ce929d0e0e4736",
    ...overrides,
  };
}

/** A version of `triage`, with an interface. */
function version(overrides: Partial<WorkflowVersion> = {}): WorkflowVersion {
  return {
    version: 3,
    digest: DIGEST,
    codeDigest: "cd".repeat(32),
    actionSet: ACTION_SET,
    input: { type: "object", properties: { repo: { type: "string" } }, required: ["repo"] },
    output: { type: "object", properties: { closed: { type: "integer" } }, required: ["closed"] },
    signals: { approval: { type: "object", properties: { close: { type: "boolean" } }, required: ["close"] } },
    labels: { conversation: "c-42" },
    secrets: { github: "ns/github" },
    createdAt: "2026-11-12T10:00:00Z",
    declarations: {
      dts: 'declare module "rewind:workflows" {\n  export const triage: Workflow<{ repo: string }, { closed: number }, { approval: { close: boolean } }>;\n}',
      prompt:
        'triage.start({ repo: string }) → run · run.signal("approval", { close: boolean }) · run.result() → { closed: number } · Triage the stale issues of a repository, after approval.',
    },
    tests: [],
    ...overrides,
  };
}

/** The workflow `triage` of the namespace user_2fQ81, its versions newest first: the first one shown. */
function workflow(overrides: Partial<Workflow> = {}, versions: WorkflowVersion[] = [version()]): Workflow {
  return {
    id: ID,
    shortId: "wf-7k2m9q",
    name: "triage",
    description: "Triage the stale issues of a repository, after approval.",
    namespace: NAMESPACE,
    labels: { conversation: "c-42" },
    createdAt: "2026-11-10T10:00:00Z",
    version: versions[0]!,
    code: "export default async () => ({ closed: 0 });\n",
    versions,
    suite: [],
    ...overrides,
  };
}

/** The workflow a code's first run made: no name, version 1. */
function unnamed(code: string): RunWorkflow {
  return { id: "wf_c0de00000000", version: 1, digest: sha256(code) };
}

let server: MockServer;
let rewind: Rewind;
let tools: AgentTools;

beforeEach(async () => {
  server = await MockServer.start();
  rewind = new Rewind({ url: server.url, apiKey: "rewind_sk_agent" });
  tools = rewind.namespace(NAMESPACE).agentTools({ actionSet: ACTION_SET, labels: LABELS });
  server.on(
    "GET",
    "/v1/runs/{id}/transcript",
    json({ runId: RUN_ID, text: "#0 run_started\n#1 call github.get_issue → error" }),
  );
});

afterEach(async () => {
  await server.close();
});

/** A start or a fork answered as the server answers it: a run of the code it was sent. */
function started(overrides: Partial<Run> = {}): MockHandler {
  return (request) => {
    const code = (request.body as { code: string }).code;
    return json(run({ codeDigest: sha256(code), ...overrides }), 201);
  };
}

/** Answers `GET /v1/runs/{id}` with `state`, by its id or its handle. */
function serveRun(state: Run): void {
  server.on("GET", "/v1/runs/{id}", (request) =>
    request.params["id"] === state.id || request.params["id"] === state.handle
      ? json(state)
      : problem(404, "not-found", `no run ${request.params["id"]}`),
  );
}

describe("the agent tools' definitions", () => {
  it("are all there with an action set, and search_actions only with one", async () => {
    expect(tools.definitions.map((d) => d.name)).toEqual([...AGENT_TOOL_NAMES]);
    const withoutActionSet = rewind.agentTools({ labels: LABELS });
    expect(withoutActionSet.definitions.map((d) => d.name)).toEqual([
      "run_workflow",
      "check_workflow",
      "test_workflow",
      "get_run",
      "list_runs",
      "signal_run",
      "cancel_run",
      "retry_run",
      "list_secrets",
      "update_workflow",
      "get_workflow",
      "record_tests",
    ]);
    expect(withoutActionSet.has("search_actions")).toBe(false);
    await expect(withoutActionSet.handle("search_actions", {})).rejects.toBeInstanceOf(RewindError);
    await expect(withoutActionSet.handle("save_workflow", {})).rejects.toBeInstanceOf(RewindError);
    expect(() => rewind.agentTools({ tools: ["search_actions"] })).toThrow("search_actions needs an action set");
    expect(() => rewind.agentTools({ tools: ["list_organizations" as never] })).toThrow(
      /^unknown agent tool "list_organizations"/,
    );
    expect(rewind.agentTools({ tools: ["run_workflow", "get_run"] }).definitions.map((d) => d.name)).toEqual([
      "run_workflow",
      "get_run",
    ]);
  });

  it("come in the Anthropic and OpenAI formats", () => {
    const runWorkflow = tools.definitions[0]!;
    expect(tools.anthropic.tools[0]).toEqual({
      name: "run_workflow",
      description: runWorkflow.description,
      input_schema: runWorkflow.inputSchema,
    });
    expect(tools.openai.tools[0]).toEqual({
      type: "function",
      function: { name: "run_workflow", description: runWorkflow.description, parameters: runWorkflow.inputSchema },
    });
    expect(tools.openai.responses.tools[0]).toEqual({
      type: "function",
      name: "run_workflow",
      description: runWorkflow.description,
      parameters: runWorkflow.inputSchema,
      strict: false,
    });
    for (const definition of tools.definitions) {
      expect(definition.inputSchema).toMatchObject({ type: "object", additionalProperties: false });
    }
  });

  it("speak the words of the workflow model only", () => {
    for (const definition of tools.definitions) {
      const text = `${definition.description} ${JSON.stringify(definition.inputSchema)}`;
      const words = text.split(/[^\p{L}\p{N}_]+/u);
      for (const word of ["save_workflow", "script", "scripts", "title", "webhook", "toolset", "save", "saved"]) {
        expect(words, `${definition.name}: ${word}`).not.toContain(word);
      }
      expect(text, definition.name).not.toContain("{ from");
    }
    expect(tools.definitions.find((d) => d.name === "list_runs")?.inputSchema.properties?.["status"]).toMatchObject({
      description: "Only runs with this status.",
    });
  });

  it("name a workflow by its id or its name", () => {
    const definition = (name: string) => tools.definitions.find((d) => d.name === name)!;
    // Code or a workflow: the handler says which is missing.
    expect(definition("run_workflow").inputSchema.required).toBeUndefined();
    expect(definition("test_workflow").inputSchema.required).toBeUndefined();
    expect(definition("check_workflow").inputSchema.required).toEqual(["code"]);
    for (const name of ["update_workflow", "get_workflow", "record_tests"]) {
      expect(definition(name).inputSchema.required, name).toEqual(["workflow"]);
    }
    for (const name of ["run_workflow", "test_workflow", "update_workflow", "get_workflow", "record_tests"]) {
      const workflow = definition(name).inputSchema.properties?.["workflow"] as { description: string };
      expect(workflow.description, name).toContain("its id (wf_…) or its name");
    }
    // The comment a workflow without a name shows as.
    for (const name of ["run_workflow", "check_workflow"]) {
      expect(definition(name).description, name).toContain("is how the workflow shows until it has a name");
    }
    const update = definition("update_workflow");
    expect(update.inputSchema.properties?.["name"]).toMatchObject({ type: ["string", "null"] });
    expect(update.description).toContain("the version is made only if every test passes");
    expect(update.description).toContain("retire the test when the behavior is deprecated");
    expect(update.description).toContain("replace it when its state or its expected result is out of date");
  });

  it("describe the test document: its world, its signals and its expectations, or a recorded test", () => {
    const testWorkflow = tools.definitions.find((d) => d.name === "test_workflow")!;
    const tests = testWorkflow.inputSchema.properties?.["tests"] as { items: { anyOf: Array<Record<string, any>> } };
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
    expect(Object.keys(inline?.["properties"].world.properties)).toEqual(["actions", "handlers"]);
    const rule = inline?.["properties"].world.properties.actions.additionalProperties.anyOf[0];
    expect(Object.keys(rule.properties)).toEqual(["args", "returns", "error", "times"]);
    expect(Object.keys(inline?.["properties"].signals.items.properties)).toEqual(["name", "payload", "when"]);
    const expectations = inline?.["properties"].expect;
    expect(Object.keys(expectations.properties)).toEqual(["status", "output", "error", "calls", "order"]);
    expect(Object.keys(expectations.properties.calls.items.properties)).toEqual(["action", "args", "times"]);
    expect(digest).toMatchObject({ type: "string" });
    expect(from).toMatchObject({ type: "object", required: ["test", "expect"] });

    const update = tools.definitions.find((d) => d.name === "update_workflow")!;
    expect(Object.keys(update.inputSchema.properties ?? {})).toEqual([
      "workflow",
      "name",
      "description",
      "code",
      "input",
      "output",
      "signals",
      "secrets",
      "tests",
      "replace",
      "retire",
    ]);
    const replace = (update.inputSchema.properties?.["replace"] as { items: Record<string, any> }).items;
    expect(Object.keys(replace["properties"])).toEqual(["name", "reason", "new", "expect"]);
    expect(replace["required"]).toEqual(["name", "reason"]);
  });
});

describe("run_workflow", () => {
  it("starts code with the agent's action set, labels and tool call id, and returns its output and logs", async () => {
    server.on("POST", "/v1/runs", (request) =>
      started({
        workflow: unnamed((request.body as { code: string }).code),
        output: payload({ closed: 3 }),
        logs: [{ at: "t", idx: 2, level: "info", message: "found 12 issues", callsite: { line: 4, column: 3 } }],
      })(request),
    );
    const code = "export default async () => ({ closed: 3 })";
    const result = await tools.handle(
      "run_workflow",
      { code, input: { repo: "acme/api" } },
      { toolCallId: "toolu_01", traceparent: "00-aa-bb-01" },
    );
    expect(result.isError).toBe(false);
    expect(result.code).toBe("passed");
    expect(result.run?.id).toBe(RUN_ID);
    // The result names the workflow the code became, by its id and its version.
    expect(result.content).toBe(
      [
        `Run ${HANDLE} (a run of the workflow wf_c0de00000000@v1) completed.`,
        "Output:",
        '{"closed":3}',
        "Last log lines:",
        "  [info] at workflow.ts:4:3 found 12 issues",
      ].join("\n"),
    );
    const request = server.calls("POST", "/v1/runs")[0];
    expect(request?.headers["idempotency-key"]).toBe(runKey("toolu_01"));
    expect(request?.body).toEqual({
      code,
      actionSet: ACTION_SET,
      input: { repo: "acme/api" },
      wait: "30s",
      labels: LABELS,
      traceparent: "00-aa-bb-01",
    });
  });

  it("runs a workflow by its name, with its version's action set", async () => {
    server.on("POST", "/v1/runs", json(run({ workflow: TRIAGE, output: payload(1) }), 201));
    const result = await tools.handle(
      "run_workflow",
      { workflow: "triage@v3", input: { repo: "acme/api" } },
      { toolCallId: "call_1" },
    );
    expect(result.isError).toBe(false);
    expect(result.code).toBeUndefined();
    expect(result.content.split("\n")[0]).toBe(`Run ${HANDLE} (a run of the workflow triage@v3, id ${ID}) completed.`);
    expect(server.calls("POST", "/v1/runs")[0]?.body).toEqual({
      workflow: "triage@v3",
      input: { repo: "acme/api" },
      wait: "30s",
      labels: LABELS,
    });

    for (const [input, said] of [
      [
        { code: "x", workflow: "triage" },
        "run_workflow: give `code` or `workflow`, not both: `code` runs new code, `workflow` an existing workflow by its id or its name.",
      ],
      [{ input: {} }, "run_workflow: give the `code` to run, or a `workflow` by its id or its name."],
      [
        { workflow: "triage", secrets: { github: "ns/github" } },
        "run_workflow: `secrets` goes with `code`: a run of a workflow uses the secrets its version names.",
      ],
    ] as const) {
      expect(await tools.handle("run_workflow", input)).toEqual({ content: said, isError: true });
    }
    expect(server.requests).toHaveLength(1);
  });

  it("bounds the output and says how to read the rest", async () => {
    const small = rewind.agentTools({ actionSet: ACTION_SET, labels: LABELS, outputLimit: 32 });
    const output = { items: "é".repeat(40) };
    serveRun(run({ output: payload(output) }));
    server.on("POST", "/v1/runs", json(run({ output: payload(output) }), 201));
    const result = await small.handle("run_workflow", { code: "x" });
    const full = JSON.stringify(output);
    expect(result.content).toContain(`Output (${Buffer.byteLength(full)} bytes; the first 32 are shown`);
    expect(result.content).toContain(`get_run({"run_id":"${HANDLE}","offset":32})`);

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
    const result = await tools.handle("run_workflow", { code: "x" });
    expect(result.content).toContain('{"rows":[1,2,3]}');
    const reads = server.calls("GET", `/v1/payloads/${big.output!.digest}`);
    expect(reads).toHaveLength(1);
    expect(reads[0]?.query.get("run")).toBe(big.id);
  });

  it("says what a waiting run waits for", async () => {
    const waiting = run({
      status: "active",
      waitingFor: [
        { kind: "signal", name: "approval", callsite: { line: 8, column: 26 }, timeoutAt: "2026-11-15T10:00:00Z" },
        {
          kind: "call",
          seq: 4,
          action: "github.delete_repo",
          callId: "t1",
          state: "approval",
          delivery: 1,
          args: payload({}),
        },
        { kind: "timer", seq: 5, fireAt: "2026-11-20T00:00:00Z" },
      ],
    });
    serveRun(waiting);
    server.on("POST", "/v1/runs", json(waiting, 201));
    const result = await tools.handle("run_workflow", { code: "x", wait: "300ms" });
    expect(result.isError).toBe(false);
    expect(result.content).toBe(
      [
        `Run ${HANDLE} is suspended, waiting for:`,
        '- the signal "approval" at workflow.ts:8:26, until 2026-11-15T10:00:00Z (then it gets undefined): send it with signal_run',
        "- the call 4 to github.delete_repo: it waits for the harness's approval",
        "- a timer, firing at 2026-11-20T00:00:00Z",
        `Follow it with get_run({"run_id":"${HANDLE}"}), send it a signal with signal_run, or cancel it with cancel_run.`,
      ].join("\n"),
    );
    expect(server.requests[0]?.body).toMatchObject({ wait: expect.stringMatching(/^\d+ms$/) });
  });

  it("returns the error, the failed call and the transcript of a failed run", async () => {
    server.on(
      "POST",
      "/v1/runs",
      json(
        run({
          status: "failed",
          error: {
            name: "ActionError",
            message: "Not Found",
            location: { line: 12, column: 9 },
            frame: "12 |   const issue = await github.get_issue({ number: 7 });\n   |         ^",
            failedCall: {
              action: "github.get_issue",
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
    const result = await tools.handle("run_workflow", { code: "x" });
    expect(result.isError).toBe(true);
    expect(result.code).toBe("passed");
    expect(result.content).toBe(
      [
        `Run ${HANDLE} failed.`,
        "ActionError: Not Found at workflow.ts:12:9",
        "12 |   const issue = await github.get_issue({ number: 7 });\n   |         ^",
        'The call that failed: github.get_issue (call 1 at workflow.ts:12:9) with {"number":7}',
        "  error: Not Found",
        "Transcript up to the failure:",
        "#0 run_started\n#1 call github.get_issue → error",
      ].join("\n"),
    );
  });

  it("returns the diagnostics of refused code", async () => {
    server.on(
      "POST",
      "/v1/runs",
      problem(422, "invalid-code", "the code does not type-check", {
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
            hint: "use the http action",
          },
        ],
      }),
    );
    const result = await tools.handle("run_workflow", { code: "x" });
    expect(result.isError).toBe(true);
    expect(result.code).toBe("refused");
    expect(result.content).toBe(
      [
        "The code was refused before anything ran: the code does not type-check",
        "workflow.ts:3:28 error TS2322: Type 'number' is not assignable to type 'string'.",
        "3 | github.list_issues({ repo: 42 })\n  |                            ^^",
        "",
        "error rewind/absent-global: fetch does not exist",
        "hint: use the http action",
      ].join("\n"),
    );
  });

  it("derives the idempotency key from the agent's labels and the tool call id", async () => {
    server.on("POST", "/v1/runs", started());
    const id = "call_é".padEnd(300, "x");
    await tools.handle("run_workflow", { code: "x" }, { toolCallId: id });
    expect(server.requests[0]?.headers["idempotency-key"]).toBe(runKey(id));
    // Two agents whose harnesses number their tool calls alike never share a key.
    server.on("POST", "/v1/runs", (request) => {
      const body = request.body as { code: string; labels: Run["labels"] };
      return json(run({ codeDigest: sha256(body.code), labels: body.labels }), 201);
    });
    await rewind.agentTools({ labels: { agent: "a" } }).handle("run_workflow", { code: "x" }, { toolCallId: "call_0" });
    await rewind.agentTools({ labels: { agent: "b" } }).handle("run_workflow", { code: "x" }, { toolCallId: "call_0" });
    await rewind
      .agentTools({ labels: { b: "2", a: "1" } })
      .handle("run_workflow", { code: "x" }, { toolCallId: "call_0" });
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
    await bounded.handle("run_workflow", { code: "x", wait: "1h" });
    await bounded.handle("run_workflow", { code: "x" });
    expect(server.requests.map((r) => (r.body as { wait: string }).wait)).toEqual(["20s", "10s"]);
  });

  it("tells the model what is wrong with its arguments", async () => {
    expect(await tools.handle("run_workflow", { code: "" })).toEqual({
      content: "run_workflow: `code` must be a non-empty string.",
      isError: true,
    });
    expect((await tools.handle("run_workflow", "{not json")).content).toBe(
      "run_workflow: the arguments of run_workflow are not valid JSON.",
    );
    expect((await tools.handle("run_workflow", { code: "x", wait: "soon" })).content).toMatch(
      /`wait` is not a duration/,
    );
    expect(server.requests).toHaveLength(0);
  });

  it("throws on faults of the platform and on unknown tools", async () => {
    server.on("POST", "/v1/runs", problem(500, "internal", "the store is down"));
    await expect(tools.handle("run_workflow", { code: "x" })).rejects.toBeInstanceOf(RewindApiError);
    server.on("POST", "/v1/runs", problem(401, "unauthorized", "unknown key"));
    await expect(tools.handle("run_workflow", { code: "x" })).rejects.toMatchObject({ status: 401 });
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
      .on("POST", "/v1/workflows/{id}/record", json({ tests: [], skipped: [] }))
      .on(
        "GET",
        "/v1/workflows/{id}",
        json(workflow({ suite: [{ name: "t", digest: "ab".repeat(32), addedAt: "2026-11-12T10:00:00Z" }] })),
      )
      .on("GET", "/v1/tests/{digest}", json({ name: "t" }));
    serveRun(run());
    await tools.handle("run_workflow", { code: "x" }, { toolCallId: "call_0" });
    await tools.handle("check_workflow", { code: "x" });
    await tools.handle("test_workflow", { code: "x", tests: ["ab"] });
    await tools.handle("list_runs", {});
    await tools.handle("get_run", { run_id: RUN_ID });
    await tools.handle("record_tests", { workflow: "triage" });
    await tools.handle("get_workflow", { workflow: "triage" });
    expect(server.requests.length).toBeGreaterThanOrEqual(9);
    for (const request of server.requests) {
      expect(request.headers["rewind-namespace"], `${request.method} ${request.path}`).toBe(NAMESPACE);
    }
  });

  it("is the token's, for a client holding an end user's token", async () => {
    server.on("POST", "/v1/runs", started());
    const asUser = new Rewind({ url: server.url, token: "rewind_nt_alice" });
    const result = await asUser.agentTools({ labels: LABELS }).handle("run_workflow", { code: "x" });
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
    await expect(unbound.handle("run_workflow", { code: "x" })).rejects.toBeInstanceOf(NamespaceRequiredError);
    expect((await tools.handle("run_workflow", { code: "x" })).isError).toBe(false);
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
    expect(await tools.handle("run_workflow", { code: "x" })).toEqual({
      content: "run_workflow failed: This end user has 5 runs active, their limit: wait for one to end.",
      isError: true,
    });
  });
});

describe("a run answered for the tool call's idempotency key", () => {
  it("is reported when it is the run this call asked for", async () => {
    server.on("POST", "/v1/runs", json(run({ codeDigest: sha256("x"), output: payload(1) })));
    const result = await tools.handle("run_workflow", { code: "x" }, { toolCallId: "call_0" });
    expect(result.content).toBe(`Run ${HANDLE} completed.\nOutput:\n1`);
    server.on("POST", "/v1/runs", json(run({ workflow: TRIAGE, output: payload(1) })));
    for (const reference of ["triage", ID, "triage@v3", `triage@${DIGEST.slice(0, 12)}`]) {
      const named = await tools.handle("run_workflow", { workflow: reference }, { toolCallId: "call_1" });
      expect(named.isError, reference).toBe(false);
    }
  });

  it("is never shown when another agent's labels started it, nor when other code or another workflow did", async () => {
    // A harness whose tool call ids repeat (call_0, call_1…) across conversations or turns.
    for (const other of [
      run({ codeDigest: sha256("x"), labels: { conversation: "someone-else" }, output: payload("their secret") }),
      run({ codeDigest: sha256("an earlier code"), output: payload("an earlier output") }),
    ]) {
      server.on("POST", "/v1/runs", json(other));
      const answered = tools.handle("run_workflow", { code: "x" }, { toolCallId: "call_0" });
      await expect(answered).rejects.toBeInstanceOf(RewindError);
      await expect(answered).rejects.toThrow(/tool call id "call_0".*another run/);
    }
    server.on("POST", "/v1/runs", json(run({ workflow: { ...TRIAGE, name: "other" } })));
    await expect(tools.handle("run_workflow", { workflow: "triage" }, { toolCallId: "call_2" })).rejects.toThrow(
      /another run \(of another code or workflow/,
    );
    server.on("POST", "/v1/runs", json(run({ workflow: TRIAGE })));
    await expect(tools.handle("run_workflow", { workflow: "triage@v2" }, { toolCallId: "call_3" })).rejects.toThrow(
      /another run/,
    );
  });

  it("is checked by retry_run too", async () => {
    serveRun(run({ status: "failed" }));
    server.on(
      "POST",
      "/v1/runs/{id}/fork",
      json(
        run({
          id: "fork-1",
          codeDigest: sha256("fixed"),
          labels: { conversation: "someone-else" },
          output: payload(2),
        }),
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

  it("stays bounded whatever the code throws", async () => {
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
              action: "github.get_issue",
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
    const result = await tools.handle("run_workflow", { code: "x" });
    expect(bytes(result.content)).toBeLessThan(BOUND);
    expect(result.content).toContain("The call that failed: github.get_issue");
    expect(result.content).toContain("Transcript up to the failure:\n#0 run_started");
  });

  it("stays bounded whatever the diagnostics, the test or the server say", async () => {
    const diagnostics = Array.from({ length: 2_000 }, () => ({
      code: "TS2322",
      message: "x".repeat(2_000),
      severity: "error" as const,
      frame: "x".repeat(2_000),
    }));
    server.on("POST", "/v1/runs", problem(422, "invalid-code", huge, { diagnostics }));
    const refused = await tools.handle("run_workflow", { code: "x" });
    expect(refused.isError).toBe(true);
    expect(bytes(refused.content)).toBeLessThan(BOUND);

    server.on("POST", "/v1/check", json({ ok: false, diagnostics }));
    expect(bytes((await tools.handle("check_workflow", { code: "x" })).content)).toBeLessThan(BOUND);

    const large = "x".repeat(500_000);
    const failed = {
      name: large,
      passed: false,
      problem: "failed",
      status: "failed",
      calls: {},
      failures: [large, large],
      error: { name: "Error", message: large },
      transcript: large,
    };
    server.on("POST", "/v1/test", json({ passed: false, tests: Array.from({ length: 20 }, () => failed) }));
    expect(bytes((await tools.handle("test_workflow", { code: "x", workflow: "triage" })).content)).toBeLessThan(BOUND);

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

  it("get_run: the state and the transcript, after waiting when asked", async () => {
    serveRun(
      run({
        status: "active",
        waitingFor: [
          {
            kind: "call",
            seq: 2,
            action: "slack.post",
            callId: "t",
            state: "claimed",
            delivery: 2,
            args: payload({}),
          },
        ],
      }),
    );
    const result = await tools.handle("get_run", { run_id: RUN_ID, wait: "200ms" });
    expect(result.isError).toBe(false);
    expect(result.content).toBe(
      [
        `Run ${HANDLE} is still running.`,
        "Waiting for:",
        "- the call 2 to slack.post: in progress (delivery 2)",
        "Transcript:",
        "#0 run_started\n#1 call github.get_issue → error",
        `Follow it with get_run({"run_id":"${HANDLE}"}), send it a signal with signal_run, or cancel it with cancel_run.`,
      ].join("\n"),
    );
    const gets = server.calls("GET", `/v1/runs/${RUN_ID}`);
    expect(gets.map((r) => r.query.get("wait"))).toEqual([null, expect.stringMatching(/^\d+ms$/)]);
  });

  it("get_run: one event in full, by pages", async () => {
    serveRun(run());
    const value = { text: "a".repeat(100) };
    server.on("GET", "/v1/runs/{id}/events/{idx}", (request) =>
      json({
        idx: Number(request.params["idx"]),
        recordedAt: "t",
        event: { type: "complete_run", output: payload(value) },
      }),
    );
    const whole = await tools.handle("get_run", { run_id: RUN_ID, event: 3 });
    expect(whole.content.startsWith(`Event 3 of run ${HANDLE}:\n{"idx":3,`)).toBe(true);
    const small = rewind.agentTools({ labels: LABELS, outputLimit: 64 });
    const page = await small.handle("get_run", { run_id: RUN_ID, event: 3, offset: 64 });
    expect(page.content).toMatch(new RegExp(`^Event 3 of run ${HANDLE}, bytes 64 to 128 of \\d+:`));
    expect(page.content).toContain(`get_run({"run_id":"${HANDLE}","event":3,"offset":128})`);
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
      `Sent the signal "approval" to run ${HANDLE}. Read what the run does next with get_run({"run_id":"${HANDLE}","wait":"30s"}).`,
    );
    const again = await tools.handle("signal_run", { run_id: RUN_ID, name: "approval" }, { toolCallId: "call_7" });
    expect(again.content).toBe(`This signal "approval" was already sent to run ${HANDLE}: nothing was added.`);
    const signals = server.calls("POST", `/v1/runs/${RUN_ID}/signals/approval`);
    expect(signals.map((r) => r.headers["idempotency-key"])).toEqual(["call_7", "call_7"]);
    expect(signals[0]?.body).toEqual({ close: true });
    expect(signals[1]?.rawBody).toBe("null");
  });

  it("signal_run: a run of a workflow refuses a signal it does not declare", async () => {
    serveRun(run({ status: "active", workflow: TRIAGE }));
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
      `Run ${HANDLE} refused the signal "ping": run ${RUN_ID} is a run of triage, which does not declare the signal "ping" (it declares approval)`,
    );
  });

  it("get_run: says which workflow a run executes, and which run started it", async () => {
    serveRun(
      run({
        workflow: TRIAGE,
        startedBy: { run: "source-1", handle: "user_2fQ81/wf-0a1b2c/06bqx3k9f2ms", seq: 2, detached: true },
        output: payload({ closed: 1 }),
      }),
    );
    const result = await tools.handle("get_run", { run_id: RUN_ID });
    // A run is named by its handle, and so are the runs it names; its workflow by its name, its id and its version.
    expect(result.content.split("\n")[0]).toBe(
      `Run ${HANDLE} (a run of the workflow triage@v3, id ${ID}; started by run user_2fQ81/wf-0a1b2c/06bqx3k9f2ms, detached) completed.`,
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
    expect(result.content).toBe(`Run ${HANDLE} was cancelled.\nCancelledError: cancelled`);
    expect(server.calls("POST", `/v1/runs/${RUN_ID}/cancel`)[0]?.body).toEqual({ wait: "30s" });
  });

  it("retry_run: forks the run with the corrected code", async () => {
    serveRun(run({ status: "failed" }));
    server.on(
      "POST",
      "/v1/runs/{id}/fork",
      started({
        id: "fork-1",
        handle: "user_2fQ81/triage/06bqx3k9f2mv",
        source: { run: RUN_ID, handle: HANDLE },
        output: payload("ok"),
      }),
    );
    const result = await tools.handle("retry_run", { run_id: RUN_ID, code: "fixed" }, { toolCallId: "call_9" });
    expect(result.content).toBe(
      `Run user_2fQ81/triage/06bqx3k9f2mv (a fork of run ${HANDLE}) completed.\nOutput:\n"ok"`,
    );
    expect(result.code).toBe("passed");
    const fork = server.calls("POST", `/v1/runs/${RUN_ID}/fork`)[0];
    expect(fork?.headers["idempotency-key"]).toBe(runKey("call_9"));
    expect(fork?.body).toEqual({ code: "fixed", labels: LABELS, wait: "30s" });

    // An agent without labels keeps the source's: the fork names none.
    await rewind.namespace(NAMESPACE).agentTools().handle("retry_run", { run_id: RUN_ID, code: "fixed" });
    expect(server.calls("POST", `/v1/runs/${RUN_ID}/fork`)[1]?.body).toEqual({ code: "fixed", wait: "30s" });
  });

  it("retry_run: says which tests of the workflow's suite refused the fix", async () => {
    serveRun(run({ status: "failed", workflow: TRIAGE }));
    const tests = [
      { name: "closes once approved", passed: true, status: "completed", calls: {}, failures: [], transcript: "#0 ok" },
      {
        name: "closes nothing without approval",
        passed: false,
        problem: "failed",
        status: "completed",
        output: { closed: 1 },
        calls: { "github.close_issue": 1, "github.list_issues": 1 },
        failures: ["github.close_issue was called 1 time, expected 0"],
        transcript: "#0 run_started\n#1 call github.close_issue",
      },
    ];
    server.on(
      "POST",
      "/v1/runs/{id}/fork",
      problem(422, "tests-failed", "1 test of the suite of triage did not pass against the corrected code", {
        tests,
      }),
    );
    const result = await tools.handle("retry_run", { run_id: RUN_ID, code: "fixed" });
    expect(result.isError).toBe(true);
    expect(result.tests).toEqual(tests);
    expect(result.run?.id).toBe(RUN_ID);
    // The same text as the Rust port's (`crates/rewind-agent-tools/src/render.rs`).
    expect(result.content).toBe(
      [
        `Run ${HANDLE} was not retried: 1 test of the suite of triage did not pass against the corrected code`,
        "1 of 2 tests did not pass.",
        'PASS "closes once approved"',
        'FAIL "closes nothing without approval" (failed): it ran, and expectations do not hold:',
        "  - github.close_issue was called 1 time, expected 0",
        "  The run completed.",
        '  Output: {"closed":1}',
        "  Calls: github.close_issue ×1, github.list_issues ×1",
        "  Transcript:",
        "#0 run_started\n#1 call github.close_issue",
        'A fix must keep every behavior the suite of triage@v3 pins: there is no way around it. Fix the code, and check it with test_workflow({"code":"…","workflow":"triage@v3"}) before you try again. If a behavior must change on purpose, make the workflow\'s next version with update_workflow, retiring or replacing its tests with a reason, and run it.',
      ].join("\n"),
    );
    const withoutUpdates = rewind.namespace(NAMESPACE).agentTools({
      labels: LABELS,
      tools: ["retry_run"],
    });
    expect((await withoutUpdates.handle("retry_run", { run_id: RUN_ID, code: "fixed" })).content).toMatch(
      /which these tools do not make: tell the person\.$/,
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
            handle: "user_2fQ81/wf-7k2m9q/06bqx3k9f2mr",
            namespace: NAMESPACE,
            status: "failed",
            error: "ActionError",
            errors: 1,
            historyLen: 5,
            labels: LABELS,
            lastActivityAt: "t",
            quarantined: false,
            codeDigest: "s",
            startedAt: "2026-11-12T10:00:00Z",
            calls: 2,
            actionSet: ACTION_SET,
            source: "r0",
          },
        ],
        nextCursor: "more",
      }),
    );
    const result = await tools.handle("list_runs", { status: "failed" });
    expect(result.content).toBe(
      [
        "1 run with the status failed, most recent first:",
        "- user_2fQ81/wf-7k2m9q/06bqx3k9f2mr: failed (ActionError), started 2026-11-12T10:00:00Z, 2 calls, 1 failed, a fork of r0",
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
            handle: "user_2fQ81/wf-7k2m9q/06bqx3k9f2mv",
            namespace: NAMESPACE,
            status: "active",
            errors: 0,
            historyLen: 3,
            labels: LABELS,
            lastActivityAt: "2026-11-12T10:00:00Z",
            quarantined: false,
            codeDigest: "s",
            startedAt: "2026-11-12T10:00:00Z",
            calls: 1,
            actionSet: ACTION_SET,
            waitingFor: [],
            workflow: { id: ID, digest: DIGEST, version: 1 },
            startedBy: "r1",
          },
        ],
      }),
    );
    const result = await tools.handle("list_runs", {});
    expect(result.content).toBe(
      [
        "1 run, most recent first:",
        `- user_2fQ81/wf-7k2m9q/06bqx3k9f2mv: active, started 2026-11-12T10:00:00Z, 1 call, a run of the workflow ${ID}@v1, started by r1`,
      ].join("\n"),
    );
  });

  it("check_workflow: the diagnostics, and the workflow that already has the code", async () => {
    server.on("POST", "/v1/check", json({ ok: true, diagnostics: [] }));
    expect(await tools.handle("check_workflow", { code: "/** Count to one */\nx" })).toEqual({
      content: "The code type-checks.",
      isError: false,
      code: "passed",
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
    const failed = await tools.handle("check_workflow", { code: "// Count to one\nx" });
    expect(failed.isError).toBe(true);
    expect(failed.code).toBe("refused");
    expect(failed.content).toBe(
      [
        "The code does not type-check: 1 error and 1 warning.",
        "workflow.ts:1:1 error TS2304: Cannot find name 'foo'.",
        "",
        "workflow.ts:2:3 warning rewind/floating-promise: not awaited",
      ].join("\n"),
    );
    expect(server.requests[0]?.body).toEqual({ code: "/** Count to one */\nx", actionSet: ACTION_SET });

    server.on("POST", "/v1/check", json({ ok: true, diagnostics: [], workflow: TRIAGE }));
    expect((await tools.handle("check_workflow", { code: "// Count to one, known\nx" })).content).toBe(
      `The code type-checks.\nThis code is already the version v3 of the workflow triage (id ${ID}): running it is a run of that workflow.`,
    );
  });

  it("check_workflow: a note when the code has no comment at its top", async () => {
    server.on("POST", "/v1/check", json({ ok: true, diagnostics: [] }));
    const bare = await tools.handle("check_workflow", { code: "export default async () => 1;" });
    expect(bare).toEqual({ content: `The code type-checks.\n${COMMENT_NOTE}`, isError: false, code: "passed" });
    for (const tagged of ["// @ts-check\nx", "/** */\nx", "x\n/** Too late */"]) {
      expect(hasComment(tagged)).toBe(false);
    }
    for (const commented of [
      "#!/usr/bin/env rewind\n/**\n * Triage\n */\nx",
      "  // Triage\n// More\nx",
      "/* One */ x",
    ]) {
      expect(hasComment(commented)).toBe(true);
    }
  });

  it("get_run: a handle the model gives names the run, percent-encoded in the path", async () => {
    serveRun(run({ output: payload(1) }));
    const result = await tools.handle("get_run", { run_id: HANDLE });
    expect(result.content.split("\n")[0]).toBe(`Run ${HANDLE} completed.`);
    expect(server.requests[0]?.path).toBe(`/v1/runs/${encodeURIComponent(HANDLE)}`);
  });

  it("test_workflow: runs the tests, and the suite of a workflow, against the code", async () => {
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
            transcript: "#0 run_started (a passing test's transcript is not shown)",
          },
          {
            name: `recorded ${RUN_ID}`,
            passed: false,
            problem: "failed",
            status: "completed",
            output: { closed: 2 },
            calls: { "github.close_issue": 2, "github.list_issues": 1 },
            failures: ["the output's closed is 2, expected 3", "github.delete_repo has no recorded response"],
            transcript: "#0 run_started\n#1 call github.list_issues",
          },
          {
            name: "posts to slack",
            passed: false,
            problem: "stale",
            calls: {},
            failures: ["slack.post_message is not one of the actions"],
            transcript: "",
          },
          {
            name: "refuses a bad repo",
            passed: false,
            problem: "incompatible",
            calls: {},
            failures: ["the input does not match the input schema of triage: repo must be a string"],
            transcript: "",
          },
          {
            name: "fails when GitHub is down",
            passed: false,
            problem: "failed",
            status: "failed",
            error: { name: "ActionError", message: "GitHub is down" },
            calls: { "github.list_issues": 1 },
            failures: ["the run failed with ActionError, expected completed"],
            transcript: "#0 run_started\n#1 call github.list_issues → error",
          },
        ],
      }),
    );
    const tests = [
      {
        name: "closes the stale issues once approved",
        input: { repo: "acme/api" },
        world: {
          actions: {
            "github.list_issues": [{ args: { state: "open" }, returns: [{ number: 4 }] }],
            "github.close_issue": { returns: {} },
          },
        },
        signals: [{ name: "approval", payload: { close: true } }],
        expect: { status: "completed", output: { closed: 1 }, calls: [{ action: "github.close_issue", times: 1 }] },
      },
      recorded,
      { test: recorded, expect: { status: "completed" } },
    ];
    const result = await tools.handle("test_workflow", { code: "x", tests, workflow: "triage" });
    expect(result.isError).toBe(true);
    expect(result.code).toBe("passed");
    expect(result.tests).toHaveLength(5);
    // The same text as the Rust port's (`crates/rewind-agent-tools/src/render.rs`).
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
        "  Transcript:",
        "#0 run_started\n#1 call github.list_issues",
        'FAIL "posts to slack" (stale): it names actions, answers or args the action set no longer has; it was not run.',
        "  - slack.post_message is not one of the actions",
        `FAIL "refuses a bad repo" (incompatible): the workflow's input or signal schemas refuse its input or its signals; it was not run.`,
        "  - the input does not match the input schema of triage: repo must be a string",
        'FAIL "fails when GitHub is down" (failed): it ran, and expectations do not hold:',
        "  - the run failed with ActionError, expected completed",
        "  The run failed.",
        "  ActionError: GitHub is down",
        "  Calls: github.list_issues ×1",
        "  Transcript:",
        "#0 run_started\n#1 call github.list_issues → error",
      ].join("\n"),
    );
    expect(server.calls("POST", "/v1/test")[0]?.body).toEqual({
      code: "x",
      actionSet: ACTION_SET,
      tests,
      workflow: "triage",
    });
  });

  it("test_workflow: a workflow's version alone, all passed, and the tests refused before running", async () => {
    server.on(
      "POST",
      "/v1/test",
      json({
        passed: true,
        tests: [
          { name: "a", passed: true, status: "completed", calls: {}, failures: [], transcript: "#0" },
          { name: "b", passed: true, status: "active", calls: {}, failures: [], transcript: "#0" },
        ],
      }),
    );
    // A workflow alone: its version's code and action set.
    expect(await tools.handle("test_workflow", { workflow: ID })).toEqual({
      content: 'All 2 tests passed.\nPASS "a"\nPASS "b"',
      isError: false,
      tests: expect.any(Array),
    });
    expect(server.calls("POST", "/v1/test")[0]?.body).toEqual({ workflow: ID });
    server.on("POST", "/v1/test", problem(422, "invalid-test", 'no stored test "00"; record one with record_tests'));
    expect(await tools.handle("test_workflow", { code: "x", tests: ["00"] })).toEqual({
      content: 'The tests were refused before running: no stored test "00"; record one with record_tests',
      isError: true,
      code: "passed",
    });
  });

  it("test_workflow: tells the model what is wrong with its arguments", async () => {
    expect((await tools.handle("test_workflow", { tests: [] })).content).toBe(
      "test_workflow: give the `code` to test, or a `workflow` whose version the tests run against.",
    );
    expect((await tools.handle("test_workflow", { code: "x" })).content).toBe(
      "test_workflow: give `tests`, or a `workflow` whose suite runs against the code, or both.",
    );
    expect((await tools.handle("test_workflow", { code: "x", tests: {} })).content).toBe(
      "test_workflow: `tests` must be an array of tests: each one an object, or the digest of a recorded test as a string.",
    );
    expect((await tools.handle("test_workflow", { code: "x", tests: [1] })).isError).toBe(true);
    expect((await tools.handle("test_workflow", { code: "x", workflow: 3 })).content).toBe(
      "test_workflow: `workflow` must be a non-empty string.",
    );
    expect(server.requests).toHaveLength(0);
  });

  it("search_actions: the declarations of the matching actions", async () => {
    server.on(
      "GET",
      "/v1/action-sets/{id}/search",
      json({
        actions: [
          {
            name: "github.close_issue",
            description: "Close",
            dts: "close_issue(args: { number: number }): Promise<void>;",
            prompt: "p",
          },
        ],
      }),
    );
    const result = await tools.handle("search_actions", { query: "close" });
    expect(result.content).toBe('1 action matches "close":\n\nclose_issue(args: { number: number }): Promise<void>;');
    expect(server.requests[0]?.path).toBe(`/v1/action-sets/${ACTION_SET}/search`);
    expect(server.requests[0]?.query.get("query")).toBe("close");
  });
});

describe("update_workflow", () => {
  /** The workflow `triage`: its version 3 with one test in its suite, and one test that left it. */
  const suite = [
    { name: "test 0", digest: "00".repeat(32), addedAt: "2026-11-19T10:00:00Z" },
    {
      name: "asks first",
      digest: "ee".repeat(32),
      addedAt: "2026-11-18T10:00:00Z",
      retiredAt: "2026-11-19T10:00:00Z",
      reason: "it no longer asks",
    },
  ];
  const v3 = version({ declarations: null, tests: [{ name: "test 0", digest: "00".repeat(32) }] });
  const triage = workflow({ suite }, [v3, version({ version: 2, digest: "02".repeat(32) })]);
  const v4 = version({
    version: 4,
    digest: "04".repeat(32),
    declarations: null,
    tests: [{ name: "test 0", digest: "00".repeat(32) }],
  });
  const testsFailed = (detail: string) =>
    problem(422, "tests-failed", detail, {
      tests: [
        {
          name: "test 0",
          passed: false,
          problem: "failed",
          status: "completed",
          output: 2,
          calls: {},
          failures: ["the output is 2, expected 1"],
          transcript: "",
        },
      ],
    });

  beforeEach(() => {
    server.on("GET", "/v1/workflows/{id}", (request) =>
      request.params["id"] === ID || request.params["id"] === "triage"
        ? json(triage)
        : problem(404, "not-found", `no workflow ${request.params["id"]}`),
    );
    server.on("PATCH", "/v1/workflows/{id}", (request) => {
      const patch = request.body as { name?: string | null; description?: string | null };
      if (patch.name === "taken") {
        return problem(409, "name-taken", "the namespace user_2fQ81 already has a workflow taken");
      }
      return json({ ...triage, ...patch });
    });
    server.on("POST", "/v1/workflows/{id}/versions", (request) => {
      const code = (request.body as { code?: string }).code ?? "";
      if (code.includes("regression")) {
        return testsFailed("1 test of 1 did not pass: no version was made");
      }
      if (code.includes("refused")) {
        return problem(422, "invalid-code", "the code does not type-check (1 error)");
      }
      if (code.includes("secret")) {
        return problem(422, "secret-required", "github needs a secret: name one in secrets");
      }
      // The same content makes no version.
      return code === triage.code ? json(triage) : json({ ...triage, version: v4, versions: [v4, ...triage.versions] });
    });
    server.on("POST", "/v1/workflows/{id}/tests", (request) => {
      const tests = (request.body as { tests?: Array<{ name: string }> }).tests ?? [];
      if (tests.some((test) => test.name === "fails against v3")) {
        return testsFailed("1 test did not pass against the version v3");
      }
      return json({
        ...triage,
        suite: [...suite, { name: "test 1", digest: "11".repeat(32), addedAt: "2026-11-20T10:00:00Z" }],
      });
    });
  });

  it("names and describes a workflow without a version", async () => {
    const named = await tools.handle("update_workflow", { workflow: ID, name: "triage", description: null });
    expect(named.isError).toBe(false);
    expect(named.code).toBeUndefined();
    expect(named.content).toBe(
      [
        `Updated the workflow triage (id ${ID}): named it triage and removed its description.`,
        "Its version stays v3.",
        'Run it with run_workflow({"workflow":"triage"}).',
      ].join("\n"),
    );
    expect(named.workflow?.id).toBe(ID);
    // A JSON merge patch: `null` removes.
    expect(server.requests.map((r) => `${r.method} ${r.path}`)).toEqual([`PATCH /v1/workflows/${ID}`]);
    expect(server.requests[0]?.body).toEqual({ name: "triage", description: null });

    const taken = await tools.handle("update_workflow", { workflow: ID, name: "taken", code: "// new" });
    expect(taken).toEqual({
      content: "update_workflow failed: the namespace user_2fQ81 already has a workflow taken",
      isError: true,
    });
    // No version after a refused name.
    expect(server.requests.map((r) => r.method)).toEqual(["PATCH", "PATCH"]);

    expect((await tools.handle("update_workflow", { workflow: ID })).content).toBe(
      "update_workflow: give what changes: a `name` or a `description`; `code`, an interface (`input`, `output`, `signals`) or `secrets` for a new version; or `tests`, `replace` and `retire` for its suite.",
    );
    expect((await tools.handle("update_workflow", { workflow: ID, name: "" })).content).toBe(
      "update_workflow: `name` must be a non-empty string, or null to remove it.",
    );
    expect((await tools.handle("update_workflow", { workflow: ID, input: "x" })).content).toBe(
      "update_workflow: `input` must be a JSON Schema: an object.",
    );
  });

  it("makes a version through the gate", async () => {
    const made = await tools.handle("update_workflow", {
      workflow: "triage",
      code: "// new",
      secrets: { github: "ns/github" },
      tests: ["9f".repeat(32)],
      retire: [{ names: ["asks first"], reason: "it no longer asks" }],
    });
    expect(made.isError).toBe(false);
    expect(made.code).toBe("passed");
    expect(made.content).toBe(
      [
        `Made the version v4 (digest 040404040404) of the workflow triage (id ${ID}): its suite, 1 test, passed against it.`,
        'Run it with run_workflow({"workflow":"triage"}).',
      ].join("\n"),
    );
    // Read first, by the name given; then written by its id, with the tools' action set.
    expect(server.requests.map((r) => `${r.method} ${r.path}`)).toEqual([
      "GET /v1/workflows/triage",
      `POST /v1/workflows/${ID}/versions`,
    ]);
    expect(server.requests[1]?.body).toEqual({
      code: "// new",
      actionSet: ACTION_SET,
      secrets: { github: "ns/github" },
      tests: ["9f".repeat(32)],
      retire: [{ names: ["asks first"], reason: "it no longer asks" }],
    });

    // The same content makes no version.
    const same = await tools.handle("update_workflow", { workflow: "triage", code: triage.code });
    expect(same.content.split("\n")[0]).toBe(
      `The workflow triage (id ${ID}) already has this content as its version v3: no version was made.`,
    );

    // A name and a version in one call: the patch first, then the version by the id.
    const both = await rewind
      .namespace(NAMESPACE)
      .agentTools({ labels: LABELS })
      .handle("update_workflow", { workflow: ID, name: "triage", output: { type: "string" } });
    expect(both.isError).toBe(false);
    expect(both.code).toBeUndefined();
    expect(both.content).toMatch(
      new RegExp(`^Updated the workflow triage \\(id ${ID}\\): named it triage\\.\\nMade the version v4`),
    );
    // The interface's parts left out keep the latest version's; no action set without code.
    expect(server.calls("POST", `/v1/workflows/${ID}/versions`).at(-1)?.body).toEqual({
      input: v3.input,
      output: { type: "string" },
      signals: v3.signals,
    });
  });

  it("says what its gate refused", async () => {
    const failed = await tools.handle("update_workflow", { workflow: "triage", code: "// regression" });
    expect(failed.isError).toBe(true);
    // The code passed its checks.
    expect(failed.code).toBe("passed");
    expect(failed.tests).toHaveLength(1);
    expect(failed.content).toMatch(
      /^No version was made: 1 test of 1 did not pass: no version was made\n1 of 1 test did not pass\.\nFAIL "test 0"/,
    );
    expect(failed.content).toMatch(/with a reason\.$/);

    const refused = await tools.handle("update_workflow", { workflow: "triage", code: "// refused" });
    expect(refused.code).toBe("refused");
    expect(refused.content).toBe(
      "No version was made: the code was refused before anything ran: the code does not type-check (1 error)",
    );

    // What the patch changed stays changed: the model reads it first.
    const partly = await tools.handle("update_workflow", { workflow: ID, description: "Triage.", code: "// secret" });
    expect(partly).toEqual({
      content: `Updated the workflow triage (id ${ID}): gave it a new description.\nNo version was made: github needs a secret: name one in secrets`,
      isError: true,
    });
  });

  it("changes a suite alone, without a version", async () => {
    const test = { name: "test 1", input: {}, expect: { output: 1 } };
    const replace = [{ name: "test 0", reason: "it returns 1 now", expect: { output: 1 } }];
    const changed = await tools.handle("update_workflow", { workflow: "triage", tests: [test], replace });
    expect(changed.isError).toBe(false);
    expect(changed.code).toBeUndefined();
    expect(changed.content.split("\n")[0]).toBe(
      `Changed the suite of the workflow triage (id ${ID}): it holds 2 tests, each passing against its version v3, which stays its latest. Its next version keeps them.`,
    );
    expect(server.requests.map((r) => `${r.method} ${r.path}`)).toEqual([
      "GET /v1/workflows/triage",
      `POST /v1/workflows/${ID}/tests`,
    ]);
    expect(server.requests[1]?.body).toEqual({ tests: [test], replace });

    const failed = await tools.handle("update_workflow", {
      workflow: "triage",
      tests: [{ name: "fails against v3", expect: { output: 2 } }],
    });
    expect(failed.isError).toBe(true);
    expect(failed.content).toMatch(/^The suite was not changed: 1 test did not pass against the version v3\n/);
    expect(failed.content).toMatch(
      /A test joins the suite only once it passes against the latest version, v3\. If the test is wrong, fix it\. If the workflow must change to pass it, give its new `code` with the test: the version is made with it once the whole suite passes\.$/,
    );
    expect(
      (await tools.handle("update_workflow", { workflow: "triage", replace: [{ name: "test 0", reason: " " }] }))
        .content,
    ).toMatch(/^update_workflow: `replace` must be an array of \{ name, reason, new\?, expect\? \}/);
  });
});

describe("get_workflow", () => {
  const BASE = "e4".repeat(32);
  const FIRST = "a1".repeat(32);
  const INLINE = "4814d92093ac" + "0".repeat(52);
  const RECORDED = "ca978112ca1b" + "0".repeat(52);
  const FOREIGN = "b2".repeat(32);
  const OLD = "3e".repeat(32);
  const RETIRED = "2e".repeat(32);
  const latest = version({
    base: BASE,
    tests: [
      { name: "closes nothing without approval", digest: INLINE },
      { name: `recorded ${RUN_ID}`, digest: RECORDED },
      { name: "recorded 0193a5c4-0000-7000-8000-000000000000", digest: FOREIGN },
    ],
    replaced: [
      { name: "closes nothing without approval", old: OLD, new: INLINE, reason: "the message names the repository" },
    ],
    retired: [
      { name: "asks before closing", digest: RETIRED, reason: "under 5 stale issues, it closes without asking" },
    ],
  });
  const based = version({
    version: 2,
    digest: BASE,
    createdAt: "2026-11-11T10:00:00Z",
    base: FIRST,
    tests: [{ name: "asks before closing", digest: RETIRED }],
  });
  const first = version({ version: 1, digest: FIRST, createdAt: "2026-11-10T10:00:00Z" });
  const triage = workflow(
    {
      source: { workflow: "wf_0rg4n1z4t10n", version: 7 },
      suite: [
        {
          name: "asks before closing",
          digest: RETIRED,
          addedAt: "2026-11-11T10:00:00Z",
          retiredAt: "2026-11-12T10:00:00Z",
          reason: "under 5 stale issues, it closes without asking",
        },
        {
          name: "closes nothing without approval",
          digest: OLD,
          addedAt: "2026-11-11T10:00:00Z",
          retiredAt: "2026-11-12T10:00:00Z",
          reason: "the message names the repository",
        },
        { name: "closes nothing without approval", digest: INLINE, addedAt: "2026-11-12T10:00:00Z" },
        { name: `recorded ${RUN_ID}`, digest: RECORDED, addedAt: "2026-11-12T10:00:00Z" },
        { name: "recorded 0193a5c4-0000-7000-8000-000000000000", digest: FOREIGN, addedAt: "2026-11-12T10:00:00Z" },
      ],
    },
    [latest, based, first],
  );
  const inline = {
    name: "closes nothing without approval",
    input: { repo: "acme/api" },
    world: { actions: { "github.list_issues": [{ returns: [{ number: 4 }] }] } },
    signals: [{ name: "approval", payload: { close: false } }],
    expect: { output: { closed: 0 }, calls: [{ action: "github.close_issue", times: 0 }] },
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
          { action: "github.list_issues", args: { repo: "acme/api" }, outcome: { result: [] } },
          { action: "github.close_issue", args: { number: 4 }, outcome: { result: {} } },
          { action: "github.close_issue", args: { number: 5 }, outcome: { result: {} } },
        ],
        signals: [{ name: "approval", payload: { close: true }, elapsed_ms: 3_600_000 }],
      },
    },
    expect: { status: "completed", output: { closed: 2 } },
    from: { run: RUN_ID, version: BASE, namespace: NAMESPACE },
  };

  function serveWorkflow(served: Workflow = triage): void {
    server.on("GET", "/v1/workflows/{id}", (request) =>
      request.params["id"] === "triage" || request.params["id"] === ID
        ? json(served)
        : problem(404, "not-found", `no workflow ${request.params["id"]}`),
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

  it("reads who it is, its code, its interface, every test of its suite, the tests that left it and its versions", async () => {
    serveWorkflow();
    const result = await tools.handle("get_workflow", { workflow: "triage" });
    expect(result.isError).toBe(false);
    expect(result.workflow).toEqual(triage);
    // The same text as the Rust port's (`crates/rewind-agent-tools/src/render.rs`).
    expect(result.content).toBe(
      [
        `The workflow triage (id ${ID}), of the namespace user_2fQ81.`,
        "It is a fork of the organization's workflow wf_0rg4n1z4t10n, whose version v7 it last took.",
        "Description: Triage the stale issues of a repository, after approval.",
        "Its version v3 of 3 versions (digest 4be1c07a9d3e), made 2026-11-12T10:00:00Z, from v2.",
        `Action set: ${ACTION_SET}`,
        `Input schema: ${JSON.stringify(latest.input)}`,
        `Output schema: ${JSON.stringify(latest.output)}`,
        "Signals:",
        `- "approval": ${JSON.stringify(latest.signals?.["approval"])}`,
        'Secrets its runs use: {"github":"ns/github"}',
        `Code starts it from "rewind:workflows" once an action set pins it: ${latest.declarations?.prompt}`,
        "Code:",
        "export default async () => ({ closed: 0 });",
        "Its suite, 3 tests, which its next version must pass (give a digest in update_workflow's `tests` or `replace` to reuse a test):",
        `Test "closes nothing without approval" (${INLINE}):`,
        JSON.stringify(inline),
        `Test "recorded ${RUN_ID}" (${RECORDED}), recorded from run ${RUN_ID} of v2:`,
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
        "Tests that left its suite, and why:",
        '- "asks before closing" (2e2e2e2e2e2e), left 2026-11-12T10:00:00Z: under 5 stale issues, it closes without asking',
        '- "closes nothing without approval" (3e3e3e3e3e3e), left 2026-11-12T10:00:00Z: the message names the repository',
        "Its versions, newest first:",
        '- v3, made 2026-11-12T10:00:00Z, from v2: 3 tests; replaced "closes nothing without approval", retired "asks before closing".',
        "- v2, made 2026-11-11T10:00:00Z, from v1: 1 test.",
        "- v1, made 2026-11-10T10:00:00Z, the first version: 0 tests.",
      ].join("\n"),
    );
    // Only the tests in its suite are read in full.
    expect(server.requests.filter((r) => r.path.startsWith("/v1/tests/")).map((r) => r.params["digest"])).toEqual([
      INLINE,
      RECORDED,
      FOREIGN,
    ]);
  });

  it("reads a workflow of the organization without a name, a comment or an interface", async () => {
    serveWorkflow(
      workflow({ name: null, description: null, namespace: null }, [
        version({ version: 1, input: null, output: null, signals: null, secrets: {}, declarations: null }),
      ]),
    );
    const result = await rewind.agentTools({ labels: LABELS, tools: ["get_workflow"] }).handle("get_workflow", {
      workflow: ID,
    });
    expect(result.content).toMatch(
      new RegExp(
        `^The workflow ${ID} \\(no name; it shows as wf-7k2m9q\\), of the organization\\.\\nIts version v1 of 1 version \\(digest 4be1c07a9d3e\\), made 2026-11-12T10:00:00Z\\.\\nAction set: 9f2c`,
      ),
    );
    expect(result.content).toContain(
      "\nInterface: none. It takes any input, returns any output and accepts any signal name.\nCode:\n",
    );
    expect(result.content).toContain("\nIts suite is empty: it has no test.\n");
  });

  it("reports a workflow that does not exist, and stays bounded whatever the tests hold", async () => {
    serveWorkflow();
    expect(await tools.handle("get_workflow", { workflow: "nope" })).toEqual({
      content: "get_workflow failed: no workflow nope",
      isError: true,
    });
    const huge = "x".repeat(3_000_000);
    serveWorkflow({ ...triage, code: huge });
    server.on("GET", "/v1/tests/{digest}", json({ ...inline, input: { huge } }));
    const result = await tools.handle("get_workflow", { workflow: "triage" });
    expect(Buffer.byteLength(result.content)).toBeLessThan(512 * 1024);
    expect(result.content).toContain("Its versions, newest first:");
    expect((await tools.handle("get_workflow", {})).content).toBe(
      "get_workflow: `workflow` must be a non-empty string.",
    );
  });
});

describe("record_tests", () => {
  it("records tests from the workflow's runs, and says how to add them to its suite", async () => {
    server.on(
      "POST",
      "/v1/workflows/{id}/record",
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
        'They join no suite yet: pass their digests in update_workflow\'s `tests` to add them to the suite of triage, or { "test": "<digest>", "expect": {…} } to give a failed run the expectation it should have met.',
      ].join("\n"),
    );
    await tools.handle("record_tests", { workflow: ID, runs: [RUN_ID] });
    expect(server.requests.map((r) => [r.path, r.body])).toEqual([
      ["/v1/workflows/triage/record", { query: "status:completed", limit: 10 }],
      [`/v1/workflows/${ID}/record`, { runs: [RUN_ID] }],
    ]);
    const withoutUpdates = rewind.agentTools({ labels: LABELS, tools: ["record_tests"] });
    expect((await withoutUpdates.handle("record_tests", { workflow: "triage" })).content).toMatch(
      /They join no suite: pass their digests in test_workflow's `tests` to run them against a code, or \{ "test": "<digest>", "expect": \{…\} \} to give a failed run the expectation it should have met\.$/,
    );
  });

  it("says when no run was recorded, and what is wrong with its arguments", async () => {
    server.on("POST", "/v1/workflows/{id}/record", json({ tests: [], skipped: [] }));
    expect((await tools.handle("record_tests", { workflow: "triage" })).content).toBe(
      "No test was recorded: no completed or failed run of triage was selected.",
    );
    server.on("POST", "/v1/workflows/{id}/record", problem(400, "invalid-query", "unknown field stauts"));
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
      name: "run_workflow",
      input: { code: "x" },
    });
    expect(block).toEqual({
      type: "tool_result",
      tool_use_id: "toolu_9",
      content: `Run ${HANDLE} was aborted by the host (limit_exceeded): the history cap`,
      is_error: true,
    });
    expect(server.requests[0]?.headers["idempotency-key"]).toBe(runKey("toolu_9"));
  });

  it("answer OpenAI tool calls, whose arguments are a JSON string", async () => {
    server.on("POST", "/v1/check", json({ ok: true, diagnostics: [] }));
    const message = await tools.openai.handle({
      id: "call_1",
      type: "function",
      function: { name: "check_workflow", arguments: '{"code":"// Count to one\\nx"}' },
    });
    expect(message).toEqual({ role: "tool", tool_call_id: "call_1", content: "The code type-checks." });
    const output = await tools.openai.responses.handle({
      type: "function_call",
      call_id: "call_2",
      name: "check_workflow",
      arguments: '{"code":"// Count to two\\ny"}',
    });
    expect(output).toEqual({ type: "function_call_output", call_id: "call_2", output: "The code type-checks." });
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
      createdBy: "member:u",
      createdAt: "2026-11-19T10:00:00Z",
      updatedBy: "member:u",
      updatedAt: "2026-11-19T10:00:00Z",
      reads: [],
    };
  }

  function withSecrets(secrets: AgentSecrets | undefined): AgentTools {
    const client = rewind.namespace(NAMESPACE);
    return new AgentToolsClass(
      {
        runs: client.runs,
        workflows: client.workflows,
        actionSets: client.actionSets,
        tests: client.tests,
        payloads: client.payloads,
        secrets,
      },
      { actionSet: ACTION_SET, labels: LABELS },
    );
  }

  it("list_secrets: what the agent may use, never a value", async () => {
    const agent = withSecrets({
      list: async () => [listed("org/zendesk", { env: "prod" }), listed("org/linear")],
      ofNamespace: async () => [listed("ns/github")],
    });
    const result = await agent.handle("list_secrets", {});
    expect(result.isError).toBe(false);
    // The same text as the Rust port's (`crates/rewind-agent-tools/tests/tools.rs`).
    expect(result.content).toBe(
      'The secrets you may use, never a value:\n- org/zendesk, the organization\'s (labels env=prod)\n- org/linear, the organization\'s\n- ns/github, the end user\'s in the namespace alice\nName one in `secrets`, by the MCP server or API whose calls need it, such as {"github": "ns/github"}.',
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
      ofNamespace: async () => [listed("ns/github")],
    });
    expect((await token.handle("list_secrets", {})).content).toBe(
      'The secrets you may use, never a value:\n- ns/github, the end user\'s in the namespace alice\nName one in `secrets`, by the MCP server or API whose calls need it, such as {"github": "ns/github"}.',
    );
    const none = await withSecrets(undefined).handle("list_secrets", {});
    expect(none).toEqual({
      content:
        "No secret you may use exists yet: a person puts one first, the end user's in their namespace or the organization's.",
      isError: false,
    });
  });

  it("go with run_workflow, retry_run and update_workflow", async () => {
    const named = { github: "ns/github", zendesk: "org/zendesk" };
    server.on("POST", "/v1/runs", started());
    await tools.handle("run_workflow", { code: "a", secrets: named }, { toolCallId: "c1" });
    expect((server.calls("POST", "/v1/runs")[0]?.body as { secrets: unknown }).secrets).toEqual(named);

    serveRun(run({ status: "failed" }));
    server.on("POST", "/v1/runs/{id}/fork", started({ id: "fork-1", source: { run: RUN_ID, handle: HANDLE } }));
    await tools.handle("retry_run", { run_id: RUN_ID, code: "b", secrets: { github: "org/github" } });
    expect(server.calls("POST", `/v1/runs/${RUN_ID}/fork`)[0]?.body).toEqual({
      code: "b",
      labels: LABELS,
      wait: "30s",
      secrets: { github: "org/github" },
    });

    server.on("GET", "/v1/workflows/{id}", json(workflow()));
    server.on("POST", "/v1/workflows/{id}/versions", json(workflow({}, [version({ version: 4 })]), 201));
    await tools.handle("update_workflow", { workflow: "triage", secrets: { github: "ns/github" } });
    expect(server.calls("POST", `/v1/workflows/${ID}/versions`)[0]?.body).toEqual({
      secrets: { github: "ns/github" },
    });

    const refused = await tools.handle("run_workflow", { code: "a", secrets: { github: 1 } });
    expect(refused.isError).toBe(true);
    expect(refused.content).toMatch(/^run_workflow: `secrets` must be an object of secrets by MCP server or API/);
  });

  it("a start refused for its secrets tells the model what it may use", async () => {
    server.on("POST", "/v1/runs", () =>
      problem(
        422,
        "secret-required",
        'the code imports github, which needs a secret: name one in "secrets" ("github": "org/<name>" or "ns/<name>"). The secrets you may use: ns/github',
      ),
    );
    const result = await tools.handle("run_workflow", { code: "a" });
    expect(result.isError).toBe(true);
    expect(result.content).toBe(
      'run_workflow failed: the code imports github, which needs a secret: name one in "secrets" ("github": "org/<name>" or "ns/<name>"). The secrets you may use: ns/github',
    );
  });
});
