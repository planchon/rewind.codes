/**
 * Criterion 2 (`archi/09-roadmap.md`, "0.1 is done when"), the part `crates/rewind/tests/cli.rs`
 * does not prove on its own: a forgotten `await`, an unhandled rejection and a deadlock each end
 * as `archi/03-execution.md` ("How a run ends") specifies, through `rewind` and the embedded
 * server it starts. The harness actions are served by the SDK's worker over the server's socket.
 *
 * `cli.rs` proves the rest, through the same binary: a run that sleeps, whose server is killed,
 * resumes in another process (`rewind run`, `rewind signal`, `rewind status`, `rewind transcript`, `rewind replay`);
 * `run.sh` runs it as part of this criterion.
 */
import { HarnessActionError, type Worker } from "@rewind.codes/sdk";
import { expect, test } from "@playwright/test";
import { Project } from "../support/project.js";

const ACTION_SET = {
  harness: [
    {
      name: "payments.charge",
      description: "Charge a card.",
      inputSchema: { type: "object", properties: { amount: { type: "number" } }, required: ["amount"] },
    },
    {
      name: "audit.record",
      description: "Record an event.",
      inputSchema: { type: "object", properties: { event: { type: "string" } }, required: ["event"] },
    },
  ],
};

/** Calls of the harness actions, in the order they were answered. */
const answered: { action: string; at: number; ok: boolean }[] = [];
/** The runs by file. */
const runs = new Map<string, string>();

test.describe.serial("criterion 2: how a run ends, through the CLI", () => {
  let project: Project;
  let actionSet: string;
  let worker: Worker;

  test.beforeAll(async () => {
    project = new Project("rewind-c2");
    project.write("actionSet.json", JSON.stringify(ACTION_SET));
    // Registering the action set starts the embedded server.
    const added = await project.json(["action-sets", "add", "actionSet.json"]);
    expect(added.code, added.ran.stderr).toBe(0);
    actionSet = added.value.id;
    worker = project.client().worker({
      actionSet,
      actions: {
        // A card is always declined.
        "payments.charge": async () => {
          answered.push({ action: "payments.charge", at: Date.now(), ok: false });
          throw new HarnessActionError("card declined");
        },
        // An audit record takes a second.
        "audit.record": async () => {
          await new Promise((resolve) => setTimeout(resolve, 1000));
          answered.push({ action: "audit.record", at: Date.now(), ok: true });
          return { ok: true };
        },
      },
      onError: () => undefined,
    });
  });

  test.afterAll(async () => {
    await worker?.stop();
    await project?.dispose();
  });

  /** `rewind run` with the action set, to its end; the run and the events of its history. */
  async function run(name: string, code: string) {
    project.write(name, code);
    // `rewind check` would refuse a forgotten await: this code runs anyway.
    const started = await project.json(["run", name, "--action-set", actionSet, "--no-check", "--wait", "60s"]);
    const id: string = started.value.id;
    runs.set(name, id);
    const exported = await project.json(["export", id]);
    const events: string[] = exported.value.events.map((event: { event: { type: string } }) => event.event.type);
    return { code: started.code, run: started.value, events };
  }

  test("a forgotten await is caught by rewind check, and its call still happens before the run ends", async () => {
    const code = `import { audit } from "rewind:actions";

export default async function main() {
  audit.record({ event: "triaged" }); // no await
  return "done";
}
`;
    project.write("forgotten.ts", code);
    const checked = await project.rewind(["check", "forgotten.ts", "--action-set", "actionSet.json"]);
    expect(checked.code).toBe(65);
    expect(checked.stdout + checked.stderr).toContain("rewind/floating-promise");

    const before = answered.length;
    const ended = await run("forgotten.ts", code);
    expect(ended.code, JSON.stringify(ended.run)).toBe(0);
    expect(ended.run.status).toBe("completed");
    expect(ended.run.output.value).toBe("done");
    // main returned first; complete_run waited for the call's outcome.
    expect(ended.events).toEqual(["run_started", "schedule_call", "call_completed", "complete_run"]);
    expect(answered.slice(before).map((a) => a.action)).toEqual(["audit.record"]);
    expect(Date.parse(ended.run.endedAt)).toBeGreaterThanOrEqual(answered.at(-1)!.at - 50);
  });

  test("a forgotten await whose call fails fails the run, and the run names the call", async () => {
    const ended = await run(
      "forgotten_fails.ts",
      `import { payments } from "rewind:actions";

export default async function main() {
  payments.charge({ amount: 100 }); // no await
  return "charged";
}
`,
    );
    expect(ended.code, JSON.stringify(ended.run)).toBe(1);
    expect(ended.run.status).toBe("failed");
    expect(ended.run.error.name).toBe("ActionError");
    expect(ended.run.error.message).toBe("payments.charge failed: card declined");
    expect(ended.run.error.failedCall).toMatchObject({
      seq: 1,
      action: "payments.charge",
      args: { value: { amount: 100 } },
      error: { message: "card declined" },
      callsite: { line: 4 },
    });
    expect(ended.events).toEqual(["run_started", "schedule_call", "call_failed", "fail_run"]);

    const transcript = await project.rewind(["transcript", ended.run.id]);
    expect(transcript.code).toBe(0);
    expect(transcript.stdout).toContain("payments.charge");
    expect(transcript.stdout).toContain("card declined");
  });

  test("a rejection left unhandled fails the run, even if a handler comes later", async () => {
    const ended = await run(
      "unhandled.ts",
      `import { payments, audit } from "rewind:actions";

export default async function main() {
  const charge = payments.charge({ amount: 100 }); // rejected before it is awaited
  await audit.record({ event: "charging" });
  try {
    await charge;
  } catch {
    return "handled too late";
  }
}
`,
    );
    expect(ended.code, JSON.stringify(ended.run)).toBe(1);
    expect(ended.run.status).toBe("failed");
    expect(ended.run.output).toBeUndefined();
    expect(ended.run.error).toMatchObject({ name: "ActionError", message: "payments.charge failed: card declined" });
    expect(ended.run.error.failedCall).toMatchObject({ seq: 1, action: "payments.charge" });
    // The run does not end while a call is in flight: the audit record has its outcome.
    expect(ended.events.indexOf("call_completed")).toBeGreaterThan(-1);
    expect(ended.events.at(-1)).toBe("fail_run");
  });

  test("a run nothing can wake fails with DeadlockError", async () => {
    const ended = await run(
      "deadlock.ts",
      `export default async function main() {
  await new Promise(() => {}); // nothing can ever resolve it
}
`,
    );
    expect(ended.code, JSON.stringify(ended.run)).toBe(1);
    expect(ended.run.status).toBe("failed");
    expect(ended.run.error.name).toBe("DeadlockError");
    expect(ended.run.error.message).toContain("nothing can wake the code");
    expect(ended.events).toEqual(["run_started", "fail_run"]);

    // Each failed run replays identically from its history.
    const replayed = await project.rewind(["replay", ended.run.id]);
    expect(replayed.code, replayed.stdout + replayed.stderr).toBe(0);
  });

  test("the failed runs are found by error kind", async () => {
    const ids = async (args: string[]) => {
      const listed = await project.json(["runs", ...args]);
      expect(listed.code, listed.ran.stderr).toBe(0);
      return (listed.value.runs as { id: string }[]).map((r) => r.id).sort();
    };
    expect(await ids(["--error", "DeadlockError"])).toEqual([runs.get("deadlock.ts")]);
    expect(await ids(["--error", "ActionError"])).toEqual(
      [runs.get("forgotten_fails.ts"), runs.get("unhandled.ts")].sort(),
    );
    expect(await ids(["--status", "completed"])).toEqual([runs.get("forgotten.ts")]);
  });
});
