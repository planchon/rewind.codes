/**
 * Criterion 7 (`archi/09-roadmap.md`, "0.1 is done when"): an operator finds any run by label,
 * tool or error in the dashboard, and sees its code next to its trace.
 *
 * The dashboard is the one `ptc-server` embeds and serves. Runs are made through the SDK, with
 * its worker answering the host tools; the operator signs in with a `reader` key.
 */
import { mkdtempSync } from "node:fs";
import { join } from "node:path";
import { HostToolError, Ptc, type Run } from "@ai-ptc/sdk";
import { expect, type Page, test } from "@playwright/test";
import { startServer } from "../support/deployment.js";
import { resultDir } from "../support/paths.js";

const TRIAGE = `import { sleep } from "ptc";
import { audit, crm } from "ptc:tools";

export default async function main(input: { email: string }) {
  const customer = await crm.lookup({ email: input.email });
  await sleep("100ms");
  await audit.record({ event: \`triaged \${customer.id}\` });
  console.log("triaged", customer.id);
  return { customer: customer.id };
}
`;

const CHARGE = `import { payments } from "ptc:tools";

export default async function main() {
  const receipt = await payments.charge({ amount: 100 });
  return receipt;
}
`;

const STUCK = `export default async function main() {
  await new Promise(() => {});
}
`;

test.describe("criterion 7: the dashboard", () => {
  let server: Awaited<ReturnType<typeof startServer>>;
  let readerKey: string;
  const runs: Record<"triage" | "triageOther" | "charge" | "stuck", Run> = {} as never;

  test.beforeAll(async () => {
    server = await startServer(mkdtempSync(join(resultDir("c7-dashboard"), "server-")));
    const { admin } = server;
    readerKey = (await admin.apiKeys.create({ tenant: "acme", role: "reader", name: "operator" })).key!;
    const harness = new Ptc({
      url: server.url,
      apiKey: (await admin.apiKeys.create({ tenant: "acme", role: "harness" })).key!,
    });
    const toolset = await harness.toolsets.create({
      host: [
        {
          name: "crm.lookup",
          description: "Find a customer by email.",
          inputSchema: { type: "object", properties: { email: { type: "string" } }, required: ["email"] },
          outputSchema: { type: "object", properties: { id: { type: "string" } }, required: ["id"] },
        },
        {
          name: "audit.record",
          description: "Record an event.",
          inputSchema: { type: "object", properties: { event: { type: "string" } }, required: ["event"] },
        },
        {
          name: "payments.charge",
          description: "Charge a card.",
          inputSchema: { type: "object", properties: { amount: { type: "number" } }, required: ["amount"] },
        },
      ],
    });
    const worker = harness.worker({
      toolset: toolset.id,
      pollWait: "1s",
      tools: {
        "crm.lookup": ({ email }: { email: string }) => ({ id: `cus_${email.split("@")[0]}` }),
        "audit.record": () => ({ ok: true }),
        "payments.charge": () => {
          throw new HostToolError("card declined", { data: { code: "card_declined" } });
        },
      },
      onError: () => undefined,
    });
    try {
      const start = (script: string, labels: Record<string, string>, input: unknown = {}) =>
        harness.runs.start({ toolset: toolset.id, script, labels, input });
      runs.triage = await start(TRIAGE, { team: "billing", conversation: "conv-7" }, { email: "ada@example.com" });
      runs.triageOther = await start(TRIAGE, { team: "support", conversation: "conv-10" }, { email: "bob@example.com" });
      runs.charge = await start(CHARGE, { team: "billing", conversation: "conv-8" });
      runs.stuck = await start(STUCK, { team: "support", conversation: "conv-9" });
      for (const run of Object.values(runs)) {
        const ended = await harness.runs.get(run.id, { wait: "60s" });
        expect(["completed", "failed"]).toContain(ended.status);
      }
      // The read model has every run.
      await expect
        .poll(async () => (await harness.runs.list({ labels: { conversation: "conv-9" }, error: "DeadlockError" })).runs.length)
        .toBe(1);
      await expect.poll(async () => (await harness.runs.list({ tool: "payments.charge" })).runs.length).toBe(1);
    } finally {
      await worker.stop();
    }
  });

  test.afterAll(async () => {
    await server?.stop();
  });

  /** The ids of the runs the table shows. */
  const shown = (page: Page) => async () =>
    (await page.locator("table.grid tbody tr .run-id").evaluateAll((spans) => spans.map((s) => s.getAttribute("title")))).sort();

  async function filter(page: Page, field: string, value: string) {
    await page.getByLabel("Filter on").selectOption(field);
    const input = page.getByLabel(`Filter value for ${field}`);
    await input.fill(value);
    await input.press("Enter");
  }

  async function unfilter(page: Page, field: string, value: string) {
    await page.getByRole("button", { name: `Remove the ${field} filter ${value}` }).click();
  }

  test("an operator finds runs by label, tool and error, and sees a run's code next to its trace", async ({ page }) => {
    await page.goto(`${server.url}/`);
    await page.getByPlaceholder("ptc_sk_…").fill(readerKey);
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page).toHaveURL(/#\/runs/);
    await expect.poll(shown(page)).toEqual(Object.values(runs).map((r) => r.id).sort());

    // By label.
    await filter(page, "label", "conversation:conv-7");
    await expect.poll(shown(page)).toEqual([runs.triage.id]);
    await unfilter(page, "label", "conversation:conv-7");
    await filter(page, "label", "team:support");
    await expect.poll(shown(page)).toEqual([runs.triageOther.id, runs.stuck.id].sort());
    await unfilter(page, "label", "team:support");

    // By tool.
    await filter(page, "tool", "payments.charge");
    await expect.poll(shown(page)).toEqual([runs.charge.id]);
    await unfilter(page, "tool", "payments.charge");
    await filter(page, "tool", "crm.lookup");
    await expect.poll(shown(page)).toEqual([runs.triage.id, runs.triageOther.id].sort());
    await unfilter(page, "tool", "crm.lookup");

    // By error: the kind of a failed tool call, or the name of the error a run failed with.
    await filter(page, "error", "error");
    await expect.poll(shown(page)).toEqual([runs.charge.id]);
    await unfilter(page, "error", "error");
    await filter(page, "error", "DeadlockError");
    await expect.poll(shown(page)).toEqual([runs.stuck.id]);
    // The filters live in the URL.
    expect(page.url()).toContain("DeadlockError");
    await unfilter(page, "error", "DeadlockError");

    // A label and a tool together, then the run opened from the table.
    await filter(page, "label", "team:billing");
    await filter(page, "tool", "crm.lookup");
    await expect.poll(shown(page)).toEqual([runs.triage.id]);
    await page.locator("table.grid tbody tr a.row-link").first().click();
    await expect(page).toHaveURL(new RegExp(`#/runs/${runs.triage.id}`));

    // Its source, each line with what it caused.
    const source = page.getByRole("region", { name: "Source" });
    await expect(source).toContainText("const customer = await crm.lookup({ email: input.email });");
    const line = (n: number) => source.locator(".code-text .code-row").nth(n - 1);
    const notes = (n: number) => source.locator(".code-notes .code-row").nth(n - 1);
    await expect(line(5)).toContainText("crm.lookup");
    await expect(notes(5)).toContainText("crm.lookup");
    await expect(line(6)).toContainText("sleep");
    await expect(notes(6)).toContainText("100ms");
    await expect(line(7)).toContainText("audit.record");
    await expect(notes(7)).toContainText("audit.record");
    await expect(notes(8)).toContainText("triaged");
    await expect(notes(4)).toBeEmpty();

    // Its timeline: each call, the timer and the log, with their outcome.
    const timeline = page.getByRole("region", { name: "Timeline" });
    await expect(timeline.getByLabel(/^#1 crm\.lookup: /)).toBeVisible();
    await expect(timeline.getByLabel(/^#2 sleep /)).toBeVisible();
    await expect(timeline.getByLabel(/^#3 audit\.record: /)).toBeVisible();
    // `console.log` is recorded at the level `info`.
    await expect(timeline.getByLabel(/^console\.info: triaged cus_ada/)).toBeVisible();

    // A failed run: the call that failed, on its line, and the error.
    await page.goto(`${server.url}/#/runs/${runs.charge.id}`);
    const failed = page.getByRole("region", { name: "Source" });
    await expect(failed.locator(".code-text .code-row").nth(3)).toContainText("payments.charge({ amount: 100 })");
    await expect(failed.locator(".code-notes .code-row").nth(3)).toContainText("payments.charge");
    await expect(page.getByText("payments.charge failed: card declined").first()).toBeVisible();
    await expect(page.getByRole("region", { name: "Timeline" }).getByLabel(/^#1 payments\.charge: /)).toBeVisible();
  });
});
