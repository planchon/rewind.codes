/**
 * Starts the setups the area specs share, A (deployed) and B (strict), once for a Playwright
 * invocation, and publishes their addresses to the specs' processes (`REWIND_GATE_STACKS`, a JSON
 * file) with the run's canary secret (`REWIND_GATE_CANARY`). Stops them at the end, unless
 * `REWIND_GATE_KEEP=1`.
 *
 * Nothing starts when `REWIND_GATE_STACKS` already names a file (setups kept from an earlier
 * invocation), or when `REWIND_GATE_NO_STACKS=1` (the invocations of the criteria, the
 * lifecycle and the sweeps, which need none).
 */
import { randomBytes } from "node:crypto";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { dockerAvailable } from "../support/docker.js";
import { gateResults } from "./support/gate.js";
import { type Stack, startDeployed, startStrict } from "./support/stack.js";

export default async function globalSetup(): Promise<(() => Promise<void>) | undefined> {
  if (process.env.REWIND_GATE_NO_STACKS === "1" || process.env.REWIND_GATE_STACKS) return undefined;
  if (!dockerAvailable()) throw new Error("the release gate needs Docker: setup A runs Postgres, MinIO and a Collector");
  mkdirSync(gateResults, { recursive: true });
  const canary = `REWIND-CANARY-${randomBytes(12).toString("hex")}`;
  const started: Stack[] = [];
  try {
    const [a, b] = await Promise.all([
      startDeployed({
        name: "A · deployed",
        dir: join(gateResults, "stack-a"),
        extraArgs: ["--allow-internal-mcp", "127.0.0.1", "--allow-private-webhooks", "--allow-stdio"],
      }),
      startStrict({ name: "B · strict", dir: join(gateResults, "stack-b") }),
    ]);
    started.push(a, b);
    const file = join(gateResults, "stacks.json");
    writeFileSync(file, JSON.stringify({ a: a.info, b: b.info, canary }, null, 2));
    process.env.REWIND_GATE_STACKS = file;
    process.env.REWIND_GATE_CANARY = canary;
  } catch (error) {
    await Promise.all(started.map((stack) => stack.stop()));
    throw error;
  }
  return async () => {
    if (process.env.REWIND_GATE_KEEP === "1") {
      console.log(`kept the setups: REWIND_GATE_STACKS=${process.env.REWIND_GATE_STACKS}`);
      return;
    }
    await Promise.all(started.map((stack) => stack.stop()));
  };
}
