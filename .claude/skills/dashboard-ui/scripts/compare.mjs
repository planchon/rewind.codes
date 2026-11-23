#!/usr/bin/env node
// The before and after of a UI review side by side (SKILL.md, "Review workflow").
//
//   node compare.mjs <before-dir> <after-dir> [<out.html>]
//
// Writes <out.html> (default: compare.html beside the two folders), each screenshot's before and
// after side by side, the changed ones first; prints each pair (changed, unchanged, only before,
// only after) with absolute paths, for the report.
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";

const [beforeArg, afterArg, outArg] = process.argv.slice(2);
if (!beforeArg || !afterArg) {
  console.error("usage: node compare.mjs <before-dir> <after-dir> [<out.html>]");
  process.exit(2);
}
const before = resolve(beforeArg);
const after = resolve(afterArg);
const out = resolve(outArg ?? join(dirname(after), "compare.html"));

const pngs = (dir) => (existsSync(dir) ? readdirSync(dir).filter((f) => f.endsWith(".png")) : []);
const hash = (file) => createHash("sha256").update(readFileSync(file)).digest("hex");
const names = [...new Set([...pngs(before), ...pngs(after)])].sort();

const pairs = names.map((name) => {
  const b = join(before, name);
  const a = join(after, name);
  const hasB = existsSync(b);
  const hasA = existsSync(a);
  const state = !hasA ? "only before" : !hasB ? "only after" : hash(b) === hash(a) ? "unchanged" : "changed";
  return { name, b: hasB ? b : null, a: hasA ? a : null, state };
});
const order = { changed: 0, "only after": 1, "only before": 2, unchanged: 3 };
pairs.sort((x, y) => order[x.state] - order[y.state] || x.name.localeCompare(y.name));

const img = (file) =>
  file ? `<img src="${relative(dirname(out), file)}" loading="lazy">` : `<div class="none">none</div>`;
const html = `<!doctype html>
<meta charset="utf-8">
<title>UI review</title>
<style>
  body { margin: 24px; font: 13px system-ui, sans-serif; background: #fafafa; color: #171717 }
  h2 { font-size: 14px; margin: 32px 0 8px }
  .state { color: #666; font-weight: 400 }
  .pair { display: grid; grid-template-columns: 1fr 1fr; gap: 12px }
  .pair img { width: 100%; border: 1px solid #ddd; background: #fff }
  .label { color: #666; margin-bottom: 4px }
  .none { color: #999; padding: 24px; border: 1px dashed #ccc }
</style>
<h1>UI review: ${pairs.filter((p) => p.state === "changed").length} changed, ${pairs.length} in all</h1>
${pairs
  .map(
    (p) => `<h2>${p.name} <span class="state">${p.state}</span></h2>
<div class="pair"><div><div class="label">Before</div>${img(p.b)}</div><div><div class="label">After</div>${img(p.a)}</div></div>`,
  )
  .join("\n")}
`;
writeFileSync(out, html);

console.log(out);
for (const p of pairs) console.log(`${p.state}\t${p.name}\n  before: ${p.b ?? "-"}\n  after:  ${p.a ?? "-"}`);
