/**
 * Area "Workflows" of the release gate, on setup A (deployed): creating workflows and making
 * their versions, starting runs of them from the API and from code, their typed inputs and
 * signals, attached and detached children, forks and promotions, and what a key limited to
 * labels sees (`archi/01-product.md`, "Workflows"; `archi/03-execution.md`, "Workflows across
 * runs"; `archi/06`, "What the caller may do").
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
/** An empty action set: the workflows' own. */
let emptyActionSet: string;

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
    actionSet: emptyActionSet,
    code: script,
    input: { type: "object", properties: { repo: { type: "string" } }, required: ["repo"] },
    output: { type: "object", properties: { closed: { type: "integer" } }, required: ["closed"] },
    signals: {
      approval: { type: "object", properties: { close: { type: "boolean" } }, required: ["close"] },
    },
  };
}

/** A workflow name of its own: an identifier. */
const fresh = (prefix = "triage") => `${prefix}_${randomBytes(3).toString("hex")}`;

/** Creates a workflow, the organization's without a namespace; returns it. */
async function save(as: Api, definition: ReturnType<typeof triage>): Promise<any> {
  const reply = await as.post("/v1/workflows", definition);
  expectOk(reply, 201);
  return reply.json;
}

/** The next version of a workflow, with this code. */
async function version(as: Api, workflow: string, code: string): Promise<any> {
  const reply = await as.post(`/v1/workflows/${workflow}/versions`, { code });
  expectOk(reply, 201);
  return reply.json;
}

/** An action set that pins these organization's workflows. */
async function pinning(as: Api, ...workflows: string[]): Promise<string> {
  const reply = await as.post("/v1/action-sets", { workflows });
  expectOk(reply);
  return reply.json.id;
}

/** A run's workflow, as the API shows it. */
const runOf = (workflow: any, number: number, digest: string) => ({
  id: workflow.id,
  name: workflow.name,
  version: number,
  digest,
});

test.beforeAll(async () => {
  tenant = await newTenant("workflows");
  api = tenant.api.ns("wf");
  const created = await tenant.api.post("/v1/action-sets", {});
  expectOk(created);
  emptyActionSet = created.json.id;
});

// ---------------------------------------------------------------------------------------------
// Success

test("creating a workflow checks it and gives its id and declarations; its name is taken after", { tag: "@success" }, async () => {
  const name = fresh();
  const created = await tenant.api.post("/v1/workflows", triage(name));
  expectOk(created, 201);
  expect(created.json).toMatchObject({ name, version: { version: 1, actionSet: emptyActionSet } });
  expect(created.json.id).toMatch(/^wf_[0-9a-z]{12}$/);
  expect(created.json.namespace ?? null).toBeNull();
  expect(created.json.version.digest).toMatch(/^[0-9a-f]{64}$/);
  expect(created.json.version.declarations.dts).toContain(`export const ${name}: Workflow<`);
  expect(created.json.version.declarations.prompt.startsWith(`${name}.start({ repo: string }) → run`), created.json.version.declarations.prompt).toBe(true);
  expectProblem(await tenant.api.post("/v1/workflows", triage(name)), 409, "name-taken");
  // The same code again makes no version.
  const again = await tenant.api.post(`/v1/workflows/${name}/versions`, { code: TRIAGE });
  expectOk(again, 200);
  expect(again.json.version.digest).toBe(created.json.version.digest);
});

test("each change is a new version: versions are listed newest first, the list shows the latest", { tag: "@success" }, async () => {
  const name = fresh();
  const first = await save(tenant.api, triage(name));
  const second = await version(tenant.api, name, TRIAGE.replace("? 3 : 0", "? 5 : 0"));
  expect(second.id).toBe(first.id);
  expect(second.version.digest).not.toBe(first.version.digest);
  const read = await tenant.api.get(`/v1/workflows/${name}`);
  expectOk(read, 200);
  expect(read.json.versions.map((v: any) => v.digest)).toEqual([second.version.digest, first.version.digest]);
  // Numbered from 1 in the order they were made.
  expect(read.json.versions.map((v: any) => v.version)).toEqual([2, 1]);
  const listed = await tenant.api.get("/v1/workflows", { query: { limit: 200 } });
  expectOk(listed, 200);
  const mine = listed.json.workflows.filter((w: any) => w.id === first.id);
  expect(mine).toHaveLength(1);
  expect(mine[0].latest.digest).toBe(second.version.digest);
});

test("the harness starts a run of a workflow by name, or by name@version, and signals it", { tag: "@success" }, async () => {
  const name = fresh();
  const first = await save(tenant.api, triage(name));
  const second = await version(tenant.api, name, TRIAGE.replace("? 3 : 0", "? 5 : 0"));

  const latest = await started(api, { workflow: name, input: { repo: "acme/api" } });
  expect(latest.workflow).toEqual(runOf(first, 2, second.version.digest));
  const pinned = await started(api, { workflow: `${name}@${first.version.digest.slice(0, 12)}`, input: { repo: "acme/web" } });
  expect(pinned.workflow).toEqual(runOf(first, 1, first.version.digest));
  // A version by its number, and the workflow by its id, name them too.
  const numbered = await started(api, { workflow: `${first.id}@v1`, input: { repo: "acme/app" } });
  expect(numbered.workflow).toEqual(runOf(first, 1, first.version.digest));

  for (const run of [latest, pinned, numbered]) {
    await waitingFor(api, run.id, "signal", "approval");
    expectOk(await api.post(`/v1/runs/${run.id}/signals/approval`, { close: true }), 202);
  }
  expect((await ended(api, latest.id)).output.value).toEqual({ closed: 5, repo: "acme/api" });
  expect((await ended(api, pinned.id)).output.value).toEqual({ closed: 3, repo: "acme/web" });
  expect((await ended(api, numbered.id)).output.value).toEqual({ closed: 3, repo: "acme/app" });
});

test("a script starts a run of a workflow, signals it and receives its result", { tag: "@success" }, async () => {
  const name = fresh();
  const saved = await save(tenant.api, triage(name));
  const actionSet = await pinning(tenant.api, name);
  const parent = await started(api, {
    actionSet,
    input: { repo: "acme/api" },
    wait: "30s",
    code: `import { ${name} } from "rewind:workflows";

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
  expect(child.workflow).toEqual(runOf(saved, 1, saved.version.digest));
  expect(child.startedBy).toMatchObject({ run: parent.id, detached: false });
});

test("a detached child lives on after its parent; a failing parent cancels its attached children", { tag: "@success" }, async () => {
  const name = fresh();
  await save(tenant.api, triage(name));
  const actionSet = await pinning(tenant.api, name);

  const detachedParent = await started(api, {
    actionSet,
    wait: "30s",
    code: `import { ${name} } from "rewind:workflows";
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
    actionSet,
    code: `import { ${name} } from "rewind:workflows";
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

test("code that does not match its workflow's schemas is 422 invalid-code, with the diagnostic", { tag: "@failure" }, async () => {
  const reply = await tenant.api.post("/v1/workflows", triage(fresh(), TRIAGE.replace("input: { repo: string }", "input: { repo: number }")));
  expectProblem(reply, 422, "invalid-code");
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

test("a version whose tests do not pass is 422 tests-failed with each outcome, and nothing is made", { tag: "@failure" }, async () => {
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

test("a suite with two tests of the same name is 422 invalid-test, and nothing is made", { tag: "@failure" }, async () => {
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

test("an unknown workflow is 404; a malformed version or a workflow with code is 400", { tag: "@failure" }, async () => {
  const name = fresh();
  await save(tenant.api, triage(name));
  expectProblem(await startRun(api, { workflow: fresh("nothing"), input: {} }), 404, "not-found");
  expectProblem(await startRun(api, { workflow: `${name}@${"0".repeat(12)}`, input: { repo: "x" } }), 404, "not-found");
  expectProblem(await startRun(api, { workflow: `${name}@xyz`, input: { repo: "x" } }), 400, "invalid-request");
  expectProblem(await startRun(api, { workflow: name, code: TRIAGE, input: { repo: "x" } }), 400, "invalid-request");
  expectProblem(await tenant.api.get(`/v1/workflows/${fresh("nothing")}`), 404, "not-found");
});

test("a script that starts a workflow with a wrong input gets a TypeError at the call", { tag: "@failure" }, async () => {
  const name = fresh();
  await save(tenant.api, triage(name));
  const actionSet = await pinning(tenant.api, name);
  const run = await started(api, {
    actionSet,
    check: false,
    code: `import { ${name} } from "rewind:workflows";
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
  const actionSet = await pinning(tenant.api, name);
  const parent = await started(api, {
    actionSet,
    code: `import { ${name} } from "rewind:workflows";
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

test("a key limited to labels sees only the workflows made under its labels, and starts its own", { tag: "@success" }, async () => {
  const name = fresh("env");
  const first = await save(tenant.api, triage(name));
  const created = await tenant.api.post("/v1/api-keys", { role: "harness", name: "dev environment", allowedLabels: { env: "dev" } });
  expectOk(created, 201);
  const dev = new Api(stacks().a.url, created.json.key as string).ns("wf");

  // The organization's version carries no label: the dev key does not see it.
  expectProblem(await dev.get(`/v1/workflows/${first.id}`), 404, "not-found");
  // Its own, in the namespace, shadows the organization's name there.
  const ours = await save(dev, triage(name));
  expect(ours.id).not.toBe(first.id);
  expect(ours.namespace).toBe("wf");
  expect(ours.labels).toEqual({ env: "dev" });
  const listed = await dev.get("/v1/workflows");
  expectOk(listed, 200);
  expect(listed.json.workflows.map((w: any) => w.id)).toEqual([ours.id]);
  const run = await started(dev, { workflow: name, input: { repo: "acme/dev" } });
  expect(run.workflow.id).toBe(ours.id);
  expect(run.labels).toMatchObject({ env: "dev" });
  evidence(`${name}: the organization's ${first.id}, env=dev's own ${ours.id}; the dev key lists and starts its own`);
  await dev.post(`/v1/runs/${run.id}/cancel`, {});
});

test("a namespace forks an organization's workflow, pulls its next version through its suite, and promotes its own", { tag: "@success" }, async () => {
  const name = fresh("gym");
  const source = await save(tenant.api, { ...triage(name), tests: [closes(3)] });
  const alice = tenant.api.ns(`alice-${randomBytes(2).toString("hex")}`);
  const fork = await alice.post(`/v1/workflows/${name}/fork`);
  expectOk(fork, 201);
  expect(fork.json).toMatchObject({ name, source: { workflow: source.id, version: 1 } });
  expect(fork.json.id).not.toBe(source.id);
  expect(fork.json.suite.map((t: any) => t.name)).toEqual(["closes 3 once approved"]);
  // Its name shadows the organization's in its namespace.
  const run = await started(alice, { workflow: name, input: { repo: "acme/api" } });
  expect(run.workflow.id).toBe(fork.json.id);
  await alice.post(`/v1/runs/${run.id}/cancel`, {});
  // A source's version that breaks the fork's suite is not pulled.
  const breaking = await tenant.api.post(`/v1/workflows/${name}/versions`, {
    code: TRIAGE.replace("? 3 : 0", "? 5 : 0"),
    replace: [{ name: "closes 3 once approved", reason: "closes five now", expect: { status: "completed", output: { closed: 5 } } }],
  });
  expectOk(breaking, 201);
  expectProblem(await alice.post(`/v1/workflows/${name}/pull`), 422, "tests-failed");
  expectProblem(await alice.post(`/v1/workflows/${fork.json.id}/fork`), 409, "not-forkable");
  expectProblem(await tenant.api.post(`/v1/workflows/${source.id}/pull`), 409, "not-a-fork");

  // A workflow a run created in the namespace, named, then promoted to the organization.
  const own = await started(alice, { code: `export default async function main() { return "${name}"; }\n` });
  const promotedName = fresh("own");
  expectOk(await alice.patch(`/v1/workflows/${own.workflow.id}`, { name: promotedName }), 200);
  const promoted = await alice.post(`/v1/workflows/${promotedName}/promote`);
  expectOk(promoted, 200);
  expect(promoted.json.id).toBe(own.workflow.id);
  expect(promoted.json.namespace ?? null).toBeNull();
  evidence(`fork ${fork.json.id} of ${source.id}: pull refused by its suite; ${own.workflow.id} promoted`);
});

test("a script started by a key limited to labels cannot signal a run outside its scope", { tag: "@attack" }, async () => {
  // The key limited to `team: red` creates its own organization's workflow (whose version carries
  // its labels) and an action set that pins it. `Tenant.key` sends `labels`; the API takes
  // `allowedLabels`.
  const created = await tenant.api.post("/v1/api-keys", { role: "harness", name: "red team", allowedLabels: { team: "red" } });
  expectOk(created, 201);
  const red = { key: created.json.key as string };
  const redOrganization = new Api(stacks().a.url, red.key);
  const redApi = redOrganization.ns("wf");
  const name = fresh("scoped");
  const saved = await save(redOrganization, triage(name));
  expect(saved.labels).toEqual({ team: "red" });
  const actionSet = await pinning(redApi, name);

  // A run of the same workflow, started by the owner without labels: outside the red scope.
  const target = await started(api, { workflow: name, input: { repo: "acme/blue" } });
  await waitingFor(api, target.id, "signal", "approval");
  const before = (await getRun(api, target.id)).historyLen;

  const attacker = await started(redApi, {
    actionSet,
    code: `import { ${name} } from "rewind:workflows";
import { ActionError } from "rewind";
export default async function main(input: { target: string }) {
  try {
    await ${name}.get(input.target).signal("approval", { close: true });
    return "signalled";
  } catch (error) {
    if (error instanceof ActionError) return \`refused: \${error.message}\`;
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
  evidence(`ActionError at dispatch: ${done.output.value}`);
  await api.post(`/v1/runs/${target.id}/cancel`, {});
});
