// The release gate's report (acceptance/release/README.md), from what release.sh left in the
// results directory: meta.json, stages.jsonl, playwright-<project>.json and exchanges/*.jsonl.
//
//   node acceptance/release/report/build.mjs <results dir>
//
// Writes report.html (self-contained), report.md (the GitHub job summary) and report.json, prints
// the verdict, and exits with 0 for GO or PARTIAL without a failure, 1 for NO-GO.
//
// The verdict: NO-GO when a check failed or did not run, or a stage failed; PARTIAL when a stage
// was left out by the command line; GO when every stage ran and every check passed.
import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";

const results = process.argv[2];
if (!results) {
  console.error("usage: build.mjs <results dir>");
  process.exit(64);
}

const readJson = (file, fallback) => (existsSync(file) ? JSON.parse(readFileSync(file, "utf8")) : fallback);
const readLines = (file) =>
  existsSync(file)
    ? readFileSync(file, "utf8")
        .split("\n")
        .filter((line) => line.trim() !== "")
        .map((line) => JSON.parse(line))
    : [];

const meta = readJson(join(results, "meta.json"), {});
const version = existsSync(join(results, "version.txt")) ? readFileSync(join(results, "version.txt"), "utf8").trim() : "";
const stages = readLines(join(results, "stages.jsonl"));

/** Areas in the order of the design, with their names. */
const AREAS = {
  criteria: "Criteria of 0.1",
  runs: "Runs",
  endings: "Endings and limits",
  "host-tools": "Host tools and tasks",
  "mcp-egress": "MCP, http tool, egress",
  workflows: "Workflows",
  "check-test": "Check and test",
  observability: "Observability",
  access: "Access",
  namespaces: "Namespaces and tokens",
  tenancy: "Tenant isolation",
  sandbox: "Sandbox",
  hardening: "API hardening",
  cli: "CLI and local",
  lifecycle: "Lifecycle",
  sweeps: "Sweeps",
};
const KINDS = ["success", "failure", "attack"];

function areaOf(project, file) {
  if (project === "criteria") return "criteria";
  const name = basename(file).replace(/\.spec\.ts$/, "");
  return name;
}

// ---------------------------------------------------------------------------------------------
// Checks, from Playwright's JSON reports

const checks = [];
for (const entry of existsSync(results) ? readdirSync(results) : []) {
  const match = /^playwright-(.+)\.json$/.exec(entry);
  if (!match) continue;
  const report = readJson(join(results, entry), {});
  const walk = (suite, titles, file) => {
    for (const spec of suite.specs ?? []) {
      for (const t of spec.tests ?? []) {
        const project = t.projectName || match[1];
        const result = (t.results ?? []).at(-1) ?? {};
        const tags = (spec.tags ?? []).map((tag) => tag.replace(/^@/, ""));
        const kind = KINDS.find((k) => tags.includes(k)) ?? "success";
        const status =
          t.status === "expected" ? "pass" : t.status === "skipped" ? "not_run" : "fail";
        const errors = (result.errors ?? []).map((e) => stripAnsi(e.message ?? String(e))).filter(Boolean);
        const annotations = [...(t.annotations ?? []), ...(result.annotations ?? [])];
        const evidence = annotations.filter((a) => a.type === "evidence").map((a) => a.description);
        const skip = annotations.find((a) => a.type === "skip")?.description;
        let exchanges = [];
        for (const attachment of result.attachments ?? []) {
          if (attachment.name === "exchanges" && attachment.body) {
            try {
              exchanges = JSON.parse(Buffer.from(attachment.body, "base64").toString("utf8"));
            } catch {
              exchanges = [];
            }
          }
        }
        const specFile = spec.file ?? file ?? "";
        checks.push({
          project,
          area: areaOf(project, specFile),
          file: specFile,
          kind,
          title: [...titles, spec.title].filter(Boolean).join(" › "),
          status,
          reason: status === "not_run" ? (skip ?? "skipped") : undefined,
          durationMs: result.duration ?? 0,
          errors,
          evidence,
          exchanges,
        });
      }
    }
    for (const child of suite.suites ?? []) walk(child, [...titles, child.title], child.file ?? file);
  };
  for (const suite of report.suites ?? []) walk(suite, [], suite.file);
  // A stage whose setup failed has errors and no tests.
  for (const error of report.errors ?? []) {
    checks.push({
      project: match[1],
      area: match[1] === "areas" ? "setup" : match[1],
      file: "",
      kind: "failure",
      title: `${match[1]}: the stage could not run its checks`,
      status: "fail",
      durationMs: 0,
      errors: [stripAnsi(error.message ?? String(error))],
      evidence: [],
      exchanges: [],
    });
  }
}

// A stage that failed without leaving a single check (its Playwright never reported).
for (const stage of stages) {
  const projects = { criteria: "criteria", areas: "areas", lifecycle: "lifecycle", sweeps: "sweeps" };
  const project = projects[stage.stage];
  const reported = project ? checks.some((c) => c.project === project) : true;
  if (stage.status === "fail" && (!project || !reported)) {
    checks.push({
      project: stage.stage,
      area: stage.stage === "load" ? "load" : stage.stage,
      file: stage.log,
      kind: "success",
      title: `${stage.title}: failed (see ${stage.log})`,
      status: "fail",
      durationMs: (stage.seconds ?? 0) * 1000,
      errors: [tail(join(results, stage.log ?? ""), 30)],
      evidence: [],
      exchanges: [],
    });
  } else if (stage.status === "pass" && !project) {
    checks.push({
      project: stage.stage,
      area: stage.stage,
      file: stage.log,
      kind: "success",
      title: stage.title,
      status: "pass",
      durationMs: (stage.seconds ?? 0) * 1000,
      errors: [],
      evidence: [],
      exchanges: [],
    });
  }
}

function tail(file, lines) {
  if (!existsSync(file)) return "";
  return readFileSync(file, "utf8").split("\n").slice(-lines).join("\n");
}

function stripAnsi(text) {
  return String(text).replace(/\u001b\[[0-9;]*m/g, "");
}

// ---------------------------------------------------------------------------------------------
// Exchanges, for the totals

let exchangeCount = 0;
let unexpected5xx = 0;
let leaks = 0;
const exchangesDir = join(results, "exchanges");
if (existsSync(exchangesDir)) {
  for (const file of readdirSync(exchangesDir)) {
    for (const line of readLines(join(exchangesDir, file))) {
      exchangeCount += 1;
      if (line.status >= 500 && !line.expected5xx) unexpected5xx += 1;
      if (line.canaryLeak) leaks += 1;
    }
  }
}

// ---------------------------------------------------------------------------------------------
// Verdict

const failed = checks.filter((c) => c.status === "fail");
const notRun = checks.filter((c) => c.status === "not_run");
const stageFailed = stages.filter((s) => s.status === "fail");
const excluded = stages.filter((s) => s.status === "excluded");
const verdict =
  failed.length > 0 || notRun.length > 0 || stageFailed.length > 0 || checks.length === 0
    ? "NO-GO"
    : meta.partial || excluded.length > 0
      ? "PARTIAL"
      : "GO";

const areaKeys = [...new Set([...Object.keys(AREAS), ...checks.map((c) => c.area)])].filter((a) => checks.some((c) => c.area === a));
const matrix = areaKeys.map((area) => {
  const mine = checks.filter((c) => c.area === area);
  const kinds = Object.fromEntries(
    KINDS.map((kind) => {
      const of = mine.filter((c) => c.kind === kind);
      return [kind, { passed: of.filter((c) => c.status === "pass").length, total: of.length }];
    }),
  );
  return {
    area,
    name: AREAS[area] ?? area,
    passed: mine.filter((c) => c.status === "pass").length,
    total: mine.length,
    failed: mine.filter((c) => c.status === "fail").length,
    kinds,
  };
});
const totals = {
  checks: checks.length,
  passed: checks.filter((c) => c.status === "pass").length,
  failed: failed.length,
  notRun: notRun.length,
  byKind: Object.fromEntries(
    KINDS.map((kind) => [kind, { passed: checks.filter((c) => c.kind === kind && c.status === "pass").length, total: checks.filter((c) => c.kind === kind).length }]),
  ),
  exchanges: exchangeCount,
  unexpected5xx,
  canaryLeaks: leaks,
};
const finishedAt = new Date().toISOString();
const totalSeconds = meta.startedAt ? Math.round((Date.parse(finishedAt) - Date.parse(meta.startedAt)) / 1000) : undefined;
/** `42 s`, `3 min 05 s`. */
function duration(seconds) {
  if (seconds === undefined) return "";
  const s = Math.round(seconds);
  return s < 60 ? `${s} s` : `${Math.floor(s / 60)} min ${String(s % 60).padStart(2, "0")} s`;
}

writeFileSync(
  join(results, "report.json"),
  JSON.stringify({ verdict, meta: { ...meta, version, finishedAt, seconds: totalSeconds }, stages, totals, areas: matrix, checks }, null, 2),
);

// ---------------------------------------------------------------------------------------------
// Markdown

const md = [];
const commit = meta.commit ? `${meta.branch ?? ""} @ ${meta.commit.slice(0, 10)}${meta.dirty ? " (uncommitted changes)" : ""}` : "unknown commit";
md.push(`# Rewind release gate: ${verdict}`, "");
md.push(
  `${totals.passed} of ${totals.checks} checks passed, ${totals.failed} failed, ${totals.notRun} not run${totalSeconds !== undefined ? ` · ${duration(totalSeconds)}` : ""}.`,
  "",
);
md.push(`${commit} · ${version || "rewind ?"} · ${meta.os ?? ""} · Docker ${meta.docker ?? "?"}`, "");
md.push("| Stage | Status | Time |", "| --- | --- | --- |");
for (const stage of stages) {
  md.push(`| ${stage.title} | ${stage.status === "excluded" ? `excluded (${stage.reason})` : stage.status.toUpperCase()} | ${duration(stage.seconds)} |`);
}
md.push("", "| Area | Passed | Success | Failure | Attack |", "| --- | --- | --- | --- | --- |");
for (const row of matrix) {
  const k = (kind) => (row.kinds[kind].total === 0 ? "" : `${row.kinds[kind].passed}/${row.kinds[kind].total}`);
  md.push(`| ${row.name} | ${row.failed > 0 ? "✗" : "✓"} ${row.passed}/${row.total} | ${k("success")} | ${k("failure")} | ${k("attack")} |`);
}
if (failed.length > 0 || notRun.length > 0) {
  md.push("", "## Failed and not run", "");
  for (const check of [...failed, ...notRun]) {
    const first = (check.errors[0] ?? check.reason ?? "").split("\n").find((l) => l.trim() !== "") ?? "";
    md.push(`- **${AREAS[check.area] ?? check.area}** (${check.kind}) ${check.title}: ${first.slice(0, 300)}`);
  }
}
md.push("", `Exchanges recorded: ${exchangeCount}; unexpected 5xx: ${unexpected5xx}; canary leaks: ${leaks}.`, "");
writeFileSync(join(results, "report.md"), `${md.join("\n")}\n`);

// ---------------------------------------------------------------------------------------------
// HTML

const esc = (text) =>
  String(text ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");

function exchangeLine(e) {
  return `${e.method} ${e.url}  [${e.auth}] → ${e.status}${e.problem ? ` ${e.problem}` : ""}${e.expected5xx ? "  (expected)" : ""}${e.canaryLeak ? "  CANARY LEAK" : ""}`;
}

function checkHtml(check, open) {
  const mark = check.status === "pass" ? "pass" : check.status === "fail" ? "fail" : "skip";
  const label = check.status === "pass" ? "PASS" : check.status === "fail" ? "FAIL" : "NOT RUN";
  const last = check.exchanges.slice(-12);
  const body = [
    check.errors.length > 0 ? `<pre class="err">${esc(check.errors.join("\n\n").slice(0, 6000))}</pre>` : "",
    check.reason ? `<p class="muted">${esc(check.reason)}</p>` : "",
    check.evidence.length > 0 ? `<ul class="ev">${check.evidence.map((e) => `<li>${esc(e)}</li>`).join("")}</ul>` : "",
    last.length > 0
      ? `<pre class="xch">${esc(last.map(exchangeLine).join("\n"))}${check.exchanges.length > last.length ? `\n… ${check.exchanges.length - last.length} earlier` : ""}</pre>`
      : "",
  ].join("");
  const detail = body.trim() === "" ? "" : body;
  return `<details class="check ${mark}"${open ? " open" : ""}><summary><span class="st ${mark}">${label}</span><span class="kind k-${check.kind}">${check.kind}</span><span class="t">${esc(check.title)}</span><span class="ms">${(check.durationMs / 1000).toFixed(1)} s</span></summary>${detail || '<p class="muted">No exchange recorded.</p>'}</details>`;
}

const attacks = checks.filter((c) => c.kind === "attack");
const html = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Rewind release gate · ${esc(verdict)}</title>
<style>
:root { --bg:#f6f7f9; --surface:#fff; --ink:#121418; --muted:#5a6170; --line:#dde1e8; --soft:#eef1f5;
  --go:#17794a; --nogo:#c2352a; --partial:#a86200; --k-success:#1f5fd6; --k-failure:#a86200; --k-attack:#b0246f;
  --sans: ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; --mono: ui-monospace, "SF Mono", Menlo, Consolas, monospace; }
@media (prefers-color-scheme: dark) { :root { --bg:#0d0f12; --surface:#15181d; --ink:#e7e9ee; --muted:#9ba3b1; --line:#2a2f38; --soft:#1b1f26;
  --go:#46c084; --nogo:#f0705f; --partial:#e2a24a; --k-success:#79a2f2; --k-failure:#e2a24a; --k-attack:#e8679f; color-scheme: dark; } }
body { margin:0; background:var(--bg); color:var(--ink); font:14px/1.55 var(--sans); padding:32px max(16px, env(safe-area-inset-left)); }
.wrap { max-width: 1100px; margin: 0 auto; display: grid; gap: 28px; }
h1 { margin:0; font-size: 1.6rem; } h2 { margin:0 0 10px; font-size: 1.1rem; }
.banner { display:flex; flex-wrap:wrap; gap: 8px 20px; align-items: baseline; }
.verdict { font: 700 1.6rem var(--mono); letter-spacing: .04em; }
.v-GO { color: var(--go); } .v-NO-GO { color: var(--nogo); } .v-PARTIAL { color: var(--partial); }
.muted { color: var(--muted); margin: 6px 0; }
.meta { font: .8rem var(--mono); color: var(--muted); }
.tbl { overflow-x:auto; background:var(--surface); border:1px solid var(--line); border-radius:10px; }
table { border-collapse: collapse; width:100%; font-size:.86rem; }
th, td { text-align:left; padding:8px 12px; border-bottom:1px solid var(--line); vertical-align: top; }
th { font-size:.72rem; text-transform: uppercase; letter-spacing:.06em; color:var(--muted); background:var(--soft); }
tr:last-child td { border-bottom:none; }
td.n { font: .84rem var(--mono); font-variant-numeric: tabular-nums; white-space: nowrap; }
.ok { color: var(--go); font-weight:600; } .ko { color: var(--nogo); font-weight:600; } .ex { color: var(--partial); }
.check { background: var(--surface); border:1px solid var(--line); border-radius: 8px; margin: 6px 0; }
.check summary { display:flex; gap:10px; align-items: baseline; padding: 8px 12px; cursor: pointer; list-style: none; }
.check summary::-webkit-details-marker { display:none; }
.check > :not(summary) { margin: 0 12px 10px; }
.st { font: 600 .72rem var(--mono); padding: 1px 6px; border-radius: 4px; color: #fff; flex: none; }
.st.pass { background: var(--go); } .st.fail { background: var(--nogo); } .st.skip { background: var(--partial); }
.kind { font: .72rem var(--mono); flex: none; } .k-success { color: var(--k-success); } .k-failure { color: var(--k-failure); } .k-attack { color: var(--k-attack); }
.t { flex: 1; min-width: 0; } .ms { font: .75rem var(--mono); color: var(--muted); flex:none; }
pre { font: .76rem/1.5 var(--mono); background: var(--soft); border-radius: 6px; padding: 10px 12px; overflow-x: auto; white-space: pre-wrap; word-break: break-word; }
pre.err { border-left: 3px solid var(--nogo); }
ul.ev { margin: 4px 0 8px; padding-left: 18px; }
details.area > summary { font-weight: 600; cursor: pointer; padding: 6px 0; }
</style></head><body><div class="wrap">
<header class="banner">
  <span class="verdict v-${verdict}">${verdict}</span>
  <h1>Rewind release gate</h1>
</header>
<p>${totals.passed} of ${totals.checks} checks passed · ${totals.failed} failed · ${totals.notRun} not run${totalSeconds !== undefined ? ` · ${duration(totalSeconds)}` : ""}</p>
<p class="meta">${esc(commit)} · ${esc(meta.subject ?? "")}<br>${esc(version)} · ${esc(meta.os ?? "")} · Docker ${esc(meta.docker ?? "")} · Node ${esc(meta.node ?? "")} · ${esc(meta.rustc ?? "")}<br>started ${esc(meta.startedAt ?? "")} · finished ${esc(finishedAt)}</p>
${verdict === "PARTIAL" ? `<p class="ex">PARTIAL is not a release verdict: some stages were left out (${esc(excluded.map((s) => `${s.stage}: ${s.reason}`).join(", "))}).</p>` : ""}

<section><h2>Stages</h2><div class="tbl"><table><thead><tr><th>Stage</th><th>Status</th><th>Time</th><th>Log</th></tr></thead><tbody>
${stages.map((s) => `<tr><td>${esc(s.title)}</td><td class="${s.status === "pass" ? "ok" : s.status === "fail" ? "ko" : "ex"}">${s.status === "excluded" ? `excluded: ${esc(s.reason)}` : esc(s.status.toUpperCase())}</td><td class="n">${duration(s.seconds)}</td><td class="n">${s.log ? `<a href="${esc(s.log)}">${esc(s.log)}</a>` : ""}</td></tr>`).join("\n")}
</tbody></table></div></section>

<section><h2>By area and kind</h2><div class="tbl"><table><thead><tr><th>Area</th><th>Passed</th><th>Success</th><th>Failure</th><th>Attack</th></tr></thead><tbody>
${matrix
  .map((row) => {
    const k = (kind) => (row.kinds[kind].total === 0 ? "" : `<span class="${row.kinds[kind].passed === row.kinds[kind].total ? "ok" : "ko"}">${row.kinds[kind].passed}/${row.kinds[kind].total}</span>`);
    return `<tr><td>${esc(row.name)}</td><td class="n ${row.failed > 0 ? "ko" : "ok"}">${row.passed}/${row.total}</td><td class="n">${k("success")}</td><td class="n">${k("failure")}</td><td class="n">${k("attack")}</td></tr>`;
  })
  .join("\n")}
<tr><td><b>All</b></td><td class="n"><b>${totals.passed}/${totals.checks}</b></td>${KINDS.map((k) => `<td class="n">${totals.byKind[k].passed}/${totals.byKind[k].total}</td>`).join("")}</tr>
</tbody></table></div>
<p class="muted">${exchangeCount} exchanges with Rewind recorded · ${unexpected5xx} unexpected 5xx · ${leaks} answers holding the canary secret.</p></section>

${failed.length + notRun.length > 0 ? `<section><h2>Failed and not run</h2>${[...failed, ...notRun].map((c) => `<p class="muted">${esc(AREAS[c.area] ?? c.area)}</p>${checkHtml(c, true)}`).join("\n")}</section>` : ""}

<section><h2>Attacks and the defenses observed</h2><div class="tbl"><table><thead><tr><th>Area</th><th>Attack</th><th>Answer</th><th>Status</th></tr></thead><tbody>
${attacks
  .map((c) => {
    const refused = [...c.exchanges].reverse().find((e) => e.status >= 400);
    const answer = c.evidence[0] ?? (refused ? `${refused.status} ${refused.problem ?? ""}` : "");
    return `<tr><td>${esc(AREAS[c.area] ?? c.area)}</td><td>${esc(c.title)}</td><td>${esc(answer)}</td><td class="${c.status === "pass" ? "ok" : "ko"}">${c.status === "pass" ? "refused" : c.status === "fail" ? "FAILED" : "not run"}</td></tr>`;
  })
  .join("\n")}
</tbody></table></div></section>

<section><h2>Every check</h2>
${matrix.map((row) => `<details class="area"${row.failed > 0 ? " open" : ""}><summary>${esc(row.name)} · ${row.passed}/${row.total}</summary>${checks.filter((c) => c.area === row.area).map((c) => checkHtml(c, false)).join("\n")}</details>`).join("\n")}
</section>
</div></body></html>
`;
writeFileSync(join(results, "report.html"), html);

process.stdout.write(`\n${md.join("\n")}\n\nreport: ${join(results, "report.html")}\n`);
process.exit(verdict === "NO-GO" ? 1 : 0);
