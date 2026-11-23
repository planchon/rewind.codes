/**
 * Criterion 7 (`archi/09-roadmap.md`, "0.1 is done when"): an operator finds any run by label,
 * tool or error in the dashboard, and sees its code next to its trace.
 *
 * The dashboard is the one `rewind-server` embeds and serves. Runs are made through the SDK, with
 * its worker answering the host tools; the operator signs in with a `reader` key, searches with
 * the query bar, steps through a run with the time cursor, and sees a run waiting for a signal
 * and the failures grouped by signature.
 */
import { mkdtempSync } from "node:fs";
import { join } from "node:path";
import { HostToolError, Rewind, type Run } from "@rewind.codes/sdk";
import { expect, type Page, test } from "@playwright/test";
import { startServer } from "../support/deployment.js";
import { resultDir } from "../support/paths.js";

const TRIAGE = `import { sleep } from "rewind";
import { audit, crm } from "rewind:tools";

export default async function main(input: { email: string }) {
  const customer = await crm.lookup({ email: input.email });
  await sleep("100ms");
  await audit.record({ event: \`triaged \${customer.id}\` });
  console.log("triaged", customer.id);
  return { customer: customer.id };
}
`;

const CHARGE = `import { payments } from "rewind:tools";

export default async function main() {
  const receipt = await payments.charge({ amount: 100 });
  return receipt;
}
`;

const STUCK = `export default async function main() {
  await new Promise(() => {});
}
`;

const WAITER = `import { signal } from "rewind";

export default async function main() {
  const go = await signal<{ go: boolean }>("go-ahead");
  return { go: go.go };
}
`;

test.describe("criterion 7: the dashboard", () => {
  let server: Awaited<ReturnType<typeof startServer>>;
  let readerKey: string;
  const runs: Record<"triage" | "triageOther" | "charge" | "stuck", Run> = {} as never;
  /** A run left waiting for the signal "go-ahead". */
  let waiter: Run;

  test.beforeAll(async () => {
    server = await startServer(mkdtempSync(join(resultDir("c7-dashboard"), "server-")));
    const { admin } = server;
    readerKey = (await admin.apiKeys.create({ tenant: "acme", role: "reader", name: "operator" })).key!;
    const harness = new Rewind({
      url: server.url,
      apiKey: (await admin.apiKeys.create({ tenant: "acme", role: "harness", name: "harness" })).key!,
    }).namespace("alice");
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
      waiter = await start(WAITER, { team: "ops", conversation: "conv-11" });
      for (const run of Object.values(runs)) {
        const ended = await harness.runs.get(run.id, { wait: "60s" });
        expect(["completed", "failed"]).toContain(ended.status);
      }
      await expect
        .poll(async () => (await harness.runs.get(waiter.id)).waitingFor?.map((w) => w.kind))
        .toEqual(["signal"]);
      // The read model has every run.
      await expect
        .poll(async () => (await harness.runs.list({ labels: { conversation: "conv-9" }, error: "DeadlockError" })).runs.length)
        .toBe(1);
      await expect.poll(async () => (await harness.runs.list({ tool: "payments.charge" })).runs.length).toBe(1);
      // The read model knows what the waiter waits for.
      await expect
        .poll(async () => {
          const response = await fetch(`${server.url}/v1/runs?q=${encodeURIComponent("waiting:signal signal:go-ahead")}`, {
            headers: { Authorization: `Bearer ${readerKey}` },
          });
          return ((await response.json()) as { runs: { id: string }[] }).runs.map((r) => r.id);
        })
        .toEqual([waiter.id]);
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

  /** Types a query in the bar and applies it. */
  async function search(page: Page, q: string) {
    const bar = page.getByRole("combobox", { name: "Query" });
    await bar.fill(q);
    await bar.press("Enter");
  }

  test("an operator finds runs by label, tool and error, and sees a run's code next to its trace", async ({ page }) => {
    await page.goto(`${server.url}/`);
    await page.getByPlaceholder("rewind_sk_…").fill(readerKey);
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page).toHaveURL(/^[^#]*\/overview$/);
    await expect(page.getByRole("link", { name: /Waiting now\s*1/ })).toBeVisible();
    await page.getByRole("navigation", { name: "Views" }).getByRole("link", { name: "Observability" }).click();
    await expect(page).toHaveURL(/^[^#]*\/observability/);
    await expect.poll(shown(page)).toEqual([...Object.values(runs), waiter].map((r) => r.id).sort());

    // By label.
    await search(page, "@conversation:conv-7");
    await expect.poll(shown(page)).toEqual([runs.triage.id]);
    await search(page, "@team:support");
    await expect.poll(shown(page)).toEqual([runs.triageOther.id, runs.stuck.id].sort());

    // By tool.
    await search(page, "tool:payments.charge");
    await expect.poll(shown(page)).toEqual([runs.charge.id]);
    await search(page, "tool:crm.lookup");
    await expect.poll(shown(page)).toEqual([runs.triage.id, runs.triageOther.id].sort());

    // By error: the kind of a failed tool call, or the name of the error a run failed with.
    await search(page, "error:error");
    await expect.poll(shown(page)).toEqual([runs.charge.id]);
    await search(page, "error:DeadlockError");
    await expect.poll(shown(page)).toEqual([runs.stuck.id]);
    // The query lives in the URL.
    expect(page.url()).toContain("DeadlockError");

    // A filter's value, in Observability's sidebar: a click asks for it.
    await search(page, "");
    const status = page.getByRole("region", { name: "Filters" }).getByRole("group", { name: "Status" });
    await status.getByRole("button", { name: /^Status failed, 2 runs$/ }).click();
    await expect.poll(shown(page)).toEqual([runs.charge.id, runs.stuck.id].sort());

    // A term the server cannot read is underlined, with its message.
    await search(page, "@team:billing stauts:failed");
    await expect(page.getByText("The query cannot be read:")).toBeVisible();
    await expect(page.locator(".qbar-mirror .q-bad")).toHaveText("stauts:failed");

    // A label and a tool together, then the run opened from the table.
    await search(page, "@team:billing tool:crm.lookup");
    await expect.poll(shown(page)).toEqual([runs.triage.id]);
    await page.locator("table.grid tbody tr a.row-link").first().click();
    await expect(page).toHaveURL(new RegExp(`^[^#]*/runs/${runs.triage.id}`));

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

    // The time cursor: the run at its first step, then after crm.lookup returned.
    const cursor = page.getByRole("region", { name: "Time cursor" });
    await expect(cursor).toContainText("Step 4 of 4");
    await page.keyboard.press("Home");
    await expect(page).toHaveURL(/[?&]step=0(&|$)/);
    await expect(cursor).toContainText("Step 1 of 4");
    const mark = (n: number) => source.locator(".code-gutter .code-row").nth(n - 1).locator(".mk");
    await expect(mark(4)).toHaveText("▸");
    await expect(mark(5)).toHaveText("①");
    await expect(notes(7)).toBeEmpty();
    await page.keyboard.press("ArrowRight");
    await expect(page).toHaveURL(/[?&]step=2(&|$)/);
    await expect(mark(5)).toHaveText("▸");
    await expect(mark(6)).toHaveText("①");
    await expect(timeline.getByRole("slider", { name: /Time cursor/ })).toHaveAttribute("aria-valuenow", "2");
    // Hovering `customer`, bound to the awaited crm.lookup, shows what it returned.
    await line(7).locator(".hv-binding").first().hover();
    const value = page.getByRole("tooltip");
    await expect(value).toContainText("= the result of #1 crm.lookup");
    // The value is a JSON tree: each key and its value, with no space between them.
    await expect(value).toContainText('"id":"cus_ada"');
    // A click watches it; at the first step it is not assigned yet.
    await line(7).locator(".hv-binding").first().click();
    const watches = page.getByRole("list", { name: "Watches" });
    await expect(watches).toContainText("cus_ada");
    await page.mouse.move(0, 0);
    await page.keyboard.press("Home");
    await expect(watches).toContainText("not assigned yet");

    // Waiting: the run waiting for the signal, on its line.
    await page.goto(`${server.url}/observability/waiting`);
    const waiting = page.locator("table.grid tbody tr", { has: page.locator(`.run-id[title="${waiter.id}"]`) });
    await expect(waiting).toContainText('signal "go-ahead"');
    await expect(waiting).toContainText('const go = await signal<{ go: boolean }>("go-ahead");');
    await expect(page.getByRole("tab", { name: /Signals/ })).toContainText("1");

    // Failures: the deadlock is a group, which opens its runs.
    await page.goto(`${server.url}/observability/failures`);
    const group = page.locator("table.grid tbody tr", { hasText: "DeadlockError" }).first();
    await expect(group).toBeVisible();
    await expect(group).toContainText(runs.stuck.id.slice(0, 8));
    await group.locator("td").first().getByRole("link").click();
    await expect(page).toHaveURL(/^[^#]*\/observability\?q=fingerprint/);
    await expect.poll(shown(page)).toEqual([runs.stuck.id]);

    // A failed run: the call that failed, on its line, and the error. Opened from a link of an
    // earlier version, its route in the fragment: the app reads it as the path it meant.
    await page.goto(`${server.url}/#/runs/${runs.charge.id}`);
    await expect(page).toHaveURL(new RegExp(`^[^#]*/runs/${runs.charge.id}$`));
    const failed = page.getByRole("region", { name: "Source" });
    await expect(failed.locator(".code-text .code-row").nth(3)).toContainText("payments.charge({ amount: 100 })");
    await expect(failed.locator(".code-notes .code-row").nth(3)).toContainText("payments.charge");
    await expect(page.getByText("payments.charge failed: card declined").first()).toBeVisible();
    await expect(page.getByRole("region", { name: "Timeline" }).getByLabel(/^#1 payments\.charge: /)).toBeVisible();
  });
});
