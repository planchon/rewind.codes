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
import { chromium, type Browser, type Locator, type Page } from "playwright";
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

async function settle(page: Page, ms = 900): Promise<void> {
  await page.waitForLoadState("networkidle").catch(() => {});
  await page.waitForTimeout(ms);
}

async function go(page: Page, route: string): Promise<void> {
  await page.goto(new URL(route, BASE).toString());
  await settle(page);
}

/** Opens a run of the triage agent waiting for its approval, from Observability's waiting view. */
async function openWaitingRun(page: Page, section = ""): Promise<void> {
  await go(page, "/observability?q=%40agent%3Atriage%20waiting%3Asignal");
  await page.locator("a[href^='/runs/']").first().click();
  await settle(page, 1500);
  if (section !== "") {
    const url = new URL(page.url());
    url.pathname = url.pathname.replace(/^(\/runs\/[^/]+)(\/.*)?$/, `$1/${section}`);
    await page.goto(url.toString());
    await settle(page, 1200);
  }
}

const SHOTS: Shot[] = [
  { name: "overview", open: (page) => go(page, "/overview") },
  { name: "observability", open: (page) => go(page, "/observability") },
  {
    name: "observability-query",
    async open(page) {
      await go(page, "/observability?q=%40agent%3Atriage%20status%3Afailed");
    },
  },
  { name: "observability-waiting", open: (page) => go(page, "/observability/waiting") },
  {
    name: "observability-suggestions",
    async open(page) {
      await go(page, "/observability");
      const query = page.getByRole("combobox", { name: "Query" }).or(page.getByLabel("Query")).first();
      await query.fill("@agent:");
      await settle(page, 600);
    },
  },
  { name: "observability-failures", open: (page) => go(page, "/observability/failures?window=7d") },
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
  {
    name: "run-started",
    async open(page) {
      await go(page, "/observability?q=%22rewind%3Aworkflows%22");
      await page.locator("a[href^='/runs/']").first().click();
      await settle(page, 1500);
      const url = new URL(page.url());
      url.pathname = url.pathname.replace(/^(\/runs\/[^/]+)(\/.*)?$/, "$1/started");
      await page.goto(url.toString());
      await settle(page, 1200);
    },
  },
  { name: "run-journal", open: (page) => openWaitingRun(page, "journal") },
  { name: "run-trace", open: (page) => openWaitingRun(page, "trace") },
  { name: "scripts", open: (page) => go(page, "/scripts") },
  { name: "tools", open: (page) => go(page, "/tools") },
  { name: "namespaces", open: (page) => go(page, "/namespaces"), height: 1000 },
  { name: "namespace", open: (page) => go(page, "/namespaces/bob") },
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
