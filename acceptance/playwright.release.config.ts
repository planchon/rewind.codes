import { join } from "node:path";
import { defineConfig, devices } from "@playwright/test";

/**
 * The release gate (`release/README.md`). `release.sh` runs one project per invocation, so that a
 * failure in one stage never skips the next:
 *
 * - `criteria`: the 0.1 criteria that run without WorkOS and without cargo (`tests/`).
 * - `areas`: one spec per area, files in parallel, on setups A and B (started by the global
 *   setup), a tenant per file.
 * - `lifecycle`: setup D, alone: kills, restarts, an outage, retention.
 * - `sweeps`: what every earlier stage left: no unexpected 5xx, no panic, no leaked canary.
 */
const results = process.env.REWIND_GATE_RESULTS ?? join(import.meta.dirname, "results", "release");
const project = process.env.REWIND_GATE_PROJECT ?? "areas";

export default defineConfig({
  globalSetup: "./release/global-setup.ts",
  outputDir: join(results, "artifacts", project),
  fullyParallel: false,
  workers: Number(process.env.REWIND_GATE_WORKERS ?? 3),
  retries: 0,
  forbidOnly: true,
  timeout: 10 * 60_000,
  expect: { timeout: 30_000 },
  reporter: [
    ["list"],
    ["json", { outputFile: join(results, `playwright-${project}.json`) }],
    ["html", { outputFolder: join(results, `html-${project}`), open: "never" }],
  ],
  use: {
    ...devices["Desktop Chrome"],
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    {
      name: "criteria",
      testDir: "tests",
      testMatch: [
        "c1-sdk-workers-postgres.spec.ts",
        "c2-local-endings.spec.ts",
        "c3-c5-mcp-test-fork.spec.ts",
        "c7-dashboard.spec.ts",
      ],
    },
    { name: "areas", testDir: "release/areas" },
    { name: "lifecycle", testDir: "release/lifecycle" },
    { name: "sweeps", testDir: "release/sweeps" },
  ],
});
