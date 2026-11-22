/**
 * Area "Workflows" of the release gate, on setup A (deployed): saving and versioning workflows,
 * starting runs of them from the API and from scripts, their typed inputs and signals, attached
 * and detached children, and the scope that keeps a script within the runs its key may reach
 * (`archi/01-product.md`, "Workflows"; `archi/03-execution.md`, "Workflows across runs";
 * `archi/06`, "What the caller may do").
 */
import { randomBytes } from "node:crypto";
import { eventually } from "../../support/processes.js";
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
import { ended, expectFailed, getRun, runIds, started, startRun, waitingFor } from "../support/runs-helpers.js";

let tenant: Tenant;
let api: Api;
/** An empty toolset: the workflows' own. */
let emptyToolset: string;

const TRIAGE = `import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const approval = await signal("approval");
  return { closed: approval.close ? 3 : 0, repo: input.repo };
}
`;

/** A workflow's definition, as `POST /v1/workflows` takes it. */
function triage(name: string, script = TRIAGE) {
  return {
    name,
    description: "Triage the stale issues of a repository, after approval.",
    toolset: emptyToolset,
    script,
    input: { type: "object", properties: { repo: { type: "string" } }, required: ["repo"] },
    output: { type: "object", properties: { closed: { type: "integer" } }, required: ["closed"] },
    signals: {
      approval: { type: "object", properties: { close: { type: "boolean" } }, required: ["close"] },
    },
  };
}

/** A workflow name of its own: an identifier. */
const fresh = (prefix = "triage") => `${prefix}_${randomBytes(3).toString("hex")}`;

/** Saves a workflow; returns the version. */
async function save(as: Api, definition: ReturnType<typeof triage>): Promise<any> {
  const reply = await as.post("/v1/workflows", definition);
  expectOk(reply, 201);
  return reply.json;
}

/** A toolset that pins these workflows. */
async function pinning(as: Api, ...workflows: string[]): Promise<string> {
  const reply = await as.post("/v1/toolsets", { workflows });
  expectOk(reply);
  return reply.json.id;
}

test.beforeAll(async () => {
  tenant = await newTenant("workflows");
  api = tenant.api.ns("wf");
  const created = await tenant.api.post("/v1/toolsets", {});
  expectOk(created);
  emptyToolset = created.json.id;
});

// ---------------------------------------------------------------------------------------------
// Success

test("saving a workflow checks it and gives its declarations; saving it again is the same version", { tag: "@success" }, async () => {
  const name = fresh();
  const created = await tenant.api.post("/v1/workflows", triage(name));
  expectOk(created, 201);
  expect(created.json).toMatchObject({ name, toolset: emptyToolset });
  expect(created.json.version).toMatch(/^[0-9a-f]{64}$/);
  expect(created.json.declarations.dts).toContain(`export const ${name}: Workflow<`);
  expect(created.json.declarations.prompt.startsWith(`${name}.start({ repo: string }) → run`), created.json.declarations.prompt).toBe(true);
  const again = await tenant.api.post("/v1/workflows", triage(name));
  expectOk(again, 200);
  expect(again.json.version).toBe(created.json.version);
});

test("each change is a new version: versions are listed newest first, the list shows the latest", { tag: "@success" }, async () => {
  const name = fresh();
  const first = await save(tenant.api, triage(name));
  const second = await save(tenant.api, triage(name, TRIAGE.replace("? 3 : 0", "? 5 : 0")));
  expect(second.version).not.toBe(first.version);
  const versions = await tenant.api.get(`/v1/workflows/${name}`);
  expectOk(versions, 200);
  expect(versions.json.versions.map((v: any) => v.version)).toEqual([second.version, first.version]);
  const listed = await tenant.api.get("/v1/workflows", { query: { limit: 200 } });
  expectOk(listed, 200);
  const mine = listed.json.workflows.filter((w: any) => w.name === name);
  expect(mine).toHaveLength(1);
  expect(mine[0].version).toBe(second.version);
});

test("the harness starts a run of a workflow by name, or by name@version, and signals it", { tag: "@success" }, async () => {
  const name = fresh();
  const first = await save(tenant.api, triage(name));
  const second = await save(tenant.api, triage(name, TRIAGE.replace("? 3 : 0", "? 5 : 0")));

  const latest = await started(api, { workflow: name, input: { repo: "acme/api" } });
  expect(latest.workflow).toEqual({ name, version: second.version });
  const pinned = await started(api, { workflow: `${name}@${first.version.slice(0, 12)}`, input: { repo: "acme/web" } });
  expect(pinned.workflow).toEqual({ name, version: first.version });

  for (const run of [latest, pinned]) {
    await waitingFor(api, run.id, "signal", "approval");
    expectOk(await api.post(`/v1/runs/${run.id}/signals/approval`, { close: true }), 202);
  }
  expect((await ended(api, latest.id)).output.value).toEqual({ closed: 5, repo: "acme/api" });
  expect((await ended(api, pinned.id)).output.value).toEqual({ closed: 3, repo: "acme/web" });
});

test("a script starts a run of a workflow, signals it and receives its result", { tag: "@success" }, async () => {
  const name = fresh();
  const saved = await save(tenant.api, triage(name));
  const toolset = await pinning(tenant.api, name);
  const parent = await started(api, {
    toolset,
    input: { repo: "acme/api" },
    wait: "30s",
    script: `import { ${name} } from "rewind:workflows";

export default async function main(input: { repo: string }) {
  const run = await ${name}.start({ repo: input.repo });
  await run.signal("approval", { close: true });
  return { id: run.id, result: await run.result() };
}
`,
  });
  const done = parent.status === "completed" ? parent : await ended(api, parent.id);
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(done.output.value.result).toEqual({ closed: 3, repo: "acme/api" });
  const child = await getRun(api, done.output.value.id);
  expect(child.status).toBe("completed");
  expect(child.namespace).toBe("wf");
  expect(child.workflow).toEqual({ name, version: saved.version });
  expect(child.startedBy).toMatchObject({ run: parent.id, detached: false });
});

test("a detached child lives on after its parent; a failing parent cancels its attached children", { tag: "@success" }, async () => {
  const name = fresh();
  await save(tenant.api, triage(name));
  const toolset = await pinning(tenant.api, name);

  const detachedParent = await started(api, {
    toolset,
    wait: "30s",
    script: `import { ${name} } from "rewind:workflows";
export default async function main() {
  const run = await ${name}.start({ repo: "acme/long" }, { detached: true });
  return run.id;
}
`,
  });
  const parentDone = detachedParent.status === "completed" ? detachedParent : await ended(api, detachedParent.id);
  expect(parentDone.status).toBe("completed");
  const detached = await getRun(api, parentDone.output.value);
  expect(detached.status).toBe("active");
  expect(detached.startedBy).toMatchObject({ run: detachedParent.id, detached: true });

  const failing = await started(api, {
    toolset,
    script: `import { ${name} } from "rewind:workflows";
export default async function main() {
  await ${name}.start({ repo: "acme/api" });
  throw new Error("the parent gave up");
}
`,
  });
  expectFailed(await ended(api, failing.id), "Error", "the parent gave up");
  const children = await eventually(
    "the failed parent's child is listed",
    async () => {
      const ids = await runIds(api, { startedBy: failing.id });
      return ids.length > 0 ? ids : undefined;
    },
    30_000,
  );
  const child = await ended(api, children[0]!);
  expect(child.status, JSON.stringify(child)).toBe("cancelled");
  expect(child.error?.message ?? "").toContain(`the run ${failing.id} that started it ended`);
  // The detached child was not touched.
  expect((await getRun(api, detached.id)).status).toBe("active");
  await api.post(`/v1/runs/${detached.id}/cancel`, {});
});

// ---------------------------------------------------------------------------------------------
// Failures

test("a workflow whose name or signals cannot be used is 422 invalid-workflow", { tag: "@failure" }, async () => {
  expectProblem(await tenant.api.post("/v1/workflows", triage("default")), 422, "invalid-workflow");
  expectProblem(await tenant.api.post("/v1/workflows", triage("not-an-identifier")), 422, "invalid-workflow");
  const reserved = { ...triage(fresh()), signals: { "rewind:ended": { type: "object" } } };
  expectProblem(await tenant.api.post("/v1/workflows", reserved), 422, "invalid-workflow");
});

test("a script that does not match its workflow's schemas is 422 invalid-script, with the diagnostic", { tag: "@failure" }, async () => {
  const reply = await tenant.api.post("/v1/workflows", triage(fresh(), TRIAGE.replace("input: { repo: string }", "input: { repo: number }")));
  expectProblem(reply, 422, "invalid-script");
  const messages = (reply.json.diagnostics ?? []).map((d: any) => d.message);
  expect(messages.some((m: string) => m.startsWith("the default export does not match the workflow's schemas")), messages.join("\n")).toBe(true);
});

/** Approved, it closes `closed` issues: a test of TRIAGE, which closes three. */
const closes = (closed: number, name = `closes ${closed} once approved`) => ({
  name,
  input: { repo: "acme/api" },
  signals: [{ name: "approval", payload: { close: true } }],
  expect: { status: "completed", output: { closed } },
});

test("a version whose tests do not pass is 422 tests-failed with each outcome, and nothing is saved", { tag: "@failure" }, async () => {
  const name = fresh();
  const reply = await tenant.api.post("/v1/workflows", { ...triage(name), tests: [closes(3), closes(5)] });
  expectProblem(reply, 422, "tests-failed");
  expect(reply.json.detail).toContain('"closes 5 once approved"');
  const outcomes = Object.fromEntries((reply.json.tests ?? []).map((t: any) => [t.name, t]));
  expect(Object.keys(outcomes).sort(), reply.text).toEqual(["closes 3 once approved", "closes 5 once approved"]);
  expect(outcomes["closes 3 once approved"].passed).toBe(true);
  expect(outcomes["closes 5 once approved"].problem).toBe("failed");
  expect(outcomes["closes 5 once approved"].failures.join("\n")).toContain('{"closed":5}');
  expectProblem(await tenant.api.get(`/v1/workflows/${name}`), 404, "not-found");
});

test("a suite with two tests of the same name is 422 invalid-test, and nothing is saved", { tag: "@failure" }, async () => {
  const name = fresh();
  const reply = await tenant.api.post("/v1/workflows", { ...triage(name), tests: [closes(3, "approved"), closes(3, "approved")] });
  expectProblem(reply, 422, "invalid-test");
  expect(reply.json.detail).toContain('"approved"');
  expectProblem(await tenant.api.get(`/v1/workflows/${name}`), 404, "not-found");
});

test("a run of a workflow refuses an input that does not match its schema, and no run starts", { tag: "@failure" }, async () => {
  const name = fresh();
  await save(tenant.api, triage(name));
  const before = await runIds(api);
  const reply = await startRun(api, { workflow: name, input: { repo: 42 } });
  expectProblem(reply, 422, "invalid-input");
  expect(reply.json.detail).toContain("input.repo");
  expect(await runIds(api)).toEqual(before);
});

test("a run of a workflow refuses a signal it does not declare, or a payload that does not match", { tag: "@failure" }, async () => {
  const name = fresh();
  await save(tenant.api, triage(name));
  const run = await started(api, { workflow: name, input: { repo: "acme/api" } });
  await waitingFor(api, run.id, "signal", "approval");
  for (const [signal, payload] of [
    ["aproval", { close: true }],
    ["approval", { close: "yes" }],
  ] as const) {
    await expectRefused({
      attempt: () => api.post(`/v1/runs/${run.id}/signals/${signal}`, payload),
      status: 422,
      slug: "invalid-signal",
      probe: async () => (await getRun(api, run.id)).historyLen,
    });
  }
  await api.post(`/v1/runs/${run.id}/cancel`, {});
});

test("an unknown workflow is 404; a malformed version or a workflow with a script is 400", { tag: "@failure" }, async () => {
  const name = fresh();
  await save(tenant.api, triage(name));
  expectProblem(await startRun(api, { workflow: fresh("nothing"), input: {} }), 404, "not-found");
  expectProblem(await startRun(api, { workflow: `${name}@${"0".repeat(12)}`, input: { repo: "x" } }), 404, "not-found");
  expectProblem(await startRun(api, { workflow: `${name}@xyz`, input: { repo: "x" } }), 400, "invalid-request");
  expectProblem(await startRun(api, { workflow: name, script: TRIAGE, input: { repo: "x" } }), 400, "invalid-request");
  expectProblem(await tenant.api.get(`/v1/workflows/${fresh("nothing")}`), 404, "not-found");
});

test("a script that starts a workflow with a wrong input gets a TypeError at the call", { tag: "@failure" }, async () => {
  const name = fresh();
  await save(tenant.api, triage(name));
  const toolset = await pinning(tenant.api, name);
  const run = await started(api, {
    toolset,
    check: false,
    script: `import { ${name} } from "rewind:workflows";
export default async function main() {
  await ${name}.start({ repo: 42 } as any);
  return "started";
}
`,
  });
  const done = await ended(api, run.id);
  expectFailed(done, "TypeError");
  expect(await runIds(api, { startedBy: run.id })).toEqual([]);
});

// ---------------------------------------------------------------------------------------------
// Attacks

test("a forged end signal cannot answer a parent's result()", { tag: "@attack" }, async () => {
  const name = fresh();
  await save(tenant.api, triage(name));
  const toolset = await pinning(tenant.api, name);
  const parent = await started(api, {
    toolset,
    script: `import { ${name} } from "rewind:workflows";
export default async function main() {
  const run = await ${name}.start({ repo: "acme/api" });
  return await run.result();
}
`,
  });
  const children = await eventually(
    "the child exists",
    async () => {
      const ids = await runIds(api, { startedBy: parent.id });
      return ids.length > 0 ? ids : undefined;
    },
    30_000,
  );
  await waitingFor(api, parent.id, "signal");
  await expectRefused({
    attempt: () =>
      api.post(`/v1/runs/${parent.id}/signals/${encodeURIComponent(`rewind:ended:${children[0]}`)}`, {
        status: "completed",
        output: { closed: 999 },
      }),
    status: 422,
    slug: "invalid-signal",
    probe: async () => {
      const state = await getRun(api, parent.id);
      return { status: state.status, historyLen: state.historyLen };
    },
  });
  evidence("422 invalid-signal: rewind:ended:<child> is reserved; the parent still waits for the real end");
  await api.post(`/v1/runs/${parent.id}/cancel`, {});
});

test("a key limited to labels saves its own version of a workflow saved first outside its scope, and starts it", { tag: "@success" }, async () => {
  const name = fresh("env");
  const first = await save(api, triage(name));
  const created = await tenant.api.post("/v1/api-keys", { role: "harness", name: "dev environment", allowedLabels: { env: "dev" } });
  expectOk(created, 201);
  const dev = new Api(stacks().a.url, created.json.key as string).ns("wf");

  const ours = await save(dev, triage(name));
  expect(ours.version).not.toBe(first.version);
  expect(ours.labels).toEqual({ env: "dev" });
  const again = await dev.post("/v1/workflows", triage(name));
  expectOk(again, 200);
  expect(again.json.version).toBe(ours.version);
  const tenantWide = await api.post("/v1/workflows", triage(name));
  expectOk(tenantWide, 200);
  expect(tenantWide.json.version, "a tenant-wide save keeps its version").toBe(first.version);

  const listed = await dev.get("/v1/workflows");
  expectOk(listed, 200);
  expect(listed.json.workflows.filter((w: any) => w.name === name).map((w: any) => w.version)).toEqual([ours.version]);
  const run = await started(dev, { workflow: name, input: { repo: "acme/dev" } });
  expect(run.workflow.version).toBe(ours.version);
  expect(run.labels).toMatchObject({ env: "dev" });
  evidence(`${name}: tenant-wide ${first.version.slice(0, 12)}, env=dev ${ours.version.slice(0, 12)}; the dev key lists and starts its own`);
  await dev.post(`/v1/runs/${run.id}/cancel`, {});
});

test("a script started by a key limited to labels cannot signal a run outside its scope", { tag: "@attack" }, async () => {
  // The key limited to `team: red` saves its own workflow (which carries its labels) and toolset.
  // `Tenant.key` sends `labels`; the API takes `allowedLabels`.
  const created = await tenant.api.post("/v1/api-keys", { role: "harness", name: "red team", allowedLabels: { team: "red" } });
  expectOk(created, 201);
  const red = { key: created.json.key as string };
  const redApi = new Api(stacks().a.url, red.key).ns("wf");
  const name = fresh("scoped");
  const saved = await save(redApi.ns("wf"), triage(name));
  expect(saved.labels).toEqual({ team: "red" });
  const toolset = await pinning(redApi, name);

  // A run of the same workflow, started by the owner without labels: outside the red scope.
  const target = await started(api, { workflow: name, input: { repo: "acme/blue" } });
  await waitingFor(api, target.id, "signal", "approval");
  const before = (await getRun(api, target.id)).historyLen;

  const attacker = await started(redApi, {
    toolset,
    script: `import { ${name} } from "rewind:workflows";
import { ToolError } from "rewind";
export default async function main(input: { target: string }) {
  try {
    await ${name}.get(input.target).signal("approval", { close: true });
    return "signalled";
  } catch (error) {
    if (error instanceof ToolError) return \`refused: \${error.message}\`;
    throw error;
  }
}
`,
    input: { target: target.id },
  });
  const done = await ended(redApi, attacker.id);
  expect(done.labels).toMatchObject({ team: "red" });
  expect(done.status, JSON.stringify(done)).toBe("completed");
  expect(String(done.output.value)).toMatch(/^refused: /);
  const after = await getRun(api, target.id);
  expect(after.status).toBe("active");
  expect(after.historyLen).toBe(before);
  expect(after.waitingFor.some((w: any) => w.kind === "signal" && w.name === "approval")).toBe(true);
  evidence(`ToolError at dispatch: ${done.output.value}`);
  await api.post(`/v1/runs/${target.id}/cancel`, {});
});
