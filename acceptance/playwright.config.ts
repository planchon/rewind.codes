import { defineConfig, devices } from "@playwright/test";

/**
 * The acceptance suite of 0.1 (`archi/09-roadmap.md`, "0.1 is done when"). One file per
 * criterion, run one at a time: each starts its own servers and containers.
 */
export default defineConfig({
  testDir: "tests",
  outputDir: "results/artifacts",
  fullyParallel: false,
  workers: 1,
  retries: 0,
  forbidOnly: true,
  timeout: 10 * 60_000,
  expect: { timeout: 30_000 },
  reporter: [["list"], ["json", { outputFile: "results/report.json" }], ["html", { outputFolder: "results/html", open: "never" }]],
  use: {
    ...devices["Desktop Chrome"],
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
});
