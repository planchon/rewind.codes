/**
 * Criterion 4 (`archi/09-roadmap.md`, "0.1 is done when"): the evals report, per model and
 * prompt rendering, the first-try check pass rate, task success, iterations and tokens.
 *
 * Real models need API keys that are not available to this suite: the report is proven with
 * two scripted models, which replay turns from a file through the same harness (agent tools,
 * engine, fake MCP servers, checker). One gets the code right the first time; the other
 * submits code that does not type-check, then fixes it. Run with real models, the same
 * command takes `--model claude-…` or `--model gpt-…` and their keys.
 */
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { expect, test } from "@playwright/test";
import { finish } from "../support/project.js";
import { resultDir, root } from "../support/paths.js";

const SOLUTION = `import { chat } from "rewind:actions";

export default async function main() {
  await chat.post_message({ channel: "#ops", text: "Deploy freeze starts Friday 18:00 UTC" });
  return "posted";
}
`;

/** A number where the action takes a string: refused by the check. */
const WRONG = SOLUTION.replace('text: "Deploy freeze starts Friday 18:00 UTC"', "text: 18");

/** A scripted model's turn that runs `code` with the agent tool `run_workflow`. */
const runCode = (code: string) => ({ calls: [{ name: "run_workflow", input: { code } }] });

interface Summary {
  model: string;
  rendering: string;
  episodes: number;
  errors: number;
  first_check_pass_rate: number | null;
  success_rate: number | null;
  mean_iterations: number | null;
  mean_tokens: number | null;
  usage: Record<string, number>;
}

test("criterion 4: the evals report per model and prompt rendering (scripted models)", async () => {
  const binary = process.env.REWIND_EVALS_BIN ?? join(root, "target", "debug", "evals");
  expect(existsSync(binary), `no evals binary at ${binary}: run \`cargo build -p evals\``).toBe(true);
  const dir = mkdtempSync(join(resultDir("c4-evals"), "run-"));
  const careful = join(dir, "careful.json");
  const hasty = join(dir, "hasty.json");
  writeFileSync(careful, JSON.stringify([runCode(SOLUTION), { text: "Posted to #ops." }]));
  writeFileSync(hasty, JSON.stringify([runCode(WRONG), runCode(SOLUTION), { text: "Posted to #ops." }]));
  const out = join(dir, "out");

  const ran = await finish(
    spawn(
      binary,
      [
        "run",
        "--model",
        `scripted:${careful}`,
        "--model",
        `scripted:${hasty}`,
        "--rendering",
        "all",
        "--task",
        "post_announcement",
        "--out",
        out,
      ],
      // The sandbox runs in UTC, as `cargo run` sets it.
      { cwd: root, env: { ...process.env, TZ: "UTC" }, stdio: ["ignore", "pipe", "pipe"] },
    ),
    600_000,
  );
  expect(ran.code, ran.stdout + ran.stderr).toBe(0);

  const report = JSON.parse(readFileSync(join(out, "report.json"), "utf8")) as {
    renderings: string[];
    summaries: Summary[];
    episodes: unknown[];
  };
  expect(report.renderings.sort()).toEqual(["compact", "dts", "search"]);
  expect(report.episodes).toHaveLength(6);
  // One summary per model and rendering.
  expect(report.summaries).toHaveLength(6);
  for (const rendering of ["compact", "dts", "search"]) {
    const of = (model: string) => report.summaries.find((s) => s.rendering === rendering && s.model === model)!;
    const right = of("scripted:careful");
    const fixed = of("scripted:hasty");
    for (const summary of [right, fixed]) {
      expect(summary, rendering).toBeDefined();
      expect(summary.episodes).toBe(1);
      expect(summary.errors).toBe(0);
      expect(summary.success_rate, JSON.stringify(summary)).toBe(1);
      // A scripted model reports no tokens: the measure is there, at zero.
      expect(summary.mean_tokens).toBe(0);
      expect(summary.usage).toBeDefined();
    }
    expect(right.first_check_pass_rate).toBe(1);
    expect(fixed.first_check_pass_rate).toBe(0);
    expect(right.mean_iterations).toBe(2);
    expect(fixed.mean_iterations).toBe(3);
  }

  const summary = readFileSync(join(out, "summary.md"), "utf8");
  for (const measure of [/first-try/i, /success/i, /turns|iterations/i, /tokens/i]) {
    expect(summary).toMatch(measure);
  }
  expect(existsSync(join(out, "transcripts"))).toBe(true);
});
