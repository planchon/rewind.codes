/**
 * Area "CLI and local" of the release gate, on setup C: the `rewind` binary in a project directory,
 * and the embedded server it starts on demand (`archi/07-code-architecture.md`, "Embedded server
 * and CLI").
 *
 * - Exit codes of 07's table: 0 completed, 1 failed, 2 cancelled, 3 aborted, 4 still waiting,
 *   6 refused (unknown run, a signal to a run that ended), 64 usage, 65 script refused at start.
 *   Not provoked here: 5 (quarantine needs five failed activations of one run), 70 (an internal
 *   fault), and 130 (a second Ctrl-C only exits at once while a call is still in flight, and the
 *   first one finishes the CLI's call within milliseconds: the race cannot be won reliably).
 * - `--json`: results on stdout, errors as problem+json on stderr.
 * - The embedded server: its lock (a second server exits 75), its files' modes, virtual time,
 *   the idle stop, `kill -9` and a resume in the next process.
 * - `rewind replay` of an exported journal, and of tampered ones.
 */
import { type ChildProcess, spawn } from "node:child_process";
import { readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { rewindBinary } from "../../support/paths.js";
import { eventually } from "../../support/processes.js";
import { finish, Project } from "../../support/project.js";
import { evidence, expect, test } from "../support/gate.js";

const OK = "export default async function main(input: { n?: number }) { return (input.n ?? 20) + 1; }\n";
const FAILS = 'export default async function main() { throw new Error("the script gave up"); }\n';
const WAITS = 'import { signal } from "rewind";\nexport default async function main() { return await signal<string>("go"); }\n';
const ABORTS = "export default async function main() { const h: number[][] = []; for (;;) h.push(new Array(100_000).fill(1)); }\n";
const IMPORTS_FS = 'import fs from "node:fs";\nexport default async function main() { return typeof fs; }\n';
const SLEEPS = 'import { sleep } from "rewind";\nexport default async function main() { await sleep("2s"); return "woke"; }\n';
const RANDOM_SLEEPS =
  'import { sleep } from "rewind";\nexport default async function main() { const r = Math.random(); await sleep("1s"); return { r, id: crypto.randomUUID() }; }\n';
const SLEEPS_10_DAYS =
  'import { sleep } from "rewind";\nexport default async function main() { const t0 = Date.now(); await sleep("10d"); return Date.now() - t0; }\n';

let project: Project;

test.beforeAll(async () => {
  project = new Project("rewind-gate-cli");
  for (const [name, text] of Object.entries({
    "ok.ts": OK,
    "fails.ts": FAILS,
    "waits.ts": WAITS,
    "aborts.ts": ABORTS,
    "imports_fs.ts": IMPORTS_FS,
    "sleeps.ts": SLEEPS,
    "random_sleeps.ts": RANDOM_SLEEPS,
  })) {
    project.write(name, text);
  }
});

test.afterAll(async () => {
  await project?.dispose();
});

/** A command's output, for a failed expectation's message. */
const said = (ran: { code: number; stdout: string; stderr: string }) => `exit ${ran.code}\nstdout: ${ran.stdout}\nstderr: ${ran.stderr}`;

test("rewind run, status, trace and export work through the embedded server the CLI starts", { tag: "@success" }, async () => {
  const ran = await project.json(["run", "ok.ts", "--input", '{"n": 41}', "--wait", "30s"]);
  expect(ran.code, said(ran.ran)).toBe(0);
  expect(ran.value).toMatchObject({ status: "completed", namespace: "local", output: { value: 42 } });
  expect(project.servers().length, "the CLI started its embedded server").toBe(1);

  const status = await project.json(["status", ran.value.id]);
  expect(status.code).toBe(0);
  expect(status.value.output.value).toBe(42);

  const trace = await project.rewind(["trace", ran.value.id]);
  expect(trace.code, said(trace)).toBe(0);
  expect(trace.stdout).toContain("42");

  const exported = await project.rewind(["export", ran.value.id, "-o", "exported.json"]);
  expect(exported.code, said(exported)).toBe(0);
  const doc = JSON.parse(readFileSync(project.path("exported.json"), "utf8"));
  expect(doc.entries.map((entry: { event: { type: string } }) => entry.event.type)).toEqual(["run_started", "complete_run"]);

  const replayed = await project.rewind(["replay", "exported.json"]);
  expect(replayed.code, said(replayed)).toBe(0);
  const listed = await project.json(["runs", "--status", "completed"]);
  expect(listed.code).toBe(0);
  expect((listed.value.runs as { id: string }[]).map((run) => run.id)).toContain(ran.value.id);
  evidence("run → 0 with --json; status, trace, export, replay of the export and runs answer");
});

test("a failed run exits 1 and an aborted one 3", { tag: "@failure" }, async () => {
  const failed = await project.json(["run", "fails.ts", "--wait", "30s"]);
  expect(failed.code, said(failed.ran)).toBe(1);
  expect(failed.value).toMatchObject({ status: "failed", error: { name: "Error", message: "the script gave up" } });

  const aborted = await project.json(["run", "aborts.ts", "--wait", "60s"]);
  expect(aborted.code, said(aborted.ran)).toBe(3);
  expect(aborted.value).toMatchObject({ status: "aborted", abort: { kind: "limit_exceeded" } });
  evidence("failed → 1, aborted (heap limit) → 3");
});

test("a waiting run exits 4; cancelled, it exits 2; a signal to it then is refused with 6", { tag: "@failure" }, async () => {
  const waiting = await project.json(["run", "waits.ts", "--wait", "1s"]);
  expect(waiting.code, said(waiting.ran)).toBe(4);
  expect(waiting.value.waitingFor?.[0]).toMatchObject({ kind: "signal", name: "go" });
  const id: string = waiting.value.id;

  const cancelled = await project.rewind(["cancel", id]);
  expect(cancelled.code, said(cancelled)).toBe(0);
  const status = await project.json(["status", id, "--wait", "30s"]);
  expect(status.code, said(status.ran)).toBe(2);
  expect(status.value.status).toBe("cancelled");

  const late = await project.rewind(["signal", id, "go", '"too late"']);
  expect(late.code, said(late)).toBe(6);
  expect(late.stderr).toContain("has ended");
  evidence("waiting → 4, cancelled → 2, a signal after its end → 6");
});

test("a signal from another process completes a waiting run", { tag: "@success" }, async () => {
  const waiting = await project.json(["run", "waits.ts", "--wait", "1s"]);
  expect(waiting.code, said(waiting.ran)).toBe(4);
  const signalled = await project.rewind(["signal", waiting.value.id, "go", '"from another process"']);
  expect(signalled.code, said(signalled)).toBe(0);
  const done = await project.json(["status", waiting.value.id, "--wait", "30s"]);
  expect(done.code).toBe(0);
  expect(done.value.output.value).toBe("from another process");
});

test("unknown runs, usage errors and refused scripts have their exit codes; --json errors are problem+json", { tag: "@failure" }, async () => {
  const unknown = await project.rewind(["--json", "status", "01a1e14c-d0b5-719d-a04a-62088d5117a6"]);
  expect(unknown.code, said(unknown)).toBe(6);
  const problem = JSON.parse(unknown.stderr.trim());
  expect(problem).toMatchObject({ type: "urn:rewind:problem:not-found", status: 404 });

  const usage = await project.rewind(["run", "--no-such-flag"]);
  expect(usage.code, said(usage)).toBe(64);

  const refused = await project.rewind(["run", "imports_fs.ts", "--wait", "5s"]);
  expect(refused.code, said(refused)).toBe(65);
  expect(refused.stderr).toContain('cannot import "node:fs"');
  expect(refused.stderr).toContain("rewind/invalid-script");

  const checked = await project.rewind(["check", "imports_fs.ts"]);
  expect(checked.code, said(checked)).toBe(65);
  expect(checked.stdout + checked.stderr).toContain("rewind/invalid-script");
  evidence("unknown run → 6 with problem+json on stderr; usage → 64; refused script → 65 (run and check)");
});

test("a first Ctrl-C finishes the call in flight and leaves the run waiting", { tag: "@failure" }, async () => {
  const child = project.spawn(["run", "waits.ts", "--wait", "60s"]);
  let stderr = "";
  child.stderr?.setEncoding("utf8").on("data", (chunk: string) => (stderr += chunk));
  const ran = finish(child, 60_000, ["run", "waits.ts"]);
  const id = await eventually("the run has started", () => /started run (\S+)/.exec(stderr)?.[1], 30_000, 50);
  child.kill("SIGINT");
  const ended = await ran;
  expect(ended.code, said({ ...ended, stderr })).toBe(4);
  expect(stderr).toContain("finishing the call in flight");
  const status = await project.json(["status", id]);
  expect(status.value.status, "Ctrl-C leaves the run alone").toBe("active");
  await project.rewind(["cancel", id]);
});

test("a run sleeping when its server is killed resumes in the next process", { tag: "@success" }, async () => {
  const started = await project.json(["run", "sleeps.ts", "--wait", "100ms"]);
  expect(started.code, said(started.ran)).toBe(4);
  const killed = await project.killServers();
  expect(killed.length).toBeGreaterThan(0);
  await new Promise((resolve) => setTimeout(resolve, 2_500));
  // The next command starts a new server; the timer, due during the outage, fires at its start.
  const resumed = await project.json(["status", started.value.id, "--wait", "30s"]);
  expect(resumed.code, said(resumed.ran)).toBe(0);
  expect(resumed.value.output.value).toBe("woke");
  const pids = project.servers();
  expect(pids.length).toBe(1);
  expect(pids).not.toContain(killed[0]);
  evidence(`kill -9 of ${killed.join(", ")}; a new server fired the due timer`);
});

test("a second embedded server on the same directory exits 75, and the first keeps serving", { tag: "@attack" }, async () => {
  const before = await project.json(["run", "ok.ts", "--wait", "30s"]);
  expect(before.code).toBe(0);
  const holder = project.servers();
  expect(holder.length).toBe(1);
  const second = await finish(
    spawn(rewindBinary(), ["server", "--embedded", "--dir", project.state], { cwd: project.root, stdio: ["ignore", "pipe", "pipe"] }),
    30_000,
  );
  expect(second.code, said(second)).toBe(75);
  expect(second.stderr).toContain("another server holds the lock");
  expect(project.servers()).toEqual(holder);
  const after = await project.json(["status", before.value.id]);
  expect(after.code).toBe(0);
  evidence("exit 75: another server holds the lock of .rewind; the first served the next request");
});

test("the embedded server's directory, socket, lock and master key are its owner's only", { tag: "@attack" }, async () => {
  const ran = await project.rewind(["run", "ok.ts", "--wait", "30s"]);
  expect(ran.code).toBe(0);
  const mode = (name: string) => (statSync(join(project.state, name)).mode & 0o777).toString(8);
  expect(mode("."), ".rewind/").toBe("700");
  expect(mode("server.sock"), "server.sock").toBe("600");
  expect(mode("master.key"), "master.key").toBe("600");
  expect(mode("server.lock"), "server.lock").toBe("600");
  evidence(".rewind 0700, server.sock 0600 (the socket takes no token), master.key 0600");
});

test("a tampered journal is refused by replay", { tag: "@attack" }, async () => {
  const ran = await project.json(["run", "random_sleeps.ts", "--wait", "30s"]);
  expect(ran.code, said(ran.ran)).toBe(0);
  const exported = await project.rewind(["export", ran.value.id, "-o", "genuine.json"]);
  expect(exported.code).toBe(0);
  const genuine = JSON.parse(readFileSync(project.path("genuine.json"), "utf8"));
  const genuineReplay = await project.rewind(["replay", "genuine.json"]);
  expect(genuineReplay.code, said(genuineReplay)).toBe(0);

  const tamper = (name: string, change: (doc: any) => void) => {
    const doc = structuredClone(genuine);
    change(doc);
    writeFileSync(project.path(name), JSON.stringify(doc));
    return name;
  };
  const cases: { file: string; code: number; says: string }[] = [
    {
      file: tamper("output.json", (doc) => (doc.entries.at(-1).event.output.value = "forged")),
      code: 1,
      says: "the replay diverges from the journal",
    },
    {
      file: tamper("seed.json", (doc) => (doc.entries[0].event.seed = "00".repeat(32))),
      code: 1,
      says: "the replay diverges from the journal",
    },
    {
      file: tamper("timer.json", (doc) => (doc.entries[1].event.duration_ms = 5)),
      code: 1,
      says: "the replay diverges from the journal",
    },
    {
      file: tamper("script.json", (doc) => (doc.script = String(doc.script).replace('"1s"', '"3s"'))),
      code: 1,
      says: "the journal is inconsistent",
    },
    { file: tamper("version.json", (doc) => (doc.v = 99)), code: 64, says: "unsupported format version 99" },
  ];
  const outcomes: string[] = [];
  for (const { file, code, says } of cases) {
    const replayed = await project.rewind(["--json", "replay", file]);
    expect(replayed.code, `${file}: ${said(replayed)}`).toBe(code);
    expect(replayed.stdout + replayed.stderr, file).toContain(says);
    outcomes.push(`${file} → ${replayed.code}`);
  }
  evidence(`genuine → 0; ${outcomes.join(", ")}`);
});

test("virtual time: a 10-day sleep completes in seconds", { tag: "@success" }, async () => {
  const local = new Project("rewind-gate-vt");
  let server: ChildProcess | undefined;
  try {
    local.write("long.ts", SLEEPS_10_DAYS);
    server = spawn(rewindBinary(), ["server", "--embedded", "--dir", local.state, "--virtual-time"], {
      cwd: local.root,
      stdio: ["ignore", "ignore", "ignore"],
    });
    await eventually("the virtual-time server listens", () => (local.servers().length > 0 ? true : undefined), 30_000, 100);
    const started = Date.now();
    const ran = await local.json(["run", "long.ts", "--wait", "60s"]);
    expect(ran.code, said(ran.ran)).toBe(0);
    expect(ran.value.output.value).toBe(10 * 24 * 3600 * 1000);
    expect(Date.now() - started).toBeLessThan(20_000);
    evidence(`sleep("10d") completed in ${Date.now() - started} ms of wall time`);
  } finally {
    server?.kill("SIGTERM");
    await local.dispose();
  }
});

test("the embedded server stops after its idle timeout", { tag: "@success" }, async () => {
  const local = new Project("rewind-gate-idle");
  try {
    const started = Date.now();
    const ran = await finish(
      spawn(rewindBinary(), ["server", "--embedded", "--dir", local.state, "--idle-timeout", "2s"], {
        cwd: local.root,
        stdio: ["ignore", "pipe", "pipe"],
      }),
      30_000,
    );
    expect(ran.code, said(ran)).toBe(0);
    expect(Date.now() - started).toBeLessThan(15_000);
    evidence(`--idle-timeout 2s: exited 0 after ${Date.now() - started} ms`);
  } finally {
    await local.dispose();
  }
});
