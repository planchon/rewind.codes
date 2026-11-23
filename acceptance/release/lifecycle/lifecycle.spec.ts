/**
 * Lifecycle of the release gate, on setup D: a deployed stack of its own (Postgres for the
 * control plane, a bucket on MinIO for the partitions, the archive and the blobs, a Collector, and
 * two nodes behind an edge), with a retention of seconds, whose nodes this spec kills and restarts
 * and whose Postgres it pauses. It runs alone, after the areas.
 *
 * - Owners (`archi/06-storage-and-scale.md`, "Partitions, owners and nodes"): the node owning a
 *   run's partition killed during an activation, or while a host tool holds the run's call: the
 *   other node takes the partition over and recovers the run, and its call is scheduled once
 *   (the task handed before kept, or handed again with the same idempotency key).
 * - A node that stops hands its partitions over before it exits; one that joins takes its share.
 * - A full restart: a timer due during the outage fires, a signal sent before is kept, and a run's
 *   attached secret is read again (the data keys opened with the same master key).
 * - An outage of Postgres: requests wait or answer 503 `unavailable`, and nothing is lost.
 * - The archive (`archi/06-storage-and-scale.md`, "Retention and deletion"): an ended attempt is
 *   sealed into the bucket and read from there; an erased run leaves nothing in the bucket; an
 *   ended run is erased at the end of its retention.
 * - An erased namespace takes its runs, its secrets and their reads with it.
 */
import { spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { eventually } from "../../support/processes.js";
import {
  allow5xx,
  type Api,
  canary,
  evidence,
  expect,
  expectHealthy,
  expectOk,
  expectProblem,
  gateResults,
  newTenant,
  type Tenant,
  test,
  unique,
} from "../support/gate.js";
import { type McpFixture, startMcpFixture } from "../support/fixtures.js";
import { ended, eventTypes, journal, replayDocument, runScript } from "../support/sandbox-helpers.js";
import { type Stack, startDeployed } from "../support/stack.js";
import { type RunState, TaskStream } from "../support/tools-helpers.js";

// Without setup A (`REWIND_GATE_NO_STACKS=1`) this stage has a canary of its own, published for
// the sweeps.
canary();

/** How long an ended run is kept, for the tenants setup D creates (`--retention`). */
const RETENTION_S = 40;

let stack: Stack;
let tenant: Tenant;
let toolset: string;
let workerKey: string;

test.beforeAll(async () => {
  test.setTimeout(5 * 60_000);
  stack = await startDeployed({
    name: "D · lifecycle",
    dir: join(gateResults, "stack-d"),
    extraArgs: [
      // An ended run is erased this long after its end, for the tenants these nodes create.
      "--retention",
      `${RETENTION_S}s`,
      // The MCP fixture of the restart listens on 127.0.0.1.
      "--allow-internal-tools",
      "127.0.0.1",
    ],
  });
  tenant = await newTenant("lifecycle", stack.info);
  const created = await tenant.api.post("/v1/toolsets", {
    host: [{ name: "t.record", description: "Records a call.", inputSchema: { type: "object" } }],
  });
  expectOk(created);
  toolset = created.json.id;
  workerKey = (await tenant.key("worker")).key;
});

test.afterAll(async () => {
  await stack?.stop();
});

/** One SQL query on setup D's control plane; its rows, `|`-separated. */
function sql(query: string): string[] {
  const { container, database } = stack.info.postgres!;
  const ran = spawnSync("docker", ["exec", container, "psql", "-U", "rewind", "-d", database, "-tAc", query], { encoding: "utf8" });
  if (ran.status !== 0) throw new Error(`psql: ${ran.stderr}`);
  return ran.stdout.split("\n").filter((line) => line.trim() !== "");
}

/** The node owning the partition of the tenant's namespace, from the partition map. */
function ownerOf(namespace: string): string | undefined {
  const owner = sql(
    `SELECT p.node FROM namespaces n JOIN partitions p ON p.id = n.partition WHERE n.tenant = '${tenant.id}' AND n.name = '${namespace}'`,
  )[0];
  return owner === undefined || owner === "" ? undefined : owner;
}

/** The partitions each node owns. */
function partitionsOf(node: string): number {
  return Number(sql(`SELECT count(*) FROM partitions WHERE node = '${node}'`)[0] ?? "0");
}

/** The objects of the bucket under `prefix`. */
function objects(prefix: string): string[] {
  return stack.minio!.objects().filter((key) => key.startsWith(prefix));
}

/**
 * Serves the host tool `t.record` with the worker key on a task stream, opened again with the same
 * worker id after it drops (its node stopped): completes each task handed, but those `hold` keeps
 * for the test to answer. Every hand-out is kept, so a check counts the tool's executions.
 */
class TaskServer {
  readonly handed: RunState[] = [];
  readonly held: RunState[] = [];
  private readonly tokens = new Set<string>();
  private readonly worker = `lifecycle-${unique()}`;
  private stream: TaskStream | undefined;
  private stopped = false;
  private readonly loop: Promise<void>;

  constructor(
    private readonly api: Api,
    private readonly hold: (task: RunState) => boolean = () => false,
  ) {
    this.loop = this.run();
  }

  private async run(): Promise<void> {
    while (!this.stopped) {
      const opened = await TaskStream.attempt(this.api, { toolset, slots: 10, worker: this.worker }).catch(() => undefined);
      if (!(opened instanceof TaskStream)) {
        await delay(250);
        continue;
      }
      if (this.stopped) {
        await opened.close();
        break;
      }
      this.stream = opened;
      opened.onTask = (task) => void this.answer(task);
      for (const task of opened.handed) void this.answer(task);
      await opened.done;
      if (!this.stopped) await delay(250);
    }
  }

  private async answer(task: RunState): Promise<void> {
    if (this.tokens.has(task.token)) return;
    this.tokens.add(task.token);
    this.handed.push(task);
    if (this.hold(task)) {
      this.held.push(task);
      return;
    }
    await this.api.post(`/v1/tasks/${task.token}/complete`, { result: { recorded: task.idempotencyKey } }).catch(() => undefined);
  }

  async stop(): Promise<void> {
    this.stopped = true;
    await this.stream?.close();
    await this.loop;
  }
}

/** The calls the run scheduled, from its journal. */
async function scheduled(run: string): Promise<number> {
  return eventTypes((await journal(tenant.api, run)).doc).filter((type) => type === "schedule_tool_call").length;
}

const CPU_THEN_CALL = `import { t } from "rewind:tools";
function spin(n: number): number {
  let x = 0;
  for (let i = 0; i < n; i++) x = (x + i) % 1000003;
  return x;
}
export default async function main(input: { n: number; call: boolean }) {
  const x = spin(input.n);
  const recorded = input.call ? await t.record({ x }) : null;
  return { x, recorded };
}
`;

test("the owner killed during an activation: the other node takes the partition over, the tool runs once", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const namespace = unique("owners-");
  const api = tenant.api.ns(namespace);
  // A loop of about 1.5 s of CPU on this machine, well under the 5 s limit on a slower node. The
  // journal's times are logical: the wall time of a start that waits for the end measures it.
  const turns = 200_000_000;
  const before = Date.now();
  const calibration = await runScript(tenant.api, CPU_THEN_CALL, { namespace, input: { n: turns, call: false }, toolset });
  const wall = Math.max(100, Date.now() - before);
  expect(calibration.status, JSON.stringify(calibration.error ?? calibration.abort)).toBe("completed");
  const n = Math.min(6 * turns, Math.max(turns / 2, Math.round((turns * 1_500) / wall)));
  const owner = await eventually("the namespace has an owner", () => ownerOf(namespace), 20_000, 50);

  const server = new TaskServer(tenant.api.as(workerKey));
  try {
    const started = await api.post("/v1/runs", { script: CPU_THEN_CALL, input: { n, call: true }, toolset, check: false });
    expectOk(started);
    const id: string = started.json.id;
    // The activation is under way on the owner's executors.
    await delay(300);
    await stack.kill(owner, "SIGKILL");

    const done = await ended(tenant.api, id, 120_000);
    expect(done.status, JSON.stringify(done.error ?? done.abort)).toBe("completed");
    expect(await scheduled(id)).toBe(1);
    expect(server.handed.filter((task) => task.runId === id)).toHaveLength(1);
    expect(done.attempts ?? []).toEqual([]);
    const survivor = ownerOf(namespace);
    expect(survivor).toBeDefined();
    expect(survivor).not.toBe(owner);
    evidence(`${owner}, the owner of the run's partition, killed during its activation; ${survivor} took the partition over and completed the run; one call`);
    await stack.restart(owner);
  } finally {
    await server.stop();
  }
});

test("the owner killed while a host tool holds the run's call: the run completes elsewhere, its call scheduled once", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const namespace = unique("holds-");
  const api = tenant.api.ns(namespace);
  // The first hand-out is held: the host tool is working on it while the owner dies.
  let first: RunState | undefined;
  const server = new TaskServer(tenant.api.as(workerKey), (task) => {
    if (first !== undefined) return false;
    first = task;
    return true;
  });
  try {
    const started = await api.post("/v1/runs", {
      script: 'import { t } from "rewind:tools";\nexport default async function main() { return await t.record({ step: 1 }); }\n',
      toolset,
      check: false,
    });
    expectOk(started);
    const id: string = started.json.id;
    const task = await eventually("the call is handed", () => server.held.find((t) => t.runId === id), 30_000, 50);
    const owner = ownerOf(namespace)!;
    await stack.kill(owner, "SIGKILL");
    // The host tool answers by its token: the new owner takes it, or has given that hand-out up
    // (the node that takes a partition over cannot know which tasks had left) and handed the call
    // again, as attempt 2 with the same idempotency key, which the server answers.
    const answered = await tenant.api.as(workerKey).post(`/v1/tasks/${task.token}/complete`, { result: { recorded: "once" } });
    if (answered.status >= 300) expectProblem(answered, 410, "task-gone");

    const done = await ended(tenant.api, id, 120_000);
    expect(done.status, JSON.stringify(done.error ?? done.abort)).toBe("completed");
    expect(await scheduled(id)).toBe(1);
    expect(eventTypes((await journal(tenant.api, id)).doc)).toEqual(["run_started", "schedule_tool_call", "tool_call_completed", "complete_run"]);
    const handOuts = server.handed.filter((t) => t.runId === id);
    expect(new Set(handOuts.map((t) => t.idempotencyKey))).toEqual(new Set([`${id}/1`]));
    expect([{ recorded: "once" }, { recorded: `${id}/1` }]).toContainEqual(done.output.value);
    const survivor = ownerOf(namespace);
    expect(survivor).toBeDefined();
    expect(survivor).not.toBe(owner);
    evidence(
      `${owner} killed while the call was handed; the first token's answer → ${answered.status}; ${handOuts.length} hand-out(s), each ${id}/1; ${survivor} completed the run; one call scheduled`,
    );
    await stack.restart(owner);
  } finally {
    await server.stop();
  }
});

test("a node that stops hands its partitions over, and takes its share again when it joins", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const [first, second] = stack.info.nodes as [string, string];
  const total = Number(sql("SELECT count(*) FROM partitions")[0]);
  await eventually("every partition has an owner, on both nodes", () =>
    partitionsOf(first) + partitionsOf(second) === total && partitionsOf(second) > 0 ? true : undefined,
  );
  // A stop hands the node's partitions over before the process exits.
  await stack.kill(second, "SIGTERM");
  expect(partitionsOf(second), `${second} kept none once stopped`).toBe(0);
  expect(partitionsOf(first)).toBe(total);
  await eventually("the deployment counts one node", async () => ((await tenant.api.get("/v1/server")).json?.nodes === 1 ? true : undefined));
  await expectHealthy(tenant, unique("alone-"));
  await stack.restart(second);
  await eventually("the node that joined takes its share", () => (partitionsOf(second) > 0 ? true : undefined), 60_000);
  await eventually("the deployment counts both nodes", async () => ((await tenant.api.get("/v1/server")).json?.nodes === 2 ? true : undefined));
  await expectHealthy(tenant, unique("rejoined-"));
  evidence(`${second} stopped: its partitions went to ${first} before it exited (${total} of ${total}); it joined again and took ${partitionsOf(second)} back`);
});

test("a full restart: a timer due during the outage fires, a signal sent before is kept, a secret is read again", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const api = tenant.api.ns("restart");
  const mcp: McpFixture = await startMcpFixture();
  const value = `${canary()}-restart`;
  expectOk(await tenant.api.put("/v1/namespaces/restart/secrets/fx", { value }), 201);
  const keyed = await tenant.api.post("/v1/toolsets", {
    mcp: [{ name: "fx", url: mcp.url, auth: { type: "secret", header: "authorization", format: "Bearer {secret}" } }],
  });
  expectOk(keyed);
  const reading = await runScript(
    tenant.api,
    'import { signal } from "rewind";\nimport { fx } from "rewind:tools";\nexport default async function main() { await signal("go"); return (await fx.echo({ text: "after the restart" })).text(); }\n',
    { namespace: "restart", toolset: keyed.json.id, extra: { wait: "1s", secrets: { fx: "ns/fx" } } },
  );
  expect(reading.waitingFor?.[0]).toMatchObject({ kind: "signal", name: "go" });
  const timer = await runScript(tenant.api, 'import { sleep } from "rewind";\nexport default async function main() { await sleep("4s"); return "timer fired"; }\n', {
    namespace: "restart",
    extra: { wait: "1s" },
  });
  const signal = await runScript(
    tenant.api,
    'import { signal } from "rewind";\nexport default async function main() { return { got: await signal<string>("go") }; }\n',
    { namespace: "restart", extra: { wait: "1s" } },
  );
  expect(timer.waitingFor?.[0]?.kind).toBe("timer");
  expect(signal.waitingFor?.[0]).toMatchObject({ kind: "signal", name: "go" });

  const [first, ...others] = stack.info.nodes as [string, ...string[]];
  for (const name of others) await stack.kill(name, "SIGTERM");
  // The last node takes the signals, its partitions those of every node.
  const sent = await api.post(`/v1/runs/${signal.id}/signals/go`, "sent before the restart", { headers: { "idempotency-key": unique("go-") } });
  expectOk(sent);
  expectOk(await api.post(`/v1/runs/${reading.id}/signals/go`, {}));
  await stack.kill(first, "SIGTERM");
  // Everything is down past the timer's due time.
  await delay(5_000);
  for (const name of stack.info.nodes) await stack.restart(name);

  const firedLate = await ended(tenant.api, timer.id, 60_000);
  expect(firedLate.output?.value, JSON.stringify(firedLate)).toBe("timer fired");
  const signalled = await ended(tenant.api, signal.id, 60_000);
  expect(signalled.output?.value, JSON.stringify(signalled)).toEqual({ got: "sent before the restart" });
  const read = await ended(tenant.api, reading.id, 60_000);
  expect(read.output?.value, JSON.stringify(read)).toBe("after the restart");
  expect(mcp.calls.map((c) => c.headers.authorization)).toEqual([`Bearer ${value}`]);
  await mcp.close();
  await expectHealthy(tenant);
  evidence("every node stopped 5 s, the signals sent to the last one before; the due timer fired at restart, the signals were delivered, and the run's attached ns/fx was read again for its call");
});

test("Postgres paused: requests wait or answer 503, and nothing is lost", { tag: "@failure" }, async () => {
  test.setTimeout(3 * 60_000);
  allow5xx("unavailable");
  const api = tenant.api.ns("outage");
  const sleeper = await runScript(tenant.api, 'import { sleep } from "rewind";\nexport default async function main() { await sleep("3s"); return "after the outage"; }\n', {
    namespace: "outage",
    extra: { wait: "1s" },
  });
  const key = unique("outage-start-");
  const start = () =>
    api.post("/v1/runs", { script: "export default async function main() { return 7; }\n", check: false }, { headers: { "idempotency-key": key } });

  // The control plane and the nodes' liveness locks are both on this Postgres.
  stack.pausePostgres();
  let paused = true;
  try {
    const read = api.get(`/v1/runs/${sleeper.id}`);
    const write = start();
    await delay(10_000);
    stack.unpausePostgres();
    paused = false;
    const answers = await Promise.all([read, write]);
    for (const reply of answers) {
      if (reply.status >= 500) expectProblem(reply, 503, "unavailable");
      else expectOk(reply);
    }
    // A start that was refused, sent again with its key, starts one run.
    const retried = await start();
    expectOk(retried);
    if (answers[1]!.status < 300) expect(retried.json.id).toBe(answers[1]!.json.id);
    const seven = await ended(tenant.api, retried.json.id, 60_000);
    expect(seven.output?.value).toBe(7);
    evidence(`during 10 s of outage: read → ${answers[0]!.status}, start → ${answers[1]!.status}${answers[1]!.slug ? ` ${answers[1]!.slug}` : ""}; never a 500`);
  } finally {
    if (paused) stack.unpausePostgres();
  }
  const done = await ended(tenant.api, sleeper.id, 60_000);
  expect(done.output?.value).toBe("after the outage");
  const listed = await api.get("/v1/runs", { query: { limit: 100 } });
  expectOk(listed);
  expect((listed.json.runs as { id: string }[]).map((run) => run.id)).toContain(sleeper.id);
  await expectHealthy(tenant);
});

test("an ended attempt is sealed into the bucket, and its journal is read from there", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const namespace = unique("archive-");
  const run = await runScript(tenant.api, 'import { sleep } from "rewind";\nexport default async function main() { await sleep(1); return { archived: "sealed" }; }\n', {
    namespace,
  });
  const done = run.status === "completed" ? run : await ended(tenant.api, run.id);
  expect(done.status).toBe("completed");
  const before = await journal(tenant.api, run.id);
  // The sealer writes the attempt's journal document to the archive once the run ended.
  const sealed = `runs/${tenant.id}/${run.id}/1.json`;
  await eventually("the attempt is sealed", () => (objects(sealed).length === 1 ? true : undefined), 60_000, 500);

  // Read from the archive: the same journal, which replays.
  const after = await journal(tenant.api, run.id);
  expect(after.doc.entries).toEqual(before.doc.entries);
  const replayed = replayDocument(after.text);
  expect(replayed.code, replayed.stdout + replayed.stderr).toBe(0);
  const state = await tenant.api.get(`/v1/runs/${run.id}`);
  expectOk(state);
  expect(state.json.output.value).toEqual({ archived: "sealed" });
  evidence(`sealed as ${sealed}; the journal still read and replayed`);
});

test("an erased run leaves nothing: its summary, its sealed journal and its blobs", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const namespace = unique("erasure-");
  const api = tenant.api.ns(namespace);
  const label = unique("erase-");
  // Above 64 KiB: the input goes to the blob store, under its run.
  const big = randomBytes(64 * 1024).toString("hex");
  const run = await runScript(tenant.api, "export default async function main(input: { big: string }) { return input.big.length; }\n", {
    namespace,
    input: { big },
    extra: { labels: { case: label } },
  });
  expect(run.status).toBe("completed");
  const digest: string = run.input.digest;
  const blobs = `blobs/${tenant.id}/${run.id}/`;
  const sealed = `runs/${tenant.id}/${run.id}/`;
  expect(objects(blobs)).toContain(`${blobs}sha256/${digest}`);
  await eventually("the attempt is sealed", () => (objects(sealed).length > 0 ? true : undefined), 60_000, 500);

  const erased = await api.delete(`/v1/runs/${run.id}`);
  expectOk(erased);
  expectProblem(await api.get(`/v1/runs/${run.id}`), 404, "not-found");
  expectProblem(await api.get(`/v1/runs/${run.id}/journal`), 404, "not-found");
  const listed = await api.get("/v1/runs", { query: { label: `case:${label}` } });
  expectOk(listed);
  expect(listed.json.runs).toEqual([]);
  // Its prefixes in the bucket are empty: the payloads went with the run.
  await eventually("the run's objects are deleted", () => (objects(blobs).length + objects(sealed).length === 0 ? true : undefined), 60_000, 500);
  evidence(`DELETE → ${erased.status}; 404 after; nothing listed; nothing under ${blobs} nor ${sealed}`);
});

test("an ended run is erased at the end of its retention", { tag: "@success" }, async () => {
  test.setTimeout(4 * 60_000);
  const namespace = unique("retention-");
  const big = randomBytes(64 * 1024).toString("hex");
  const run = await runScript(tenant.api, "export default async function main(input: { big: string }) { return input.big.length; }\n", {
    namespace,
    input: { big },
  });
  expect(run.status).toBe("completed");
  const ranAt = Date.now();
  const prefixes = [`blobs/${tenant.id}/${run.id}/`, `runs/${tenant.id}/${run.id}/`];
  expectOk(await tenant.api.get(`/v1/runs/${run.id}`));
  // The batch that sealed the run wrote the timer of its retention's end; the owner erases it then.
  await eventually(
    "the run is erased",
    async () => ((await tenant.api.get(`/v1/runs/${run.id}`)).status === 404 ? true : undefined),
    (RETENTION_S + 90) * 1000,
    1_000,
  );
  const kept = Math.round((Date.now() - ranAt) / 1000);
  expect(kept).toBeGreaterThanOrEqual(RETENTION_S - 5);
  await eventually("its objects are deleted", () => (prefixes.every((prefix) => objects(prefix).length === 0) ? true : undefined), 60_000, 1_000);
  evidence(`retention ${RETENTION_S}s: the run answered 404 after ${kept}s, nothing left under ${prefixes.join(" nor ")}`);
});

test("an erased namespace leaves nothing: its runs, secrets, their reads and blobs; its tokens refused", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const name = unique("erase-ns-");
  const api = tenant.api.ns(name);
  const minted = await tenant.api.post(`/v1/namespaces/${name}/tokens`, {});
  expectOk(minted, 201);
  const token: string = minted.json.token;
  expectOk(await tenant.api.put(`/v1/namespaces/${name}/secrets/github`, { value: `${canary()}-erased` }), 201);
  // A finished run with a payload in the blob store, and one still waiting for a signal.
  const big = randomBytes(64 * 1024).toString("hex");
  const done = await runScript(tenant.api, "export default async function main(input: { big: string }) { return input.big.length; }\n", {
    namespace: name,
    input: { big },
  });
  expect(done.status).toBe("completed");
  const blobs = `blobs/${tenant.id}/${done.id}/`;
  expect(objects(blobs).length).toBeGreaterThan(0);
  const waiting = await api.post("/v1/runs", {
    script: 'import { signal } from "rewind";\nexport default async function main() { return await signal<number>("never"); }\n',
  });
  expectOk(waiting, 201);
  const usage = async () => Number((await tenant.api.get("/v1/organization/namespace-defaults")).json?.usage?.runsStarted ?? 0);
  const startedBefore = await usage();

  const erased = await tenant.api.delete(`/v1/namespaces/${name}`);
  expectOk(erased, 202);
  // At once: no new run, no new token, the old token refused, the waiting run cancelled.
  expectProblem(await api.post("/v1/runs", { script: "export default async function main() { return 1; }\n" }), 409, "namespace-deleting");
  expectProblem(await tenant.api.post(`/v1/namespaces/${name}/tokens`, {}), 409, "namespace-deleting");
  expectProblem(await tenant.api.as(token).get("/v1/runs"), 401, "unauthorized");
  await eventually("the waiting run is cancelled", async () => {
    const state = await tenant.api.get(`/v1/runs/${waiting.json.id}`);
    return state.status === 404 || state.json?.status === "cancelled" ? true : undefined;
  });

  // In the background: the owner of its partition erases its runs, then the control plane its
  // secrets, their reads and the namespace.
  await eventually(
    "the namespace is erased",
    () => {
      const left = sql(
        `SELECT (SELECT count(*) FROM namespaces WHERE tenant = '${tenant.id}' AND name = '${name}') +
                (SELECT count(*) FROM secrets WHERE tenant = '${tenant.id}' AND namespace = '${name}') +
                (SELECT count(*) FROM secret_reads WHERE tenant = '${tenant.id}' AND namespace = '${name}')`,
      )[0];
      return left === "0" ? true : undefined;
    },
    120_000,
    2_000,
  );
  expectProblem(await tenant.api.get(`/v1/namespaces/${name}`), 404, "not-found");
  expectProblem(await tenant.api.get(`/v1/runs/${done.id}`), 404, "not-found");
  expectProblem(await tenant.api.get(`/v1/runs/${waiting.json.id}`), 404, "not-found");
  await eventually("the namespace's blobs are deleted", () => (objects(blobs).length === 0 ? true : undefined), 60_000, 1_000);
  // Its opaque token stays refused; its row is purged a day after its revocation (archi/06).
  expectProblem(await tenant.api.as(token).get("/v1/whoami"), 401, "unauthorized");
  // The tenant's own counts keep what the end user consumed (archi/06).
  expect(await usage()).toBeGreaterThanOrEqual(startedBefore);
  evidence("DELETE /v1/namespaces → 202; starts and tokens 409 namespace-deleting; its token 401; the waiting run cancelled; then no namespace, run, secret or secret read, its token refused, and its blobs deleted; the tenant's count of runs kept");
});
