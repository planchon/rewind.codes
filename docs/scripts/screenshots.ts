// Takes the dashboard's screenshots of the documentation, in light and dark, from the dashboard
// running against its mocked API:
//
//   pnpm --dir dashboard dev:mock --port 5199      # in one terminal
//   pnpm --dir docs screenshots                     # in another; DASHBOARD_URL to point elsewhere
//
// It signs in as Ada ("Continue with GitHub" in the mocked world), picks Acme, and writes
// images/dashboard/<name>-<theme>.webp, lossless (a third of the PNG's size). Restart the mocked
// dashboard before a full run: it keeps what earlier runs created (the API key of
// organization-api-key-created). The mocked world is built from the current time, so dates and
// durations move from one run to the next; the shapes do not.
//
//   pnpm --dir docs screenshots run-debugger find   # only these shots
import { mkdir } from "node:fs/promises";
import { chromium, type Browser, type Locator, type Page, type Request } from "playwright";
import sharp from "sharp";

const BASE = process.env.DASHBOARD_URL ?? "http://localhost:5199/";
const OUT = new URL("../images/dashboard/", import.meta.url);
const ONLY = process.argv.slice(2);

type Theme = "light" | "dark";

interface Shot {
  name: string;
  /** Brings the page to the state to capture. */
  open(page: Page): Promise<void>;
  /** The viewport's height for this shot. 900 by default. */
  height?: number;
  /** Captures this element only, instead of the viewport. */
  element?: (page: Page) => Locator;
  /** Changes the mocked world: taken after every other shot of both themes, so none shows it. */
  last?: boolean;
}

/**
 * The requests of each page still in flight, but its event streams (the change stream, a run's
 * events), which stay open as long as the page: Playwright's "networkidle" never comes with them.
 */
const inFlight = new WeakMap<Page, Set<Request>>();

function track(page: Page): void {
  const open = new Set<Request>();
  inFlight.set(page, open);
  page.on("request", (request) => {
    const stream =
      request.headers().accept === "text/event-stream" ||
      /\/v1\/changes|\/events\?follow=/.test(request.url());
    if (!stream) open.add(request);
  });
  page.on("requestfinished", (request) => open.delete(request));
  page.on("requestfailed", (request) => open.delete(request));
}

/** Waits for the page's reads to be done for half a second (15 s at most), then `ms`. */
async function settle(page: Page, ms = 900): Promise<void> {
  const open = inFlight.get(page);
  const deadline = Date.now() + 15_000;
  let quietSince = Date.now();
  while (open && Date.now() < deadline) {
    if (open.size > 0) quietSince = Date.now();
    else if (Date.now() - quietSince >= 500) break;
    await page.waitForTimeout(100);
  }
  await page.waitForTimeout(ms);
}

async function go(page: Page, route: string): Promise<void> {
  await page.goto(new URL(route, BASE).toString());
  await settle(page);
}

/**
 * Opens the first run a list of Runs shows for `query`, then one of its sections (`history`,
 * `logs`, `started`; the debugger without one). A run's page is `/runs/<handle>`, beside the
 * views `/runs/running`, `/runs/waiting` and `/runs/failures`, which its links never name.
 */
async function openRun(page: Page, query: string, section = ""): Promise<void> {
  await go(page, `/runs?q=${encodeURIComponent(query)}`);
  await page
    .locator(
      "main a[href^='/runs/']:not([href^='/runs/running']):not([href^='/runs/waiting']):not([href^='/runs/failures'])",
    )
    .first()
    .click();
  await settle(page, 1500);
  if (section !== "") {
    const url = new URL(page.url());
    url.pathname = `${url.pathname.replace(/\/+$/, "")}/${section}`;
    await page.goto(url.toString());
    await settle(page, 1200);
  }
}

/** Opens a run of the triage agent waiting for its approval. */
function openWaitingRun(page: Page, section = ""): Promise<void> {
  return openRun(page, "@agent:triage waiting:signal", section);
}

const SHOTS: Shot[] = [
  // Seven days: the calls of the mocked world that failed are older than a day.
  { name: "overview", open: (page) => go(page, "/overview?window=7d") },
  { name: "runs", open: (page) => go(page, "/runs") },
  {
    name: "runs-query",
    async open(page) {
      await go(page, `/runs?q=${encodeURIComponent("@agent:triage status:failed")}`);
      // The facets before @agent folded, so that the checked @agent shows under the checked status.
      for (const title of ["Namespace", "Workflow", "@env"]) {
        const summary = page
          .locator("details.filter-group[open] > summary")
          .filter({ has: page.locator(".filter-title", { hasText: new RegExp(`^${title}$`) }) });
        if ((await summary.count()) > 0) await summary.first().click();
      }
      await page.mouse.move(1100, 800);
      await settle(page, 300);
    },
  },
  { name: "runs-waiting", open: (page) => go(page, "/runs/waiting") },
  {
    name: "runs-suggestions",
    async open(page) {
      await go(page, "/runs");
      const query = page.getByRole("combobox", { name: "Query" }).or(page.getByLabel("Query")).first();
      await query.fill("@agent:");
      await settle(page, 600);
    },
  },
  { name: "runs-failures", open: (page) => go(page, "/runs/failures?window=7d") },
  { name: "run-debugger", open: (page) => openWaitingRun(page), height: 1000 },
  {
    name: "run-time-cursor",
    async open(page) {
      await openWaitingRun(page);
      const issues = page.locator('.code-text [data-line="5"] .hv', { hasText: "issues" }).first();
      await issues.click();
      await issues.hover();
      await settle(page, 600);
    },
    height: 1000,
  },
  // The runs a run started: the run that started a detached run of digest from rewind:workflows.
  {
    name: "run-started",
    open: (page) => openRun(page, "action:workflows.digest.start", "started"),
  },
  { name: "run-history", open: (page) => openWaitingRun(page, "history") },
  { name: "workflows", open: (page) => go(page, "/workflows") },
  {
    name: "workflow",
    // The organization's triage, at its latest version: its code, interface and suite.
    async open(page) {
      await go(page, "/workflows");
      await page
        .locator("main tr")
        .filter({ has: page.locator(".wf-name", { hasText: /^triage$/ }) })
        .filter({ hasNot: page.locator(".tag", { hasText: "fork" }) })
        .first()
        .locator("a.wf-name")
        .click();
      await settle(page, 1500);
    },
    height: 1000,
  },
  { name: "actions", open: (page) => go(page, "/actions") },
  { name: "namespaces", open: (page) => go(page, "/namespaces"), height: 1000 },
  // One end user's scope, at its Usage.
  { name: "namespace", open: (page) => go(page, "/ns/bob/usage") },
  { name: "organization-members", open: (page) => go(page, "/organization/members") },
  { name: "organization-roles", open: (page) => go(page, "/organization/roles") },
  { name: "organization-api-keys", open: (page) => go(page, "/organization/api-keys") },
  {
    name: "organization-api-key-created",
    async open(page) {
      await go(page, "/organization/api-keys");
      await page.getByRole("button", { name: "New key" }).click();
      await page.getByLabel("Name").fill("support-harness");
      await page.getByLabel("Role").selectOption("harness");
      await page.getByRole("button", { name: "Create the key" }).click();
      await settle(page, 800);
    },
    // The panel alone: each theme creates a key, which the list below it would show.
    element: (page) => page.locator('section[aria-label="The new key"]'),
    last: true,
  },
  { name: "connect", open: (page) => go(page, "/connect"), height: 1500 },
  {
    name: "consent",
    // `pnpm dev:mock` keeps a request of claude.ai waiting, at a fixed address.
    open: (page) => go(page, "/connect/authorize?request=claude-demo"),
    // The card alone: the page around it is empty.
    element: (page) => page.locator(".gate-card"),
  },
  { name: "account-connections", open: (page) => go(page, "/account/connections") },
  { name: "organization-connections", open: (page) => go(page, "/organization/connections") },
  {
    name: "find",
    async open(page) {
      await go(page, "/overview");
      await page.keyboard.press("f");
      await settle(page, 600);
    },
  },
];

async function signIn(page: Page): Promise<void> {
  await page.getByRole("button", { name: /Continue with GitHub/ }).click();
  await settle(page);
  await page.getByRole("button", { name: /Acme/ }).click();
  await settle(page);
}

function wanted(name: string): boolean {
  return ONLY.length === 0 || ONLY.includes(name);
}

/** Writes a screenshot of the page, or of one element of it, as lossless WebP. */
async function save(target: Page | Locator, name: string, theme: Theme): Promise<void> {
  const png = await target.screenshot();
  await sharp(png).webp({ lossless: true }).toFile(new URL(`${name}-${theme}.webp`, OUT).pathname);
  console.log(`wrote images/dashboard/${name}-${theme}.webp`);
}

async function capture(browser: Browser, theme: Theme, last: boolean): Promise<void> {
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    deviceScaleFactor: 2,
    colorScheme: theme,
    timezoneId: "UTC",
    locale: "en-US",
  });
  const page = await context.newPage();
  track(page);
  // The mocked world keeps one browser's session: start signed out.
  await page.request.post(new URL("auth/logout", BASE).toString(), { data: {} });
  await page.goto(BASE);
  await settle(page);
  // The sign-in card alone: the page around it is empty.
  if (!last && wanted("sign-in")) await save(page.locator(".gate-card"), "sign-in", theme);
  await signIn(page);
  for (const shot of SHOTS) {
    if (!wanted(shot.name) || (shot.last ?? false) !== last) continue;
    await page.setViewportSize({ width: 1440, height: shot.height ?? 900 });
    await shot.open(page);
    await save(shot.element?.(page) ?? page, shot.name, theme);
  }
  await context.close();
}

await mkdir(OUT, { recursive: true });
const browser = await chromium.launch();
try {
  for (const last of [false, true]) {
    for (const theme of ["light", "dark"] as const) await capture(browser, theme, last);
  }
} finally {
  await browser.close();
}
