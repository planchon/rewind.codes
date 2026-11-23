// Run handles and script titles in the mocked API, as the server computes them
// (rewind-core's `handle.rs` and `doc.rs`): `<namespace>/<origin>/<suffix>`, whose suffix is the
// start of the run's UUIDv7 in Crockford base32; a script's title from the comment at its top,
// or the one a person gave it (`PATCH /v1/scripts/{digest}`).
import type { RunFixture, World } from "./fixtures";

const ALPHABET = "0123456789abcdefghjkmnpqrstvwxyz";
const SUFFIX_LEN = 12;
const MAX_SUFFIX_LEN = 25;

/** The 122 bits of a UUIDv7 that are not its version nor its variant, padded by 3 to 125. */
function padded(id: string): bigint {
  const uuid = BigInt(`0x${id.replaceAll("-", "")}`);
  const ms = uuid >> 80n;
  const randA = (uuid >> 64n) & 0xfffn;
  const randB = uuid & ((1n << 62n) - 1n);
  return ((ms << 74n) | (randA << 62n) | randB) << 3n;
}

export function runSuffix(id: string, len = SUFFIX_LEN): string {
  const bits = padded(id);
  let out = "";
  for (let i = 0; i < len; i += 1) {
    out += ALPHABET[Number((bits >> BigInt(125 - 5 * (i + 1))) & 31n)];
  }
  return out;
}

/** The shortest suffix, at least 12 characters, `id` shares with none of `others`. */
export function suffixLenApart(id: string, others: readonly string[]): number {
  const mine = runSuffix(id, MAX_SUFFIX_LEN);
  let len = SUFFIX_LEN;
  for (const other of others) {
    if (other === id) continue;
    const theirs = runSuffix(other, MAX_SUFFIX_LEN);
    let common = 0;
    while (common < MAX_SUFFIX_LEN && mine[common] === theirs[common]) common += 1;
    len = Math.max(len, common + 1);
  }
  return Math.min(len, MAX_SUFFIX_LEN);
}

export function originSlug(title: string): string | null {
  const slug = title
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+/, "")
    .slice(0, 40)
    .replace(/-+$/, "");
  return slug === "" ? null : slug;
}

export interface ScriptDoc {
  title: string;
  description: string | null;
}

/** The comment at the top of a script: its first line the title, the rest the description. */
export function scriptDoc(source: string): ScriptDoc | null {
  const rest = source.trimStart();
  let lines: string[];
  if (rest.startsWith("/*")) {
    const end = rest.indexOf("*/");
    if (end < 0) return null;
    lines = rest
      .slice(2, end)
      .replace(/^\*+/, "")
      .split("\n")
      .map((l) =>
        l
          .trim()
          .replace(/^\*\s?/, "")
          .trim(),
      );
  } else if (rest.startsWith("//")) {
    lines = [];
    for (const line of rest.split("\n")) {
      const t = line.trim();
      if (!t.startsWith("//")) break;
      lines.push(t.replace(/^\/+/, "").trim());
    }
  } else {
    return null;
  }
  lines = lines.filter((l) => !l.startsWith("@"));
  const first = lines.findIndex((l) => l !== "");
  if (first < 0) return null;
  const title = lines[first] ?? "";
  const description = lines
    .slice(first + 1)
    .join("\n")
    .split(/\n\s*\n/)
    .map((p) => p.split("\n").join(" ").trim())
    .filter(Boolean)
    .join("\n\n");
  return { title, description: description || null };
}

export interface ScriptTitle {
  title: string;
  description: string | null;
  updatedBy: string;
  updatedAt: number;
}

/** A script's effective title: the one a person gave it, else its comment's. */
export function scriptTitleOf(
  world: World,
  digest: string,
  source: string,
): { title: string; description: string | null; source: "person" | "comment" } | null {
  const given = world.scriptTitles.get(digest);
  if (given) return { title: given.title, description: given.description, source: "person" };
  const doc = scriptDoc(source);
  return doc ? { ...doc, source: "comment" } : null;
}

/** Gives every run of the world its origin and suffix length, once: they never change after. */
export function ensureHandles(world: World): void {
  const pending = world.runs.filter((r) => r.origin === undefined || r.suffixLen === undefined);
  if (pending.length === 0) return;
  const ids = world.runs.map((r) => r.id);
  for (const run of pending) {
    if (run.origin === undefined) {
      const title = scriptTitleOf(world, run.script.digest, run.script.source)?.title;
      run.origin = run.workflow?.name ?? (title ? originSlug(title) : null) ?? "script";
    }
    if (run.suffixLen === undefined) {
      const near = ids.filter((id) => id.slice(0, 18) === run.id.slice(0, 18));
      run.suffixLen = suffixLenApart(run.id, near);
    }
  }
}

export function handleOf(world: World, run: RunFixture): string {
  ensureHandles(world);
  return `${run.namespace}/${run.origin ?? "script"}/${runSuffix(run.id, run.suffixLen ?? SUFFIX_LEN)}`;
}

/** The run a path names: its UUID, or its handle (percent-encoded or not). */
export function findRun(world: World, id: string): RunFixture | undefined {
  const wanted = id.includes("%") ? decodeURIComponent(id) : id;
  if (!wanted.includes("/")) return world.runs.find((r) => r.id === wanted);
  return world.runs.find((r) => handleOf(world, r) === wanted.toLowerCase());
}
