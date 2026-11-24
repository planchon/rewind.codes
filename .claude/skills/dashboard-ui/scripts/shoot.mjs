#!/usr/bin/env node
// Screenshots of the dashboard for a before/after review (SKILL.md, "Review workflow").
//
//   node shoot.mjs <shots.json> <out-dir>
//
// shots.json:
//   {
//     "base": "http://localhost:5391",   the mock UI (default)
//     "widths": [1280],                  viewport widths (default [1280]); height 800
//     "themes": ["light", "dark"],       color schemes (default both)
//     "scale": 2,                        device scale factor (default 2)
//     "shots": [
//       {
//         "name": "observability-stuck", file name: <name>-<theme>-<width>.png
//         "path": "/observability",
//         "widths": [1100], "themes": ["light"],    override the defaults
//         "steps": [                     run in order once the page is loaded
//           { "click": "<selector>" },   the first match
//           { "hover": "<selector>" },
//           { "type": "acme" },        typed where the focus is
//           { "scroll": 600 },           a wheel over the page's content, in px
//           { "scrollX": { "selector": ".table-wrap", "to": "end" } },   or a number of px
//           { "wait": 500 }
//         ],
//         "clip": "<selector>",          the first match and 16 px around it, not the viewport
//         "full": true,                  the whole page, not the viewport
//         "corners": "<selector>"        also <name>-<theme>-<width>-corners.png: the element's
//                                        top corners at 4×, side by side
//       }
//     ]
//   }
//
// A page error or a failed request is printed and written to <out-dir>/errors.txt, and the
// script exits with 1: a review with errors is not done. Playwright comes from `acceptance/`
// (`pnpm --dir acceptance install`; once, `pnpm --dir acceptance exec playwright install
// chromium`), or from wherever Node finds `playwright-core`.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../../..");

function chromium() {
  const require = createRequire(import.meta.url);
  const paths = [process.cwd(), join(repo, "acceptance"), join(repo, "dashboard")];
  for (const name of ["playwright-core", "playwright", "@playwright/test"]) {
    try {
      return require(require.resolve(name, { paths })).chromium;
    } catch {}
  }
  console.error(
    "Playwright is missing: pnpm --dir acceptance install, then once: pnpm --dir acceptance exec playwright install chromium",
  );
  process.exit(2);
}

const [listPath, outDir] = process.argv.slice(2);
if (!listPath || !outDir) {
  console.error("usage: node shoot.mjs <shots.json> <out-dir>");
  process.exit(2);
}
const list = JSON.parse(readFileSync(listPath, "utf8"));
const base = list.base ?? "http://localhost:5391";
mkdirSync(outDir, { recursive: true });

const errors = [];

async function open(browser, shot, theme, width, scale) {
  const context = await browser.newContext({
    viewport: { width, height: 800 },
    colorScheme: theme,
    deviceScaleFactor: scale,
  });
  const page = await context.newPage();
  const where = `${shot.name} ${theme} ${width}`;
  page.on("pageerror", (e) => errors.push(`${where}: page error: ${e.message}`));
  page.on("response", (r) => {
    if (r.status() >= 500) errors.push(`${where}: ${r.status()} ${r.url()}`);
  });
  await page.goto(base + shot.path, { waitUntil: "load" });
  await page.waitForTimeout(1500);
  await page.mouse.move(Math.round(width * 0.6), 400);
  for (const step of shot.steps ?? []) {
    if (step.click) await page.locator(step.click).first().click();
    if (step.hover) await page.locator(step.hover).first().hover();
    if (step.type) await page.keyboard.type(step.type);
    if (step.scroll) await page.mouse.wheel(0, step.scroll);
    if (step.scrollX) {
      const { selector, to } = step.scrollX;
      await page.locator(selector).first().evaluate((el, to) => {
        el.scrollLeft = to === "end" ? el.scrollWidth : Number(to);
      }, to);
    }
    if (step.wait) await page.waitForTimeout(step.wait);
    await page.waitForTimeout(400);
  }
  return { context, page };
}

const browser = await chromium().launch();
const written = [];
for (const shot of list.shots) {
  for (const theme of shot.themes ?? list.themes ?? ["light", "dark"]) {
    for (const width of shot.widths ?? list.widths ?? [1280]) {
      const file = join(outDir, `${shot.name}-${theme}-${width}.png`);
      const { context, page } = await open(browser, shot, theme, width, list.scale ?? 2);
      if (shot.clip) {
        const box = await page.locator(shot.clip).first().boundingBox();
        if (!box) throw new Error(`${shot.name}: nothing matches ${shot.clip}`);
        const x = Math.max(0, box.x - 16);
        const y = Math.max(0, box.y - 16);
        await page.screenshot({
          path: file,
          clip: { x, y, width: box.width + 32, height: box.height + 32 },
        });
      } else {
        await page.screenshot({ path: file, fullPage: Boolean(shot.full) });
      }
      written.push(file);
      await context.close();

      if (shot.corners) {
        const zoom = await open(browser, shot, theme, width, 4);
        // What shows of the element: cut by the boxes that clip or scroll it (a table wider than
        // its page) and by the viewport.
        const box = await zoom.page
          .locator(shot.corners)
          .first()
          .evaluate((el) => {
            const r = el.getBoundingClientRect();
            let [left, top, right, bottom] = [r.left, r.top, r.right, r.bottom];
            for (let p = el.parentElement; p; p = p.parentElement) {
              const s = getComputedStyle(p);
              if (s.overflowX === "visible" && s.overflowY === "visible") continue;
              const q = p.getBoundingClientRect();
              left = Math.max(left, q.left);
              right = Math.min(right, q.right);
            }
            right = Math.min(right, document.documentElement.clientWidth);
            return { x: left, y: top, width: right - left, height: bottom - top };
          });
        const size = { width: 60, height: 40 };
        const left = await zoom.page.screenshot({
          clip: { x: box.x - 12, y: box.y - 12, ...size },
        });
        const right = await zoom.page.screenshot({
          clip: { x: box.x + box.width - 48, y: box.y - 12, ...size },
        });
        await zoom.context.close();
        // Both corners side by side, in one image at their own pixels.
        const montage = await browser.newContext({ deviceScaleFactor: 1 });
        const pair = await montage.newPage();
        await pair.setContent(
          `<body style="margin:0"><div id="pair" style="display:inline-flex;gap:16px;padding:16px;background:#888">` +
            `<img src="data:image/png;base64,${left.toString("base64")}">` +
            `<img src="data:image/png;base64,${right.toString("base64")}"></div></body>`,
        );
        const cornersFile = join(outDir, `${shot.name}-${theme}-${width}-corners.png`);
        await pair.locator("#pair").screenshot({ path: cornersFile });
        written.push(cornersFile);
        await montage.close();
      }
    }
  }
}
await browser.close();

for (const file of written) console.log(file);
if (errors.length > 0) {
  writeFileSync(join(outDir, "errors.txt"), `${errors.join("\n")}\n`);
  console.error(`\n${errors.length} error(s):\n${errors.join("\n")}`);
  process.exit(1);
}
