/**
 * Lifecycle of the release gate, on setup D: a deployed stack of its own (Postgres for the
 * control plane, a bucket on MinIO for the partitions, the archive and the blobs, a Collector, and
 * two nodes behind an edge), with a retention of seconds, whose nodes this spec kills and restarts
 * and whose Postgres it pauses. It runs alone, after the areas.
 *
 * - Owners (`archi/06-storage-and-scale.md`, "Partitions, owners and nodes"): the node owning a
 *   run's partition killed during an activation, or while a harness action holds the run's call: the
 *   other node takes the partition over and recovers the run, and its call is scheduled once
 *   (the call handed before kept, or handed again with the same idempotency key).
 * - A node that stops hands its partitions over before it exits; one that joins takes its share.
 * - A full restart: a timer due during the outage fires, a signal sent before is kept, and a secret
 *   a run uses is read again (the data keys opened with the same master key).
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
  type Reply,
  evidence,
  expect,
  expectHealthy,
  expectOk,
  expectProblem,
  gateResults,
  newOrg,
  type Org,
  test,
  unique,
} from "../support/gate.js";
import { type McpFixture, startMcpFixture } from "../support/fixtures.js";
import { ended, eventTypes, replayDocument, runCode, runExport } from "../support/sandbox-helpers.js";
import { type Stack, startDeployed } from "../support/stack.js";
import { type RunState, CallStream } from "../support/actions-helpers.js";

// Without setup A (`REWIND_GATE_NO_STACKS=1`) this stage has a canary of its own, published for
// the sweeps.
canary();

/** How long an ended run is kept, for the organizations setup D creates (`--retention`). */
const RETENTION_S = 40;

let stack: Stack;
let org: Org;
let actionSet: string;
let workerKey: string;

test.beforeAll(async () => {
  test.setTimeout(5 * 60_000);
  stack = await startDeployed({
    name: "D · lifecycle",
    dir: join(gateResults, "stack-d"),
    extraArgs: [
      // An ended run is erased this long after its end, for the organizations these nodes create.
      "--retention",
      `${RETENTION_S}s`,
      // The MCP fixture of the restart listens on 127.0.0.1.
      "--allow-internal-addresses",
      "127.0.0.1",
    ],
  });
  org = await newOrg("lifecycle", stack.info);
  const created = await org.api.post("/v1/action-sets", {
    harness: [{ name: "t.record", description: "Records a call.", inputSchema: { type: "object" } }],
  });
  expectOk(created);
  actionSet = created.json.id;
  workerKey = (await org.key("worker")).key;
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

/** The node owning the partition of the organization's namespace, from the partition map. */
function ownerOf(namespace: string): string | undefined {
  const owner = sql(
    `SELECT p.node FROM namespaces n JOIN partitions p ON p.id = n.partition WHERE n.tenant = '${org.id}' AND n.name = '${namespace}'`,
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
 * Serves the harness action `t.record` with the worker key on a call stream, opened again with the same
 * worker id after it drops (its node stopped): completes each call handed, but those `hold` keeps
 * for the test to answer. Every hand-out is kept, so a check counts the action's executions.
 */
class CallServer {
  readonly handed: RunState[] = [];
  readonly held: RunState[] = [];
  private readonly tokens = new Set<string>();
  private readonly worker = `lifecycle-${unique()}`;
  private stream: CallStream | undefined;
  private stopped = false;
  private readonly loop: Promise<void>;

  constructor(
    private readonly api: Api,
    private readonly hold: (call: RunState) => boolean = () => false,
  ) {
    this.loop = this.run();
  }

  private async run(): Promise<void> {
    while (!this.stopped) {
      const opened = await CallStream.tryOpen(this.api, { actionSet, slots: 10, worker: this.worker }).catch(() => undefined);
      if (!(opened instanceof CallStream)) {
        await delay(250);
        continue;
      }
      if (this.stopped) {
        await opened.close();
        break;
      }
      this.stream = opened;
      opened.onCall = (call) => void this.answer(call);
      for (const call of opened.handed) void this.answer(call);
      await opened.done;
      if (!this.stopped) await delay(250);
    }
  }

  private async answer(call: RunState): Promise<void> {
    if (this.tokens.has(call.token)) return;
    this.tokens.add(call.token);
    this.handed.push(call);
    if (this.hold(call)) {
      this.held.push(call);
      return;
    }
    await this.api.post(`/v1/calls/${call.token}/complete`, { result: { recorded: call.idempotencyKey } }).catch(() => undefined);
  }

  async stop(): Promise<void> {
    this.stopped = true;
    await this.stream?.close();
    await this.loop;
  }
}

/**
 * A request sent while a partition moves to another node: answered 503 `unavailable` until the
 * new owner holds it, and sent again, as a client does (`archi/08-decisions.md`, "Taken while
 * cutting the server over to the run plane").
 */
async function whileMoving(send: () => Promise<Reply>, timeoutMs = 60_000): Promise<Reply> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const reply = await send();
    if (reply.status !== 503 || reply.slug !== "unavailable" || Date.now() > deadline) return reply;
    await delay(200);
  }
}

/** The calls the run scheduled, from its history. */
async function scheduled(run: string): Promise<number> {
  return eventTypes((await runExport(org.api, run)).doc).filter((type) => type === "schedule_call").length;
}

const CPU_THEN_CALL = `import { t } from "rewind:actions";
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

test("the owner killed during an activation: the other node takes the partition over, the action runs once", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  // Until the other node takes the partition over, its run's reads answer 503 and are sent again.
  allow5xx("unavailable");
  const namespace = unique("owners-");
  const api = org.api.ns(namespace);
  // A loop of about 1.5 s of CPU on this machine, well under the 5 s limit on a slower node. The
  // history's times are logical: the wall time of a start that waits for the end measures it.
  const turns = 200_000_000;
  const before = Date.now();
  const calibration = await runCode(org.api, CPU_THEN_CALL, { namespace, input: { n: turns, call: false }, actionSet });
  const wall = Math.max(100, Date.now() - before);
  expect(calibration.status, JSON.stringify(calibration.error ?? calibration.abort)).toBe("completed");
  const n = Math.min(6 * turns, Math.max(turns / 2, Math.round((turns * 1_500) / wall)));
  const owner = await eventually("the namespace has an owner", () => ownerOf(namespace), 20_000, 50);

  const server = new CallServer(org.api.as(workerKey));
  try {
    const started = await api.post("/v1/runs", { code: CPU_THEN_CALL, input: { n, call: true }, actionSet, check: false });
    expectOk(started);
    const id: string = started.json.id;
    // The activation is under way on the owner's executors.
    await delay(300);
    await stack.kill(owner, "SIGKILL");

    const done = await ended(org.api, id, 120_000);
    expect(done.status, JSON.stringify(done.error ?? done.abort)).toBe("completed");
    expect(await scheduled(id)).toBe(1);
    expect(server.handed.filter((call) => call.runId === id)).toHaveLength(1);
    expect(done.deliveries ?? []).toEqual([]);
    const survivor = ownerOf(namespace);
    expect(survivor).toBeDefined();
    expect(survivor).not.toBe(owner);
    evidence(`${owner}, the owner of the run's partition, killed during its activation; ${survivor} took the partition over and completed the run; one call`);
    await stack.restart(owner);
  } finally {
    await server.stop();
  }
});

test("the owner killed while a harness action holds the run's call: the run completes elsewhere, its call scheduled once", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  allow5xx("unavailable");
  const namespace = unique("holds-");
  const api = org.api.ns(namespace);
  // The first hand-out is held: the harness action is working on it while the owner dies.
  let first: RunState | undefined;
  const server = new CallServer(org.api.as(workerKey), (call) => {
    if (first !== undefined) return false;
    first = call;
    return true;
  });
  try {
    const started = await api.post("/v1/runs", {
      code: 'import { t } from "rewind:actions";\nexport default async function main() { return await t.record({ step: 1 }); }\n',
      actionSet,
      check: false,
    });
    expectOk(started);
    const id: string = started.json.id;
    const held = await eventually("the call is handed", () => server.held.find((c) => c.runId === id), 30_000, 50);
    const owner = ownerOf(namespace)!;
    await stack.kill(owner, "SIGKILL");
    // The harness action answers by its token: the new owner takes it, or has given that hand-out up
    // (the node that takes a partition over cannot know which calls had left) and handed the call
    // again, as delivery 2 with the same idempotency key, which the server answers.
    const answered = await whileMoving(() => org.api.as(workerKey).post(`/v1/calls/${held.token}/complete`, { result: { recorded: "once" } }));
    if (answered.status >= 300) expectProblem(answered, 410, "call-gone");

    const done = await ended(org.api, id, 120_000);
    expect(done.status, JSON.stringify(done.error ?? done.abort)).toBe("completed");
    expect(await scheduled(id)).toBe(1);
    expect(eventTypes((await runExport(org.api, id)).doc)).toEqual(["run_started", "schedule_call", "call_completed", "complete_run"]);
    const handOuts = server.handed.filter((c) => c.runId === id);
    expect(new Set(handOuts.map((c) => c.idempotencyKey))).toEqual(new Set([`${id}/1`]));
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
  await eventually("the deployment counts one node", async () => ((await org.api.get("/v1/server")).json?.nodes === 1 ? true : undefined));
  await expectHealthy(org, unique("alone-"));
  await stack.restart(second);
  await eventually("the node that joined takes its share", () => (partitionsOf(second) > 0 ? true : undefined), 60_000);
  await eventually("the deployment counts both nodes", async () => ((await org.api.get("/v1/server")).json?.nodes === 2 ? true : undefined));
  await expectHealthy(org, unique("rejoined-"));
  evidence(`${second} stopped: its partitions went to ${first} before it exited (${total} of ${total}); it joined again and took ${partitionsOf(second)} back`);
});

test("a full restart: a timer due during the outage fires, a signal sent before is kept, a secret is read again", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  allow5xx("unavailable");
  const api = org.api.ns("restart");
  const mcp: McpFixture = await startMcpFixture();
  const value = `${canary()}-restart`;
  expectOk(await org.api.put("/v1/namespaces/restart/secrets/fx", { value }), 201);
  const keyed = await org.api.post("/v1/action-sets", {
    mcp: [{ name: "fx", url: mcp.url, auth: { type: "secret", header: "authorization", format: "Bearer {secret}" } }],
  });
  expectOk(keyed);
  const reading = await runCode(
    org.api,
    'import { signal } from "rewind";\nimport { fx } from "rewind:actions";\nexport default async function main() { await signal("go"); return (await fx.echo({ text: "after the restart" })).text(); }\n',
    { namespace: "restart", actionSet: keyed.json.id, extra: { wait: "1s", secrets: { fx: "ns/fx" } } },
  );
  expect(reading.waitingFor?.[0]).toMatchObject({ kind: "signal", name: "go" });
  const timer = await runCode(org.api, 'import { sleep } from "rewind";\nexport default async function main() { await sleep("4s"); return "timer fired"; }\n', {
    namespace: "restart",
    extra: { wait: "1s" },
  });
  const signal = await runCode(
    org.api,
    'import { signal } from "rewind";\nexport default async function main() { return { got: await signal<string>("go") }; }\n',
    { namespace: "restart", extra: { wait: "1s" } },
  );
  expect(timer.waitingFor?.[0]?.kind).toBe("timer");
  expect(signal.waitingFor?.[0]).toMatchObject({ kind: "signal", name: "go" });

  const [first, ...others] = stack.info.nodes as [string, ...string[]];
  for (const name of others) await stack.kill(name, "SIGTERM");
  // The last node takes the signals, its partitions those of every node.
  const go = unique("go-");
  const sent = await whileMoving(() => api.post(`/v1/runs/${signal.id}/signals/go`, "sent before the restart", { headers: { "idempotency-key": go } }));
  expectOk(sent);
  const readingGo = unique("go-");
  expectOk(await whileMoving(() => api.post(`/v1/runs/${reading.id}/signals/go`, {}, { headers: { "idempotency-key": readingGo } })));
  await stack.kill(first, "SIGTERM");
  // Everything is down past the timer's due time.
  await delay(5_000);
  for (const name of stack.info.nodes) await stack.restart(name);

  const firedLate = await ended(org.api, timer.id, 60_000);
  expect(firedLate.output?.value, JSON.stringify(firedLate)).toBe("timer fired");
  const signalled = await ended(org.api, signal.id, 60_000);
  expect(signalled.output?.value, JSON.stringify(signalled)).toEqual({ got: "sent before the restart" });
  const read = await ended(org.api, reading.id, 60_000);
  expect(read.output?.value, JSON.stringify(read)).toBe("after the restart");
  // The call may have been in flight when its node stopped: the next owner sends it again with
  // the same idempotency key (`idempotent: true`), and each time with the run's secret.
  expect(mcp.calls.length).toBeGreaterThanOrEqual(1);
  expect(mcp.calls.every((c) => c.headers.authorization === `Bearer ${value}`), "every call carries the run's secret").toBe(true);
  expect(new Set(mcp.calls.map((c) => JSON.stringify(c.meta?.["codes.rewind/idempotency-key"]))).size, "one idempotency key").toBe(1);
  await mcp.close();
  await expectHealthy(org);
  evidence("every node stopped 5 s, the signals sent to the last one before; the due timer fired at restart, the signals were delivered, and the ns/fx the run uses was read again for its call");
});

test("Postgres paused: requests wait or answer 503, and nothing is lost", { tag: "@failure" }, async () => {
  test.setTimeout(3 * 60_000);
  allow5xx("unavailable");
  const api = org.api.ns("outage");
  const sleeper = await runCode(org.api, 'import { sleep } from "rewind";\nexport default async function main() { await sleep("3s"); return "after the outage"; }\n', {
    namespace: "outage",
    extra: { wait: "1s" },
  });
  const key = unique("outage-start-");
  const start = () =>
    api.post("/v1/runs", { code: "export default async function main() { return 7; }\n", check: false }, { headers: { "idempotency-key": key } });

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
    const again = await start();
    expectOk(again);
    if (answers[1]!.status < 300) expect(again.json.id).toBe(answers[1]!.json.id);
    const seven = await ended(org.api, again.json.id, 60_000);
    expect(seven.output?.value).toBe(7);
    evidence(`during 10 s of outage: read → ${answers[0]!.status}, start → ${answers[1]!.status}${answers[1]!.slug ? ` ${answers[1]!.slug}` : ""}; never a 500`);
  } finally {
    if (paused) stack.unpausePostgres();
  }
  const done = await ended(org.api, sleeper.id, 60_000);
  expect(done.output?.value).toBe("after the outage");
  const listed = await api.get("/v1/runs", { query: { limit: 100 } });
  expectOk(listed);
  expect((listed.json.runs as { id: string }[]).map((run) => run.id)).toContain(sleeper.id);
  await expectHealthy(org);
});

test("an ended attempt is sealed into the bucket, and its history is read from there", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const namespace = unique("archive-");
  const run = await runCode(org.api, 'import { sleep } from "rewind";\nexport default async function main() { await sleep(1); return { archived: "sealed" }; }\n', {
    namespace,
  });
  const done = run.status === "completed" ? run : await ended(org.api, run.id);
  expect(done.status).toBe("completed");
  const before = await runExport(org.api, run.id);
  // The sealer writes the attempt's run export to the archive once the run ended.
  const sealed = `runs/${org.id}/${run.id}/1.json`;
  await eventually("the attempt is sealed", () => (objects(sealed).length === 1 ? true : undefined), 60_000, 500);

  // Read from the archive: the same history, which replays.
  const after = await runExport(org.api, run.id);
  expect(after.doc.events).toEqual(before.doc.events);
  const replayed = replayDocument(after.text);
  expect(replayed.code, replayed.stdout + replayed.stderr).toBe(0);
  const state = await org.api.get(`/v1/runs/${run.id}`);
  expectOk(state);
  expect(state.json.output.value).toEqual({ archived: "sealed" });
  evidence(`sealed as ${sealed}; the history still read and replayed`);
});

test("an erased run leaves nothing: its summary, its sealed history and its blobs", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const namespace = unique("erasure-");
  const api = org.api.ns(namespace);
  const label = unique("erase-");
  // Above 64 KiB: the input goes to the blob store, under its run.
  const big = randomBytes(64 * 1024).toString("hex");
  const run = await runCode(org.api, "export default async function main(input: { big: string }) { return input.big.length; }\n", {
    namespace,
    input: { big },
    extra: { labels: { case: label } },
  });
  expect(run.status).toBe("completed");
  const digest: string = run.input.digest;
  const blobs = `blobs/${org.id}/${run.id}/`;
  const sealed = `runs/${org.id}/${run.id}/`;
  expect(objects(blobs)).toContain(`${blobs}sha256/${digest}`);
  await eventually("the attempt is sealed", () => (objects(sealed).length > 0 ? true : undefined), 60_000, 500);

  const erased = await api.delete(`/v1/runs/${run.id}`);
  expectOk(erased);
  expectProblem(await api.get(`/v1/runs/${run.id}`), 404, "not-found");
  expectProblem(await api.get(`/v1/runs/${run.id}/export`), 404, "not-found");
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
  const run = await runCode(org.api, "export default async function main(input: { big: string }) { return input.big.length; }\n", {
    namespace,
    input: { big },
  });
  expect(run.status).toBe("completed");
  const ranAt = Date.now();
  const prefixes = [`blobs/${org.id}/${run.id}/`, `runs/${org.id}/${run.id}/`];
  expectOk(await org.api.get(`/v1/runs/${run.id}`));
  // The batch that sealed the run wrote the timer of its retention's end; the owner erases it then.
  await eventually(
    "the run is erased",
    async () => ((await org.api.get(`/v1/runs/${run.id}`)).status === 404 ? true : undefined),
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
  const api = org.api.ns(name);
  const minted = await org.api.post(`/v1/namespaces/${name}/tokens`, {});
  expectOk(minted, 201);
  const token: string = minted.json.token;
  expectOk(await org.api.put(`/v1/namespaces/${name}/secrets/github`, { value: `${canary()}-erased` }), 201);
  // A finished run with a payload in the blob store, and one still waiting for a signal.
  const big = randomBytes(64 * 1024).toString("hex");
  const done = await runCode(org.api, "export default async function main(input: { big: string }) { return input.big.length; }\n", {
    namespace: name,
    input: { big },
  });
  expect(done.status).toBe("completed");
  const blobs = `blobs/${org.id}/${done.id}/`;
  expect(objects(blobs).length).toBeGreaterThan(0);
  const waiting = await api.post("/v1/runs", {
    code: 'import { signal } from "rewind";\nexport default async function main() { return await signal<number>("never"); }\n',
  });
  expectOk(waiting, 201);
  const usage = async () => Number((await org.api.get("/v1/organization/namespace-defaults")).json?.usage?.runsStarted ?? 0);
  const startedBefore = await usage();

  const erased = await org.api.delete(`/v1/namespaces/${name}`);
  expectOk(erased, 202);
  // At once: no new run, no new token, the old token refused, the waiting run cancelled.
  expectProblem(await api.post("/v1/runs", { code: "export default async function main() { return 1; }\n" }), 409, "namespace-deleting");
  expectProblem(await org.api.post(`/v1/namespaces/${name}/tokens`, {}), 409, "namespace-deleting");
  expectProblem(await org.api.as(token).get("/v1/runs"), 401, "unauthorized");
  await eventually("the waiting run is cancelled", async () => {
    const state = await org.api.get(`/v1/runs/${waiting.json.id}`);
    return state.status === 404 || state.json?.status === "cancelled" ? true : undefined;
  });

  // In the background: the owner of its partition erases its runs, then the control plane its
  // secrets, their reads and the namespace.
  await eventually(
    "the namespace is erased",
    () => {
      const left = sql(
        `SELECT (SELECT count(*) FROM namespaces WHERE tenant = '${org.id}' AND name = '${name}') +
                (SELECT count(*) FROM secrets WHERE tenant = '${org.id}' AND namespace = '${name}') +
                (SELECT count(*) FROM secret_reads WHERE tenant = '${org.id}' AND namespace = '${name}')`,
      )[0];
      return left === "0" ? true : undefined;
    },
    120_000,
    2_000,
  );
  expectProblem(await org.api.get(`/v1/namespaces/${name}`), 404, "not-found");
  expectProblem(await org.api.get(`/v1/runs/${done.id}`), 404, "not-found");
  expectProblem(await org.api.get(`/v1/runs/${waiting.json.id}`), 404, "not-found");
  await eventually("the namespace's blobs are deleted", () => (objects(blobs).length === 0 ? true : undefined), 60_000, 1_000);
  // Its opaque token stays refused; its row is purged a day after its revocation (archi/06).
  expectProblem(await org.api.as(token).get("/v1/whoami"), 401, "unauthorized");
  // The organization's own counts keep what the end user consumed (archi/06).
  expect(await usage()).toBeGreaterThanOrEqual(startedBefore);
  evidence("DELETE /v1/namespaces → 202; starts and tokens 409 namespace-deleting; its token 401; the waiting run cancelled; then no namespace, run, secret or secret read, its token refused, and its blobs deleted; the organization's count of runs kept");
});
