/**
 * The agent tools against a live Rewind server: the bound a model meets through `run_script`.
 * Skipped unless `REWIND_LIVE_URL` and `REWIND_LIVE_KEY` (a key with the role `harness`) are set, as
 * the deployment's CI job and `deploy/README.md` do:
 *
 *   REWIND_LIVE_URL=http://localhost:8080 REWIND_LIVE_KEY=rewind_sk_… pnpm vitest run test/live.test.ts
 */

import { describe, expect, it } from "vitest";

import { OUTPUT_LIMIT, Rewind } from "../src/index.js";

const url = process.env["REWIND_LIVE_URL"];
const apiKey = process.env["REWIND_LIVE_KEY"];

/** A script whose output, as compact JSON, is about 600 KB: above `run_script`'s 256 KiB. */
const LARGE_OUTPUT = `export default async function main() {
  return { rows: Array.from({ length: 6000 }, (_, i) => ({ i, text: "row " + i + " " + "x".repeat(80) })) };
}`;

function expectedOutput(): string {
  const rows = Array.from({ length: 6000 }, (_, i) => ({ i, text: `row ${i} ${"x".repeat(80)}` }));
  return JSON.stringify({ rows });
}

describe.skipIf(url === undefined || apiKey === undefined)("against a live server", () => {
  it("run_script answers at most 256 KiB of output, and get_run pages through the rest", async () => {
    const rewind = new Rewind({ url: url ?? "", apiKey: apiKey ?? "" });
    const tools = rewind.namespace("sdk-live").agentTools({ labels: { suite: "sdk-live" }, wait: "60s" });
    const result = await tools.handle("run_script", { code: LARGE_OUTPUT }, { toolCallId: `live-${Date.now()}` });
    expect(result.isError, result.content).toBe(false);
    expect(result.run?.status).toBe("completed");

    const full = expectedOutput();
    const lines = result.content.split("\n");
    const header = lines.findIndex((line) => line.startsWith("Output ("));
    expect(lines[header]).toContain(`Output (${Buffer.byteLength(full)} bytes; the first `);
    const first = lines[header + 1] ?? "";
    expect(Buffer.byteLength(first)).toBeLessThanOrEqual(OUTPUT_LIMIT);
    expect(Buffer.byteLength(result.content)).toBeLessThan(OUTPUT_LIMIT + 4096);

    let text = first;
    let next = /"offset":(\d+)/.exec(lines[header] ?? "");
    while (next !== null) {
      const page = await tools.handle("get_run", { run_id: result.run?.id, offset: Number(next[1]) });
      expect(page.isError, page.content).toBe(false);
      expect(Buffer.byteLength(page.content)).toBeLessThan(OUTPUT_LIMIT + 4096);
      const [, body = "", more] = page.content.split("\n");
      text += body;
      next = more === undefined ? null : /"offset":(\d+)/.exec(more);
    }
    expect(text).toBe(full);
  }, 120_000);
});
