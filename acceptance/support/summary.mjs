// Prints the acceptance status of each criterion, from Playwright's JSON report and the steps
// `run.sh` ran itself (`results/steps.jsonl`), and writes it to `results/summary.md`.
//
//   node acceptance/support/summary.mjs <results dir> <criterion…>
//
// Exits with 1 when a selected criterion did not pass.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [results, ...selected] = process.argv.slice(2);
const CRITERIA = {
  1: "A harness drives runs through the SDK, with several workers on one Postgres; one trace per run under the harness's span; runs searched by label and tool",
  2: "Locally, run, signal, replay and trace across processes; a forgotten await, an unhandled rejection and a deadlock end as specified",
  3: "A script calls a real MCP server, with types checked beforehand",
  4: "The evals report, per model and prompt rendering, first-try check pass rate, task success, iterations and tokens",
  5: "ptc test replays a recorded real run as a scenario; ptc fork continues a failed run without repeating its calls",
  6: "Load tests and limit tests pass",
  7: "An operator finds any run by label, tool or error in the dashboard, and sees its code next to its trace",
};

/** Every check: its criterion, its name and whether it passed. */
const checks = [];

const reportPath = join(results, "report.json");
if (existsSync(reportPath)) {
  const report = JSON.parse(readFileSync(reportPath, "utf8"));
  const walk = (suite, titles) => {
    for (const spec of suite.specs ?? []) {
      const path = [...titles, spec.title];
      const found = [...path].reverse().map((t) => /criterion (\d)/.exec(t)).find(Boolean);
      if (!found) continue;
      const statuses = spec.tests.map((t) => t.status);
      const status = statuses.every((s) => s === "expected")
        ? "pass"
        : statuses.every((s) => s === "skipped")
          ? "skipped"
          : "fail";
      checks.push({ criterion: found[1], name: `${suite.file ?? ""} › ${spec.title}`.replace(/^ › /, ""), status });
    }
    for (const child of suite.suites ?? []) walk({ ...child, file: child.file ?? suite.file }, [...titles, child.title]);
  };
  for (const suite of report.suites ?? []) walk(suite, [suite.title]);
}

const stepsPath = join(results, "steps.jsonl");
if (existsSync(stepsPath)) {
  for (const line of readFileSync(stepsPath, "utf8").split("\n")) {
    if (line.trim() !== "") checks.push(JSON.parse(line));
  }
}

let ok = true;
const lines = ["# Acceptance of ai-ptc 0.1", "", "| Criterion | Status | Checks |", "| --- | --- | --- |"];
for (const criterion of selected) {
  const mine = checks.filter((c) => c.criterion === criterion);
  const status =
    mine.length === 0
      ? "NOT RUN"
      : mine.some((c) => c.status === "fail")
        ? "FAIL"
        : mine.every((c) => c.status === "pass")
          ? "PASS"
          : "INCOMPLETE";
  if (status !== "PASS") ok = false;
  const detail = mine.map((c) => `${c.status === "pass" ? "✓" : c.status === "fail" ? "✗" : "–"} ${c.name}`).join("<br>");
  lines.push(`| ${criterion}. ${CRITERIA[criterion]} | ${status} | ${detail || "nothing ran"} |`);
}
const text = `${lines.join("\n")}\n`;
writeFileSync(join(results, "summary.md"), text);
process.stdout.write(`\n${text}`);
process.exit(ok ? 0 : 1);
