/**
 * Lifecycle of the release gate, on setup D: a deployed stack of its own (Postgres, MinIO, a
 * Collector, an api server and two workers), with retention in seconds, whose servers this spec
 * kills and restarts and whose Postgres it pauses. It runs alone, after the areas.
 *
 * - Leases (`archi/06-storage-and-scale.md`, "Leases, fencing, workers"): a worker killed in the
 *   middle of an activation, or while a host tool holds the run's call, loses the run to the
 *   other worker once its lease expires, and no tool runs twice.
 * - A full restart: a timer due during the outage fires, a signal sent before is kept.
 * - An outage of Postgres: requests wait or answer 503 `unavailable`, and nothing is lost.
 * - Retention (`archi/06-storage-and-scale.md`, "Retention and deletion"): a finished run past its
 *   journal retention is archived to S3 and still read; an erased run leaves no row, and its
 *   payload's blob is deleted after the grace.
 * - The api role runs no script: `POST /v1/test` answers 503 `unavailable` there (08).
 */
import { spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { join } from "node:path";
import { eventually } from "../../support/processes.js";
import {
  allow5xx,
  Api,
  canary,
  evidence,
  expect,
  expectHealthy,
  expectOk,
  expectProblem,
  gateResults,
  newTenant,
  type Reply,
  type Tenant,
  test,
  unique,
} from "../support/gate.js";
import { ended, eventTypes, journal, replayDocument, runScript } from "../support/sandbox-helpers.js";
import { type Stack, startDeployed } from "../support/stack.js";

// Without setup A (`REWIND_GATE_NO_STACKS=1`) this stage has a canary of its own, published for
// the sweeps.
canary();

let stack: Stack;
let tenant: Tenant;
let toolset: string;
let workerKey: string;

test.beforeAll(async () => {
  test.setTimeout(5 * 60_000);
  stack = await startDeployed({
    name: "D · lifecycle",
    dir: join(gateResults, "stack-d"),
    // Retention in seconds, for the tenants these servers create; the housekeeping runs when a
    // worker starts.
    extraArgs: ["--journal-retention", "2s", "--archive-retention", "1h", "--payload-grace", "1s"],
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

/** One SQL query on setup D's Postgres; its rows, `|`-separated. */
function sql(query: string): string[] {
  const ran = spawnSync("docker", ["exec", stack.info.postgres!.container, "psql", "-U", "rewind", "-d", "postgres", "-tAc", query], {
    encoding: "utf8",
  });
  if (ran.status !== 0) throw new Error(`psql: ${ran.stderr}`);
  return ran.stdout.split("\n").filter((line) => line.trim() !== "");
}

/** The worker the store gives the run to first, after its last commit (sticky execution). */
function stickyWorker(run: string): string | undefined {
  const worker = sql(`SELECT sticky_worker FROM runs WHERE id = '${run}'`)[0];
  return worker === undefined || worker === "" ? undefined : worker;
}

/** The worker holding the run's lease, if any. */
function leaseOwner(run: string): string | undefined {
  const owner = sql(`SELECT lease_owner FROM runs WHERE id = '${run}' AND lease_until > (extract(epoch from now()) * 1000)::bigint`)[0];
  return owner === undefined || owner === "" ? undefined : owner;
}

/**
 * Serves the host tool `t.record` with the worker key: claims its tasks and completes them.
 * Every claim is kept, so a check counts the tool's executions.
 */
class TaskServer {
  readonly claimed: { token: string; idempotencyKey: string; runId: string; args: unknown }[] = [];
  private stopped = false;
  private readonly loop: Promise<void>;

  constructor(private readonly api: Api) {
    this.loop = this.run();
  }

  private async run(): Promise<void> {
    while (!this.stopped) {
      let reply: Reply;
      try {
        reply = await this.api.get("/v1/tasks", { query: { toolset, wait: "1s" } });
      } catch {
        continue;
      }
      for (const task of reply.json?.tasks ?? []) {
        this.claimed.push({ token: task.token, idempotencyKey: task.idempotencyKey, runId: task.runId, args: task.args });
        await this.api.post(`/v1/tasks/${task.token}/complete`, { result: { recorded: task.idempotencyKey } });
      }
    }
  }

  async stop(): Promise<void> {
    this.stopped = true;
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

test("a worker killed in the middle of an activation: the other takes the run over, the tool runs once", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const api = tenant.api.ns("leases");
  // A loop of about 1.5 s of CPU on this machine, well under the 5 s limit on a slower worker. The
  // journal's times are logical: the wall time of a start that waits for the end measures it.
  const turns = 200_000_000;
  const before = Date.now();
  const calibration = await runScript(api, CPU_THEN_CALL, { namespace: "leases", input: { n: turns, call: false }, toolset });
  const wall = Math.max(100, Date.now() - before);
  expect(calibration.status, JSON.stringify(calibration.error ?? calibration.abort)).toBe("completed");
  const n = Math.min(6 * turns, Math.max(turns / 2, Math.round((turns * 1_500) / wall)));

  const server = new TaskServer(tenant.api.as(workerKey));
  try {
    const started = await api.post("/v1/runs", { script: CPU_THEN_CALL, input: { n, call: true }, toolset, check: false });
    expectOk(started);
    const id: string = started.json.id;
    const owner = await eventually("a worker takes the run", () => leaseOwner(id), 20_000, 20);
    await stack.kill(owner, "SIGKILL");
    // Killed before its activation committed: nothing of it is in the history.
    const atKill = await api.get(`/v1/runs/${id}`);
    expect(atKill.json.historyLen, "the activation had not committed when its worker died").toBe(0);
    expect(sql(`SELECT count(*) FROM history WHERE run_id = '${id}'`)[0]).toBe("0");

    const done = await ended(tenant.api, id, 120_000);
    expect(done.status, JSON.stringify(done.error ?? done.abort)).toBe("completed");
    expect(await scheduled(id)).toBe(1);
    expect(server.claimed.filter((task) => task.runId === id)).toHaveLength(1);
    expect(done.attempts ?? []).toEqual([]);
    const survivor = stickyWorker(id);
    expect(survivor).toBeDefined();
    expect(survivor).not.toBe(owner);
    evidence(`${owner} killed mid-activation (nothing committed); ${survivor} took the run after its lease and completed it; one call`);
    await stack.restart(owner);
  } finally {
    await server.stop();
  }
});

test("a worker killed while a host tool holds the run's call: the run completes elsewhere, the tool runs once", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const api = tenant.api.ns("leases");
  {
    const started = await api.post("/v1/runs", {
      script: 'import { t } from "rewind:tools";\nexport default async function main() { return await t.record({ step: 1 }); }\n',
      toolset,
      check: false,
    });
    expectOk(started);
    const id: string = started.json.id;
    // The host tool claims the call, and keeps it.
    const task = await eventually(
      "the call is claimed",
      async () => {
        const reply = await tenant.api.as(workerKey).get("/v1/tasks", { query: { toolset, wait: "2s" } });
        return reply.json?.tasks?.[0];
      },
      30_000,
    );
    // No lease is held while the call waits: the run's sticky worker would get it first.
    const owner = await eventually("the run has a sticky worker", () => stickyWorker(id), 20_000, 50);
    await stack.kill(owner, "SIGKILL");
    const completed = await tenant.api.as(workerKey).post(`/v1/tasks/${task.token}/complete`, { result: { recorded: "once" } });
    expectOk(completed);

    const done = await ended(tenant.api, id, 120_000);
    expect(done.status, JSON.stringify(done.error ?? done.abort)).toBe("completed");
    expect(done.output.value).toEqual({ recorded: "once" });
    expect(await scheduled(id)).toBe(1);
    expect(eventTypes((await journal(tenant.api, id)).doc)).toEqual(["run_started", "schedule_tool_call", "tool_call_completed", "complete_run"]);
    const survivor = stickyWorker(id);
    expect(survivor).toBeDefined();
    expect(survivor).not.toBe(owner);
    evidence(`${owner}, the run's sticky worker, was killed while the call was claimed; ${survivor} completed the run; one call`);
    await stack.restart(owner);
  }
});

test("a full restart: a timer due during the outage fires, a signal sent before is kept", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const api = tenant.api.ns("restart");
  const timer = await runScript(api, 'import { sleep } from "rewind";\nexport default async function main() { await sleep("4s"); return "timer fired"; }\n', {
    namespace: "restart",
    extra: { wait: "1s" },
  });
  const signal = await runScript(
    api,
    'import { signal } from "rewind";\nexport default async function main() { return { got: await signal<string>("go") }; }\n',
    { namespace: "restart", extra: { wait: "1s" } },
  );
  expect(timer.waitingFor?.[0]?.kind).toBe("timer");
  expect(signal.waitingFor?.[0]).toMatchObject({ kind: "signal", name: "go" });

  for (const name of stack.info.workers) await stack.kill(name, "SIGTERM");
  // The api server takes the signal while no worker runs.
  const sent = await api.post(`/v1/runs/${signal.id}/signals/go`, "sent before the restart", { headers: { "idempotency-key": unique("go-") } });
  expectOk(sent);
  await stack.kill("api", "SIGTERM");
  // Everything is down past the timer's due time.
  await new Promise((resolve) => setTimeout(resolve, 5_000));
  await stack.restart("api");
  for (const name of stack.info.workers) await stack.restart(name);

  const firedLate = await ended(tenant.api, timer.id, 60_000);
  expect(firedLate.output?.value, JSON.stringify(firedLate)).toBe("timer fired");
  const signalled = await ended(tenant.api, signal.id, 60_000);
  expect(signalled.output?.value, JSON.stringify(signalled)).toEqual({ got: "sent before the restart" });
  await expectHealthy(tenant);
  evidence("api and both workers stopped 5 s; the due timer fired at restart and the signal sent meanwhile was delivered");
});

test("Postgres paused: requests wait or answer 503, and nothing is lost", { tag: "@failure" }, async () => {
  test.setTimeout(3 * 60_000);
  allow5xx("unavailable");
  const api = tenant.api.ns("outage");
  const sleeper = await runScript(api, 'import { sleep } from "rewind";\nexport default async function main() { await sleep("3s"); return "after the outage"; }\n', {
    namespace: "outage",
    extra: { wait: "1s" },
  });
  const key = unique("outage-start-");
  const start = () =>
    api.post("/v1/runs", { script: "export default async function main() { return 7; }\n", check: false }, { headers: { "idempotency-key": key } });

  stack.pausePostgres();
  let paused = true;
  try {
    const read = api.get(`/v1/runs/${sleeper.id}`);
    const write = start();
    await new Promise((resolve) => setTimeout(resolve, 10_000));
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

test("a finished run past its journal retention is archived to S3, and still read", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const api = tenant.api.ns("archive");
  const run = await runScript(api, 'import { sleep } from "rewind";\nexport default async function main() { await sleep(1); return { archived: "later" }; }\n', {
    namespace: "archive",
  });
  const done = run.status === "completed" ? run : await ended(tenant.api, run.id);
  expect(done.status).toBe("completed");
  const before = await journal(tenant.api, run.id);
  // Past the 2 s retention, and every feed item applied: then a worker's housekeeping archives it.
  await eventually("the run's feed is applied", () => (sql(`SELECT count(*) FROM feed WHERE run_id = '${run.id}'`)[0] === "0" ? true : undefined), 60_000);
  await new Promise((resolve) => setTimeout(resolve, 2_500));
  await stack.restart(stack.info.workers[0]!);
  const archive = await eventually(
    "the run is archived",
    () => sql(`SELECT encode(archive_document, 'hex') FROM runs WHERE id = '${run.id}' AND archive_document IS NOT NULL`)[0],
    60_000,
    500,
  );
  expect(sql(`SELECT count(*) FROM history WHERE run_id = '${run.id}'`)[0], "its history left Postgres").toBe("0");
  expect(stack.minio!.objects()).toContain(`${tenant.id}/sha256/${archive}`);

  // Loaded on demand from S3: the same journal, which replays.
  const after = await journal(tenant.api, run.id);
  expect(after.doc.entries).toEqual(before.doc.entries);
  const replayed = replayDocument(after.text);
  expect(replayed.code, replayed.stdout + replayed.stderr).toBe(0);
  const state = await tenant.api.get(`/v1/runs/${run.id}`);
  expectOk(state);
  expect(state.json.output.value).toEqual({ archived: "later" });
  evidence(`archived as ${tenant.id}/sha256/${archive.slice(0, 12)}…; history rows 0; the journal still read and replayed`);
});

test("an erased run leaves no row, and its payload's blob is deleted after the grace", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const api = tenant.api.ns("erasure");
  const label = unique("erase-");
  // Above 64 KiB: the input goes to the blob store.
  const big = randomBytes(64 * 1024).toString("hex");
  const run = await runScript(api, "export default async function main(input: { big: string }) { return input.big.length; }\n", {
    namespace: "erasure",
    input: { big },
    extra: { labels: { case: label } },
  });
  expect(run.status).toBe("completed");
  const digest: string = run.input.digest;
  const blob = `${tenant.id}/sha256/${digest}`;
  expect(stack.minio!.objects()).toContain(blob);

  const erased = await api.delete(`/v1/runs/${run.id}`);
  expectOk(erased);
  expectProblem(await api.get(`/v1/runs/${run.id}`), 404, "not-found");
  expectProblem(await api.get(`/v1/runs/${run.id}/journal`), 404, "not-found");
  const listed = await api.get("/v1/runs", { query: { label: `case:${label}` } });
  expectOk(listed);
  expect(listed.json.runs).toEqual([]);
  for (const table of ["runs", "history", "run_summaries", "run_labels", "payload_refs"]) {
    const column = table === "runs" ? "id" : "run_id";
    expect(sql(`SELECT count(*) FROM ${table} WHERE ${column} = '${run.id}'`)[0], `${table} keeps no row of the run`).toBe("0");
  }

  // After the 1 s grace, a worker's housekeeping deletes the unreferenced payload and its blob.
  await eventually(
    "the blob is deleted",
    async () => {
      await new Promise((resolve) => setTimeout(resolve, 1_500));
      await stack.restart(stack.info.workers[1]!);
      await new Promise((resolve) => setTimeout(resolve, 1_000));
      return stack.minio!.objects().includes(blob) ? undefined : true;
    },
    90_000,
    0,
  );
  expect(sql(`SELECT count(*) FROM payloads WHERE tenant = '${tenant.id}' AND digest = decode('${digest}', 'hex')`)[0]).toBe("0");
  evidence(`DELETE → ${erased.status}; 404 after; no row in runs, history, summaries, labels, refs; ${blob.slice(0, 40)}… deleted`);
});

test("an erased namespace leaves nothing: its runs, usage, script counts and blobs; its tokens refused", { tag: "@success" }, async () => {
  test.setTimeout(3 * 60_000);
  const name = unique("erase-ns-");
  const api = tenant.api.ns(name);
  const minted = await tenant.api.post(`/v1/namespaces/${name}/tokens`, {});
  expectOk(minted, 201);
  const token: string = minted.json.token;
  // A finished run with a payload in the blob store, and one still waiting for a signal.
  const big = randomBytes(64 * 1024).toString("hex");
  const done = await runScript(api, "export default async function main(input: { big: string }) { return input.big.length; }\n", {
    namespace: name,
    input: { big },
  });
  expect(done.status).toBe("completed");
  const blob = `${tenant.id}/sha256/${done.input.digest}`;
  const waiting = await api.post("/v1/runs", {
    script: 'import { signal } from "rewind";\nexport default async function main() { return await signal<number>("never"); }\n',
  });
  expectOk(waiting, 201);

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

  // In the background, at a worker's housekeeping (it runs when a worker starts): everything goes.
  await eventually(
    "the namespace is erased",
    async () => {
      await stack.restart(stack.info.workers[0]!);
      const left = sql(
        `SELECT (SELECT count(*) FROM namespaces WHERE tenant = '${tenant.id}' AND name = '${name}') +
                (SELECT count(*) FROM runs WHERE tenant = '${tenant.id}' AND namespace = '${name}') +
                (SELECT count(*) FROM namespace_scripts WHERE tenant = '${tenant.id}' AND namespace = '${name}')`,
      )[0];
      return left === "0" ? true : undefined;
    },
    120_000,
    2_000,
  );
  expectProblem(await tenant.api.get(`/v1/namespaces/${name}`), 404, "not-found");
  expectProblem(await tenant.api.get(`/v1/runs/${done.id}`), 404, "not-found");
  expect(sql(`SELECT count(*) FROM usage WHERE tenant = '${tenant.id}' AND namespace = '${name}'`)[0], "its usage rows").toBe("0");
  // Its opaque token stays refused; its row is purged a day after its revocation (archi/06).
  expectProblem(await tenant.api.as(token).get("/v1/whoami"), 401, "unauthorized");
  // The tenant's own usage keeps what the end user consumed (archi/06).
  expect(Number(sql(`SELECT coalesce(sum(runs_started), 0) FROM usage WHERE tenant = '${tenant.id}' AND namespace = '*'`)[0])).toBeGreaterThanOrEqual(2);
  // Its payload is no longer referenced: gone after the grace.
  await eventually(
    "the namespace's blob is deleted",
    async () => {
      await stack.restart(stack.info.workers[1]!);
      return stack.minio!.objects().includes(blob) ? undefined : true;
    },
    90_000,
    1_500,
  );
  evidence("DELETE /v1/namespaces → 202; starts and tokens 409 namespace-deleting; its token 401; the waiting run cancelled; then no namespace, run, script count or usage row, its token refused, and its blob deleted; the tenant's usage kept");
});

test("the api role runs no script: /v1/test answers 503 there", { tag: "@failure" }, async () => {
  allow5xx("unavailable");
  // The api server itself, not the edge, which sends /v1/test to the workers.
  const apiRole = new Api(stack.info.apiUrl, tenant.ownerKey);
  const reply = await apiRole.ns("test").post("/v1/test", { script: "export default async function main() { return 1; }\n" });
  expectProblem(reply, 503, "unavailable");
  evidence("POST /v1/test on the api server → 503 unavailable (only workers run scripts)");
});
