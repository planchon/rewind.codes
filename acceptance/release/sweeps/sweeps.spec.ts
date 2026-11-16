/**
 * The sweeps of the release gate: what every earlier stage left behind, read once they are done.
 * The exchanges every check recorded, the servers' logs and the Collector's output.
 *
 * - No answer of Rewind held the run's canary secret, and no log, trace or metric did.
 * - No 5xx was answered outside a check that said it expects one.
 * - No server panicked.
 * - Each problem type of `api/README.md` was provoked at least once by a full run of the gate.
 */
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { root } from "../../support/paths.js";
import { evidence, type Exchange, expect, gateResults, test } from "../support/gate.js";

/**
 * Problem types the gate cannot provoke: a fault of the platform; and WorkOS's, since its
 * organizations are not made in the gate (`last-owner` protects members, and only WorkOS brings
 * members: keys never count as owners, 08). Criterion 8 proves them against WorkOS Staging.
 */
const NEVER_PROVOKED = new Set(["internal", "identity-unavailable", "last-owner"]);

function exchanges(): Exchange[] {
  const dir = join(gateResults, "exchanges");
  if (!existsSync(dir)) return [];
  return readdirSync(dir).flatMap((file) =>
    readFileSync(join(dir, file), "utf8")
      .split("\n")
      .filter((line) => line.trim() !== "")
      .map((line) => JSON.parse(line) as Exchange),
  );
}

/** Every file under the results whose name matches, outside Playwright's own artifacts. */
function files(match: RegExp, dir = gateResults): string[] {
  if (!existsSync(dir)) return [];
  const found: string[] = [];
  for (const entry of readdirSync(dir)) {
    if (entry === "artifacts" || entry.startsWith("html-") || entry === "exchanges") continue;
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) found.push(...files(match, path));
    else if (match.test(entry)) found.push(path);
  }
  return found;
}

function canaries(): string[] {
  const found = new Set<string>();
  for (const file of files(/^stacks.*\.json$/)) {
    const canary = JSON.parse(readFileSync(file, "utf8")).canary;
    if (typeof canary === "string") found.add(canary);
  }
  if (process.env.REWIND_GATE_CANARY) found.add(process.env.REWIND_GATE_CANARY);
  return [...found];
}

function fullRun(): boolean {
  const meta = join(gateResults, "meta.json");
  return existsSync(meta) && JSON.parse(readFileSync(meta, "utf8")).partial === false;
}

const short = (e: Exchange) => `${e.file ?? "?"} › ${e.test ?? "?"}: ${e.method} ${e.url} → ${e.status} ${e.problem ?? ""}`;

test("no answer of Rewind held the canary secret", { tag: "@attack" }, () => {
  const all = exchanges();
  const leaks = all.filter((e) => e.canaryLeak);
  evidence(`${all.length} answers searched; ${leaks.length} held the canary`);
  expect(leaks.map(short)).toEqual([]);
});

test("no log, trace or metric holds the canary secret", { tag: "@attack" }, () => {
  const secrets = canaries();
  const searched = files(/\.(log|jsonl)$/);
  const leaks: string[] = [];
  for (const file of searched) {
    const text = readFileSync(file, "utf8");
    for (const secret of secrets) if (text.includes(secret)) leaks.push(file.replace(`${gateResults}/`, ""));
  }
  evidence(`${searched.length} logs and Collector files searched for ${secrets.length} canary secret(s)`);
  expect(leaks).toEqual([]);
});

test("no 5xx was answered outside a check that expects one", { tag: "@failure" }, () => {
  const all = exchanges();
  const unexpected = all.filter((e) => e.status >= 500 && !e.expected5xx);
  evidence(`${all.length} answers, ${all.filter((e) => e.status >= 500).length} 5xx, ${unexpected.length} unexpected`);
  expect(unexpected.map(short)).toEqual([]);
});

test("no server panicked", { tag: "@failure" }, () => {
  const logs = files(/\.log$/);
  const panics: string[] = [];
  for (const file of logs) {
    for (const line of readFileSync(file, "utf8").split("\n")) {
      if (/panicked at|RUST_BACKTRACE|fatal runtime error|Fatal (JavaScript|process) out of memory/.test(line)) {
        panics.push(`${file.replace(`${gateResults}/`, "")}: ${line.slice(0, 300)}`);
      }
    }
  }
  evidence(`${logs.length} logs searched`);
  expect(panics).toEqual([]);
});

test("each problem type of api/README.md was provoked", { tag: "@success" }, () => {
  const readme = readFileSync(join(root, "api", "README.md"), "utf8");
  const slugs = [...readme.matchAll(/^\| `([a-z-]+)` \| (\d{3}) \|/gm)].map((m) => m[1]!);
  expect(slugs.length, "api/README.md lists the problem types").toBeGreaterThan(10);
  const seen = new Set(exchanges().map((e) => e.problem).filter(Boolean));
  const missing = slugs.filter((slug) => !NEVER_PROVOKED.has(slug) && !seen.has(slug));
  if (!fullRun()) {
    evidence(`partial run: ${slugs.length - missing.length} of ${slugs.length} problem types seen; the full gate requires each`);
    return;
  }
  evidence(`${seen.size} problem types seen: ${[...seen].sort().join(", ")}`);
  expect(missing, "problem types no check provoked").toEqual([]);
});
